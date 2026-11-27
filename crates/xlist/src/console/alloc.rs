//! The command allocator, and the upload memory list slot 0x4a0 hands out.
//!
//! List 0x4a0 `(size, alignment) -> pointer` is memory the CPU writes and the GPU reads at the
//! same address (the title's BVH builder writes instance descriptions there). On the console the
//! list owns it until reset. Here the **allocator** owns it until *its* reset, because D3D12 only
//! lets an allocator be reset once the GPU has finished every list recorded on it, while a list
//! may be reset as soon as it is submitted: memory freed at list reset could still be read by the
//! GPU (the fork freed it at list reset). The memory is host upload heap, persistently mapped,
//! carved from chunks that are kept and reused at every reset, so a frame allocates nothing.
//!
//! The pointer handed out is the title's "GPU address" for that memory too; every list resolves
//! such addresses through its allocator's chunks after its other [`MemoryRange`]s miss.

use std::ffi::c_void;
use std::mem::offset_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use super::slots::{self, Fp};
use super::{Backend, Header, ListHost, MemoryRange, Raw};

/// One block of upload memory: mapped at `cpu`, read by the GPU at `gpu`.
pub(crate) struct UploadChunk {
    pub resource: Option<ID3D12Resource>,
    pub cpu: *mut u8,
    pub gpu: u64,
    pub bytes: u64,
    /// The stand-in's memory (unit tests without a GPU).
    #[allow(dead_code)]
    pub keep: Option<Box<[u8]>>,
}
// SAFETY: the mapping lives as long as the resource; it is written by one recording thread at a time.
unsafe impl Send for UploadChunk {}

impl UploadChunk {
    pub fn on_device(device: &ID3D12Device, bytes: u64) -> windows::core::Result<UploadChunk> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut r,
            )?;
            let r = r.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let mut p: *mut c_void = std::ptr::null_mut();
            r.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p))?;
            let gpu = r.GetGPUVirtualAddress();
            Ok(UploadChunk { resource: Some(r), cpu: p.cast(), gpu, bytes, keep: None })
        }
    }

    fn range(&self) -> MemoryRange {
        MemoryRange {
            console: self.cpu as u64,
            bytes: self.bytes,
            host_va: self.gpu,
            resource: self.resource.as_ref().map_or(std::ptr::null_mut(), |r| r.as_raw()),
            resource_offset: 0,
        }
    }
}

const CHUNK: u64 = 1 << 20;

#[derive(Default)]
pub(crate) struct Uploads {
    chunks: Vec<UploadChunk>,
    /// the chunk being carved, and bytes used in it
    current: usize,
    used: u64,
}

impl Uploads {
    fn alloc(&mut self, backend: &dyn Backend, size: u64, alignment: u64) -> *mut u8 {
        let align = alignment.max(16);
        if !align.is_power_of_two() || size == 0 || size > (1 << 32) {
            return std::ptr::null_mut();
        }
        loop {
            if let Some(c) = self.chunks.get(self.current) {
                let at = (c.cpu as u64 + self.used).next_multiple_of(align) - c.cpu as u64;
                if at + size <= c.bytes {
                    self.used = at + size;
                    return unsafe { c.cpu.add(at as usize) };
                }
                if self.current + 1 < self.chunks.len() {
                    self.current += 1;
                    self.used = 0;
                    continue;
                }
            }
            let bytes = (size + align).next_multiple_of(1 << 16).max(CHUNK);
            match backend.upload(bytes) {
                Ok(c) => {
                    self.chunks.push(c);
                    self.current = self.chunks.len() - 1;
                    self.used = 0;
                }
                Err(_) => return std::ptr::null_mut(),
            }
        }
    }

    fn reset(&mut self) {
        self.current = 0;
        self.used = 0;
    }

    fn find(&self, va: u64) -> Option<MemoryRange> {
        self.chunks.iter().map(UploadChunk::range).find(|r| r.map(va).is_some())
    }
}

/// The allocator as the title sees it.
#[repr(C)]
pub(crate) struct AllocatorObject {
    pub header: Header,
    /// list type, +0x10 (assumed by the fork from the queue's pattern)
    pub kind: u32,
    _pad: u32,
    pub ours: AllocatorState,
}

const _: () = assert!(offset_of!(AllocatorObject, kind) == 0x10);

pub(crate) struct AllocatorState {
    pub host: Arc<dyn ListHost>,
    pub backend: Arc<dyn Backend>,
    pub allocator: ID3D12CommandAllocator,
    pub uploads: Mutex<Uploads>,
    pub kind: u32,
    /// Host allocators behind this console one, with the list recording into each (0: none). The console lets
    /// several lists record into one allocator at once; a host allocator takes one recording list at a time
    /// (`Reset` fails with E_INVALIDARG otherwise, and the title asserts), so each open list gets its own.
    pub pool: Mutex<Vec<(ID3D12CommandAllocator, usize)>>,
}

