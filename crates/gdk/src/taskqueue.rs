//! XTaskQueue: callback queues with a work port and a completion port.
//!
//! Each port has a dispatch mode (`XTaskQueueDispatchMode`): `Manual` (callbacks wait until the title
//! calls `XTaskQueueDispatch`), `ThreadPool` (run on our pool), `SerializedThreadPool` (one at a time,
//! in order, on a thread of the port's own) or `Immediate` (on the submitting thread, inside the
//! submit call). A composite queue shares ports with another. A null queue handle means the process
//! queue (thread pool / thread pool until the title replaces or clears it).
//!
//! Termination: no new callbacks are accepted (`E_ABORT`); every callback still pending, including
//! delayed ones, is delivered through its port with `canceled = true` (so a manual port still needs
//! dispatching); once the queue has nothing pending, the terminated callback runs, on the thread that
//! finished the last one (or the terminating thread if nothing was pending). `wait` blocks until the
//! terminated callback has returned, as documented.
//!
//! What blocks: `XTaskQueueDispatch` up to its timeout, `XTaskQueueTerminate` with `wait`. Nothing
//! else waits on title code: submits take a lock for a push, delayed callbacks go to one timer
//! thread, the pool grows a thread when every worker is busy rather than queueing behind them.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::hr::*;

pub type Callback = unsafe extern "system" fn(*mut c_void, bool);
pub type MonitorCallback = unsafe extern "system" fn(*mut c_void, Handle, u32);
pub type TerminatedCallback = unsafe extern "system" fn(*mut c_void);

/// `XTaskQueueHandle`: `Arc::into_raw` of the queue; duplicate and close add and drop a reference.
pub type Handle = *const Queue;
/// `XTaskQueuePortHandle`: the port's address; its queues keep it alive.
pub type PortHandle = *const Port;

pub const WORK: u32 = 0;
pub const COMPLETION: u32 = 1;
pub const INFINITE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Manual = 0,
    ThreadPool = 1,
    SerializedThreadPool = 2,
    Immediate = 3,
}

impl Mode {
    pub fn from_raw(v: u32) -> Option<Mode> {
        [Mode::Manual, Mode::ThreadPool, Mode::SerializedThreadPool, Mode::Immediate]
            .get(v as usize)
            .copied()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One submitted callback. `canceled` is decided when it runs: its queue was terminated first.
struct Item {
    queue: Arc<Queue>,
    context: usize,
    callback: Callback,
}

impl Item {
    fn run(self) {
        let canceled = self.queue.terminated.load(Ordering::Acquire);
        // SAFETY: the title's callback with the title's context, as submitted.
        unsafe { (self.callback)(self.context as *mut c_void, canceled) };
        self.queue.one_done();
    }
}

pub struct Port {
    mode: Mode,
    items: Mutex<VecDeque<Item>>,
    ready: Condvar,
    worker: AtomicBool,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        Arc::new(Port { mode, items: Mutex::default(), ready: Condvar::new(), worker: AtomicBool::new(false) })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn push(self: &Arc<Self>, item: Item) {
        match self.mode {
            Mode::Immediate => item.run(),
            Mode::ThreadPool => pool().push(item),
            Mode::Manual => {
                lock(&self.items).push_back(item);
                // Dispatchers of several queues may share the port; each checks its own.
                self.ready.notify_all();
            }
            Mode::SerializedThreadPool => {
                lock(&self.items).push_back(item);
                self.ready.notify_one();
                if !self.worker.swap(true, Ordering::AcqRel) {
                    let port = self.clone();
                    std::thread::Builder::new()
                        .name("gdk serialized port".into())
                        .spawn(move || port.serial())
                        .expect("spawn the serialized port's thread");
                }
            }
        }
    }

    /// The serialized port's thread: one callback at a time, in order. It ends once only it holds
    /// the port and nothing is queued.
    fn serial(self: Arc<Self>) {
        loop {
            let item = {
                let items = lock(&self.items);
                let (mut items, _) = self
                    .ready
                    .wait_timeout_while(items, Duration::from_millis(100), |i| i.is_empty())
                    .unwrap_or_else(|e| e.into_inner());
                items.pop_front()
            };
            match item {
                Some(item) => item.run(),
                None if Arc::strong_count(&self) == 1 => {
                    self.worker.store(false, Ordering::Release);
                    return;
                }
                None => {}
            }
        }
    }

    fn wake(&self) {
        let _items = lock(&self.items);
        self.ready.notify_all();
    }
}

/// The thread pool behind thread-pool ports. A new worker starts whenever more callbacks wait than
/// workers are idle, so a callback that waits for another never starves it; idle workers beyond
/// the core count leave after 10 s.
struct Pool {
    state: Mutex<PoolState>,
    ready: Condvar,
}

#[derive(Default)]
struct PoolState {
    items: VecDeque<Item>,
    idle: usize,
    workers: usize,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool { state: Mutex::default(), ready: Condvar::new() })
}

impl Pool {
    fn push(&'static self, item: Item) {
        let mut s = lock(&self.state);
        s.items.push_back(item);
        if s.items.len() > s.idle {
            s.workers += 1;
            drop(s);
            std::thread::Builder::new()
                .name("gdk pool".into())
                .spawn(move || self.work())
                .expect("spawn a pool thread");
        } else {
            self.ready.notify_one();
        }
    }

