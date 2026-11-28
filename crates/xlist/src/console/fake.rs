//! A stand-in for the host's D3D12 objects, for tests without a GPU: a COM-shaped object whose
//! table counts the calls to each slot and keeps their first three integer arguments. It answers
//! every interface id with itself, so one object is an allocator or any version of a list.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D12::*;

use super::alloc::UploadChunk;
use super::slots::Fp;
use super::{Backend, Raw};

pub const SLOTS: usize = 128;

#[repr(C)]
pub struct Fake {
    vtable: *const Fp,
    refs: AtomicU32,
    pub calls: [AtomicU64; SLOTS],
    pub args: [[AtomicU64; 3]; SLOTS],
    /// the last call's first three integer arguments
    pub last: [[AtomicU64; 3]; SLOTS],
    /// `Set{Compute,Graphics}Root32BitConstants`: (host slot, parameter, the words, offset), every call while
    /// `keep_words` (off for the benches: a lock and a vector are not the host's cost)
    pub words: Mutex<Vec<(usize, u32, Vec<u32>, u32)>>,
    pub keep_words: AtomicBool,
    /// The console list recording into this one ([`host_of`]): reading a count first translates what that list
    /// keeps as records ([`super::mode`]), so a test sees the host list as the title's calls so far made it.
    pub owner: AtomicUsize,
}

impl Fake {
    pub fn new() -> Raw {
        let f = Box::new(Fake {
            vtable: TABLE.as_ptr(),
            refs: AtomicU32::new(1),
            calls: [const { AtomicU64::new(0) }; SLOTS],
            args: [const { [const { AtomicU64::new(0) }; 3] }; SLOTS],
            last: [const { [const { AtomicU64::new(0) }; 3] }; SLOTS],
            words: Mutex::new(Vec::new()),
            keep_words: AtomicBool::new(false),
            owner: AtomicUsize::new(0),
        });
        Box::into_raw(f).cast()
    }

    /// The stand-in behind a host interface pointer.
    pub fn of<'a>(p: Raw) -> &'a Fake {
        unsafe { &*p.cast::<Fake>() }
    }

    /// Translate what the owning console list keeps as records.
    pub fn sync(&self) {
        let o = self.owner.load(Ordering::Relaxed);
        if o != 0 {
            let _ = super::stats(o as Raw);
        }
    }

    pub fn count(&self, slot: usize) -> u64 {
        self.sync();
        self.calls[slot].load(Ordering::Relaxed)
    }

    pub fn arg(&self, slot: usize, i: usize) -> u64 {
        self.sync();
        self.args[slot][i].load(Ordering::Relaxed)
    }

    pub fn last(&self, slot: usize, i: usize) -> u64 {
        self.sync();
        self.last[slot][i].load(Ordering::Relaxed)
    }
}

unsafe extern "system" fn qi(this: Raw, _iid: *const GUID, out: *mut Raw) -> HRESULT {
    unsafe {
        (*this.cast::<Fake>()).refs.fetch_add(1, Ordering::Relaxed);
        *out = this;
    }
    HRESULT(0)
}
unsafe extern "system" fn add_ref(this: Raw) -> u32 {
    unsafe { (*this.cast::<Fake>()).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}
unsafe extern "system" fn release(this: Raw) -> u32 {
    let left = unsafe { (*this.cast::<Fake>()).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this.cast::<Fake>()) });
    }
    left
}
unsafe extern "system" fn slot<const N: usize>(this: Raw, a: u64, b: u64, c: u64) -> u64 {
    let f = unsafe { &*this.cast::<Fake>() };
    if f.calls[N].fetch_add(1, Ordering::Relaxed) == 0 {
        f.args[N][0].store(a, Ordering::Relaxed);
        f.args[N][1].store(b, Ordering::Relaxed);
        f.args[N][2].store(c, Ordering::Relaxed);
    }
    f.last[N][0].store(a, Ordering::Relaxed);
    f.last[N][1].store(b, Ordering::Relaxed);
    f.last[N][2].store(c, Ordering::Relaxed);
    0
}

/// `Set*Root32BitConstants(param, count, data, offset)`: counted, and the words kept.
unsafe extern "system" fn constants<const N: usize>(this: Raw, param: u32, n: u32, data: *const u32, offset: u32) {
    unsafe { slot::<N>(this, param.into(), n.into(), data as u64) };
    let f = unsafe { &*this.cast::<Fake>() };
    if !f.keep_words.load(Ordering::Relaxed) {
        return;
    }
    let w = if data.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(data, n as usize) }.to_vec() };
    f.words.lock().unwrap().push((N, param, w, offset));
}

