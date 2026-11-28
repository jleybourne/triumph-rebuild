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
    /// every free and remap of it is reported ([`changed`]): inside a reservation the title made through our
    /// overrides ([`crate::exports::title_reserved`]). Only these trust range invalidation ([`ranges_on`]); any
    /// other allocation (a host heap's, the CRT's, the APU heap's) is confirmed again after every change
    reported: bool,
}

/// Bumped whenever memory the title's allocations could live in is freed, decommitted or remapped
/// ([`changed`]), with the range it touched kept in [`CHANGES`]: an opened allocation confirmed in an earlier
/// generation is still the same allocation when none of the changes since then touched it.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The latest changes, oldest first: (the generation it made, its range `lo..hi`). Bounded; an allocation
/// confirmed before the oldest one kept queries the address space again.
static CHANGES: Mutex<std::collections::VecDeque<(u64, u64, u64)>> = Mutex::new(std::collections::VecDeque::new());
const CHANGES_KEPT: usize = 4096;

/// The title freed, decommitted or remapped `lo..hi` (`VirtualFree`, physical pages mapped or unmapped). Called
/// **after** the change: an opened allocation that overlaps the range is confirmed again at its next lookup; every
/// other one keeps answering from the cache (a lock and a short scan once per allocation and change, against the
/// address-space query: `QueryVirtualMemoryInformation`, and `VirtualQuery` for mapped views).
pub fn changed(lo: u64, hi: u64) {
    let mut c = CHANGES.lock().unwrap_or_else(|e| e.into_inner());
    // the generation moves under the lock: a lookup that read generation g finds every change up to g logged
    let g = GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
    if c.len() == CHANGES_KEPT {
        c.pop_front();
    }
    c.push_back((g, lo, hi.max(lo.saturating_add(1))));
    CHANGE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Everything may have changed: every opened allocation is confirmed again.
pub fn changed_all() {
    changed(0, u64::MAX);
}

/// No change since generation `since` touched `lo..hi` (known only while the log reaches back that far).
fn untouched(lo: u64, hi: u64, since: u64) -> bool {
    if !ranges_on() {
        return false;
    }
    let c = CHANGES.lock().unwrap_or_else(|e| e.into_inner());
    if c.front().is_none_or(|&(g, _, _)| g > since + 1) {
        return false;
    }
    c.iter().rev().take_while(|e| e.0 > since).all(|&(_, a, b)| b <= lo || a >= hi)
}

/// `REBUILD_TITLEMEM_RANGES=1`: a change confirms again only the opened allocations it touched (off: every one, as
/// before; tests turn it on with [`set_ranges`]). Off by default until a card run confirms it: the first card run
/// with it (and the list pool) ended in a GPU reset that was not isolated (`agents/listcost.md`).
pub fn ranges_on() -> bool {
    let v = RANGES.load(std::sync::atomic::Ordering::Relaxed);
    if v != 0 {
        return v == 1;
    }
    let on = std::env::var("REBUILD_TITLEMEM_RANGES").is_ok_and(|v| v.trim() == "1");
    RANGES.store(if on { 1 } else { 2 }, std::sync::atomic::Ordering::Relaxed);
    on
}

/// 0 unread, 1 on, 2 off.
static RANGES: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Turn range invalidation on or off ([`ranges_on`]).
pub fn set_ranges(on: bool) {
    RANGES.store(if on { 1 } else { 2 }, std::sync::atomic::Ordering::Relaxed);
}

/// Changes reported ([`changed`]).
pub static CHANGE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Lookups answered without a query of the address space, and those that queried it.
pub static CACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static QUERIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Of [`CACHED`]: an allocation confirmed in an earlier generation that no change since touched.
pub static UNTOUCHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Why a lookup queried the address space ([`WHY`], [`why_report`]).
pub mod why {
    /// an opened allocation over the address, touched by a change since it was confirmed: the same one again
    pub const CONFIRMED: usize = 0;
    /// opened over the address, but gone or changed: opened again (or refused)
    pub const REOPENED: usize = 1;
    /// no opened allocation over the address: opened now
    pub const NEW: usize = 2;
    /// free memory, nothing committed, or not writable
    pub const NONE: usize = 3;
    /// the address lies past the committed part of an opened allocation
    pub const PAST: usize = 4;
    pub const NAMES: [&str; 5] = ["confirmed", "reopened", "new", "no memory", "past committed"];
}
/// Lookups that queried, by [`why`], and the TSC ticks they took there.
pub static WHY: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];
pub static WHY_TICKS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// The lookups that queried, by reason, with their mean cost.
pub fn why_report() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let mut out = format!(
        "{} changes reported (ranges {}), {} cached lookups untouched by the changes since; sections {}: {} lookups through a section view, {} views opened; queried:",
        CHANGE_CALLS.load(Relaxed),
        if ranges_on() { "on" } else { "off" },
        UNTOUCHED.load(Relaxed),
        if sections_on() { "on" } else { "off" },
        SECTION_HITS.load(Relaxed),
        SECTIONS_OPENED.load(Relaxed)
    );
    for (i, n) in why::NAMES.iter().enumerate() {
        let c = WHY[i].load(Relaxed);
        if c > 0 {
            let ms = rebuild_xlist::console::cost::ms(WHY_TICKS[i].load(Relaxed));
            out += &format!(" {n} {c} ({:.1} µs each)", ms * 1000.0 / c as f64);
        }
    }
    out
}

