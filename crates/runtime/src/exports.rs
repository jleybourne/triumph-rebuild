//! The console-only functions we implement, with the console's exact signatures, and the table the
//! resolver binds them from. `xmem.dll` first (13 functions).

#![cfg(windows)]
// the console's own names, so the import table and the code read the same
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::Memory::*;

use crate::physical::{reserve_placeholder, Physical, FRAME};
use crate::xmem::{self, host, Region, Xmem, ADDRESS_32BIT, ALLOC_32BIT, GRAPHICS, PAGES_2M, PAGES_64K_BIT};

/// A first-fit heap over graphics chunks (write-watched, each inside one window).
struct ChunkHeap {
    flags: u64,
    free: BTreeMap<u64, u64>,
    used: BTreeMap<u64, u64>,
}

const CHUNK: u64 = 64 << 20;
const BIG: u64 = 32 << 20;

impl ChunkHeap {
    fn new(flags: u64) -> ChunkHeap {
        ChunkHeap { flags, free: BTreeMap::new(), used: BTreeMap::new() }
    }

    fn alloc(&mut self, x: &mut Xmem, size: u64, align: u64) -> u64 {
        let size = size.max(1).div_ceil(16) * 16;
        if size >= BIG {
            let a = host::virtual_alloc(x, 0, size.div_ceil(65536) * 65536, (MEM_RESERVE | MEM_COMMIT).0, self.flags, PAGE_READWRITE.0);
            if a != 0 {
                self.used.insert(a, size);
            }
            return a;
        }
        for _ in 0..2 {
            let fit = self.free.iter().find_map(|(&start, &len)| {
                let at = start.div_ceil(align) * align;
                (at + size <= start + len).then_some((start, len, at))
            });
            if let Some((start, len, at)) = fit {
                self.free.remove(&start);
                if at > start {
                    self.free.insert(start, at - start);
                }
                if at + size < start + len {
                    self.free.insert(at + size, start + len - at - size);
                }
                self.used.insert(at, size);
                return at;
            }
            let c = host::virtual_alloc(x, 0, CHUNK, (MEM_RESERVE | MEM_COMMIT).0, self.flags, PAGE_READWRITE.0);
            if c == 0 {
                return 0;
            }
            self.free.insert(c, CHUNK);
        }
        0
    }

    fn free(&mut self, x: &mut Xmem, at: u64) -> bool {
        let Some(size) = self.used.remove(&at) else { return false };
        if size >= BIG {
            return host::virtual_free(x, at, 0, MEM_RELEASE.0);
        }
        let (mut start, mut len) = (at, size);
        if let Some((&ps, &pl)) = self.free.range(..start).next_back() {
            if ps + pl == start {
                self.free.remove(&ps);
                start = ps;
                len += pl;
            }
        }
        if let Some(&nl) = self.free.get(&(start + len)) {
            self.free.remove(&(start + len));
            len += nl;
        }
        self.free.insert(start, len);
        true
    }
}

struct State {
    xmem: Xmem,
    physical: Physical,
    heap: ChunkHeap,
    heap32: ChunkHeap,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| State {
        xmem: Xmem::default(),
        physical: Physical::default(),
        heap: ChunkHeap::new(GRAPHICS),
        heap32: ChunkHeap::new(GRAPHICS | ADDRESS_32BIT),
    });
    f(s)
}

/// Graphics regions committed so far (for the tiles and the upload ring to register).
pub fn graphics_regions() -> Vec<Region> {
    with(|s| s.xmem.graphics().copied().collect())
}

// ------------------------------------------------------------------------------------- xmem.dll

pub extern "system" fn XMemVirtualAlloc(base: *mut c_void, size: usize, kind: u32, flags: u64, protect: u32) -> *mut c_void {
    with(|s| {
        // a 64 KiB-page reservation with no commit: a placeholder the title will map physical pages into
        if kind & (PAGES_2M | PAGES_64K_BIT) == (PAGES_2M | PAGES_64K_BIT) && kind & MEM_COMMIT.0 == 0 && base.is_null() {
            let a = reserve_placeholder(size);
            if a != 0 {
                s.xmem.record(Region { base: a, size: size as u64, flags, protect });
            }
            return a as *mut c_void;
        }
        host::virtual_alloc(&mut s.xmem, base as u64, size as u64, kind, flags, protect) as *mut c_void
    })
}

/// The console's `XMemVirtualQuery` output: the host's `MEMORY_BASIC_INFORMATION`, then the XMem flags
/// and a tag.
#[repr(C)]
pub struct BasicInformation {
    pub host: MEMORY_BASIC_INFORMATION,
    pub flags: u64,
    pub tag_code: u32,
    pub tag: [u8; 4],
}

