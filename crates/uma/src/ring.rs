//! Build 2: the title's CPU-written memory reaches the tiles on an async ring.
//!
//! ```text
//! submit thread   flush() -> fence value V, at once     never watches, diffs or copies
//!                 title queue.Wait(fence, V)            the GPU waits, not the CPU
//! coordinator     watch (GetWriteWatch + reset) -> diff on the pool -> gather into a staging slot
//!                 -> one scatter dispatch per window on the async compute queue -> Signal(V)
//! pool            persistent workers; each diffs a contiguous share of the written pages
//! slots           a ring of staging + index buffers, reused when the fence passes their last use
//! ```
//!
//! Chosen by Build 0 (a): hash diff (12.5 % of the region, faster than a shadow), compute scatter
//! (one CopyBufferRegion per run collapses past ~100k runs), workers (34 ms serial at worst).
//!
//! In the title run (`docs/design/memory-run.md`) regions come and go as the title reserves and frees graphics
//! memory ([`Ring::add`], [`Ring::remove`]; a pass holds the region list read-locked, so nothing is freed under
//! it), windows are 512 MiB sub-windows made on demand ([`Ring::dynamic`]), flushes that arrive while a pass runs
//! are served by the next pass (one Signal of the highest value), and card pages reused for new memory are zeroed
//! on the ring's queue ahead of the uploads ([`Ring::zero`]).
//!
//! Visibility is the console's: the GPU sees CPU bytes as of the moment the coordinator read them,
//! which may include writes the title made after calling flush. A chunk caught half-written is sent
//! again next flush, because write-watch flags its page again; nothing is lost.
//!
//! A chunk's hash counts as sent only once the scatter that carries it is on the ring's queue, ahead of the
//! Signal the title's queue waits for, so every use is ordered after the copy. The diff leaves the hashes alone
//! and the commit follows the submit; no CPU wait is added. Write-watch is reset when the pages are read, so a
//! page whose changes were not submitted (more than a slot holds, or a failed flush) is carried to the next
//! flush and diffed again there.

#![cfg(windows)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex, OnceLock, RwLock};
use std::time::Instant;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::*;

use crate::chunks::{self, Run, CHUNK, CHUNKS_PER_PAGE, OS_PAGE};
use crate::WINDOW;

const WRITE_WATCH_FLAG_RESET: u32 = 1;
pub const MEM_WRITE_WATCH: VIRTUAL_ALLOCATION_TYPE = VIRTUAL_ALLOCATION_TYPE(0x0020_0000);

/// Staging bytes per slot, and slots in the ring.
pub const SLOT_BYTES: usize = 96 << 20;
pub const SLOTS: usize = 3;

/// Chunk hashes are kept per block of this many bytes, made the first time a chunk in it is sent: a large
/// reservation that is mostly uncommitted costs nothing.
const BLOCK: usize = 64 << 10;
const BLOCK_CHUNKS: usize = BLOCK / CHUNK;

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

/// CPU memory the title writes and the GPU reads, at a console address inside one window.
pub struct Watched {
    pub base: *mut u8,
    pub len: usize,
    pub console: u64,
}

unsafe impl Send for Watched {}
unsafe impl Sync for Watched {}

impl Watched {
    /// Allocate `len` bytes of write-watched memory for console address `console`.
    pub fn alloc(console: u64, len: usize) -> Result<Watched, String> {
        if console as usize % WINDOW + len > WINDOW || len % OS_PAGE != 0 {
            return Err("a watched region must be whole pages inside one window".into());
        }
        let base = unsafe { VirtualAlloc(None, len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
        if base.is_null() {
            return Err(format!("VirtualAlloc write-watch {} MiB failed", len >> 20));
        }
        Ok(Watched { base: base as *mut u8, len, console })
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.base as *mut _, 0, MEM_RELEASE);
        }
    }
}

/// What one flush did, measured on the coordinator.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlushStats {
    pub value: u64,
    /// flush requests this pass served (requests that arrive while a pass runs are served by the next one)
    pub served: u64,
    pub pages: usize,
    pub runs: usize,
    pub bytes: usize,
    pub slot_wait_ms: f64,
    pub watch_ms: f64,
    pub diff_ms: f64,
    pub gather_ms: f64,
    pub record_ms: f64,
    /// flush() called -> scatter submitted and signalled
    pub latency_ms: f64,
    /// Pages with changes that did not fit in the slot: they go with the next flush.
    pub carried_pages: usize,
    /// 64-byte chunks with no window to land in (dropped; never expected)
    pub homeless: usize,
    /// pages zeroed on the GPU (card pages reused)
    pub zeroed: usize,
    /// regions GetWriteWatch refused this pass (skipped)
    pub unwatchable: usize,
}

/// Over the ring's life: passes, bytes sent, and the coordinator's time (µs).
#[derive(Default)]
pub struct Totals {
    pub passes: AtomicU64,
    pub flushes: AtomicU64,
    pub bytes: AtomicU64,
    pub busy_us: AtomicU64,
    pub latency_us: AtomicU64,
    pub carried: AtomicU64,
    pub homeless: AtomicU64,
    pub failed: AtomicU64,
    /// the pass's phases summed (µs): waiting for a slot, write-watch, diff, gather, record + submit
    pub slot_wait_us: AtomicU64,
    pub watch_us: AtomicU64,
    pub diff_us: AtomicU64,
    pub gather_us: AtomicU64,
    pub record_us: AtomicU64,
    /// waiting for the region lock (a free of watched memory held it)
    pub lock_us: AtomicU64,
}

struct Ptr<T>(*mut T);
impl<T> Clone for Ptr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Ptr<T> {}
unsafe impl<T> Send for Ptr<T> {}
unsafe impl<T> Sync for Ptr<T> {}

/// A dirty page: which region (index in this pass's list), and its byte offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Page {
    region: u32,
    offset: u32,
}

