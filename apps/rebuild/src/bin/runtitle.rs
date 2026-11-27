//! Build 3: load the title through the Windows loader (prepared images) and run its startup to its first
//! render call.
//!
//! REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin runtitle            # load and bind only
//! REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin runtitle -- --run   # and run its startup
//!
//! The run stops at `D3D12XboxCreateDevice` (the first render call), or after `--seconds N` (default 60),
//! and prints the discovery list: every unbuilt import the title called, in first-call order. The game
//! folder is only read; prepared copies go to %LOCALAPPDATA%\triumph-rebuild\images.
//!
//! With `--sample N` (opt-in: it suspends a thread), a sampler stops the title's main thread every N seconds and prints
//! its stack as image+rva, so a title that waits shows where. At the end, every thread is sampled once.
//! File and DirectStorage calls go through the drives (`rebuild_runtime::drives`): G: the title's folder,
//! read-only; T: emptied at start; S: storage, kept.

#[path = "../sampler.rs"]
mod sampler;
#[path = "../crash.rs"]
mod crash;
#[path = "../watch.rs"]
mod watch;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use std::os::windows::io::AsRawHandle;

use rebuild_runtime::exports::{on_create_device, set_alloc_hooks, set_command_line};
use rebuild_runtime::modules::misses;
use rebuild_runtime::title::{load_title, trap_hits, Loaded};

fn report(started: Instant) {
    println!("discovery ({:.1} s in): unbuilt imports the title called, first-call order", started.elapsed().as_secs_f64());
    for (dll, name, n) in trap_hits() {
        println!("  {n:>6}×  {dll}!{name}");
    }
    let d = rebuild_winfiles::discoveries();
    if !d.is_empty() {
        println!("paths the drives don't know: {}", d.len());
        for x in d.iter().take(40) {
            println!("          {x:?}");
        }
    }
    let g = rebuild_runtime::gdk_discovery();
    if !g.is_empty() {
        println!("gdk runtime discovery: {}", g.len());
        for l in g {
            println!("          {l}");
        }
    }
    let m = misses();
    if !m.is_empty() {
        println!("lookups that found nothing (GetProcAddress / LoadLibrary): {}", m.len());
        for (dll, name) in m {
            println!("          {dll}!{name}");
        }
    }
}

static mut STARTED: Option<Instant> = None;

/// `<REBUILD_SESSION_DIR>\summary.toml`: how the run ended, for the launcher's session list.
fn write_summary(outcome: &str, detail: &str) {
    let Some(dir) = std::env::var_os("REBUILD_SESSION_DIR") else { return };
    let elapsed = unsafe { STARTED }.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
    let limits = rebuild_runtime::limits::get();
    let text = format!(
        "[summary]\noutcome = \"{outcome}\"\ndetail = \"{}\"\nseconds = {elapsed:.3}\n{}\
         [title]\nbudget_mib = {}\nbudget_used_mib = {}\n\
         [limits]\nram_mb = {}\nvram_mb = {}\ncpu_cores = {}\ncpu_rate = {}\n",
        detail.replace('\\', "\\\\").replace('"', "\\\""),
        // the frame keys (avg_fps, low1_fps, frame_ms, gpu_ms, frames, measured_from) from the present's counters
        rebuild_runtime::framelog::summary_keys(false),
        rebuild_runtime::budget::limit() >> 20,
        rebuild_runtime::budget::used() >> 20,
        limits.ram_mb,
        limits.vram_mb,
        limits.cpu_cores,
        limits.cpu_rate
    );
    let _ = std::fs::write(std::path::PathBuf::from(dir).join("summary.toml"), text);
}

fn reached_device(args: &[u64; 4]) {
    let started = unsafe { STARTED }.unwrap_or_else(Instant::now);
    println!("REACHED   the first render call: D3D12XboxCreateDevice{args:x?}");
    write_summary("reached", "the first render call: D3D12XboxCreateDevice");
    let _ = started;
    // Build 5: the device exists now; the title runs on until the deadline and the trap list names what it called
    REACHED.store(true, std::sync::atomic::Ordering::Relaxed);
}

static REACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The device slots the title called that are not written yet, first called first.
fn device_report() {
    if !REACHED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    println!("{}", rebuild_runtime::pipeline::report());
    let t = rebuild_runtime::device::traps();
    println!("device    {} slots called that are not written yet (first call, ms after the device):", t.len());
    for (off, name, n, ms) in t {
        println!("  {off:#05x}  {name:<36} {n:>8}×  first at {ms:>9.3} ms");
    }
    let q = rebuild_runtime::queue::traps();
    if !q.is_empty() {
        println!("queue     {} slots called that are not written yet:", q.len());
        for (off, name, n, ms) in q {
            println!("  {off:#05x}  {name:<36} {n:>8}×  first at {ms:>9.3} ms");
        }
    }
    let lt = rebuild_xlist::console::traps();
    if !lt.is_empty() {
        println!("list      slots called that are not written yet: {}", lt.len());
        for t in lt.iter().take(30) {
            println!("          {t:?}");
        }
    }
    println!("device    every slot called through the device's own functions:");
    for (off, name, n, ms, imp) in rebuild_runtime::device::called() {
        println!("  {off:#05x}  {name:<36} {n:>8}×  first at {ms:>9.3} ms{}", if imp { "" } else { "  (trap)" });
    }
    let fc: Vec<u64> = rebuild_runtime::frame::CALLS.iter().map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).collect();
    println!("frame     interval/schedule/wait/statistics calls: {fc:?}");
    let rt = rebuild_d3d12x::traps();
    if !rt.is_empty() {
        println!("resources slots called that trap: {}", rt.len());
        for t in rt.iter().take(30) {
            println!("          {} {:#05x} {}x first at {:.3} ms", t.object, t.offset, t.count, t.first_ns as f64 / 1e6);
        }
    }
    let dx = rebuild_runtime::dxgi::traps();
    if !dx.is_empty() {
        println!("dxgi      slots called that are not written yet: {dx:x?}");
    }
    let f = rebuild_runtime::fence::traps();
    if !f.is_empty() {
        println!("fence     slots called that are not written yet: {f:x?}");
    }
    let p = rebuild_runtime::cpu::patched();
    if p > 0 {
        println!("cpu       {p} clwb sites patched to NOPs (the host CPU has no clwb)");
    }
    let (z, ze) = rebuild_runtime::cpu::clzero();
    if z + ze > 0 {
        println!("cpu       {z} clzero sites detoured, {ze} executions emulated (AMD-only; the host CPU has none)");
    }
    let (n, iid) = rebuild_runtime::device::unknown_iid();
    if n > 0 {
        println!("  QueryInterface: {n} asks for interfaces the device does not serve; last {iid:02x?}");
    }
}

/// Title DLLs in an order where each comes after the title DLLs it imports.
fn dependency_order(images: &[Loaded]) -> Vec<usize> {
    let names: Vec<String> = images.iter().map(|l| l.name.clone()).collect();
    let mut done: HashSet<usize> = HashSet::new();
    let mut order = Vec::new();
    fn visit(i: usize, images: &[Loaded], names: &[String], done: &mut HashSet<usize>, order: &mut Vec<usize>, depth: usize) {
        if done.contains(&i) || depth > 64 {
            return;
        }
        done.insert(i);
        for m in &images[i].image.imports {
            if let Some(j) = names.iter().position(|n| *n == m.dll.to_ascii_lowercase()) {
                visit(j, images, names, done, order, depth + 1);
            }
        }
        order.push(i);
    }
    for i in 0..images.len() {
        visit(i, images, &names, &mut done, &mut order, 0);
    }
    order
}

/// Writes to stderr with one WriteFile: no Rust lock, no allocation (the title may hold the heap lock).
fn raw_err(b: &[u8]) {
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    unsafe {
        if let Ok(h) = GetStdHandle(STD_ERROR_HANDLE) {
            let _ = WriteFile(h, Some(b), None, None);
        }
    }
}

