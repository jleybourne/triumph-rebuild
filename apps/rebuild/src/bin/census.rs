//! Build 4 front end: the shader census (lane shader, S1-S4).
//!
//! cargo run --release --bin census [-- --out agents/shader-census.csv --threads 4]
//!
//! Reads `shaders.rpf` from the folder in `REBUILD_GAME_DIR` (read-only: the file is
//! hashed before and after), finds every native program, decodes every instruction,
//! re-encodes each from its fields and compares the bytes, and writes one row per
//! program (hashes and counts only, no code) to the CSV.
//!
//! Optional cross-checks against the fork's own measurements, by path:
//! `REBUILD_FORK_CENSUS` (its full-archive TSV: hash, stage, console instruction count)
//! and `REBUILD_FORK_WORLD` (its 1,433-program world-frame CSV).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rebuild_shader::census::{self, Program};
use rebuild_shader::features::{self, Findings, Row};
use rebuild_shader::HwStage;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 { 0.0 } else { 100.0 * n as f64 / d as f64 }
}

fn quantiles(mut v: Vec<usize>) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_unstable();
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!("median {} · p90 {} · p99 {} · max {}", q(0.5), q(0.9), q(0.99), q(1.0))
}

fn stage_name(s: HwStage) -> &'static str {
    match s {
        HwStage::Ls => "ls",
        HwStage::Hs => "hs",
        HwStage::Es => "es",
        HwStage::Gs => "gs",
        HwStage::Vs => "vs",
        HwStage::Ps => "ps",
        HwStage::Ms => "ms",
        HwStage::Cs => "cs",
    }
}