pub extern "system" fn XMemVirtualQuery(_process: HANDLE, address: *const c_void, out: *mut BasicInformation) -> BOOL {
    if out.is_null() {
        return BOOL(0);
    }
    let n = unsafe { VirtualQuery(Some(address), &mut (*out).host, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
    if n == 0 {
        return BOOL(0);
    }
    let flags = with(|s| s.xmem.find(address as u64).map(|r| r.flags).unwrap_or(0));
    unsafe {
        (*out).flags = if (*out).host.State == MEM_COMMIT || flags != 0 { flags } else { 0 };
        (*out).tag_code = 0;
        (*out).tag = [0; 4];
    }
    BOOL(1)
}

pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    let align = xmem::alloc_alignment(attributes);
    with(|s| {
        let State { xmem, heap, heap32, .. } = s;
        let h = if attributes & ALLOC_32BIT != 0 { heap32 } else { heap };
        h.alloc(xmem, size as u64, align) as *mut c_void
    })
}

pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    with(|s| {
        let State { xmem, heap, heap32, .. } = s;
        let h = if attributes & ALLOC_32BIT != 0 { heap32 } else { heap };
        h.free(xmem, address as u64);
    })
}

/// The title's general allocator. The title's exe may export `__XMemAllocationHooks__` (an alloc and a
/// free routine); the layer installs them with [`set_alloc_hooks`] when it maps the exe.
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks() {
        Some((alloc, _)) => unsafe { alloc(size, attributes) },
        None => XMemAllocDefault(size, attributes),
    }
}

pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks() {
        Some((_, free)) => unsafe { free(address, attributes) },
        None => XMemFreeDefault(address, attributes),
    }
}

type AllocHook = unsafe extern "system" fn(usize, u64) -> *mut c_void;
type FreeHook = unsafe extern "system" fn(*mut c_void, u64);
static HOOKS: Mutex<Option<(usize, usize)>> = Mutex::new(None);

/// Install the title's `__XMemAllocationHooks__` routines (alloc, free).
pub fn set_alloc_hooks(alloc: usize, free: usize) {
    *HOOKS.lock().unwrap() = (alloc != 0 && free != 0).then_some((alloc, free));
}

fn hooks() -> Option<(AllocHook, FreeHook)> {
    let h = (*HOOKS.lock().unwrap())?;
    // SAFETY: installed from the title's own export of these types
    Some(unsafe { (std::mem::transmute::<usize, AllocHook>(h.0), std::mem::transmute::<usize, FreeHook>(h.1)) })
}

pub extern "system" fn XMemAllocatePhysicalPages(_flags: u64, count: *mut usize, pages: *mut usize) -> BOOL {
    if count.is_null() || pages.is_null() {
        return BOOL(0);
    }
    let n = unsafe { *count };
    match with(|s| s.physical.allocate(n)) {
        Some(frames) => {
            for (i, f) in frames.iter().enumerate() {
                unsafe { pages.add(i).write(*f) };
            }
            BOOL(1)
        }
        None => {
            unsafe { *count = 0 };
            BOOL(0)
        }
    }
}

pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return BOOL(0);
    }
    let frames = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    BOOL(with(|s| s.physical.free(&frames)) as i32)
}

/// Map `n` frames contiguously at `address`; a null `pages` unmaps the range instead.
pub extern "system" fn XMemMapPhysicalPages(address: *mut c_void, n: usize, pages: *const usize) -> *mut c_void {
    let base = address as u64;
    let ok = with(|s| {
        (0..n).all(|i| {
            let at = base + (i * FRAME) as u64;
            if pages.is_null() {
                s.physical.unmap(at);
                true
            } else {
                s.physical.map(at, unsafe { *pages.add(i) })
            }
        })
    });
    if ok { address } else { std::ptr::null_mut() }
}

/// Map frame `pages[i]` at `addresses[i]`; a null `pages` unmaps each address.
pub extern "system" fn XMemMapPhysicalPagesScatter(addresses: *const *mut c_void, n: usize, pages: *const usize) -> BOOL {
    if addresses.is_null() {
        return BOOL(0);
    }
    let ok = with(|s| {
        (0..n).all(|i| {
            let at = unsafe { *addresses.add(i) } as u64;
            if pages.is_null() {
                s.physical.unmap(at);
                true
            } else {
                s.physical.map(at, unsafe { *pages.add(i) })
            }
        })
    });
    BOOL(ok as i32)
}

/// Moving budget between the title and the system: nothing to move on the host.
pub extern "system" fn XMemTransferMemory(_kind: u32, _size: usize) -> BOOL {
    BOOL(1)
}

/// The statistics layout is not known yet (discovery); refused rather than guessed.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, _out: *mut c_void) -> BOOL {
    BOOL(0)
}

pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    BOOL(0)
}

