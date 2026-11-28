//! D2: the request engine. Queues, a worker pool, and in-order completion through status arrays.
//!
//! Every queue's submitted reads go to one pool of workers, which run them in parallel. Reads may
//! finish in any order; **completion is what is ordered**: a status entry is written once every
//! read enqueued before it on its queue has finished, and it carries the first failure among those
//! reads since the previous status entry (or `S_OK`). The title polls these entries.
//!
//! Scheduling: four priority classes (low, normal, high, real-time), each with a virtual clock
//! charged `bytes x weight` per read started (weights 1000, 100, 10, 1: about 10x per level); a
//! worker starts the next read of the class with the earliest clock, FIFO inside the class. Reads
//! that buffer their source (compressed, or into pages) hold their source size of the staging
//! budget (`SetStagingBufferSize`, 32 MiB by default) while they run.
//!
//! Queue behaviour the title depends on (from the fork's runs, read for design):
//! - a queue that fills half its capacity with unsubmitted commands submits itself;
//! - such an automatic submit leaves reads **into physical pages** (and every status entry behind
//!   one) for the title's own `Submit`, because the title writes those page lists right before
//!   calling `Submit`;
//! - `EnqueueRequest` blocks while the queue is full (one slot always stays free);
//! - `Close` drops what has not started and returns once the reads already running are done.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use rebuild_vfs::Vfs;

use crate::zlib::{inflate, InflateError};
use crate::{
    Hresult, Request, E_INVALIDARG, E_NOTIMPL, E_PENDING, END_OF_FILE, FILE_NOT_FOUND, INVALID_DATA, KNOWN_OPTIONS,
    MEMORY_SOURCE, PAGE, PAGE_DESTINATION, PATH_NOT_FOUND, S_OK, ZLIB,
};

/// The console's default staging buffer.
pub const DEFAULT_STAGING: u64 = 32 << 20;
const WEIGHT: [u64; 4] = [1000, 100, 10, 1];

/// Writes bytes into the title's 64 KiB physical pages (the lead binds it over `runtime::physical`).
pub trait Pages: Send + Sync {
    /// Writes `data` starting `offset` bytes into the first of `pages`; false if a page is unknown.
    fn write(&self, pages: &[u64], offset: usize, data: &[u8]) -> bool;
}

/// A BCPack decoder: [`crate::bcpack::Decoder`] (D5 in `agents/dstorage.md`), or none (requests refused).
pub trait BcPack: Send + Sync {
    /// Decodes BCPack data of BC format `bc` from `src` into `out` (whole BC blocks).
    fn decode(&self, bc: u8, src: &[u8], out: &mut [u8]) -> bool;
}

/// A status array the title polls.
pub struct StatusArray {
    entries: Box<[AtomicI32]>,
}

impl StatusArray {
    pub fn new(capacity: u32) -> Arc<Self> {
        Arc::new(Self { entries: (0..capacity).map(|_| AtomicI32::new(S_OK)).collect() })
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The entry's result: `E_PENDING`, the first failure, or `S_OK`; `E_INVALIDARG` out of range.
    pub fn result(&self, index: u32) -> Hresult {
        self.entries.get(index as usize).map_or(E_INVALIDARG, |e| e.load(Ordering::Acquire))
    }
    pub fn is_complete(&self, index: u32) -> bool {
        self.result(index) != E_PENDING
    }
}

#[derive(Clone)]
pub struct Config {
    /// Worker threads; 0 picks half the host's logical processors (at least 2).
    pub workers: usize,
    pub staging: u64,
    pub pages: Option<Arc<dyn Pages>>,
    pub bcpack: Option<Arc<dyn BcPack>>,
}

impl Default for Config {
    fn default() -> Self {
        Self { workers: 0, staging: DEFAULT_STAGING, pages: None, bcpack: None }
    }
}

/// Counters since the engine started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub reads: u64,
    pub failed: u64,
    /// Bytes written to destinations.
    pub bytes_out: u64,
    /// Bytes read from files and memory.
    pub bytes_in: u64,
    pub zlib_reads: u64,
    /// Requests with BCPack bits and no decoder (failed with `E_NOTIMPL`).
    pub bcpack_refused: u64,
    /// Reads that had to wait for staging room.
    pub staging_waits: u64,
    /// Time accounting, in nanoseconds summed over reads (timing only; no effect on behaviour).
    pub times: Times,
}

