//! Build 5: submission. Queue 0x50 `ExecuteCommandLists(count, *lists)` and 0x118
//! `ExecuteCommandListsX(count, *lists, frame token, flags)` (docs/research/d3d12x-device.md §2.2).
//!
//! Each console list is already translated (crates/xlist, at record time): [`segments`] gives its host lists in
//! order, each with the memory waits that must hold before it runs. Submission walks them and sends runs of
//! segments with no waits in one host `ExecuteCommandLists`. A wait between two segments becomes a **queue-side
//! wait**, never a CPU hold on the title's thread:
//!
//! ```text
//! the wait's address is a fence's value cell    host queue Wait(host fence, value)          GPU orders it
//! (GetGpuAddress), compared >= / > / ==
//! the memory already holds                      nothing (labels only grow)
//! anything else the CPU can read                host queue Wait(the queue's bridge fence, n);  GPU waits; the
//!                                               the waiter thread signals n from the CPU         title does not
//!                                               once the memory holds
//! memory the CPU cannot read                    dropped and counted (`refused`)
//! ```
//!
//! The bridge is what keeps the fork's mistake away (it held whole submissions on the CPU and lost 2.4-2.9 s a
//! frame, research §5.6): the lists are with the host queue at once, so the title may reset them, and every other
//! queue keeps running; only the GPU queue behind the wait stalls, as on the console.
//!
//! **Cost.** No lock and no allocation on the common path: the queue comes from the object, the lists' segments
//! from the list, the batch is a stack array. The bridge's pending list (a lock) is touched only by a wait the GPU
//! cannot see that does not hold yet.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use rebuild_xlist::console::{segments, stats, sync_descriptors, MemoryWait};
use windows::core::Interface;

use crate::queue::Queue;

/// Host lists per `ExecuteCommandLists` call at most (a longer run is sent in parts).
const BATCH: usize = 64;

/// What submission did with memory waits since the start: met at submit, made fence waits, bridged, refused.
pub static WAITS_MET: AtomicU64 = AtomicU64::new(0);
pub static WAITS_FENCE: AtomicU64 = AtomicU64::new(0);
pub static WAITS_BRIDGED: AtomicU64 = AtomicU64::new(0);
pub static WAITS_REFUSED: AtomicU64 = AtomicU64::new(0);
/// TSC ticks in [`sync_descriptors`] on the submitting threads (the frame line's `xsync`).
pub static SYNC_TICKS: AtomicU64 = AtomicU64::new(0);
/// Console lists submitted, and those that were not ours (skipped).
pub static LISTS: AtomicU64 = AtomicU64::new(0);
pub static FOREIGN: AtomicU64 = AtomicU64::new(0);

pub unsafe extern "system" fn ExecuteCommandLists(this: *mut c_void, count: u32, lists: *const *mut c_void) {
    unsafe { execute(this, count, lists) };
}

/// The frame token and flags tie the submission to a frame for the console's pacing; frames are paced at the
/// present (Build 8), so it runs as a plain submission.
pub unsafe extern "system" fn ExecuteCommandListsX(this: *mut c_void, count: u32, lists: *const *mut c_void, _token: u64, _flags: u32) {
    unsafe { execute(this, count, lists) };
}