    fn work(&'static self) {
        let keep = std::thread::available_parallelism().map_or(4, |n| n.get());
        let mut s = lock(&self.state);
        loop {
            if let Some(item) = s.items.pop_front() {
                drop(s);
                item.run();
                s = lock(&self.state);
                continue;
            }
            s.idle += 1;
            let (next, timeout) = self
                .ready
                .wait_timeout(s, Duration::from_secs(10))
                .unwrap_or_else(|e| e.into_inner());
            s = next;
            s.idle -= 1;
            if timeout.timed_out() && s.items.is_empty() && s.workers > keep {
                s.workers -= 1;
                return;
            }
        }
    }
}

/// Delayed callbacks: one thread, a heap by due time.
struct Timer {
    heap: Mutex<BinaryHeap<Reverse<Delayed>>>,
    ready: Condvar,
}

struct Delayed {
    due: Instant,
    seq: u64,
    port: u32,
    item: Item,
}

impl PartialEq for Delayed {
    fn eq(&self, o: &Self) -> bool {
        (self.due, self.seq) == (o.due, o.seq)
    }
}
impl Eq for Delayed {}
impl PartialOrd for Delayed {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Delayed {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.due, self.seq).cmp(&(o.due, o.seq))
    }
}

fn timer() -> &'static Timer {
    static TIMER: OnceLock<Timer> = OnceLock::new();
    TIMER.get_or_init(|| {
        std::thread::Builder::new()
            .name("gdk timer".into())
            .spawn(|| timer().run())
            .expect("spawn the timer thread");
        Timer { heap: Mutex::default(), ready: Condvar::new() }
    })
}

impl Timer {
    fn add(&self, delay_ms: u32, port: u32, item: Item) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let due = Instant::now() + Duration::from_millis(delay_ms.into());
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        lock(&self.heap).push(Reverse(Delayed { due, seq, port, item }));
        self.ready.notify_one();
    }

    fn run(&self) {
        let mut heap = lock(&self.heap);
        loop {
            let now = Instant::now();
            match heap.peek().map(|d| d.0.due) {
                Some(due) if due <= now => {
                    let d = heap.pop().expect("peeked").0;
                    drop(heap);
                    let queue = d.item.queue.clone();
                    queue.deliver(d.port, d.item);
                    heap = lock(&self.heap);
                }
                Some(due) => {
                    heap = self.ready.wait_timeout(heap, due - now).unwrap_or_else(|e| e.into_inner()).0;
                }
                None => heap = self.ready.wait(heap).unwrap_or_else(|e| e.into_inner()),
            }
        }
    }

    /// Takes `queue`'s delayed callbacks out, to be delivered now.
    fn take(&self, queue: &Arc<Queue>) -> Vec<Delayed> {
        let mut heap = lock(&self.heap);
        let (mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *heap).into_iter().partition(|d| Arc::ptr_eq(&d.0.item.queue, queue));
        *heap = rest.into_iter().collect();
        let mut mine: Vec<Delayed> = mine.into_iter().map(|d| d.0).collect();
        mine.sort();
        mine
    }
}