/// Where a read's time goes, summed over every finished read (ns), and status-entry latency.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Times {
    /// Enqueued until the queue was submitted (held reads wait for the title's `Submit`).
    pub unsubmitted_ns: u64,
    /// Submitted until a worker started it.
    pub ready_ns: u64,
    /// Reading the source (file or memory).
    pub read_ns: u64,
    /// Inflate and BCPack decode.
    pub decode_ns: u64,
    /// Writing into physical pages (frame lookups included).
    pub pages_ns: u64,
    /// Enqueued until finished, and the slowest one.
    pub latency_ns: u64,
    pub latency_max_ns: u64,
    /// Status entries written, the sum and maximum of enqueue-to-written.
    pub statuses: u64,
    pub status_ns: u64,
    pub status_max_ns: u64,
    /// Worker time, summed over workers: running reads, waiting for the engine's lock after one, and idle.
    pub worker_exec_ns: u64,
    pub worker_lock_ns: u64,
    pub worker_idle_ns: u64,
}

#[derive(Default)]
struct Counters {
    reads: AtomicU64,
    failed: AtomicU64,
    bytes_out: AtomicU64,
    bytes_in: AtomicU64,
    zlib_reads: AtomicU64,
    bcpack_refused: AtomicU64,
    staging_waits: AtomicU64,
    unsubmitted_ns: AtomicU64,
    ready_ns: AtomicU64,
    read_ns: AtomicU64,
    decode_ns: AtomicU64,
    pages_ns: AtomicU64,
    latency_ns: AtomicU64,
    latency_max_ns: AtomicU64,
    statuses: AtomicU64,
    status_ns: AtomicU64,
    status_max_ns: AtomicU64,
    worker_exec_ns: AtomicU64,
    worker_lock_ns: AtomicU64,
    worker_idle_ns: AtomicU64,
}

/// Nanoseconds since the first call (a cheap shared clock for the counters).
fn now_ns() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

/// A queue's id in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueId(u64);

/// Commands carry the time they were enqueued (`now_ns`), for the latency counters.
enum Command {
    Read(Request, u64),
    Status(Arc<StatusArray>, u32, u64),
}

impl Command {
    fn is_page_read(&self) -> bool {
        matches!(self, Command::Read(r, _) if r.options & PAGE_DESTINATION != 0)
    }
}

struct Queue {
    class: usize,
    capacity: usize,
    unsubmitted: Vec<Command>,
    /// Left by an automatic submit until the title's own `Submit`.
    held: Vec<Command>,
    /// Submitted reads not yet finished (waiting or running), by sequence number.
    pending: BTreeSet<u64>,
    running: usize,
    statuses: VecDeque<(u64, Arc<StatusArray>, u32, u64)>,
    failures: BTreeMap<u64, Hresult>,
}

impl Queue {
    fn entries(&self) -> usize {
        self.unsubmitted.len() + self.held.len() + self.pending.len() + self.statuses.len()
    }

    /// Writes every status entry whose earlier reads have all finished.
    fn flush(&mut self, c: &Counters) {
        while let Some(&(seq, _, _, _)) = self.statuses.front() {
            if self.pending.first().is_some_and(|&p| p < seq) {
                return;
            }
            let (_, array, index, enqueued) = self.statuses.pop_front().unwrap();
            let waited = now_ns().saturating_sub(enqueued);
            c.statuses.fetch_add(1, Ordering::Relaxed);
            c.status_ns.fetch_add(waited, Ordering::Relaxed);
            c.status_max_ns.fetch_max(waited, Ordering::Relaxed);
            let later = self.failures.split_off(&seq);
            let first = std::mem::replace(&mut self.failures, later).into_values().next();
            if let Some(e) = array.entries.get(index as usize) {
                e.store(first.unwrap_or(S_OK), Ordering::Release);
            }
        }
    }
}

struct Job {
    queue: u64,
    seq: u64,
    request: Request,
    enqueued: u64,
    submitted: u64,
}

// SAFETY: a request's pointers stay valid until the title sees it complete (the caller's promise in
// `Engine::enqueue_request`); nothing in it is tied to the thread that enqueued it.
unsafe impl Send for Job {}
unsafe impl Send for Command {}

#[derive(Default)]
struct State {
    queues: HashMap<u64, Queue>,
    next_queue: u64,
    next_seq: u64,
    ready: [VecDeque<Job>; 4],
    clock: [u64; 4],
    /// The clock of the class that started the last read.
    now: u64,
    staged: u64,
    staging: u64,
    shutdown: bool,
}

