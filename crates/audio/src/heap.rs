//! A2: the APU heap (`acphal.dll`: `ApuCreateHeap`, `ApuAlloc`, `ApuFree`, `ApuMapVirtualAddress`,
//! `ApuMapApuAddress`, `ApuIsVirtualAddressValid`).
//!
//! On the console, audio memory has two addresses: the CPU's virtual address and a 32-bit address
//! the audio hardware uses. Here one host range is reserved up front and committed block by block.
//! A block's device address is `DEVICE_BASE + (host - base)`, so both maps are one subtraction, need
//! no lock, and round trip exactly for every byte.
//!
//! Memory is zeroed when allocated: free memory always reads zero, because `free` clears a block (by
//! hand when small, with fresh pages when large) and committed chunks start zero. Address space is
//! committed in 1 MiB chunks on first use and stays committed, so a steady state makes no system calls.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS,
    PAGE_READWRITE,
};

use crate::{lock, BOOL, E_INVALIDARG, E_OUTOFMEMORY, HRESULT, S_OK};

/// Device address of the heap's first byte. Non-zero, because 0 means "none" to titles.
pub const DEVICE_BASE: u32 = 0x0010_0000;
/// Host range reserved when the title allocates before (or without) `ApuCreateHeap`. Address space
/// only: nothing is committed until allocated. The title's largest block is 401 MB (fork run log).
pub const DEFAULT_RESERVE: usize = 2 << 30;
/// The most a 32-bit device address can reach above `DEVICE_BASE`.
pub const MAX_RESERVE: usize = (u32::MAX - DEVICE_BASE) as usize + 1 - (64 << 10);
/// `ApuCreateHeap` after the heap exists (code as the fork observed it).
pub const HEAP_ALREADY_CREATED: HRESULT = 0x8ACA_0001_u32 as i32;

const PAGE: usize = 4096;
/// Commit granularity: chunks stay committed once used (the title's high-water mark).
const CHUNK: usize = 1 << 20;
/// Blocks this large are cleared by taking fresh pages rather than by hand.
const FRESH_PAGES_FROM: usize = 64 << 10;
/// Smallest block and smallest alignment.
const GRAIN: usize = 16;

pub struct ApuHeap {
    base: usize,
    len: usize,
    ranges: Mutex<Ranges>,
}

/// Offsets into the range, start to end; and which 1 MiB chunks are committed.
struct Ranges {
    free: BTreeMap<usize, usize>,
    live: BTreeMap<usize, usize>,
    committed: Vec<bool>,
}

