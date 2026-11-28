//! XAsync: the async-call blocks every runtime `*Async` function takes, and the provider protocol.
//!
//! The title owns the `XAsyncBlock` (queue, context, completion routine, then four words of system
//! area). A call lives from `XAsyncBegin` until its result is taken (`XAsyncGetResult`), or until it
//! completes with a failure or with no result data; then the provider gets `Cleanup` once. Our state
//! for a call is kept by block address; the block's system area holds the status (word 1) and the
//! result size (word 2), so both stay readable after the call's state is gone.
//!
//! Completion publishes the status, wakes `XAsyncGetStatus` waiters, then queues the completion
//! routine on the block's completion port. Waiters do not wait for the routine: on a manual
//! completion port nobody may be dispatching, and a wait must not hang on that.
//!
//! The provider ops (`XAsyncOp`): Begin 0, DoWork 1, GetResult 2, Cancel 3, Cleanup 4.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};

use crate::hr::*;
use crate::taskqueue::{self, Handle, COMPLETION, WORK};

pub type CompletionRoutine = unsafe extern "system" fn(*mut XAsyncBlock);
pub type Provider = unsafe extern "system" fn(u32, *const ProviderData) -> HRESULT;
pub type Work = unsafe extern "system" fn(*mut XAsyncBlock) -> HRESULT;

pub const OP_BEGIN: u32 = 0;
pub const OP_DO_WORK: u32 = 1;
pub const OP_GET_RESULT: u32 = 2;
pub const OP_CANCEL: u32 = 3;
pub const OP_CLEANUP: u32 = 4;

/// `XAsyncBlock`, 56 bytes.
#[repr(C)]
pub struct XAsyncBlock {
    pub queue: Handle,
    pub context: *mut c_void,
    pub callback: Option<CompletionRoutine>,
    /// The system area: [unused, status, result size, unused] here.
    pub internal: [usize; 4],
}

impl XAsyncBlock {
    pub fn new(queue: Handle, callback: Option<CompletionRoutine>) -> XAsyncBlock {
        XAsyncBlock { queue, context: std::ptr::null_mut(), callback, internal: [0; 4] }
    }
}

/// `XAsyncProviderData`, 32 bytes.
#[repr(C)]
pub struct ProviderData {
    pub async_block: *mut XAsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    completed: Mutex<bool>,
    done: Condvar,
    gate: Mutex<Gate>,
}

/// When the provider's Cleanup may run. A provider commonly calls `XAsyncComplete` from inside its
/// DoWork (or its Cancel) and keeps using its context until that op returns; the public reference
/// implementation (libHttpClient `AsyncLib.cpp`) therefore runs Cleanup only after DoWork returns.
/// `busy` counts DoWork / Cancel ops in flight; a Cleanup asked for meanwhile is deferred to the last
/// one's return.
#[derive(Default)]
struct Gate {
    busy: u32,
    cleanup_due: bool,
    cleaned: bool,
}

impl Call {
    fn op(&self, op: u32, block: *mut XAsyncBlock, size: usize, buffer: *mut c_void) -> HRESULT {
        let data = ProviderData { async_block: block, buffer_size: size, buffer, context: self.context as *mut c_void };
        // SAFETY: the title's (or our) provider, with the context given to `begin`.
        unsafe { (self.provider)(op, &data) }
    }

    /// Enters a DoWork / Cancel op; false once Cleanup has run or is due (the call is over).
    fn enter(&self) -> bool {
        let mut g = lock(&self.gate);
        if g.cleaned || g.cleanup_due {
            return false;
        }
        g.busy += 1;
        true
    }

    /// Leaves an op entered with [`Call::enter`]; runs a Cleanup that waited for it.
    fn leave(&self, block: *mut XAsyncBlock) {
        let run = {
            let mut g = lock(&self.gate);
            g.busy -= 1;
            let run = g.busy == 0 && g.cleanup_due && !g.cleaned;
            g.cleaned |= run;
            run
        };
        if run {
            self.op(OP_CLEANUP, block, 0, std::ptr::null_mut());
        }
    }