/// Our implementation of a console-only function, by DLL and name.
pub fn console_export(dll: &str, name: &str) -> Option<u64> {
    if !dll.eq_ignore_ascii_case("xmem.dll") {
        return None;
    }
    let f: usize = match name {
        "XMemVirtualAlloc" => XMemVirtualAlloc as *const () as usize,
        "XMemVirtualQuery" => XMemVirtualQuery as *const () as usize,
        "XMemAlloc" => XMemAlloc as *const () as usize,
        "XMemFree" => XMemFree as *const () as usize,
        "XMemAllocDefault" => XMemAllocDefault as *const () as usize,
        "XMemFreeDefault" => XMemFreeDefault as *const () as usize,
        "XMemAllocatePhysicalPages" => XMemAllocatePhysicalPages as *const () as usize,
        "XMemFreePhysicalPages" => XMemFreePhysicalPages as *const () as usize,
        "XMemMapPhysicalPages" => XMemMapPhysicalPages as *const () as usize,
        "XMemMapPhysicalPagesScatter" => XMemMapPhysicalPagesScatter as *const () as usize,
        "XMemTransferMemory" => XMemTransferMemory as *const () as usize,
        "XMemGetWorkingSetStatistics" => XMemGetWorkingSetStatistics as *const () as usize,
        "XMemIsToolsMemAvailable" => XMemIsToolsMemAvailable as *const () as usize,
        _ => return None,
    };
    Some(f as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_13_xmem_imports_bind() {
        for n in [
            "XMemAlloc", "XMemAllocDefault", "XMemAllocatePhysicalPages", "XMemFree", "XMemFreeDefault", "XMemFreePhysicalPages",
            "XMemGetWorkingSetStatistics", "XMemIsToolsMemAvailable", "XMemMapPhysicalPages", "XMemMapPhysicalPagesScatter",
            "XMemTransferMemory", "XMemVirtualAlloc", "XMemVirtualQuery",
        ] {
            assert!(console_export("xmem.dll", n).is_some(), "{n}");
        }
        assert!(console_export("xmem.dll", "NotAFunction").is_none());
    }

    #[test]
    fn xmem_alloc_is_aligned_graphics_memory_in_one_window_and_reused_after_free() {
        let attrs = 12u64 << xmem::ALIGNMENT_SHIFT;
        let a = XMemAlloc(1000, attrs) as u64;
        let b = XMemAlloc(5000, attrs) as u64;
        assert!(a != 0 && b != 0 && a % 4096 == 0 && b % 4096 == 0);
        assert!(a >= xmem::WINDOW, "graphics memory is above 4 GiB");
        unsafe { (a as *mut u8).write_bytes(7, 1000) };
        XMemFree(a as *mut c_void, attrs);
        let c = XMemAlloc(900, attrs) as u64;
        assert_eq!(c, a, "a freed block is reused");
        let mut info = std::mem::MaybeUninit::<BasicInformation>::zeroed();
        assert_eq!(XMemVirtualQuery(HANDLE::default(), c as *const c_void, info.as_mut_ptr()).0, 1);
        assert_eq!(unsafe { info.assume_init() }.flags & GRAPHICS, GRAPHICS);
        let low = XMemAlloc(64, ALLOC_32BIT) as u64;
        assert!(low != 0 && low < 1 << 32, "a 32-bit allocation is below 4 GiB");
        for (p, at) in [(b, attrs), (c, attrs), (low, ALLOC_32BIT)] {
            XMemFree(p as *mut c_void, at);
        }
        assert!(graphics_regions().iter().all(|r| r.base >> 32 == (r.base + r.size - 1) >> 32));
    }

    #[test]
    fn physical_pages_through_the_exports() {
        let mut n = 3usize;
        let mut frames = [0usize; 3];
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, frames.as_mut_ptr()).0, 1);
        let base = XMemVirtualAlloc(std::ptr::null_mut(), 8 * FRAME, MEM_RESERVE.0 | PAGES_2M | PAGES_64K_BIT, GRAPHICS, PAGE_READWRITE.0);
        assert!(!base.is_null());
        assert_eq!(XMemMapPhysicalPages(base, 3, frames.as_ptr()), base);
        unsafe { (base as *mut u32).add(FRAME / 4 + 1).write(0xABCD) }; // frame 1, word 1
        let alias = (base as u64 + 6 * FRAME as u64) as *mut c_void;
        assert_eq!(XMemMapPhysicalPagesScatter(&alias, 1, &frames[1]).0, 1);
        assert_eq!(unsafe { (alias as *const u32).add(1).read() }, 0xABCD, "the alias sees frame 1");
        assert_eq!(XMemMapPhysicalPagesScatter(&alias, 1, std::ptr::null()).0, 1);
        assert_eq!(XMemMapPhysicalPages(base, 3, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(3, frames.as_ptr()).0, 1);
    }
}