fn main() {
    let threads: usize = arg("--threads").and_then(|t| t.parse().ok()).unwrap_or(4);
    let out = arg("--out").unwrap_or_else(|| "agents/shader-census.csv".into());
    let Ok(dir) = std::env::var("REBUILD_GAME_DIR") else {
        eprintln!("census: set REBUILD_GAME_DIR to the title's folder");
        std::process::exit(2);
    };
    let path = std::path::Path::new(&dir).join("shaders.rpf");
    let began = std::time::Instant::now();
    let data = std::fs::read(&path).expect("read shaders.rpf");
    let before = rebuild_shader::blake3::hash(&data);

    // S1: the programs.
    let walk = census::walk(&data, threads);
    let t_walk = began.elapsed().as_secs_f64();
    println!("census — Build 4 front end, lane shader");
    println!("archive      {} bytes, blake3 {}", data.len(), &before.to_hex()[..16]);
    println!(
        "records      {} SGD5 records ({} corrupt), {} raw containers; {} containers, {} without DXIL, {} duplicates",
        walk.records, walk.corrupt, walk.raw, walk.containers, walk.no_dxil, walk.duplicates
    );
    for (e, n) in &walk.native_errors {
        println!("  native unreadable {n}: {e}");
    }
    let mut by_stage: BTreeMap<HwStage, usize> = BTreeMap::new();
    for p in &walk.programs {
        *by_stage.entry(p.native.stage).or_default() += 1;
    }
    let graphics_compute: usize = [HwStage::Vs, HwStage::Ps, HwStage::Cs, HwStage::Ms]
        .iter()
        .map(|s| by_stage.get(s).copied().unwrap_or(0))
        .sum();
    println!(
        "programs     {} native programs; {graphics_compute} VS+PS+CS+MS (the fork's 25,694 count) · {:.1} s",
        walk.programs.len(),
        t_walk
    );
    println!(
        "  by stage   {}",
        by_stage.iter().map(|(s, n)| format!("{} {n}", stage_name(*s))).collect::<Vec<_>>().join(" · ")
    );

    // S2-S4: rows, on `threads` threads.
    let next = AtomicUsize::new(0);
    let rows: Mutex<Vec<(usize, Row)>> = Mutex::new(Vec::new());
    let found = Mutex::new(Findings::default());
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                let mut mine = Vec::new();
                let mut f = Findings::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = walk.programs.get(i) else { break };
                    mine.push((i, features::analyse(&p.native, &mut f)));
                }
                rows.lock().unwrap().extend(mine);
                let mut all = found.lock().unwrap();
                for (k, n) in f.unknown {
                    *all.unknown.entry(k).or_default() += n;
                }
                for (k, n) in f.mismatches {
                    *all.mismatches.entry(k).or_default() += n;
                }
                for (k, n) in f.by_layout {
                    *all.by_layout.entry(k).or_default() += n;
                }
            });
        }
    });
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by_key(|r| r.0);
    let rows: Vec<Row> = rows.into_iter().map(|r| r.1).collect();
    let found = found.into_inner().unwrap();

    // The CSV: hashes and counts only.
    let mut csv = String::from(
        "hash,code,stage,wave64,code_bytes,instructions,hints,exact,vgprs,sgprs,lds_bytes,dpp,sdwa,lds,gds,scratch,b16,\
         blocks,branch_scc,branch_vcc,branch_exec,branch_always,regions,loops,structured,wave_class,wave_ops,wave_ops_divergent\n",
    );
    for (p, r) in walk.programs.iter().zip(&rows) {
        let n = &p.native;
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            p.hash,
            p.code_hash,
            stage_name(n.stage),
            u8::from(n.wave64),
            r.code_bytes,
            r.instructions,
            r.hints,
            r.exact,
            n.vgprs,
            n.sgprs,
            n.lds_bytes,
            r.dpp,
            r.sdwa,
            r.lds,
            r.gds,
            r.scratch,
            r.b16,
            r.blocks,
            r.branch_scc,
            r.branch_vcc,
            r.branch_exec,
            r.branch_always,
            r.regions,
            r.loops,
            u8::from(r.structured()),
            r.class().name(),
            r.wave_ops,
            r.wave_ops_divergent
        ));
    }
    if let Some(parent) = std::path::Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::File::create(&out).and_then(|mut f| f.write_all(csv.as_bytes())).expect("write the CSV");

    // S2 totals.
    let n = rows.len();
    let count = |f: &dyn Fn(&Row) -> bool| rows.iter().filter(|r| f(r)).count();
    let sum = |f: &dyn Fn(&Row) -> usize| rows.iter().map(f).sum::<usize>();
    let wave64 = walk.programs.iter().filter(|p| p.native.wave64).count();
    let insts = sum(&|r| r.instructions);
    let exact = sum(&|r| r.exact);
    let failed = count(&|r| r.decode_error.is_some());
    println!(
        "decode       {} of {n} programs decode ({failed} do not); {insts} instructions, {} of them hints (s_nop, s_waitcnt...)",
        n - failed,
        sum(&|r| r.hints)
    );
    let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rows {
        if let Some(e) = r.decode_error {
            *reasons.entry(e).or_default() += 1;
        }
    }
    for (e, k) in &reasons {
        println!("  decode failed {k}: {e}");
    }
    println!(
        "re-encode    {exact} of {insts} instructions byte-exact ({:.4} %); programs all exact {}",
        pct(exact, insts),
        count(&|r| r.decode_error.is_none() && r.exact == r.instructions)
    );
    for (m, k) in &found.mismatches {
        println!("  mismatch   {k}: {m}");
    }
    println!(
        "unknown      {} distinct opcodes, {} instructions",
        found.unknown.len(),
        found.unknown.values().sum::<usize>()
    );
    for ((e, op), k) in &found.unknown {
        println!("  unknown    {e} {op:#x}: {k}");
    }
    println!(
        "  layouts    {}",
        found.by_layout.iter().map(|(l, k)| format!("{l} {k}")).collect::<Vec<_>>().join(" · ")
    );
    println!(
        "features    wave64 {wave64} ({:.1} %) · DPP {} programs · SDWA {} · LDS {} · GDS {} · scratch {} · 16-bit {}",
        pct(wave64, n),
        count(&|r| r.dpp > 0),
        count(&|r| r.sdwa > 0),
        count(&|r| r.lds > 0),
        count(&|r| r.gds > 0),
        count(&|r| r.scratch > 0),
        count(&|r| r.b16 > 0)
    );
    println!(
        "  instrs     DPP {} · SDWA {} · LDS {} · scratch {} · 16-bit {}",
        sum(&|r| r.dpp),
        sum(&|r| r.sdwa),
        sum(&|r| r.lds),
        sum(&|r| r.scratch),
        sum(&|r| r.b16)
    );

    let mut classes: BTreeMap<features::WaveClass, (usize, usize)> = BTreeMap::new();
    let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
    for (p, r) in walk.programs.iter().zip(&rows) {
        let e = classes.entry(r.class()).or_default();
        e.0 += 1;
        e.1 += usize::from(p.native.wave64);
        if let Some(w) = r.wide_reason {
            *reasons.entry(w).or_default() += 1;
        }
    }
    println!(
        "wave class   {}",
        classes.iter().map(|(c, (k, w))| format!("{} {k} ({w} of them wave64)", c.name())).collect::<Vec<_>>().join(" · ")
    );
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by_key(|r| std::cmp::Reverse(r.1));
    println!(
        "  first wave-wide op  {}",
        reasons.iter().map(|(n, k)| format!("{n} {k}")).collect::<Vec<_>>().join(" · ")
    );

    // S4: the feature summary.
    let vg: Vec<usize> = walk.programs.iter().map(|p| p.native.vgprs as usize).collect();
    let sg: Vec<usize> = walk.programs.iter().map(|p| p.native.sgprs as usize).collect();
    println!("S4 registers VGPRs {}", quantiles(vg));
    println!("             SGPRs {}", quantiles(sg));
    for (s, _) in &by_stage {
        let v: Vec<usize> =
            walk.programs.iter().filter(|p| p.native.stage == *s).map(|p| p.native.vgprs as usize).collect();
        println!("  {} VGPRs {}", stage_name(*s), quantiles(v));
    }
    let (scc, vcc, exec) = (sum(&|r| r.branch_scc), sum(&|r| r.branch_vcc), sum(&|r| r.branch_exec));
    let cond = scc + vcc + exec + sum(&|r| r.branch_other);
    println!(
        "S4 branches  {cond} conditional: scc {scc} ({:.1} %) · vcc {vcc} ({:.1} %) · exec {exec} ({:.1} %) · other {}; s_branch {}",
        pct(scc, cond),
        pct(vcc, cond),
        pct(exec, cond),
        sum(&|r| r.branch_other),
        sum(&|r| r.branch_always)
    );
    println!(
        "             wave-uniform (scc+vcc) {:.1} % · per-lane (exec) {:.1} %; EXEC-narrowing regions {} in {} programs",
        pct(scc + vcc, cond),
        pct(exec, cond),
        sum(&|r| r.regions),
        count(&|r| r.regions > 0)
    );
    println!(
        "S4 wave ops  {} cross-lane ops in {} programs; {} inside EXEC-narrowed regions, in {} programs (these become predication)",
        sum(&|r| r.wave_ops),
        count(&|r| r.wave_ops > 0),
        sum(&|r| r.wave_ops_divergent),
        count(&|r| r.wave_ops_divergent > 0)
    );
    let ok = n - failed;
    println!(
        "S4 structure {} of {ok} structured ({:.2} %): {} irreducible, {} with an indirect jump, {} with a call; \
         {} with no branch at all, {} with a loop",
        count(&|r| r.decode_error.is_none() && r.structured()),
        pct(count(&|r| r.decode_error.is_none() && r.structured()), ok),
        count(&|r| r.decode_error.is_none() && !r.reducible),
        count(&|r| r.indirect > 0),
        count(&|r| r.calls > 0),
        count(&|r| r.decode_error.is_none() && r.branch_always + cond_of(r) == 0),
        count(&|r| r.loops > 0)
    );
    println!("             blocks per program {}", quantiles(rows.iter().map(|r| r.blocks).collect()));
    println!("             instructions per program {}", quantiles(rows.iter().map(|r| r.instructions).collect()));

    // Cross-checks against the fork.
    if let Ok(p) = std::env::var("REBUILD_FORK_CENSUS") {
        fork_census(&p, &walk.programs, &rows);
    }
    if let Ok(p) = std::env::var("REBUILD_FORK_WORLD") {
        fork_world(&p, &walk, &rows);
    }

    // The game folder is read-only: check the archive did not change.
    let after = rebuild_shader::blake3::hash(&std::fs::read(&path).expect("re-read shaders.rpf"));
    println!(
        "read-only    shaders.rpf blake3 before {} after {} — {}",
        &before.to_hex()[..16],
        &after.to_hex()[..16],
        if before == after { "unchanged" } else { "CHANGED" }
    );
    println!("wrote        {out} ({n} rows) · {:.1} s", began.elapsed().as_secs_f64());
    if before != after {
        std::process::exit(1);
    }
}

