//! Build 5: the frame line and the run's summary (docs/design/launcher.md, "L6 built": the session folder
//! contract).
//!
//! The title's threads only add to counters (relaxed atomics, no lock, no allocation). A thread of ours, started
//! with the device, wakes once a second and:
//!
//! - appends one frame line to `REBUILD_FRAME_LOG` (when set), `|`-separated `key value [unit]` segments as
//!   `apps/launcher/src/frameline.rs` reads them;
//! - looks for `<REBUILD_SESSION_DIR>/capture.request` and asks the present for a capture
//!   ([`crate::present::request_capture`]);
//! - at `REBUILD_RUN_SECONDS` (from process start), writes `<REBUILD_SESSION_DIR>/summary.toml` and ends the
//!   process, as the contract asks.
//!
//! **The fields.** Per frame (title presents in the second): `ms` (mean interval between presents), `gpu` (the
//! title's GPU time: the union of its host lists' timestamp pairs over every direct and compute queue, measured
//! per frame and read a few frames late, [`crate::gputime`]; the mean of the frames finished in the second),
//! `draws` and `dispatches` (what the submitted lists recorded, `ListStats`), `dropped`
//! (draws and dispatches the lists dropped because their pipeline has no host object, `ListStats::skipped`),
//! `wait` (the title's time in frame-event waits and blocking fence waits). Per second: `fps`, `late` (presents
//! not shown because the display queue was full or our copy of an earlier frame had not finished: the title is
//! never blocked for the display), `held` (memory waits the bridge kept, see [`crate::submit`]). Then `internal ->
//! output`: the title's back buffer and the swap chain. `upload`, `xlate` and `upscale` are not measured yet and
//! stay absent. Segments the launcher does not know are ignored by it.
//!
//! **The summary** (`[summary]`): `avg_fps`, `low1_fps` (the 99th percentile frame time as fps, from a 0.25 ms
//! histogram), `frame_ms`, `gpu_ms` (as the line's `gpu`), `gpu_sum_ms` (the lists' times summed: overlapping
//! queues counted twice), `gpu_span_ms` (the present queue's span from a frame's first submission to the end of
//! the present's copy: idle gaps included), `frames`, `seconds` and `measured_from` over the measuring window:
//! `REBUILD_MEASURE_FROM=title_start` (the default) starts it at the first present, `after_loading` at the first
//! frame with a draw. Then `frame_rate` (`title`, `cap N`, `unlimited`) and `vsync`, as they were at the end.
//!
//! **The header.** Before the first frame line, and again whenever it changes: the pacing as
//! [`crate::frame::describe`] words it (`frame rate unlimited (...) | vsync off (tearing allowed)`). It carries
//! no frame field, so the launcher reads it as a startup message.

#![cfg(windows)]

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static FRAMES: AtomicU64 = AtomicU64::new(0);
static LATE: AtomicU64 = AtomicU64::new(0);
static DRAWS: AtomicU64 = AtomicU64::new(0);
static DISPATCHES: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
/// Draws submitted since the last present (whether the frame had any, for `after_loading`).
static FRAME_DRAWS: AtomicU64 = AtomicU64::new(0);
static WAIT_US: AtomicU64 = AtomicU64::new(0);
static GPU_US: AtomicU64 = AtomicU64::new(0);
static GPU_N: AtomicU64 = AtomicU64::new(0);
/// Frame intervals (µs) summed, and how many.
static FRAME_US: AtomicU64 = AtomicU64::new(0);
static FRAME_N: AtomicU64 = AtomicU64::new(0);
/// The last present, ns after [`start`]'s base (0: none yet).
static LAST_PRESENT: AtomicU64 = AtomicU64::new(0);
/// The present's own cost on the title's thread (ns), summed, and how many.
static PRESENT_NS: AtomicU64 = AtomicU64::new(0);
/// Sizes as `w << 32 | h`.
static INTERNAL: AtomicU64 = AtomicU64::new(0);
static OUTPUT: AtomicU64 = AtomicU64::new(0);

/// The measuring window: open, its start (ns after the base), its frames, frame-time sum and GPU sums.
static MEASURING: AtomicBool = AtomicBool::new(false);
static WINDOW_START: AtomicU64 = AtomicU64::new(0);
static WINDOW_FRAMES: AtomicU64 = AtomicU64::new(0);
static WINDOW_US: AtomicU64 = AtomicU64::new(0);
static WINDOW_GPU_US: AtomicU64 = AtomicU64::new(0);
static WINDOW_GPU_N: AtomicU64 = AtomicU64::new(0);
static WINDOW_GPU_SUM_US: AtomicU64 = AtomicU64::new(0);
static WINDOW_SPAN_US: AtomicU64 = AtomicU64::new(0);
static WINDOW_SPAN_N: AtomicU64 = AtomicU64::new(0);
const BUCKET_US: u64 = 250;
const BUCKETS: usize = 4000;
static HISTOGRAM: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];

