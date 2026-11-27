//! D1: the objects the title holds, as COM-shaped tables of `extern "system"` methods.
//!
//! Slots are 8 bytes each; slot 0-2 are `QueryInterface`, `AddRef`, `Release`. The slots the title
//! calls (from its call sites, as the fork read them; named in `agents/dstorage.md`):
//!
//! | object | slot | offset | method |
//! |:--|--:|--:|:--|
//! | factory | 4 | 0x20 | `OpenFile(path: *const u16, iid, out)` |
//! | factory | 5 | 0x28 | `CreateStatusArray(capacity: u32, name: *const c_char, iid, out)` |
//! | factory | 8 | 0x40 | `SetStagingBufferSize(bytes: u32) -> HRESULT` |
//! | factory | 10 | 0x50 | `CreateQueue(desc: *const QueueDesc, iid, out)` |
//! | queue | 3 | 0x18 | `EnqueueRequest(request: *const Request)` |
//! | queue | 4 | 0x20 | `EnqueueStatus(array, index: u32)` |
//! | queue | 6 | 0x30 | `Submit()` |
//! | queue | 8 | 0x40 | `Close()` (the title calls this, not `Release`) |
//! | status array | 3 | 0x18 | `IsComplete(index: u32) -> bool` |
//! | status array | 4 | 0x20 | `GetHResult(index: u32) -> HRESULT` |
//!
//! Every other slot returns `E_NOTIMPL` and is counted per object and slot ([`missed`]): a call
//! there is a discovery, never a guess.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::engine::{Engine, QueueId, StatusArray};
use crate::{
    Guid, Hresult, QueueDesc, Request, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, IID_FACTORY, IID_FILE, IID_QUEUE,
    IID_STATUS, IID_UNKNOWN, S_OK,
};

type Slot = *const ();
type Out = *mut *mut c_void;

#[repr(transparent)]
struct Vtable<const N: usize>([Slot; N]);
// SAFETY: function pointers only.
unsafe impl<const N: usize> Sync for Vtable<N> {}

/// One object: the method table first (as the title expects), then ours.
#[repr(C)]
struct Obj<T> {
    vtable: *const Slot,
    refs: AtomicU32,
    iid: Guid,
    state: T,
}

impl<T> Obj<T> {
    fn new<const N: usize>(vtable: &'static Vtable<N>, iid: Guid, state: T) -> *mut Obj<T> {
        Box::into_raw(Box::new(Obj { vtable: vtable.0.as_ptr(), refs: AtomicU32::new(1), iid, state }))
    }
}

const OBJECTS: [&str; 4] = ["factory", "file", "queue", "status array"];
const FACTORY: usize = 0;
const FILE: usize = 1;
const QUEUE: usize = 2;
const STATUS: usize = 3;
static MISSES: [[AtomicU32; 16]; 4] = [const { [const { AtomicU32::new(0) }; 16] }; 4];

extern "system" fn missing<const OBJECT: usize, const SLOT: usize>(_this: *mut c_void) -> Hresult {
    MISSES[OBJECT][SLOT].fetch_add(1, Ordering::Relaxed);
    E_NOTIMPL
}

/// Calls to slots nobody has implemented: (object, slot, count).
pub fn missed() -> Vec<(&'static str, usize, u32)> {
    let mut out = Vec::new();
    for (o, slots) in MISSES.iter().enumerate() {
        for (s, n) in slots.iter().enumerate() {
            let n = n.load(Ordering::Relaxed);
            if n > 0 {
                out.push((OBJECTS[o], s, n));
            }
        }
    }
    out
}

fn put(out: Out, object: *mut c_void) -> Hresult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the caller passes a place for an interface pointer.
    unsafe { *out = object };
    S_OK
}

extern "system" fn query_interface<T>(this: *mut Obj<T>, iid: *const Guid, out: Out) -> Hresult {
    // SAFETY: an object we made; the title passes a GUID (null tolerated).
    let (obj, iid) = unsafe { (&*this, iid.as_ref()) };
    match iid {
        Some(&i) if i == obj.iid || i == IID_UNKNOWN => {
            obj.refs.fetch_add(1, Ordering::Relaxed);
            put(out, this.cast())
        }
        _ => E_NOINTERFACE,
    }
}

extern "system" fn add_ref<T>(this: *mut Obj<T>) -> u32 {
    // SAFETY: an object we made and the caller holds.
    unsafe { &*this }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

extern "system" fn release<T: Finish>(this: *mut Obj<T>) -> u32 {
    // SAFETY: as above.
    let left = unsafe { &*this }.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference to a box made by `Obj::new`.
        let obj = unsafe { Box::from_raw(this) };
        obj.state.finish(this as usize);
    }
    left
}