fn cond_of(r: &Row) -> usize {
    r.branch_scc + r.branch_vcc + r.branch_exec + r.branch_other
}

/// The fork's full TSV: `<file>@<offset> <name> #<hash> ~<code8> <Stage>\twave64=..\tc_total=..`.
/// Matched by (code hash, stage), since duplicate containers keep a different hash
/// depending on its thread order.
fn fork_census(path: &str, programs: &[Program], rows: &[Row]) {
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("fork census  {path}: unreadable");
        return;
    };
    let mine: HashMap<(String, String), (usize, bool)> = programs
        .iter()
        .zip(rows)
        .map(|(p, r)| {
            ((p.code_hash.clone(), stage_name(p.native.stage).to_owned()), (r.instructions - r.hints, p.native.wave64))
        })
        .collect();
    let (mut lines, mut found, mut same, mut wave_same, mut theirs_total, mut mine_total) = (0, 0, 0, 0, 0usize, 0usize);
    let mut diffs: Vec<i64> = Vec::new();
    for line in text.lines() {
        let mut cols = line.split('\t');
        let Some(label) = cols.next() else { continue };
        let words: Vec<&str> = label.split_whitespace().collect();
        let (Some(code), Some(stage)) = (words.iter().find(|w| w.starts_with('~')), words.last()) else {
            continue;
        };
        let kv: HashMap<&str, usize> =
            cols.filter_map(|c| c.split_once('=')).filter_map(|(k, v)| Some((k, v.parse().ok()?))).collect();
        lines += 1;
        let key = (code[1..].to_owned(), stage.to_ascii_lowercase());
        if let Some(&(n, w64)) = mine.get(&key) {
            found += 1;
            let c = kv.get("c_total").copied().unwrap_or(0);
            theirs_total += c;
            mine_total += n;
            if c == n {
                same += 1;
            } else {
                diffs.push(n as i64 - c as i64);
            }
            if kv.get("wave64").copied() == Some(usize::from(w64)) {
                wave_same += 1;
            }
        }
    }
    diffs.sort_unstable();
    println!(
        "fork census  {lines} rows; {found} matched by (code hash, stage); console instructions without hints equal in {same}, \
         wave64 equal in {wave_same}; instructions theirs {theirs_total} · ours {mine_total}"
    );
    if !diffs.is_empty() {
        println!(
            "             {} differ: ours - theirs min {} · median {} · max {}",
            diffs.len(),
            diffs[0],
            diffs[diffs.len() / 2],
            diffs[diffs.len() - 1]
        );
    }
}

