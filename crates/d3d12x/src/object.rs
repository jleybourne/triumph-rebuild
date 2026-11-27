//! What every object this crate hands out starts with, and the methods they share.
//!
//! ```text
//! +0x00 vtable   +0x08 u32 bits (0)   +0x0C u32 refcount        (agents/d3d12x-object-layouts.csv)
//! +0x10 our part: the context, private data, then the object's own fields
//! ```
//!
//! The title reads nothing past +0x10 of a resource, heap or descriptor heap (no inline field
//! reads are recorded for them), so the rest is ours.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOINTERFACE, S_OK};

const DXGI_ERROR_NOT_FOUND: HRESULT = HRESULT(0x887A_0002u32 as i32);
const DXGI_ERROR_MORE_DATA: HRESULT = HRESULT(0x887A_0003u32 as i32);
use windows::Win32::Graphics::Direct3D12::*;

use crate::slots::{self, Fp, Object, Table};
use crate::{Inner, Raw};

/// Object header every console object starts with.
#[repr(C)]
pub struct Header {
    pub vtable: *const Fp,
    /// device index bits 0-2 and bookkeeping; 0
    pub bits: u32,
    pub refs: AtomicU32,
}

/// Bytes of the console layout before our part.
pub const HEADER_BYTES: usize = 0x10;

/// The shared part after the header (+0x10).
#[repr(C)]
pub struct Common {
    pub ctx: Arc<Inner>,
    private: Mutex<Vec<(GUID, Vec<u8>)>>,
}

impl Common {
    pub fn new(ctx: &Arc<Inner>) -> Common {
        Common { ctx: ctx.clone(), private: Mutex::new(Vec::new()) }
    }
}

/// The head of each of our objects: header, then [`Common`].
#[repr(C)]
pub struct Head {
    pub header: Header,
    pub common: Common,
}

impl Head {
    pub fn new(object: Object, ctx: &Arc<Inner>) -> Head {
        slots::start_clock();
        Head { header: Header { vtable: table(object).funcs.as_ptr(), bits: 0, refs: AtomicU32::new(1) }, common: Common::new(ctx) }
    }
}

/// The context of one of our objects (traps use it to report).
pub(crate) fn context_of<'a>(this: Raw) -> Option<&'a Arc<Inner>> {
    if this.is_null() {
        return None;
    }
    // SAFETY: every object of ours starts with `Head`.
    Some(unsafe { &(*this.cast::<Head>()).common.ctx })
}

/// `this` is one of our objects of kind `object` (its table is ours).
#[inline]
pub(crate) fn is(this: Raw, object: Object) -> bool {
    // SAFETY: a non-null object pointer the title passed; its first word is a table pointer.
    !this.is_null() && unsafe { (*this.cast::<Header>()).vtable } == table(object).funcs.as_ptr()
}

/// The table of `object`, built once.
pub(crate) fn table(object: Object) -> &'static Table {
    static T: [OnceLock<Table>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    T[object as usize].get_or_init(|| slots::build(object, &crate::methods(object)))
}

/// The interfaces each object answers in `QueryInterface`.
pub(crate) fn iids(object: Object) -> &'static [GUID] {
    const UNKNOWN: GUID = windows::core::IUnknown::IID;
    static RESOURCE: [GUID; 7] =
        [ID3D12Resource::IID, ID3D12Resource1::IID, ID3D12Resource2::IID, ID3D12Pageable::IID, ID3D12DeviceChild::IID, ID3D12Object::IID, UNKNOWN];
    static HEAP: [GUID; 6] = [ID3D12Heap::IID, ID3D12Heap1::IID, ID3D12Pageable::IID, ID3D12DeviceChild::IID, ID3D12Object::IID, UNKNOWN];
    static DH: [GUID; 5] = [ID3D12DescriptorHeap::IID, ID3D12Pageable::IID, ID3D12DeviceChild::IID, ID3D12Object::IID, UNKNOWN];
    match object {
        Object::Resource => &RESOURCE,
        Object::Heap => &HEAP,
        Object::DescriptorHeap => &DH,
    }
}

fn head<'a>(this: Raw) -> &'a Head {
    // SAFETY: called from our methods only.
    unsafe { &*this.cast::<Head>() }
}

pub(crate) fn add_ref(this: Raw) -> u32 {
    head(this).header.refs.fetch_add(1, Ordering::Relaxed) + 1
}

/// Hand a new object to the title: answers `iid` if the object has it, else releases it and
/// fails as D3D12 does. `out` null: `S_FALSE`-free success check ("would creation succeed").
pub(crate) fn hand_out(this: Raw, object: Object, iid: *const GUID, out: *mut Raw, release: unsafe extern "system" fn(Raw) -> u32) -> HRESULT {
    if out.is_null() {
        unsafe { release(this) };
        return windows::Win32::Foundation::S_FALSE;
    }
    // SAFETY: the title passes an interface id or null.
    let wanted = unsafe { iid.as_ref() };
    if wanted.is_some_and(|g| !iids(object).contains(g)) {
        if let Some(g) = wanted {
            head(this).common.ctx.events.unknown_iid(object.name(), g);
        }
        unsafe {
            release(this);
            out.write(std::ptr::null_mut());
        }
        return E_NOINTERFACE;
    }
    unsafe { out.write(this) };
    S_OK
}

