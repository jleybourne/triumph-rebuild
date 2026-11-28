//! The title's own memory, read and written by the GPU in place.
//!
//! The title gives the GPU addresses of memory it allocated itself (graphics `XMemAlloc` / `XMemVirtualAlloc`
//! regions, physical pages it mapped): vertex buffers, root views, constant buffers, labels it polls. Until the
//! lead's memory model joins (tiles and the upload ring, `crates/uma`), no [`rebuild_d3d12x::Memory`] knew those
//! addresses, so every such vertex buffer, root view and label was refused: the loading screen's rectangles read
//! their vertices from there, and its labels went unwritten.
//!
//! Here an address the resource memory does not hold is looked up with `VirtualQuery`; the whole allocation that
//! holds it (one `VirtualAlloc`, or one mapped view) is opened as a D3D12 heap (`OpenExistingHeapFromAddress`,
//! measured in Build 2: `crates/uma/src/labels.rs`, `gputest --labels`) with one buffer placed over all of it.
//! The range is then the allocation: CPU address = console address, the buffer's GPU address for the host. Each
//! allocation is opened once and kept while it stays the same allocation (same base and size); one that cannot
//! be opened is remembered and not tried again. The GPU reads such memory over the bus: correct, not fast. The
//! tiles and the ring replace this for the bulk of the title's memory when they join.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Memory::*;

use rebuild_xlist::console::MemoryRange;

/// An allocation of the title's, as the GPU sees it.
struct Opened {
    bytes: u64,
    /// the whole allocation's size as its descriptor has it (a different one: freed and allocated again)
    alloc: u64,
    /// `None`: it could not be opened (kept so it is not tried again)
    gpu: Option<(ID3D12Heap, ID3D12Resource, u64)>,
    /// the [`GENERATION`] it was last confirmed in: while nothing was freed, decommitted or remapped since, a
    /// lookup inside it answers from here with no query of the address space
    checked: u64,
}

/// Bumped whenever memory the title's allocations could live in is freed, decommitted or remapped
/// ([`changed`]): an opened allocation confirmed in the current generation is still the same allocation.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The title freed, decommitted or remapped memory (`VirtualFree`, physical pages mapped or unmapped): every
/// opened allocation is confirmed again at its next lookup. A few relaxed adds a frame at most; a lookup between
/// two changes skips the address-space query (`QueryVirtualMemoryInformation`, and `VirtualQuery` for mapped views,
/// 112 µs a GPU-timestamp label on the card, `runs/build6-lists.md`).
pub fn changed() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Lookups answered without a query of the address space, and those that queried it.
pub static CACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static QUERIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct TitleMemory {
    device: ID3D12Device3,
    opened: Mutex<BTreeMap<u64, Opened>>,
}

// SAFETY: D3D12 objects are free-threaded; the map is locked.
unsafe impl Send for TitleMemory {}
unsafe impl Sync for TitleMemory {}

/// The allocation holding `va`: its base and its committed size (`None` if `va` is not committed memory the CPU
/// can read and write). A fully committed allocation answers from its descriptor (µs); a partly committed one
/// walks its regions with `VirtualQuery` (which costs ms on a large region: the title's are fully committed).
pub fn allocation(va: u64) -> Option<(u64, u64)> {
    let a = rebuild_d3d12x::vmem::allocation_of(va)?;
    if a.uncommitted() {
        return None;
    }
    if a.fully_committed() && writable(PAGE_PROTECTION_FLAGS(a.protect)) {
        return Some((a.base, a.size));
    }
    walked_allocation(va)
}

/// The allocation holding `va` by walking its regions with `VirtualQuery` (exact; ms on a large region).
fn walked_allocation(va: u64) -> Option<(u64, u64)> {
    let mut m = MEMORY_BASIC_INFORMATION::default();
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if unsafe { VirtualQuery(Some(va as *const c_void), &mut m, size) } == 0 || m.State != MEM_COMMIT || !writable(PAGE_PROTECTION_FLAGS(rebuild_d3d12x::vmem::effective(va, m.Protect.0))) {
        return None;
    }
    let base = m.AllocationBase as u64;
    // the allocation's extent: every region after its base that belongs to it
    let mut at = base;
    loop {
        let mut r = MEMORY_BASIC_INFORMATION::default();
        if unsafe { VirtualQuery(Some(at as *const c_void), &mut r, size) } == 0 || r.AllocationBase as u64 != base || r.State != MEM_COMMIT {
            break;
        }
        at = r.BaseAddress as u64 + r.RegionSize as u64;
    }
    (at > va).then_some((base, at - base))
}

fn writable(p: PAGE_PROTECTION_FLAGS) -> bool {
    let p = p.0 & 0xff;
    p == PAGE_READWRITE.0 || p == PAGE_EXECUTE_READWRITE.0 || p == PAGE_WRITECOPY.0 || p == PAGE_EXECUTE_WRITECOPY.0
}

impl TitleMemory {
    pub fn new(device: &ID3D12Device) -> Option<TitleMemory> {
        Some(TitleMemory { device: device.cast().ok()?, opened: Mutex::new(BTreeMap::new()) })
    }