/// The process's start, as an `Instant` (the frame line's `t` and `REBUILD_RUN_SECONDS` count from it).
static BASE: OnceLock<Instant> = OnceLock::new();

fn base() -> Instant {
    *BASE.get_or_init(|| Instant::now().checked_sub(process_age()).unwrap_or_else(Instant::now))
}

fn process_age() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.is_err() {
        return Duration::ZERO;
    }
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    Duration::from_nanos(t(now).saturating_sub(t(c)) * 100)
}

/// Time since the process started (the input script's clock).
pub fn elapsed() -> Duration {
    base().elapsed()
}

/// Frames the title has presented so far (shown or late).
pub fn frames() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

fn now_ns() -> u64 {
    base().elapsed().as_nanos() as u64
}

// ------------------------------------------------------------------------------------- what the title's threads add

/// A submission: what its lists recorded.
#[inline]
pub fn submitted(draws: u64, dispatches: u64, skipped: u64) {
    if draws > 0 {
        DRAWS.fetch_add(draws, Ordering::Relaxed);
        FRAME_DRAWS.fetch_add(draws, Ordering::Relaxed);
    }
    if dispatches > 0 {
        DISPATCHES.fetch_add(dispatches, Ordering::Relaxed);
    }
    if skipped > 0 {
        SKIPPED.fetch_add(skipped, Ordering::Relaxed);
    }
}

/// The title waited (frame events, blocking fence waits).
#[inline]
pub fn waited(d: Duration) {
    WAIT_US.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
}

/// A present was not shown (the display queue was full).
#[inline]
pub fn late() {
    LATE.fetch_add(1, Ordering::Relaxed);
}

/// The title's GPU time of a frame that finished: `us` its lists' union, `sum_us` their plain sum.
#[inline]
pub fn gpu(us: u64, sum_us: u64) {
    GPU_US.fetch_add(us, Ordering::Relaxed);
    GPU_N.fetch_add(1, Ordering::Relaxed);
    if MEASURING.load(Ordering::Relaxed) {
        WINDOW_GPU_US.fetch_add(us, Ordering::Relaxed);
        WINDOW_GPU_SUM_US.fetch_add(sum_us, Ordering::Relaxed);
        WINDOW_GPU_N.fetch_add(1, Ordering::Relaxed);
    }
}

/// The present queue's span of a frame that finished (first submission to the end of the present's copy).
#[inline]
pub fn gpu_span(us: u64) {
    if MEASURING.load(Ordering::Relaxed) {
        WINDOW_SPAN_US.fetch_add(us, Ordering::Relaxed);
        WINDOW_SPAN_N.fetch_add(1, Ordering::Relaxed);
    }
}

