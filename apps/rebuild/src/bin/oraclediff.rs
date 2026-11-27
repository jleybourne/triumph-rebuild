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
    let detail: Vec<String> = d.iter().map(|x| x.name()).collect();
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
