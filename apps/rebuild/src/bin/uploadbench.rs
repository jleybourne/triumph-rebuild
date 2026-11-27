//! Build 0 (a): the dirty-chunk upload benchmark.
//!
//! cargo run --release --bin uploadbench                 the full matrix (the Build 0 answer)
//! cargo run --release --bin uploadbench -- --quick      a short smoke run
//! --tiled: the destination is a tiled window (Build 1) instead of a committed buffer
//! options: --mb N (bytes the title writes per frame, default 115) --region-mb N (default 1024)
//!          --frames N (default 20)

use rebuild_uma::upload::{self, Change, Config, Diff, Path};

fn arg(name: &str, default: usize) -> usize {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    let tiled = std::env::args().any(|a| a == "--tiled");
    let written = arg("--mb", 115) << 20;
    let region = arg("--region-mb", 1024) << 20;
    let frames = arg("--frames", if quick { 5 } else { 20 });

    let mut gpu = match upload::Gpu::new() {
        Ok(g) => g,
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    };
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("uploadbench — Build 0 (a): write-watch + 64 B diff + upload, our own code");
    println!("gpu       {}", gpu.adapter);
    println!("cpu       {cpus} hardware threads");
    println!(
        "title     rewrites {} MiB a frame in a {} MiB region · {} timed frames after 3 warm-up · medians",
        written >> 20,
        region >> 20,
        frames
    );
    println!("dst       {}", if tiled { "a tiled window: reserved 4 GiB buffer, 64 KiB tiles mapped on 64 MiB heaps (Build 1)" } else { "a committed default-heap buffer" });
    println!(
        "gpu ms    {}",
        if gpu.copy_timestamps() { "copy-queue and compute timestamps" } else { "compute timestamps only (copy queue has none)" }
    );

    match gpu.ceiling(written, 10) {
        Ok((ms, gbs)) => println!("ceiling   one {} MiB copy, staging -> video memory: {ms:.2} ms = {gbs:.2} GB/s", written >> 20),
        Err(e) => println!("ceiling   FAILED {e}"),
    }
    println!();
    println!(
        "{:<17} {:<6} {:<7} {:>2} | {:>6} {:>8} {:>7} | {:>6} {:>6} {:>6} {:>6} {:>6} | {:>7} {:>7} | {}",
        "change", "diff", "path", "T", "pages", "runs", "MB", "watch", "diff", "gather", "record", "gpu", "wall", "p95", "check"
    );

    let changes: &[Change] = if quick {
        &[Change::Chunks(1.0), Change::Chunks(0.5)]
    } else {
        &[Change::Chunks(1.0), Change::Chunks(0.5), Change::Chunks(0.1), Change::Ranges(0.1), Change::Chunks(0.0)]
    };
    // --change chunks:0.5 | ranges:0.1   runs only that scenario
    let only: Option<Change> = {
        let args: Vec<String> = std::env::args().collect();
        args.iter().position(|a| a == "--change").and_then(|i| args.get(i + 1)).and_then(|v| {
            let (kind, p) = v.split_once(':')?;
            let p: f64 = p.parse().ok()?;
            match kind {
                "chunks" => Some(Change::Chunks(p)),
                "ranges" => Some(Change::Ranges(p)),
                _ => None,
            }
        })
    };
    let one;
    let changes: &[Change] = match only {
        Some(c) => {
            one = [c];
            &one
        }
        None => changes,
    };
    let threads: &[usize] = if quick { &[4] } else { &[1, 4] };
    let mut failed = false;
    for &change in changes {
        for diff in [Diff::Shadow, Diff::Hash] {
            for path in [Path::Copy, Path::Scatter] {
                for &t in threads {
                    let cfg = Config { region, written, change, diff, path, threads: t, frames, warmup: 3, seed: 0x7121_0001, tiled };
                    match upload::run(&mut gpu, &cfg) {
                        Ok(s) => {
                            let m = s.median;
                            let check = match &s.verified {
                                Ok(()) => "verified".to_string(),
                                Err(e) => {
                                    failed = true;
                                    format!("WRONG: {e}")
                                }
                            };
                            println!(
                                "{:<17} {:<6} {:<7} {:>2} | {:>6} {:>8} {:>7.1} | {:>6.2} {:>6.2} {:>6.2} {:>6.2} {:>6.2} | {:>7.2} {:>7.2} | {}",
                                change.label(),
                                format!("{diff:?}").to_lowercase(),
                                format!("{path:?}").to_lowercase(),
                                t,
                                m.dirty_pages,
                                m.runs,
                                m.bytes as f64 / (1 << 20) as f64,
                                m.watch_ms,
                                m.diff_ms,
                                m.gather_ms,
                                m.record_ms,
                                m.gpu_ms,
                                m.wall_ms,
                                s.p95_wall_ms,
                                check
                            );
                        }
                        Err(e) => {
                            failed = true;
                            println!("{:<17} {diff:?} {path:?} {t}: FAILED {e}", change.label());
                        }
                    }
                }
            }
        }
    }
    println!();
    println!("columns   ms, medians; wall = watch start -> fence complete (the title's own writes excluded)");
    if failed {
        std::process::exit(1);
    }
}
