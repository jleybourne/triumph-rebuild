//! `REBUILD_PROFILE=<ms>`: a wall-clock profile of every thread, and DirectStorage throughput once a second.
//!
//! Every `<ms>` milliseconds each thread of the process (but this one) is sampled with [`sampler::sample_copy`]
//! (suspend, copy registers and the top 64 KiB of stack, resume, unwind the copy). Samples are counted by the
//! thread and by the innermost [`DEPTH`] frames, so a thread blocked in a wait shows the wait and its caller,
//! and a thread that computes shows where. At the end ([`report`]) each thread's most common stacks print with
//! their share of that thread's samples; host frames are named through DbgHelp (our PDB, system exports).
//!
//! Once a second a `dsrate` line prints what DirectStorage did in that second: MiB out and in, reads, and where a
//! read's time went (see `rebuild_dstorage::Times`).
//!
//! Diagnostic only, opt-in: it suspends threads.

#![cfg(windows)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::GetExitCodeThread;

use crate::sampler;

/// Frames kept per sample, innermost first.
const DEPTH: usize = 10;
/// Stack bytes copied per sample.
const COPY: usize = 64 << 10;

#[derive(Default)]
struct Thread {
    name: String,
    start: u64,
    samples: u32,
    stacks: HashMap<[u64; DEPTH], u32>,
}

struct Profile {
    threads: HashMap<u32, Thread>,
    rounds: u32,
    started: Instant,
    elapsed: Duration,
    sample_time: Duration,
    /// title seconds at the last round
    until: f64,
}

static PROFILE: Mutex<Option<Profile>> = Mutex::new(None);

/// The interval from `REBUILD_PROFILE`, if it is set (0: the `dsrate` lines only, no thread is stopped).
fn interval() -> Option<Duration> {
    let ms: u64 = std::env::var("REBUILD_PROFILE").ok()?.trim().parse().ok()?;
    Some(Duration::from_millis(ms))
}

/// Starts the profiler thread if `REBUILD_PROFILE` is set.
pub fn start() {
    let Some(every) = interval() else { return };
    if every.is_zero() {
        println!("profile   dsrate once a second (REBUILD_PROFILE=0: no sampling)");
        let _ = std::thread::Builder::new().name("dsrate".into()).spawn(|| {
            let mut prev = (Instant::now(), rebuild_dstorage::stats().unwrap_or_default(), frame_waits());
            loop {
                std::thread::sleep(Duration::from_millis(250));
                ds_tick(&mut prev);
            }
        });
        return;
    }
    println!("profile   every thread every {} ms (REBUILD_PROFILE); dsrate once a second", every.as_millis());
    *PROFILE.lock().unwrap() =
        Some(Profile { threads: HashMap::new(), rounds: 0, started: Instant::now(), elapsed: Duration::ZERO, sample_time: Duration::ZERO, until: 0.0 });
    let _ = std::thread::Builder::new().name("profiler".into()).spawn(move || run(every));
}

extern "system" {
    fn NtQueryInformationThread(thread: HANDLE, class: u32, info: *mut u64, len: u32, ret: *mut u32) -> i32;
}

fn start_address(h: HANDLE) -> u64 {
    let mut a = 0u64;
    // ThreadQuerySetWin32StartAddress = 9
    let st = unsafe { NtQueryInformationThread(h, 9, &mut a, 8, std::ptr::null_mut()) };
    if st >= 0 {
        a
    } else {
        0
    }
}

fn description(h: HANDLE) -> String {
    use windows::Win32::System::Threading::GetThreadDescription;
    unsafe {
        match GetThreadDescription(h) {
            Ok(p) if !p.is_null() => {
                let s = p.to_string().unwrap_or_default();
                let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(p.0 as *mut _));
                s
            }
            _ => String::new(),
        }
    }
}

fn alive(h: HANDLE) -> bool {
    let mut code = 0u32;
    unsafe { GetExitCodeThread(h, &mut code) }.is_ok() && code == 259 // STILL_ACTIVE
}