impl ApuHeap {
    /// Reserves `len` bytes (rounded up to 64 KiB) of host address space.
    pub fn reserve(len: usize) -> Option<ApuHeap> {
        let len = len.max(1).next_multiple_of(64 << 10);
        if len > MAX_RESERVE {
            return None;
        }
        // SAFETY: a fresh reservation, no address requested.
        let base = unsafe { VirtualAlloc(None, len, MEM_RESERVE, PAGE_NOACCESS) };
        if base.is_null() {
            return None;
        }
        Some(ApuHeap {
            base: base as usize,
            len,
            ranges: Mutex::new(Ranges {
                free: BTreeMap::from([(0, len)]),
                live: BTreeMap::new(),
                committed: vec![false; len.div_ceil(CHUNK)],
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.ranges).live.is_empty()
    }

    pub fn live_blocks(&self) -> usize {
        lock(&self.ranges).live.len()
    }

    /// Commits the chunks under `[start, stop)` that are not yet, one call per run.
    fn commit(&self, committed: &mut [bool], start: usize, stop: usize) -> bool {
        let (first, last) = (start / CHUNK, (stop - 1) / CHUNK);
        let mut c = first;
        while c <= last {
            if committed[c] {
                c += 1;
                continue;
            }
            let run = (c..=last).take_while(|&k| !committed[k]).count();
            let at = c * CHUNK;
            let bytes = ((c + run) * CHUNK).min(self.len) - at;
            // SAFETY: inside our reservation.
            let p = unsafe { VirtualAlloc(Some((self.base + at) as *const c_void), bytes, MEM_COMMIT, PAGE_READWRITE) };
            if p.is_null() {
                return false;
            }
            committed[c..c + run].fill(true);
            c += run;
        }
        true
    }

    /// A zeroed block: its host address and device address. `align` is a power of two, at least 4.
    pub fn alloc(&self, size: usize, align: usize) -> Result<(*mut u8, u32), HRESULT> {
        if size == 0 || align < 4 || !align.is_power_of_two() || align > self.len {
            return Err(E_INVALIDARG);
        }
        let align = align.max(GRAIN);
        let size = size.checked_next_multiple_of(GRAIN).ok_or(E_OUTOFMEMORY)?;
        let mut r = lock(&self.ranges);
        let (start, end, at) = r
            .free
            .iter()
            .find_map(|(&s, &e)| {
                let at = s.checked_next_multiple_of(align)?;
                (at.checked_add(size)? <= e).then_some((s, e, at))
            })
            .ok_or(E_OUTOFMEMORY)?;
        let stop = at + size;
        let r = &mut *r;
        if !self.commit(&mut r.committed, at, stop) {
            return Err(E_OUTOFMEMORY);
        }
        r.free.remove(&start);
        if start < at {
            r.free.insert(start, at);
        }
        if stop < end {
            r.free.insert(stop, end);
        }
        r.live.insert(at, stop);
        // Free memory always reads zero (fresh pages, or cleared by `free`): nothing to clear here.
        Ok(((self.base + at) as *mut u8, DEVICE_BASE + at as u32))
    }

    /// Frees the block that starts at `ptr`; false for anything else. The block is cleared first, so
    /// the next allocation needs no clearing: small blocks by hand, large ones by handing their whole
    /// pages back to the system and taking fresh zero pages.
    pub fn free(&self, ptr: *const u8) -> bool {
        let offset = (ptr as usize).wrapping_sub(self.base);
        let Some(end) = lock(&self.ranges).live.get(&offset).copied() else {
            return false;
        };
        let p = (self.base + offset) as *mut u8;
        let size = end - offset;
        if size < FRESH_PAGES_FROM {
            // SAFETY: still live and ours until it is removed below.
            unsafe { std::ptr::write_bytes(p, 0, size) };
        } else {
            let (first, last) = (offset.next_multiple_of(PAGE), end / PAGE * PAGE);
            // SAFETY: as above; the pages are recommitted at once.
            unsafe {
                let page = (self.base + first) as *mut c_void;
                let _ = VirtualFree(page, last - first, MEM_DECOMMIT);
                VirtualAlloc(Some(page), last - first, MEM_COMMIT, PAGE_READWRITE);
                std::ptr::write_bytes(p, 0, first - offset);
                std::ptr::write_bytes((self.base + last) as *mut u8, 0, end - last);
            }
        }
        let mut r = lock(&self.ranges);
        if r.live.remove(&offset).is_none() {
            return false;
        }
        let (mut start, mut end) = (offset, end);
        if let Some((&s, &e)) = r.free.range(..start).next_back() {
            if e == start {
                r.free.remove(&s);
                start = s;
            }
        }
        if let Some(e) = r.free.remove(&end) {
            end = e;
        }
        r.free.insert(start, end);
        true
    }

    /// The device address of a host address inside the heap; 0 outside it.
    pub fn device(&self, ptr: *const u8) -> u32 {
        let offset = (ptr as usize).wrapping_sub(self.base);
        if offset < self.len {
            DEVICE_BASE + offset as u32
        } else {
            0
        }
    }

    /// The host address of a device address inside the heap; null outside it.
    pub fn host(&self, device: u32) -> *mut u8 {
        let offset = device.wrapping_sub(DEVICE_BASE) as usize;
        if device >= DEVICE_BASE && offset < self.len {
            (self.base + offset) as *mut u8
        } else {
            std::ptr::null_mut()
        }
    }

    /// `len` bytes at a device address, if all of them lie inside one live block.
    pub fn span(&self, device: u32, len: usize) -> Option<*mut u8> {
        let offset = device.checked_sub(DEVICE_BASE)? as usize;
        let r = lock(&self.ranges);
        let (_, &end) = r.live.range(..=offset).next_back()?;
        (offset.checked_add(len)? <= end).then_some((self.base + offset) as *mut u8)
    }
}

impl Drop for ApuHeap {
    fn drop(&mut self) {
        // SAFETY: our reservation, released once.
        let _ = unsafe { VirtualFree(self.base as *mut c_void, 0, MEM_RELEASE) };
    }
}

// The process's heap, behind the console functions.

static HEAP: OnceLock<Arc<ApuHeap>> = OnceLock::new();

/// The heap the console functions use; reserved with `DEFAULT_RESERVE` on first use unless
/// `ApuCreateHeap` came first.
pub fn global() -> &'static Arc<ApuHeap> {
    HEAP.get_or_init(|| {
        Arc::new(ApuHeap::reserve(DEFAULT_RESERVE).expect("reserve the APU heap's address range"))
    })
}

/// `HRESULT ApuCreateHeap(UINT32 cachedBytes, UINT32 nonCachedBytes)`
///
/// Sizes the heap once, before the first allocation. The reservation is the larger of the two sizes
/// summed and `DEFAULT_RESERVE`: the sizes' units and whether the console enforces them as limits are
/// not known, and reserving address space costs nothing.
pub extern "system" fn ApuCreateHeap(cached: u32, non_cached: u32) -> HRESULT {
    if cached == 0 {
        return E_INVALIDARG;
    }
    if HEAP.get().is_some() {
        return HEAP_ALREADY_CREATED;
    }
    let len = (cached as usize + non_cached as usize).clamp(DEFAULT_RESERVE, MAX_RESERVE);
    let Some(heap) = ApuHeap::reserve(len) else {
        return E_OUTOFMEMORY;
    };
    match HEAP.set(Arc::new(heap)) {
        Ok(()) => S_OK,
        Err(_) => HEAP_ALREADY_CREATED,
    }
}

/// `HRESULT ApuAlloc(void** virtualAddress, UINT32* apuAddress /*optional*/, UINT32 size,
/// UINT32 alignment, UINT32 flags)`
///
/// `alignment` is a power of two, at least 4. `flags` (the title passes 1) is taken to choose cached
/// or non-cached memory; the host has one kind, so it is not used.
///
/// # Safety
/// `out` is writable; `apu` is null or writable.
pub unsafe extern "system" fn ApuAlloc(
    out: *mut *mut c_void,
    apu: *mut u32,
    size: u32,
    align: u32,
    _flags: u32,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    match global().alloc(size as usize, align as usize) {
        Ok((ptr, device)) => {
            out.write(ptr.cast());
            if !apu.is_null() {
                apu.write(device);
            }
            S_OK
        }
        Err(e) => e,
    }
}

/// `void ApuFree(void* virtualAddress)`: a block from `ApuAlloc`; anything else is ignored.
pub extern "system" fn ApuFree(ptr: *mut c_void) {
    global().free(ptr.cast());
}

/// `UINT32 ApuMapVirtualAddress(const void* virtualAddress)`: 0 outside the heap.
pub extern "system" fn ApuMapVirtualAddress(ptr: *const c_void) -> u32 {
    global().device(ptr.cast())
}

/// `void* ApuMapApuAddress(UINT32 apuAddress)`: null outside the heap.
pub extern "system" fn ApuMapApuAddress(device: u32) -> *mut c_void {
    global().host(device).cast()
}

/// `BOOL ApuIsVirtualAddressValid(const void* virtualAddress, UINT32 alignment)`: inside the heap
/// and, when `alignment` is non-zero, its device address a multiple of it.
pub extern "system" fn ApuIsVirtualAddressValid(ptr: *const c_void, align: u32) -> BOOL {
    let device = global().device(ptr.cast());
    BOOL::from(device != 0 && (align == 0 || device % align == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2_addresses_round_trip_for_every_byte() {
        let heap = ApuHeap::reserve(16 << 20).unwrap();
        let mut blocks = Vec::new();
        for (size, align) in [(1, 4), (100, 16), (4096, 4096), (70_000, 256), (3, 128), (65_536, 65_536)] {
            let (ptr, device) = heap.alloc(size, align).unwrap();
            assert_eq!(device as usize % align.max(GRAIN), 0, "size {size} align {align}");
            assert_eq!(ptr as usize % align.max(GRAIN), 0);
            for i in 0..size {
                let p = ptr.wrapping_add(i);
                assert_eq!(heap.device(p), device + i as u32);
                assert_eq!(heap.host(device + i as u32), p);
            }
            // SAFETY: the block is ours and zeroed.
            assert!(unsafe { std::slice::from_raw_parts(ptr, size) }.iter().all(|&b| b == 0));
            assert_eq!(heap.span(device, size), Some(ptr));
            assert_eq!(heap.span(device, size + GRAIN), None, "past the block");
            blocks.push(ptr);
        }
        // Outside the heap: 0 and null.
        let outside = 0u8;
        assert_eq!(heap.device(&outside), 0);
        assert!(heap.host(0).is_null());
        assert!(heap.host(DEVICE_BASE - 1).is_null());
        assert!(heap.host(DEVICE_BASE + heap.len() as u32).is_null());
        for p in blocks {
            assert!(heap.free(p));
            assert!(!heap.free(p), "freed twice");
        }
        assert!(heap.is_empty());
        // Everything merged back into one free range.
        assert_eq!(lock(&heap.ranges).free.len(), 1);
    }

    #[test]
    fn a2_reused_memory_comes_back_zeroed() {
        let heap = ApuHeap::reserve(1 << 20).unwrap();
        // A small neighbour keeps the first page committed while the big block comes and goes.
        let (small, _) = heap.alloc(64, 16).unwrap();
        for round in 0..3u8 {
            let (big, _) = heap.alloc(3 * PAGE + 100, 16).unwrap();
            // SAFETY: ours.
            let bytes = unsafe { std::slice::from_raw_parts_mut(big, 3 * PAGE + 100) };
            assert!(bytes.iter().all(|&b| b == 0), "round {round}");
            bytes.fill(0xA5);
            assert!(heap.free(big));
        }
        // SAFETY: ours; the neighbour's bytes are untouched by the others' clearing.
        unsafe { small.write_bytes(7, 64) };
        let (other, _) = heap.alloc(200, 16).unwrap();
        // SAFETY: ours.
        assert!(unsafe { std::slice::from_raw_parts(small, 64) }.iter().all(|&b| b == 7));
        // SAFETY: ours.
        assert!(unsafe { std::slice::from_raw_parts(other, 200) }.iter().all(|&b| b == 0));
    }

    #[test]
    fn a2_rejects_bad_requests_and_runs_out_cleanly() {
        let heap = ApuHeap::reserve(64 << 10).unwrap();
        assert_eq!(heap.alloc(0, 16), Err(E_INVALIDARG));
        assert_eq!(heap.alloc(16, 2), Err(E_INVALIDARG));
        assert_eq!(heap.alloc(16, 6), Err(E_INVALIDARG));
        assert_eq!(heap.alloc(128 << 10, 16), Err(E_OUTOFMEMORY));
        let (a, _) = heap.alloc(32 << 10, 16).unwrap();
        let (b, _) = heap.alloc(32 << 10, 16).unwrap();
        assert_eq!(heap.alloc(16, 16), Err(E_OUTOFMEMORY));
        heap.free(a);
        assert_eq!(heap.alloc(32 << 10, 16).map(|(p, _)| p), Ok(a), "first fit reuses the hole");
        heap.free(b);
    }

    #[test]
    fn a2_console_functions() {
        let (mut ptr, mut apu) = (std::ptr::null_mut(), 0u32);
        // SAFETY: valid out pointers.
        assert_eq!(unsafe { ApuAlloc(&mut ptr, &mut apu, 0x100, 128, 1) }, S_OK);
        assert!(apu >= DEVICE_BASE && apu % 128 == 0);
        let inner = ptr.cast::<u8>().wrapping_add(0x40).cast_const().cast();
        assert_eq!(ApuMapVirtualAddress(inner), apu + 0x40);
        assert_eq!(ApuMapApuAddress(apu + 0x40), inner.cast_mut());
        assert_eq!(ApuIsVirtualAddressValid(ptr, 128), 1);
        assert_eq!(ApuIsVirtualAddressValid(inner, 128), 0, "misaligned");
        assert_eq!(ApuIsVirtualAddressValid(&apu as *const u32 as *const c_void, 0), 0);
        // SAFETY: valid out pointer.
        assert_eq!(unsafe { ApuAlloc(&mut ptr, std::ptr::null_mut(), 0x100, 6, 1) }, E_INVALIDARG);
        ApuFree(ptr);
        assert_eq!(ApuCreateHeap(64 << 20, 0), HEAP_ALREADY_CREATED, "after the first allocation");
        assert_eq!(ApuCreateHeap(0, 0), E_INVALIDARG);
    }
}