impl AllocatorState {
    /// A host allocator for list `list` to record into: the one it already holds, a free one, or a new one.
    pub fn take(&self, list: usize) -> windows::core::Result<ID3D12CommandAllocator> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((a, _)) = pool.iter().find(|(_, h)| *h == list) {
            return Ok(a.clone());
        }
        if let Some(slot) = pool.iter_mut().find(|(_, h)| *h == 0) {
            slot.1 = list;
            return Ok(slot.0.clone());
        }
        let a = self.backend.allocator(self.kind)?;
        pool.push((a.clone(), list));
        Ok(a)
    }

    /// Hand what `from` holds to `to` (a list known only after it is made).
    pub fn rehold(&self, from: usize, to: usize) {
        for slot in self.pool.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().filter(|(_, h)| *h == from) {
            slot.1 = to;
        }
    }

    /// List `list` stopped recording: its host allocator is free for another list.
    pub fn release(&self, list: usize) {
        for slot in self.pool.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().filter(|(_, h)| *h == list) {
            slot.1 = 0;
        }
    }

    pub fn alloc(&self, size: u64, alignment: u64) -> *mut u8 {
        self.uploads.lock().unwrap_or_else(|e| e.into_inner()).alloc(&*self.backend, size, alignment)
    }
    pub fn find(&self, va: u64) -> Option<MemoryRange> {
        self.uploads.lock().unwrap_or_else(|e| e.into_inner()).find(va)
    }
}

/// Make an allocator for list type `kind` (0 direct, 2 compute, 3 copy) on `device`. `host` is
/// the lead's side; every list recorded on this allocator uses it. The pointer starts with one
/// reference; the title's `Release` (0x10) frees it.
pub fn create_allocator(kind: u32, device: &ID3D12Device, host: Arc<dyn ListHost>) -> windows::core::Result<Raw> {
    create_on(kind, super::backend_of(device), host)
}

pub(crate) fn create_on(kind: u32, backend: Arc<dyn Backend>, host: Arc<dyn ListHost>) -> windows::core::Result<Raw> {
    slots::start_clock();
    let allocator = backend.allocator(kind)?;
    let obj = Box::new(AllocatorObject {
        header: Header { vtable: slots::allocator_table().funcs.as_ptr(), bits: 0, refs: AtomicU32::new(1) },
        kind,
        _pad: 0,
        ours: AllocatorState { host, backend, pool: Mutex::new(vec![(allocator.clone(), 0)]), allocator, uploads: Mutex::new(Uploads::default()), kind },
    });
    Ok(Box::into_raw(obj).cast())
}

/// Ours, if `p` is an allocator we made.
pub(crate) fn state<'a>(p: Raw) -> Option<&'a AllocatorState> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the first word of any object the title passes is its table.
    let obj = p.cast::<AllocatorObject>();
    unsafe { std::ptr::eq((*obj).header.vtable, slots::allocator_table().funcs.as_ptr()).then(|| &(*obj).ours) }
}

pub(crate) fn host_of<'a>(p: Raw) -> Option<&'a Arc<dyn ListHost>> {
    state(p).map(|s| &s.host)
}

/// The host allocator behind a console allocator.
pub fn host_allocator<'a>(allocator: Raw) -> Option<&'a ID3D12CommandAllocator> {
    state(allocator).map(|s| &s.allocator)
}

/// The host range of upload memory (list 0x4a0) holding `va` in `allocator`'s chunks, for
/// resolving such an address outside a list.
pub fn upload_range(allocator: Raw, va: u64) -> Option<MemoryRange> {
    state(allocator)?.find(va)
}

// ---- slots ----

const IIDS: [GUID; 5] = [
    ID3D12CommandAllocator::IID,
    ID3D12Pageable::IID,
    ID3D12DeviceChild::IID,
    ID3D12Object::IID,
    windows::core::IUnknown::IID,
];

unsafe extern "system" fn query(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        if !iid.is_null() && IIDS.contains(&*iid) {
            add_ref(this);
            *out = this;
            return S_OK;
        }
        *out = std::ptr::null_mut();
        if let (Some(h), false) = (host_of(this), iid.is_null()) {
            h.unknown_iid("allocator", &*iid);
        }
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: Raw) -> u32 {
    unsafe { (*this.cast::<Header>()).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: Raw) -> u32 {
    let left = unsafe { (*this.cast::<Header>()).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this.cast::<AllocatorObject>()) });
    }
    left
}

unsafe extern "system" fn set_name(_: Raw, _name: *const u16) -> HRESULT {
    S_OK
}

unsafe extern "system" fn get_device(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    match host_of(this) {
        Some(h) => unsafe { super::list::device_query(h.device(), iid, out) },
        None => E_INVALIDARG,
    }
}

/// 0x40 Reset: the host allocator's, and the upload chunks are reused from the start.
unsafe extern "system" fn reset(this: Raw) -> HRESULT {
    let Some(s) = state(this) else { return E_INVALIDARG };
    // every host allocator behind it (the title resets once the GPU is done with all of its lists)
    let pool: Vec<ID3D12CommandAllocator> = s.pool.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(a, _)| a.clone()).collect();
    for a in pool {
        if let Err(e) = unsafe { a.Reset() } {
            return e.code();
        }
    }
    s.uploads.lock().unwrap_or_else(|e| e.into_inner()).reset();
    S_OK
}

/// 0x48: the console's allocation callback; accepted and ignored (as the fork did).
unsafe extern "system" fn callback(_: Raw) -> HRESULT {
    S_OK
}

pub(crate) static IMPLEMENTED: &[(usize, &str, Fp)] = &[
    (0x00, "QueryInterface", Fp(query as *const ())),
    (0x08, "AddRef", Fp(add_ref as *const ())),
    (0x10, "Release", Fp(release as *const ())),
    (0x30, "SetName", Fp(set_name as *const ())),
    (0x38, "GetDevice", Fp(get_device as *const ())),
    (0x40, "Reset", Fp(reset as *const ())),
    (0x48, "allocation callback (console)", Fp(callback as *const ())),
];