    /// The title's allocation holding `va`, opened for the GPU.
    pub fn range(&self, va: u64) -> Option<MemoryRange> {
        let r = self.lookup(va);
        // REBUILD_TITLEMEM_CHECK=1: every answer against the exact VirtualQuery walk (slow: for diagnosis)
        static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *CHECK.get_or_init(|| std::env::var("REBUILD_TITLEMEM_CHECK").is_ok_and(|v| v == "1")) {
            use std::sync::atomic::Ordering::Relaxed;
            let n = CHECKED.fetch_add(1, Relaxed) + 1;
            if n.is_power_of_two() && n >= 1024 {
                eprintln!("titlemem  check: {n} answers, {} differ from the VirtualQuery walk", MISMATCHED.load(Relaxed));
            }
            let fast = r.as_ref().map(|m| (m.console, m.bytes));
            let walked = walked_allocation(va);
            // an allocation that could not be opened answers None on both paths: compare only what was answered
            if fast.is_some() && fast != walked || fast.is_none() && walked.is_some_and(|(b, _)| self.opened.lock().unwrap_or_else(|e| e.into_inner()).get(&b).is_none_or(|o| o.gpu.is_some())) {
                if MISMATCHED.fetch_add(1, Relaxed) < 16 {
                    eprintln!("titlemem  check {va:#x}: answered {fast:x?}, VirtualQuery walk {walked:x?}, descriptor {:x?}", rebuild_d3d12x::vmem::allocation_of(va));
                }
            }
        }
        r
    }

    fn lookup(&self, va: u64) -> Option<MemoryRange> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        // fastest: an allocation confirmed since the last free, decommit or remap that covers `va`
        let generation = GENERATION.load(Acquire);
        {
            let opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((&base, o)) = opened.range(..=va).next_back() {
                if o.checked == generation && va - base < o.bytes {
                    CACHED.fetch_add(1, Relaxed);
                    let (_, buffer, gpu_va) = o.gpu.as_ref()?;
                    return Some(MemoryRange { console: base, bytes: o.bytes, host_va: *gpu_va, resource: buffer.as_raw(), resource_offset: 0 });
                }
            }
        }
        QUERIED.fetch_add(1, Relaxed);
        // fast path: the allocation's descriptor (3 µs, never VirtualQuery: labels come here every frame) names it;
        // an opened one of the same size that still covers `va` is used as is
        let a = rebuild_d3d12x::vmem::allocation_of(va)?;
        if a.uncommitted() {
            return None;
        }
        let alloc_base = a.base;
        {
            let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(o) = opened.get_mut(&alloc_base).filter(|o| o.alloc == a.size && va - alloc_base < o.bytes) {
                o.checked = generation;
                let (_, buffer, gpu_va) = o.gpu.as_ref()?;
                return Some(MemoryRange { console: alloc_base, bytes: o.bytes, host_va: *gpu_va, resource: buffer.as_raw(), resource_offset: 0 });
            }
        }
        let (base, bytes) = allocation(va)?;
        let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
        if opened.get(&base).is_none_or(|o| o.bytes != bytes || o.alloc != a.size) {
            let gpu = self.open(base, bytes);
            if gpu.is_none() {
                OPEN_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // a stale entry for the same base (the title freed it and allocated again) is released here; work
            // already recorded holds its own references
            opened.insert(base, Opened { bytes, alloc: a.size, gpu, checked: generation });
        }
        let o = opened.get_mut(&base)?;
        o.checked = generation;
        let (_, buffer, gpu_va) = o.gpu.as_ref()?;
        Some(MemoryRange { console: base, bytes: o.bytes, host_va: *gpu_va, resource: buffer.as_raw(), resource_offset: 0 })
    }

    fn open(&self, base: u64, bytes: u64) -> Option<(ID3D12Heap, ID3D12Resource, u64)> {
        let heap: ID3D12Heap = match unsafe { self.device.OpenExistingHeapFromAddress(base as *const c_void) } {
            Ok(h) => h,
            Err(e) => {
                if OPEN_FAILED.load(std::sync::atomic::Ordering::Relaxed) < 4 {
                    eprintln!("titlemem  {base:#x} ({bytes:#x} bytes) cannot be opened for the GPU: {e}");
                }
                return None;
            }
        };
        let hd = unsafe { heap.GetDesc() };
        let size = hd.SizeInBytes.min(bytes);
        // a resource in a cross-adapter heap must say so; a heap that is not one takes none (the flag costs)
        let cross = if hd.Flags.0 & D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER.0 != 0 { D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER } else { D3D12_RESOURCE_FLAG_NONE };
        let buffer = |flags: D3D12_RESOURCE_FLAGS| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: flags,
                ..Default::default()
            };
            let mut r: Option<ID3D12Resource> = None;
            unsafe { self.device.CreatePlacedResource(&heap, 0, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r) }.ok().and(r)
        };
        // shaders may write it through root UAVs; a heap that refuses that still serves reads and labels
        let r = buffer(cross | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS).or_else(|| buffer(cross))?;
        let va = unsafe { r.GetGPUVirtualAddress() };
        Some((heap, r, va))
    }
}