macro_rules! table {
    ($($n:literal)*) => {
        static SLOT_FNS: [Fp; SLOTS] = [$(Fp(slot::<$n> as *const ())),*];
    };
}
table!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
    40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77
    78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111
    112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127);

static TABLE: std::sync::LazyLock<Vec<Fp>> = std::sync::LazyLock::new(|| {
    let mut t = SLOT_FNS.to_vec();
    t[0] = Fp(qi as *const ());
    t[1] = Fp(add_ref as *const ());
    t[2] = Fp(release as *const ());
    t[host::SET_COMPUTE_ROOT_CONSTANTS] = Fp(constants::<{ host::SET_COMPUTE_ROOT_CONSTANTS }> as *const ());
    t[host::SET_GRAPHICS_ROOT_CONSTANTS] = Fp(constants::<{ host::SET_GRAPHICS_ROOT_CONSTANTS }> as *const ());
    t
});

/// Host slot numbers in `ID3D12GraphicsCommandList7`'s table (IUnknown 3, object 4, device
/// child 1, command list 1, then the graphics list's methods in PC order).
pub mod host {
    pub const CLOSE: usize = 9;
    pub const RESET: usize = 10;
    pub const DRAW_INSTANCED: usize = 12;
    pub const DRAW_INDEXED: usize = 13;
    pub const DISPATCH: usize = 14;
    pub const COPY_BUFFER_REGION: usize = 15;
    pub const SET_PIPELINE_STATE: usize = 25;
    pub const RESOURCE_BARRIER: usize = 26;
    pub const SET_DESCRIPTOR_HEAPS: usize = 28;
    pub const SET_COMPUTE_ROOT_SIGNATURE: usize = 29;
    pub const SET_GRAPHICS_ROOT_SIGNATURE: usize = 30;
    pub const SET_COMPUTE_ROOT_TABLE: usize = 31;
    pub const SET_GRAPHICS_ROOT_TABLE: usize = 32;
    pub const SET_COMPUTE_ROOT_CONSTANTS: usize = 35;
    pub const SET_GRAPHICS_ROOT_CONSTANTS: usize = 36;
    pub const SET_GRAPHICS_ROOT_CBV: usize = 38;
    pub const SET_COMPUTE_ROOT_SRV: usize = 39;
    pub const SET_GRAPHICS_ROOT_SRV: usize = 40;
    pub const OM_SET_RENDER_TARGETS: usize = 46;
    pub const CLEAR_RTV: usize = 48;
    pub const EXECUTE_INDIRECT: usize = 59;
    pub const WRITE_BUFFER_IMMEDIATE: usize = 66;
    pub const BARRIER: usize = 80;
}

#[derive(Default)]
pub struct FakeBackend {
    pub lists: AtomicUsize,
    /// lists it makes keep the root-constant words they are given
    pub keep_words: bool,
}

impl Backend for FakeBackend {
    fn allocator(&self, _kind: u32) -> windows::core::Result<ID3D12CommandAllocator> {
        Ok(unsafe { ID3D12CommandAllocator::from_raw(Fake::new()) })
    }
    fn list(&self, _kind: u32, _allocator: &ID3D12CommandAllocator) -> windows::core::Result<ID3D12GraphicsCommandList> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        let f = Fake::new();
        Fake::of(f).keep_words.store(self.keep_words, Ordering::Relaxed);
        Ok(unsafe { ID3D12GraphicsCommandList::from_raw(f) })
    }
    fn upload(&self, bytes: u64) -> windows::core::Result<UploadChunk> {
        let mut keep = vec![0u8; bytes as usize].into_boxed_slice();
        let cpu = keep.as_mut_ptr();
        Ok(UploadChunk { resource: None, cpu, gpu: cpu as u64 ^ (1 << 46), bytes, keep: Some(keep) })
    }
    fn enhanced_barriers(&self) -> bool {
        true
    }
}

pub fn backend() -> Arc<FakeBackend> {
    Arc::new(FakeBackend::default())
}

/// A backend whose lists keep the root-constant words they are given.
pub fn backend_keeping_words() -> Arc<FakeBackend> {
    Arc::new(FakeBackend { keep_words: true, ..Default::default() })
}

/// The stand-in behind a list's current host list.
pub fn host_of(list: Raw) -> &'static Fake {
    let seg = super::segments(list).expect("ours");
    let f = Fake::of(seg.last().expect("a segment").list.list.as_raw());
    f.owner.store(list as usize, Ordering::Relaxed);
    f
}
