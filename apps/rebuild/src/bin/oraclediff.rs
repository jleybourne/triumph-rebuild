//! Lane shader, S6: compare our DXIL with the oracle's, per program hash. No GPU.
//!
//! cargo run --release --bin oraclediff -- --ours <dir> --out <csv outside git> [--threads 4]
//! cargo run --release --bin oraclediff -- --self --mutate --out <csv>    # the closing check
//!
//! `<dir>` holds `<hash>.dxil` files (DXIL containers). The oracle is `REBUILD_ORACLE_DIR`
//! (or `--oracle`). For every hash in the oracle the CSV gets one row: whether ours exists,
//! which of signatures, bindings, thread-group size, wave size and the instruction
//! histogram differ, and the histogram counts (ours/oracle).
//!
//! `--self` compares the oracle with itself (must give 0 diffs). `--mutate` also injects,
//! into every oracle program it applies to, a moved binding, a dropped output-signature
//! element and a removed wave op, and counts how many the comparison catches.
//!
//! Reading one diff (S7):
//!
//! ```text
//! --sig <ours.dxil> <oracle.dxil>   both sides' signatures and bindings, "=" both, "<" ours only, ">" oracle only
//! --console <hash>                  the console program: stage IO, container signatures, every instruction's
//!                                   fields (REBUILD_SHADERS, a copy of shaders.rpf; printed, never written)
//! --ties <warm cache> [--fix <dir>] the S5 pairing's same-tick cache entries, settled by signatures or, with
//!                                   REBUILD_FORK_EXE, by re-translating each alone; --fix writes the right entries
//! --calls <file.dxil>               every call and the dx.op it resolves to
//! ```
//!
//! `InputReads` in the CSV's detail (pixel only, not an interface column): input components the oracle reads
//! and ours does not.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rebuild_shader::oraclediff::{self, CATEGORIES, Diff, Summary};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

#[derive(Default)]
struct Tally {
    rows: Vec<(usize, String)>,
    with_diffs: usize,
    missing: usize,
    unreadable: Vec<(String, String)>,
    by_kind: std::collections::BTreeMap<String, usize>,
    /// (applicable, caught) per mutation.
    mutations: [(usize, usize); 3],
}