pub struct Queue {
    ports: [Arc<Port>; 2],
    terminated: AtomicBool,
    /// Callbacks submitted through this queue that have not run yet.
    pending: AtomicUsize,
    monitors: Mutex<Vec<(u64, usize, MonitorCallback)>>,
    termination: Mutex<Termination>,
    terminated_cv: Condvar,
}

#[derive(Default)]
struct Termination {
    callback: Option<(usize, TerminatedCallback)>,
    requested: bool,
    started: bool,
    done: bool,
}

impl Queue {
    fn new(ports: [Arc<Port>; 2]) -> Arc<Queue> {
        Arc::new(Queue {
            ports,
            terminated: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            monitors: Mutex::default(),
            termination: Mutex::default(),
            terminated_cv: Condvar::new(),
        })
    }

    pub fn port(&self, port: u32) -> Option<&Arc<Port>> {
        self.ports.get(port as usize)
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    pub fn submit(self: &Arc<Self>, port: u32, delay_ms: u32, context: *mut c_void, callback: Callback) -> HRESULT {
        if port > COMPLETION {
            return E_INVALIDARG;
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.is_terminated() {
            self.one_done();
            return E_ABORT;
        }
        let item = Item { queue: self.clone(), context: context as usize, callback };
        if delay_ms == 0 {
            self.deliver(port, item);
        } else {
            timer().add(delay_ms, port, item);
        }
        S_OK
    }

    fn deliver(self: &Arc<Self>, port: u32, item: Item) {
        self.ports[port as usize].push(item);
        let monitors = lock(&self.monitors).clone();
        for (_, context, callback) in monitors {
            // SAFETY: the title's monitor, registered with this context.
            unsafe { callback(context as *mut c_void, Arc::as_ptr(self), port) };
        }
    }

    fn one_done(&self) {
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 && self.is_terminated() {
            self.finish_termination();
        }
    }

    /// Runs the terminated callback once, when termination was asked for and nothing is pending.
    fn finish_termination(&self) {
        let callback = {
            let mut t = lock(&self.termination);
            if !t.requested || t.started || self.pending.load(Ordering::Acquire) != 0 {
                return;
            }
            t.started = true;
            t.callback.take()
        };
        if let Some((context, callback)) = callback {
            // SAFETY: the title's terminated callback with its context.
            unsafe { callback(context as *mut c_void) };
        }
        lock(&self.termination).done = true;
        self.terminated_cv.notify_all();
    }

    pub fn terminate(self: &Arc<Self>, wait: bool, context: *mut c_void, callback: Option<TerminatedCallback>) -> HRESULT {
        {
            let mut t = lock(&self.termination);
            if t.requested {
                return E_ABORT;
            }
            t.requested = true;
            t.callback = callback.map(|c| (context as usize, c));
        }
        self.terminated.store(true, Ordering::Release);
        for d in timer().take(self) {
            self.deliver(d.port, d.item);
        }
        for port in &self.ports {
            port.wake();
        }
        self.finish_termination();
        if wait {
            let t = lock(&self.termination);
            drop(self.terminated_cv.wait_while(t, |t| !t.done).unwrap_or_else(|e| e.into_inner()));
        }
        S_OK
    }

    /// Runs one callback of a manual port. False when none came within `timeout_ms`, the port is not
    /// manual, or this queue is terminated and its port is empty (a dispatcher's loop ends).
    pub fn dispatch(self: &Arc<Self>, port: u32, timeout_ms: u32) -> bool {
        let Some(p) = self.port(port) else { return false };
        if p.mode != Mode::Manual {
            return false;
        }
        let deadline = (timeout_ms != INFINITE).then(|| Instant::now() + Duration::from_millis(timeout_ms.into()));
        let mut items = lock(&p.items);
        loop {
            if let Some(item) = items.pop_front() {
                drop(items);
                item.run();
                return true;
            }
            if self.is_terminated() {
                return false;
            }
            match deadline {
                None => items = p.ready.wait(items).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return false;
                    }
                    items = p.ready.wait_timeout(items, d - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }

    pub fn register_monitor(&self, context: *mut c_void, callback: MonitorCallback) -> u64 {
        let token = crate::object::token();
        lock(&self.monitors).push((token, context as usize, callback));
        token
    }

    pub fn unregister_monitor(&self, token: u64) {
        lock(&self.monitors).retain(|m| m.0 != token);
    }
}

enum Process {
    Unset,
    Cleared,
    Set(Arc<Queue>),
}

static PROCESS: Mutex<Process> = Mutex::new(Process::Unset);

fn process_queue() -> Option<Arc<Queue>> {
    let mut p = lock(&PROCESS);
    if let Process::Unset = *p {
        *p = Process::Set(Queue::new([Port::new(Mode::ThreadPool), Port::new(Mode::ThreadPool)]));
    }
    match &*p {
        Process::Set(q) => Some(q.clone()),
        _ => None,
    }
}

/// A new reference to the queue behind `handle` (null: the process queue).
pub fn queue(handle: Handle) -> Option<Arc<Queue>> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a handle we returned that the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Some(Arc::from_raw(handle))
    }
}

/// Hands a new reference to the title.
fn into_handle(q: Arc<Queue>) -> Handle {
    Arc::into_raw(q)
}

// The public API, in its argument order.

pub unsafe fn create(work: u32, completion: u32, out: *mut Handle) -> HRESULT {
    let (Some(w), Some(c)) = (Mode::from_raw(work), Mode::from_raw(completion)) else { return E_INVALIDARG };
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { out.write(into_handle(Queue::new([Port::new(w), Port::new(c)]))) };
    S_OK
}

pub unsafe fn get_port(handle: Handle, port: u32, out: *mut PortHandle) -> HRESULT {
    let Some(q) = queue(handle) else { return E_INVALIDARG };
    match q.port(port) {
        Some(p) if !out.is_null() => {
            unsafe { out.write(Arc::as_ptr(p)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub unsafe fn create_composite(work: PortHandle, completion: PortHandle, out: *mut Handle) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let port = |p: PortHandle| {
        // SAFETY: a port handle from `get_port` whose queue is still open.
        unsafe {
            Arc::increment_strong_count(p);
            Arc::from_raw(p)
        }
    };
    unsafe { out.write(into_handle(Queue::new([port(work), port(completion)]))) };
    S_OK
}

pub unsafe fn duplicate(handle: Handle, out: *mut Handle) -> HRESULT {
    match queue(handle) {
        Some(q) if !out.is_null() => {
            unsafe { out.write(into_handle(q)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub unsafe fn close(handle: Handle) {
    if !handle.is_null() {
        // SAFETY: drops the title's reference.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

pub unsafe fn dispatch(handle: Handle, port: u32, timeout_ms: u32) -> bool {
    queue(handle).is_some_and(|q| q.dispatch(port, timeout_ms))
}

pub unsafe fn submit_delayed(handle: Handle, port: u32, delay_ms: u32, context: *mut c_void, callback: Option<Callback>) -> HRESULT {
    match (queue(handle), callback) {
        (Some(q), Some(cb)) => q.submit(port, delay_ms, context, cb),
        _ => E_INVALIDARG,
    }
}

pub unsafe fn terminate(handle: Handle, wait: bool, context: *mut c_void, callback: Option<TerminatedCallback>) -> HRESULT {
    match queue(handle) {
        Some(q) => q.terminate(wait, context, callback),
        None => E_INVALIDARG,
    }
}

pub unsafe fn register_monitor(handle: Handle, context: *mut c_void, callback: Option<MonitorCallback>, token: *mut u64) -> HRESULT {
    let (Some(q), Some(cb)) = (queue(handle), callback) else { return E_INVALIDARG };
    if token.is_null() {
        return E_INVALIDARG;
    }
    unsafe { token.write_unaligned(q.register_monitor(context, cb)) };
    S_OK
}

pub unsafe fn unregister_monitor(handle: Handle, token: u64) {
    if let Some(q) = queue(handle) {
        q.unregister_monitor(token);
    }
}

/// A new handle to the process queue; false (and a null handle) if the title cleared it.
pub unsafe fn get_process_queue(out: *mut Handle) -> bool {
    let q = process_queue();
    let found = q.is_some();
    if !out.is_null() {
        unsafe { out.write(q.map_or(std::ptr::null(), into_handle)) };
    }
    found
}

pub unsafe fn set_process_queue(handle: Handle) {
    let new = if handle.is_null() { Process::Cleared } else { queue(handle).map_or(Process::Cleared, Process::Set) };
    *lock(&PROCESS) = new;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn make(work: Mode, completion: Mode) -> Handle {
        let mut q = std::ptr::null();
        assert_eq!(unsafe { create(work as u32, completion as u32, &mut q) }, S_OK);
        q
    }

    /// Records (context, canceled, thread) of every call, per test, through a channel boxed as the
    /// context's owner.
    struct Log(Mutex<Vec<(usize, bool, std::thread::ThreadId)>>);
    unsafe extern "system" fn record(context: *mut c_void, canceled: bool) {
        let (log, tag) = unsafe { &*(context as *const (Arc<Log>, usize)) };
        lock(&log.0).push((*tag, canceled, std::thread::current().id()));
    }
    fn ctx(log: &Arc<Log>, tag: usize) -> *mut c_void {
        Box::into_raw(Box::new((log.clone(), tag))).cast()
    }
    fn new_log() -> Arc<Log> {
        Arc::new(Log(Mutex::default()))
    }

    #[test]
    fn manual_ports_run_in_order_only_when_dispatched() {
        let q = make(Mode::Manual, Mode::Manual);
        let log = new_log();
        for tag in 0..5 {
            assert_eq!(unsafe { submit_delayed(q, WORK, 0, ctx(&log, tag), Some(record)) }, S_OK);
        }
        assert!(lock(&log.0).is_empty(), "nothing runs before dispatch");
        assert!(!unsafe { dispatch(q, COMPLETION, 0) }, "the completion port is empty");
        while unsafe { dispatch(q, WORK, 0) } {}
        let tags: Vec<usize> = lock(&log.0).iter().map(|e| e.0).collect();
        assert_eq!(tags, [0, 1, 2, 3, 4]);
        assert!(lock(&log.0).iter().all(|e| !e.1 && e.2 == std::thread::current().id()));
        unsafe { close(q) };
    }

    #[test]
    fn serialized_runs_in_order_on_one_thread() {
        let q = make(Mode::SerializedThreadPool, Mode::Immediate);
        let log = new_log();
        for tag in 0..200 {
            assert_eq!(unsafe { submit_delayed(q, WORK, 0, ctx(&log, tag), Some(record)) }, S_OK);
        }
        let start = Instant::now();
        while lock(&log.0).len() < 200 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let log = lock(&log.0);
        assert_eq!(log.iter().map(|e| e.0).collect::<Vec<_>>(), (0..200).collect::<Vec<_>>());
        assert!(log.iter().all(|e| e.2 == log[0].2) && log[0].2 != std::thread::current().id());
        unsafe { close(q) };
    }

    #[test]
    fn immediate_runs_inside_submit() {
        let q = make(Mode::Immediate, Mode::Immediate);
        let log = new_log();
        unsafe { submit_delayed(q, COMPLETION, 0, ctx(&log, 7), Some(record)) };
        assert_eq!(lock(&log.0)[0].0, 7);
        assert_eq!(lock(&log.0)[0].2, std::thread::current().id());
        unsafe { close(q) };
    }

    #[test]
    fn delayed_callbacks_come_in_due_order() {
        let q = make(Mode::Manual, Mode::Manual);
        let log = new_log();
        let start = Instant::now();
        unsafe { submit_delayed(q, WORK, 120, ctx(&log, 2), Some(record)) };
        unsafe { submit_delayed(q, WORK, 40, ctx(&log, 1), Some(record)) };
        assert!(start.elapsed() < Duration::from_millis(35), "submitting a delay does not wait for it");
        assert!(!unsafe { dispatch(q, WORK, 0) }, "not due yet");
        assert!(unsafe { dispatch(q, WORK, 5000) });
        assert!(unsafe { dispatch(q, WORK, 5000) });
        assert!(start.elapsed() >= Duration::from_millis(120));
        assert_eq!(lock(&log.0).iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2]);
        unsafe { close(q) };
    }

    #[test]
    fn pool_callbacks_may_wait_for_each_other() {
        let q = make(Mode::ThreadPool, Mode::ThreadPool);
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        unsafe extern "system" fn waits(c: *mut c_void, _: bool) {
            let (rx, done) = unsafe { &*(c as *const (Mutex<mpsc::Receiver<()>>, mpsc::Sender<()>)) };
            lock(rx).recv_timeout(Duration::from_secs(5)).expect("the second callback ran");
            done.send(()).unwrap();
        }
        unsafe extern "system" fn releases(c: *mut c_void, _: bool) {
            unsafe { &*(c as *const mpsc::Sender<()>) }.send(()).unwrap();
        }
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let a = Box::leak(Box::new((rx, done_tx)));
        let b = Box::leak(Box::new(tx));
        unsafe { submit_delayed(q, WORK, 0, a as *mut _ as *mut c_void, Some(waits)) };
        unsafe { submit_delayed(q, WORK, 0, b as *mut _ as *mut c_void, Some(releases)) };
        done_rx.recv_timeout(Duration::from_secs(10)).expect("no starvation");
        unsafe { close(q) };
    }

    #[test]
    fn terminate_cancels_pending_and_then_calls_back() {
        let q = make(Mode::Manual, Mode::Manual);
        let log = new_log();
        unsafe { submit_delayed(q, WORK, 0, ctx(&log, 1), Some(record)) };
        unsafe { submit_delayed(q, COMPLETION, 0, ctx(&log, 2), Some(record)) };
        unsafe { submit_delayed(q, WORK, 60_000, ctx(&log, 3), Some(record)) };
        static TERMINATED: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn terminated(_: *mut c_void) {
            TERMINATED.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(unsafe { terminate(q, false, std::ptr::null_mut(), Some(terminated)) }, S_OK);
        assert_eq!(TERMINATED.load(Ordering::SeqCst), 0, "three callbacks still to deliver");
        assert_eq!(
            unsafe { submit_delayed(q, WORK, 0, ctx(&log, 9), Some(record)) },
            E_ABORT,
            "nothing new after terminate"
        );
        assert_eq!(unsafe { terminate(q, false, std::ptr::null_mut(), None) }, E_ABORT, "once");
        while unsafe { dispatch(q, WORK, 0) } {}
        assert!(unsafe { dispatch(q, COMPLETION, 0) });
        assert!(!unsafe { dispatch(q, COMPLETION, INFINITE) }, "terminated and empty: returns at once");
        let log = lock(&log.0);
        let mut tags: Vec<(usize, bool)> = log.iter().map(|e| (e.0, e.1)).collect();
        tags.sort();
        assert_eq!(tags, [(1, true), (2, true), (3, true)], "the delayed one too, at once");
        assert_eq!(TERMINATED.load(Ordering::SeqCst), 1);
        unsafe { close(q) };
    }

    #[test]
    fn terminate_with_wait_returns_after_the_callback() {
        let q = make(Mode::ThreadPool, Mode::ThreadPool);
        static SLOW_DONE: AtomicBool = AtomicBool::new(false);
        unsafe extern "system" fn slow(_: *mut c_void, _: bool) {
            std::thread::sleep(Duration::from_millis(50));
            SLOW_DONE.store(true, Ordering::SeqCst);
        }
        unsafe { submit_delayed(q, WORK, 0, std::ptr::null_mut(), Some(slow)) };
        static CALLED: AtomicBool = AtomicBool::new(false);
        unsafe extern "system" fn terminated(_: *mut c_void) {
            assert!(SLOW_DONE.load(Ordering::SeqCst), "after the pending callback");
            CALLED.store(true, Ordering::SeqCst);
        }
        assert_eq!(unsafe { terminate(q, true, std::ptr::null_mut(), Some(terminated)) }, S_OK);
        assert!(CALLED.load(Ordering::SeqCst));
        unsafe { close(q) };
    }

    #[test]
    fn terminate_ends_a_waiting_dispatcher_of_its_own_queue_only() {
        let a = make(Mode::Manual, Mode::Manual);
        let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
        unsafe {
            get_port(a, WORK, &mut work);
            get_port(a, COMPLETION, &mut completion);
        }
        let mut b = std::ptr::null();
        assert_eq!(unsafe { create_composite(work, completion, &mut b) }, S_OK);
        let (tx, rx) = mpsc::channel();
        let a_addr = a as usize;
        std::thread::spawn(move || {
            while unsafe { dispatch(a_addr as Handle, WORK, INFINITE) } {}
            tx.send(()).unwrap();
        });
        assert_eq!(unsafe { terminate(b, false, std::ptr::null_mut(), None) }, S_OK);
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "b's end is not a's");
        let log = new_log();
        unsafe { submit_delayed(a, WORK, 0, ctx(&log, 5), Some(record)) };
        let start = Instant::now();
        while lock(&log.0).is_empty() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(lock(&log.0)[0].0, 5, "a's dispatcher still runs a's work");
        assert_eq!(unsafe { terminate(a, false, std::ptr::null_mut(), None) }, S_OK);
        rx.recv_timeout(Duration::from_secs(5)).expect("a's dispatcher ends");
        unsafe {
            close(a);
            close(b);
        }
    }

    #[test]
    fn monitors_see_each_submit() {
        let q = make(Mode::Manual, Mode::Manual);
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn monitor(_: *mut c_void, _: Handle, port: u32) {
            SEEN.fetch_add(1 + port as usize * 10, Ordering::SeqCst);
        }
        let mut token = 0;
        assert_eq!(unsafe { register_monitor(q, std::ptr::null_mut(), Some(monitor), &mut token) }, S_OK);
        let log = new_log();
        unsafe { submit_delayed(q, COMPLETION, 0, ctx(&log, 0), Some(record)) };
        assert_eq!(SEEN.load(Ordering::SeqCst), 11);
        unsafe { unregister_monitor(q, token) };
        unsafe { submit_delayed(q, WORK, 0, ctx(&log, 0), Some(record)) };
        assert_eq!(SEEN.load(Ordering::SeqCst), 11);
        unsafe { close(q) };
    }

    #[test]
    fn handles_and_the_process_queue() {
        let q = make(Mode::Manual, Mode::Manual);
        let mut d = std::ptr::null();
        assert_eq!(unsafe { duplicate(q, &mut d) }, S_OK);
        assert_eq!(d, q, "the same queue");
        unsafe { close(q) };
        let log = new_log();
        unsafe { submit_delayed(d, WORK, 0, ctx(&log, 1), Some(record)) };
        assert!(unsafe { dispatch(d, WORK, 0) }, "still open through the duplicate");
        unsafe { close(d) };
        let mut p = std::ptr::null();
        assert!(unsafe { get_process_queue(&mut p) });
        assert!(!p.is_null());
        unsafe { close(p) };
        assert_eq!(unsafe { create(9, 0, &mut d) }, E_INVALIDARG);
        assert_eq!(unsafe { submit_delayed(std::ptr::null(), 2, 0, std::ptr::null_mut(), Some(record)) }, E_INVALIDARG);
    }

    /// No call blocks: submitting to busy pool and serialized ports returns at once.
    #[test]
    fn submits_do_not_wait_for_busy_callbacks() {
        let pool_q = make(Mode::ThreadPool, Mode::SerializedThreadPool);
        unsafe extern "system" fn busy(_: *mut c_void, _: bool) {
            std::thread::sleep(Duration::from_millis(30));
        }
        let mut worst = Duration::ZERO;
        for i in 0..64 {
            let start = Instant::now();
            let port = if i % 2 == 0 { WORK } else { COMPLETION };
            assert_eq!(unsafe { submit_delayed(pool_q, port, 0, std::ptr::null_mut(), Some(busy)) }, S_OK);
            worst = worst.max(start.elapsed());
        }
        eprintln!("taskqueue: worst submit to busy ports {worst:?}");
        // A submit that waited for a busy callback would take >= 30 ms; 20 ms leaves room for the
        // scheduler under a parallel test run (alone: ~0.1 ms, agents/gdk.md).
        assert!(worst < Duration::from_millis(20), "worst submit {worst:?}");
        assert_eq!(unsafe { terminate(pool_q, true, std::ptr::null_mut(), None) }, S_OK);
        unsafe { close(pool_q) };
    }
}