/// One watched region: CPU bytes at `base`, console address `console`, the hash of every chunk as sent.
struct Region {
    base: Ptr<u8>,
    len: usize,
    console: u64,
    sent: Box<[OnceLock<Box<[AtomicU64]>>]>,
    _keep: Option<Arc<Watched>>,
}

impl Region {
    fn new(base: *mut u8, len: usize, console: u64, keep: Option<Arc<Watched>>) -> Region {
        Region { base: Ptr(base), len, console, sent: (0..len.div_ceil(BLOCK)).map(|_| OnceLock::new()).collect(), _keep: keep }
    }

    /// The hashes of the page at `offset` as last sent (all-zero chunks if nothing was sent in its block).
    fn page_hashes(&self, offset: usize) -> &[u64] {
        let b = offset / BLOCK;
        let at = (offset % BLOCK) / CHUNK;
        match self.sent[b].get() {
            // SAFETY: AtomicU64 has u64's layout; hashes are only written in the commit phase, never while a diff reads
            Some(h) => unsafe { std::slice::from_raw_parts((h.as_ptr() as *const u64).add(at), CHUNKS_PER_PAGE) },
            None => &zero_page()[..],
        }
    }

    fn commit(&self, chunk: usize, h: u64) {
        let block = self.sent[chunk / BLOCK_CHUNKS].get_or_init(|| (0..BLOCK_CHUNKS).map(|_| AtomicU64::new(chunks::zero_hash())).collect());
        block[chunk % BLOCK_CHUNKS].store(h, Ordering::Relaxed);
    }
}

fn zero_page() -> &'static [u64; CHUNKS_PER_PAGE] {
    static Z: OnceLock<[u64; CHUNKS_PER_PAGE]> = OnceLock::new();
    Z.get_or_init(|| [chunks::zero_hash(); CHUNKS_PER_PAGE])
}

/// One diff worker's output: its runs in console order and, chunk for chunk in the same order, the hash each
/// chunk will count as sent once submitted (region, chunk index in it, hash).
#[derive(Default)]
struct Work {
    runs: Vec<Run>,
    pending: Vec<(u32, u32, u64)>,
    /// Chunks of `pending` that fit in the slot, after [`fit`].
    kept: usize,
}

/// Keep the runs' first `room` bytes (in order; a run may be cut at a chunk), take what is kept from `room`,
/// and return how many chunks stay.
fn fit(runs: &mut Vec<Run>, room: &mut usize) -> usize {
    let mut chunks = 0;
    let mut keep = runs.len();
    for (i, run) in runs.iter_mut().enumerate() {
        if *room == 0 {
            keep = i;
            break;
        }
        let len = run.len.min(*room);
        run.len = len;
        *room -= len;
        chunks += len / CHUNK;
    }
    runs.truncate(keep);
    chunks
}

/// Cut runs where they cross a multiple of `span` (a window boundary): each run then lands in one window.
fn split_at(runs: &mut Vec<Run>, span: usize) {
    if !runs.iter().any(|r| r.offset / span != (r.offset + r.len - 1) / span) {
        return;
    }
    let mut out = Vec::with_capacity(runs.len() + 1);
    for r in runs.drain(..) {
        let (mut at, end) = (r.offset, r.offset + r.len);
        while at < end {
            let stop = ((at / span + 1) * span).min(end);
            out.push(Run { offset: at, len: stop - at });
            at = stop;
        }
    }
    *runs = out;
}

/// Persistent workers: `run(f)` calls `f(worker)` once on each worker and returns when all are done.
struct Pool {
    jobs: Vec<Sender<Arc<dyn Fn(usize) + Send + Sync>>>,
    done: Arc<Barrier>,
}

impl Pool {
    fn new(n: usize) -> Pool {
        let done = Arc::new(Barrier::new(n + 1));
        let mut jobs = Vec::new();
        for w in 0..n {
            let (tx, rx) = channel::<Arc<dyn Fn(usize) + Send + Sync>>();
            let done = done.clone();
            std::thread::Builder::new()
                .name(format!("rebuild-diff-{w}"))
                .spawn(move || {
                    while let Ok(f) = rx.recv() {
                        f(w);
                        done.wait();
                    }
                })
                .expect("spawn diff worker");
            jobs.push(tx);
        }
        Pool { jobs, done }
    }

    fn run(&self, f: Arc<dyn Fn(usize) + Send + Sync>) {
        for tx in &self.jobs {
            tx.send(f.clone()).expect("diff worker gone");
        }
        self.done.wait();
    }

    fn workers(&self) -> usize {
        self.jobs.len()
    }
}

const SCATTER: &[u8] = br#"
ByteAddressBuffer   packed : register(t0);
ByteAddressBuffer   index  : register(t1);
RWByteAddressBuffer dst    : register(u0);
cbuffer C : register(b0) { uint count; uint gx; uint first; };
[numthreads(256, 1, 1)]
void main(uint3 g : SV_GroupID, uint t : SV_GroupIndex) {
    uint lane = (g.y * gx + g.x) * 256 + t;
    uint chunk = lane >> 2;
    if (chunk >= count) return;
    uint part = (lane & 3) * 16;
    uint at = first + chunk;
    dst.Store4(index.Load(at * 4) * 64 + part, packed.Load4(at * 64 + part));
}
"#;

/// Zero `count` 16-byte units at the root UAV's address (a reused card page, before anything reads it).
const FILL: &[u8] = br#"
RWByteAddressBuffer dst : register(u0);
cbuffer C : register(b0) { uint count; uint gx; uint first; };
[numthreads(256, 1, 1)]
void main(uint3 g : SV_GroupID, uint t : SV_GroupIndex) {
    uint lane = (g.y * gx + g.x) * 256 + t;
    if (lane < count) dst.Store4(lane * 16, uint4(0, 0, 0, 0));
}
"#;

struct Slot {
    staging: ID3D12Resource,
    staging_ptr: *mut u8,
    index: ID3D12Resource,
    index_ptr: *mut u32,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    last_use: u64,
}

