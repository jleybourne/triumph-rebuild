//! Pipeline creation off the title's threads.
//!
//! The device calls [`Pool::request`] when the title creates a pipeline: it returns a [`Handle`] at once and a
//! worker creates the pipeline. At the first draw that needs it, [`Pool::get`] (non-blocking) or
//! [`Pool::wait`] (blocking) gives the object. A `wait` on a pipeline no worker has started yet creates it on
//! the calling thread, so a draw never queues behind thousands of other creations.
//!
//! - Requests are deduplicated by [`Key`]: the same key gives the same handle and one creation.
//! - Three queues: pipelines a title thread is drawing with and found not ready ([`Pool::hurry`]), then the
//!   title's requests, then prewarm work ([`Pool::prewarm`]), which only uses idle workers.
//! - Handles index a slab whose segments never move, so `get` is lock-free and returns a plain reference.
//! - Every key the title requested is kept, for the next session's prewarm ([`Pool::requested`]).
//!
//! The pool is generic over a [`Creator`]: the host one (`d3d::PsoCreator`) creates `ID3D12PipelineState`s
//! through the pipeline library cache; tests use a fake.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::state::{Desc, Key};

/// What makes pipelines. Called from worker threads (and from a thread in [`Pool::wait`]), concurrently,
/// never twice for one key.
pub trait Creator: Send + Sync + 'static {
    type Out: Send + Sync + 'static;
    fn create(&self, key: &Key, desc: &Desc) -> Result<Self::Out, String>;
}

/// A pipeline the pool knows. Small and `Copy`; the device keeps it in its pipeline object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(u32);

impl Handle {
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Where a pipeline is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Creating,
    Ready,
    Failed,
}

const QUEUED: u8 = 0;
const CREATING: u8 = 1;
const READY: u8 = 2;
const FAILED: u8 = 3;

const SEGMENT: usize = 4096;
const SEGMENTS: usize = 256; // 1,048,576 pipelines

struct Slot<O> {
    state: AtomicU8,
    requested: AtomicBool,
    /// on no queue: created at its first wait
    parked: AtomicBool,
    /// on the urgent queue (a title thread binds it)
    hurried: AtomicBool,
    key: OnceLock<Key>,
    desc: Mutex<Option<Desc>>,
    out: OnceLock<Result<O, String>>,
}

impl<O> Default for Slot<O> {
    fn default() -> Self {
        Slot { state: AtomicU8::new(QUEUED), requested: AtomicBool::new(false), parked: AtomicBool::new(false), hurried: AtomicBool::new(false), key: OnceLock::new(), desc: Mutex::new(None), out: OnceLock::new() }
    }
}

#[derive(Default)]
struct Queues {
    urgent: VecDeque<u32>,
    title: VecDeque<u32>,
    prewarm: VecDeque<u32>,
    stop: bool,
}

/// Counters, for the bench and the frame line.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub requests: u64,
    /// Requests answered with an existing handle.
    pub deduplicated: u64,
    pub created: u64,
    pub failed: u64,
    /// Created on the thread that waited, because no worker had started it.
    pub inline: u64,
    /// Waits that found the pipeline not ready.
    pub waits_blocked: u64,
    /// Time spent blocked in `wait` (including inline creation).
    pub wait_time: Duration,
    /// Time workers spent creating.
    pub create_time: Duration,
    pub prewarm_queued: u64,
    /// Title requests past the eager cap: registered, created at their first use ([`Pool::request_capped`]).
    pub parked: u64,
    /// Parked pipelines created since (at a first use, or by a title request that found room).
    pub unparked: u64,
    /// Pipelines moved to the urgent queue ([`Pool::hurry`]).
    pub hurried: u64,
}

struct Inner<C: Creator> {
    creator: C,
    segments: Box<[OnceLock<Box<[Slot<C::Out>]>>]>,
    len: AtomicU32,
    keys: Mutex<HashMap<Key, u32>>,
    queues: Mutex<Queues>,
    work: Condvar,
    done: Mutex<()>,
    done_cv: Condvar,
    waiters: AtomicU32,
    requested: Mutex<Vec<(Key, Desc)>>,
    n: [AtomicU64; 11],
    wait_ns: AtomicU64,
    create_ns: AtomicU64,
}

const REQUESTS: usize = 0;
const DEDUP: usize = 1;
const CREATED: usize = 2;
const FAILED_N: usize = 3;
const INLINE: usize = 4;
const BLOCKED: usize = 5;
const PREWARM: usize = 6;
const PARKED: usize = 7;
const UNPARKED: usize = 8;
/// Title requests queued for the workers (not parked).
const TITLE_QUEUED: usize = 9;
const HURRIED: usize = 10;

