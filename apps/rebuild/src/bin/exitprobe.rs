//! exitprobe: which of our memory patterns makes Windows slow to tear a process down.
//!
//! A `runtitle` run exited after 20 s, then its last thread spun in the kernel for minutes (address
//! space already gone, process unkillable). This finds which allocation pattern does that, **without the
//! title**: each step is a child process (this exe, `--child <pattern> <mib>`) that calls our own xmem
//! functions, prints `ready`, and exits. The parent times from `ready` to the process being gone.
//!
//! Sizes double from small. A pattern stops at the first teardown over `--limit-ms` (default 2000),
//! and the whole sweep stops if any child is not gone after 10 s, so the worst case is one child stuck
//! for a while, never a growing pile.
//!
//! ```text
//! ww        one write-watched graphics reservation of <mib>, 64 MiB of it committed and touched
//! wwmany    <mib>/256 write-watched graphics reservations of 256 MiB each, 64 KiB of each committed
//! heap      XMemAlloc graphics memory: <mib> in 1 MiB allocations (64 MiB write-watched chunks)
//! place     one 64 KiB-page placeholder of <mib>, every frame allocated in one call and mapped
//! placeone  as `place`, but frames allocated one per call (a section per frame)
//! ```

#![cfg(windows)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rebuild_runtime::exports::{XMemAlloc, XMemAllocatePhysicalPages, XMemMapPhysicalPages, XMemVirtualAlloc};
use rebuild_runtime::xmem::{GRAPHICS, PAGES_2M, PAGES_64K_BIT};

const MIB: usize = 1 << 20;
const FRAME: usize = 64 << 10;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const RW: u32 = 4;

fn touch(base: usize, len: usize) {
    for off in (0..len).step_by(4096) {
        unsafe { ((base + off) as *mut u8).write_volatile(1) };
    }
}

fn child(pattern: &str, mib: usize) -> Result<(), String> {
    let bytes = mib * MIB;
    match pattern {
        "ww" => {
            let a = XMemVirtualAlloc(std::ptr::null_mut(), bytes, MEM_RESERVE, GRAPHICS, RW) as usize;
            if a == 0 {
                return Err("reserve failed".into());
            }
            let c = (64 * MIB).min(bytes);
            if XMemVirtualAlloc(a as *mut _, c, MEM_COMMIT, GRAPHICS, RW).is_null() {
                return Err("commit failed".into());
            }
            touch(a, c);
        }
        "wwmany" => {
            for i in 0..(mib / 256).max(1) {
                // reserve 256 MiB, commit and touch only 64 KiB of it: commit stays tiny at every size
                let a = XMemVirtualAlloc(std::ptr::null_mut(), 256 * MIB, MEM_RESERVE, GRAPHICS, RW) as usize;
                if a == 0 || XMemVirtualAlloc(a as *mut _, 64 << 10, MEM_COMMIT, GRAPHICS, RW).is_null() {
                    return Err(format!("reservation {i} failed"));
                }
                touch(a, 64 << 10);
            }
        }
        "heap" => {
            for i in 0..mib {
                let a = XMemAlloc(MIB, 0) as usize;
                if a == 0 {
                    return Err(format!("alloc {i} failed"));
                }
                touch(a, 4096);
            }
        }
        "place" | "placeone" => {
            let a = XMemVirtualAlloc(std::ptr::null_mut(), bytes, MEM_RESERVE | PAGES_2M | PAGES_64K_BIT, GRAPHICS, RW) as usize;
            if a == 0 {
                return Err("placeholder failed".into());
            }
            let n = bytes / FRAME;
            let mut pages = vec![0usize; n];
            if pattern == "place" {
                let mut count = n;
                if XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()).0 == 0 {
                    return Err("frames failed".into());
                }
            } else {
                for p in pages.iter_mut() {
                    let mut count = 1usize;
                    if XMemAllocatePhysicalPages(0, &mut count, p).0 == 0 {
                        return Err("frame failed".into());
                    }
                }
            }
            if XMemMapPhysicalPages(a as *mut _, n, pages.as_ptr()).is_null() {
                return Err("map failed".into());
            }
            touch(a, bytes.min(64 * MIB));
        }
        p => return Err(format!("unknown pattern {p}")),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        let (p, mib) = (args[2].as_str(), args[3].parse().unwrap());
        // keep each child well inside the budget the runtime enforces
        match child(p, mib) {
            Ok(()) => println!("ready"),
            Err(e) => println!("error {e}"),
        }
        std::process::exit(0);
    }
    let limit = Duration::from_millis(
        args.iter().position(|a| a == "--limit-ms").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(2000),
    );
    let only: Option<&str> = args.iter().position(|a| a == "--pattern").and_then(|i| args.get(i + 1)).map(String::as_str);
    let me = std::env::current_exe().unwrap();
    // (pattern, first MiB, last MiB)
    let plan: &[(&str, usize, usize)] =
        &[("ww", 1024, 1 << 20), ("wwmany", 1024, 64 << 10), ("heap", 256, 2048), ("place", 256, 2048), ("placeone", 64, 1024)];
    println!("exitprobe: teardown time of a child that used one memory pattern (stop at {} ms)", limit.as_millis());
    for &(pattern, first, last) in plan {
        if only.is_some_and(|o| o != pattern) {
            continue;
        }
        let mut mib = first;
        while mib <= last {
            let mut c = Command::new(&me)
                .args(["--child", pattern, &mib.to_string()])
                .env("REBUILD_MEMORY_BUDGET_MB", "3072")
                .stdout(Stdio::piped())
                .spawn()
                .expect("child");
            let started = Instant::now();
            // setup has a limit too: a spin inside an allocation call never prints `ready`
            let out = c.stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut line = String::new();
                let _ = BufReader::new(out).read_line(&mut line);
                let _ = tx.send(line);
            });
            let Ok(line) = rx.recv_timeout(Duration::from_secs(10)) else {
                println!("{pattern:<9} {mib:>8} MiB  setup > 10 s: an allocation call did not return. STOPPING the sweep");
                return;
            };
            let alloc = started.elapsed();
            let t0 = Instant::now();
            let gone = loop {
                if let Ok(Some(_)) = c.try_wait() {
                    break Some(t0.elapsed());
                }
                if t0.elapsed() > Duration::from_secs(10) {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let line = line.trim();
            match gone {
                Some(t) => println!("{pattern:<9} {mib:>8} MiB  setup {:>7.1} ms  teardown {:>8.1} ms  {line}", alloc.as_secs_f64() * 1e3, t.as_secs_f64() * 1e3),
                None => {
                    println!("{pattern:<9} {mib:>8} MiB  setup {:>7.1} ms  teardown > 10 s: STOPPING the sweep  {line}", alloc.as_secs_f64() * 1e3);
                    return;
                }
            }
            if line != "ready" || gone.unwrap() > limit {
                break;
            }
            mib *= 2;
        }
    }
}