struct Gpu {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    signature: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    fill: ID3D12PipelineState,
    slots: Vec<Slot>,
}

unsafe impl Send for Gpu {}

/// The GPU address of window `w` (`console / span`), if it exists.
pub type WindowVa = Arc<dyn Fn(u64) -> Option<u64> + Send + Sync>;

/// Called by a pass, before it records, with the console address of every 64 KiB page it is about to write into
/// (sorted, each once): the owner backs those that have no card page yet (lazy placement). Runs on the
/// coordinator with the region lock held; it must not wait for anything that waits for a pass.
pub type Backer = Arc<dyn Fn(&[u64]) + Send + Sync>;

/// What the submit side and the coordinator share.
struct Shared {
    /// sorted by console address; read-locked for a whole pass, write-locked to add or remove one
    regions: RwLock<Vec<Region>>,
    /// console addresses of pages read (write-watch reset) whose changes were not sent: diffed again next pass
    carry: Mutex<Vec<u64>>,
    /// (GPU address, bytes) to zero before the next pass's uploads
    zero: Mutex<Vec<(u64, u32)>>,
    backer: OnceLock<Backer>,
    totals: Totals,
}

struct Coordinator {
    gpu: Gpu,
    shared: Arc<Shared>,
    span: usize,
    va: WindowVa,
    pool: Pool,
    addresses: Vec<*mut std::ffi::c_void>,
    pages: Vec<Page>,
    work: Vec<Mutex<Work>>,
    next_slot: usize,
    slot_bytes: usize,
}

unsafe impl Send for Coordinator {}

enum Msg {
    Flush(u64, Instant),
    Stop,
}

/// The ring. Owned by the submit side; the work happens on its coordinator and pool.
pub struct Ring {
    tx: Mutex<Sender<Msg>>,
    stats: Mutex<Receiver<Result<FlushStats, String>>>,
    pub fence: ID3D12Fence,
    pub queue: ID3D12CommandQueue,
    next_value: AtomicU64,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: D3D12 objects are free-threaded; the channel ends are locked.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    /// `windows`: the GPU address of each 4 GiB window (Build 1's tiles). `regions` must each lie in
    /// one window.
    pub fn new(device: &ID3D12Device, windows: Vec<u64>, regions: Vec<Arc<Watched>>, workers: usize) -> Result<Ring, String> {
        Self::with_slot_bytes(device, windows, regions, workers, SLOT_BYTES)
    }

    /// As [`Ring::new`], with `slot_bytes` of staging per slot (a multiple of 64).
    pub fn with_slot_bytes(device: &ID3D12Device, windows: Vec<u64>, regions: Vec<Arc<Watched>>, workers: usize, slot_bytes: usize) -> Result<Ring, String> {
        let va: WindowVa = Arc::new(move |w| windows.get(w as usize).copied());
        let ring = Self::dynamic(device, WINDOW as u64, va, workers, slot_bytes)?;
        for r in regions {
            unsafe { ResetWriteWatch(r.base as *const _, r.len) };
            ring.insert(Region::new(r.base, r.len, r.console, Some(r.clone())))?;
        }
        Ok(ring)
    }