/// What an object does when its last reference goes.
trait Finish {
    fn finish(&self, _key: usize) {}
}

// ---- factory ----------------------------------------------------------------------------------

struct FactoryState {
    engine: Arc<Engine>,
}

impl Finish for FactoryState {}

type Factory = *mut Obj<FactoryState>;

/// The factory never goes away while its owner ([`OwnedFactory`] or [`install`]) lives.
extern "system" fn factory_release(_this: Factory) -> u32 {
    1
}

static FACTORY_VTABLE: Vtable<16> = Vtable([
    query_interface::<FactoryState> as Slot,
    add_ref::<FactoryState> as Slot,
    factory_release as Slot,
    missing::<FACTORY, 3> as Slot,
    open_file as Slot,
    create_status_array as Slot,
    missing::<FACTORY, 6> as Slot,
    missing::<FACTORY, 7> as Slot,
    set_staging_buffer_size as Slot,
    missing::<FACTORY, 9> as Slot,
    create_queue as Slot,
    missing::<FACTORY, 11> as Slot,
    missing::<FACTORY, 12> as Slot,
    missing::<FACTORY, 13> as Slot,
    missing::<FACTORY, 14> as Slot,
    missing::<FACTORY, 15> as Slot,
]);

fn wide(path: *const u16) -> String {
    // SAFETY: the title passes a NUL-terminated UTF-16 path.
    unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(path, len))
    }
}

extern "system" fn open_file(this: Factory, path: *const u16, _iid: *const Guid, out: Out) -> Hresult {
    if path.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: our factory.
    let engine = unsafe { &*this }.state.engine.clone();
    let file = Obj::new(&FILE_VTABLE, IID_FILE, FileState { engine });
    // SAFETY: just made.
    match unsafe { &*file }.state.engine.open_file(file as usize, &wide(path)) {
        Ok(()) => put(out, file.cast()),
        Err(hr) => {
            // SAFETY: never handed out.
            drop(unsafe { Box::from_raw(file) });
            hr
        }
    }
}

extern "system" fn create_status_array(
    _this: Factory,
    capacity: u32,
    _name: *const c_char,
    _iid: *const Guid,
    out: Out,
) -> Hresult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    put(out, Obj::new(&STATUS_VTABLE, IID_STATUS, StatusArray::new(capacity)).cast())
}

extern "system" fn set_staging_buffer_size(this: Factory, bytes: u32) -> Hresult {
    // SAFETY: our factory.
    unsafe { &*this }.state.engine.set_staging(u64::from(bytes));
    S_OK
}

extern "system" fn create_queue(this: Factory, desc: *const QueueDesc, _iid: *const Guid, out: Out) -> Hresult {
    // SAFETY: the title passes its description (null tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else { return E_INVALIDARG };
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: our factory.
    let engine = unsafe { &*this }.state.engine.clone();
    let id = engine.create_queue(desc.capacity, desc.priority);
    put(out, Obj::new(&QUEUE_VTABLE, IID_QUEUE, QueueState { engine, id, closed: AtomicBool::new(false) }).cast())
}

// ---- file -------------------------------------------------------------------------------------

struct FileState {
    engine: Arc<Engine>,
}

impl Finish for FileState {
    fn finish(&self, key: usize) {
        self.engine.close_file(key);
    }
}

static FILE_VTABLE: Vtable<8> = Vtable([
    query_interface::<FileState> as Slot,
    add_ref::<FileState> as Slot,
    release::<FileState> as Slot,
    missing::<FILE, 3> as Slot,
    missing::<FILE, 4> as Slot,
    missing::<FILE, 5> as Slot,
    missing::<FILE, 6> as Slot,
    missing::<FILE, 7> as Slot,
]);

// ---- status array -----------------------------------------------------------------------------

impl Finish for Arc<StatusArray> {}

type Status = *mut Obj<Arc<StatusArray>>;

extern "system" fn is_complete(this: Status, index: u32) -> bool {
    // SAFETY: our array.
    unsafe { &*this }.state.is_complete(index)
}

extern "system" fn get_hresult(this: Status, index: u32) -> Hresult {
    // SAFETY: our array.
    unsafe { &*this }.state.result(index)
}

static STATUS_VTABLE: Vtable<8> = Vtable([
    query_interface::<Arc<StatusArray>> as Slot,
    add_ref::<Arc<StatusArray>> as Slot,
    release::<Arc<StatusArray>> as Slot,
    is_complete as Slot,
    get_hresult as Slot,
    missing::<STATUS, 5> as Slot,
    missing::<STATUS, 6> as Slot,
    missing::<STATUS, 7> as Slot,
]);