impl State {
    /// The next read to start, if the staging budget has room for it.
    fn take(&mut self, counters: &Counters) -> Option<(Job, u64)> {
        let class = (0..4).filter(|&c| !self.ready[c].is_empty()).min_by_key(|&c| (self.clock[c], 3 - c))?;
        let job = self.ready[class].front()?;
        let staging = staging_bytes(&job.request);
        if self.staged > 0 && self.staged + staging > self.staging {
            counters.staging_waits.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let job = self.ready[class].pop_front()?;
        let size = u64::from(job.request.destination_size.max(job.request.source_size)).max(1);
        self.now = self.clock[class];
        self.clock[class] += size * WEIGHT[class];
        self.staged += staging;
        if let Some(q) = self.queues.get_mut(&job.queue) {
            q.running += 1;
        }
        Some((job, staging))
    }

    fn submit(&mut self, id: u64, explicit: bool, c: &Counters) {
        let Some(q) = self.queues.get_mut(&id) else { return };
        let now = now_ns();
        let class = q.class;
        let mut commands = if explicit { std::mem::take(&mut q.held) } else { Vec::new() };
        commands.append(&mut q.unsubmitted);
        let mut reads = Vec::new();
        for command in commands {
            if !explicit && (command.is_page_read() || (matches!(command, Command::Status(..)) && !q.held.is_empty())) {
                q.held.push(command);
                continue;
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            match command {
                Command::Read(request, enqueued) => {
                    q.pending.insert(seq);
                    c.unsubmitted_ns.fetch_add(now.saturating_sub(enqueued), Ordering::Relaxed);
                    reads.push(Job { queue: id, seq, request, enqueued, submitted: now });
                }
                Command::Status(array, index, enqueued) => q.statuses.push_back((seq, array, index, enqueued)),
            }
        }
        q.flush(c);
        if reads.is_empty() {
            return;
        }
        // A class that had nothing waiting starts from now, not from credit it left behind.
        if self.ready[class].is_empty() {
            self.clock[class] = self.clock[class].max(self.now);
        }
        self.ready[class].extend(reads);
    }
}

/// Source bytes a read buffers while it runs (counted against the staging budget).
fn staging_bytes(r: &Request) -> u64 {
    let buffered = r.options & (ZLIB | crate::BCPACK_MASK | PAGE_DESTINATION) != 0;
    if buffered && r.options & MEMORY_SOURCE == 0 {
        u64::from(r.source_size)
    } else {
        0
    }
}

struct Shared {
    state: Mutex<State>,
    /// Reads to start, staging freed, or shutdown.
    work: Condvar,
    /// A read finished or a queue changed (for full queues and `close`).
    finished: Condvar,
    vfs: Arc<Vfs>,
    files: Mutex<HashMap<usize, rebuild_vfs::Handle>>,
    pages: Option<Arc<dyn Pages>>,
    bcpack: Option<Arc<dyn BcPack>>,
    counters: Counters,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The DirectStorage engine: owns its workers; no global state.
pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl Engine {
    pub fn new(vfs: Arc<Vfs>, config: Config) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { staging: config.staging.max(1), ..State::default() }),
            work: Condvar::new(),
            finished: Condvar::new(),
            vfs,
            files: Mutex::new(HashMap::new()),
            pages: config.pages,
            bcpack: config.bcpack,
            counters: Counters::default(),
        });
        let n = match config.workers {
            0 => std::thread::available_parallelism().map_or(4, |n| n.get() / 2).max(2),
            n => n,
        };
        let workers = (0..n)
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("dstorage {i}"))
                    .spawn(move || work(&shared))
                    .expect("spawn a dstorage worker")
            })
            .collect();
        Self { shared, workers }
    }

    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    pub fn vfs(&self) -> &Arc<Vfs> {
        &self.shared.vfs
    }

    /// Opens a console path for reading and registers it under `key` (the file object's address:
    /// requests name their file by it).
    pub fn open_file(&self, key: usize, path: &str) -> Result<(), Hresult> {
        // overlapped: the workers' reads of one file run side by side (a synchronous handle runs them one at a time).
        // REBUILD_DS_SYNC_READS=1 keeps the synchronous handle (for A/B measurements).
        let opened = if sync_reads() { self.shared.vfs.open(path, rebuild_vfs::OpenOptions::read()) } else { self.shared.vfs.open_concurrent(path) };
        let handle = opened.map_err(|e| {
            let hr = vfs_error(&e);
            if trace_on() {
                eprintln!("dstorage  open failed ({hr:#x}): {path}");
            }
            hr
        })?;
        let old = self.shared.files.lock().unwrap_or_else(|e| e.into_inner()).insert(key, handle);
        if let Some(old) = old {
            let _ = self.shared.vfs.close(old);
        }
        Ok(())
    }

    pub fn close_file(&self, key: usize) {
        let handle = self.shared.files.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Some(handle) = handle {
            let _ = self.shared.vfs.close(handle);
        }
    }

    pub fn set_staging(&self, bytes: u64) {
        self.shared.lock().staging = bytes.max(1);
        self.shared.work.notify_all();
    }

    pub fn create_queue(&self, capacity: u16, priority: i8) -> QueueId {
        let mut s = self.shared.lock();
        let id = s.next_queue;
        s.next_queue += 1;
        s.queues.insert(
            id,
            Queue {
                class: (priority.clamp(-1, 2) + 1) as usize,
                capacity: usize::from(capacity).max(2),
                unsubmitted: Vec::new(),
                held: Vec::new(),
                pending: BTreeSet::new(),
                running: 0,
                statuses: VecDeque::new(),
                failures: BTreeMap::new(),
            },
        );
        QueueId(id)
    }

    /// Adds a read to the queue.
    ///
    /// # Safety
    /// The request's destination (and its page list), and a memory source, must stay valid and
    /// untouched by anyone else until a status entry enqueued after it reports completion, or the
    /// queue is closed.
    pub unsafe fn enqueue_request(&self, queue: QueueId, request: Request) {
        // REBUILD_TRACE_FILES: the first 3,000 requests as the title made them
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if trace_on() && N.fetch_add(1, Ordering::Relaxed) < 3000 {
            eprintln!(
                "dsreq     file {:#x} offset {:#x} size {:#x} dest {:#x} options {:#x} intermediate {:#x}",
                request.source as usize, request.offset, request.source_size, request.destination_size, request.options, request.intermediate_size
            );
        }
        self.enqueue(queue, Command::Read(request, now_ns()));
    }

    /// Adds a status entry; it reads `E_PENDING` from now until the reads before it finish.
    pub fn enqueue_status(&self, queue: QueueId, array: &Arc<StatusArray>, index: u32) -> bool {
        let Some(entry) = array.entries.get(index as usize) else { return false };
        entry.store(E_PENDING, Ordering::Release);
        self.enqueue(queue, Command::Status(array.clone(), index, now_ns()));
        true
    }

    fn enqueue(&self, queue: QueueId, command: Command) {
        let mut s = self.shared.lock();
        loop {
            let Some(q) = s.queues.get(&queue.0) else { return };
            if q.entries() + 1 < q.capacity {
                break;
            }
            // Full with reads held for a Submit the title cannot make while it waits here.
            if !q.held.is_empty() {
                s.submit(queue.0, true, &self.shared.counters);
                self.shared.work.notify_all();
            }
            s = self.shared.finished.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        let q = s.queues.get_mut(&queue.0).unwrap();
        q.unsubmitted.push(command);
        if q.unsubmitted.len() >= q.capacity / 2 {
            s.submit(queue.0, false, &self.shared.counters);
            self.shared.work.notify_all();
        }
    }

    /// The title's `Submit`: everything enqueued (and held) goes to the workers.
    pub fn submit(&self, queue: QueueId) {
        self.shared.lock().submit(queue.0, true, &self.shared.counters);
        self.shared.work.notify_all();
    }

    /// Ends the queue: what has not started never completes; returns once its running reads finish.
    pub fn close(&self, queue: QueueId) {
        let mut s = self.shared.lock();
        let Some(q) = s.queues.get_mut(&queue.0) else { return };
        q.unsubmitted.clear();
        q.held.clear();
        q.statuses.clear();
        let class = q.class;
        s.ready[class].retain(|j| j.queue != queue.0);
        while s.queues.get(&queue.0).is_some_and(|q| q.running > 0) {
            s = self.shared.finished.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.queues.remove(&queue.0);
        drop(s);
        self.shared.finished.notify_all();
    }

    pub fn stats(&self) -> Stats {
        let c = &self.shared.counters;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Stats {
            reads: l(&c.reads),
            failed: l(&c.failed),
            bytes_out: l(&c.bytes_out),
            bytes_in: l(&c.bytes_in),
            zlib_reads: l(&c.zlib_reads),
            bcpack_refused: l(&c.bcpack_refused),
            staging_waits: l(&c.staging_waits),
            times: Times {
                unsubmitted_ns: l(&c.unsubmitted_ns),
                ready_ns: l(&c.ready_ns),
                read_ns: l(&c.read_ns),
                decode_ns: l(&c.decode_ns),
                pages_ns: l(&c.pages_ns),
                latency_ns: l(&c.latency_ns),
                latency_max_ns: l(&c.latency_max_ns),
                statuses: l(&c.statuses),
                status_ns: l(&c.status_ns),
                status_max_ns: l(&c.status_max_ns),
                worker_exec_ns: l(&c.worker_exec_ns),
                worker_lock_ns: l(&c.worker_lock_ns),
                worker_idle_ns: l(&c.worker_idle_ns),
            },
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.work.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        let files: Vec<_> = self.shared.files.lock().unwrap_or_else(|e| e.into_inner()).drain().collect();
        for (_, h) in files {
            let _ = self.shared.vfs.close(h);
        }
    }
}

fn vfs_error(e: &rebuild_vfs::Error) -> Hresult {
    use rebuild_vfs::Error::*;
    match e {
        NotFound => FILE_NOT_FOUND,
        PathNotFound | NoDrive(_) => PATH_NOT_FOUND,
        Io(io) => io.raw_os_error().map_or(E_INVALIDARG, |c| (0x8007_0000 | (c as u32 & 0xffff)) as i32),
        _ => E_INVALIDARG,
    }
}

fn work(shared: &Shared) {
    let mut s = shared.lock();
    loop {
        if s.shutdown {
            return;
        }
        let Some((job, staging)) = s.take(&shared.counters) else {
            let t = now_ns();
            s = shared.work.wait(s).unwrap_or_else(|e| e.into_inner());
            shared.counters.worker_idle_ns.fetch_add(now_ns() - t, Ordering::Relaxed);
            continue;
        };
        drop(s);
        let c = &shared.counters;
        let started = now_ns();
        c.ready_ns.fetch_add(started.saturating_sub(job.submitted), Ordering::Relaxed);
        let result = execute(shared, &job.request);
        let ended = now_ns();
        c.worker_exec_ns.fetch_add(ended - started, Ordering::Relaxed);
        let latency = ended.saturating_sub(job.enqueued);
        c.latency_ns.fetch_add(latency, Ordering::Relaxed);
        c.latency_max_ns.fetch_max(latency, Ordering::Relaxed);
        c.reads.fetch_add(1, Ordering::Relaxed);
        if result.is_err() {
            c.failed.fetch_add(1, Ordering::Relaxed);
        }
        s = shared.lock();
        c.worker_lock_ns.fetch_add(now_ns() - ended, Ordering::Relaxed);
        s.staged -= staging;
        if let Some(q) = s.queues.get_mut(&job.queue) {
            q.running -= 1;
            q.pending.remove(&job.seq);
            if let Err(hr) = result {
                q.failures.insert(job.seq, hr);
                trace("request failed", job.request.source as u64, hr);
            }
            q.flush(c);
        }
        if staging > 0 {
            shared.work.notify_all();
        }
        shared.finished.notify_all();
    }
}

fn sync_reads() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_DS_SYNC_READS").is_some_and(|v| v == "1"))
}

fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_TRACE_FILES").is_some())
}