/// `REBUILD_TITLEMEM_SECTIONS=1`: a physical frame the title maps is reached through its section's one whole view
/// (opened once as a heap), not opened at every address the title maps it at. Measured: the title's GPU-timestamp
/// labels sit in 64 KiB frames it maps afresh every few frames (0xb40000, 0xb60000, ... each label at +0xff20), so
/// nearly every lookup that queried was a new mapping opened as a heap of its own (0.4-2.5 ms each on the card).
pub fn sections_on() -> bool {
    static S: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("REBUILD_TITLEMEM_SECTIONS").is_ok_and(|v| v.trim() == "1"))
}

/// Where a mapped frame's bytes are in its section's whole view.
#[derive(Clone, Copy)]
struct FrameAt {
    id: u64,
    view: u64,
    bytes: u64,
    off: u64,
}

const FRAME: u64 = 64 << 10;

/// Mapped frames by console address ([`frame_mapped`]), and section views gone since the last lookup.
static FRAMES: Mutex<BTreeMap<u64, FrameAt>> = Mutex::new(BTreeMap::new());
static GONE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The title mapped a frame at `addr`: its bytes are at `off` in section view `id` (`bytes` long, at `view`).
pub fn frame_mapped(addr: u64, id: u64, view: u64, bytes: u64, off: u64) {
    FRAMES.lock().unwrap_or_else(|e| e.into_inner()).insert(addr, FrameAt { id, view, bytes, off });
}

/// The frame mapped at `addr` is unmapped.
pub fn frame_unmapped(addr: u64) {
    if sections_on() {
        FRAMES.lock().unwrap_or_else(|e| e.into_inner()).remove(&addr);
    }
}

/// Section view `id` is about to be unmapped: its heap is released at the next lookup (work recorded already holds
/// its own references).
pub fn section_gone(id: u64) {
    if sections_on() && id != 0 {
        GONE.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    }
}

/// Lookups answered through a section view, and section views opened.
pub static SECTION_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static SECTIONS_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `REBUILD_TITLEMEM_TRACE=1`: the first lookups that query, one line each (address, reason, µs, the allocation).
fn trace_on() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var("REBUILD_TITLEMEM_TRACE").is_ok_and(|v| v == "1"))
}

pub struct TitleMemory {
    device: ID3D12Device3,
    opened: Mutex<BTreeMap<u64, Opened>>,
    /// section views opened as heaps, by view number ([`sections_on`]); `None`: refused
    sections: Mutex<std::collections::HashMap<u64, Option<(ID3D12Heap, ID3D12Resource, u64)>>>,
    /// this one's lookups that queried the address space ([`QUERIED`] counts every instance's)
    queried: std::sync::atomic::AtomicU64,
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
        Some(TitleMemory { device: device.cast().ok()?, opened: Mutex::new(BTreeMap::new()), sections: Mutex::new(std::collections::HashMap::new()), queried: std::sync::atomic::AtomicU64::new(0) })
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

