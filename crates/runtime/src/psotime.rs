//! The pipeline timeline: when host pipelines are made, and when a title thread waits for one.
//!
//! A title thread that binds a pipeline whose host PSO is not ready either waits for it
//! (`REBUILD_PSO_FIRST_USE=wait`: created on that thread if no worker has started it, "inline", else waited for
//! from the pool) or, by default, skips the draw and asks the pool to make it next ("pending"). Every wait is
//! timed and attributed here; one over [`EVENT_MS`] gets its own stderr line (time, thread, kind, ms, pipeline).
//!
//! Once a second the frame-line thread prints a `psotl` line on stderr beside the frame rate: pipelines made in the
//! second, waits (count, ms, inline / pool), pending binds, the pool's queue, the longest wait still in progress
//! (a title thread stuck on a pipeline shows here while it is stuck, not only once it returns), and the process's
//! private commit against the job's memory limit (a RAM cap is not a pipeline stall). The title's hang watchdog
//! (exe+0x2215a60, a thread that sleeps 1 s and fires after 30 checks without a new frame heartbeat at
//! exe+0x71b6338; its fatal is the null write at exe+0x23cf683) is what a long stall here turns into.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A wait longer than this gets its own line.
pub const EVENT_MS: u64 = 50;

static WAITS: AtomicU64 = AtomicU64::new(0);
static WAIT_NS: AtomicU64 = AtomicU64::new(0);
static INLINE: AtomicU64 = AtomicU64::new(0);
static INLINE_NS: AtomicU64 = AtomicU64::new(0);
static PENDING: AtomicU64 = AtomicU64::new(0);
static LONGEST_NS: AtomicU64 = AtomicU64::new(0);

/// Waits in progress: (thread id, start in ns after the frame-line base); 0 marks a free slot.
const SLOTS: usize = 64;
static IN_WAIT: [(AtomicU64, AtomicU64); SLOTS] = [const { (AtomicU64::new(0), AtomicU64::new(0)) }; SLOTS];

fn now_ns() -> u64 {
    crate::framelog::elapsed().as_nanos() as u64
}

fn tid() -> u64 {
    // SAFETY: no arguments
    u64::from(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() })
}

/// What a wait was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// no worker had started it: created on the waiting thread
    Inline,
    /// a worker was making it: waited for
    Pool,
}

/// Times `f` (a blocking wait for a pipeline) as a wait of `kind` on this thread; `what` names the pipeline for the
/// event line.
pub fn wait<R>(kind: Kind, what: impl FnOnce() -> String, f: impl FnOnce() -> R) -> R {
    let me = tid();
    let start = now_ns();
    let slot = IN_WAIT.iter().position(|(t, _)| t.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed).is_ok());
    if let Some(i) = slot {
        IN_WAIT[i].1.store(start.max(1), Ordering::Release);
    }
    let t = Instant::now();
    let r = f();
    let ns = t.elapsed().as_nanos() as u64;
    if let Some(i) = slot {
        IN_WAIT[i].1.store(0, Ordering::Release);
        IN_WAIT[i].0.store(0, Ordering::Release);
    }
    WAITS.fetch_add(1, Ordering::Relaxed);
    WAIT_NS.fetch_add(ns, Ordering::Relaxed);
    if kind == Kind::Inline {
        INLINE.fetch_add(1, Ordering::Relaxed);
        INLINE_NS.fetch_add(ns, Ordering::Relaxed);
    }
    LONGEST_NS.fetch_max(ns, Ordering::Relaxed);
    if ns >= EVENT_MS * 1_000_000 {
        eprintln!("psowait   t {:.3} s thread {me} {} {:.0} ms {}", start as f64 / 1e9, if kind == Kind::Inline { "inline" } else { "pool" }, ns as f64 / 1e6, what());
    }
    r
}

/// A bind found its pipeline not ready and did not wait (its draws are skipped).
#[inline]
pub fn pending() {
    PENDING.fetch_add(1, Ordering::Relaxed);
}

/// Totals: waits, wait ms, inline waits, inline ms, pending binds, longest wait ms.
pub fn totals() -> (u64, f64, u64, f64, u64, f64) {
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    (r(&WAITS), r(&WAIT_NS) as f64 / 1e6, r(&INLINE), r(&INLINE_NS) as f64 / 1e6, r(&PENDING), r(&LONGEST_NS) as f64 / 1e6)
}

/// The longest wait in progress: (thread id, seconds so far).
fn stuck() -> Option<(u64, f64)> {
    let now = now_ns();
    IN_WAIT
        .iter()
        .filter_map(|(t, s)| {
            let (t, s) = (t.load(Ordering::Acquire), s.load(Ordering::Acquire));
            (t != 0 && s != 0).then(|| (t, now.saturating_sub(s) as f64 / 1e9))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// The process's private commit and the job's limit (process or job), MiB.
fn commit() -> (Option<f64>, Option<f64>) {
    use windows::Win32::System::JobObjects::{JobObjectExtendedLimitInformation, QueryInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_PROCESS_MEMORY};
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
    let mut m = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: the EX struct with its size in cb, passed as the base struct
    let private = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) }.is_ok().then(|| mib(m.PrivateUsage));
    let mut j = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the job of this process (None), an out struct of its size
    let ok = unsafe { QueryInformationJobObject(None, JobObjectExtendedLimitInformation, &mut j as *mut _ as *mut _, std::mem::size_of_val(&j) as u32, None) }.is_ok();
    let flags = j.BasicLimitInformation.LimitFlags;
    let limit = if !ok {
        None
    } else if flags.0 & JOB_OBJECT_LIMIT_PROCESS_MEMORY.0 != 0 {
        Some(mib(j.ProcessMemoryLimit))
    } else if flags.0 & JOB_OBJECT_LIMIT_JOB_MEMORY.0 != 0 {
        Some(mib(j.JobMemoryLimit))
    } else {
        None
    };
    (private, limit)
}

/// Counter values at one second.
#[derive(Clone, Copy, Default)]
pub struct Mark {
    made: u64,
    waits: u64,
    wait_ns: u64,
    inline: u64,
    inline_ns: u64,
    pending: u64,
}

pub fn mark() -> Mark {
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    Mark { made: crate::pipeline::made(), waits: r(&WAITS), wait_ns: r(&WAIT_NS), inline: r(&INLINE), inline_ns: r(&INLINE_NS), pending: r(&PENDING) }
}

/// The second's `psotl` line.
pub fn line(t: u64, fps: f64, a: &Mark, b: &Mark) -> String {
    let made = b.made - a.made;
    let waits = b.waits - a.waits;
    let pending = b.pending - a.pending;
    let stuck = stuck();
    let ms = |ns: u64| ns as f64 / 1e6;
    let mut s = format!(
        "psotl     t {t}s | fps {fps:.1} | made {made} | wait {:.0} ms x{waits} (inline {:.0} ms x{}) | pending {pending} | queue {}",
        ms(b.wait_ns - a.wait_ns),
        ms(b.inline_ns - a.inline_ns),
        b.inline - a.inline,
        crate::pipeline::queued(),
    );
    if let Some((tid, secs)) = stuck {
        s += &format!(" | waiting {secs:.1} s on thread {tid}");
    }
    let (private, limit) = commit();
    if let Some(p) = private {
        s += &format!(" | commit {p:.0} MiB");
        if let Some(l) = limit {
            s += &format!(" of job limit {l:.0} MiB");
        }
    }
    s
}