// ---- shared methods ----

pub(crate) unsafe extern "system" fn query<const O: usize>(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    let object = Object::ALL[O];
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes an interface id.
    match unsafe { iid.as_ref() } {
        Some(g) if iids(object).contains(g) => {
            add_ref(this);
            unsafe { out.write(this) };
            S_OK
        }
        g => {
            if let Some(g) = g {
                head(this).common.ctx.events.unknown_iid(object.name(), g);
            }
            unsafe { out.write(std::ptr::null_mut()) };
            E_NOINTERFACE
        }
    }
}

pub(crate) unsafe extern "system" fn add_ref_method(this: Raw) -> u32 {
    add_ref(this)
}

pub(crate) unsafe extern "system" fn get_private_data(this: Raw, guid: *const GUID, size: *mut u32, data: *mut c_void) -> HRESULT {
    let (Some(g), false) = (unsafe { guid.as_ref() }, size.is_null()) else {
        return E_INVALIDARG;
    };
    let list = head(this).common.private.lock().unwrap_or_else(|e| e.into_inner());
    let Some((_, bytes)) = list.iter().find(|(k, _)| k == g) else {
        unsafe { size.write(0) };
        return DXGI_ERROR_NOT_FOUND;
    };
    let room = unsafe { size.read() } as usize;
    unsafe { size.write(bytes.len() as u32) };
    if data.is_null() {
        return S_OK;
    }
    if room < bytes.len() {
        return DXGI_ERROR_MORE_DATA;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len()) };
    S_OK
}

pub(crate) unsafe extern "system" fn set_private_data(this: Raw, guid: *const GUID, size: u32, data: *const c_void) -> HRESULT {
    let Some(g) = (unsafe { guid.as_ref() }) else {
        return E_INVALIDARG;
    };
    let mut list = head(this).common.private.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(k, _)| k != g);
    if !data.is_null() && size > 0 {
        // SAFETY: the title passes `size` bytes.
        list.push((*g, unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) }.to_vec()));
    }
    S_OK
}

/// `SetName`: names only show in tools; accepted.
pub(crate) unsafe extern "system" fn set_name(_this: Raw, _name: *const u16) -> HRESULT {
    S_OK
}

/// `GetDevice` (0x38): the title's device answers through its own `QueryInterface`.
pub(crate) unsafe extern "system" fn get_device(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    let device = head(this).common.ctx.device.load(Ordering::Relaxed);
    if device.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the device is a COM-shaped object whose slot 0 is QueryInterface.
    unsafe {
        let qi: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT = std::mem::transmute(*(*device.cast::<*const *const ()>()));
        qi(device, iid, out)
    }
}

/// The eight common slots (IUnknown, ID3D12Object, GetDevice) for object `O`, with its release.
pub(crate) fn common_slots<const O: usize>(release: unsafe extern "system" fn(Raw) -> u32) -> Vec<(usize, &'static str, Fp)> {
    vec![
        (0x00, "QueryInterface", Fp(query::<O> as *const ())),
        (0x08, "AddRef", Fp(add_ref_method as *const ())),
        (0x10, "Release", Fp(release as *const ())),
        (0x18, "GetPrivateData", Fp(get_private_data as *const ())),
        (0x20, "SetPrivateData", Fp(set_private_data as *const ())),
        (0x30, "SetName", Fp(set_name as *const ())),
        (0x38, "GetDevice", Fp(get_device as *const ())),
    ]
}

/// Release for a boxed object type `T` (whose first field is [`Head`]).
pub(crate) unsafe fn release_boxed<T>(this: Raw) -> u32 {
    let left = head(this).header.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: made by `Box::into_raw` of a `T`; this was the last reference.
        drop(unsafe { Box::from_raw(this.cast::<T>()) });
    }
    left
}

/// A struct return: MSVC C++ passes `(this, out)` and expects `out` back; the fork declared
/// some as `(out, this)`. Which register holds `this` is told by its table pointer, so both
/// orders work (the first call of each is reported to [`crate::Events::note`]).
pub(crate) fn this_and_out<T>(a: Raw, b: Raw, object: Object) -> (Raw, *mut T) {
    static SEEN: [std::sync::atomic::AtomicBool; 2] = [std::sync::atomic::AtomicBool::new(false), std::sync::atomic::AtomicBool::new(false)];
    let (this, out, k) = if is(a, object) { (a, b, 0) } else { (b, a, 1) };
    if !SEEN[k].swap(true, Ordering::Relaxed) {
        head(this).common.ctx.events.note(if k == 0 { "struct return (this, out)" } else { "struct return (out, this)" }, 0);
    }
    (this, out.cast())
}
