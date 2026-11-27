//! `xmem.dll`: the title's memory, placed for our memory model (plan decision 1, Builds 1-2).
//!
//! The console's meaning of the arguments is from the fork's study (`victory-hle` `modules/xmem.rs`,
//! read for design). The placement is ours:
//!
//! - **graphics memory** (flag bits 0-1) is placed so it never straddles a 4 GiB boundary. Each
//!   allocation lies in one window, so its CPU address *is* its GPU address: window `addr >> 32`, offset
//!   `addr & 0xFFFF_FFFF`, exactly what the shader core lowers loads and stores to. It is allocated with
//!   write-watch, so the Build 2 ring sends what the CPU changes;
//! - **32-bit memory** (flag bit 2, or attribute bit 33) stays below 4 GiB;
//! - **CPU-only memory** is ordinary host memory;
//! - `XMemVirtualQuery` reports each region's real flags, not "graphics" for everything.
//!
//! Every graphics region committed is recorded ([`Xmem::regions`]) for the tiles and the ring to
//! register: the layer wires that when the title runs.

use std::collections::BTreeMap;

/// XMem flags: memory both processors reach
pub const GRAPHICS: u64 = 3;
/// XMem flags: a 32-bit address
pub const ADDRESS_32BIT: u64 = 4;
/// allocation type: 2 MiB pages (host reads it as large pages, which need a privilege: dropped)
pub const PAGES_2M: u32 = 0x2000_0000;
/// with PAGES_2M: 64 KiB pages (host reads it as AWE: dropped)
pub const PAGES_64K_BIT: u32 = 0x40_0000;
/// the host's protection bits; above them the console adds GPU access bits (0x800-0x40000)
pub const HOST_PROTECTION: u32 = 0x1ff;
/// `XMemAlloc` attributes: alignment as log2, 5 bits at 24
pub const ALIGNMENT_SHIFT: u64 = 24;
/// `XMemAlloc` attributes: a 32-bit address
pub const ALLOC_32BIT: u64 = 1 << 33;

pub const WINDOW: u64 = 1 << 32;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for 32-bit memory; otherwise from
/// 4 GiB up to `47 - field` bits for the 3-bit field at bit 12, or 40 bits without one.
pub fn address_range(flags: u64) -> (u64, u64) {
    if flags & ADDRESS_32BIT != 0 {
        return (0x1_0000, u32::MAX as u64);
    }
    let bits = match flags >> 12 & 7 {
        0 => 40,
        f => 47 - f,
    };
    (WINDOW, (1u64 << bits) - 1)
}

pub fn is_graphics(flags: u64) -> bool {
    flags & GRAPHICS != 0
}

/// The alignment `XMemAlloc` attributes ask for (at least 16).
pub fn alloc_alignment(attributes: u64) -> u64 {
    1u64 << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4)
}

/// The windows an allocation of `size` may be placed in, first to last, so it lies inside one window
/// and inside `range`. Refused (empty) when it cannot fit in a single window.
pub fn candidate_windows(range: (u64, u64), size: u64) -> impl Iterator<Item = (u64, u64)> {
    let (lo, hi) = range;
    let first = lo / WINDOW;
    let last = hi / WINDOW;
    (first..=last).filter_map(move |w| {
        let start = (w * WINDOW).max(lo);
        let end = ((w + 1) * WINDOW - 1).min(hi);
        (size <= WINDOW && end >= start && end - start + 1 >= size).then_some((start, end))
    })
}

/// A region the title committed, as `XMemVirtualQuery` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub base: u64,
    pub size: u64,
    pub flags: u64,
    pub protect: u32,
}

impl Region {
    pub fn window(&self) -> u64 {
        self.base >> 32
    }
}

/// The state behind the exports: every region by base address.
#[derive(Default)]
pub struct Xmem {
    regions: BTreeMap<u64, Region>,
}

impl Xmem {
    pub fn record(&mut self, r: Region) {
        self.regions.insert(r.base, r);
    }

    pub fn forget(&mut self, base: u64) -> Option<Region> {
        self.regions.remove(&base)
    }

    /// The region holding `addr`.
    pub fn find(&self, addr: u64) -> Option<Region> {
        self.regions.range(..=addr).next_back().map(|(_, r)| *r).filter(|r| addr < r.base + r.size)
    }

    /// Graphics regions: the ones the tiles and the upload ring must register.
    pub fn graphics(&self) -> impl Iterator<Item = &Region> {
        self.regions.values().filter(|r| is_graphics(r.flags))
    }
}

#[cfg(windows)]
pub mod host {
    //! The allocations themselves, on the host.

    use super::*;
    use std::ffi::c_void;
    use windows::Win32::System::Memory::*;
    use windows::Win32::System::Threading::GetCurrentProcess;