    /// A ring with no regions yet, over windows of `span` bytes whose GPU addresses `va` gives (a window that
    /// does not exist yet drops what would land there, counted). Regions come and go with [`Ring::add`] and
    /// [`Ring::remove`].
    pub fn dynamic(device: &ID3D12Device, span: u64, va: WindowVa, workers: usize, slot_bytes: usize) -> Result<Ring, String> {
        if slot_bytes == 0 || slot_bytes % CHUNK != 0 {
            return Err("a slot must hold whole chunks".into());
        }
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COMPUTE, ..Default::default() })
                .map_err(err("compute queue"))?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_SHARED).map_err(err("fence"))?;
            let (signature, pso) = scatter_pipeline(device, SCATTER)?;
            let (_, fill) = scatter_pipeline(device, FILL)?;
            let mut slots = Vec::new();
            for _ in 0..SLOTS {
                let staging = upload(device, slot_bytes)?;
                let index = upload(device, slot_bytes / CHUNK * 4)?;
                let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE).map_err(err("allocator"))?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COMPUTE, &alloc, None).map_err(err("list"))?;
                list.Close().map_err(err("close"))?;
                slots.push(Slot { staging_ptr: map(&staging)?, index_ptr: map(&index)? as *mut u32, staging, index, alloc, list, last_use: 0 });
            }
            let shared = Arc::new(Shared { regions: RwLock::new(Vec::new()), carry: Mutex::new(Vec::new()), zero: Mutex::new(Vec::new()), backer: OnceLock::new(), totals: Totals::default() });
            let coord = Coordinator {
                gpu: Gpu { queue: queue.clone(), fence: fence.clone(), signature, pso, fill, slots },
                shared: shared.clone(),
                span: span as usize,
                va,
                pool: Pool::new(workers.max(1)),
                addresses: Vec::new(),
                pages: Vec::new(),
                work: (0..workers.max(1)).map(|_| Mutex::new(Work::default())).collect(),
                next_slot: 0,
                slot_bytes,
            };
            let (tx, rx) = channel::<Msg>();
            let (stx, srx) = channel();
            let thread = std::thread::Builder::new()
                .name("rebuild-upload".into())
                .spawn(move || {
                    let mut c = coord;
                    while let Ok(Msg::Flush(mut v, mut t)) = rx.recv() {
                        // every flush asked meanwhile is served by this one pass (the fence is monotonic)
                        let mut served = 1;
                        let mut stop = false;
                        while let Ok(m) = rx.try_recv() {
                            match m {
                                Msg::Flush(v2, t2) => {
                                    v = v.max(v2);
                                    t = t.min(t2);
                                    served += 1;
                                }
                                Msg::Stop => stop = true,
                            }
                        }
                        let r = c.flush(v, t, served);
                        if stx.send(r).is_err() || stop {
                            break;
                        }
                    }
                })
                .map_err(err("spawn coordinator"))?;
            Ok(Ring { tx: Mutex::new(tx), stats: Mutex::new(srx), fence, queue, next_value: AtomicU64::new(0), shared, thread: Some(thread) })
        }
    }

    fn insert(&self, r: Region) -> Result<(), String> {
        let mut regions = self.shared.regions.write().unwrap_or_else(|e| e.into_inner());
        let k = regions.partition_point(|x| x.console < r.console);
        let overlaps = |x: &Region| x.console < r.console + r.len as u64 && r.console < x.console + x.len as u64;
        if regions.get(k).is_some_and(overlaps) || (k > 0 && overlaps(&regions[k - 1])) {
            return Err(format!("watched region {:#x} (+{:#x}) overlaps one already watched", r.console, r.len));
        }
        regions.insert(k, r);
        Ok(())
    }

    /// Watch `len` bytes of write-watched memory at `base` (whole 4 KiB pages), the GPU copy at console address
    /// `console`. Its write-watch state is kept: what was written before the first flush is sent by it.
    ///
    /// # Safety
    /// The memory stays reserved with write-watch until [`Ring::remove`] returns for it.
    pub unsafe fn add(&self, base: *mut u8, len: usize, console: u64) -> Result<(), String> {
        if len == 0 || len % OS_PAGE != 0 || base as usize % OS_PAGE != 0 {
            return Err("a watched region must be whole pages".into());
        }
        self.insert(Region::new(base, len, console, None))
    }

    /// Stop watching the region at console address `console`. Returns once no pass reads it: the caller may
    /// free it then.
    pub fn remove(&self, console: u64) -> bool {
        let mut regions = self.shared.regions.write().unwrap_or_else(|e| e.into_inner());
        let Some(k) = regions.iter().position(|r| r.console == console) else { return false };
        let r = regions.remove(k);
        let end = r.console + r.len as u64;
        self.shared.carry.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p < r.console || p >= end);
        true
    }

    /// Run `f` while no pass reads any region (decommit or protect pages in one), then drop pages of
    /// `console..+len` carried to the next pass.
    pub fn quiet<R>(&self, console: u64, len: u64, f: impl FnOnce() -> R) -> R {
        let _regions = self.shared.regions.write().unwrap_or_else(|e| e.into_inner());
        let out = f();
        self.shared.carry.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p < console || p >= console + len);
        out
    }

    /// Set what backs pages before a pass writes them ([`Backer`]); once.
    pub fn set_backer(&self, b: Backer) {
        let _ = self.shared.backer.set(b);
    }

    /// Zero `bytes` (a multiple of 16) at GPU address `va` on the ring's queue, ahead of the next pass's uploads.
    pub fn zero(&self, va: u64, bytes: u32) {
        self.shared.zero.lock().unwrap_or_else(|e| e.into_inner()).push((va, bytes));
    }

    /// Ask for everything the CPU changed since the last flush to be sent. Returns at once with
    /// the fence value the title's queue must wait for. Does no copying on the calling thread.
    pub fn flush(&self) -> u64 {
        let v = self.next_value.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(Msg::Flush(v, Instant::now()));
        v
    }

    /// Statistics of finished flushes, oldest first (non-blocking).
    pub fn finished(&self) -> Vec<Result<FlushStats, String>> {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).try_iter().collect()
    }

    /// The next finished flush's statistics, waiting for the coordinator (tests).
    pub fn next_finished(&self) -> Result<FlushStats, String> {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).recv().map_err(|_| "upload coordinator gone".to_string())?
    }

    /// Totals over the ring's life.
    pub fn totals(&self) -> &Totals {
        &self.shared.totals
    }

    /// Watched regions now.
    pub fn regions(&self) -> (usize, u64) {
        let r = self.shared.regions.read().unwrap_or_else(|e| e.into_inner());
        (r.len(), r.iter().map(|r| r.len as u64).sum())
    }

    /// Block until flush `value` has landed on the GPU (tests and shutdown only).
    pub fn wait(&self, value: u64) -> Result<(), String> {
        unsafe { self.fence.SetEventOnCompletion(value, HANDLE::default()) }.map_err(err("wait"))
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Coordinator {
    /// One pass. On failure every page read this time (write-watch already reset for it) is carried, and no
    /// hash was committed, so the next pass sends those changes.
    fn flush(&mut self, value: u64, asked: Instant, served: u64) -> Result<FlushStats, String> {
        let shared = self.shared.clone();
        let t = Instant::now();
        let regions = shared.regions.read().unwrap_or_else(|e| e.into_inner());
        shared.totals.lock_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.pages.clear();
        let mut carry_in = std::mem::take(&mut *shared.carry.lock().unwrap_or_else(|e| e.into_inner()));
        let r = self.flush_inner(&regions, &mut carry_in, value, asked);
        let tot = &shared.totals;
        tot.passes.fetch_add(1, Ordering::Relaxed);
        tot.flushes.fetch_add(served, Ordering::Relaxed);
        tot.busy_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        let mut carry = shared.carry.lock().unwrap_or_else(|e| e.into_inner());
        match &r {
            Ok(st) => {
                let us = |ms: f64| (ms * 1000.0) as u64;
                tot.slot_wait_us.fetch_add(us(st.slot_wait_ms), Ordering::Relaxed);
                tot.watch_us.fetch_add(us(st.watch_ms), Ordering::Relaxed);
                tot.diff_us.fetch_add(us(st.diff_ms), Ordering::Relaxed);
                tot.gather_us.fetch_add(us(st.gather_ms), Ordering::Relaxed);
                tot.record_us.fetch_add(us(st.record_ms), Ordering::Relaxed);
                tot.bytes.fetch_add(st.bytes as u64, Ordering::Relaxed);
                tot.latency_us.fetch_add((st.latency_ms * 1000.0) as u64, Ordering::Relaxed);
                tot.carried.fetch_add(st.carried_pages as u64, Ordering::Relaxed);
                tot.homeless.fetch_add(st.homeless as u64, Ordering::Relaxed);
                carry.append(&mut carry_in);
            }
            Err(_) => {
                tot.failed.fetch_add(1, Ordering::Relaxed);
                // what was read this time, and the leftovers if they were not merged yet
                carry.extend(self.pages.iter().map(|p| regions[p.region as usize].console + p.offset as u64));
                carry.append(&mut carry_in);
                carry.sort_unstable();
                carry.dedup();
            }
        }
        r.map(|mut st| {
            st.served = served;
            st
        })
    }

    /// `carry` comes in as the pages carried from the last pass and goes out as those to carry to the next.
    fn flush_inner(&mut self, regions: &[Region], carry: &mut Vec<u64>, value: u64, asked: Instant) -> Result<FlushStats, String> {
        let mut st = FlushStats { value, ..Default::default() };

        // a slot the GPU has finished with
        let t = Instant::now();
        let si = self.next_slot;
        self.next_slot = (si + 1) % self.gpu.slots.len();
        let last = self.gpu.slots[si].last_use;
        if last > 0 {
            unsafe { self.gpu.fence.SetEventOnCompletion(last, HANDLE::default()) }.map_err(err("slot wait"))?;
        }
        st.slot_wait_ms = ms(t);

        // watch
        let t = Instant::now();
        for (ri, r) in regions.iter().enumerate() {
            let want = r.len / OS_PAGE;
            if self.addresses.len() < want {
                self.addresses.resize(want, std::ptr::null_mut());
            }
            let mut count = want;
            let mut gran = 0u32;
            let rc = unsafe {
                GetWriteWatch(WRITE_WATCH_FLAG_RESET, r.base.0 as *const _, r.len, Some(self.addresses.as_mut_ptr()), Some(&mut count), Some(&mut gran))
            };
            if rc != 0 {
                // not write-watched memory (a bug of the caller's): skipped and counted, the rest goes up
                st.unwatchable += 1;
                continue;
            }
            let base = r.base.0 as usize;
            self.pages.extend(self.addresses[..count].iter().map(|a| Page { region: ri as u32, offset: (*a as usize - base) as u32 }));
        }
        if !carry.is_empty() {
            // the last pass's leftovers (by console address: regions may have come and gone), merged so the
            // pages stay in console order, each once
            for &p in carry.iter() {
                let k = regions.partition_point(|r| r.console <= p);
                if k > 0 && p < regions[k - 1].console + regions[k - 1].len as u64 {
                    self.pages.push(Page { region: (k - 1) as u32, offset: (p - regions[k - 1].console) as u32 });
                }
            }
            carry.clear();
            self.pages.sort_unstable();
            self.pages.dedup();
        }
        st.pages = self.pages.len();
        st.watch_ms = ms(t);

        // diff on the pool: contiguous shares, so runs stay in console-address order
        let t = Instant::now();
        let n = self.pool.workers();
        let per = self.pages.len().div_ceil(n).max(1);
        let pages = Ptr(self.pages.as_mut_ptr());
        let npages = self.pages.len();
        let regs = Ptr(regions.as_ptr() as *mut Region);
        let nregs = regions.len();
        let work = Ptr(self.work.as_mut_ptr());
        let span = self.span;
        self.pool.run(Arc::new(move |w| {
            let (pages, work, regs) = (pages, work, regs);
            // SAFETY: worker w owns pages [w*per, (w+1)*per) and work[w]; the regions are read-locked by the coordinator
            let mine = unsafe { std::slice::from_raw_parts(pages.0, npages) };
            let mine = &mine[(w * per).min(npages)..((w + 1) * per).min(npages)];
            let regs = unsafe { std::slice::from_raw_parts(regs.0 as *const Region, nregs) };
            let mut guard = unsafe { (*work.0.add(w)).lock().unwrap() };
            let out = &mut *guard;
            out.runs.clear();
            out.pending.clear();
            let mut idx = Vec::new();
            for p in mine {
                let r = &regs[p.region as usize];
                let off = p.offset as usize;
                let first = off / CHUNK;
                idx.clear();
                unsafe {
                    let page = std::slice::from_raw_parts(r.base.0.add(off), OS_PAGE);
                    chunks::diff_hash_pending(page, r.page_hashes(off), r.console as usize + off, first, &mut out.runs, &mut idx);
                }
                out.pending.extend(idx.iter().map(|&(i, h)| (p.region, i as u32, h)));
            }
            split_at(&mut out.runs, span);
        }));
        st.diff_ms = ms(t);

        // fit: the first slot's worth in console order goes now; pages holding the rest are carried
        let mut room = self.slot_bytes;
        for m in &self.work {
            let mut guard = m.lock().unwrap();
            let wk = &mut *guard;
            wk.kept = fit(&mut wk.runs, &mut room);
            for &(r, i, _) in &wk.pending[wk.kept..] {
                let page = regions[r as usize].console + ((i as usize * CHUNK) & !(OS_PAGE - 1)) as u64;
                if carry.last() != Some(&page) {
                    carry.push(page);
                }
            }
        }

        // the pages the kept runs land in get card pages first, if the owner places them lazily
        if let Some(backer) = self.shared.backer.get() {
            let mut need: Vec<u64> = Vec::new();
            for r in &self.work {
                for run in r.lock().unwrap().runs.iter() {
                    let (first, end) = (run.offset / crate::PAGE, (run.offset + run.len).div_ceil(crate::PAGE));
                    for p in first..end {
                        let a = (p * crate::PAGE) as u64;
                        if need.last() != Some(&a) {
                            need.push(a);
                        }
                    }
                }
            }
            if !need.is_empty() {
                backer(&need);
            }
        }

        // gather: each worker packs its runs into the slot at its own offset
        let t = Instant::now();
        let mut bases = Vec::with_capacity(n);
        let mut at = 0usize;
        for r in &self.work {
            bases.push(at);
            at += chunks::bytes(&r.lock().unwrap().runs);
        }
        st.bytes = at;
        st.runs = self.work.iter().map(|r| r.lock().unwrap().runs.len()).sum();
        debug_assert!(at <= self.slot_bytes);
        let slot = &self.gpu.slots[si];
        let (staging, index) = (Ptr(slot.staging_ptr), Ptr(slot.index_ptr));
        let bases = Arc::new(bases);
        let b2 = bases.clone();
        self.pool.run(Arc::new(move |w| {
            let (work, staging, index, regs) = (work, staging, index, regs);
            assert!(!chunks::is_submit_thread(), "title memory copied on the submit thread");
            let regs = unsafe { std::slice::from_raw_parts(regs.0 as *const Region, nregs) };
            let out = unsafe { (*work.0.add(w)).lock().unwrap() };
            let mut at = b2[w];
            for run in out.runs.iter() {
                // the region holding this run (sorted: a binary search)
                let k = regs.partition_point(|r| r.console as usize <= run.offset) - 1;
                let r = &regs[k];
                unsafe {
                    std::ptr::copy_nonoverlapping(r.base.0.add(run.offset - r.console as usize), staging.0.add(at), run.len);
                    let first = ((run.offset % span) / CHUNK) as u32;
                    for c in 0..(run.len / CHUNK) as u32 {
                        index.0.add(at / CHUNK + c as usize).write(first + c);
                    }
                }
                at += run.len;
            }
        }));
        st.gather_ms = ms(t);

        // record: zero fills first, then one dispatch per window (chunks are in console order, and no run
        // crosses a window, so each window is a contiguous packed range)
        let t = Instant::now();
        let mut per_window: Vec<(usize, u32, u32)> = Vec::new(); // window, first chunk, count
        let mut chunk = 0u32;
        for r in &self.work {
            for run in r.lock().unwrap().runs.iter() {
                let w = run.offset / span;
                let c = (run.len / CHUNK) as u32;
                match per_window.last_mut() {
                    Some(last) if last.0 == w && last.1 + last.2 == chunk => last.2 += c,
                    _ => per_window.push((w, chunk, c)),
                }
                chunk += c;
            }
        }
        let zero = std::mem::take(&mut *self.shared.zero.lock().unwrap_or_else(|e| e.into_inner()));
        st.zeroed = zero.len();
        unsafe {
            let slot = &mut self.gpu.slots[si];
            slot.alloc.Reset().map_err(err("reset"))?;
            slot.list.Reset(&slot.alloc, &self.gpu.fill).map_err(err("reset"))?;
            slot.list.SetComputeRootSignature(&self.gpu.signature);
            for &(va, bytes) in &zero {
                let units = bytes / 16;
                let groups = units.div_ceil(256);
                let gx = groups.min(32768);
                slot.list.SetComputeRoot32BitConstant(0, units, 0);
                slot.list.SetComputeRoot32BitConstant(0, gx, 1);
                slot.list.SetComputeRootUnorderedAccessView(3, va);
                slot.list.Dispatch(gx, groups.div_ceil(gx), 1);
            }
            if !zero.is_empty() && !per_window.is_empty() {
                // the uploads may land on the pages just zeroed
                slot.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 { UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }) },
                }]);
            }
            slot.list.SetPipelineState(&self.gpu.pso);
            slot.list.SetComputeRootShaderResourceView(1, slot.staging.GetGPUVirtualAddress());
            slot.list.SetComputeRootShaderResourceView(2, slot.index.GetGPUVirtualAddress());
            for &(w, first, count) in &per_window {
                let Some(va) = (self.va)(w as u64) else {
                    st.homeless += count as usize;
                    continue;
                };
                let groups = (count * 4).div_ceil(256);
                let gx = groups.min(32768);
                slot.list.SetComputeRoot32BitConstant(0, count, 0);
                slot.list.SetComputeRoot32BitConstant(0, gx, 1);
                slot.list.SetComputeRoot32BitConstant(0, first, 2);
                slot.list.SetComputeRootUnorderedAccessView(3, va);
                slot.list.Dispatch(gx, groups.div_ceil(gx), 1);
            }
            slot.list.Close().map_err(err("close"))?;
            if !per_window.is_empty() || !zero.is_empty() {
                self.gpu.queue.ExecuteCommandLists(&[Some(slot.list.cast().map_err(err("cast"))?)]);
            }
            self.gpu.queue.Signal(&self.gpu.fence, value).map_err(err("signal"))?;
            slot.last_use = value;
        }
        st.record_ms = ms(t);

        // commit: the scatter is queued ahead of the Signal every user waits for, so the kept chunks count as
        // sent; each worker writes its own pages' hashes
        self.pool.run(Arc::new(move |w| {
            let (work, regs) = (work, regs);
            let regs = unsafe { std::slice::from_raw_parts(regs.0 as *const Region, nregs) };
            let out = unsafe { (*work.0.add(w)).lock().unwrap() };
            for &(r, i, h) in &out.pending[..out.kept] {
                regs[r as usize].commit(i as usize, h);
            }
        }));
        st.carried_pages = carry.len();
        st.latency_ms = ms(asked);
        Ok(st)
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn upload(device: &ID3D12Device, bytes: usize) -> Result<ID3D12Resource, String> {
    let mut desc = crate::tiles::buffer_desc(bytes as u64);
    desc.Flags = D3D12_RESOURCE_FLAG_NONE;
    let mut r: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut r,
        )
    }
    .map_err(err("staging"))?;
    r.ok_or_else(|| "staging: none".into())
}

