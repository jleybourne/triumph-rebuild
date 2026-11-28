//! Build 0 (c): the title's real GPU time, from a timestamp pair around every host list it executes.
//!
//! The lists record the pair themselves (`rebuild_xlist::console::stamps`): a query before a host list's first
//! command and one after its last, in the device's timestamp ring. What is left is here, on the queue's side:
//!
//! ```text
//! submission (title's thread)   the pairs of the host lists it executed are noted on the queue (a lock, no
//!                               allocation); at the first submission of a later frame, one small list resolves
//!                               the frame's pairs into the queue's readback ring, ahead of the new lists (so
//!                               after the old ones, in queue order), in runs of consecutive pairs
//! present (title's thread)      [`collect`]: the entries whose submission the queue's progress fence has passed
//!                               are read (no wait: a few frames late), turned into ns, kept by frame; a frame
//!                               no queue still holds is finished: its busy time goes to `framelog::gpu`
//! ```
//!
//! **What is measured.** A frame's `gpu` is the union of its lists' intervals over every direct and compute queue:
//! the time some queue ran the title's work, overlap counted once, gaps (CPU waits, memory waits) not at all. The
//! plain sum of the lists' times (overlapping async compute counted twice) is kept beside it for the summary.
//! Copy lists are not timed (their counter is another clock on some hosts).
//!
//! **Cost.** Per host list two `EndQuery` (recorded by the list). Per queue and frame one resolve list (an
//! allocator reset, a list reset, one `ResolveQueryData` a run of consecutive pairs, a close), taken from a pool
//! the fence frees; nothing waits on the CPU, ever: with no free list or no room in the ring the pairs are dropped
//! and counted.

#![cfg(windows)]

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rebuild_xlist::console::stamps::{self, Stamps};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// Resolved pairs a queue's readback ring holds (16 bytes each).
pub const ENTRIES: u64 = 1 << 14;
/// Pairs noted before a resolve is forced within the frame.
const PENDING_MAX: usize = 1024;
/// Resolve lists a queue keeps at most (more in flight: the pairs are dropped).
const POOL_MAX: usize = 64;
/// A queue's unresolved pairs older than this many frames no longer hold back the frames after them.
const STALE_FRAMES: u64 = 4;
/// A list interval longer than this is taken as garbage (a slot reused while still in flight) and dropped.
const MAX_LIST_NS: u64 = 1_000_000_000;

/// Pairs dropped (no resolve list free, no room in the ring, garbage values, or read after their frame finished).
pub static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Pairs resolved and read.
pub static READ: AtomicU64 = AtomicU64::new(0);

/// A resolve recorded: `n` pairs of `frame` at ring entries `first..first + n`, done when the queue's progress fence
/// reaches `value`.
struct Entry {
    frame: u64,
    value: u64,
    first: u64,
    n: u32,
}

/// One queue's side: its pairs, resolve lists and readback ring.
pub struct Timer {
    stamps: Arc<Stamps>,
    device: ID3D12Device,
    kind: D3D12_COMMAND_LIST_TYPE,
    /// the queue's timestamp frequency
    pub hz: u64,
    readback: ID3D12Resource,
    cells: *const u64,
    pending: Vec<u32>,
    pending_frame: u64,
    /// ring entries used so far (the next is `next % ENTRIES`)
    next: u64,
    entries: VecDeque<Entry>,
    pool: VecDeque<(ID3D12CommandAllocator, ID3D12GraphicsCommandList, u64)>,
}

// SAFETY: D3D12 objects are free-threaded; the mapped cells are only read under the queue timer's lock.
unsafe impl Send for Timer {}