    pub const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// `XMemVirtualAlloc`'s work: honour `range`, keep graphics memory inside one window, and watch
    /// its writes. Returns the base, or null.
    pub fn virtual_alloc(x: &mut Xmem, base: u64, size: u64, kind: u32, flags: u64, protect: u32) -> u64 {
        let graphics = is_graphics(flags);
        let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
        // no address: a commit is a reserve and a commit (Windows' rule, which the title relies on). We place at an
        // exact address we found, where a commit alone would fail, so the reserve is made explicit.
        if base == 0 {
            kind |= MEM_RESERVE.0;
        }
        let protect = match protect & HOST_PROTECTION {
            0 => PAGE_NOACCESS.0,
            p => p,
        };
        // a placeholder reservation (the title maps physical pages into it later, physical.rs) cannot be
        // write-watched, and needs no watch: its contents are physical pages the GPU reads in place
        let placeholder = kind & MEM_RESERVE_PLACEHOLDER.0 != 0;
        if graphics && kind & MEM_RESERVE.0 != 0 && !placeholder {
            kind |= MEM_WRITE_WATCH;
        }
        let got = if base != 0 {
            // a fixed address: the title's choice (a commit inside its own reservation, usually)
            unsafe { VirtualAlloc2(GetCurrentProcess(), Some(base as *const c_void), size as usize, VIRTUAL_ALLOCATION_TYPE(kind), protect, None) }
        } else {
            // never MEM_ADDRESS_REQUIREMENTS: with write-watch, a request into a window that has no room
            // spins in the kernel (exitprobe wwmany 8 GiB: the 17th 256 MiB reservation, minutes, the
            // process unkillable). We find the gap ourselves and ask for that exact address, which never
            // searches: it succeeds or fails at once.
            let mut got = std::ptr::null_mut();
            for (lo, hi) in candidate_windows(address_range(flags), size) {
                if let Some(at) = free_gap(lo, hi, size) {
                    got = unsafe { VirtualAlloc2(GetCurrentProcess(), Some(at as *const c_void), size as usize, VIRTUAL_ALLOCATION_TYPE(kind), protect, None) };
                    if !got.is_null() {
                        break;
                    }
                    // taken between the query and the call (another thread): one fresh look in this window
                    if let Some(at) = free_gap(lo, hi, size) {
                        got = unsafe { VirtualAlloc2(GetCurrentProcess(), Some(at as *const c_void), size as usize, VIRTUAL_ALLOCATION_TYPE(kind), protect, None) };
                        if !got.is_null() {
                            break;
                        }
                    }
                }
            }
            got
        };
        if got.is_null() {
            // a failed placement is a title-visible failure: name it (the first few)
            static FAILED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                let err = std::io::Error::last_os_error();
                eprintln!(
                    "xmem      placement failed: base {base:#x} size {size:#x} kind {kind:#x} flags {flags:#x} protect {protect:#x} range {:x?}: {err}",
                    address_range(flags)
                );
            }
            return 0;
        }
        let at = got as u64;
        if kind & MEM_RESERVE.0 != 0 {
            x.record(Region { base: at, size, flags, protect });
        }
        at
    }

    /// The first 64 KiB-aligned free gap of `size` bytes inside `[lo, hi]`, by walking the address space
    /// with VirtualQuery (user mode, bounded: one step per region).
    pub fn free_gap(lo: u64, hi: u64, size: u64) -> Option<u64> {
        const ALIGN: u64 = 64 << 10;
        let mut at = lo.div_ceil(ALIGN) * ALIGN;
        for _ in 0..(1 << 20) {
            if at > hi || hi - at + 1 < size {
                return None;
            }
            let mut m = MEMORY_BASIC_INFORMATION::default();
            let n = unsafe { VirtualQuery(Some(at as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
            if n == 0 {
                return None;
            }
            let end = m.BaseAddress as u64 + m.RegionSize as u64; // one past the region
            if m.State == MEM_FREE {
                let start = at.max(m.BaseAddress as u64).div_ceil(ALIGN) * ALIGN;
                if start + size <= end && start + size - 1 <= hi {
                    return Some(start);
                }
            }
            at = end.div_ceil(ALIGN) * ALIGN;
        }
        None
    }

    pub fn virtual_free(x: &mut Xmem, base: u64, size: u64, kind: u32) -> bool {
        let r = unsafe { VirtualFree(base as *mut c_void, size as usize, VIRTUAL_FREE_TYPE(kind)) };
        if let Err(e) = &r {
            static FAILED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                let mut m = MEMORY_BASIC_INFORMATION::default();
                unsafe { VirtualQuery(Some(base as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
                eprintln!(
                    "xmem      free failed: base {base:#x} size {size:#x} kind {kind:#x}: {e}; region there: base {:#x} size {:#x} state {:#x} type {:#x}",
                    m.BaseAddress as u64, m.RegionSize, m.State.0, m.Type.0
                );
            }
        }
        let ok = r.is_ok();
        // splitting a placeholder (MEM_PRESERVE_PLACEHOLDER) leaves the reservation: only a real release forgets it
        if ok && kind & MEM_RELEASE.0 != 0 && kind & MEM_PRESERVE_PLACEHOLDER.0 == 0 {
            x.forget(base);
        }
        ok
    }

    /// How many pages were written since the last call, for a graphics region (what the ring reads).
    pub fn written_pages(r: &Region) -> usize {
        let mut addrs = vec![std::ptr::null_mut(); (r.size / 4096) as usize];
        let mut count = addrs.len();
        let mut gran = 0u32;
        unsafe { GetWriteWatch(0, r.base as *const c_void, r.size as usize, Some(addrs.as_mut_ptr()), Some(&mut count), Some(&mut gran)) };
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_range_follows_the_flags() {
        assert_eq!(address_range(GRAPHICS | ADDRESS_32BIT), (0x1_0000, 0xFFFF_FFFF));
        assert_eq!(address_range(GRAPHICS), (WINDOW, (1 << 40) - 1));
        assert_eq!(address_range(GRAPHICS | 1 << 12), (WINDOW, (1 << 46) - 1));
    }

    #[test]
    fn alignment_comes_from_the_attributes() {
        assert_eq!(alloc_alignment(0), 16);
        assert_eq!(alloc_alignment(12 << ALIGNMENT_SHIFT), 4096);
        assert_eq!(alloc_alignment(16 << ALIGNMENT_SHIFT), 65536);
    }

    #[test]
    fn graphics_memory_never_straddles_a_window() {
        let w: Vec<_> = candidate_windows((WINDOW, (1 << 40) - 1), 64 << 20).take(2).collect();
        assert_eq!(w[0], (WINDOW, 2 * WINDOW - 1));
        assert_eq!(w[1], (2 * WINDOW, 3 * WINDOW - 1));
        // bigger than a window: refused
        assert_eq!(candidate_windows((WINDOW, (1 << 40) - 1), WINDOW + 1).count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_commit_without_an_address_reserves_too() {
        use windows::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE};
        let mut x = Xmem::default();
        // as the title asks at device creation: commit 64 KiB, graphics, no address, no reserve flag
        let a = host::virtual_alloc(&mut x, 0, 64 << 10, MEM_COMMIT.0 | PAGES_2M | PAGES_64K_BIT, GRAPHICS | 0x280, PAGE_READWRITE.0 | 0x42400);
        assert_ne!(a, 0, "placed and committed");
        unsafe { (a as *mut u64).write_volatile(7) };
        assert!(x.find(a).is_some(), "recorded as a region");
        assert!(host::virtual_free(&mut x, a, 0, MEM_RELEASE.0));
    }

    #[cfg(windows)]
    #[test]
    fn a_graphics_placeholder_reservation_is_placed_without_write_watch() {
        use windows::Win32::System::Memory::{MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS};
        let mut x = Xmem::default();
        // as the title asks for its descriptor memory: reserve | placeholder, graphics + 32-bit, no access
        let a = host::virtual_alloc(&mut x, 0, 0xB90_0000, MEM_RESERVE.0 | MEM_RESERVE_PLACEHOLDER.0, GRAPHICS | ADDRESS_32BIT, PAGE_NOACCESS.0);
        assert_ne!(a, 0);
        assert!(a + 0xB90_0000 <= u32::MAX as u64 + 1, "below 4 GiB as asked");
        // the title then splits it (VirtualFree with MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER), piece by piece
        let ok = host::virtual_free(&mut x, a, 0x10_0000, 0x8002);
        assert!(ok, "split: {}", std::io::Error::last_os_error());
        assert!(host::virtual_free(&mut x, a + 0x10_0000, 0x10_0000, 0x8002), "second split: {}", std::io::Error::last_os_error());
    }

    #[test]
    fn regions_are_found_by_any_address_inside() {
        let mut x = Xmem::default();
        x.record(Region { base: 0x1_0000_0000, size: 0x2_0000, flags: GRAPHICS, protect: 4 });
        assert_eq!(x.find(0x1_0001_FFFF).unwrap().window(), 1);
        assert!(x.find(0x1_0002_0000).is_none());
        assert_eq!(x.graphics().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn on_the_host_graphics_memory_lands_in_one_window_and_is_watched() {
        use windows::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
        let mut x = Xmem::default();
        let size = 8 << 20;
        let a = host::virtual_alloc(&mut x, 0, size, MEM_RESERVE.0 | MEM_COMMIT.0 | PAGES_2M, GRAPHICS, PAGE_READWRITE.0 | 0x800);
        assert!(a >= WINDOW, "graphics memory above 4 GiB, got {a:#x}");
        assert_eq!(a >> 32, (a + size - 1) >> 32, "inside one window");
        let r = x.find(a + 100).unwrap();
        assert!(is_graphics(r.flags));
        assert_eq!(host::written_pages(&r), 0);
        unsafe { (a as *mut u8).add(3 * 4096).write_volatile(1) };
        assert_eq!(host::written_pages(&r), 1, "write-watch is on");
        assert!(host::virtual_free(&mut x, a, 0, MEM_RELEASE.0));
        assert!(x.find(a).is_none());

        let low = host::virtual_alloc(&mut x, 0, 1 << 20, MEM_RESERVE.0 | MEM_COMMIT.0, GRAPHICS | ADDRESS_32BIT, PAGE_READWRITE.0);
        assert!(low != 0 && low + (1 << 20) <= 1 << 32, "32-bit memory below 4 GiB, got {low:#x}");
        assert!(host::virtual_free(&mut x, low, 0, MEM_RELEASE.0));
    }
}