/// Where a new slot goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Queue {
    Title,
    Prewarm,
    /// on no queue: created when first waited for
    Parked,
}

impl<C: Creator> Inner<C> {
    fn slot(&self, i: u32) -> &Slot<C::Out> {
        let (s, k) = (i as usize / SEGMENT, i as usize % SEGMENT);
        &self.segments[s].get().expect("handle from this pool")[k]
    }

    fn bump(&self, k: usize) {
        self.n[k].fetch_add(1, Ordering::Relaxed);
    }

    /// Creates slot `i`'s pipeline if nobody has started it. Returns whether this call did.
    fn run(&self, i: u32) -> bool {
        let s = self.slot(i);
        if s.state.compare_exchange(QUEUED, CREATING, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return false;
        }
        let desc = s.desc.lock().unwrap().take().expect("a queued slot has its description");
        let t = Instant::now();
        let r = self.creator.create(s.key.get().unwrap(), &desc);
        self.create_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let ok = r.is_ok();
        self.bump(if ok { CREATED } else { FAILED_N });
        let _ = s.out.set(r);
        s.state.store(if ok { READY } else { FAILED }, Ordering::Release);
        // pairs with the fence in `wait`: either the waiter sees the result, or this sees the waiter
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::SeqCst) > 0 {
            let _g = self.done.lock().unwrap();
            self.done_cv.notify_all();
        }
        true
    }

    fn worker(&self) {
        loop {
            let i = {
                let mut q = self.queues.lock().unwrap();
                loop {
                    if q.stop {
                        return;
                    }
                    if let Some(i) = q.urgent.pop_front().or_else(|| q.title.pop_front()).or_else(|| q.prewarm.pop_front()) {
                        break i;
                    }
                    q = self.work.wait(q).unwrap();
                }
            };
            self.run(i);
        }
    }
}

/// The pool: a creator and its worker threads. Dropping it finishes the work in flight, drops what is
/// still queued, and joins the workers.
pub struct Pool<C: Creator> {
    inner: Arc<Inner<C>>,
    workers: Vec<JoinHandle<()>>,
}

/// Worker threads for a machine: the logical processors (capped by `cpu_cores` when the launcher's limit
/// is set, `REBUILD_LIMIT_CPU_CORES`), less two for the title's own threads, at least 1, at most 12.
pub fn default_threads(cpu_cores: Option<u32>) -> usize {
    let n = std::thread::available_parallelism().map_or(4, |n| n.get());
    let n = match cpu_cores {
        Some(c) if c > 0 => n.min(c as usize),
        _ => n,
    };
    n.saturating_sub(2).clamp(1, 12)
}

/// The launcher's CPU core limit, read from the environment the way `rebuild_runtime::limits` reads it
/// (this crate cannot depend on the runtime: the runtime will depend on it).
pub fn cpu_core_limit() -> Option<u32> {
    std::env::var("REBUILD_LIMIT_CPU_CORES").ok()?.trim().parse().ok().filter(|n| *n > 0)
}

impl<C: Creator> Pool<C> {
    /// A pool with `threads` workers (see [`default_threads`]).
    pub fn new(creator: C, threads: usize) -> Pool<C> {
        let inner = Arc::new(Inner {
            creator,
            segments: (0..SEGMENTS).map(|_| OnceLock::new()).collect(),
            len: AtomicU32::new(0),
            keys: Mutex::new(HashMap::new()),
            queues: Mutex::new(Queues::default()),
            work: Condvar::new(),
            done: Mutex::new(()),
            done_cv: Condvar::new(),
            waiters: AtomicU32::new(0),
            requested: Mutex::new(Vec::new()),
            n: Default::default(),
            wait_ns: AtomicU64::new(0),
            create_ns: AtomicU64::new(0),
        });
        let workers = (0..threads.max(1))
            .map(|k| {
                let inner = inner.clone();
                std::thread::Builder::new().name(format!("pso-{k}")).spawn(move || inner.worker()).expect("spawn a pipeline worker")
            })
            .collect();
        Pool { inner, workers }
    }

    pub fn creator(&self) -> &C {
        &self.inner.creator
    }

    pub fn threads(&self) -> usize {
        self.workers.len()
    }