fn main() {
    // --sig <ours.dxil> <oracle.dxil>: both sides' signatures and bindings, for reading an interface diff.
    // Elements both sides have are listed once ("="), the rest as "<" (ours only) and ">" (the oracle's only).
    if let Some(i) = std::env::args().position(|a| a == "--sig") {
        let a: Vec<String> = std::env::args().collect();
        let read = |f: &str| oraclediff::summarise(&std::fs::read(f).expect("read")).expect("summarise");
        let (ours, theirs) = (read(&a[i + 1]), read(&a[i + 2]));
        println!("ours   {}\noracle {}\ndiffs  {:?}", a[i + 1], a[i + 2], oraclediff::compare(&ours, &theirs));
        let side = |title: &str, x: Vec<String>, y: Vec<String>| {
            println!("{title}");
            for (mark, line) in oraclediff::list_diff(&x, &y) {
                println!("  {mark} {line}");
            }
        };
        let sig = |v: &[oraclediff::SigElem]| v.iter().map(oraclediff::SigElem::short).collect::<Vec<_>>();
        let res = |v: &[oraclediff::Binding]| v.iter().map(oraclediff::Binding::short).collect::<Vec<_>>();
        side("in", sig(&ours.sig_in), sig(&theirs.sig_in));
        side("out", sig(&ours.sig_out), sig(&theirs.sig_out));
        side("bindings", res(&ours.bindings), res(&theirs.bindings));
        println!("inputs never read: ours {:?} · oracle {:?}", ours.unread_in, theirs.unread_in);
        println!("input components the oracle reads and ours does not: {:?}", oraclediff::reads_missing(&ours, &theirs));
        return;
    }
    // --console <hash>: the console program behind a diff, from REBUILD_SHADERS (a copy of shaders.rpf, never the
    // game folder): its stage IO as the stage data states it, the container's signatures, and every instruction
    // with its fields. Printed only, never written (game-derived).
    if let Some(hash) = arg("--console") {
        console(&hash);
        return;
    }
    // --ties <warm cache> [--fix <dir>]: the S5 pairing's entries created in the same clock tick, settled by
    // signature agreement with their containers; --fix writes the corrected <hash>.dxil/.vsh into <dir>
    if let Some(cache) = arg("--ties") {
        ties(std::path::Path::new(&cache), arg("--fix").map(PathBuf::from));
        return;
    }
    // --calls <file.dxil>: every call in the module with the dx.op opcode it resolves to (reader debugging)
    if let Some(f) = arg("--calls") {
        let data = std::fs::read(&f).expect("read the container");
        match rebuild_shader::oraclediff::dx_op_calls(&data) {
            Ok(calls) => {
                let resolved = calls.iter().filter(|c| c.1.is_some()).count();
                println!("{} calls, {resolved} resolved to a dx.op opcode", calls.len());
                for (name, op) in calls.iter().take(40) {
                    println!("  {name:<40} {op:?}");
                }
            }
            Err(e) => println!("unreadable: {e}"),
        }
        if let Ok((names, callees)) = rebuild_shader::oraclediff::raw_calls(&data) {
            println!("named values {}: {:?}", names.len(), names.iter().take(12).collect::<Vec<_>>());
            println!("callee ids {}: {:?}", callees.len(), callees.iter().take(20).collect::<Vec<_>>());
        }
        return;
    }
    let oracle = PathBuf::from(
        arg("--oracle").or_else(|| std::env::var("REBUILD_ORACLE_DIR").ok()).expect("--oracle or REBUILD_ORACLE_DIR"),
    );
    let self_check = flag("--self");
    let ours = if self_check { oracle.clone() } else { PathBuf::from(arg("--ours").expect("--ours <dir> or --self")) };
    let out = arg("--out").expect("--out <csv, outside git>");
    let threads: usize = arg("--threads").and_then(|t| t.parse().ok()).unwrap_or(4);
    let mutate = flag("--mutate");
    let began = std::time::Instant::now();

    if let Some(n) = arg("--audit").and_then(|v| v.parse::<usize>().ok()) {
        audit(&oracle, n);
        return;
    }
    let mut hashes: Vec<String> = std::fs::read_dir(&oracle)
        .expect("read the oracle folder")
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".dxil").map(str::to_owned))
        .collect();
    hashes.sort();

    let next = AtomicUsize::new(0);
    let total = Mutex::new(Tally::default());
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                let mut t = Tally::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(hash) = hashes.get(i) else { break };
                    let theirs_bytes = std::fs::read(oracle.join(format!("{hash}.dxil"))).unwrap_or_default();
                    let theirs = match oraclediff::summarise(&theirs_bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            t.unreadable.push((hash.clone(), format!("oracle: {e}")));
                            t.rows.push((i, format!("{hash},,1,0,-1,,,,,,,,,,,,oracle unreadable: {e}")));
                            continue;
                        }
                    };
                    let ours_bytes = std::fs::read(ours.join(format!("{hash}.dxil")));
                    let row = match ours_bytes.map_err(|e| e.to_string()).and_then(|b| oraclediff::summarise(&b)) {
                        Ok(mine) => {
                            let d = oraclediff::compare(&mine, &theirs);
                            t.with_diffs += usize::from(!d.is_empty());
                            for x in &d {
                                let k = match x {
                                    Diff::Count(c, ..) => (*c).to_owned(),
                                    x => x.name(),
                                };
                                *t.by_kind.entry(k).or_default() += 1;
                            }
                            row(hash, &mine, &theirs, &d)
                        }
                        Err(e) => {
                            t.missing += 1;
                            format!("{hash},{},0,1,-1,,,,,,,,,,,,ours: {e}", theirs.stage)
                        }
                    };
                    t.rows.push((i, row));
                    if mutate {
                        mutations(&theirs_bytes, &theirs, &mut t.mutations);
                    }
                }
                let mut all = total.lock().unwrap();
                all.rows.extend(t.rows);
                all.with_diffs += t.with_diffs;
                all.missing += t.missing;
                all.unreadable.extend(t.unreadable);
                for (k, n) in t.by_kind {
                    *all.by_kind.entry(k).or_default() += n;
                }
                for k in 0..3 {
                    all.mutations[k].0 += t.mutations[k].0;
                    all.mutations[k].1 += t.mutations[k].1;
                }
            });
        }
    });
    let mut t = total.into_inner().unwrap();
    t.rows.sort_by_key(|r| r.0);
    let mut csv = String::from(
        "hash,stage,ours,oracle,diffs,sig_in,sig_out,sig_patch,bindings,threads,wave_size,memory,wave,control,barrier,io,detail\n",
    );
    for (_, r) in &t.rows {
        csv.push_str(r);
        csv.push('\n');
    }
    std::fs::write(&out, csv).expect("write the CSV");

    println!("oraclediff — lane shader S6{}", if self_check { " (the oracle against itself)" } else { "" });
    println!(
        "programs     {} in the oracle; {} compared, {} without ours, {} oracle unreadable",
        hashes.len(),
        hashes.len() - t.missing - t.unreadable.len(),
        t.missing,
        t.unreadable.len()
    );
    for (h, e) in t.unreadable.iter().take(10) {
        println!("  unreadable {h}: {e}");
    }
    println!("diffs        {} programs differ", t.with_diffs);
    for (k, n) in &t.by_kind {
        println!("  {k:<12} {n}");
    }
    if mutate {
        for (name, (applicable, caught)) in ["binding moved", "output element dropped", "wave op removed"].iter().zip(t.mutations) {
            println!("mutation     {name:<24} caught in {caught} of {applicable} programs it applies to");
        }
    }
    println!("wrote        {out} · {:.1} s", began.elapsed().as_secs_f64());
}

