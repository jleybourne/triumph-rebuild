//! Build 4: GFX10 → HLSL translation, over the shader corpus.
//!
//! REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- coverage
//!
//! Reads a **copy** of the title's `shaders.rpf` (never the game folder). `coverage` lists, for the v0 set
//! (straight-line, agnostic programs; `docs/design/shader-ir.md`), every opcode and how many programs use it,
//! then the greedy order: implement opcodes in this order and this many programs become fully covered.
//! `lift` runs the translator (`rebuild_sir::lift`) over the same set: programs with nothing left
//! untranslated, and the untranslated opcodes by how many programs they block. `ps` lifts the v0 pixel
//! shaders with their inputs and outputs, assembles each program from its signatures and compiles it with
//! DXC (`ps_6_6`, `-O3`), outputs under `REBUILD_TRANSLATE_OUT` (default `D:\victory-data\translate`).
//! Beside each `<hash>.<stage>.dxil` goes `<hash>.<stage>.samplers`: the immediate samplers' console words
//! (u32 count, then four u32 per sampler, little-endian, in `sir_imm<k>` order, then a u32 whose bit k is set when
//! sampler k is read by a comparison).
//! `v1` does the same for every agnostic VS, PS and CS, straight-line or not, through the control-flow
//! translator (`rebuild_sir::lift::lift_program`), and reports each stage split into one block and several.
//! Every refused program and its reason goes to `<out>/refused.<stage>.txt`. `REBUILD_TRANSLATE_LIFT_ONLY=1`
//! stops after the lift and assembly (no DXC), for quick coverage counts. `REBUILD_TRANSLATE_HASHES=<file>`
//! takes only the programs whose hash starts a line of the file (a `refused.<stage>.txt`, say).

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
        "ps" => ps_report(&walk, "ps", false),
        "vs" => ps_report(&walk, "vs", false),
        "cs" => ps_report(&walk, "cs", false),
        "v1" => {
            for k in ["ps", "vs", "cs"] {
                ps_report(&walk, k, true);
                println!();
            }
        }
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