/// `REBUILD_TRACE_FILES`: the first 100 failed requests on stderr.
fn trace(what: &str, file: u64, hr: Hresult) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if trace_on() && N.fetch_add(1, Ordering::Relaxed) < 100 {
        eprintln!("dstorage  {what} ({:#x}): file {file:#x}", hr as u32);
    }
}

thread_local! {
    static SOURCE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static OUT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs one read: source â†’ (inflate) â†’ destination.
fn execute(shared: &Shared, r: &Request) -> Result<(), Hresult> {
    if r.options & !KNOWN_OPTIONS != 0 {
        return Err(E_NOTIMPL);
    }
    if r.source.is_null() || r.destination.is_null() {
        return Err(E_INVALIDARG);
    }
    let bc = r.bcpack_format();
    let bcpack = match (bc, &shared.bcpack) {
        (0, _) => None,
        (_, Some(d)) => Some(d.clone()),
        (_, None) => {
            shared.counters.bcpack_refused.fetch_add(1, Ordering::Relaxed);
            return Err(E_NOTIMPL);
        }
    };
    let len = r.destination_size as usize;
    let to_pages = r.options & PAGE_DESTINATION != 0;
    let zlib = r.options & ZLIB != 0;
    if !zlib && bcpack.is_none() && r.source_size > r.destination_size {
        return Err(E_INVALIDARG);
    }
    let c = &shared.counters;

    // A plain read into an address goes straight from the file into the destination.
    if !zlib && bcpack.is_none() && !to_pages {
        let n = r.source_size as usize;
        // SAFETY: the caller of `enqueue_request` keeps `destination_size` bytes valid.
        let out = unsafe { std::slice::from_raw_parts_mut(r.destination.cast::<u8>(), n) };
        let t = now_ns();
        read_source(shared, r, out)?;
        c.read_ns.fetch_add(now_ns() - t, Ordering::Relaxed);
        c.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        c.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
        return Ok(());
    }

    SOURCE.with_borrow_mut(|src| {
        // Memory sources are read where they are; files into a per-worker buffer.
        let source: &[u8] = if r.options & MEMORY_SOURCE != 0 {
            // SAFETY: the caller keeps `source_size` bytes at the source valid.
            unsafe { std::slice::from_raw_parts(r.source.cast::<u8>(), r.source_size as usize) }
        } else {
            src.resize(r.source_size as usize, 0);
            let t = now_ns();
            read_source(shared, r, src)?;
            c.read_ns.fetch_add(now_ns() - t, Ordering::Relaxed);
            src
        };
        c.bytes_in.fetch_add(source.len() as u64, Ordering::Relaxed);
        if zlib {
            c.zlib_reads.fetch_add(1, Ordering::Relaxed);
        }
        // zlib first, then BCPack (the console's order), through an intermediate buffer.
        let produce = |out: &mut [u8]| -> Result<usize, Hresult> {
            match (&bcpack, zlib) {
                (None, true) => inflate(source, out).map_err(zlib_error),
                (None, false) => {
                    out[..source.len()].copy_from_slice(source);
                    Ok(source.len())
                }
                (Some(d), z) => {
                    let mut packed = Vec::new();
                    let packed: &[u8] = if z {
                        packed.resize(r.intermediate_size as usize, 0);
                        let n = inflate(source, &mut packed).map_err(zlib_error)?;
                        &packed[..n]
                    } else {
                        source
                    };
                    if d.decode(bc, packed, out) {
                        Ok(out.len())
                    } else {
                        Err(INVALID_DATA)
                    }
                }
            }
        };
        let written = if to_pages {
            let pages = shared.pages.as_ref().ok_or(E_NOTIMPL)?;
            let offset = (r.page_offset & 0xffff) as usize;
            // SAFETY: the title passes one page number per 64 KiB the destination touches.
            let list =
                unsafe { std::slice::from_raw_parts(r.destination.cast::<u64>(), (offset + len).div_ceil(PAGE)) };
            OUT.with_borrow_mut(|out| {
                out.resize(len, 0);
                let t = now_ns();
                let n = produce(out)?;
                let t2 = now_ns();
                c.decode_ns.fetch_add(t2 - t, Ordering::Relaxed);
                let ok = pages.write(list, offset, &out[..n]);
                c.pages_ns.fetch_add(now_ns() - t2, Ordering::Relaxed);
                if ok {
                    Ok(n)
                } else {
                    Err(E_INVALIDARG)
                }
            })?
        } else {
            // SAFETY: as above, `destination_size` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(r.destination.cast::<u8>(), len) };
            let t = now_ns();
            let n = produce(out)?;
            c.decode_ns.fetch_add(now_ns() - t, Ordering::Relaxed);
            n
        };
        c.bytes_out.fetch_add(written as u64, Ordering::Relaxed);
        Ok(())
    })
}

fn zlib_error(e: InflateError) -> Hresult {
    match e {
        InflateError::Corrupt | InflateError::Short => INVALID_DATA,
    }
}

/// Fills `out` from the request's file at its offset, or from its memory source.
fn read_source(shared: &Shared, r: &Request, out: &mut [u8]) -> Result<(), Hresult> {
    if r.options & MEMORY_SOURCE != 0 {
        // SAFETY: the caller keeps `source_size` bytes at the source valid.
        out.copy_from_slice(unsafe { std::slice::from_raw_parts(r.source.cast::<u8>(), out.len()) });
        return Ok(());
    }
    let handle = *shared.files.lock().unwrap_or_else(|e| e.into_inner()).get(&(r.source as usize)).ok_or(E_INVALIDARG)?;
    let n = shared.vfs.read_at(handle, r.offset, out).map_err(|e| vfs_error(&e))?;
    if n < out.len() {
        return Err(END_OF_FILE);
    }
    Ok(())
}