fn row(hash: &str, mine: &Summary, theirs: &Summary, d: &[Diff]) -> String {
    let has = |x: &Diff| u8::from(d.contains(x));
    let counts: Vec<String> = (0..CATEGORIES.len()).map(|i| format!("{}/{}", mine.hist.get(i), theirs.hist.get(i))).collect();
    let detail: Vec<String> = d
        .iter()
        .map(|x| match x {
            Diff::InputReads => format!("InputReads({})", oraclediff::reads_missing(mine, theirs).join(" ")),
            x => x.name(),
        })
        .collect();
    format!(
        "{hash},{},1,1,{},{},{},{},{},{},{},{},{}",
        theirs.stage,
        d.len(),
        has(&Diff::SigIn),
        has(&Diff::SigOut),
        has(&Diff::SigPatch),
        has(&Diff::Bindings),
        has(&Diff::Threads),
        has(&Diff::WaveSize),
        counts.join(","),
        detail.join(" ")
    )
}

/// The S5 oracle pairs the fork's cache entries with containers by creation time (`oracle.rs`). Entries created
/// in the same tick sort by path, so their order is a coin toss; same-stage swaps pass the stage check. This
/// lists each tie group, settles it by signature agreement with the containers (`best_assignment`), and says
/// which hashes the oracle has the wrong way round.
fn ties(cache: &std::path::Path, fix: Option<PathBuf>) {
    use rebuild_shader::HwStage;
    use std::collections::{HashMap, HashSet};
    const STAGES: [HwStage; 8] =
        [HwStage::Ls, HwStage::Hs, HwStage::Es, HwStage::Gs, HwStage::Vs, HwStage::Ps, HwStage::Ms, HwStage::Cs];
    fn files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                files(&p, out);
            } else if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".tmp")) {
                out.push(p);
            }
        }
    }
    let entry = |bytes: &[u8]| -> Option<(HwStage, Vec<u8>)> {
        let word = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
        (bytes.get(..4)? == b"VSH1").then_some(())?;
        Some((*STAGES.get(word(4)?)?, bytes.get(12..12 + word(8)?)?.to_vec()))
    };
    let rpf = std::env::var("REBUILD_SHADERS").expect("REBUILD_SHADERS: a copy of shaders.rpf (never the game folder)");
    let fork = std::env::var("REBUILD_FORK_EXE").ok();
    let walk = rebuild_shader::census::walk(&std::fs::read(&rpf).expect("read the shaders.rpf copy"), 4);
    // the order oracle.rs expects: each container once, if it carries a VS/PS/MS/CS program
    let mut by_code: HashMap<&str, &rebuild_shader::census::Program> = HashMap::new();
    for p in &walk.programs {
        if matches!(p.native.stage, HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs) {
            by_code.entry(p.code_hash.as_str()).or_insert(p);
        }
    }
    let mut seen = HashSet::new();
    let expected: Vec<(&str, &rebuild_shader::census::Program)> = walk
        .order
        .iter()
        .filter(|(h, _)| seen.insert(h.as_str()))
        .filter_map(|(h, c)| by_code.get(c.as_str()).map(|&p| (h.as_str(), p)))
        .collect();
    let mut list = Vec::new();
    files(cache, &mut list);
    let mut timed: Vec<_> =
        list.into_iter().map(|p| (std::fs::metadata(&p).and_then(|m| m.created()).expect("creation time"), p)).collect();
    timed.sort();
    println!("ties — the S5 pairing's entries created in the same tick ({} entries, {} expected)", timed.len(), expected.len());
    if timed.len() != expected.len() {
        println!("counts differ: the pairing itself is refused");
        return;
    }
    let (mut groups, mut swapped, mut unsettled) = (0, Vec::new(), 0);
    let mut i = 0;
    while i < timed.len() {
        let mut j = i + 1;
        while j < timed.len() && timed[j].0 == timed[i].0 {
            j += 1;
        }
        if j - i > 1 {
            groups += 1;
            let entries: Vec<(HwStage, Vec<u8>, Vec<u8>)> = timed[i..j]
                .iter()
                .map(|(_, p)| {
                    let bytes = std::fs::read(p).expect("read entry");
                    let (s, d) = entry(&bytes).expect("a VSH1 entry");
                    (s, d, bytes)
                })
                .collect();
            let scores: Vec<Vec<usize>> = expected[i..j]
                .iter()
                .map(|(_, p)| {
                    let cin = oraclediff::signature(&p.isg1).unwrap_or_default();
                    let cout = oraclediff::signature(&p.osg1).unwrap_or_default();
                    entries
                        .iter()
                        .map(|(s, d, _)| match oraclediff::summarise(d) {
                            // a stage mismatch rules the pairing out
                            Ok(sum) if *s == p.native.stage => 1 + oraclediff::sig_agreement(&cin, &cout, &sum),
                            _ => 0,
                        })
                        .collect()
                })
                .collect();
            let hashes: Vec<&str> = expected[i..j].iter().map(|(h, _)| *h).collect();
            // signatures first; where they cannot tell, the fork re-translates each container alone
            // (REBUILD_FORK_EXE, the build that made the oracle), and its entry names the right one
            let (settled, how) = match oraclediff::best_assignment(&scores) {
                Some(best) => (Some(best), format!("signatures {scores:?}")),
                None => match &fork {
                    Some(exe) => {
                        let alone: Vec<Option<Vec<u8>>> = hashes.iter().map(|h| translate_alone(exe, &rpf, h)).collect();
                        // container k takes the entry equal to its lone translation; identical entries are interchangeable
                        let pick: Option<Vec<usize>> = alone
                            .iter()
                            .enumerate()
                            .map(|(k, a)| {
                                let a = a.as_ref()?;
                                if entries[k].2 == *a { Some(k) } else { entries.iter().position(|e| e.2 == *a) }
                            })
                            .collect();
                        (pick, "re-translated alone".to_owned())
                    }
                    None => (None, format!("signatures {scores:?}")),
                },
            };
            match settled {
                Some(best) => {
                    let wrong: Vec<usize> = (0..best.len()).filter(|&k| best[k] != k).collect();
                    println!("  {:?} {how} -> {}", hashes, if wrong.is_empty() { "in order" } else { "SWAPPED" });
                    for k in wrong {
                        swapped.push(hashes[k].to_owned());
                        if let Some(dir) = &fix {
                            std::fs::create_dir_all(dir).expect("create the fix folder");
                            let (_, dxil, bytes) = &entries[best[k]];
                            std::fs::write(dir.join(format!("{}.dxil", hashes[k])), dxil).unwrap();
                            std::fs::write(dir.join(format!("{}.vsh", hashes[k])), bytes).unwrap();
                        }
                    }
                }
                None => {
                    unsettled += 1;
                    println!("  {hashes:?} {how} -> cannot tell (left as paired; set REBUILD_FORK_EXE to re-translate)");
                }
            }
        }
        i = j;
    }
    println!("groups       {groups} · {} hashes the wrong way round · {unsettled} groups unsettled", swapped.len());
    for h in &swapped {
        println!("  swapped    {h}");
    }
    if let Some(dir) = fix {
        println!("wrote        the corrected entries of the swapped hashes into {}", dir.display());
    }
}