/// The v0 pixel shaders: lifted with IO, assembled, compiled.
/// `ps` (pixel shaders) or `vs` (vertex shaders that run as the hardware VS): lift with IO, assemble, compile.
/// `v1`: every agnostic program of the stage, with control flow, through `lift_program`.
fn ps_report(walk: &census::Walk, kind: &str, v1: bool) {
    let vs = kind == "vs";
    use rebuild_pipelines::sidecar;
    use rebuild_pipelines::translate::{self, Refused};
    use rebuild_shader::native::StageIo;
    use rebuild_shader::signature;
    let dxc = translate::dxc_path().expect("dxc.exe (Windows SDK) or REBUILD_DXC");
    let out = std::path::PathBuf::from(std::env::var("REBUILD_TRANSLATE_OUT").unwrap_or_else(|_| r"D:\victory-data\translate".into()));
    std::fs::create_dir_all(&out).expect("output folder");
    let next = AtomicUsize::new(0);
    let tally: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());
    let compiled = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    // v1: (programs, compiled) with one block, and with several
    let single = [AtomicUsize::new(0), AtomicUsize::new(0)];
    let multi = [AtomicUsize::new(0), AtomicUsize::new(0)];
    let failures: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    // every refused program with its full first reason: `<out>/refused.<stage>.txt`
    let refused: Mutex<Vec<String>> = Mutex::new(Vec::new());
    // REBUILD_TRANSLATE_LIFT_ONLY=1: lift and assemble only (no files, no DXC); "compiled" counts lifted programs
    let lift_only = std::env::var_os("REBUILD_TRANSLATE_LIFT_ONLY").is_some();
    // REBUILD_TRANSLATE_HASHES=<file>: only the programs whose hash is the first word of a line of the file
    let only: Option<BTreeSet<String>> = std::env::var("REBUILD_TRANSLATE_HASHES").ok().map(|f| {
        std::fs::read_to_string(f).expect("hash list").lines().filter_map(|l| l.split_whitespace().next().map(String::from)).collect()
    });
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let mut f = Findings::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = walk.programs.get(i) else { break };
                    let io_ok = match &p.native.io {
                        StageIo::Ps { .. } => kind == "ps",
                        StageIo::Cs(_) => kind == "cs",
                        StageIo::Vs { .. } => vs && p.native.stage == rebuild_shader::native::HwStage::Vs,
                        _ => false,
                    };
                    if !io_ok || only.as_ref().is_some_and(|o| !o.contains(&p.hash)) {
                        continue;
                    }
                    let row = features::analyse(&p.native, &mut f);
                    if (row.blocks != 1 && !v1) || row.class() != WaveClass::Agnostic || row.decode_error.is_some() {
                        continue;
                    }
                    total.fetch_add(1, Ordering::Relaxed);
                    let split = if row.blocks == 1 { &single } else { &multi };
                    split[0].fetch_add(1, Ordering::Relaxed);
                    let bump = |k: String| *tally.lock().unwrap().entry(k).or_default() += 1;
                    // the same lift and assembly the runtime runs on demand (rebuild_pipelines::translate)
                    let h = match translate::lift_hlsl(&p.native, &p.isg1, &p.osg1, v1) {
                        Ok(h) => h,
                        Err(Refused { reason: e, body }) => {
                            if e == "patched code does not decode" || e == "signature unreadable" {
                                bump(e);
                                continue;
                            }
                            // REBUILD_TRANSLATE_DUMP=<text>: write refused programs whose reason contains it
                            if let Ok(want) = std::env::var("REBUILD_TRANSLATE_DUMP") {
                                if e.contains(&want) {
                                    let isg1 = signature::parse(&p.isg1).unwrap_or_default();
                                    let txt = format!(
                                        "{e}\nlayout {:#?}\nused {:#?}\ninputs {:?}\nisg1 {:?}\n--- lifted\n{}",
                                        p.native.layout, p.native.used, p.native.io, isg1, body
                                    );
                                    let _ = std::fs::write(out.join(format!("{}.fail.txt", p.hash)), txt);
                                }
                            }
                            refused.lock().unwrap().push(format!("{} {kind} {e}", p.hash));
                            let key = if e.starts_with("untranslated") {
                                e
                            } else if e.contains("which is no user data") {
                                "reads an SGPR that is no user data".to_string()
                            } else {
                                format!("io: {e}")
                            };
                            bump(key.split(" row ").next().unwrap().to_string());
                            continue;
                        }
                    };
                    if lift_only {
                        compiled.fetch_add(1, Ordering::Relaxed);
                        split[1].fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let ext = kind;
                    let hl = out.join(format!("{}.{ext}.hlsl", p.hash));
                    let obj = out.join(format!("{}.{ext}.dxil", p.hash));
                    std::fs::write(&hl, &h.src).expect("write hlsl");
                    // the immediate samplers beside the program, so a pipeline build need not re-read the archive:
                    // `<hash>.<stage>.samplers` (rebuild_pipelines::sidecar)
                    std::fs::write(out.join(format!("{}.{ext}.samplers", p.hash)), sidecar::encode(&h.samplers)).expect("write samplers");
                    match translate::compile_file(&dxc, h.stage, &hl, &obj) {
                        Ok(()) => {
                            compiled.fetch_add(1, Ordering::Relaxed);
                            split[1].fetch_add(1, Ordering::Relaxed);
                        }
                        Err(first) => {
                            bump("dxc error".into());
                            failures.lock().unwrap().push((p.hash.clone(), first));
                        }
                    }
                }
            });
        }
    });
    let mut refused = refused.into_inner().unwrap();
    refused.extend(failures.lock().unwrap().iter().map(|(h, e)| format!("{h} {kind} dxc: {e}")));
    refused.sort();
    let _ = std::fs::write(out.join(format!("refused.{kind}.txt")), refused.join("
"));
    let (n, ok) = (total.load(Ordering::Relaxed), compiled.load(Ordering::Relaxed));
    if v1 {
        println!("translate v1 {kind} — every agnostic program, control flow structured, compiled by DXC ({kind}_6_6, -O3)");
        for (what, x) in [("one block (v0)", &single), ("several blocks", &multi)] {
            let (a, b) = (x[0].load(Ordering::Relaxed), x[1].load(Ordering::Relaxed));
            println!("  {what:<16} {b} / {a} ({:.1} %)", 100.0 * b as f64 / a.max(1) as f64);
        }
    } else if kind == "cs" {
        println!("translate cs — v0 compute programs, lifted with IO and compiled by DXC (cs_6_6, -O3)");
    } else if vs {
        println!("translate vs — v0 vertex shaders (hardware VS), lifted with IO and compiled by DXC (vs_6_6, -O3)");
    } else {
        println!("translate ps — v0 pixel shaders, lifted with IO and compiled by DXC (ps_6_6, -O3)");
    }
    println!("programs  {n} · compiled {ok} ({:.1} %)", 100.0 * ok as f64 / n.max(1) as f64);
    let mut v: Vec<_> = tally.into_inner().unwrap().into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\n  not compiled, by first reason          programs");
    for (k, c) in v.iter().take(30) {
        println!("  {k:<40} {c:>6}");
    }
    let f = failures.into_inner().unwrap();
    if !f.is_empty() {
        println!("\n  dxc errors (first 10):");
        for (h, e) in f.iter().take(10) {
            println!("  {h}  {e}");
        }
    }
}
