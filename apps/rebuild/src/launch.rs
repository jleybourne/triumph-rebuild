//! Starting the title: shared by `rebuild.exe --config` (the launcher's path) and `runtitle` (by hand).
//!
//! Load the title through the Windows loader (prepared images), bind its imports, and run its startup past
//! `D3D12XboxCreateDevice` (the first render call) until the deadline (`--seconds N`; none from
//! `rebuild.exe` unless asked), then print the discovery list: every unbuilt import the title called, in
//! first-call order. The game folder is only read; prepared copies go to the game's per-build `images` cache
//! (`rebuild_storage`, `docs/design/storage.md`), and files from before that layout are migrated first.
//!
//! With `--sample N` (opt-in: it suspends a thread), a sampler stops the title's main thread every N seconds and prints
//! its stack as image+rva, so a title that waits shows where. At the end, every thread is sampled once.
//! File and DirectStorage calls go through the drives (`rebuild_runtime::drives`): G: the title's folder,
//! read-only; T: emptied at start; S: storage, kept.
//!
//! The session folder is `REBUILD_SESSION_DIR` (read at start): the crash report (`crash-<pid>.txt`) and
//! `summary.toml` go there.

use crate::{crash, patch, profile, sampler, watch};

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
    if let Some(line) = rebuild_runtime::sandbox::blocked_report() {
        println!("{line}");
    }
    let g = rebuild_runtime::gdk_discovery();
    if !g.is_empty() {
        println!("gdk runtime discovery: {}", g.len());
        for l in g {
            println!("          {l}");
        }
    }
    if let Some(r) = rebuild_runtime::gameinput::report() {
        println!("{r}");
    }
    let tw = rebuild_runtime::titlewindow::count();
    if tw > 0 {
        println!("window    the title made {tw} windows ({} message-only), none shown", rebuild_runtime::titlewindow::message_only_count());
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

/// Seconds since the title started (0 before).
pub fn seconds_in() -> f64 {
    unsafe { STARTED }.map_or(0.0, |s| s.elapsed().as_secs_f64())
}

fn toml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `<REBUILD_SESSION_DIR>\summary.toml`: how the run ended, for the launcher's session list. The matrix run
/// it belongs to (`REBUILD_MATRIX_ID`, set by the launcher) is recorded with it.
pub fn write_summary(outcome: &str, detail: &str) {
    let Some(dir) = std::env::var_os("REBUILD_SESSION_DIR") else { return };
    let elapsed = unsafe { STARTED }.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
    let limits = rebuild_runtime::limits::get();
    let matrix = std::env::var("REBUILD_MATRIX_ID").map(|m| format!("matrix = \"{}\"\n", toml_str(&m))).unwrap_or_default();
    let text = format!(
        "[summary]\noutcome = \"{outcome}\"\ndetail = \"{}\"\nseconds = {elapsed:.3}\n{matrix}{}\
         [title]\nbudget_mib = {}\nbudget_used_mib = {}\n\
         [limits]\nram_mb = {}\nvram_mb = {}\ncpu_cores = {}\ncpu_rate = {}\n",
        toml_str(detail),
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
    match rebuild_dstorage::stats() {
        Some(d) => println!(
            "dstorage  {} reads, {} failed, {} MiB in, {} MiB out, {} zlib, {} bcpack refused, {} staging waits",
            d.reads, d.failed, d.bytes_in >> 20, d.bytes_out >> 20, d.zlib_reads, d.bcpack_refused, d.staging_waits
        ),
        None => println!("dstorage  the title has not asked for DirectStorage"),
    }
    if let Some(d) = rebuild_dstorage::stats() {
        let t = d.times;
        let per = |ns: u64| if d.reads == 0 { 0.0 } else { ns as f64 / d.reads as f64 / 1e6 };
        println!(
            "dstorage  per read ms: latency {:.2} (max {:.1}) = unsubmitted {:.2} + ready {:.2} + read {:.2} + decode {:.2} + pages {:.2}; {} statuses avg {:.2} ms (max {:.1}); page frame waits {}, misses {}",
            per(t.latency_ns),
            t.latency_max_ns as f64 / 1e6,
            per(t.unsubmitted_ns),
            per(t.ready_ns),
            per(t.read_ns),
            per(t.decode_ns),
            per(t.pages_ns),
            t.statuses,
            if t.statuses == 0 { 0.0 } else { t.status_ns as f64 / t.statuses as f64 / 1e6 },
            t.status_max_ns as f64 / 1e6,
            rebuild_runtime::drives::FRAME_WAITS.load(std::sync::atomic::Ordering::Relaxed),
            rebuild_runtime::drives::FRAME_MISSES.load(std::sync::atomic::Ordering::Relaxed),
        );
    }
    if let Some(dred) = rebuild_runtime::device::dred_report() {
        print!("{dred}");
    }
    {
        use rebuild_runtime::submit::{BAD, FOREIGN, LISTS, REEXECUTED};
        use std::sync::atomic::Ordering::Relaxed;
        println!(
            "submit    {} lists executed, {} not ours, {} re-executions waited for their earlier run, {} host lists not run (Close failed; {} Close failures)",
            LISTS.load(Relaxed), FOREIGN.load(Relaxed), REEXECUTED.load(Relaxed), BAD.load(Relaxed),
            rebuild_xlist::console::CLOSE_FAILED.load(Relaxed)
        );
        {
            use rebuild_runtime::submit::*;
            println!(
                "work      {} draws, {} dispatches recorded; dropped: {} no host pipeline, {} topology outside the pipeline's type, {} other; {} clears, {} copies; {} render targets with no host view",
                RUN_DRAWS.load(Relaxed),
                RUN_DISPATCHES.load(Relaxed),
                RUN_SKIPPED_PIPELINE.load(Relaxed),
                RUN_SKIPPED_TOPOLOGY.load(Relaxed),
                RUN_SKIPPED_OTHER.load(Relaxed),
                RUN_CLEARS.load(Relaxed),
                RUN_COPIES.load(Relaxed),
                RUN_RT_UNMAPPED.load(Relaxed)
            );
            println!(
                "lists     {} rectangle-list draws expanded to triangles, {} dropped (no input layout, vertices not from buffers, or indexed)",
                rebuild_xlist::console::RECTS_EXPANDED.load(Relaxed),
                rebuild_xlist::console::RECTS_DROPPED.load(Relaxed)
            );
            println!(
                "titlemem  {} of the title's allocations opened for the GPU in place, {} could not be; lookups: {} answered from an allocation confirmed since the last free or remap, {} queried the address space",
                rebuild_runtime::titlemem::OPENED.load(Relaxed),
                rebuild_runtime::titlemem::OPEN_FAILED.load(Relaxed),
                rebuild_runtime::titlemem::CACHED.load(Relaxed),
                rebuild_runtime::titlemem::QUERIED.load(Relaxed)
            );
            println!("titlemem  {}", rebuild_runtime::titlemem::why_report());
            println!("{}", rebuild_runtime::memory::report());
            println!("{}", rebuild_runtime::memory::vram_line());
            println!("cputex    {}", rebuild_runtime::cputex::report());
            println!("bink      {}", rebuild_runtime::bink::report());
            println!("pagemap   {}", rebuild_runtime::pagemap::report());
            println!(
                "gputime   {} GPU timestamp labels written into the title's memory; {} host-list timing pairs read, {} dropped",
                rebuild_xlist::console::STAMP_LABELS.load(Relaxed),
                rebuild_runtime::gputime::READ.load(Relaxed),
                rebuild_runtime::gputime::DROPPED.load(Relaxed)
            );
        }
        let ir = &rebuild_xlist::console::INDIRECT_REFUSED;
        println!("lists     {} indirect calls with no host signature on a graphics pipeline, {} on a compute one", ir[0].load(Relaxed), ir[1].load(Relaxed));
        {
            use rebuild_xlist::console::indirect::*;
            let n: Vec<u64> = rebuild_xlist::console::INDIRECT.iter().map(|c| c.load(Relaxed)).collect();
            println!(
                "lists     {} indirect calls ({} dropped: no host pipeline); by work: {} draw, {} draw-indexed, {} dispatch, {} mesh, {} none, {} other; {} with root arguments under the library root (mapped); refused: {} side, {} buffer views, {} unmappable, {} wrong root",
                n[CALLS], n[NO_PIPELINE], n[1], n[2], n[3], n[4], n[0], n[5], n[MAPPED], n[SIDE], n[BUFFER_VIEWS], n[UNMAPPABLE], n[WRONG_ROOT]
            );
        }
        {
            let (ticks, work, lists) = rebuild_xlist::console::cost::totals();
            let ms = rebuild_xlist::console::cost::ms(ticks);
            println!(
                "xlate     lists translated {} (REBUILD_XLATE): {ms:.1} ms in the list functions over {lists} lists, {work} draws and dispatches ({:.2} µs each, list costs included); descriptor sync on submit {:.1} ms",
                if rebuild_xlist::console::mode::at_close() { "at Close" } else { "as each call arrives" },
                ms * 1000.0 / work.max(1) as f64,
                rebuild_xlist::console::cost::ms(rebuild_runtime::submit::SYNC_TICKS.load(Relaxed))
            );
            if rebuild_xlist::console::mode::at_close() {
                println!("xlate     {}", rebuild_xlist::console::mix::report());
            }
            println!("xlate     {}", rebuild_xlist::console::pool::report());
            for l in rebuild_xlist::console::by_slot::report().unwrap_or_default() {
                println!("xslot     {l}");
            }
        }
        println!("lists     {} draws and dispatches dropped: a library pipeline with no shadow table bound", rebuild_xlist::console::NO_SHADOW.load(Relaxed));
        println!("lists     {} descriptor tables in no bound heap (user data 0)", rebuild_xlist::console::NO_HEAP_TABLES.load(Relaxed));
        println!("views     {} UAVs of resources without unordered access written as null views", rebuild_d3d12x::heap::NO_UAV.load(Relaxed));
        println!("views     {} depth textures with UAVs given a twin, {} depth/twin copies (docs/design/depth-uav.md)", rebuild_d3d12x::resource::TWINS.load(Relaxed), rebuild_xlist::console::TWIN_COPIES.load(Relaxed));
        if let Some(r) = rebuild_runtime::device::resources() {
            for l in r.view_page_report() {
                println!("views     {l}");
            }
        }
    }
    let t =rebuild_runtime::device::traps();
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
    println!("pacing    {}", rebuild_runtime::frame::describe());
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
    let w = rebuild_runtime::cpu::wait_hints();
    if w > 0 {
        println!("cpu       {w} monitorx/mwaitx sites patched (AMD-only wait hints)");
    }
    let (z, ze) = rebuild_runtime::cpu::clzero();
    if z + ze > 0 {
        println!("cpu       {z} clzero sites detoured, {ze} executions emulated (AMD-only; the host CPU has none)");
    }
    let s4 = rebuild_runtime::cpu::sse4a();
    if s4 > 0 {
        println!("cpu       {s4} insertq/extrq executions emulated (AMD SSE4a; the host CPU has none)");
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
/// - with a deadline, a **watchdog thread** that ends the process at the deadline plus a margin, whatever
///   any other thread is doing (it takes no lock: a sleep, then TerminateProcess);
/// - a **preflight**: no run when the system has too little commit left for the title's budget.
fn guard(seconds: Option<u64>) -> Result<(), String> {
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
    if let Some(seconds) = seconds {
        let hard = Duration::from_secs(seconds + 15);
        std::thread::Builder::new()
            .name("watchdog".into())
            .stack_size(64 << 10)
            .spawn(move || {
                std::thread::sleep(hard);
                raw_err(b"WATCHDOG  deadline passed: ending the process\n");
                unsafe {
                    let _ = TerminateProcess(GetCurrentProcess(), 0xDEAD);
                }
            })
            .map_err(|e| format!("watchdog: {e}"))?;
    }
    println!(
        "guard     job (killed on exit, own memory <= {:.1} GiB), title budget {:.1} GiB, {}",
        own as f64 / GIB as f64,
        budget as f64 / GIB as f64,
        match seconds {
            Some(s) => format!("watchdog at {} s", s + 15),
            None => "no deadline".to_string(),
        }
    );
    Ok(())
}

/// One run of the title.
#[derive(Debug, Clone)]
pub struct Options {
    /// the title's folder (only read)
    pub game: PathBuf,
    /// run its startup; `false` = load and bind only
    pub run: bool,
    /// end the run after this many seconds; `None` = until the title exits or the process is ended
    pub seconds: Option<u64>,
    /// sample the title's main thread every N seconds (opt-in: it suspends a thread)
    pub sample: Option<u64>,
    /// the BCPack format tables (`dstorage.bcpack_tables`), for the drives
    pub bcpack_tables: Option<PathBuf>,
}

/// A run that could not start: printed, recorded in the session's summary, and the process ends with `code`.
pub fn fail(code: i32, why: &str) -> ! {
    println!("FAILED    {why}");
    write_summary("failed", why);
    std::process::exit(code);
}

/// The name of the mutex a title run holds for its whole life (the OS releases it when the process ends, crash
/// included, so it is never stale).
pub const RUN_MUTEX: &str = rebuild_storage::RUN_MUTEX;

/// Hold [`RUN_MUTEX`] or exit 4: another title run is in progress.
fn single_run() {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = HSTRING::from(RUN_MUTEX);
    match unsafe { CreateMutexW(None, true, &name) } {
        Ok(h) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                fail(4, "another title run is in progress on this machine (one at a time): not starting");
            }
            // a HANDLE has no destructor: never closed, so held until the process ends
            let _ = h;
        }
        Err(e) => eprintln!("run lock: could not create {RUN_MUTEX} ({e}); starting without it"),
    }
}

/// Load the title and run it. Never returns: the process ends with the run.
pub fn run(o: Options) -> ! {
    let Options { game: dir, run, seconds, sample, bcpack_tables } = o;
    // one title run at a time on this machine, enforced here rather than by scripts: a second run would empty the
    // shared T: overlay under the first and share the GPU with it
    single_run();
    // the sampler suspends the title's thread: opt-in only (`--sample N`), never by default
    let every = sample.unwrap_or(2);
    if !dir.is_dir() {
        fail(1, &format!("the game folder {} is not a folder that exists", dir.display()));
    }
    // debug: the offline online-mode sandbox. A loud line when it is on, so no run is mistaken for a real
    // online session; the runtime layer blocks every non-loopback connection while it is.
    rebuild_runtime::sandbox::announce();
    // host lists reset ahead of time, off the title's recording threads (REBUILD_LIST_POOL=1; off: reset as they record)
    rebuild_runtime::submit::enable_list_pool();
    // where this game's files live: its id and build from its manifest (or the launcher's), set for the whole
    // process before anything opens a cache; files from before this layout are moved in first (saves copied)
    let st = rebuild_storage::storage();
    let store = rebuild_storage::game_for(&dir);
    rebuild_storage::set_current(store.clone());
    println!("storage   {} ({}): game {} build {}", st.user.display(), st.mode.label(), store.id, store.build);
    for n in &st.notes {
        println!("storage   {n}");
    }
    let legacy = rebuild_storage::migrate::Legacy::from_env(&rebuild_storage::Env::from_process());
    let moved = rebuild_storage::migrate::game_data(st, &store, &legacy);
    for l in &moved.lines {
        println!("migrate   {l}");
    }
    for (from, to) in &moved.moved {
        println!("migrate   {} -> {}", from.display(), to.display());
    }
    for (p, why) in &moved.kept {
        println!("migrate   kept {}: {why}", p.display());
    }
    let cache = store.images_dir();

    let title = match load_title(&dir, &cache) {
        Ok(t) => t,
        Err(e) => fail(1, &e.to_string()),
    };
    let Some(exe) = title.images.iter().find(|l| l.name.ends_with(".exe")) else {
        fail(1, &format!("no .exe was loaded from {}", dir.display()))
    };
    println!("launch    the title loaded by the Windows loader (prepared images), imports bound");
    println!(
        "loaded    {} images; {} imports bound ({} from the title's own DLLs); {} bound to trap thunks (unbuilt)",
        title.images.len(),
        title.bound,
        title.from_title,
        title.trapped.len()
    );
    if !run {
        std::process::exit(0);
    }
    crash::install(title.images.iter().map(|l| (l.name.clone(), l.base, l.image.size_of_image as u64)).collect());
    // REBUILD_PATCH: diagnostic code patches, before the title runs
    patch::apply(&title.images.iter().map(|l| (l.name.clone(), l.base, l.image.size_of_image as u64)).collect::<Vec<_>>());
    crash::on_fatal(device_report);
    // REBUILD_WATCH: who writes an address (hardware watchpoint on every thread)
    watch::install();
    // console CPU instructions the host lacks (clwb), patched where the title uses them; first in the handler line
    rebuild_runtime::cpu::install();
    // cold slices of watched memory: the I/O syscalls guarded and the write-fault handler first in line
    rebuild_runtime::memory::install_cold();
    // the title's code assumes its TLS block is slot 0 (an exe's): give it slot 0 before any thread runs
    if let Some(tls) = &exe.image.tls {
        let index = (exe.base + (tls.index_va - exe.image.image_base)) as *mut u32;
        let callbacks: Vec<usize> = tls.callbacks.iter().map(|r| (exe.base + *r as u64) as usize).collect();
        match unsafe { rebuild_runtime::tls::give_slot0(index, exe.base as usize, &callbacks) } {
            Ok(0) => println!("tls       the title already has slot 0"),
            Ok(k) => println!("tls       the title's block moved from slot {k} to slot 0 on every thread; ours to {k}"),
            Err(e) => fail(1, &format!("tls: {e}")),
        }
    }
    if let Err(e) = guard(seconds) {
        println!("REFUSED   {e}");
        write_summary("refused", &e);
        std::process::exit(2);
    }

    let drives = rebuild_runtime::drives::Drives { bcpack_tables, ..rebuild_runtime::drives::Drives::in_store(dir.clone(), &store) };
    match rebuild_runtime::drives::install(&drives) {
        Ok(_) => println!("drives    G: the title's folder (read-only), T: emptied, S: storage; files and DirectStorage on them"),
        Err(e) => fail(1, &e),
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
    match seconds {
        Some(s) => println!("entry     the exe's entry point on a {} MiB stack; the run ends at {s} s", stack >> 20),
        None => println!("entry     the exe's entry point on a {} MiB stack; the run ends with the title", stack >> 20),
    }
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
    // REBUILD_PROFILE=<ms>: every thread sampled, and DirectStorage throughput once a second
    profile::start();
    let handle = windows::Win32::Foundation::HANDLE(main_thread.as_raw_handle());
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let mut last: Vec<u64> = Vec::new();
    let outcome = loop {
        let mut wait = Duration::from_secs(every);
        if let Some(d) = deadline {
            wait = wait.min(d.saturating_duration_since(Instant::now()));
        }
        raw_err(b"beat: waiting\n");
        match rx.recv_timeout(wait) {
            Ok(code) => break Some(code),
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
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
    let reached = REACHED.load(std::sync::atomic::Ordering::Relaxed);
    let secs = seconds.map_or_else(|| format!("{:.0}", unsafe { STARTED }.unwrap().elapsed().as_secs_f64()), |s| s.to_string());
    match outcome {
        Some(code) => {
            let when = if reached { "after" } else { "before" };
            println!("EXITED    the title's entry returned {code:#x} {when} a render call");
            write_summary("exited", &format!("entry returned {code:#x} {when} a render call"));
        }
        None if reached => {
            println!("DEADLINE  {secs} s with the device");
            write_summary("deadline", &format!("ran {secs} s with the device"));
        }
        None => {
            println!("TIMEOUT   no render call within {secs} s");
            // where every thread stands (a hang inside a call we serve shows here)
            crash::other_threads();
            write_summary("timeout", &format!("no render call within {secs} s"));
        }
    }
    device_report();
    profile::report(&images);
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