    /// Finds the key's slot or makes one (queued on `title` or prewarm). Returns the handle, whether it is
    /// new, and for a title request the description back (unused, or a copy) for the session record.
    fn enqueue(&self, key: Key, desc: Desc, queue: Queue) -> (Handle, bool, Option<Desc>) {
        let title = queue != Queue::Prewarm;
        let inner = &*self.inner;
        let (i, copy) = {
            let mut keys = inner.keys.lock().unwrap();
            if let Some(&i) = keys.get(&key) {
                drop(keys);
                if queue == Queue::Title {
                    // a prewarm entry the title now wants: move it ahead if no worker has it yet
                    let s = inner.slot(i);
                    if s.state.load(Ordering::Relaxed) == QUEUED && !s.requested.load(Ordering::Relaxed) {
                        let mut q = inner.queues.lock().unwrap();
                        q.title.push_back(i);
                        drop(q);
                        inner.work.notify_one();
                    }
                }
                return (Handle(i), false, Some(desc));
            }
            let i = inner.len.fetch_add(1, Ordering::Relaxed);
            assert!((i as usize) < SEGMENT * SEGMENTS, "pipeline slab full");
            let seg = inner.segments[i as usize / SEGMENT].get_or_init(|| (0..SEGMENT).map(|_| Slot::default()).collect());
            let s = &seg[i as usize % SEGMENT];
            let _ = s.key.set(key);
            let copy = title.then(|| desc.clone());
            *s.desc.lock().unwrap() = Some(desc);
            keys.insert(key, i);
            (i, copy)
        };
        if queue == Queue::Parked {
            inner.slot(i).parked.store(true, Ordering::Relaxed);
            inner.bump(PARKED);
            return (Handle(i), true, copy);
        }
        let mut q = inner.queues.lock().unwrap();
        if title {
            inner.bump(TITLE_QUEUED);
            q.title.push_back(i);
        } else {
            q.prewarm.push_back(i);
        }
        drop(q);
        inner.work.notify_one();
        (Handle(i), true, copy)
    }

    /// The title made a pipeline: returns at once. Equal keys give the same handle.
    pub fn request(&self, key: Key, desc: Desc) -> Handle {
        let inner = &*self.inner;
        inner.bump(REQUESTS);
        let (h, new, desc) = self.enqueue(key, desc, Queue::Title);
        if !new {
            inner.bump(DEDUP);
        }
        if !inner.slot(h.0).requested.swap(true, Ordering::Relaxed) {
            inner.requested.lock().unwrap().push((key, desc.expect("title requests return their description")));
        }
        h
    }

    /// [`Pool::request`] while fewer than `eager` title requests were queued for the workers; past that the
    /// pipeline is only registered (a handle, its description kept) and created at its first [`Pool::wait`].
    /// The title creates tens of thousands of pipelines and draws with a few thousand: creating every one
    /// ran the process out of memory (57,353 host PSOs on the card).
    pub fn request_capped(&self, key: Key, desc: Desc, eager: u64) -> Handle {
        let inner = &*self.inner;
        if inner.n[TITLE_QUEUED].load(Ordering::Relaxed) < eager {
            return self.request(key, desc);
        }
        inner.bump(REQUESTS);
        let (h, new, desc) = self.enqueue(key, desc, Queue::Parked);
        if !new {
            inner.bump(DEDUP);
        }
        if !inner.slot(h.0).requested.swap(true, Ordering::Relaxed) {
            inner.requested.lock().unwrap().push((key, desc.expect("title requests return their description")));
        }
        h
    }

    /// A pipeline the title may ask for (from the last session): created when workers are idle. Not recorded
    /// as requested unless the title asks for it.
    pub fn prewarm(&self, key: Key, desc: Desc) -> Handle {
        let (h, new, _) = self.enqueue(key, desc, Queue::Prewarm);
        if new {
            self.inner.bump(PREWARM);
        }
        h
    }

    /// The handle a key already has.
    pub fn find(&self, key: &Key) -> Option<Handle> {
        self.inner.keys.lock().unwrap().get(key).map(|&i| Handle(i))
    }

    pub fn status(&self, h: Handle) -> Status {
        match self.inner.slot(h.0).state.load(Ordering::Acquire) {
            QUEUED => Status::Queued,
            CREATING => Status::Creating,
            READY => Status::Ready,
            _ => Status::Failed,
        }
    }

    /// The pipeline if it is ready. Never blocks.
    pub fn get(&self, h: Handle) -> Option<&C::Out> {
        self.inner.slot(h.0).out.get()?.as_ref().ok()
    }