fn map(r: &ID3D12Resource) -> Result<*mut u8, String> {
    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { r.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)) }.map_err(err("map"))?;
    Ok(p as *mut u8)
}

fn scatter_pipeline(device: &ID3D12Device, source: &[u8]) -> Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
    unsafe {
        let blob = |b: &ID3DBlob| std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec();
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        D3DCompile(source.as_ptr().cast(), source.len(), PCSTR::null(), None, None, windows::core::s!("main"), windows::core::s!("cs_5_0"), 0, 0, &mut code, Some(&mut errors))
            .map_err(|e| format!("scatter compile: {e}: {}", errors.as_ref().map(|b| String::from_utf8_lossy(&blob(b)).into_owned()).unwrap_or_default()))?;
        let code = blob(&code.ok_or("scatter: no code")?);
        let view = |kind, reg| D3D12_ROOT_PARAMETER {
            ParameterType: kind,
            Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: reg, RegisterSpace: 0 } },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let params = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 3 } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
            view(D3D12_ROOT_PARAMETER_TYPE_SRV, 1),
            view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
        ];
        let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 4, pParameters: params.as_ptr(), ..Default::default() };
        let mut sb: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut sb, None).map_err(err("signature"))?;
        let signature: ID3D12RootSignature = device.CreateRootSignature(0, &blob(&sb.ok_or("signature: none")?)).map_err(err("signature"))?;
        let pso = device
            .CreateComputePipelineState::<ID3D12PipelineState>(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&signature),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            })
            .map_err(err("scatter pipeline"))?;
        Ok((signature, pso))
    }
}