/// Keeps a run from taking the machine down (it did, more than once, before these guards):
///
/// - a **job object** holding this process, killed when the job's last handle closes (at exit, however
///   the process ends), with a cap on this process's own committed memory;
/// - a **watchdog thread** that ends the process at the deadline plus a margin, whatever any other
///   thread is doing (it takes no lock: a sleep, then TerminateProcess);
/// - a **preflight**: no run when the system has too little commit left for the title's budget.
fn guard(seconds: u64) -> Result<(), String> {
    use windows::Win32::System::JobObjects::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    const GIB: u64 = 1 << 30;
    // user-set hardware limits first: the budget reads the RAM limit
    rebuild_runtime::limits::set(rebuild_runtime::limits::Limits::from_env());
    let budget = rebuild_runtime::budget::limit();
    let avail = rebuild_runtime::budget::host_commit_available();
    // the budget (physical pages, charged to the system) plus our own private memory, plus room for Windows
    let own = rebuild_runtime::budget::private_cap();
    if avail < own + 4 * GIB {
        return Err(format!(
            "the system can commit {:.1} GiB more; a run needs {:.1} GiB (budget {:.1} + own {:.1} + 4 for Windows). Close programs or lower REBUILD_MEMORY_BUDGET_MB",
            avail as f64 / GIB as f64,
            (own + 4 * GIB) as f64 / GIB as f64,
            budget as f64 / GIB as f64,
            4.0
        ));
    }
    unsafe {
        let job = CreateJobObjectW(None, None).map_err(|e| format!("job object: {e}"))?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        info.ProcessMemoryLimit = own as usize;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("job limits: {e}"))?;
        AssignProcessToJobObject(job, GetCurrentProcess()).map_err(|e| format!("job assign: {e}"))?;
        for line in rebuild_runtime::limits::apply_cpu(job)? {
            println!("limit     {line}");
        }
        for line in rebuild_runtime::limits::get().describe() {
            println!("limit     set: {line}");
        }
        // the handle is never closed: it closes when the process ends, and the job kills what is left
        let _ = job;
    }
    let hard = Duration::from_secs(seconds + 15);
    std::thread::Builder::new()
        .name("watchdog".into())
        .stack_size(64 << 10)
        .spawn(move || {
            std::thread::sleep(hard);
            raw_err(b"WATCHDOG  deadline passed: ending the process
");
            unsafe {
                let _ = TerminateProcess(GetCurrentProcess(), 0xDEAD);
            }
        })
        .map_err(|e| format!("watchdog: {e}"))?;
    println!(
        "guard     job (killed on exit, own memory <= {:.1} GiB), title budget {:.1} GiB, watchdog at {} s",
        own as f64 / GIB as f64,
        budget as f64 / GIB as f64,
        seconds + 15
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let run = args.iter().any(|a| a == "--run");
    // the sampler suspends the title's thread: opt-in only (`--sample N`), never by default
    let sample: Option<u64> = args.iter().position(|a| a == "--sample").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok());
    let every = sample.unwrap_or(2);
    let seconds: u64 = args.iter().position(|a| a == "--seconds").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(60);
    let Some(dir) = std::env::var_os("REBUILD_GAME_DIR").map(PathBuf::from) else {
        println!("FAILED    set REBUILD_GAME_DIR to the title's folder (it is only read)");
        std::process::exit(1);
    };
    let cache = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("triumph-rebuild").join("images");

    let title = match load_title(&dir, &cache) {
        Ok(t) => t,
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    };
    let exe = title.images.iter().find(|l| l.name.ends_with(".exe")).expect("the title's exe");
    println!("runtitle — Build 3: the title loaded by the Windows loader (prepared images), imports bound");
    println!(
        "loaded    {} images; {} imports bound ({} from the title's own DLLs); {} bound to trap thunks (unbuilt)",
        title.images.len(),
        title.bound,
        title.from_title,
        title.trapped.len()
    );
    if !run {
        return;
    }
    crash::install(title.images.iter().map(|l| (l.name.clone(), l.base, l.image.size_of_image as u64)).collect());
    crash::on_fatal(device_report);
    // REBUILD_WATCH: who writes an address (hardware watchpoint on every thread)
    watch::install();
    // console CPU instructions the host lacks (clwb), patched where the title uses them; first in the handler line
    rebuild_runtime::cpu::install();
    // the title's code assumes its TLS block is slot 0 (an exe's): give it slot 0 before any thread runs
    if let Some(tls) = &exe.image.tls {
        let index = (exe.base + (tls.index_va - exe.image.image_base)) as *mut u32;
        let callbacks: Vec<usize> = tls.callbacks.iter().map(|r| (exe.base + *r as u64) as usize).collect();
        match unsafe { rebuild_runtime::tls::give_slot0(index, exe.base as usize, &callbacks) } {
            Ok(0) => println!("tls       the title already has slot 0"),
            Ok(k) => println!("tls       the title's block moved from slot {k} to slot 0 on every thread; ours to {k}"),
            Err(e) => {
                println!("FAILED    tls: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = guard(seconds) {
        println!("REFUSED   {e}");
        std::process::exit(2);
    }

    match rebuild_runtime::drives::install(&rebuild_runtime::drives::Drives::new(&dir)) {
        Ok(_) => println!("drives    G: the title's folder (read-only), T: emptied, S: storage; files and DirectStorage on them"),
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    }
    set_command_line(&format!(r"G:\{}", exe.name));
    // the title's own allocator for XMemAlloc, if its exe exports one
    if let Some(e) = exe.image.exports.iter().find(|e| e.name.as_deref() == Some("__XMemAllocationHooks__")) {
        if let Some(rva) = e.rva {
            let hooks = (exe.base + rva as u64) as *const [u64; 2];
            let [a, f] = unsafe { hooks.read() };
            set_alloc_hooks(a as usize, f as usize);
            println!("hooks     the exe's __XMemAllocationHooks__ installed");
        }
    }
    unsafe { STARTED = Some(Instant::now()) };
    on_create_device(reached_device);

    // DLL entry points, dependencies first
    type DllMain = unsafe extern "system" fn(u64, u32, u64) -> i32;
    for i in dependency_order(&title.images) {
        let l = &title.images[i];
        if l.name.ends_with(".exe") || l.image.entry_rva == 0 {
            continue;
        }
        let entry: DllMain = unsafe { std::mem::transmute((l.base + l.image.entry_rva as u64) as usize) };
        let ok = unsafe { entry(l.base, 1, 0) };
        println!("dllmain   {:<40} {}", l.name, if ok != 0 { "ok" } else { "FAILED" });
    }
    // the exe's TLS callbacks, then its entry on a thread with the stack it asks for
    if let Some(tls) = &exe.image.tls {
        for rva in &tls.callbacks {
            let cb: DllMain = unsafe { std::mem::transmute((exe.base + *rva as u64) as usize) };
            unsafe { cb(exe.base, 1, 0) };
        }
        println!("tls       {} callbacks of the exe run", tls.callbacks.len());
    }
    let entry = exe.base + exe.image.entry_rva as u64;
    let stack = (exe.image.stack_reserve as usize).max(8 << 20);
    let (tx, rx) = mpsc::channel();
    println!("entry     the exe's entry point on a {} MiB stack; waiting up to {seconds} s for the first render call", stack >> 20);
    let main_thread = std::thread::Builder::new()
        .name("title-main".into())
        .stack_size(stack)
        .spawn(move || {
            let main: unsafe extern "system" fn() -> i32 = unsafe { std::mem::transmute(entry as usize) };
            let code = unsafe { main() };
            let _ = tx.send(code);
        })
        .expect("title thread");
    let images: Vec<(String, u64, u64)> = title.images.iter().map(|l| (l.name.clone(), l.base, l.image.size_of_image as u64)).collect();
    let handle = windows::Win32::Foundation::HANDLE(main_thread.as_raw_handle());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last: Vec<u64> = Vec::new();
    let outcome = loop {
        let wait = Duration::from_secs(every).min(deadline.saturating_duration_since(Instant::now()));
        raw_err(b"beat: waiting\n");
        match rx.recv_timeout(wait) {
            Ok(code) => break Some(code),
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            break None;
        }
        if sample.is_none() {
            continue;
        }
        // heartbeats on stderr (one WriteFile, no Rust lock, no allocation): where the main thread is
        raw_err(b"beat: sampling\n");
        let sampled = sampler::sample(handle);
        raw_err(b"beat: sampled\n");
        if let Some(s) = sampled {
            let frames = s.frames[..s.len].to_vec();
            let t = unsafe { STARTED }.unwrap().elapsed().as_secs_f64();
            if frames == last {
                println!("sample    {t:>5.1} s  title-main: same stack");
            } else {
                println!("sample    {t:>5.1} s  title-main:");
                for a in &frames {
                    println!("            {}", sampler::name(*a, &images));
                }
                last = frames;
            }
        }
    };
    match outcome {
        Some(code) => {
            println!("EXITED    the title's entry returned {code:#x} before a render call");
            write_summary("exited", &format!("entry returned {code:#x} before a render call"));
        }
        None if REACHED.load(std::sync::atomic::Ordering::Relaxed) => {
            println!("DEADLINE  {seconds} s with the device");
            write_summary("deadline", &format!("ran {seconds} s with the device"));
        }
        None => {
            println!("TIMEOUT   no render call within {seconds} s");
            // where every thread stands (a hang inside a call we serve shows here)
            crash::other_threads();
            write_summary("timeout", &format!("no render call within {seconds} s"));
        }
    }
    device_report();
    if sample.is_some() {
        println!("threads   every thread of the process, innermost frames first:");
        for (id, h) in sampler::threads() {
            if let Some(s) = sampler::sample(h) {
                let top: Vec<String> = s.frames[..s.len.min(8)].iter().map(|a| sampler::name(*a, &images)).collect();
                println!("  {id:>6}  {}", top.join("  <  "));
            }
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(h);
            }
        }
    }
    report(unsafe { STARTED }.unwrap());
    std::process::exit(0);
}