/// Allocations opened for the GPU, and those that could not be.
pub static OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static OPEN_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// REBUILD_TITLEMEM_CHECK: answers compared, and how many differed
pub static CHECKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MISMATCHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_inside_an_allocation_gives_the_whole_allocation() {
        let bytes = 3 * 65536;
        let p = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as u64;
        assert_ne!(p, 0);
        assert_eq!(allocation(p + 70_000), Some((p, bytes as u64)));
        assert_eq!(allocation(p), Some((p, bytes as u64)));
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
        assert_eq!(allocation(p + 70_000), None, "freed memory is no allocation");
    }

    #[test]
    fn read_only_or_reserved_memory_is_not_opened() {
        let p = unsafe { VirtualAlloc(None, 65536 * 2, MEM_RESERVE, PAGE_READWRITE) } as u64;
        assert_eq!(allocation(p), None, "reserved, not committed");
        // commit the first page only: the allocation is that page
        unsafe { VirtualAlloc(Some(p as *const c_void), 65536, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(allocation(p + 10), Some((p, 65536)));
        assert_eq!(allocation(p + 65536 + 10), None);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe { VirtualProtect(p as *const c_void, 65536, PAGE_READONLY, &mut old).unwrap() };
        assert_eq!(allocation(p + 10), None, "the GPU may write labels there");
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
    }

    #[test]
    fn a_title_allocation_opens_as_a_gpu_buffer_on_warp() {
        // WARP only: the card is never used by tests run beside the title
        let Ok(factory) = (unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1::<windows::Win32::Graphics::Dxgi::IDXGIFactory4>() }) else { return };
        let Ok(adapter) = (unsafe { factory.EnumWarpAdapter::<windows::core::IUnknown>() }) else { return };
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
            return;
        }
        let m = TitleMemory::new(&device.unwrap()).expect("ID3D12Device3");
        let bytes = 2 * 65536;
        let p = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as u64;
        let r = m.range(p + 100).expect("opened");
        assert_eq!((r.console, r.bytes), (p, bytes as u64));
        assert_eq!(r.map(p + 100), Some(r.host_va + 100));
        assert!(!r.resource.is_null());
        let again = m.range(p + 65_000).unwrap();
        assert_eq!((again.host_va, again.resource), (r.host_va, r.resource), "opened once");
        // confirmed: the next lookups answer from the opened allocation, with no query of the address space
        // (other tests may free memory meanwhile, which only sends a lookup to the query: retried)
        use std::sync::atomic::Ordering::Relaxed;
        let cached = (0..20).any(|_| {
            let c = CACHED.load(Relaxed);
            assert_eq!(m.range(p + 8).map(|x| x.host_va), Some(r.host_va));
            CACHED.load(Relaxed) > c
        });
        assert!(cached, "answered from the opened allocation");
        // a free anywhere: the next lookup queries again (and finds the same allocation, still there)
        changed();
        let q1 = QUERIED.load(Relaxed);
        assert_eq!(m.range(p + 8).map(|x| x.host_va), Some(r.host_va));
        assert!(QUERIED.load(Relaxed) > q1, "queried after a change");
        // outside every opened allocation: never answered from the cache
        assert!(m.range(p + bytes as u64 + 16).is_none_or(|x| x.console != p));
        assert!(m.range(0x10).is_none());
        drop(m);
        // cleanup only: WARP may still hold the heap it opened over the memory for a moment (the free then fails
        // with E_INVALIDARG, seen once in a full gate); a leak of 128 KiB in a test process is harmless
        for _ in 0..50 {
            if unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE) }.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The physical pages' case (the regression that made #64 render black): a frame of a pagefile section mapped
    /// into a placeholder is its own allocation, and its descriptor counts no commit of its own (the section holds
    /// it). It must still be found, whole, as `VirtualQuery` finds it.
    #[test]
    fn a_physical_frame_mapped_into_a_placeholder_is_an_allocation() {
        use crate::physical::{reserve_placeholder, Physical, FRAME};
        let mut p = Physical::default();
        let frames = p.allocate(2).unwrap();
        let base = reserve_placeholder(4 * FRAME);
        assert_ne!(base, 0);
        assert!(p.map(base + FRAME as u64, frames[0]));
        let at = base + FRAME as u64;
        unsafe { ((at + 100) as *mut u32).write_volatile(7) };
        let d = rebuild_d3d12x::vmem::allocation_of(at + 10).expect("described");
        println!("mapped frame: {d:?}");
        assert_eq!(allocation(at + 10), Some((at, FRAME as u64)), "the frame, whole");
        assert_eq!(rebuild_d3d12x::vmem::committed_protect(at + 8, 8).map(|p| p & 0xff), Some(PAGE_READWRITE.0), "readable for waits");
        assert_eq!(allocation(base + 10), None, "the placeholder beside it is no memory");
        p.unmap(at);
        assert!(p.free(&frames));
    }
}