    /// The provider's Cleanup, once: now, or when the ops in flight return.
    fn cleanup(&self, block: *mut XAsyncBlock) {
        let run = {
            let mut g = lock(&self.gate);
            if g.cleaned {
                false
            } else if g.busy > 0 {
                g.cleanup_due = true;
                false
            } else {
                g.cleaned = true;
                true
            }
        };
        if run {
            self.op(OP_CLEANUP, block, 0, std::ptr::null_mut());
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static CALLS: LazyLock<Mutex<HashMap<usize, Arc<Call>>>> = LazyLock::new(Default::default);

fn call(block: *mut XAsyncBlock) -> Option<Arc<Call>> {
    lock(&CALLS).get(&(block as usize)).cloned()
}

/// The block's status and result-size words.
fn words(block: *mut XAsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives its call; its system area is ours. AtomicUsize has
    // usize's layout.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut XAsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

/// Ends the call: our state goes, the provider cleans up.
fn end(block: *mut XAsyncBlock) {
    let removed = lock(&CALLS).remove(&(block as usize));
    if let Some(call) = removed {
        call.cleanup(block);
    }
}

/// `XAsyncBegin`. A failed Begin op completes the call with that failure (so the completion routine
/// still runs) and `XAsyncBegin` itself succeeds, as Microsoft's public reference implementation
/// (libHttpClient `AsyncLib.cpp`) does. A block already in flight is refused with `E_INVALIDARG` (same).
pub unsafe fn begin(block: *mut XAsyncBlock, context: *mut c_void, identity: *const c_void, provider: Option<Provider>) -> HRESULT {
    let Some(provider) = provider else { return E_INVALIDARG };
    if block.is_null() {
        return E_INVALIDARG;
    }
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        completed: Mutex::new(false),
        done: Condvar::new(),
        gate: Mutex::default(),
    });
    {
        let mut calls = lock(&CALLS);
        if calls.contains_key(&(block as usize)) {
            return E_INVALIDARG;
        }
        let (status, size) = words(block);
        status.store(E_PENDING as u32 as usize, Ordering::Release);
        size.store(0, Ordering::Release);
        calls.insert(block as usize, call.clone());
    }
    let hr = call.op(OP_BEGIN, block, 0, std::ptr::null_mut());
    if failed(hr) {
        unsafe { complete(block, hr, 0) };
    }
    S_OK
}

/// The call's work on its work port. Canceled (its queue terminated): the provider gets its Cancel op,
/// then the call completes with `E_ABORT` if the provider did not complete it (as the reference
/// implementation does). Cleanup, if the call ends here, runs after the op returns.
unsafe extern "system" fn do_work(block: *mut c_void, canceled: bool) {
    let block = block.cast::<XAsyncBlock>();
    let Some(call) = call(block) else { return };
    if !call.enter() {
        return;
    }
    if !*lock(&call.completed) {
        if canceled {
            call.op(OP_CANCEL, block, 0, std::ptr::null_mut());
            unsafe { complete(block, E_ABORT, 0) };
        } else {
            let hr = call.op(OP_DO_WORK, block, 0, std::ptr::null_mut());
            if hr != E_PENDING {
                unsafe { complete(block, hr, 0) };
            }
        }
    }
    call.leave(block);
}

/// `XAsyncSchedule`: the provider's DoWork on the block's work port, after `delay_ms`.
pub unsafe fn schedule(block: *mut XAsyncBlock, delay_ms: u32) -> HRESULT {
    if block.is_null() || call(block).is_none() {
        return E_INVALIDARG;
    }
    let Some(queue) = taskqueue::queue(unsafe { (*block).queue }) else { return E_INVALIDARG };
    queue.submit(WORK, delay_ms, block.cast(), do_work)
}

/// A completion routine on its way: the block and the routine read from it before the status was
/// published (after that the title may reuse the block, so it is not read again).
struct Completion {
    block: *mut XAsyncBlock,
    routine: CompletionRoutine,
}

unsafe extern "system" fn run_completion(context: *mut c_void, _canceled: bool) {
    // SAFETY: boxed in `complete`, run once.
    let c = unsafe { Box::from_raw(context.cast::<Completion>()) };
    unsafe { (c.routine)(c.block) };
}

/// `XAsyncComplete`: the first result wins; `E_PENDING` is not a result.
pub unsafe fn complete(block: *mut XAsyncBlock, result: HRESULT, required_size: usize) {
    if result == E_PENDING || block.is_null() {
        return;
    }
    let Some(call) = call(block) else { return };
    let (queue, routine);
    let ending = failed(result) || required_size == 0;
    {
        let mut completed = lock(&call.completed);
        if *completed {
            return;
        }
        // Read before publishing: once the status is out, the title may reuse or free the block.
        (queue, routine) = unsafe { ((*block).queue, (*block).callback) };
        if ending {
            // Out of the table before the status is out, so the block is free for a new call
            // as soon as a waiter sees it complete.
            lock(&CALLS).remove(&(block as usize));
        }
        let (status, size) = words(block);
        size.store(required_size, Ordering::Release);
        status.store(result as u32 as usize, Ordering::Release);
        *completed = true;
    }
    call.done.notify_all();
    if ending {
        call.cleanup(block);
    }
    if let Some(routine) = routine {
        let context = Box::into_raw(Box::new(Completion { block, routine })).cast::<c_void>();
        match taskqueue::queue(queue) {
            Some(q) if q.submit(COMPLETION, 0, context, run_completion) == S_OK => {}
            // A terminated (or cleared) queue: the routine still runs, here; the console's choice
            // for this case is unknown.
            _ => unsafe { run_completion(context, true) },
        }
    }
}

/// `XAsyncGetStatus`. With `wait`, blocks until the call completes (documented); never on the
/// completion routine.
pub unsafe fn get_status(block: *mut XAsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait {
        match call(block) {
            Some(call) => {
                let completed = lock(&call.completed);
                drop(call.done.wait_while(completed, |c| !*c).unwrap_or_else(|e| e.into_inner()));
            }
            // No call but still pending: `complete` takes the call out of the table just before it writes the
            // status (so the block is free the moment a waiter sees it), and this waiter landed in between.
            // The status is a few instructions away; never hand back E_PENDING from a wait.
            None => {
                let start = std::time::Instant::now();
                while status(block) == E_PENDING && start.elapsed() < std::time::Duration::from_secs(1) {
                    std::thread::yield_now();
                }
            }
        }
    }
    status(block)
}

/// `XAsyncGetResultSize`: the call's status; the size for any success code, else 0 (the size is
/// always written, as the public reference implementation does).
pub unsafe fn get_result_size(block: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = if !failed(hr) && hr != E_PENDING { words(block).1.load(Ordering::Acquire) } else { 0 };
    unsafe { out.write_unaligned(size) };
    hr
}

/// `XAsyncCancel`: the provider's Cancel op, nothing more. Cancellation is the provider's to finish:
/// `XAsyncOp::Cancel` says the provider "should cancel all asynchronous work and then call
/// XAsyncComplete" with `E_ABORT`; `XAsyncCancel` says cancellation "may happen synchronously,
/// asynchronously, or not at all"; and calls started with `XAsyncRun` "can't be canceled"
/// (learn.microsoft.com, GDK XAsync reference; docs/research/public-sources-review.md). Completing
/// the call here instead would run the provider's Cleanup while its DoWork may still be running.
pub unsafe fn cancel(block: *mut XAsyncBlock) {
    if let Some(call) = call(block) {
        if *lock(&call.completed) || !call.enter() {
            return;
        }
        call.op(OP_CANCEL, block, 0, std::ptr::null_mut());
        call.leave(block);
    }
}

/// `XAsyncGetResult`: the provider writes the result into `buffer`, and the call ends.
pub unsafe fn get_result(block: *mut XAsyncBlock, identity: *const c_void, size: usize, buffer: *mut c_void, used: *mut usize) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    if hr == E_PENDING || failed(hr) {
        return hr;
    }
    let need = words(block).1.load(Ordering::Acquire);
    if need == 0 {
        if !used.is_null() {
            unsafe { used.write_unaligned(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else { return E_ILLEGAL_METHOD_CALL };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    // A null buffer is an invalid argument, a short one too small (the reference implementation's
    // order). Unlike it, the call stays alive after either, so the title may ask again.
    if buffer.is_null() {
        return E_INVALIDARG;
    }
    if size < need {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let got = call.op(OP_GET_RESULT, block, size, buffer);
    if !used.is_null() {
        unsafe { used.write_unaligned(need) };
    }
    end(block);
    if failed(got) { got } else { hr }
}

unsafe extern "system" fn run_provider(op: u32, data: *const ProviderData) -> HRESULT {
    let data = unsafe { &*data };
    match op {
        OP_BEGIN => unsafe { schedule(data.async_block, 0) },
        OP_DO_WORK => {
            // SAFETY: the context is the `Work` given to `run`.
            let work: Work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
            unsafe { work(data.async_block) }
        }
        _ => S_OK,
    }
}

/// `XAsyncRun`: `work` on the block's work port; its return value is the call's result.
pub unsafe fn run(block: *mut XAsyncBlock, work: Option<Work>) -> HRESULT {
    let Some(work) = work else { return E_INVALIDARG };
    unsafe { begin(block, work as *mut c_void, run as *const c_void, Some(run_provider)) }
}

// Helpers for runtime areas: calls whose outcome is known when they start.

type Outcome = Result<Vec<u8>, HRESULT>;

unsafe extern "system" fn known_provider(op: u32, data: *const ProviderData) -> HRESULT {
    let data = unsafe { &*data };
    let outcome = data.context.cast::<Outcome>();
    match op {
        OP_BEGIN => {
            let (hr, len) = match unsafe { &*outcome } {
                Ok(bytes) => (S_OK, bytes.len()),
                Err(hr) => (*hr, 0),
            };
            unsafe { complete(data.async_block, hr, len) };
        }
        OP_GET_RESULT => {
            if let Ok(bytes) = unsafe { &*outcome } {
                unsafe { data.buffer.cast::<u8>().copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
            }
        }
        // SAFETY: boxed in `finish`; nothing runs after cleanup.
        OP_CLEANUP => drop(unsafe { Box::from_raw(outcome) }),
        _ => {}
    }
    S_OK
}

/// Starts a call on `block` that completes at once with `outcome`, through the block's completion
/// port as usual. `identity` tells this call's result apart from others on the same block.
pub unsafe fn finish(block: *mut XAsyncBlock, identity: &'static u8, outcome: Outcome) -> HRESULT {
    let context = Box::into_raw(Box::new(outcome));
    let hr = unsafe { begin(block, context.cast(), (identity as *const u8).cast(), Some(known_provider)) };
    if failed(hr) {
        // Refused before the call existed: no cleanup will come.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// The result of a [`finish`]ed call as a `T`.
pub unsafe fn take<T>(block: *mut XAsyncBlock, identity: &'static u8, out: *mut T) -> HRESULT {
    unsafe { get_result(block, (identity as *const u8).cast(), size_of::<T>(), out.cast(), std::ptr::null_mut()) }
}

/// The bytes of a plain value, for [`finish`].
pub fn bytes<T: Copy>(value: T) -> Vec<u8> {
    // SAFETY: a `Copy` value read as bytes; padding is never interpreted.
    unsafe { std::slice::from_raw_parts((&value as *const T).cast::<u8>(), size_of::<T>()) }.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taskqueue::{Mode, INFINITE};
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    fn queue(work: Mode, completion: Mode) -> Handle {
        let mut q = std::ptr::null();
        assert_eq!(unsafe { taskqueue::create(work as u32, completion as u32, &mut q) }, S_OK);
        q
    }

    /// Schedules its work, completes with 8 bytes: the context's value. Records which thread ran
    /// DoWork in the block's context.
    unsafe extern "system" fn eight(op: u32, data: *const ProviderData) -> HRESULT {
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => unsafe { schedule(data.async_block, 0) },
            OP_DO_WORK => {
                let block = unsafe { &*data.async_block };
                if !block.context.is_null() {
                    unsafe { *block.context.cast::<Option<ThreadId>>() = Some(std::thread::current().id()) };
                }
                unsafe { complete(data.async_block, S_OK, 8) };
                S_OK
            }
            OP_GET_RESULT => {
                unsafe { data.buffer.cast::<u64>().write_unaligned(data.context as u64) };
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn layouts() {
        assert_eq!(size_of::<XAsyncBlock>(), 56);
        assert_eq!(size_of::<ProviderData>(), 32);
    }

    /// DoWork runs where the work port is dispatched, the completion routine where the completion
    /// port is: two manual ports dispatched from two threads.
    #[test]
    fn callbacks_run_on_the_right_port() {
        let q = queue(Mode::Manual, Mode::Manual);
        static ROUTINE_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);
        unsafe extern "system" fn routine(_: *mut XAsyncBlock) {
            *lock(&ROUTINE_THREAD) = Some(std::thread::current().id());
        }
        let mut work_thread: Option<ThreadId> = None;
        let mut block = XAsyncBlock::new(q, Some(routine));
        block.context = (&mut work_thread as *mut Option<ThreadId>).cast();
        let b = &mut block as *mut XAsyncBlock;
        let identity = 0x10 as *const c_void;
        assert_eq!(unsafe { begin(b, 42 as *mut c_void, identity, Some(eight)) }, S_OK);
        assert_eq!(unsafe { get_status(b, false) }, E_PENDING);
        let qa = q as usize;
        let worker = std::thread::spawn(move || {
            assert!(unsafe { taskqueue::dispatch(qa as Handle, WORK, INFINITE) });
            std::thread::current().id()
        });
        let worker_id = worker.join().unwrap();
        assert_eq!(unsafe { get_status(b, true) }, S_OK, "status is out before the routine runs");
        assert!(lock(&ROUTINE_THREAD).is_none(), "the completion port is manual");
        let completer = std::thread::spawn(move || {
            assert!(unsafe { taskqueue::dispatch(qa as Handle, COMPLETION, INFINITE) });
            std::thread::current().id()
        });
        let completer_id = completer.join().unwrap();
        assert_eq!(work_thread, Some(worker_id));
        assert_eq!(*lock(&ROUTINE_THREAD), Some(completer_id));
        assert_ne!(worker_id, completer_id);

        let mut size = 0;
        assert_eq!(unsafe { get_result_size(b, &mut size) }, S_OK);
        assert_eq!(size, 8);
        let mut small = 0u32;
        assert_eq!(
            unsafe { get_result(b, identity, 4, (&mut small as *mut u32).cast(), std::ptr::null_mut()) },
            E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(
            unsafe { get_result(b, 0x11 as *const c_void, 8, (&mut small as *mut u32).cast(), std::ptr::null_mut()) },
            E_INVALIDARG,
            "another call's identity"
        );
        let (mut value, mut used) = (0u64, 0usize);
        assert_eq!(unsafe { get_result(b, identity, 8, (&mut value as *mut u64).cast(), &mut used) }, S_OK);
        assert_eq!((value, used), (42, 8));
        assert!(call(b).is_none(), "the call ends with its result");
        assert_eq!(unsafe { get_status(b, false) }, S_OK, "status stays readable");
        unsafe { taskqueue::close(q) };
    }

    /// Calls complete in the order their work is dispatched, and the block can be reused after.
    #[test]
    fn ordering_and_reuse() {
        let q = queue(Mode::Manual, Mode::Immediate);
        static ORDER: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        unsafe extern "system" fn routine(b: *mut XAsyncBlock) {
            lock(&ORDER).push(unsafe { (*b).context } as usize);
        }
        unsafe extern "system" fn quick(op: u32, data: *const ProviderData) -> HRESULT {
            match op {
                OP_BEGIN => unsafe { schedule((*data).async_block, 0) },
                OP_DO_WORK => S_OK,
                _ => S_OK,
            }
        }
        let mut blocks: Vec<XAsyncBlock> = (0..4).map(|_| XAsyncBlock::new(q, Some(routine))).collect();
        for (i, b) in blocks.iter_mut().enumerate() {
            b.context = i as *mut c_void;
            assert_eq!(unsafe { begin(b, std::ptr::null_mut(), std::ptr::null(), Some(quick)) }, S_OK);
        }
        let b0 = &mut blocks[0] as *mut XAsyncBlock;
        assert_eq!(
            unsafe { begin(b0, std::ptr::null_mut(), std::ptr::null(), Some(quick)) },
            E_INVALIDARG,
            "a block in flight"
        );
        while unsafe { taskqueue::dispatch(q, WORK, 0) } {}
        assert_eq!(*lock(&ORDER), [0, 1, 2, 3]);
        assert_eq!(unsafe { begin(b0, std::ptr::null_mut(), std::ptr::null(), Some(quick)) }, S_OK, "reused");
        assert!(unsafe { taskqueue::dispatch(q, WORK, 0) });
        assert_eq!(unsafe { get_status(b0, false) }, S_OK);
        unsafe { taskqueue::close(q) };
    }

    static CANCELS: AtomicU32 = AtomicU32::new(0);
    static CLEANUPS: AtomicU32 = AtomicU32::new(0);
    /// Never finishes on its own; canceled, it completes with E_ABORT as `XAsyncOp::Cancel` asks.
    unsafe extern "system" fn forever(op: u32, data: *const ProviderData) -> HRESULT {
        match op {
            OP_BEGIN => unsafe { schedule((*data).async_block, 0) },
            OP_DO_WORK => E_PENDING,
            OP_CANCEL => {
                CANCELS.fetch_add(1, Ordering::SeqCst);
                unsafe { complete((*data).async_block, E_ABORT, 0) };
                S_OK
            }
            OP_CLEANUP => {
                CLEANUPS.fetch_add(1, Ordering::SeqCst);
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn cancel_completes_with_abort_and_cleans_up_once() {
        let q = queue(Mode::ThreadPool, Mode::ThreadPool);
        let mut block = XAsyncBlock::new(q, None);
        let b = &mut block as *mut XAsyncBlock;
        let (c0, k0) = (CANCELS.load(Ordering::SeqCst), CLEANUPS.load(Ordering::SeqCst));
        assert_eq!(unsafe { begin(b, std::ptr::null_mut(), std::ptr::null(), Some(forever)) }, S_OK);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(unsafe { get_status(b, false) }, E_PENDING);
        unsafe { cancel(b) };
        assert_eq!(unsafe { get_status(b, true) }, E_ABORT);
        unsafe { cancel(b) };
        assert_eq!(CANCELS.load(Ordering::SeqCst) - c0, 1);
        assert_eq!(CLEANUPS.load(Ordering::SeqCst) - k0, 1);
        unsafe { complete(b, S_OK, 8) };
        assert_eq!(unsafe { get_status(b, false) }, E_ABORT, "the first result wins");
        unsafe { taskqueue::close(q) };
    }

    /// The documented cancelable provider ("Set up cancelability"): Cancel only signals, DoWork
    /// notices, completes with E_ABORT and keeps using its state until it returns. Cleanup must wait
    /// for DoWork to return, and XAsyncCancel must not complete the call itself.
    #[test]
    fn cancel_is_cooperative_and_cleanup_waits_for_do_work() {
        static IN_WORK: AtomicBool = AtomicBool::new(false);
        static SIGNAL: AtomicBool = AtomicBool::new(false);
        static CLEANED: AtomicU32 = AtomicU32::new(0);
        static CLEANED_IN_WORK: AtomicBool = AtomicBool::new(false);
        unsafe extern "system" fn cooperative(op: u32, data: *const ProviderData) -> HRESULT {
            let block = unsafe { (*data).async_block };
            match op {
                OP_BEGIN => unsafe { schedule(block, 0) },
                OP_DO_WORK => {
                    IN_WORK.store(true, Ordering::SeqCst);
                    while !SIGNAL.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    unsafe { complete(block, E_ABORT, 0) };
                    // Still using its state after XAsyncComplete, as providers do.
                    std::thread::sleep(Duration::from_millis(30));
                    IN_WORK.store(false, Ordering::SeqCst);
                    S_OK
                }
                OP_CANCEL => {
                    SIGNAL.store(true, Ordering::SeqCst);
                    S_OK
                }
                OP_CLEANUP => {
                    CLEANED_IN_WORK.fetch_or(IN_WORK.load(Ordering::SeqCst), Ordering::SeqCst);
                    CLEANED.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }
        let q = queue(Mode::ThreadPool, Mode::ThreadPool);
        let mut block = XAsyncBlock::new(q, None);
        let b = &mut block as *mut XAsyncBlock;
        assert_eq!(unsafe { begin(b, std::ptr::null_mut(), std::ptr::null(), Some(cooperative)) }, S_OK);
        let start = Instant::now();
        while !IN_WORK.load(Ordering::SeqCst) {
            assert!(start.elapsed() < Duration::from_secs(5), "DoWork never started");
            std::thread::yield_now();
        }
        unsafe { cancel(b) };
        assert_eq!(unsafe { get_status(b, true) }, E_ABORT);
        let start = Instant::now();
        while CLEANED.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "no cleanup");
            std::thread::yield_now();
        }
        assert!(!CLEANED_IN_WORK.load(Ordering::SeqCst), "Cleanup ran while DoWork was still running");
        assert_eq!(CLEANED.load(Ordering::SeqCst), 1);
        unsafe { taskqueue::close(q) };
    }

    /// "Basic async methods that are started with XAsyncRun can't be canceled": the work's own
    /// result stands.
    #[test]
    fn xasyncrun_is_not_cancelable() {
        unsafe extern "system" fn work(_: *mut XAsyncBlock) -> HRESULT {
            std::thread::sleep(Duration::from_millis(30));
            S_OK
        }
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        let b = &mut block as *mut XAsyncBlock;
        assert_eq!(unsafe { run(b, Some(work)) }, S_OK);
        unsafe { cancel(b) };
        assert_eq!(unsafe { get_status(b, true) }, S_OK);
    }

    #[test]
    fn result_size_is_written_on_failure_and_a_null_buffer_is_invalid() {
        static ID: u8 = 0;
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        let b = &mut block as *mut XAsyncBlock;
        assert_eq!(unsafe { finish(b, &ID, Err(E_GAMESTORE_NETWORK_ERROR)) }, S_OK);
        let mut size = 99;
        assert_eq!(unsafe { get_result_size(b, &mut size) }, E_GAMESTORE_NETWORK_ERROR);
        assert_eq!(size, 0, "written even for a failed call");

        assert_eq!(unsafe { finish(b, &ID, Ok(bytes(7u32))) }, S_OK);
        let id = (&ID as *const u8).cast();
        assert_eq!(unsafe { get_result(b, id, 4, std::ptr::null_mut(), std::ptr::null_mut()) }, E_INVALIDARG);
        let mut v = 0u32;
        assert_eq!(unsafe { take(b, &ID, &mut v) }, S_OK, "still there after the refusal");
        assert_eq!(v, 7);
    }

    /// Terminating the queue with a call's work still queued: the work runs canceled, the call fails
    /// with E_ABORT, and its completion routine still runs.
    #[test]
    fn terminate_with_pending_work() {
        let q = queue(Mode::Manual, Mode::Manual);
        static ROUTINE: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn routine(_: *mut XAsyncBlock) {
            ROUTINE.fetch_add(1, Ordering::SeqCst);
        }
        let mut block = XAsyncBlock::new(q, Some(routine));
        let b = &mut block as *mut XAsyncBlock;
        assert_eq!(unsafe { begin(b, std::ptr::null_mut(), std::ptr::null(), Some(eight)) }, S_OK);
        assert_eq!(unsafe { taskqueue::terminate(q, false, std::ptr::null_mut(), None) }, S_OK);
        assert!(unsafe { taskqueue::dispatch(q, WORK, 0) }, "the canceled work is delivered");
        assert_eq!(unsafe { get_status(b, false) }, E_ABORT);
        assert_eq!(ROUTINE.load(Ordering::SeqCst), 1, "routine ran although the port is closed");
        assert!(call(b).is_none());
        unsafe { taskqueue::close(q) };
    }

    #[test]
    fn run_returns_the_work_result_and_waits_do_not_hang() {
        unsafe extern "system" fn work(_: *mut XAsyncBlock) -> HRESULT {
            std::thread::sleep(Duration::from_millis(20));
            S_FALSE
        }
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        let b = &mut block as *mut XAsyncBlock;
        assert_eq!(unsafe { run(b, Some(work)) }, S_OK);
        assert_eq!(unsafe { get_status(b, true) }, S_FALSE);
        let mut size = 99;
        assert_eq!(unsafe { get_result_size(b, &mut size) }, S_FALSE);
        assert_eq!(size, 0);
        assert!(call(b).is_none(), "no data: the call ended at completion");
    }

    /// A block is free for its next call as soon as a wait sees the last one complete.
    #[test]
    fn a_block_is_reusable_once_its_wait_returns() {
        unsafe extern "system" fn work(_: *mut XAsyncBlock) -> HRESULT {
            S_OK
        }
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        let b = &mut block as *mut XAsyncBlock;
        for i in 0..2000 {
            assert_eq!(unsafe { run(b, Some(work)) }, S_OK, "call {i}");
            assert_eq!(unsafe { get_status(b, true) }, S_OK);
        }
    }

    #[test]
    fn finished_calls_hand_out_their_bytes() {
        static ID: u8 = 0;
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        let b = &mut block as *mut XAsyncBlock;
        assert_eq!(unsafe { finish(b, &ID, Ok(bytes(0x1122_3344_u32))) }, S_OK);
        assert_eq!(unsafe { get_status(b, true) }, S_OK);
        let mut v = 0u32;
        assert_eq!(unsafe { take(b, &ID, &mut v) }, S_OK);
        assert_eq!(v, 0x1122_3344);
        assert_eq!(unsafe { finish(b, &ID, Err(E_GAMESTORE_NETWORK_ERROR)) }, S_OK);
        assert_eq!(unsafe { get_status(b, false) }, E_GAMESTORE_NETWORK_ERROR);
        assert_eq!(unsafe { take(b, &ID, &mut v) }, E_GAMESTORE_NETWORK_ERROR);
    }

    /// No call blocks: begin + schedule onto a pool whose workers are all busy returns at once.
    #[test]
    fn begin_does_not_wait_for_busy_work() {
        let q = queue(Mode::ThreadPool, Mode::ThreadPool);
        unsafe extern "system" fn slow(op: u32, data: *const ProviderData) -> HRESULT {
            match op {
                OP_BEGIN => unsafe { schedule((*data).async_block, 0) },
                OP_DO_WORK => {
                    std::thread::sleep(Duration::from_millis(30));
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut blocks: Vec<XAsyncBlock> = (0..64).map(|_| XAsyncBlock::new(q, None)).collect();
        let mut worst = Duration::ZERO;
        for b in blocks.iter_mut() {
            let start = Instant::now();
            assert_eq!(unsafe { begin(b, std::ptr::null_mut(), std::ptr::null(), Some(slow)) }, S_OK);
            worst = worst.max(start.elapsed());
        }
        eprintln!("xasync: worst begin+schedule onto busy pool {worst:?}");
        // Waiting for a busy worker would take >= 30 ms; 20 ms leaves room for the scheduler under a
        // parallel test run (alone: ~0.1 ms, agents/gdk.md).
        assert!(worst < Duration::from_millis(20), "worst {worst:?}");
        for b in blocks.iter_mut() {
            assert_eq!(unsafe { get_status(b, true) }, S_OK);
        }
        unsafe { taskqueue::close(q) };
    }
}