fn run(every: Duration) {
    // REBUILD_PROFILE_UNTIL=<s>: stop sampling at that many seconds of title time (the loading screen alone)
    let until: Option<f64> = std::env::var("REBUILD_PROFILE_UNTIL").ok().and_then(|v| v.trim().parse().ok());
    let mut handles: HashMap<u32, HANDLE> = HashMap::new();
    let mut refreshed = Instant::now() - Duration::from_secs(10);
    let mut ds_prev = (Instant::now(), rebuild_dstorage::stats().unwrap_or_default(), frame_waits());
    let mut next = Instant::now();
    loop {
        next += every;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
        if until.is_some_and(|u| crate::launch::seconds_in() >= u) {
            ds_tick(&mut ds_prev);
            continue;
        }
        if refreshed.elapsed() >= Duration::from_millis(500) {
            refreshed = Instant::now();
            for (id, h) in sampler::threads() {
                if handles.contains_key(&id) {
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                } else {
                    let name = description(h);
                    let start = start_address(h);
                    let mut g = PROFILE.lock().unwrap();
                    let Some(p) = g.as_mut() else { return }; // the report took it: the run is ending
                    let t = p.threads.entry(id).or_default();
                    t.name = name;
                    t.start = start;
                    drop(g);
                    handles.insert(id, h);
                }
            }
            handles.retain(|_, h| {
                let a = alive(*h);
                if !a {
                    unsafe {
                        let _ = CloseHandle(*h);
                    }
                }
                a
            });
        }
        let t0 = Instant::now();
        let mut got: Vec<(u32, [u64; DEPTH])> = Vec::with_capacity(handles.len());
        for (&id, &h) in &handles {
            if let Some(s) = sampler::sample_copy(h, COPY) {
                let mut k = [0u64; DEPTH];
                let n = s.len.min(DEPTH);
                k[..n].copy_from_slice(&s.frames[..n]);
                got.push((id, k));
            }
        }
        let spent = t0.elapsed();
        {
            let mut g = PROFILE.lock().unwrap();
            let Some(p) = g.as_mut() else { return };
            p.rounds += 1;
            p.sample_time += spent;
            p.elapsed = p.started.elapsed();
            p.until = crate::launch::seconds_in();
            for (id, k) in got {
                let t = p.threads.entry(id).or_default();
                t.samples += 1;
                *t.stacks.entry(k).or_default() += 1;
            }
        }
        ds_tick(&mut ds_prev);
    }
}

/// A `dsrate` line once a second.
fn ds_tick(prev: &mut Snap) {
    if prev.0.elapsed() >= Duration::from_secs(1) {
        let now = (Instant::now(), rebuild_dstorage::stats().unwrap_or_default(), frame_waits());
        println!("{}", dsrate(prev, &now));
        *prev = now;
    }
}

fn frame_waits() -> u64 {
    rebuild_runtime::drives::FRAME_WAITS.load(std::sync::atomic::Ordering::Relaxed)
}

type Snap = (Instant, rebuild_dstorage::Stats, u64);

fn dsrate(a: &Snap, b: &Snap) -> String {
    let s = (b.0 - a.0).as_secs_f64();
    let (x, y) = (&a.1, &b.1);
    let reads = y.reads - x.reads;
    let per = |f: fn(&rebuild_dstorage::Times) -> u64| {
        if reads == 0 {
            0.0
        } else {
            (f(&y.times) - f(&x.times)) as f64 / reads as f64 / 1e6
        }
    };
    let statuses = y.times.statuses - x.times.statuses;
    let status_ms = if statuses == 0 { 0.0 } else { (y.times.status_ns - x.times.status_ns) as f64 / statuses as f64 / 1e6 };
    let t = crate::launch::seconds_in();
    let busy = |f: fn(&rebuild_dstorage::Times) -> u64| (f(&y.times) - f(&x.times)) as f64 / 1e9 / s;
    format!(
        "dsrate    {t:>5.1} s  out {:>6.1} MiB/s  in {:>6.1} MiB/s  {:>5.0} reads/s ({} zlib)  per read ms: latency {:.2} = unsubmitted {:.2} + ready {:.2} + read {:.2} + decode {:.2} + pages {:.2}  | {} statuses avg {:.2} ms (max so far {:.1})  frame waits {}  | workers: {:.2} running reads, {:.2} after-read lock, {:.2} idle",
        (y.bytes_out - x.bytes_out) as f64 / s / 1048576.0,
        (y.bytes_in - x.bytes_in) as f64 / s / 1048576.0,
        reads as f64 / s,
        y.zlib_reads - x.zlib_reads,
        per(|t| t.latency_ns),
        per(|t| t.unsubmitted_ns),
        per(|t| t.ready_ns),
        per(|t| t.read_ns),
        per(|t| t.decode_ns),
        per(|t| t.pages_ns),
        statuses,
        status_ms,
        y.times.status_max_ns as f64 / 1e6,
        b.2 - a.2,
        busy(|t| t.worker_exec_ns),
        busy(|t| t.worker_lock_ns),
        busy(|t| t.worker_idle_ns),
    )
}

