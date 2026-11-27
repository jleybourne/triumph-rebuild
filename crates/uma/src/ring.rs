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

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex};
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

/// A dirty page: which region, and its byte offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Page {
    region: u32,
    offset: u32,
}

/// One diff worker's output: its runs in console order and, chunk for chunk in the same order, the hash each
/// chunk will count as sent once submitted (index into the coordinator's `sent`).
#[derive(Default)]
struct Work {
    runs: Vec<Run>,
    pending: Vec<(usize, u64)>,
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

/// The page holding hash `i`, given where each region's hashes start in `sent`.
fn page_of(starts: &[usize], i: usize) -> Page {
    let r = starts.partition_point(|&s| s <= i) - 1;
    Page { region: r as u32, offset: (((i - starts[r]) * CHUNK) & !(OS_PAGE - 1)) as u32 }
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
    slots: Vec<Slot>,
    windows: Vec<u64>,
}

unsafe impl Send for Gpu {}

struct Coordinator {
    gpu: Gpu,
    regions: Vec<Arc<Watched>>,
    /// The hash of every chunk as sent, all regions back to back; region r's start at `starts[r]`.
    sent: Vec<u64>,
    starts: Vec<usize>,
    pool: Pool,
    addresses: Vec<*mut std::ffi::c_void>,
    pages: Vec<Page>,
    /// Pages read (write-watch reset) whose changes were not submitted: diffed again next flush.
    carry: Vec<Page>,
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
    tx: Sender<Msg>,
    stats: Receiver<Result<FlushStats, String>>,
    pub fence: ID3D12Fence,
    next_value: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ring {
    /// `windows`: the GPU address of each 4 GiB window (Build 1's tiles). `regions` must each lie in
    /// one window, sorted by console address.
    pub fn new(device: &ID3D12Device, windows: Vec<u64>, regions: Vec<Arc<Watched>>, workers: usize) -> Result<Ring, String> {
        Self::with_slot_bytes(device, windows, regions, workers, SLOT_BYTES)
    }

    /// As [`Ring::new`], with `slot_bytes` of staging per slot (a multiple of 64).
    pub fn with_slot_bytes(device: &ID3D12Device, windows: Vec<u64>, regions: Vec<Arc<Watched>>, workers: usize, slot_bytes: usize) -> Result<Ring, String> {
        if slot_bytes == 0 || slot_bytes % CHUNK != 0 {
            return Err("a slot must hold whole chunks".into());
        }
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COMPUTE, ..Default::default() })
                .map_err(err("compute queue"))?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_SHARED).map_err(err("fence"))?;
            let (signature, pso) = scatter_pipeline(device)?;
            let mut slots = Vec::new();
            for _ in 0..SLOTS {
                let staging = upload(device, slot_bytes)?;
                let index = upload(device, slot_bytes / CHUNK * 4)?;
                let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE).map_err(err("allocator"))?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COMPUTE, &alloc, None).map_err(err("list"))?;
                list.Close().map_err(err("close"))?;
                slots.push(Slot { staging_ptr: map(&staging)?, index_ptr: map(&index)? as *mut u32, staging, index, alloc, list, last_use: 0 });
            }
            for r in &regions {
                ResetWriteWatch(r.base as *const _, r.len);
            }
            let max_pages: usize = regions.iter().map(|r| r.len / OS_PAGE).max().unwrap_or(0);
            let starts: Vec<usize> = regions.iter().scan(0, |at, r| Some(std::mem::replace(at, *at + r.len / CHUNK))).collect();
            let coord = Coordinator {
                gpu: Gpu { queue, fence: fence.clone(), signature, pso, slots, windows },
                sent: vec![chunks::zero_hash(); regions.iter().map(|r| r.len / CHUNK).sum()],
                starts,
                regions,
                pool: Pool::new(workers.max(1)),
                addresses: vec![std::ptr::null_mut(); max_pages],
                pages: Vec::new(),
                carry: Vec::new(),
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
                    while let Ok(Msg::Flush(v, t)) = rx.recv() {
                        let r = c.flush(v, t);
                        if stx.send(r).is_err() {
                            break;
                        }
                    }
                })
                .map_err(err("spawn coordinator"))?;
            Ok(Ring { tx, stats: srx, fence, next_value: 0, thread: Some(thread) })
        }
    }

    /// Ask for everything the CPU changed since the last flush to be sent. Returns at once with
    /// the fence value the title's queue must wait for. Does no copying on the calling thread.
    pub fn flush(&mut self) -> u64 {
        self.next_value += 1;
        self.tx.send(Msg::Flush(self.next_value, Instant::now())).expect("upload coordinator gone");
        self.next_value
    }

    /// Statistics of finished flushes, oldest first (non-blocking).
    pub fn finished(&self) -> Vec<Result<FlushStats, String>> {
        self.stats.try_iter().collect()
    }

    /// The next finished flush's statistics, waiting for the coordinator (tests).
    pub fn next_finished(&self) -> Result<FlushStats, String> {
        self.stats.recv().map_err(|_| "upload coordinator gone".to_string())?
    }

    /// Block until flush `value` has landed on the GPU (tests and shutdown only).
    pub fn wait(&self, value: u64) -> Result<(), String> {
        unsafe { self.fence.SetEventOnCompletion(value, HANDLE::default()) }.map_err(err("wait"))
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Coordinator {
    /// One flush. On failure every page read this time (write-watch already reset for it) is carried, and no
    /// hash was committed, so the next flush sends those changes.
    fn flush(&mut self, value: u64, asked: Instant) -> Result<FlushStats, String> {
        self.pages.clear();
        let r = self.flush_inner(value, asked);
        if r.is_err() {
            // what was read this time, and the last flush's leftovers if they were not merged yet
            let mut carry = std::mem::take(&mut self.pages);
            carry.append(&mut self.carry);
            carry.sort_unstable();
            carry.dedup();
            self.carry = carry;
        }
        r
    }

    fn flush_inner(&mut self, value: u64, asked: Instant) -> Result<FlushStats, String> {
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
        self.pages.clear();
        for (ri, r) in self.regions.iter().enumerate() {
            let mut count = self.addresses.len();
            let mut gran = 0u32;
            let rc = unsafe {
                GetWriteWatch(WRITE_WATCH_FLAG_RESET, r.base as *const _, r.len, Some(self.addresses.as_mut_ptr()), Some(&mut count), Some(&mut gran))
            };
            if rc != 0 {
                return Err("GetWriteWatch failed".into());
            }
            let base = r.base as usize;
            self.pages.extend(self.addresses[..count].iter().map(|a| Page { region: ri as u32, offset: (*a as usize - base) as u32 }));
        }
        if !self.carry.is_empty() {
            // the last flush's leftovers, merged so the pages stay in console order, each once
            self.pages.append(&mut self.carry);
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
        let regions: Vec<(Ptr<u8>, u64, usize)> = self.regions.iter().zip(&self.starts).map(|(r, &s)| (Ptr(r.base), r.console, s)).collect();
        let regions = Arc::new(regions);
        let sent = Ptr(self.sent.as_mut_ptr());
        let work = Ptr(self.work.as_mut_ptr());
        let rs = regions.clone();
        self.pool.run(Arc::new(move |w| {
            let (pages, work, sent) = (pages, work, sent);
            // SAFETY: worker w owns pages [w*per, (w+1)*per) and work[w]; `sent` is only read here
            let mine = unsafe { std::slice::from_raw_parts(pages.0, npages) };
            let mine = &mine[(w * per).min(npages)..((w + 1) * per).min(npages)];
            let mut guard = unsafe { (*work.0.add(w)).lock().unwrap() };
            let out = &mut *guard;
            out.runs.clear();
            out.pending.clear();
            for p in mine {
                let (base, console, start) = rs[p.region as usize];
                let off = p.offset as usize;
                let first = start + off / CHUNK;
                unsafe {
                    let page = std::slice::from_raw_parts(base.0.add(off), OS_PAGE);
                    let h = std::slice::from_raw_parts(sent.0.add(first), CHUNKS_PER_PAGE);
                    chunks::diff_hash_pending(page, h, console as usize + off, first, &mut out.runs, &mut out.pending);
                }
            }
        }));
        st.diff_ms = ms(t);

        // fit: the first slot's worth in console order goes now; pages holding the rest are carried
        let mut room = self.slot_bytes;
        let mut carry = Vec::new();
        for m in &self.work {
            let mut guard = m.lock().unwrap();
            let wk = &mut *guard;
            wk.kept = fit(&mut wk.runs, &mut room);
            for &(i, _) in &wk.pending[wk.kept..] {
                let page = page_of(&self.starts, i);
                if carry.last() != Some(&page) {
                    carry.push(page);
                }
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
        let (rs, b2) = (regions.clone(), bases.clone());
        self.pool.run(Arc::new(move |w| {
            let (work, staging, index) = (work, staging, index);
            assert!(!chunks::is_submit_thread(), "title memory copied on the submit thread");
            let out = unsafe { (*work.0.add(w)).lock().unwrap() };
            let mut at = b2[w];
            for run in out.runs.iter() {
                // the region holding this run (few regions: a linear search is fine)
                let (base, console, _) = rs
                    .iter()
                    .copied()
                    .filter(|(_, c, _)| *c as usize <= run.offset)
                    .max_by_key(|(_, c, _)| *c)
                    .expect("run inside a region");
                unsafe {
                    std::ptr::copy_nonoverlapping(base.0.add(run.offset - console as usize), staging.0.add(at), run.len);
                    let first = ((run.offset % WINDOW) / CHUNK) as u32;
                    for k in 0..(run.len / CHUNK) as u32 {
                        index.0.add(at / CHUNK + k as usize).write(first + k);
                    }
                }
                at += run.len;
            }
        }));
        st.gather_ms = ms(t);

        // record one dispatch per window: chunks are in console order, so each window is a
        // contiguous packed range
        let t = Instant::now();
        let mut per_window: Vec<(usize, u32, u32)> = Vec::new(); // window, first chunk, count
        let mut chunk = 0u32;
        for r in &self.work {
            for run in r.lock().unwrap().runs.iter() {
                let w = run.offset / WINDOW;
                let c = (run.len / CHUNK) as u32;
                match per_window.last_mut() {
                    Some(last) if last.0 == w => last.2 += c,
                    _ => per_window.push((w, chunk, c)),
                }
                chunk += c;
            }
        }
        unsafe {
            let slot = &mut self.gpu.slots[si];
            slot.alloc.Reset().map_err(err("reset"))?;
            slot.list.Reset(&slot.alloc, &self.gpu.pso).map_err(err("reset"))?;
            slot.list.SetComputeRootSignature(&self.gpu.signature);
            slot.list.SetComputeRootShaderResourceView(1, slot.staging.GetGPUVirtualAddress());
            slot.list.SetComputeRootShaderResourceView(2, slot.index.GetGPUVirtualAddress());
            for &(w, first, count) in &per_window {
                let groups = (count * 4).div_ceil(256);
                let gx = groups.min(32768);
                slot.list.SetComputeRoot32BitConstant(0, count, 0);
                slot.list.SetComputeRoot32BitConstant(0, gx, 1);
                slot.list.SetComputeRoot32BitConstant(0, first, 2);
                slot.list.SetComputeRootUnorderedAccessView(3, self.gpu.windows[w]);
                slot.list.Dispatch(gx, groups.div_ceil(gx), 1);
            }
            slot.list.Close().map_err(err("close"))?;
            if !per_window.is_empty() {
                self.gpu.queue.ExecuteCommandLists(&[Some(slot.list.cast().map_err(err("cast"))?)]);
            }
            self.gpu.queue.Signal(&self.gpu.fence, value).map_err(err("signal"))?;
            slot.last_use = value;
        }
        st.record_ms = ms(t);

        // commit: the scatter is queued ahead of the Signal every user waits for, so the kept chunks count as
        // sent; each worker writes its own pages' hashes
        self.pool.run(Arc::new(move |w| {
            let (work, sent) = (work, sent);
            // SAFETY: worker w's pending hashes are of its own pages, disjoint from every other worker's
            let out = unsafe { (*work.0.add(w)).lock().unwrap() };
            for &(i, h) in &out.pending[..out.kept] {
                unsafe { sent.0.add(i).write(h) };
            }
        }));
        st.carried_pages = carry.len();
        self.carry = carry;
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

fn scatter_pipeline(device: &ID3D12Device) -> Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
    unsafe {
        let blob = |b: &ID3DBlob| std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec();
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        D3DCompile(SCATTER.as_ptr().cast(), SCATTER.len(), PCSTR::null(), None, None, windows::core::s!("main"), windows::core::s!("cs_5_0"), 0, 0, &mut code, Some(&mut errors))
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
    fn page_of_finds_the_region_and_page_of_a_hash() {
        // two regions: 4 pages (256 hashes) then 2 pages
        let starts = [0, 4 * CHUNKS_PER_PAGE];
        assert_eq!(page_of(&starts, 0), Page { region: 0, offset: 0 });
        assert_eq!(page_of(&starts, 2 * CHUNKS_PER_PAGE + 5), Page { region: 0, offset: 2 * OS_PAGE as u32 });
        assert_eq!(page_of(&starts, 4 * CHUNKS_PER_PAGE - 1), Page { region: 0, offset: 3 * OS_PAGE as u32 });
        assert_eq!(page_of(&starts, 4 * CHUNKS_PER_PAGE), Page { region: 1, offset: 0 });
        assert_eq!(page_of(&starts, 5 * CHUNKS_PER_PAGE + 63), Page { region: 1, offset: OS_PAGE as u32 });
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
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
            let rb = buffer(device, D3D12_HEAP_TYPE_READBACK, len, D3D12_RESOURCE_STATE_COPY_DEST);
            list.CopyBufferRegion(&rb, 0, window, 0, len as u64);
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
        let mut ring = Ring::with_slot_bytes(&device, vec![va], vec![region.clone()], 2, 2 * OS_PAGE).unwrap();

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
}