/// The title presented a frame (shown or late); `cost` is our time in the call.
pub fn presented(cost: Duration, internal: (u32, u32), output: (u32, u32)) {
    crate::gputime::collect();
    let now = now_ns();
    FRAMES.fetch_add(1, Ordering::Relaxed);
    PRESENT_NS.fetch_add(cost.as_nanos() as u64, Ordering::Relaxed);
    INTERNAL.store((u64::from(internal.0) << 32) | u64::from(internal.1), Ordering::Relaxed);
    OUTPUT.store((u64::from(output.0) << 32) | u64::from(output.1), Ordering::Relaxed);
    let drew = FRAME_DRAWS.swap(0, Ordering::Relaxed) > 0;
    let last = LAST_PRESENT.swap(now.max(1), Ordering::Relaxed);
    if !MEASURING.load(Ordering::Relaxed) && (drew || !after_loading()) {
        WINDOW_START.store(now, Ordering::Relaxed);
        MEASURING.store(true, Ordering::Relaxed);
        return;
    }
    if last == 0 {
        return;
    }
    let us = now.saturating_sub(last) / 1000;
    FRAME_US.fetch_add(us, Ordering::Relaxed);
    FRAME_N.fetch_add(1, Ordering::Relaxed);
    if MEASURING.load(Ordering::Relaxed) {
        WINDOW_FRAMES.fetch_add(1, Ordering::Relaxed);
        WINDOW_US.fetch_add(us, Ordering::Relaxed);
        HISTOGRAM[((us / BUCKET_US) as usize).min(BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
    }
}

fn after_loading() -> bool {
    static A: OnceLock<bool> = OnceLock::new();
    *A.get_or_init(|| std::env::var("REBUILD_MEASURE_FROM").is_ok_and(|v| v.trim() == "after_loading"))
}

/// Presents so far, and their mean cost on the title's thread.
pub fn present_cost() -> (u64, Duration) {
    let n = FRAMES.load(Ordering::Relaxed);
    (n, Duration::from_nanos(PRESENT_NS.load(Ordering::Relaxed) / n.max(1)))
}

// ------------------------------------------------------------------------------------- the line

/// One second of the frame line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    pub t: u64,
    pub fps: f64,
    pub ms: Option<f64>,
    pub gpu: Option<f64>,
    pub draws: Option<u64>,
    pub dispatches: Option<u64>,
    pub dropped: Option<u64>,
    pub wait: Option<f64>,
    pub late: u64,
    pub held: u64,
    pub internal: Option<(u32, u32)>,
    pub output: Option<(u32, u32)>,
    /// the upload ring (`crate::memory`): coordinator ms a pass, and MB sent a frame
    pub upload: Option<(f64, f64)>,
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The line as the launcher reads it.
pub fn format(l: &Line) -> String {
    let mut s = format!("t {}s | fps {:.1}", l.t, l.fps);
    if let Some(v) = l.ms {
        s += &format!(" | ms {v:.1}");
    }
    if let Some(v) = l.gpu {
        s += &format!(" | gpu {v:.1} ms");
    }
    if let Some(v) = l.draws {
        s += &format!(" | draws {}", thousands(v));
    }
    if let Some(v) = l.dispatches {
        s += &format!(" | dispatches {}", thousands(v));
    }
    if let Some(v) = l.dropped {
        s += &format!(" | dropped {}", thousands(v));
    }
    if let Some(v) = l.wait {
        s += &format!(" | wait {v:.1} ms");
    }
    s += &format!(" | late {} | held {}", l.late, l.held);
    if let Some((ms, mb)) = l.upload {
        s += &format!(" | upload {ms:.1} ms | upmb {mb:.2} MB");
    }
    match (l.internal, l.output) {
        (Some(i), Some(o)) => s += &format!(" | {}x{} -> {}x{}", i.0, i.1, o.0, o.1),
        (None, Some(o)) => s += &format!(" | {}x{}", o.0, o.1),
        _ => {}
    }
    s
}

/// Counter values at one moment, for deltas.
#[derive(Clone, Copy, Default)]
struct Counts {
    frames: u64,
    late: u64,
    draws: u64,
    dispatches: u64,
    skipped: u64,
    wait_us: u64,
    gpu_us: u64,
    gpu_n: u64,
    frame_us: u64,
    frame_n: u64,
    held: u64,
    /// the upload ring's totals: bytes, busy µs, passes
    up: (u64, u64, u64),
}

fn counts() -> Counts {
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    Counts {
        frames: r(&FRAMES),
        late: r(&LATE),
        draws: r(&DRAWS),
        dispatches: r(&DISPATCHES),
        skipped: r(&SKIPPED),
        wait_us: r(&WAIT_US),
        gpu_us: r(&GPU_US),
        gpu_n: r(&GPU_N),
        frame_us: r(&FRAME_US),
        frame_n: r(&FRAME_N),
        held: crate::submit::WAITS_BRIDGED.load(Ordering::Relaxed),
        up: crate::memory::upload_totals().map_or((0, 0, 0), |t| (t.0, t.1, t.2)),
    }
}

fn size(a: &AtomicU64) -> Option<(u32, u32)> {
    let v = a.load(Ordering::Relaxed);
    (v != 0).then_some(((v >> 32) as u32, v as u32))
}

fn line(t: u64, seconds: f64, a: &Counts, b: &Counts) -> Line {
    let frames = b.frames - a.frames;
    let per = |x: u64| (frames > 0).then(|| x / frames);
    let fn_ = b.frame_n - a.frame_n;
    let gn = b.gpu_n - a.gpu_n;
    Line {
        t,
        fps: frames as f64 / seconds.max(1e-3),
        ms: (fn_ > 0).then(|| (b.frame_us - a.frame_us) as f64 / fn_ as f64 / 1000.0),
        gpu: (gn > 0).then(|| (b.gpu_us - a.gpu_us) as f64 / gn as f64 / 1000.0),
        draws: per(b.draws - a.draws),
        dispatches: per(b.dispatches - a.dispatches),
        dropped: per(b.skipped - a.skipped),
        wait: (frames > 0).then(|| (b.wait_us - a.wait_us) as f64 / frames as f64 / 1000.0),
        late: b.late - a.late,
        held: b.held - a.held,
        internal: size(&INTERNAL),
        output: size(&OUTPUT),
        upload: crate::memory::get().map(|_| {
            let passes = (b.up.2 - a.up.2).max(1);
            ((b.up.1 - a.up.1) as f64 / passes as f64 / 1000.0, (b.up.0 - a.up.0) as f64 / 1e6 / frames.max(1) as f64)
        }),
    }
}

// ------------------------------------------------------------------------------------- the summary

/// The 99th percentile frame time of the window, in ms.
fn p99_ms() -> Option<f64> {
    let n: u64 = HISTOGRAM.iter().map(|b| u64::from(b.load(Ordering::Relaxed))).sum();
    if n == 0 {
        return None;
    }
    let want = n - n / 100;
    let mut seen = 0;
    for (i, b) in HISTOGRAM.iter().enumerate() {
        seen += u64::from(b.load(Ordering::Relaxed));
        if seen >= want {
            return Some(((i as u64 + 1) * BUCKET_US) as f64 / 1000.0);
        }
    }
    None
}

/// The frame keys of `[summary]` (no table header): what [`write_summary`] writes, for a caller that writes the
/// table itself (runtitle adds its outcome). `seconds` is left to such a caller when `with_seconds` is false.
pub fn summary_keys(with_seconds: bool) -> String {
    let frames = WINDOW_FRAMES.load(Ordering::Relaxed);
    let us = WINDOW_US.load(Ordering::Relaxed);
    let mut s = String::new();
    if frames > 0 && us > 0 {
        let ms = us as f64 / frames as f64 / 1000.0;
        s += &format!("avg_fps = {:.2}\nframe_ms = {ms:.3}\n", 1000.0 / ms);
        if let Some(p) = p99_ms() {
            s += &format!("low1_fps = {:.2}\n", 1000.0 / p);
        }
    }
    let gn = WINDOW_GPU_N.load(Ordering::Relaxed);
    if gn > 0 {
        s += &format!("gpu_ms = {:.3}\n", WINDOW_GPU_US.load(Ordering::Relaxed) as f64 / gn as f64 / 1000.0);
        s += &format!("gpu_sum_ms = {:.3}\n", WINDOW_GPU_SUM_US.load(Ordering::Relaxed) as f64 / gn as f64 / 1000.0);
    }
    let sn = WINDOW_SPAN_N.load(Ordering::Relaxed);
    if sn > 0 {
        s += &format!("gpu_span_ms = {:.3}\n", WINDOW_SPAN_US.load(Ordering::Relaxed) as f64 / sn as f64 / 1000.0);
    }
    s += &format!("frames = {}\n", FRAMES.load(Ordering::Relaxed));
    if with_seconds && MEASURING.load(Ordering::Relaxed) {
        s += &format!("seconds = {:.1}\n", now_ns().saturating_sub(WINDOW_START.load(Ordering::Relaxed)) as f64 / 1e9);
    }
    s += &format!("measured_from = \"{}\"\n", if after_loading() { "after_loading" } else { "title_start" });
    s += &format!("frame_rate = \"{}\"\nvsync = {}\n", crate::frame::frame_limit(), crate::frame::vsync());
    s
}

/// `<REBUILD_SESSION_DIR>/summary.toml` with the frame keys and `outcome`.
pub fn write_summary(outcome: &str) {
    let Some(dir) = std::env::var_os("REBUILD_SESSION_DIR") else { return };
    let text = format!("[summary]\noutcome = \"{outcome}\"\n{}", summary_keys(true));
    let _ = std::fs::write(std::path::Path::new(&dir).join("summary.toml"), text);
}

// ------------------------------------------------------------------------------------- the thread

static STARTED: AtomicBool = AtomicBool::new(false);
/// The last second's line (the in-game menu's stats), whether or not a log is written.
static LATEST: Mutex<Option<Line>> = Mutex::new(None);

/// The last second's frame line, once the first second has passed.
pub fn latest() -> Option<Line> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Start the once-a-second thread (with the device; later calls do nothing).
pub fn start() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = base();
    let _ = std::thread::Builder::new().name("frame-line".into()).stack_size(256 << 10).spawn(run);
}

fn run() {
    let log = std::env::var_os("REBUILD_FRAME_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
    let mut log = log.map(std::io::BufWriter::new);
    let session = std::env::var_os("REBUILD_SESSION_DIR").map(std::path::PathBuf::from);
    let run_seconds: Option<u64> = std::env::var("REBUILD_RUN_SECONDS").ok().and_then(|v| v.trim().parse().ok()).filter(|&s| s > 0);
    let request = session.as_ref().map(|s| s.join("capture.request"));
    let mut prev = counts();
    let mut prev_t = base().elapsed();
    let mut header = String::new();
    loop {
        // the next whole second after the process started
        let next = Duration::from_secs(prev_t.as_secs() + 1);
        std::thread::sleep(next.saturating_sub(base().elapsed()));
        let now_t = base().elapsed();
        let now = counts();
        let l = line(now_t.as_secs(), (now_t - prev_t).as_secs_f64(), &prev, &now);
        if let Some(w) = log.as_mut() {
            let pacing = crate::frame::describe();
            if pacing != header {
                let _ = writeln!(w, "{pacing}");
                header = pacing;
            }
            let _ = writeln!(w, "{}", format(&l));
            let _ = w.flush();
        }
        *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
        prev = now;
        prev_t = now_t;
        if let Some(r) = &request {
            if r.exists() {
                crate::present::request_capture(Some(r.clone()));
            }
        }
        if run_seconds.is_some_and(|s| now_t.as_secs() >= s) {
            write_summary("run_seconds");
            crate::present::shutdown();
            eprintln!("frame-line REBUILD_RUN_SECONDS reached: summary written, exiting");
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample both ends agree on: `apps/launcher` parses the same file in its own tests.
    pub const SAMPLE: &str = include_str!("frameline.sample");

    #[test]
    fn the_line_has_the_launchers_shape_and_reads_back() {
        let l = Line {
            t: 12,
            fps: 30.8,
            ms: Some(32.47),
            gpu: Some(21.04),
            draws: Some(4512),
            dispatches: Some(120),
            dropped: Some(0),
            wait: Some(6.0),
            late: 1,
            held: 2,
            internal: Some((2560, 1440)),
            output: Some((3840, 2160)),
            upload: None,
        };
        let s = format(&l);
        assert_eq!(s, SAMPLE.trim_end());
        // read back the way apps/launcher/src/frameline.rs does: segments, key, number (commas dropped)
        let mut got = std::collections::HashMap::new();
        let mut res = None;
        for seg in s.split('|') {
            let w: Vec<&str> = seg.split_whitespace().collect();
            if w.len() == 3 && w[1] == "->" {
                res = Some((w[0].to_string(), w[2].to_string()));
                continue;
            }
            let v: f64 = w[1].replace(',', "").trim_end_matches('s').parse().unwrap();
            got.insert(w[0].to_string(), v);
        }
        assert_eq!(got["fps"], 30.8);
        assert_eq!(got["ms"], 32.5);
        assert_eq!(got["gpu"], 21.0);
        assert_eq!(got["draws"], 4512.0);
        assert_eq!(got["dropped"], 0.0);
        assert_eq!(got["wait"], 6.0);
        assert_eq!(got["t"], 12.0);
        assert_eq!(res, Some(("2560x1440".into(), "3840x2160".into())));
        // a second with no frames: only what is known
        let quiet = format(&Line { t: 3, ..Default::default() });
        assert_eq!(quiet, "t 3s | fps 0.0 | late 0 | held 0");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(20_293_000), "20,293,000");
    }

    #[test]
    fn per_frame_fields_come_from_deltas() {
        let a = Counts::default();
        let b = Counts { frames: 30, draws: 30 * 4512, skipped: 30, wait_us: 30 * 6000, frame_us: 29 * 33_000, frame_n: 29, gpu_us: 20_000 * 28, gpu_n: 28, ..a };
        let l = line(5, 1.0, &a, &b);
        assert_eq!((l.fps, l.draws, l.dropped), (30.0, Some(4512), Some(1)));
        assert_eq!(l.ms, Some(33.0));
        assert_eq!(l.gpu, Some(20.0));
        assert_eq!(l.wait, Some(6.0));
        let none = line(5, 1.0, &a, &a);
        assert_eq!((none.ms, none.draws, none.gpu), (None, None, None));
    }
}