impl Timer {
    /// A timer for a queue of `kind` whose timestamps tick at `hz`.
    pub fn new(device: &ID3D12Device, stamps: Arc<Stamps>, kind: D3D12_COMMAND_LIST_TYPE, hz: u64) -> Option<Timer> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: ENTRIES * 16,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut r) }.ok()?;
        let readback = r?;
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe { readback.Map(0, None, Some(&mut p)) }.ok()?;
        Some(Timer {
            stamps,
            device: device.clone(),
            kind,
            hz,
            readback,
            cells: p as *const u64,
            pending: Vec::with_capacity(PENDING_MAX + 256),
            pending_frame: 0,
            next: 0,
            entries: VecDeque::with_capacity(256),
            pool: VecDeque::with_capacity(POOL_MAX),
        })
    }

    /// Before a submission of `frame` that the queue signals with `value`: the resolve list for an earlier frame's
    /// pairs, to execute first.
    pub fn begin(&mut self, frame: u64, value: u64, completed: u64) -> Option<*mut c_void> {
        (!self.pending.is_empty() && self.pending_frame != frame).then(|| self.resolve(value, completed)).flatten()
    }

    /// After a submission of `frame` (signalled with `value`) executed host lists with timing `pairs`: noted; the
    /// resolve list to execute after them if the pairs had to be resolved now.
    pub fn add(&mut self, frame: u64, pairs: &[u32], value: u64, completed: u64) -> Option<*mut c_void> {
        if pairs.is_empty() {
            return None;
        }
        let out = if !self.pending.is_empty() && (self.pending_frame != frame || self.pending.len() + pairs.len() > PENDING_MAX) { self.resolve(value, completed) } else { None };
        self.pending_frame = frame;
        self.pending.extend_from_slice(pairs);
        out
    }

    /// A list resolving every noted pair into the ring, or `None` (the pairs dropped) when no list is free or the ring
    /// is full of entries not read yet.
    fn resolve(&mut self, value: u64, completed: u64) -> Option<*mut c_void> {
        let n = self.pending.len() as u64;
        let unread = self.entries.front().map_or(0, |e| self.next - e.first);
        let list = if unread + n > ENTRIES { None } else { self.resolver(completed) };
        let Some((a, l)) = list else {
            DROPPED.fetch_add(n, Ordering::Relaxed);
            self.pending.clear();
            return None;
        };
        let heap = &self.stamps.heap;
        let (first, mut k) = (self.next, 0usize);
        while k < self.pending.len() {
            // a run of consecutive pairs into consecutive entries, not across the ring's end
            let at = (first + k as u64) % ENTRIES;
            let mut run = 1usize;
            while k + run < self.pending.len() && self.pending[k + run] == self.pending[k] + 2 * run as u32 && at + (run as u64) < ENTRIES {
                run += 1;
            }
            unsafe { l.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, self.pending[k], 2 * run as u32, &self.readback, at * 16) };
            k += run;
        }
        let raw = unsafe { l.Close() }.is_ok().then(|| l.as_raw());
        if raw.is_some() {
            self.entries.push_back(Entry { frame: self.pending_frame, value, first, n: n as u32 });
            self.next += n;
        } else {
            DROPPED.fetch_add(n, Ordering::Relaxed);
        }
        self.pending.clear();
        // the pool keeps the list alive until the queue has run it
        self.pool.push_back((a, l, value));
        raw
    }

    /// A resolve list, open: the oldest of the pool if the GPU is done with it, else a new one (at most
    /// [`POOL_MAX`]).
    fn resolver(&mut self, completed: u64) -> Option<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
        if self.pool.front().is_some_and(|p| p.2 <= completed) {
            let (a, l, _) = self.pool.pop_front()?;
            unsafe {
                a.Reset().ok()?;
                l.Reset(&a, None).ok()?;
            }
            return Some((a, l));
        }
        if self.pool.len() >= POOL_MAX {
            return None;
        }
        unsafe {
            let a: ID3D12CommandAllocator = self.device.CreateCommandAllocator(self.kind).ok()?;
            let l: ID3D12GraphicsCommandList = self.device.CreateCommandList(0, self.kind, &a, None).ok()?;
            Some((a, l))
        }
    }

    /// Read every entry the queue finished (`completed`: its progress fence): `out(frame, begin ns, end ns)` per pair.
    pub fn read(&mut self, completed: u64, mut out: impl FnMut(u64, u64, u64)) {
        while self.entries.front().is_some_and(|e| e.value <= completed) {
            let e = self.entries.pop_front().expect("front");
            for i in 0..u64::from(e.n) {
                let at = ((e.first + i) % ENTRIES) as usize;
                // SAFETY: the mapped ring; the GPU wrote these cells before the fence passed
                let (b, t) = unsafe { (self.cells.add(2 * at).read_volatile(), self.cells.add(2 * at + 1).read_volatile()) };
                if t < b || stamps::ticks_ns(t - b, self.hz) > MAX_LIST_NS {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                READ.fetch_add(1, Ordering::Relaxed);
                out(e.frame, stamps::ticks_ns(b, self.hz), stamps::ticks_ns(t, self.hz));
            }
        }
    }

    /// The oldest frame this queue still holds pairs of: entries not read, or noted pairs not resolved (those only
    /// while they are recent: a queue that stopped submitting must not hold every later frame).
    pub fn holds(&self, current: u64) -> Option<u64> {
        let resolved = self.entries.front().map(|e| e.frame);
        let noted = (!self.pending.is_empty() && self.pending_frame + STALE_FRAMES >= current).then_some(self.pending_frame);
        match (resolved, noted) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// The timer a queue carries: made at its first timed submission.
pub struct QueueTimer {
    inner: Mutex<Option<Timer>>,
    /// making it failed (or timing is off): never tried again
    off: std::sync::atomic::AtomicBool,
}

impl QueueTimer {
    pub const fn new() -> QueueTimer {
        QueueTimer { inner: Mutex::new(None), off: std::sync::atomic::AtomicBool::new(false) }
    }

    fn with<R>(&self, q: &crate::queue::Queue, f: impl FnOnce(&mut Timer) -> R) -> Option<R> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let made = crate::device::get().and_then(|d| {
                let st = stamps::for_device(&d.host)?;
                let hz = unsafe { q.host.GetTimestampFrequency() }.ok()?;
                Timer::new(&d.host, st, q.desc.Type, hz)
            });
            if made.is_none() || !matches!(q.desc.Type, D3D12_COMMAND_LIST_TYPE_DIRECT | D3D12_COMMAND_LIST_TYPE_COMPUTE) {
                self.off.store(true, Ordering::Relaxed);
                return None;
            }
            *g = made;
            TIMERS.lock().unwrap_or_else(|e| e.into_inner()).push(q as *const crate::queue::Queue as usize);
        }
        g.as_mut().map(f)
    }

    /// See [`Timer::begin`]. Only once the queue has a timer (its first [`add`](Self::add) makes it).
    pub fn begin(&self, q: &crate::queue::Queue, frame: u64, value: u64) -> Option<*mut c_void> {
        if self.off.load(Ordering::Relaxed) {
            return None;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let t = g.as_mut()?;
        let completed = unsafe { q.progress.GetCompletedValue() };
        t.begin(frame, value, completed)
    }

    /// See [`Timer::add`].
    pub fn add(&self, q: &crate::queue::Queue, frame: u64, pairs: &[u32], value: u64) -> Option<*mut c_void> {
        if pairs.is_empty() {
            return None;
        }
        let completed = unsafe { q.progress.GetCompletedValue() };
        self.with(q, |t| t.add(frame, pairs, value, completed)).flatten()
    }
}

impl Default for QueueTimer {
    fn default() -> Self {
        QueueTimer::new()
    }
}

/// Queues with a timer (addresses of `'static` queues).
static TIMERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Frames read but not finished: their intervals (ns); frames below `done` are finished.
struct Frames {
    open: BTreeMap<u64, Vec<(u64, u64)>>,
    done: u64,
}
static FRAMES: Mutex<Frames> = Mutex::new(Frames { open: BTreeMap::new(), done: 0 });

/// The busy time of `intervals` (their union) and their plain sum, in ns. Sorts them.
pub fn busy(intervals: &mut [(u64, u64)]) -> (u64, u64) {
    intervals.sort_unstable();
    let (mut union, mut sum) = (0u64, 0u64);
    let mut cur: Option<(u64, u64)> = None;
    for &(b, e) in intervals.iter() {
        sum += e - b;
        cur = match cur {
            Some((cb, ce)) if b <= ce => Some((cb, ce.max(e))),
            Some((cb, ce)) => {
                union += ce - cb;
                Some((b, e))
            }
            None => Some((b, e)),
        };
    }
    if let Some((cb, ce)) = cur {
        union += ce - cb;
    }
    (union, sum)
}

/// Read what the queues finished and hand every frame no queue still holds to the frame line (at each present; no
/// wait).
pub fn collect() {
    let queues: Vec<usize> = TIMERS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if queues.is_empty() {
        return;
    }
    let current = crate::present::PRESENT_CALLS.load(Ordering::Relaxed);
    let mut frames = FRAMES.lock().unwrap_or_else(|e| e.into_inner());
    let mut limit = current;
    for q in queues {
        // SAFETY: queues are leaked for the run
        let q = unsafe { &*(q as *const crate::queue::Queue) };
        let mut g = q.timer.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else { continue };
        let completed = unsafe { q.progress.GetCompletedValue() };
        let done = frames.done;
        t.read(completed, |f, b, e| {
            if f < done {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            } else {
                frames.open.entry(f).or_default().push((b, e));
            }
        });
        if let Some(h) = t.holds(current) {
            limit = limit.min(h);
        }
    }
    while let Some(entry) = frames.open.first_entry() {
        if *entry.key() >= limit {
            break;
        }
        let mut v = entry.remove();
        let (union, sum) = busy(&mut v);
        crate::framelog::gpu(union / 1000, sum / 1000);
    }
    frames.done = frames.done.max(limit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_time_counts_overlap_once_and_gaps_not_at_all() {
        // two queues: graphics 0-10 and 12-20, async compute 5-15 over both; a gap 20-30; then 30-31
        let mut v = vec![(12, 20), (0, 10), (30, 31), (5, 15)];
        assert_eq!(busy(&mut v), (21, 29));
        assert_eq!(busy(&mut []), (0, 0));
        let mut touching = vec![(0, 5), (5, 9)];
        assert_eq!(busy(&mut touching), (9, 9));
        let mut inside = vec![(0, 100), (10, 20), (30, 40)];
        assert_eq!(busy(&mut inside), (100, 120));
    }

    struct NoHost;
    impl rebuild_xlist::console::PipelineSource for NoHost {
        fn pipeline(&self, _: rebuild_xlist::console::Raw) -> Option<rebuild_xlist::console::HostPipeline> {
            None
        }
    }
    impl rebuild_xlist::console::ListHost for NoHost {
        fn resource(&self, _: rebuild_xlist::console::Raw) -> Option<rebuild_xlist::console::Raw> {
            None
        }
        fn descriptor_heap(&self, _: rebuild_xlist::console::Raw) -> Option<rebuild_xlist::console::HeapBinding> {
            None
        }
        fn cpu_descriptor(&self, _: usize) -> Option<usize> {
            None
        }
        fn memory(&self, _: u64) -> Option<rebuild_xlist::console::MemoryRange> {
            None
        }
    }

    /// Three console lists of frame 1, noted after their submission and resolved in one run by the next frame's
    /// first submission: each is read once its fence value passes, under frame 1, in order, end after begin.
    #[test]
    fn a_frames_list_pairs_resolve_in_one_run_and_read_back_on_warp() {
        use rebuild_xlist::console::{create_allocator, create_list, segments};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory4};
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory4>() }) else { return };
        let Ok(adapter) = (unsafe { factory.EnumWarpAdapter::<windows::core::IUnknown>() }) else { return };
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
            return;
        }
        let device = device.unwrap();
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let host: Arc<dyn rebuild_xlist::console::ListHost> = Arc::new(NoHost);
            let a = create_allocator(0, &device, host).unwrap();
            let mut lists = Vec::new();
            let mut pairs = Vec::new();
            for _ in 0..3 {
                let l = create_list(0, a, std::ptr::null_mut()).unwrap();
                // Close (slot 0x48)
                let close: unsafe extern "system" fn(*mut c_void) -> windows::core::HRESULT = std::mem::transmute(*(*(l as *const *const usize)).add(0x48 / 8));
                assert!(close(l).is_ok());
                let seg = &segments(l).unwrap()[0];
                pairs.push(seg.list.stamp.load(Ordering::Relaxed));
                lists.push(Some(seg.list.list.cast::<ID3D12CommandList>().unwrap()));
            }
            assert!(pairs.windows(2).all(|w| w[1] == w[0] + 2), "lists recorded in turn take consecutive pairs: {pairs:?}");
            let ring = stamps::for_device(&device).unwrap();
            let hz = queue.GetTimestampFrequency().unwrap();
            let mut t = Timer::new(&device, ring, D3D12_COMMAND_LIST_TYPE_DIRECT, hz).expect("timer");
            // frame 1's submission, signalled with 1
            queue.ExecuteCommandLists(&lists);
            assert!(t.add(1, &pairs, 1, 0).is_none(), "noted, not resolved yet");
            queue.Signal(&fence, 1).unwrap();
            assert!(t.begin(1, 2, 0).is_none(), "the same frame resolves nothing");
            assert_eq!(t.holds(1), Some(1));
            // frame 2's first submission: frame 1's pairs resolved ahead of it
            let r = t.begin(2, 2, 0).expect("a resolve list");
            let raw = r;
            let resolve = ID3D12CommandList::from_raw_borrowed(&raw).unwrap().clone();
            queue.ExecuteCommandLists(&[Some(resolve)]);
            queue.Signal(&fence, 2).unwrap();
            let mut got = Vec::new();
            t.read(1, |f, b, e| got.push((f, b, e)));
            assert!(got.is_empty(), "the resolve's value (2) has not been reached at 1");
            fence.SetEventOnCompletion(2, windows::Win32::Foundation::HANDLE::default()).unwrap();
            t.read(fence.GetCompletedValue(), |f, b, e| got.push((f, b, e)));
            assert_eq!(got.len(), 3, "{got:?}");
            assert!(got.iter().all(|&(f, b, e)| f == 1 && b > 0 && e >= b), "{got:?}");
            assert!(got.windows(2).all(|w| w[1].1 >= w[0].2), "in queue order: {got:?}");
            assert_eq!(t.holds(3), None, "nothing held once read");
            // the pool gives the resolve list back once the queue is done with it
            assert!(t.add(3, &[pairs[0]], 3, 2).is_none());
            assert!(t.begin(4, 4, 2).is_some(), "the resolve list is reused");
            assert_eq!(t.pool.len(), 1);
            device.GetDeviceRemovedReason().expect("device alive");
        }
    }
}