// ---- queue ------------------------------------------------------------------------------------

struct QueueState {
    engine: Arc<Engine>,
    id: QueueId,
    closed: AtomicBool,
}

impl QueueState {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.engine.close(self.id);
        }
    }
}

impl Finish for QueueState {
    fn finish(&self, _key: usize) {
        self.close();
    }
}

type Queue = *mut Obj<QueueState>;

extern "system" fn enqueue_request(this: Queue, request: *const Request) {
    // SAFETY: the title passes its request (null tolerated) and keeps its buffers valid until it
    // sees the request complete, which is `Engine::enqueue_request`'s contract.
    unsafe {
        let Some(&request) = request.as_ref() else { return };
        let q = &(*this).state;
        q.engine.enqueue_request(q.id, request);
    }
}

extern "system" fn enqueue_status(this: Queue, array: Status, index: u32) {
    // SAFETY: our queue; the title passes an array we made (null tolerated).
    let (q, array) = unsafe { (&(*this).state, array.as_ref()) };
    if let Some(array) = array {
        q.engine.enqueue_status(q.id, &array.state, index);
    }
}

extern "system" fn submit(this: Queue) {
    // SAFETY: our queue.
    let q = unsafe { &(*this).state };
    q.engine.submit(q.id);
}

extern "system" fn close(this: Queue) {
    // SAFETY: our queue.
    unsafe { &(*this).state }.close();
}

static QUEUE_VTABLE: Vtable<12> = Vtable([
    query_interface::<QueueState> as Slot,
    add_ref::<QueueState> as Slot,
    release::<QueueState> as Slot,
    enqueue_request as Slot,
    enqueue_status as Slot,
    missing::<QUEUE, 5> as Slot,
    submit as Slot,
    missing::<QUEUE, 7> as Slot,
    close as Slot,
    missing::<QUEUE, 9> as Slot,
    missing::<QUEUE, 10> as Slot,
    missing::<QUEUE, 11> as Slot,
]);

// ---- the export -------------------------------------------------------------------------------

/// A factory object owned by Rust code (tests, or a runtime that wants several).
pub struct OwnedFactory(Factory);

// SAFETY: the factory's state is an `Arc<Engine>`, which is shared between threads by design.
unsafe impl Send for OwnedFactory {}
unsafe impl Sync for OwnedFactory {}

impl OwnedFactory {
    pub fn new(engine: Engine) -> Self {
        Self(Obj::new(&FACTORY_VTABLE, IID_FACTORY, FactoryState { engine: Arc::new(engine) }))
    }
    /// The interface pointer the title would hold.
    pub fn as_ptr(&self) -> *mut c_void {
        self.0.cast()
    }
    pub fn engine(&self) -> &Arc<Engine> {
        // SAFETY: ours until drop.
        &unsafe { &*self.0 }.state.engine
    }
}

impl Drop for OwnedFactory {
    fn drop(&mut self) {
        // SAFETY: made by `Obj::new`, released only here.
        drop(unsafe { Box::from_raw(self.0) });
    }
}

static INSTALLED: OnceLock<OwnedFactory> = OnceLock::new();

/// Makes `engine` the one `DStorageGetFactory` hands out (once per process).
pub fn install(engine: Engine) -> Result<(), Engine> {
    let mut engine = Some(engine);
    INSTALLED.get_or_init(|| OwnedFactory::new(engine.take().unwrap()));
    match engine {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// `xcrdapi.dll!DStorageGetFactory(REFIID, void**)`.
///
/// # Safety
/// `iid` must be null or point to a GUID; `out` must be null or writable.
#[allow(non_snake_case)]
pub unsafe extern "system" fn DStorageGetFactory(iid: *const Guid, out: Out) -> Hresult {
    let Some(factory) = INSTALLED.get() else { return E_NOTIMPL };
    match iid.as_ref() {
        Some(&i) if i == IID_FACTORY || i == IID_UNKNOWN => put(out, factory.as_ptr()),
        _ => E_NOINTERFACE,
    }
}

/// Reads method `slot` of an object's table (tests call the objects exactly as the title does).
///
/// # Safety
/// `object` must be one of ours and `F` the method's exact `extern "system"` signature.
pub unsafe fn method<F: Copy>(object: *mut c_void, slot: usize) -> F {
    let table = *object.cast::<*const Slot>();
    std::mem::transmute_copy(&*table.add(slot))
}