/// The fork's world-frame CSV: `hash,stage,total,...` (total = emitted DXIL instructions).
fn fork_world(path: &str, walk: &census::Walk, rows: &[Row]) {
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("fork world   {path}: unreadable");
        return;
    };
    let mut code_of: HashMap<&str, &str> =
        walk.programs.iter().map(|p| (p.hash.as_str(), p.code_hash.as_str())).collect();
    for (h, c) in &walk.aliases {
        code_of.insert(h.as_str(), c.as_str());
    }
    let mut by_code: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, p) in walk.programs.iter().enumerate() {
        by_code.entry(p.code_hash.as_str()).or_default().push(i);
    }
    let (mut hashes, mut found, mut dxil_total) = (0usize, 0usize, 0usize);
    let mut programs: HashSet<usize> = HashSet::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 3 {
            continue;
        }
        hashes += 1;
        dxil_total += cols[2].parse::<usize>().unwrap_or(0);
        let Some(code) = code_of.get(cols[0]) else { continue };
        found += 1;
        let want = match cols[1] {
            "vertex" => Some("vs"),
            "pixel" => Some("ps"),
            "compute" => Some("cs"),
            "mesh" => Some("ms"),
            _ => None,
        };
        for &i in &by_code[code] {
            let s = stage_name(walk.programs[i].native.stage);
            if want.is_none_or(|w| w == s) && matches!(s, "vs" | "ps" | "cs" | "ms") {
                programs.insert(i);
            }
        }
    }
    let console: usize = programs.iter().map(|&i| rows[i].instructions).sum();
    let wave64 = programs.iter().filter(|&&i| walk.programs[i].native.wave64).count();
    println!(
        "fork world   {hashes} hashes ({dxil_total} DXIL instructions, theirs); {found} found here → {} programs, \
         {console} console instructions, {wave64} wave64",
        programs.len()
    );
}