/// Prints the profile: per thread, its most common stacks (call at the end of the run).
pub fn report(images: &[(String, u64, u64)]) {
    let Some(p) = PROFILE.lock().unwrap().take() else { return };
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut name = |a: u64| names.entry(a).or_insert_with(|| symbol(a, images)).clone();
    println!(
        "profile   {} rounds in {:.1} s (to {:.1} s of title time); sampling took {:.1} ms a round; per thread, innermost frame first:",
        p.rounds,
        p.elapsed.as_secs_f64(),
        p.until,
        p.sample_time.as_secs_f64() * 1e3 / p.rounds.max(1) as f64
    );
    let mut threads: Vec<(&u32, &Thread)> = p.threads.iter().filter(|(_, t)| t.samples > 0).collect();
    threads.sort_by_key(|(id, t)| (t.start, **id));
    for (id, t) in threads {
        let mut out = String::new();
        let _ = writeln!(out, "thread {id} '{}' start {} samples {}", t.name, name(t.start), t.samples);
        let mut stacks: Vec<(&[u64; DEPTH], &u32)> = t.stacks.iter().collect();
        stacks.sort_by(|a, b| b.1.cmp(a.1));
        for (k, n) in stacks.iter().take(6) {
            let pct = **n as f64 * 100.0 / t.samples as f64;
            if pct < 2.0 {
                break;
            }
            let frames: Vec<String> = k.iter().take_while(|&&a| a != 0).map(|&a| name(a)).collect();
            let _ = writeln!(out, "  {pct:>5.1}%  {}", frames.join(" < "));
        }
        print!("{out}");
    }
    busy(&p, &mut name);
}

/// Whether a leaf frame is a wait (the thread is blocked, not running).
fn waiting(leaf: &str) -> bool {
    ["Wait", "Delay", "RemoveIoCompletion", "WorkerFactory", "GetMessage", "Sleep"].iter().any(|w| leaf.contains(w))
}

/// Samples where a thread was not blocked, across every thread: by leaf, by the innermost frame of ours (the
/// layer's code: in this executable), and by the innermost frame of the title's.
fn busy(p: &Profile, name: &mut impl FnMut(u64) -> String) {
    let me = sampler::name(waiting as *const () as usize as u64, &[]);
    let me = me.split('+').next().unwrap_or("").to_string();
    let stem = format!("{}!", me.trim_end_matches(".exe"));
    let ours_frame = |f: &str| f.starts_with(&stem) || f.starts_with(&format!("{me}+"));
    let (mut leaf, mut ours, mut title): (HashMap<String, u32>, HashMap<String, u32>, HashMap<String, u32>) = Default::default();
    let mut total = 0u32;
    for t in p.threads.values() {
        for (k, &n) in &t.stacks {
            if k[0] == 0 {
                continue;
            }
            let l = name(k[0]);
            if waiting(&l) {
                continue;
            }
            total += n;
            *leaf.entry(l).or_default() += n;
            let frames: Vec<String> = k.iter().take_while(|&&a| a != 0).map(|&a| name(a)).collect();
            if let Some(f) = frames.iter().find(|f| ours_frame(f)) {
                *ours.entry(f.clone()).or_default() += n;
            }
            if let Some(f) = frames.iter().find(|f| f.contains(".exe+") && !ours_frame(f)) {
                *title.entry(f.clone()).or_default() += n;
            }
        }
    }
    let secs = p.elapsed.as_secs_f64().max(1e-9);
    let per = p.rounds.max(1) as f64 / secs;
    println!("profile   busy (not waiting) samples: {total}, about {:.2} threads running on average", total as f64 / p.rounds.max(1) as f64);
    for (what, m) in [("leaf", &leaf), ("ours", &ours), ("title", &title)] {
        let mut v: Vec<(&String, &u32)> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        for (f, n) in v.iter().take(15) {
            println!("  busy {what:<5} {:>6} samples  {:>6.2} thread-s  {f}", n, **n as f64 / per);
        }
    }
}

/// `image+rva` for the title's images; `function+off` (DbgHelp) or `module+off` for the host.
fn symbol(addr: u64, images: &[(String, u64, u64)]) -> String {
    if let Some((n, b, _)) = images.iter().find(|(_, b, sz)| addr >= *b && addr < b + sz) {
        return format!("{n}+{:#x}", addr - b);
    }
    use windows::Win32::System::Diagnostics::Debug::{SymFromAddr, SymInitialize, SymSetOptions, SYMBOL_INFO};
    use windows::Win32::System::Threading::GetCurrentProcess;
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // false when another part of the process (the crash reporter) initialised DbgHelp first: it is usable either way
    let _ = *READY.get_or_init(|| unsafe {
        SymSetOptions(0x0000_0004 | 0x0000_0002 | 0x0000_0200);
        SymInitialize(GetCurrentProcess(), None, true).is_ok()
    });
    {
        #[repr(C)]
        struct Sym {
            info: SYMBOL_INFO,
            name: [u8; 256],
        }
        let mut s: Sym = unsafe { std::mem::zeroed() };
        s.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
        s.info.MaxNameLen = 256;
        let mut disp = 0u64;
        if unsafe { SymFromAddr(GetCurrentProcess(), addr, Some(&mut disp), &mut s.info) }.is_ok() {
            let len = (s.info.NameLen as usize).min(255);
            let n = unsafe { std::slice::from_raw_parts(s.info.Name.as_ptr() as *const u8, len) };
            let module = sampler::name(addr, &[]);
            let module = module.split('+').next().unwrap_or("").trim_end_matches(".dll").trim_end_matches(".exe").to_string();
            return format!("{module}!{}+{disp:#x}", String::from_utf8_lossy(n));
        }
    }
    sampler::name(addr, images)
}