/// Count the pages write-watch reports for a region without resetting (tests).
pub fn written_pages(r: &Watched) -> usize {
    let mut addrs = vec![std::ptr::null_mut(); r.len / OS_PAGE];
    let mut count = addrs.len();
    let mut gran = 0u32;
    unsafe { GetWriteWatch(0, r.base as *const _, r.len, Some(addrs.as_mut_ptr()), Some(&mut count), Some(&mut gran)) };
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_watch_counts_exactly_the_pages_written() {
        let r = Watched::alloc(0, 64 * OS_PAGE).unwrap();
        unsafe { ResetWriteWatch(r.base as *const _, r.len) };
        for p in [0usize, 3, 4, 63] {
            unsafe { r.base.add(p * OS_PAGE + 17).write_volatile(1) };
        }
        // two writes into one page count once
        unsafe { r.base.add(3 * OS_PAGE + 900).write_volatile(2) };
        assert_eq!(written_pages(&r), 4);
        unsafe { ResetWriteWatch(r.base as *const _, r.len) };
        assert_eq!(written_pages(&r), 0);
        // reading a page that is already in memory does not count
        let _ = unsafe { r.base.add(3 * OS_PAGE).read_volatile() };
        assert_eq!(written_pages(&r), 0);
        // measured: the first touch of a never-used (demand-zero) page counts, even a read. The diff
        // then finds nothing changed and sends nothing, so it costs one page diff, never a wrong byte.
        let _ = unsafe { r.base.add(10 * OS_PAGE).read_volatile() };
        assert_eq!(written_pages(&r), 1);
    }

    #[test]
    fn a_region_may_not_straddle_a_window() {
        assert!(Watched::alloc((WINDOW - OS_PAGE) as u64, 2 * OS_PAGE).is_err());
    }

    #[test]
    fn fit_keeps_the_first_bytes_and_cuts_at_a_chunk() {
        let runs = vec![Run { offset: 0, len: 128 }, Run { offset: 1024, len: 256 }, Run { offset: 4096, len: 64 }];
        let mut a = runs.clone();
        let mut room = 1 << 20;
        assert_eq!(fit(&mut a, &mut room), 7);
        assert_eq!((a, room), (runs.clone(), (1 << 20) - 448));
        let mut b = runs.clone();
        let mut room = 192;
        assert_eq!(fit(&mut b, &mut room), 3);
        assert_eq!((b, room), (vec![Run { offset: 0, len: 128 }, Run { offset: 1024, len: 64 }], 0));
        // a later worker finds no room
        let mut c = runs.clone();
        assert_eq!(fit(&mut c, &mut room), 0);
        assert!(c.is_empty());
        // exactly full
        let mut d = runs.clone();
        let mut room = 384;
        assert_eq!(fit(&mut d, &mut room), 6);
        assert_eq!(d, runs[..2].to_vec());
    }

    #[test]
    fn runs_are_cut_at_window_boundaries() {
        let mut r = vec![Run { offset: 0, len: 128 }, Run { offset: 4096 - 64, len: 192 }, Run { offset: 9000, len: 64 }];
        split_at(&mut r, 4096);
        assert_eq!(r, vec![Run { offset: 0, len: 128 }, Run { offset: 4096 - 64, len: 64 }, Run { offset: 4096, len: 128 }, Run { offset: 9000, len: 64 }]);
    }

    fn warp() -> ID3D12Device {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4};
        unsafe {
            let f: IDXGIFactory4 = CreateDXGIFactory1().expect("DXGI factory");
            let a: IDXGIAdapter = f.EnumWarpAdapter().expect("WARP adapter");
            let mut d: Option<ID3D12Device> = None;
            D3D12CreateDevice(&a, D3D_FEATURE_LEVEL_11_0, &mut d).expect("WARP device");
            d.expect("WARP device")
        }
    }

    fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: usize, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let mut desc = crate::tiles::buffer_desc(bytes as u64);
        if kind != D3D12_HEAP_TYPE_DEFAULT {
            desc.Flags = D3D12_RESOURCE_FLAG_NONE;
        }
        let mut r: Option<ID3D12Resource> = None;
        unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }
            .expect("buffer");
        r.expect("buffer")
    }

    /// What the GPU holds in `window`'s first `len` bytes.
    fn read_back(device: &ID3D12Device, window: &ID3D12Resource, len: usize) -> Vec<u8> {
        read_at(device, window, 0, len)
    }

    fn read_at(device: &ID3D12Device, window: &ID3D12Resource, offset: u64, len: usize) -> Vec<u8> {
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
            let rb = buffer(device, D3D12_HEAP_TYPE_READBACK, len, D3D12_RESOURCE_STATE_COPY_DEST);
            list.CopyBufferRegion(&rb, 0, window, offset, len as u64);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, HANDLE::default()).unwrap();
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            rb.Map(0, Some(&D3D12_RANGE { Begin: 0, End: len }), Some(&mut p)).unwrap();
            let v = std::slice::from_raw_parts(p as *const u8, len).to_vec();
            rb.Unmap(0, None);
            v
        }
    }

    /// Item 6 of the deep dive: a flush that changed more than a slot holds used to fail after the diff had
    /// already marked every changed chunk as sent (and write-watch was reset), so those chunks never went up. Now
    /// a hash counts only once its scatter is queued; the rest is carried, sent by the next flushes, each chunk
    /// once, and the GPU ends up with every byte.
    #[test]
    fn a_flush_larger_than_a_slot_carries_the_rest_and_loses_nothing() {
        let device = warp();
        let len = 16 * OS_PAGE;
        let region = Arc::new(Watched::alloc(0, len).unwrap());
        let window = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, len, D3D12_RESOURCE_STATE_COMMON);
        let va = unsafe { window.GetGPUVirtualAddress() };
        let ring = Ring::with_slot_bytes(&device, vec![va], vec![region.clone()], 2, 2 * OS_PAGE).unwrap();

        // five whole pages change: 20 KiB against an 8 KiB slot
        let changed = [0usize, 1, 5, 9, 15];
        for &p in &changed {
            for b in 0..OS_PAGE {
                unsafe { region.base.add(p * OS_PAGE + b).write_volatile(((p * 31 + b) as u8) | 1) };
            }
        }
        let mut stats = Vec::new();
        let mut last;
        loop {
            last = ring.flush();
            let st = ring.next_finished().unwrap();
            stats.push(st);
            if st.carried_pages == 0 {
                break;
            }
            assert!(stats.len() < 10, "carrying never ends");
        }
        let bytes: Vec<usize> = stats.iter().map(|s| s.bytes).collect();
        let carried: Vec<usize> = stats.iter().map(|s| s.carried_pages).collect();
        assert_eq!(bytes, vec![2 * OS_PAGE, 2 * OS_PAGE, OS_PAGE], "a slot's worth a flush");
        assert_eq!(carried, vec![3, 1, 0]);
        // nothing is sent twice, and nothing is left
        last = last.max(ring.flush());
        assert_eq!(ring.next_finished().unwrap().bytes, 0);
        ring.wait(last).unwrap();

        let got = read_back(&device, &window, len);
        let want = unsafe { std::slice::from_raw_parts(region.base as *const u8, len) };
        if let Some(i) = got.chunks(CHUNK).zip(want.chunks(CHUNK)).position(|(a, b)| a != b) {
            panic!("first wrong chunk at +{:#x}", i * CHUNK);
        }
    }

    /// The title run's shape: a region registered after the title wrote it lands in its 512 MiB sub-window (made on
    /// demand), and a card page reused for new memory is zeroed before anything reads it (else the old bytes show).
    #[test]
    fn a_dynamic_region_lands_in_its_sub_window_and_reused_pages_are_zeroed() {
        use crate::space::{Space, SUB};
        use crate::{Uma, PAGE};
        let device = warp();
        let space: Arc<OnceLock<Mutex<Space>>> = Arc::new(OnceLock::new());
        let s2 = space.clone();
        let va: WindowVa = Arc::new(move |k| s2.get()?.lock().unwrap().va(k as usize));
        let ring = Ring::dynamic(&device, SUB, va, 2, 1 << 20).unwrap();
        assert!(space.set(Mutex::new(Space::new(&device, &ring.queue).unwrap())).is_ok());
        let space = space.get().unwrap();
        let mut uma = Uma::new(4096, 64 << 20);

        let k = 9u64;
        let console = k * SUB + 3 * PAGE as u64;
        let (id, mapped) = uma.place(console, 2 * PAGE).unwrap();
        space.lock().unwrap().map(&mapped).unwrap();
        assert_eq!(space.lock().unwrap().take_created(), vec![k as u32]);
        let region = Watched::alloc(0, 2 * PAGE).unwrap();
        for i in 0..2 * PAGE {
            unsafe { region.base.add(i).write_volatile((i as u8) | 1) };
        }
        unsafe { ring.add(region.base, region.len, console).unwrap() };
        let v = ring.flush();
        let st = ring.next_finished().unwrap();
        assert_eq!(st.bytes, 2 * PAGE);
        assert_eq!(st.homeless, 0);
        ring.wait(v).unwrap();
        let sub = space.lock().unwrap().sub(k as usize).unwrap().clone();
        let got = read_at(&device, &sub, 3 * PAGE as u64, 2 * PAGE);
        assert!(got == unsafe { std::slice::from_raw_parts(region.base as *const u8, 2 * PAGE) }, "the region through its sub-window");

        // the memory goes; its card pages are reused for new memory elsewhere in the sub-window
        assert!(ring.remove(console));
        space.lock().unwrap().unmap(&uma.retire(id).unwrap()).unwrap();
        let before = uma.card_pages_reached();
        let console2 = k * SUB + 40 * PAGE as u64;
        let (_, mapped) = uma.place(console2, PAGE).unwrap();
        assert!(mapped.iter().all(|b| b.card < before), "a reused card page");
        space.lock().unwrap().map(&mapped).unwrap();
        let sub_va = space.lock().unwrap().va(k as usize).unwrap();
        ring.zero(sub_va + 40 * PAGE as u64, PAGE as u32);
        let v = ring.flush();
        assert_eq!(ring.next_finished().unwrap().zeroed, 1);
        ring.wait(v).unwrap();
        assert!(read_at(&device, &sub, 40 * PAGE as u64, PAGE).iter().all(|&b| b == 0), "the old bytes are gone");
    }
}
