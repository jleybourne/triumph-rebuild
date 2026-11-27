//! Build 4: GFX10 → HLSL translation, over the shader corpus.
//!
//! REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- coverage
//!
//! Reads a **copy** of the title's `shaders.rpf` (never the game folder). `coverage` lists, for the v0 set
//! (straight-line, agnostic programs; `docs/design/shader-ir.md`), every opcode and how many programs use it,
//! then the greedy order: implement opcodes in this order and this many programs become fully covered.
//! `lift` runs the translator (`rebuild_sir::lift`) over the same set: programs with nothing left
//! untranslated, and the untranslated opcodes by how many programs they block.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rebuild_shader::census;
use rebuild_shader::features::{self, Findings, WaveClass};
use rebuild_shader::gfx10;
use rebuild_shader::native::HwStage;

fn stage_name(s: HwStage) -> &'static str {
    match s {
        HwStage::Vs => "vs",
        HwStage::Ps => "ps",
        HwStage::Cs => "cs",
        HwStage::Ms => "ms",
        HwStage::Es => "es",
        HwStage::Gs => "gs",
        _ => "other",
    }
}

/// Instructions with no effect on results (the census's "hints").
fn is_hint(name: &str) -> bool {
    name == "s_nop"
        || name.starts_with("s_waitcnt")
        || name == "s_clause"
        || name.starts_with("s_inst_prefetch")
        || name == "s_ttracedata"
        || name.starts_with("s_sethalt")
        || name == "s_code_end"
        || name == "s_dcache_inv"
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "coverage".into());
    let Ok(path) = std::env::var("REBUILD_SHADERS") else {
        eprintln!("translate: set REBUILD_SHADERS to a copy of shaders.rpf (never the game folder)");
        std::process::exit(2);
    };
    let data = std::fs::read(&path).expect("read the shaders.rpf copy");
    let walk = census::walk(&data, 8);

    // the v0 set, with the opcodes each program uses
    let next = AtomicUsize::new(0);
    let picked: Mutex<Vec<(HwStage, BTreeSet<&'static str>, Vec<String>, usize)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let mut f = Findings::default();
                let mut mine = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = walk.programs.get(i) else { break };
                    if !matches!(p.native.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs) {
                        continue;
                    }
                    let row = features::analyse(&p.native, &mut f);
                    if row.blocks != 1 || row.class() != WaveClass::Agnostic || row.decode_error.is_some() {
                        continue;
                    }
                    let Ok(insts) = gfx10::decode(&p.native.code) else { continue };
                    let ops: BTreeSet<&'static str> =
                        insts.iter().filter_map(|i| i.name()).filter(|n| !is_hint(n)).collect();
                    let l = rebuild_sir::lift::lift(&insts);
                    let mut missing: Vec<String> = l.unsupported.into_iter().map(|(_, n)| n).collect();
                    missing.sort();
                    missing.dedup();
                    mine.push((p.native.stage, ops, missing, insts.len()));
                }
                picked.lock().unwrap().extend(mine);
            });
        }
    });
    let picked = picked.into_inner().unwrap();
    match mode.as_str() {
        "coverage" => coverage(&picked.iter().map(|(s, o, _, _)| (*s, o.clone())).collect::<Vec<_>>()),
        "lift" => lift_report(&picked),
        m => {
            eprintln!("translate: unknown mode {m}");
            std::process::exit(2);
        }
    }
}

fn coverage(picked: &[(HwStage, BTreeSet<&'static str>)]) {
    let mut by_stage: BTreeMap<&str, usize> = BTreeMap::new();
    let mut users: BTreeMap<&str, usize> = BTreeMap::new();
    for (s, ops) in picked {
        *by_stage.entry(stage_name(*s)).or_default() += 1;
        for o in ops {
            *users.entry(o).or_default() += 1;
        }
    }
    println!("translate coverage — v0 set (straight-line, agnostic VS/PS/CS)");
    println!("programs  {} ({})", picked.len(), by_stage.iter().map(|(s, n)| format!("{s} {n}")).collect::<Vec<_>>().join(" · "));
    println!("opcodes   {} distinct (hints excluded)", users.len());

    // greedy: each step adds the opcode that completes the most programs (ties: the most used)
    let mut have: BTreeSet<&str> = BTreeSet::new();
    let mut covered = 0usize;
    println!("\n  step  opcode                              users  covered (cumulative)");
    while have.len() < users.len() {
        let best = users
            .keys()
            .filter(|o| !have.contains(*o))
            .map(|o| {
                let done = picked.iter().filter(|(_, ops)| ops.contains(o) && ops.iter().all(|x| x == o || have.contains(x))).count();
                (done, users[o], *o)
            })
            .max()
            .unwrap();
        have.insert(best.2);
        covered += best.0;
        println!("  {:>4}  {:<34} {:>6}  {:>6}  {:>5.1} %", have.len(), best.2, best.1, covered, 100.0 * covered as f64 / picked.len() as f64);
    }
}

fn lift_report(picked: &[(HwStage, BTreeSet<&'static str>, Vec<String>, usize)]) {
    let done = picked.iter().filter(|p| p.2.is_empty()).count();
    let mut by_stage: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut blocks: BTreeMap<&str, usize> = BTreeMap::new();
    let mut alone: BTreeMap<&str, usize> = BTreeMap::new();
    for (s, _, missing, _) in picked {
        let e = by_stage.entry(stage_name(*s)).or_default();
        e.1 += 1;
        if missing.is_empty() {
            e.0 += 1;
        }
        for m in missing {
            *blocks.entry(m.as_str()).or_default() += 1;
        }
        if missing.len() == 1 {
            *alone.entry(missing[0].as_str()).or_default() += 1;
        }
    }
    println!("translate lift — v0 set through rebuild_sir::lift");
    println!("programs  {} · fully translated {done} ({:.1} %)", picked.len(), 100.0 * done as f64 / picked.len() as f64);
    for (s, (d, n)) in &by_stage {
        println!("  {s}  {d} / {n}");
    }
    let mut v: Vec<_> = blocks.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    println!("
  untranslated opcode                  blocks  (only one missing)");
    for (n, c) in v.iter().take(40) {
        println!("  {n:<36} {c:>6}  {:>6}", alone.get(n).copied().unwrap_or(0));
    }
    println!("  ({} untranslated opcodes in all)", v.len());
}