/// The fork's offline translator on one container, into a fresh cache of its own (a temporary folder): the
/// entry it files, or None. CPU only (`--warm` makes no device), as S5's `--verify`.
fn translate_alone(exe: &str, rpf: &str, hash: &str) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("oraclediff-ties-{}-{hash}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let list = dir.join("hash.txt");
    std::fs::write(&list, hash).ok()?;
    let ok = std::process::Command::new(exe)
        .args(["translate", "--warm"])
        .arg(&list)
        .args(["--jobs", "1", rpf])
        .env("VICTORY_CACHE_DIR", dir.join("cache"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let mut found = Vec::new();
    fn walk(d: &std::path::Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { walk(&p, out) } else { out.push(p) }
        }
    }
    walk(&dir.join("cache"), &mut found);
    let entry = if ok && found.len() == 1 { std::fs::read(&found[0]).ok() } else { None };
    let _ = std::fs::remove_dir_all(&dir);
    entry
}

fn console(hash: &str) {
    use rebuild_shader::gfx10;
    let path = std::env::var("REBUILD_SHADERS").expect("REBUILD_SHADERS: a copy of shaders.rpf (never the game folder)");
    let walk = rebuild_shader::census::walk(&std::fs::read(path).expect("read the shaders.rpf copy"), 4);
    let Some(p) = walk.programs.iter().find(|p| p.hash == hash) else {
        println!("{hash}: not in the archive");
        return;
    };
    let n = &p.native;
    println!("{hash} {:?} wave64 {} vgprs {} sgprs {}", n.stage, n.wave64, n.vgprs, n.sgprs);
    println!("io {:#?}", n.io);
    for (part, title) in [(&p.isg1, "container in"), (&p.osg1, "container out")] {
        if let Ok(sig) = oraclediff::signature(part) {
            println!("{title}");
            for e in &sig {
                println!("  {}", e.short());
            }
        }
    }
    let code = n.patched_code();
    for inst in gfx10::decode(&code).expect("decode") {
        let d = gfx10::enc::decode(&inst);
        let mut fields: Vec<String> =
            d.layout.fields.iter().zip(&d.values).filter(|(f, _)| f.name != "op" && f.name != "op_hi").map(|(f, v)| format!("{}={v}", f.name)).collect();
        if let Some((l, v)) = &d.extra {
            fields.extend(l.fields.iter().zip(v).map(|(f, v)| format!("{}={v}", f.name)));
        }
        if let Some(lit) = d.literal {
            fields.push(format!("lit={lit:#x}"));
        }
        println!("{:5x} {:<24} {}", inst.offset, inst.name().unwrap_or("?"), fields.join(" "));
    }
}

/// Each mutation on an oracle program, compared with the program as it is.
fn mutations(bytes: &[u8], base: &Summary, tally: &mut [(usize, usize); 3]) {
    let caught = |m: Option<Summary>, want: &dyn Fn(&Diff) -> bool| -> Option<bool> {
        m.map(|m| oraclediff::compare(&m, base).iter().any(want))
    };
    let results = [
        caught(oraclediff::mutate::move_binding(bytes).and_then(|b| oraclediff::summarise(&b).ok()), &|d| {
            *d == Diff::Bindings
        }),
        caught(oraclediff::mutate::drop_output(bytes).and_then(|b| oraclediff::summarise(&b).ok()), &|d| {
            *d == Diff::SigOut
        }),
        caught(oraclediff::drop_wave_op(base), &|d| matches!(d, Diff::Count("wave", ..))),
    ];
    for (k, r) in results.into_iter().enumerate() {
        if let Some(c) = r {
            tally[k].0 += 1;
            tally[k].1 += usize::from(c);
        }
    }
}

/// `--audit N`: over every Nth oracle program, each dx.op function name with the opcodes
/// its calls resolved to. One function name must give one opcode (the "unary", "binary"
/// and "tertiary" families excepted), and none may be unresolved.
fn audit(oracle: &std::path::Path, step: usize) {
    let mut files: Vec<_> = std::fs::read_dir(oracle).unwrap().flatten().map(|e| e.path()).collect();
    files.retain(|p| p.extension().is_some_and(|x| x == "dxil"));
    files.sort();
    let mut seen: std::collections::BTreeMap<String, std::collections::BTreeMap<Option<u32>, usize>> = Default::default();
    let mut programs = 0;
    for p in files.iter().step_by(step.max(1)) {
        programs += 1;
        for (name, op) in oraclediff::dx_op_calls(&std::fs::read(p).unwrap()).unwrap() {
            let base = name.trim_start_matches("dx.op.").split('.').next().unwrap_or("").to_owned();
            *seen.entry(base).or_default().entry(op).or_default() += 1;
        }
    }
    let (mut calls, mut unresolved, mut ambiguous) = (0, 0, 0);
    for (name, ops) in &seen {
        calls += ops.values().sum::<usize>();
        unresolved += ops.get(&None).copied().unwrap_or(0);
        ambiguous += usize::from(ops.len() > 1);
        let list: Vec<String> = ops.iter().map(|(o, n)| format!("{}x{n}", o.map_or("?".into(), |o| o.to_string()))).collect();
        println!("  {name:<28} {}", list.join(" "));
    }
    println!("audit        {programs} programs · {calls} dx.op calls · {} functions · {unresolved} unresolved · {ambiguous} names with more than one opcode", seen.len());
}
