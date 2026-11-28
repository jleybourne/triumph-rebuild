//! What the process's address space holds at an address, cheaply.
//!
//! `VirtualQuery` computes `RegionSize` by walking the page tables of the region it lands in: measured 0.94 ms
//! on a touched 256 MiB region, 4.6 ms on 1 GiB, 16 ms on 2 GiB (the title's graphics memory is several of
//! those). `QueryVirtualMemoryInformation(MemoryRegionInfo)` reads the allocation's descriptor only (3 µs on
//! the same 2 GiB). It gives the whole allocation, how much of it is committed, and the protection it was made
//! with, which is all a fully committed allocation needs; anything else falls back to `VirtualQuery`.

use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::*;

/// One allocation (`VirtualAlloc`, or one mapped view) as its descriptor has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub base: u64,
    pub size: u64,
    pub commit: u64,
    /// the protection it was made with (`AllocationProtect`)
    pub protect: u32,
    /// private memory (`VirtualAlloc`). A mapped view (a section: the title's physical pages) counts **no** commit
    /// of its own here (the section holds it), so `commit` says nothing about it.
    pub private: bool,
}

impl Allocation {
    /// Private memory with every page committed. Never true of a mapped view: ask `VirtualQuery` there (views are
    /// small: 64 KiB frames).
    pub fn fully_committed(&self) -> bool {
        self.private && self.commit == self.size && self.size != 0
    }

    /// Nothing of it can be committed (private memory with no commit); a mapped view may be, whatever `commit` says.
    pub fn uncommitted(&self) -> bool {
        self.private && self.commit == 0
    }
}

/// The allocation holding `va`, from its descriptor alone (`None`: free memory).
pub fn allocation_of(va: u64) -> Option<Allocation> {
    let mut r = WIN32_MEMORY_REGION_INFORMATION::default();
    // the current process's pseudo handle
    let me = HANDLE(-1isize as *mut c_void);
    unsafe { QueryVirtualMemoryInformation(me, va as *const c_void, MemoryRegionInfo, &mut r as *mut _ as *mut c_void, std::mem::size_of::<WIN32_MEMORY_REGION_INFORMATION>(), None) }.ok()?;
    if r.AllocationBase.is_null() {
        return None;
    }
    // flags bit 0: Private
    let private = unsafe { r.Anonymous.Flags } & 1 != 0;
    Some(Allocation { base: r.AllocationBase as u64, size: r.RegionSize as u64, commit: r.CommitSize as u64, protect: r.AllocationProtect, private })
}

/// Protection bits that allow reads, and writes.
pub const READ: u32 = 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80;
pub const WRITE: u32 = 0x04 | 0x08 | 0x40 | 0x80;
const NO_ACCESS: u32 = 0x01 | 0x100; // PAGE_NOACCESS, PAGE_GUARD

/// The protection of committed memory over `at..at + len`, if it is all committed: from the allocation's descriptor
/// when the allocation is fully committed with an access it was made with, else from `VirtualQuery` (the range
/// must then lie in one region of uniform protection). `None`: not all committed.
///
/// A page the owner re-protected later (`VirtualProtect`) keeps its first protection here; the callers use this to
/// decide whether to read or write memory the title gave them, where that is the title's business.
pub fn committed_protect(at: u64, len: u64) -> Option<u32> {
    let end = at.checked_add(len)?;
    if let Some(a) = allocation_of(at) {
        if a.fully_committed() && end <= a.base + a.size && a.protect & READ != 0 && a.protect & NO_ACCESS == 0 {
            return Some(a.protect & 0xff);
        }
    }
    let mut m = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a query, no access
    if unsafe { VirtualQuery(Some(at as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
        return None;
    }
    (m.State == MEM_COMMIT && end <= m.BaseAddress as u64 + m.RegionSize as u64).then_some(m.Protect.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_names_the_allocation_and_what_is_committed() {
        let bytes = 4 << 20;
        let p = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE, PAGE_READWRITE) } as u64;
        assert_ne!(p, 0);
        let a = allocation_of(p + 70_000).unwrap();
        assert_eq!((a.base, a.size, a.commit), (p, bytes as u64, 0));
        assert_eq!(committed_protect(p, 16), None, "reserved only");
        unsafe { VirtualAlloc(Some(p as *const c_void), 65536, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(allocation_of(p + 10).unwrap().commit, 65536);
        assert_eq!(committed_protect(p + 100, 16), Some(PAGE_READWRITE.0), "the committed part, by VirtualQuery");
        assert_eq!(committed_protect(p + 65536 - 8, 16), None, "past the committed part");
        unsafe { VirtualAlloc(Some(p as *const c_void), bytes, MEM_COMMIT, PAGE_READWRITE) };
        let a = allocation_of(p + bytes as u64 - 1).unwrap();
        assert!(a.fully_committed());
        assert_eq!(committed_protect(p + 1000, 4096), Some(PAGE_READWRITE.0), "from the descriptor");
        assert_eq!(committed_protect(p + bytes as u64 - 8, 16), None, "past the allocation");
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
        assert_eq!(allocation_of(p), None, "freed");
    }

    /// The point of it: a large touched allocation answers in microseconds, not milliseconds.
    #[test]
    fn the_descriptor_is_fast_on_a_large_touched_allocation() {
        let bytes = 512usize << 20;
        let p = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as *mut u8;
        assert!(!p.is_null());
        for i in (0..bytes).step_by(4096) {
            unsafe { p.add(i).write_volatile(1) };
        }
        let t = std::time::Instant::now();
        for _ in 0..100 {
            assert!(committed_protect(p as u64 + 12345, 64).is_some());
        }
        let per = t.elapsed().as_secs_f64() * 1e6 / 100.0;
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
        assert!(per < 200.0, "{per:.1} µs a query on 512 MiB");
    }
}