    /// The pipeline, blocking until it exists; created on this thread if no worker has started it.
    pub fn wait(&self, h: Handle) -> Result<&C::Out, &str> {
        let inner = &*self.inner;
        let s = inner.slot(h.0);
        if let Some(r) = s.out.get() {
            return r.as_ref().map_err(|e| e.as_str());
        }
        let t = Instant::now();
        inner.bump(BLOCKED);
        if inner.run(h.0) {
            inner.bump(INLINE);
            if s.parked.load(Ordering::Relaxed) && !s.hurried.load(Ordering::Relaxed) {
                inner.bump(UNPARKED);
            }
        } else {
            inner.waiters.fetch_add(1, Ordering::SeqCst);
            std::sync::atomic::fence(Ordering::SeqCst);
            let mut g = inner.done.lock().unwrap();
            while s.out.get().is_none() {
                g = inner.done_cv.wait_timeout(g, Duration::from_millis(2)).unwrap().0;
            }
            drop(g);
            inner.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        inner.wait_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        s.out.get().unwrap().as_ref().map_err(|e| e.as_str())
    }

    /// Blocks until every queued pipeline (title and prewarm) is created or failed.
    pub fn wait_all(&self) {
        // under the key lock: every slot counted has its segment
        let n = {
            let _keys = self.inner.keys.lock().unwrap();
            self.inner.len.load(Ordering::Acquire)
        };
        for i in 0..n {
            let _ = self.wait(Handle(i));
        }
    }

    /// Pipelines queued and not started.
    pub fn pending(&self) -> usize {
        let q = self.inner.queues.lock().unwrap();
        q.urgent.len() + q.title.len() + q.prewarm.len()
    }

    /// A title thread wants this pipeline now and will not wait for it: if no worker has started it (queued
    /// behind other work, or parked past the eager cap), it goes to the front, ahead of every other request.
    /// Never blocks on creation; returns the pipeline's status.
    pub fn hurry(&self, h: Handle) -> Status {
        let inner = &*self.inner;
        let s = inner.slot(h.0);
        if s.state.load(Ordering::Acquire) == QUEUED && !s.hurried.swap(true, Ordering::AcqRel) {
            if s.parked.load(Ordering::Relaxed) {
                inner.bump(UNPARKED);
            }
            inner.bump(HURRIED);
            inner.queues.lock().unwrap().urgent.push_back(h.0);
            inner.work.notify_one();
        }
        self.status(h)
    }

    /// Every pipeline the title requested this session, in first-request order, with its description: what
    /// [`crate::session::Record`] keeps for the next session's prewarm.
    pub fn requested(&self) -> Vec<(Key, Desc)> {
        self.inner.requested.lock().unwrap().clone()
    }

    pub fn stats(&self) -> Stats {
        let n = |k: usize| self.inner.n[k].load(Ordering::Relaxed);
        Stats {
            requests: n(REQUESTS),
            deduplicated: n(DEDUP),
            created: n(CREATED),
            failed: n(FAILED_N),
            inline: n(INLINE),
            waits_blocked: n(BLOCKED),
            wait_time: Duration::from_nanos(self.inner.wait_ns.load(Ordering::Relaxed)),
            create_time: Duration::from_nanos(self.inner.create_ns.load(Ordering::Relaxed)),
            prewarm_queued: n(PREWARM),
            parked: n(PARKED),
            unparked: n(UNPARKED),
            hurried: n(HURRIED),
        }
    }
}

impl<C: Creator> Drop for Pool<C> {
    fn drop(&mut self) {
        {
            let mut q = self.inner.queues.lock().unwrap();
            q.stop = true;
            q.urgent.clear();
            q.title.clear();
            q.prewarm.clear();
        }
        self.inner.work.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Creates a string after `delay`, counting calls per key and recording their order; fails for cs hash
    /// 0xbad; blocks while its cs hash is held.
    struct Fake {
        delay: Duration,
        calls: Mutex<HashMap<Key, usize>>,
        order: Mutex<Vec<u64>>,
        total: AtomicUsize,
        held: Mutex<std::collections::HashSet<u64>>,
        held_cv: Condvar,
    }

    impl Fake {
        fn new(ms: u64) -> Fake {
            Fake { delay: Duration::from_millis(ms), calls: Mutex::default(), order: Mutex::default(), total: AtomicUsize::new(0), held: Mutex::default(), held_cv: Condvar::new() }
        }
        fn hold(&self, cs: impl IntoIterator<Item = u64>) {
            self.held.lock().unwrap().extend(cs);
        }
        fn release(&self) {
            self.held.lock().unwrap().clear();
            self.held_cv.notify_all();
        }
    }

    impl Creator for Arc<Fake> {
        type Out = String;
        fn create(&self, key: &Key, desc: &Desc) -> Result<String, String> {
            let mut g = self.held.lock().unwrap();
            while g.contains(&key.shaders[0]) {
                g = self.held_cv.wait(g).unwrap();
            }
            drop(g);
            std::thread::sleep(self.delay);
            *self.calls.lock().unwrap().entry(*key).or_default() += 1;
            self.order.lock().unwrap().push(key.shaders[0]);
            self.total.fetch_add(1, Ordering::SeqCst);
            match desc {
                Desc::Compute { cs: 0xbad } => Err("refused".into()),
                _ => Ok(key.name()),
            }
        }
    }

    fn cs(h: u64) -> (Key, Desc) {
        let d = Desc::Compute { cs: h };
        (Key::of(&d), d)
    }

    /// Past the eager cap a request only registers: nothing is created until the first wait, which creates it
    /// on the waiting thread; a second request of a parked key gives the same handle.
    #[test]
    fn requests_past_the_eager_cap_are_created_at_first_use() {
        let f = Arc::new(Fake::new(0));
        let pool = Pool::new(f.clone(), 2);
        let hs: Vec<_> = (0..5u64).map(|k| pool.request_capped(cs(k).0, cs(k).1, 2)).collect();
        assert_eq!(pool.request_capped(cs(4).0, cs(4).1, 2), hs[4]);
        let _ = pool.wait(hs[0]);
        let _ = pool.wait(hs[1]);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(f.total.load(Ordering::SeqCst), 2, "only the eager two are created");
        assert_eq!(pool.status(hs[3]), Status::Queued);
        assert_eq!(pool.get(hs[3]), None);
        assert_eq!(pool.wait(hs[3]).unwrap(), &cs(3).0.name());
        let st = pool.stats();
        assert_eq!((st.parked, st.unparked, st.created, st.deduplicated), (3, 1, 3, 1));
        assert_eq!(f.total.load(Ordering::SeqCst), 3);
        assert_eq!(pool.requested().len(), 5, "every title request is recorded for the next session");
    }

    /// A pipeline a title thread binds before it is ready is hurried, never created on that thread: a parked one
    /// is made by a worker ahead of the title's queue, and hurrying twice queues it once.
    #[test]
    fn hurried_pipelines_jump_every_queue_and_never_block() {
        let f = Arc::new(Fake::new(0));
        f.hold([0]);
        let pool = Pool::new(f.clone(), 1);
        let first = pool.request(cs(0).0, cs(0).1);
        while pool.status(first) != Status::Creating {
            std::thread::yield_now();
        }
        let queued: Vec<Handle> = (1..10).map(|k| pool.request_capped(cs(k).0, cs(k).1, 10)).collect();
        let parked = pool.request_capped(cs(100).0, cs(100).1, 1);
        assert_eq!(pool.stats().parked, 1);
        let t = Instant::now();
        assert_eq!(pool.hurry(parked), Status::Queued);
        assert_eq!(pool.hurry(parked), Status::Queued);
        assert_eq!(pool.hurry(queued[8]), Status::Queued);
        assert!(t.elapsed() < Duration::from_millis(50), "hurry never creates on the calling thread");
        assert_eq!(pool.get(parked), None);
        f.release();
        let t = Instant::now();
        while f.total.load(Ordering::SeqCst) < 11 {
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(f.order.lock().unwrap()[..3], [0, 100, 9], "hurried ones go first, in order");
        let st = pool.stats();
        assert_eq!((st.hurried, st.unparked, st.inline, st.created), (2, 1, 0, 11));
        assert_eq!(pool.hurry(parked), Status::Ready);
    }

    #[test]
    fn identical_requests_are_created_once() {
        let f = Arc::new(Fake::new(1));
        let pool = Pool::new(f.clone(), 4);
        let mut hs = Vec::new();
        std::thread::scope(|s| {
            let ts: Vec<_> = (0..8)
                .map(|_| {
                    let pool = &pool;
                    s.spawn(move || (0..50u64).map(|k| pool.request(cs(k % 10).0, cs(k % 10).1)).collect::<Vec<_>>())
                })
                .collect();
            for t in ts {
                hs.push(t.join().unwrap());
            }
        });
        for v in &hs[1..] {
            assert_eq!(v, &hs[0], "every thread got the same handles");
        }
        pool.wait_all();
        assert_eq!(f.total.load(Ordering::SeqCst), 10);
        assert!(f.calls.lock().unwrap().values().all(|&n| n == 1));
        let st = pool.stats();
        assert_eq!((st.requests, st.deduplicated, st.created), (400, 390, 10));
        assert_eq!(pool.requested().len(), 10);
    }

    #[test]
    fn requests_return_at_once_and_complete_in_the_background() {
        let f = Arc::new(Fake::new(20));
        let pool = Pool::new(f.clone(), 2);
        let t = Instant::now();
        let hs: Vec<Handle> = (0..6).map(|k| pool.request(cs(k).0, cs(k).1)).collect();
        assert!(t.elapsed() < Duration::from_millis(15), "request does not block: {:?}", t.elapsed());
        assert!(hs.iter().any(|h| pool.get(*h).is_none()));
        // completion without anyone waiting
        let t = Instant::now();
        while hs.iter().any(|h| pool.status(*h) != Status::Ready) {
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        for (k, h) in hs.iter().enumerate() {
            assert_eq!(pool.get(*h).unwrap(), &cs(k as u64).0.name());
        }
        assert_eq!(pool.stats().inline, 0);
    }

    #[test]
    fn a_wait_creates_unstarted_work_itself() {
        let f = Arc::new(Fake::new(0));
        f.hold([0, 1]); // both workers block inside create on their first pipelines
        let pool = Pool::new(f.clone(), 2);
        let first: Vec<Handle> = (0..2).map(|k| pool.request(cs(k).0, cs(k).1)).collect();
        while first.iter().any(|h| pool.status(*h) != Status::Creating) {
            std::thread::yield_now();
        }
        let queued: Vec<Handle> = (2..100).map(|k| pool.request(cs(k).0, cs(k).1)).collect();
        let last = *queued.last().unwrap();
        assert_eq!(pool.status(last), Status::Queued);
        assert_eq!(pool.wait(last).unwrap(), &cs(99).0.name(), "created here while both workers are busy");
        assert_eq!(pool.stats().inline, 1);
        assert_eq!(pool.status(queued[0]), Status::Queued);
        f.release();
        pool.wait_all();
        assert_eq!(f.total.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn failures_are_reported_to_every_waiter() {
        let f = Arc::new(Fake::new(5));
        let pool = Pool::new(f, 2);
        let (k, d) = cs(0xbad);
        let h = pool.request(k, d.clone());
        assert_eq!(pool.wait(h), Err("refused"));
        assert_eq!(pool.request(k, d), h);
        assert_eq!(pool.wait(h), Err("refused"));
        assert!(pool.get(h).is_none());
        assert_eq!(pool.status(h), Status::Failed);
        assert_eq!(pool.stats().failed, 1);
    }

    #[test]
    fn prewarm_yields_to_the_title_and_is_recorded_only_when_requested() {
        let f = Arc::new(Fake::new(0));
        f.hold([0]);
        let pool = Pool::new(f.clone(), 1);
        let first = pool.prewarm(cs(0).0, cs(0).1);
        while pool.status(first) != Status::Creating {
            std::thread::yield_now();
        }
        let pre: Vec<Handle> = (1..20).map(|k| pool.prewarm(cs(k).0, cs(k).1)).collect();
        let title = pool.request(cs(1000).0, cs(1000).1);
        let again = pool.request(cs(19).0, cs(19).1);
        assert_eq!(again, pre[18], "a prewarmed key is the same pipeline");
        f.release();
        // let the one worker drain both queues (a wait here would create on this thread instead)
        let t = Instant::now();
        while f.total.load(Ordering::SeqCst) < 21 {
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(pool.get(title).is_some());
        assert_eq!(pool.requested(), vec![cs(1000), cs(19)]);
        assert_eq!(f.order.lock().unwrap()[..3], [0, 1000, 19], "the title's requests jump the prewarm queue");
        assert_eq!(pool.stats().prewarm_queued, 20);
        assert_eq!(f.total.load(Ordering::SeqCst), 21);
    }

    #[test]
    fn threads_follow_the_core_limit() {
        assert_eq!(default_threads(Some(1)), 1);
        assert_eq!(default_threads(Some(4)), 2.min(default_threads(None)));
        assert!(default_threads(None) >= 1 && default_threads(None) <= 12);
    }
}