    /// A mapped physical frame through its section's whole view, opened once ([`sections_on`]).
    fn through_section(&self, va: u64) -> Option<Option<MemoryRange>> {
        let frame = va & !(FRAME - 1);
        let f = *FRAMES.lock().unwrap_or_else(|e| e.into_inner()).get(&frame)?;
        let mut sections = self.sections.lock().unwrap_or_else(|e| e.into_inner());
        for id in std::mem::take(&mut *GONE.lock().unwrap_or_else(|e| e.into_inner())) {
            sections.remove(&id);
        }
        let opened = sections.entry(f.id).or_insert_with(|| {
            let gpu = self.open(f.view, f.bytes);
            if gpu.is_some() {
                SECTIONS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            gpu
        });
        // a heap smaller than the view (never seen) leaves the frames past it to the per-mapping path
        let (_, buffer, gpu_va) = opened.as_ref()?;
        let size = unsafe { buffer.GetDesc() }.Width;
        if f.off + FRAME > size {
            return None;
        }
        SECTION_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(Some(MemoryRange { console: frame, bytes: FRAME, host_va: gpu_va + f.off, resource: buffer.as_raw(), resource_offset: f.off }))
    }

    fn lookup(&self, va: u64) -> Option<MemoryRange> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        if sections_on() {
            if let Some(r) = self.through_section(va) {
                return r;
            }
        }
        // fastest: an allocation over `va` confirmed since the last change that touched it
        let generation = GENERATION.load(Acquire);
        let mut hint = why::NEW;
        {
            let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((&base, o)) = opened.range_mut(..=va).next_back() {
                if va - base < o.bytes {
                    if o.checked != generation && o.reported && untouched(base, base + o.bytes.max(o.alloc), o.checked) {
                        o.checked = generation;
                        UNTOUCHED.fetch_add(1, Relaxed);
                    }
                    if o.checked == generation {
                        CACHED.fetch_add(1, Relaxed);
                        let (_, buffer, gpu_va) = o.gpu.as_ref()?;
                        return Some(MemoryRange { console: base, bytes: o.bytes, host_va: *gpu_va, resource: buffer.as_raw(), resource_offset: 0 });
                    }
                    hint = why::CONFIRMED;
                } else if va - base < o.alloc {
                    hint = why::PAST;
                }
            }
        }
        QUERIED.fetch_add(1, Relaxed);
        self.queried.fetch_add(1, Relaxed);
        let t0 = rebuild_xlist::console::cost::now();
        let (r, why) = self.query(va, generation, hint);
        let t = rebuild_xlist::console::cost::now().wrapping_sub(t0);
        WHY[why].fetch_add(1, Relaxed);
        WHY_TICKS[why].fetch_add(t, Relaxed);
        if trace_on() {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if N.fetch_add(1, Relaxed) < 64 {
                eprintln!(
                    "titlemem  query {va:#x}: {} in {:.1} µs; answered {:x?}; descriptor {:x?}",
                    why::NAMES[why],
                    rebuild_xlist::console::cost::ms(t) * 1000.0,
                    r.as_ref().map(|m| (m.console, m.bytes)),
                    rebuild_d3d12x::vmem::allocation_of(va)
                );
            }
        }
        r
    }

