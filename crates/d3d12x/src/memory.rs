//! Console memory, as this crate needs it from the lead's memory model.
//!
//! Plan decision 1: a console address is a GPU address. The lead's model (Build 1 tiles at
//! console addresses, the Build 2 ring for what the CPU writes, physical pages read in place)
//! answers [`Memory`]; this crate never decides where memory lives. [`HostMemory`] is a stand-in
//! on one upload heap (CPU address = console address, the GPU reads it in place), enough for the
//! GPU test and for wiring before the tiles are joined up.

use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::format::{HeapProperties, KIB_64};
use crate::MemoryRange;

/// The lead's memory model, as resources and views need it.
pub trait Memory: Send + Sync {
    /// Console memory for a committed resource or a heap: `bytes` at `alignment` (a power of
    /// two, at least 64 KiB), CPU-addressable at the returned address, which is also its GPU
    /// address. `None`: out of memory.
    fn allocate(&self, bytes: u64, alignment: u64, heap: &HeapProperties) -> Option<u64>;
    /// Give back what [`Memory::allocate`] returned.
    fn free(&self, address: u64, bytes: u64);
    /// The range of console GPU memory holding `va` (`ListHost::memory`; every buffer view is
    /// made from it). As large as possible: callers cache the last one.
    fn range(&self, va: u64) -> Option<MemoryRange>;
    /// The range holding `va` as the CPU sees it (labels and timestamps the GPU writes for the title's CPU to
    /// poll). Defaults to [`Memory::range`]; a model that keeps a card copy of CPU memory answers the CPU's.
    fn visible(&self, va: u64) -> Option<MemoryRange> {
        self.range(va)
    }
    /// A host buffer whose byte 0 is console `address`, sharing its bytes (copies and barriers
    /// name it). `None`: none can be made (no GPU, or an address the model cannot alias).
    fn buffer(&self, address: u64, bytes: u64) -> Option<ID3D12Resource>;
    /// A host texture for a console texture whose memory is at `address`, if the model keeps
    /// textures at their addresses (tiles); `None`: this crate makes a committed one.
    fn texture(&self, _address: u64, _desc: &D3D12_RESOURCE_DESC) -> Option<ID3D12Resource> {
        None
    }
}

/// A stand-in [`Memory`]: one upload heap, mapped, with one buffer over all of it. Console
/// addresses are its CPU addresses; buffers are placed resources in the same heap (64 KiB
/// aligned), so a placed buffer and the whole-heap buffer share bytes. Allocation is a bump;
/// `free` keeps the bytes (a stand-in for tests and bring-up, not the memory model).
pub struct HostMemory {
    heap: ID3D12Heap,
    whole: ID3D12Resource,
    device: ID3D12Device,
    base: u64,
    bytes: u64,
    va: u64,
    next: Mutex<u64>,
}
// SAFETY: D3D12 objects are free-threaded; the bump is locked.
unsafe impl Send for HostMemory {}
unsafe impl Sync for HostMemory {}

impl HostMemory {
    pub fn new(device: &ID3D12Device, bytes: u64) -> windows::core::Result<HostMemory> {
        let bytes = bytes.next_multiple_of(KIB_64);
        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            device.CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: bytes,
                    Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
                    Alignment: KIB_64,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                },
                &mut heap,
            )
        }?;
        let heap = heap.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        let whole = placed(device, &heap, 0, bytes)?;
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe { whole.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)) }?;
        let va = unsafe { whole.GetGPUVirtualAddress() };
        Ok(HostMemory { heap, whole, device: device.clone(), base: p as u64, bytes, va, next: Mutex::new(0) })
    }

    pub fn base(&self) -> u64 {
        self.base
    }
}

fn placed(device: &ID3D12Device, heap: &ID3D12Heap, offset: u64, bytes: u64) -> windows::core::Result<ID3D12Resource> {
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreatePlacedResource(heap, offset, &crate::resource::buffer_desc(bytes, false), D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut r) }?;
    r.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY))
}

impl Memory for HostMemory {
    fn allocate(&self, bytes: u64, alignment: u64, _heap: &HeapProperties) -> Option<u64> {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let at = next.next_multiple_of(alignment.max(KIB_64));
        let end = at.checked_add(bytes.next_multiple_of(KIB_64))?;
        if end > self.bytes {
            return None;
        }
        *next = end;
        Some(self.base + at)
    }

    fn free(&self, _address: u64, _bytes: u64) {}

    fn range(&self, va: u64) -> Option<MemoryRange> {
        let off = va.checked_sub(self.base)?;
        (off < self.bytes).then(|| MemoryRange { console: self.base, bytes: self.bytes, host_va: self.va, resource: self.whole.as_raw(), resource_offset: 0 })
    }

    fn buffer(&self, address: u64, bytes: u64) -> Option<ID3D12Resource> {
        let off = address.checked_sub(self.base)?;
        if off % KIB_64 != 0 || off + bytes > self.bytes {
            return None;
        }
        placed(&self.device, &self.heap, off, bytes.max(1)).ok()
    }
}

/// A [`Memory`] with no GPU: host memory whose addresses are the console's, mapped 1:1 to
/// "host GPU addresses" with no resource (the unit tests' stand-in).
pub struct PlainMemory {
    base: *mut u8,
    bytes: u64,
    next: Mutex<u64>,
}
// SAFETY: the bump is locked; the bytes are the title's to share.
unsafe impl Send for PlainMemory {}
unsafe impl Sync for PlainMemory {}

impl PlainMemory {
    pub fn new(bytes: u64) -> PlainMemory {
        let layout = std::alloc::Layout::from_size_align(bytes as usize, KIB_64 as usize).expect("layout");
        // SAFETY: a non-zero layout.
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null());
        PlainMemory { base, bytes, next: Mutex::new(0) }
    }

    pub fn base(&self) -> u64 {
        self.base as u64
    }
}

impl Drop for PlainMemory {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.bytes as usize, KIB_64 as usize).expect("layout");
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.base, layout) };
    }
}

impl Memory for PlainMemory {
    fn allocate(&self, bytes: u64, alignment: u64, _heap: &HeapProperties) -> Option<u64> {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let at = next.next_multiple_of(alignment.max(KIB_64));
        let end = at.checked_add(bytes.next_multiple_of(KIB_64))?;
        if end > self.bytes {
            return None;
        }
        *next = end;
        Some(self.base as u64 + at)
    }
    fn free(&self, _address: u64, _bytes: u64) {}
    fn range(&self, va: u64) -> Option<MemoryRange> {
        let base = self.base as u64;
        let off = va.checked_sub(base)?;
        (off < self.bytes).then(|| MemoryRange { console: base, bytes: self.bytes, host_va: base, resource: std::ptr::null_mut(), resource_offset: 0 })
    }
    fn buffer(&self, _address: u64, _bytes: u64) -> Option<ID3D12Resource> {
        None
    }
}