unsafe fn execute(this: *mut c_void, count: u32, lists: *const *mut c_void) {
    let Some(q) = crate::queue::find(this as usize) else { return };
    if lists.is_null() || count == 0 {
        return;
    }
    // SAFETY: the title passes `count` list pointers.
    let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
    // what the CPU changed in the title's watched memory goes up first: the queue waits for it on the GPU
    // (crate::memory; a channel send here, no copy)
    crate::memory::flush_wait(&q.host);
    let mut batch = Batch { q, n: 0, raw: [std::ptr::null_mut(); BATCH] };
    // this submission's progress value: every host list in it is marked with it, the queue signals it at the end
    let me = q as *const Queue as usize;
    let mut value = q.progress_next.fetch_add(1, Ordering::AcqRel) + 1;
    if let Some(begin) = crate::present::frame_begin(q) {
        batch.push(begin);
    }
    // textures the CPU writes (Bink's video frames): their new bytes, copied from the card's copy of their memory
    // ahead of the lists that sample them (crate::cputex; nothing to do is one atomic load)
    if let Some(copies) = crate::cputex::refresh(q, value) {
        batch.push(copies);
    }
    // the GPU time: an earlier frame's timing pairs resolved ahead of this submission's lists ([`crate::gputime`])
    let frame = crate::present::PRESENT_CALLS.load(Ordering::Relaxed);
    if let Some(r) = q.timer.begin(q, frame, value) {
        batch.push(r);
    }
    let mut pairs = Pairs { n: 0, at: [0; PAIRS] };
    let (mut draws, mut dispatches, mut skipped) = (0u64, 0u64, 0u64);
    for &list in lists {
        let Some(segs) = segments(list) else {
            FOREIGN.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // the descriptor tables its translated programs read, mirrored from the title's bytes (the root seam)
        let t0 = rebuild_xlist::console::cost::now();
        sync_descriptors(list);
        SYNC_TICKS.fetch_add(rebuild_xlist::console::cost::now().wrapping_sub(t0), Ordering::Relaxed);
        if let Some(s) = stats(list) {
            draws += u64::from(s.draws);
            dispatches += u64::from(s.dispatches);
            skipped += u64::from(s.skipped);
            totals(&s);
            if trace_frame() {
                eprintln!(
                    "tracelist frame {} queue {:p} list {list:p}: calls {} draws {} dispatches {} skipped {} (pipeline {}, topology {}) refused {} clears {} copies {} rt sets {} (unmapped {}) segments {}",
                    crate::present::PRESENT_CALLS.load(Ordering::Relaxed),
                    this,
                    s.calls,
                    s.draws,
                    s.dispatches,
                    s.skipped,
                    s.skipped_pipeline,
                    s.skipped_topology,
                    s.refused,
                    s.clears,
                    s.copies,
                    s.rt_sets,
                    s.rt_unmapped,
                    segs.len()
                );
            }
        }
        for seg in segs {
            for w in &seg.waits {
                wait(&mut batch, w);
            }
            // a list still running from an earlier submission, on this queue or another: wait for it (the console
            // allows the re-execution, a host queue refuses it, and a card can hang on it). This also keeps the
            // mark below covering the earlier run: the list pool resets its allocator after the last mark only.
            let last = seg.list.executed_at.load(Ordering::Acquire);
            let on = seg.list.executed_on.load(Ordering::Relaxed);
            if last != 0 && !executed(on, last) {
                batch.flush();
                if on == me && last == value {
                    // the same list twice in this submission: close this value now, take a fresh one for the rest
                    unsafe { let _ = q.host.Signal(&q.progress, value); };
                    value = q.progress_next.fetch_add(1, Ordering::AcqRel) + 1;
                }
                // SAFETY: as in `executed`
                wait_done(unsafe { &*(on as *const Queue) }, last);
                REEXECUTED.fetch_add(1, Ordering::Relaxed);
            }
            // every list given to a submission is marked, executed or skipped: the list pool never resets a list
            // the title closed without a mark (it may be on the GPU by a path that does not mark)
            seg.list.executed_on.store(me, Ordering::Relaxed);
            seg.list.executed_at.store(value, Ordering::Release);
            // a list the host refused at Close: executing it would remove the device
            if seg.list.bad.load(Ordering::Relaxed) {
                BAD.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            batch.push(seg.list.list.as_raw());
            let stamp = seg.list.stamp.load(Ordering::Relaxed);
            if stamp != rebuild_xlist::console::stamps::NONE {
                if pairs.n == PAIRS {
                    if let Some(r) = q.timer.add(q, frame, &pairs.at, value) {
                        batch.push(r);
                    }
                    pairs.n = 0;
                }
                pairs.at[pairs.n] = stamp;
                pairs.n += 1;
            }
        }
    }
    if let Some(r) = q.timer.add(q, frame, &pairs.at[..pairs.n], value) {
        batch.push(r);
    }
    batch.flush();
    unsafe {
        let _ = q.host.Signal(&q.progress, value);
    }
    LISTS.fetch_add(lists.len() as u64, Ordering::Relaxed);
    crate::framelog::submitted(draws, dispatches, skipped);
}

/// Over the run, the work in submitted lists: draws (indirect included), dispatches, skipped for want of a host
/// pipeline, skipped for a topology outside the pipeline's type, other skips, clears, copies.
pub static RUN_DRAWS: AtomicU64 = AtomicU64::new(0);
pub static RUN_DISPATCHES: AtomicU64 = AtomicU64::new(0);
pub static RUN_SKIPPED_PIPELINE: AtomicU64 = AtomicU64::new(0);
pub static RUN_SKIPPED_TOPOLOGY: AtomicU64 = AtomicU64::new(0);
pub static RUN_SKIPPED_OTHER: AtomicU64 = AtomicU64::new(0);
pub static RUN_CLEARS: AtomicU64 = AtomicU64::new(0);
pub static RUN_COPIES: AtomicU64 = AtomicU64::new(0);
pub static RUN_RT_UNMAPPED: AtomicU64 = AtomicU64::new(0);

fn totals(s: &rebuild_xlist::console::ListStats) {
    RUN_DRAWS.fetch_add(u64::from(s.draws), Ordering::Relaxed);
    RUN_DISPATCHES.fetch_add(u64::from(s.dispatches), Ordering::Relaxed);
    RUN_SKIPPED_PIPELINE.fetch_add(u64::from(s.skipped_pipeline), Ordering::Relaxed);
    RUN_SKIPPED_TOPOLOGY.fetch_add(u64::from(s.skipped_topology), Ordering::Relaxed);
    RUN_SKIPPED_OTHER.fetch_add(u64::from(s.skipped.saturating_sub(s.skipped_pipeline + s.skipped_topology)), Ordering::Relaxed);
    RUN_CLEARS.fetch_add(u64::from(s.clears), Ordering::Relaxed);
    RUN_COPIES.fetch_add(u64::from(s.copies), Ordering::Relaxed);
    RUN_RT_UNMAPPED.fetch_add(u64::from(s.rt_unmapped), Ordering::Relaxed);
}

/// `REBUILD_TRACE_LISTS=a-b` (or `a`): one line per submitted list while the present count is in that range.
fn trace_frame() -> bool {
    static R: std::sync::OnceLock<Option<(u64, u64)>> = std::sync::OnceLock::new();
    let r = R.get_or_init(|| {
        let v = std::env::var("REBUILD_TRACE_LISTS").ok()?;
        let mut it = v.split('-').map(|x| x.trim().parse::<u64>().ok());
        let a = it.next()??;
        let b = it.next().flatten().unwrap_or(a);
        Some((a, b))
    });
    r.is_some_and(|(a, b)| (a..=b).contains(&crate::present::PRESENT_CALLS.load(Ordering::Relaxed)))
}

/// Host lists reset ahead of time and recycled after their last execution (`rebuild_xlist::console::pool`): this
/// submission is what marks each host list's queue and progress value, so the title's run turns it on (tests that
/// execute host lists themselves leave it off), with `REBUILD_LIST_POOL=1`.
pub fn enable_list_pool() -> bool {
    rebuild_xlist::console::pool::enable(executed)
}

/// The queue at `queue` (a [`Queue`], leaked: they live for the run) has finished its submission `value`.
fn executed(queue: usize, value: u64) -> bool {
    if queue == 0 {
        return true;
    }
    // SAFETY: `executed_on` holds only addresses of queues from crate::queue, which are never freed
    let q = unsafe { &*(queue as *const Queue) };
    unsafe { q.progress.GetCompletedValue() >= value }
}

/// Re-executions of a host list that had to wait for its earlier run.
pub static REEXECUTED: AtomicU64 = AtomicU64::new(0);
/// Host lists not executed because their Close failed.
pub static BAD: AtomicU64 = AtomicU64::new(0);

/// Wait (at most 2 s) until the queue's progress fence reaches `value`.
fn wait_done(q: &Queue, value: u64) {
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else { return };
    unsafe {
        if q.progress.SetEventOnCompletion(value, event).is_ok() {
            let _ = WaitForSingleObject(event, 2000);
        }
        let _ = windows::Win32::Foundation::CloseHandle(event);
    }
}

/// Timing pairs (the first query of each) of the host lists a submission executed, noted on the queue in groups.
const PAIRS: usize = 128;
struct Pairs {
    n: usize,
    at: [u32; PAIRS],
}

/// Host lists gathered for one `ExecuteCommandLists`.
struct Batch {
    q: &'static Queue,
    n: usize,
    raw: [*mut c_void; BATCH],
}

impl Batch {
    #[inline]
    fn push(&mut self, list: *mut c_void) {
        if self.n == BATCH {
            self.flush();
        }
        self.raw[self.n] = list;
        self.n += 1;
    }

    #[inline]
    fn flush(&mut self) {
        if self.n > 0 && rebuild_xlist::console::skip::on(rebuild_xlist::console::skip::SUBMIT) {
            self.n = 0; // REBUILD_GPU_SKIP=submit: recorded, never executed
        }
        if self.n > 0 {
            // SAFETY: closed host graphics lists (an ID3D12GraphicsCommandList is an ID3D12CommandList).
            rebuild_xlist::console::SUBMITTING.fetch_add(1, Ordering::Relaxed);
            unsafe { (Interface::vtable(&self.q.host).ExecuteCommandLists)(Interface::as_raw(&self.q.host), self.n as u32, self.raw.as_ptr() as *const _) };
            rebuild_xlist::console::SUBMITTING.fetch_sub(1, Ordering::Relaxed);
            self.n = 0;
        }
    }
}

/// How a memory wait is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// It holds already.
    Met,
    /// A host queue wait on this fence for this value.
    Fence(usize, u64),
    /// The bridge: the GPU waits, the waiter thread releases it.
    Bridge,
    /// Memory the CPU cannot read: the wait is dropped.
    Refused,
}

/// The fence value a monotonic comparison against a fence's cell needs, if the comparison is one a fence wait
/// can keep (>=, >, ==; a fence only grows, so == is taken as >=).
pub fn fence_value(w: &MemoryWait) -> Option<u64> {
    let r = if w.wide { w.reference } else { w.reference & 0xFFFF_FFFF };
    match w.comparison {
        7 | 3 => Some(r),
        5 => r.checked_add(1),
        _ => None,
    }
}

/// Where a wait goes (read on the submitting thread; no lock).
pub fn place(w: &MemoryWait) -> Place {
    if let Some(f) = crate::fence::by_cell(w.address) {
        if w.holds_for(f.value()) {
            return Place::Met;
        }
        if let Some(v) = fence_value(w) {
            return Place::Fence(f as *const crate::fence::Fence as usize, v);
        }
        return Place::Bridge;
    }
    if !readable(w.address, if w.wide { 8 } else { 4 }) {
        return Place::Refused;
    }
    if w.holds_for(read(w)) {
        Place::Met
    } else {
        Place::Bridge
    }
}

fn wait(batch: &mut Batch, w: &MemoryWait) {
    match place(w) {
        Place::Met => {
            WAITS_MET.fetch_add(1, Ordering::Relaxed);
        }
        Place::Refused => {
            if WAITS_REFUSED.fetch_add(1, Ordering::Relaxed) == 0 {
                eprintln!("submit    a memory wait on {:#x} the CPU cannot read: dropped (counted from now)", w.address);
            }
        }
        Place::Fence(f, v) => {
            batch.flush();
            // SAFETY: fences live for the run.
            let f = unsafe { &*(f as *const crate::fence::Fence) };
            let _ = unsafe { batch.q.host.Wait(&f.host, v) };
            WAITS_FENCE.fetch_add(1, Ordering::Relaxed);
        }
        Place::Bridge => {
            batch.flush();
            bridge(batch.q, w);
            WAITS_BRIDGED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The memory at `w.address`, as wide as the wait reads it.
fn read(w: &MemoryWait) -> u64 {
    // SAFETY: `readable` said so; the GPU or another thread may write it, hence volatile.
    unsafe {
        if w.wide {
            std::ptr::read_volatile(w.address as *const u64)
        } else {
            u64::from(std::ptr::read_volatile(w.address as *const u32))
        }
    }
}

/// The CPU can read `bytes` at `address`: console memory the device knows (CPU address = console address), or
/// committed readable memory of the process.
fn readable(address: u64, bytes: u64) -> bool {
    if address == 0 || address % bytes != 0 {
        return false;
    }
    if crate::device::list_host().and_then(|h| h.memory(address)).is_some() {
        return true;
    }
    // the allocation's descriptor (µs), not VirtualQuery (ms on the title's large regions)
    rebuild_d3d12x::vmem::committed_protect(address, bytes).is_some_and(|p| p & rebuild_d3d12x::vmem::READ != 0 && p & 0x100 == 0)
}

// ------------------------------------------------------------------------------------- the bridge

struct Pending {
    queue: &'static Queue,
    value: u64,
    wait: MemoryWait,
}

struct Waiter {
    pending: Mutex<Vec<Pending>>,
    wake: Condvar,
}

static WAITER: OnceLock<&'static Waiter> = OnceLock::new();

fn waiter() -> &'static Waiter {
    WAITER.get_or_init(|| {
        let w: &'static Waiter = Box::leak(Box::new(Waiter { pending: Mutex::new(Vec::with_capacity(64)), wake: Condvar::new() }));
        let _ = std::thread::Builder::new().name("memory-wait".into()).stack_size(64 << 10).spawn(move || run_waiter(w));
        w
    })
}

/// The host queue waits on its bridge fence for the next value; the waiter signals it once `w` holds.
fn bridge(q: &'static Queue, w: &MemoryWait) {
    let value = q.bridge_next.fetch_add(1, Ordering::AcqRel) + 1;
    let wt = waiter();
    // the entry first, then the queue's wait: the waiter may signal before the GPU gets there, which is fine
    wt.pending.lock().unwrap_or_else(|e| e.into_inner()).push(Pending { queue: q, value, wait: *w });
    wt.wake.notify_one();
    let _ = unsafe { q.host.Wait(&q.bridge, value) };
}

/// Release what holds, in each queue's order (a fence only grows, so a later wait waits for every earlier one).
fn run_waiter(w: &'static Waiter) {
    let mut blocked: Vec<*const Queue> = Vec::new();
    loop {
        {
            let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
            while p.is_empty() {
                p = w.wake.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            blocked.clear();
            p.retain(|e| {
                let qp = e.queue as *const Queue;
                if blocked.contains(&qp) {
                    return true;
                }
                if e.wait.holds_for(read(&e.wait)) {
                    let _ = unsafe { e.queue.bridge.Signal(e.value) };
                    false
                } else {
                    blocked.push(qp);
                    true
                }
            });
            if p.is_empty() {
                continue;
            }
        }
        // labels change as the GPU finishes work (or the title writes them): look again shortly
        std::thread::sleep(Duration::from_micros(250));
    }
}

/// Release every wait the bridge holds (their queues run on whatever the memory says): for shutdown, so no host
/// queue is left waiting on a fence nothing will signal.
pub fn release_all() {
    let Some(w) = WAITER.get() else { return };
    let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
    for e in p.drain(..) {
        let _ = unsafe { e.queue.bridge.Signal(e.queue.bridge_next.load(Ordering::Acquire)) };
    }
}

/// Waits the bridge holds right now.
pub fn bridged_pending() -> usize {
    WAITER.get().map_or(0, |w| w.pending.lock().map(|p| p.len()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_waits_take_the_value_a_monotonic_comparison_needs() {
        let w = |comparison, reference, wide| MemoryWait { address: 0x1000, comparison, reference, wide };
        assert_eq!(fence_value(&w(7, 5, true)), Some(5));
        assert_eq!(fence_value(&w(5, 5, true)), Some(6));
        assert_eq!(fence_value(&w(3, 0x1_0000_0005, false)), Some(5), "32-bit: the low word");
        assert_eq!(fence_value(&w(2, 5, true)), None, "< cannot be a fence wait");
        assert_eq!(fence_value(&w(6, 5, true)), None);
    }

    #[test]
    fn memory_the_cpu_cannot_read_is_refused_and_memory_that_holds_is_met() {
        assert_eq!(place(&MemoryWait { address: 0x10, comparison: 7, reference: 1, wide: true }), Place::Refused);
        let cell = Box::new(7u64);
        let a = &*cell as *const u64 as u64;
        assert_eq!(place(&MemoryWait { address: a, comparison: 7, reference: 7, wide: true }), Place::Met);
        assert_eq!(place(&MemoryWait { address: a, comparison: 7, reference: 8, wide: true }), Place::Bridge);
    }
}