    /// The slow path: the address space asked (`hint`: what the cache saw), with the reason it ends with.
    fn query(&self, va: u64, generation: u64, hint: usize) -> (Option<MemoryRange>, usize) {
        // the allocation's descriptor (3 µs, never VirtualQuery: labels come here every frame) names it; an opened
        // one of the same size that still covers `va` is used as is
        let Some(a) = rebuild_d3d12x::vmem::allocation_of(va) else { return (None, why::NONE) };
        if a.uncommitted() {
            return (None, why::NONE);
        }
        let alloc_base = a.base;
        {
            let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(o) = opened.get_mut(&alloc_base).filter(|o| o.alloc == a.size && va - alloc_base < o.bytes) {
                o.checked = generation;
                let why = if hint == why::NEW { why::CONFIRMED } else { hint };
                let Some((_, buffer, gpu_va)) = o.gpu.as_ref() else { return (None, why) };
                return (Some(MemoryRange { console: alloc_base, bytes: o.bytes, host_va: *gpu_va, resource: buffer.as_raw(), resource_offset: 0 }), why);
            }
        }
        let Some((base, bytes)) = allocation(va) else { return (None, if hint == why::PAST { why::PAST } else { why::NONE }) };
        // asked before the lock (the reservations' lock is taken by frees, which report under it)
        let reported = crate::exports::title_reserved(base);
        let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
        let mut why = why::CONFIRMED;
        if opened.get(&base).is_none_or(|o| o.bytes != bytes || o.alloc != a.size) {
            why = if opened.contains_key(&base) { why::REOPENED } else { why::NEW };
            let gpu = self.open(base, bytes);
            if gpu.is_none() {
                OPEN_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // a stale entry for the same base (the title freed it and allocated again) is released here; work
            // already recorded holds its own references
            opened.insert(base, Opened { bytes, alloc: a.size, gpu, checked: generation, reported });
        }
        let Some(o) = opened.get_mut(&base) else { return (None, why) };
        o.checked = generation;
        let Some((_, buffer, gpu_va)) = o.gpu.as_ref() else { return (None, why) };
        (Some(MemoryRange { console: base, bytes: o.bytes, host_va: *gpu_va, resource: buffer.as_raw(), resource_offset: 0 }), why)
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
        // the title's (through the override): only its frees are all reported, so only it trusts the ranges
        let p = crate::exports::VirtualAlloc(std::ptr::null_mut(), bytes, (MEM_RESERVE | MEM_COMMIT).0, PAGE_READWRITE.0) as u64;
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
        // a change elsewhere: still answered from the cache (other tests' frees may land meanwhile: retried)
        set_ranges(true);
        let kept = (0..20).any(|_| {
            changed(p + bytes as u64, p + bytes as u64 + 65536);
            changed(0x1000, p);
            let u = UNTOUCHED.load(Relaxed);
            assert_eq!(m.range(p + 8).map(|x| x.host_va), Some(r.host_va));
            UNTOUCHED.load(Relaxed) > u
        });
        assert!(kept, "a change outside the allocation leaves it confirmed");
        // a change over it: the next lookup queries again (and finds the same allocation, still there)
        changed(p + 4096, p + 8192);
        let q1 = QUERIED.load(Relaxed);
        assert_eq!(m.range(p + 8).map(|x| x.host_va), Some(r.host_va));
        assert!(QUERIED.load(Relaxed) > q1, "queried after a change over it");
        changed_all();
        let q2 = QUERIED.load(Relaxed);
        assert_eq!(m.range(p + 8).map(|x| x.host_va), Some(r.host_va));
        assert!(QUERIED.load(Relaxed) > q2, "queried after a change of everything");
        // outside every opened allocation: never answered from the cache
        assert!(m.range(p + bytes as u64 + 16).is_none_or(|x| x.console != p));
        assert!(m.range(0x10).is_none());
        drop(m);
        // cleanup only: WARP may still hold the heap it opened over the memory for a moment (the free then fails
        // with E_INVALIDARG, seen once in a full gate); a leak of 128 KiB in a test process is harmless
        for _ in 0..50 {
            if crate::exports::VirtualFree(p as *mut c_void, 0, MEM_RELEASE.0).0 != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Suspect (b) of `agents/listcost.md`: range invalidation trusts every free and remap to be reported, which holds
    /// only inside reservations the title made through our overrides. Any other allocation (a host heap's, the
    /// CRT's, the APU heap's) can be freed and made again unreported, so it is confirmed again after every change.
    #[test]
    fn only_allocations_whose_frees_are_reported_trust_range_invalidation_on_warp() {
        use std::sync::atomic::Ordering::Relaxed;
        let Ok(factory) = (unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1::<windows::Win32::Graphics::Dxgi::IDXGIFactory4>() }) else { return };
        let Ok(adapter) = (unsafe { factory.EnumWarpAdapter::<windows::core::IUnknown>() }) else { return };
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
            return;
        }
        let m = TitleMemory::new(&device.unwrap()).expect("ID3D12Device3");
        let bytes = 65536;
        // the title's: through the VirtualAlloc override (its frees go through ours and are reported)
        let title = crate::exports::VirtualAlloc(std::ptr::null_mut(), bytes, (MEM_RESERVE | MEM_COMMIT).0, PAGE_READWRITE.0) as u64;
        // memory the overrides never saw
        let host = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as u64;
        assert!(title != 0 && host != 0);
        assert!(crate::exports::title_reserved(title + 8) && !crate::exports::title_reserved(host + 8));
        let t = m.range(title + 8).expect("opened");
        let h = m.range(host + 8).expect("opened");
        {
            let o = m.opened.lock().unwrap();
            assert!(o[&title].reported, "the title's reservation");
            assert!(!o[&host].reported, "not one of the title's");
        }
        set_ranges(true);
        // a change elsewhere: the allocation whose frees nobody reports is asked again, every time
        for _ in 0..3 {
            changed(0x1000, 0x2000);
            let q = m.queried.load(Relaxed);
            assert_eq!(m.range(host + 8).map(|x| x.host_va), Some(h.host_va));
            assert_eq!(m.queried.load(Relaxed), q + 1, "confirmed again after a change that does not touch it");
        }
        // the title's answers from the cache (other tests' frees may land meanwhile: retried)
        let kept = (0..20).any(|_| {
            changed(0x1000, 0x2000);
            let q = m.queried.load(Relaxed);
            assert_eq!(m.range(title + 8).map(|x| x.host_va), Some(t.host_va));
            m.queried.load(Relaxed) == q
        });
        assert!(kept, "a reported allocation untouched by the change stays confirmed");
        // its free goes through the override, which reports it: the next lookup asks again (WARP may refuse the
        // free while it holds the heap; reported either way)
        let _ = crate::exports::VirtualFree(title as *mut c_void, 0, MEM_RELEASE.0);
        let q = m.queried.load(Relaxed);
        let _ = m.range(title + 8);
        assert_eq!(m.queried.load(Relaxed), q + 1, "the title's free is reported");
        drop(m);
        for _ in 0..50 {
            if unsafe { VirtualFree(host as *mut c_void, 0, MEM_RELEASE) }.is_ok() {
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

