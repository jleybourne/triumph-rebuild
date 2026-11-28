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

/// A live physical frame's 64 KiB, by frame number (DirectStorage page destinations write here).
pub fn frame_ptr(frame: usize) -> Option<*mut u8> {
    with(|s| s.physical.frame_ptr(frame))
}

/// Graphics regions committed so far (for the tiles and the upload ring to register).
pub fn graphics_regions() -> Vec<Region> {
    with(|s| s.xmem.graphics().copied().collect())
}

/// Run `f` over the write-watched reservations while no allocation or free can run (the memory model's catch-up
/// at device creation: what the title placed before there was a device).
pub fn with_watched<R>(f: impl FnOnce(&[(u64, u64)]) -> R) -> R {
    with(|s| f(&s.xmem.watched()))
}

// ------------------------------------------------------------------------------------- xmem.dll

pub extern "system" fn XMemVirtualAlloc(base: *mut c_void, size: usize, kind: u32, flags: u64, protect: u32) -> *mut c_void {
    with(|s| {
        // a 64 KiB-page reservation with no commit: a placeholder the title will map physical pages into
        if kind & (PAGES_2M | PAGES_64K_BIT) == (PAGES_2M | PAGES_64K_BIT) && kind & MEM_COMMIT.0 == 0 {
            if base.is_null() {
                let a = reserve_placeholder(size);
                if a == 0 {
                    eprintln!("xmem      placeholder reserve failed: size {size:#x}: {}", std::io::Error::last_os_error());
                } else {
                    s.xmem.record(Region { base: a, size: size as u64, flags, protect });
                    crate::overlap::alloc("XMemVirtualAlloc placeholder", a, size as u64);
                }
                return a as *mut c_void;
            }
            // a 64 KiB-page reservation over a piece of a placeholder the title split (MEM_REPLACE_PLACEHOLDER): the
            // piece already is the placeholder physical pages map into (physical.rs), so it stays one; only
            // recorded. Turning it into a plain reservation would leave nothing to map into.
            if kind & MEM_REPLACE_PLACEHOLDER.0 != 0 {
                s.xmem.record(Region { base: base as u64, size: size as u64, flags, protect });
                return base;
            }
        }
        let a = host::virtual_alloc(&mut s.xmem, base as u64, size as u64, kind, flags, protect);
        if base.is_null() || kind & MEM_RESERVE.0 != 0 {
            crate::overlap::alloc("XMemVirtualAlloc", a, size as u64);
        }
        a as *mut c_void
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
        // a cold slice's read-only mark is ours (write tracking), not the title's
        (*out).host.Protect = PAGE_PROTECTION_FLAGS(rebuild_d3d12x::vmem::effective(address as u64, (*out).host.Protect.0));
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
        let a = h.alloc(xmem, size as u64, align);
        crate::overlap::alloc("XMemAllocDefault", a, size as u64);
        a as *mut c_void
    })
}

pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    with(|s| {
        let State { xmem, heap, heap32, .. } = s;
        let h = if attributes & ALLOC_32BIT != 0 { heap32 } else { heap };
        crate::overlap::free(address as u64);
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
            eprintln!("xmem      allocate physical pages failed: {n} frames ({} MiB used of the budget)", crate::budget::used() >> 20);
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
    crate::overlap::map("map", base, n, (!pages.is_null() && n > 0).then(|| unsafe { *pages }));
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
    if !ok {
        static FAILED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
            eprintln!("xmem      map physical pages failed: address {base:#x} pages {n}: {}", std::io::Error::last_os_error());
        }
    }
    if ok { address } else { std::ptr::null_mut() }
}

/// Map frame `pages[i]` at `addresses[i]`; a null `pages` unmaps each address.
pub extern "system" fn XMemMapPhysicalPagesScatter(addresses: *const *mut c_void, n: usize, pages: *const usize) -> BOOL {
    if addresses.is_null() {
        return BOOL(0);
    }
    if n > 0 {
        crate::overlap::map("scatter", unsafe { *addresses } as u64, n, (!pages.is_null()).then(|| unsafe { *pages }));
        if crate::overlap::on() && !pages.is_null() {
            let first = unsafe { *pages };
            let at = unsafe { *addresses } as u64;
            let pairs: Vec<String> = (0..n.min(6)).map(|i| unsafe { format!("{:#x}->{:#x}", *addresses.add(i) as u64, *pages.add(i)) }).collect();
            let raw: Vec<String> = (0..8).map(|i| unsafe { format!("{:#x}", *(pages as *const u32).add(i)) }).collect();
            eprintln!("scatter   n {n}: {} | pages as u32: {}", pairs.join(" "), raw.join(" "));
            let already = with(|s| s.physical.mapped_at(first));
            if let Some(&other) = already.iter().find(|&&a| a != at) {
                eprintln!("alias     frame {first:#x} mapped at {at:#x} is already mapped at {other:#x}");
                let _ = crate::overlap::ALIAS.compare_exchange(0, other, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let ok = with(|s| {
        (0..n).all(|i| {
            let at = unsafe { *addresses.add(i) } as u64;
            // a null array, or page 0 in it, unmaps that address (the title writes 0 for "no page")
            let page = if pages.is_null() { 0 } else { unsafe { *pages.add(i) } };
            if page == 0 {
                s.physical.unmap(at);
                true
            } else {
                s.physical.map(at, page)
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

// ----------------------------------------------------------------- memory and process overrides

/// The console's `VirtualAlloc`: protection bits above the host's (0x800-0x40000) are GPU access.
/// GPU-reachable memory is placed like `XMemVirtualAlloc` graphics memory (inside one window,
/// write-watched); everything else is the host's call, unchanged.
pub extern "system" fn VirtualAlloc(base: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void {
    let gpu = protect & !xmem::HOST_PROTECTION != 0;
    with(|s| {
        let flags = if gpu { GRAPHICS } else { 0 };
        let a = host::virtual_alloc(&mut s.xmem, base as u64, size as u64, kind, flags, protect);
        if base.is_null() || kind & MEM_RESERVE.0 != 0 {
            crate::overlap::alloc("VirtualAlloc", a, size as u64);
        }
        a as *mut c_void
    })
}

pub extern "system" fn VirtualFree(base: *mut c_void, size: usize, kind: u32) -> BOOL {
    if kind & 0x8000 != 0 {
        crate::overlap::free(base as u64); // MEM_RELEASE
    }
    BOOL(with(|s| host::virtual_free(&mut s.xmem, base as u64, size as u64, kind)) as i32)
}

static COMMAND_LINE_A: Mutex<Option<std::ffi::CString>> = Mutex::new(None);
static COMMAND_LINE_W: Mutex<Option<Vec<u16>>> = Mutex::new(None);

/// Set what the title sees as its command line (the layer sets its console exe path); never the host's.
pub fn set_command_line(line: &str) {
    *COMMAND_LINE_A.lock().unwrap() = std::ffi::CString::new(line).ok();
    *COMMAND_LINE_W.lock().unwrap() = Some(line.encode_utf16().chain(std::iter::once(0)).collect());
}

pub extern "system" fn GetCommandLineA() -> *const u8 {
    let mut g = COMMAND_LINE_A.lock().unwrap();
    g.get_or_insert_with(|| std::ffi::CString::new(r"G:\default.exe").unwrap()).as_ptr() as *const u8
}

pub extern "system" fn GetCommandLineW() -> *const u16 {
    let mut g = COMMAND_LINE_W.lock().unwrap();
    g.get_or_insert_with(|| r"G:\default.exe".encode_utf16().chain(std::iter::once(0)).collect()).as_ptr()
}

/// Memory and process overrides, by name.
pub fn override_export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "VirtualAlloc" => VirtualAlloc as *const (),
        "VirtualFree" => VirtualFree as *const (),
        "GetCommandLineA" => GetCommandLineA as *const (),
        "GetCommandLineW" => GetCommandLineW as *const (),
        // the console's device type (registry OEM\ConsoleMode)
        other if other.starts_with("Reg") => return crate::registry::export(other).or_else(|| rebuild_winfiles::override_export(other)),
        // files: console drives through the overlay file system (lane `files`, PR #12)
        other => return rebuild_winfiles::override_export(other),
    };
    Some(f as usize as u64)
}

// ------------------------------------------------------------------------------- d3d12_xs.dll

/// What `D3D12XboxCreateDevice` does when it is reached: the startup run stops here (Build 3's
/// closing point), or later the D3D12.X device is made (Build 5).
static ON_DEVICE: Mutex<Option<fn(&[u64; 4])>> = Mutex::new(None);

pub fn on_create_device(f: fn(&[u64; 4])) {
    *ON_DEVICE.lock().unwrap() = Some(f);
}

/// The first render call: `(adapter = null, params, riid, out)` (docs/research/d3d12x-device.md section 1).
/// Logs what the title asked for, then makes the D3D12.X device (`crate::device::create`).
pub extern "system" fn D3D12XboxCreateDevice(a: u64, b: u64, c: u64, d: u64) -> i32 {
    eprintln!("first render call reached: D3D12XboxCreateDevice({a:#x}, {b:#x}, {c:#x}, {d:#x})");
    // the interface id and the creation parameters, once: the fork never decoded past the version word
    if c != 0 {
        let g = unsafe { std::slice::from_raw_parts(c as *const u8, 16) };
        let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
        let d2 = u16::from_le_bytes([g[4], g[5]]);
        let d3 = u16::from_le_bytes([g[6], g[7]]);
        eprintln!(
            "  riid {d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
        );
    }
    if b != 0 {
        let w = unsafe { std::slice::from_raw_parts(b as *const u32, 16) };
        eprintln!("  params {:08x?}", w);
    }
    let hr = crate::device::create(c as *const [u8; 16], d as *mut *mut c_void);
    if let Some(f) = *ON_DEVICE.lock().unwrap() {
        f(&[a, b, c, d]);
    }
    hr
}

/// `D3D12ConfigureVirtualMemoryX(*{flags u32, page_table_pages u32, preserved_bytes u32, pad; out lowest u64,
/// out highest u64})` (docs/research/d3d12x-device.md section 1). The title may size its GPU address layout
/// from the range: ours is where graphics memory lives, from the first 4 GiB window to the top of user space.
pub extern "system" fn D3D12ConfigureVirtualMemoryX(desc: *mut u8) -> i32 {
    if desc.is_null() {
        return 0x8007_0057u32 as i32; // E_INVALIDARG
    }
    unsafe {
        (desc.add(0x10) as *mut u64).write_unaligned(crate::xmem::WINDOW);
        (desc.add(0x18) as *mut u64).write_unaligned(USER_SPACE_TOP);
    }
    0
}

/// The highest user-mode address on 64-bit Windows.
pub const USER_SPACE_TOP: u64 = 0x7FFF_FFFE_FFFF;

fn host_d3d12(name: &str) -> Option<u64> {
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    let w: Vec<u16> = "d3d12.dll".encode_utf16().chain(std::iter::once(0)).collect();
    let m = unsafe { LoadLibraryExW(PCWSTR(w.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
    let c = std::ffi::CString::new(name).ok()?;
    unsafe { GetProcAddress(m, PCSTR(c.as_ptr() as *const u8)) }.map(|f| f as usize as u64)
}

type SerializeVersioned = unsafe extern "system" fn(*const u8, *mut *mut c_void, *mut *mut c_void) -> i32;
static HOST_SERIALIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
/// Root signature flags the host's D3D12 defines (through SAMPLER_HEAP_DIRECTLY_INDEXED, 0x800).
const HOST_ROOT_FLAGS: u32 = 0xfff;
static CONSOLE_ROOT_FLAGS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Console root signature flags seen so far (bits the host does not define; serialized without them).
pub fn console_root_flags() -> u32 {
    CONSOLE_ROOT_FLAGS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `D3D12SerializeVersionedRootSignature` for console descriptions: the console sets flag bits of its own
/// (0x1000000, 0x2000000 from the BVH builder) that the host rejects as E_INVALIDARG. The host serializes a
/// copy with only its own flags (at +0x28 in versions 1.0, 1.1 and 1.2 alike).
unsafe extern "system" fn serialize_versioned_root_signature(desc: *const u8, blob: *mut *mut c_void, errors: *mut *mut c_void) -> i32 {
    let Some(&host) = HOST_SERIALIZE.get() else { return 0x8000_4005u32 as i32 };
    // SAFETY: the host's export of this signature
    let host: SerializeVersioned = unsafe { std::mem::transmute(host) };
    if desc.is_null() {
        return unsafe { host(desc, blob, errors) };
    }
    let mut copy = [0u8; 0x30];
    unsafe { std::ptr::copy_nonoverlapping(desc, copy.as_mut_ptr(), 0x30) };
    let flags = u32::from_le_bytes(copy[0x28..0x2c].try_into().unwrap());
    if flags & !HOST_ROOT_FLAGS != 0 {
        CONSOLE_ROOT_FLAGS.fetch_or(flags & !HOST_ROOT_FLAGS, std::sync::atomic::Ordering::Relaxed);
        copy[0x28..0x2c].copy_from_slice(&(flags & HOST_ROOT_FLAGS).to_le_bytes());
    }
    // ask for the host's error text ourselves, so a refusal can be named even when the title passes no `errors`
    let mut own_errors: *mut c_void = std::ptr::null_mut();
    let errs = if errors.is_null() { &mut own_errors as *mut *mut c_void } else { errors };
    let hr = unsafe { host(copy.as_ptr(), blob, errs) };
    if hr < 0 {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
            let version = u32::from_le_bytes(copy[0..4].try_into().unwrap());
            let params = u32::from_le_bytes(copy[8..12].try_into().unwrap());
            let samplers = u32::from_le_bytes(copy[0x18..0x1c].try_into().unwrap());
            let mut text = String::new();
            let e = unsafe { *errs };
            if !e.is_null() {
                // ID3DBlob: GetBufferPointer (slot 3), GetBufferSize (slot 4)
                unsafe {
                    let vt = *(e as *const *const usize);
                    let ptr: unsafe extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(*vt.add(3));
                    let len: unsafe extern "system" fn(*mut c_void) -> usize = std::mem::transmute(*vt.add(4));
                    text = String::from_utf8_lossy(std::slice::from_raw_parts(ptr(e), len(e))).trim_end_matches('\0').trim().to_string();
                }
            }
            eprintln!("rootsig   serialize refused {hr:#x}: version {version}, {params} parameters, {samplers} static samplers, flags {flags:#x}: {text}");
        }
    }
    if !own_errors.is_null() {
        // release our own error blob (IUnknown::Release, slot 2)
        unsafe {
            let vt = *(own_errors as *const *const usize);
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(2));
            release(own_errors);
        }
    }
    hr
}

/// Our implementation of a console-only function, by DLL and name.
/// The console-only DLLs we serve: `LoadLibrary` of these never reaches the host, even where the host
/// has a DLL of the same name (the PC's own `xgameruntime.dll` would take console GUIDs).
pub const CONSOLE_DLLS: &[&str] =
    &["d3d12_xs.dll", "xmem.dll", "acphal.dll", "xapu.dll", "chad.dll", "xcrdapi.dll", "xg_xs.dll", "xgameruntime.dll", "amd_fidelityfx_loader_xbox.dll", "amd_fidelityfx_upscaler_xbox.dll"];

/// Title DLLs served from here instead of mapped (lane `ffx`): AMD's FidelityFX loader and upscaler for
/// Xbox, whose upscale is console code. `REBUILD_FFX=title` maps the title's own again.
pub fn replaces_title_dll(dll: &str) -> bool {
    rebuild_ffx::enabled() && rebuild_ffx::DLLS.iter().any(|d| d.eq_ignore_ascii_case(dll))
}

pub fn is_console_dll(dll: &str) -> bool {
    CONSOLE_DLLS.iter().any(|d| d.eq_ignore_ascii_case(dll))
}

pub fn console_export(dll: &str, name: &str) -> Option<u64> {
    if dll.eq_ignore_ascii_case("d3d12_xs.dll") {
        return match name {
            "D3D12XboxCreateDevice" => Some(D3D12XboxCreateDevice as *const () as usize as u64),
            "D3D12ConfigureVirtualMemoryX" => Some(D3D12ConfigureVirtualMemoryX as *const () as usize as u64),
            // standard: the host's own
            "D3D12SerializeVersionedRootSignature" => {
                let host = host_d3d12(name)?;
                let _ = HOST_SERIALIZE.set(host as usize);
                Some(serialize_versioned_root_signature as *const () as usize as u64)
            }
            "D3D12CreateRootSignatureDeserializer" => host_d3d12(name),
            _ => None,
        };
    }
    let lower = dll.to_ascii_lowercase();
    match lower.as_str() {
        "xmem.dll" => {}
        "acphal.dll" | "xapu.dll" | "chad.dll" => return audio_export(&lower, name),
        "xcrdapi.dll" => {
            return (name == "DStorageGetFactory").then(|| rebuild_dstorage::DStorageGetFactory as *const () as usize as u64)
        }
        "xg_xs.dll" => return crate::xg::export(name),
        "xgameruntime.dll" => return rebuild_gdk::runtime::get_proc(name).map(|f| f as usize as u64),
        "amd_fidelityfx_loader_xbox.dll" | "amd_fidelityfx_upscaler_xbox.dll" => return rebuild_ffx::export(name),
        _ => return None,
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

/// The 19 audio functions (lane `audio`, PR #11), by the DLL the title imports them from.
fn audio_export(dll: &str, name: &str) -> Option<u64> {
    use rebuild_audio::{acp, heap, xapu, xdsp};
    let f: *const () = match (dll, name) {
        ("acphal.dll", "AcpHalCreate") => acp::AcpHalCreate as *const (),
        ("acphal.dll", "AcpHalAllocateShapeContexts") => acp::AcpHalAllocateShapeContexts as *const (),
        ("acphal.dll", "AcpHalReleaseShapeContexts") => acp::AcpHalReleaseShapeContexts as *const (),
        ("acphal.dll", "ApuAlloc") => heap::ApuAlloc as *const (),
        ("acphal.dll", "ApuFree") => heap::ApuFree as *const (),
        ("acphal.dll", "ApuCreateHeap") => heap::ApuCreateHeap as *const (),
        ("acphal.dll", "ApuIsVirtualAddressValid") => heap::ApuIsVirtualAddressValid as *const (),
        ("acphal.dll", "ApuMapApuAddress") => heap::ApuMapApuAddress as *const (),
        ("acphal.dll", "ApuMapVirtualAddress") => heap::ApuMapVirtualAddress as *const (),
        ("xapu.dll", "XApuConnect") => xapu::XApuConnect as *const (),
        ("xapu.dll", "XApuDisconnect") => xapu::XApuDisconnect as *const (),
        ("xapu.dll", "XApuEnqueueCommand") => xapu::XApuEnqueueCommand as *const (),
        ("xapu.dll", "XApuDequeueResult") => xapu::XApuDequeueResult as *const (),
        ("xapu.dll", "XApuGetResultCount") => xapu::XApuGetResultCount as *const (),
        ("chad.dll", "XDspActivate") => xdsp::XDspActivate as *const (),
        ("chad.dll", "XDspDeactivate") => xdsp::XDspDeactivate as *const (),
        ("chad.dll", "XDspConnectWithMaximumStreamLimit") => xdsp::XDspConnectWithMaximumStreamLimit as *const (),
        ("chad.dll", "XDspDisconnect") => xdsp::XDspDisconnect as *const (),
        ("chad.dll", "XDspSubmitCommand") => xdsp::XDspSubmitCommand as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_root_signature_flags_serialize_on_the_host() {
        let f = console_export("d3d12_xs.dll", "D3D12SerializeVersionedRootSignature").expect("bound");
        let f: SerializeVersioned = unsafe { std::mem::transmute(f as usize) };
        // version 1.1, no parameters or samplers, the BVH builder's console flags plus ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
        let mut desc = [0u8; 0x30];
        desc[0] = 2;
        desc[0x28..0x2c].copy_from_slice(&0x0300_0001u32.to_le_bytes());
        let mut blob: *mut c_void = std::ptr::null_mut();
        assert_eq!(unsafe { f(desc.as_ptr(), &mut blob, std::ptr::null_mut()) }, 0);
        assert!(!blob.is_null());
        // release the blob (IUnknown::Release, slot 2)
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*(*(blob as *const *const usize)).add(2));
            release(blob);
        }
        assert_eq!(console_root_flags() & 0x0300_0000, 0x0300_0000);
    }

    #[test]
    fn the_merged_lanes_bind_audio_19_dstorage_1_xg_2_files_32_crt_13() {
        let audio = [
            ("acphal.dll", "AcpHalCreate"), ("acphal.dll", "AcpHalAllocateShapeContexts"), ("acphal.dll", "AcpHalReleaseShapeContexts"),
            ("acphal.dll", "ApuAlloc"), ("acphal.dll", "ApuFree"), ("acphal.dll", "ApuCreateHeap"), ("acphal.dll", "ApuIsVirtualAddressValid"),
            ("acphal.dll", "ApuMapApuAddress"), ("acphal.dll", "ApuMapVirtualAddress"), ("xapu.dll", "XApuConnect"), ("xapu.dll", "XApuDisconnect"),
            ("xapu.dll", "XApuEnqueueCommand"), ("xapu.dll", "XApuDequeueResult"), ("xapu.dll", "XApuGetResultCount"), ("chad.dll", "XDspActivate"),
            ("chad.dll", "XDspDeactivate"), ("chad.dll", "XDspConnectWithMaximumStreamLimit"), ("chad.dll", "XDspDisconnect"), ("chad.dll", "XDspSubmitCommand"),
        ];
        for (d, n) in audio {
            assert!(console_export(d, n).is_some(), "{d}!{n}");
        }
        assert!(console_export("XCRDAPI.dll", "DStorageGetFactory").is_some());
        assert!(console_export("xg_xs.dll", "XGCreateTextureComputer").is_some());
        assert!(console_export("xg_xs.dll", "XGSuggestSwizzleMode").is_some());
        assert!(console_export("chad.dll", "XApuConnect").is_none(), "by the DLL it lives in");
        // every files override is in the runtime's list, and binds to ours
        let files: Vec<&str> = crate::OVERRIDES.iter().filter(|(_, w)| *w == "files").map(|(f, _)| *f).collect();
        assert_eq!(files, rebuild_winfiles::FUNCTIONS, "the runtime's files list is the files crate's, in order");
        for f in files {
            assert_eq!(crate::modules::override_export(f), rebuild_winfiles::override_export(f), "{f}");
        }
        let crt: Vec<&str> = crate::OVERRIDES.iter().filter(|(_, w)| *w == "crt").map(|(f, _)| *f).collect();
        assert_eq!(crt, rebuild_winfiles::crt::CRT_FUNCTIONS, "the runtime's crt list is the files crate's, in order");
        for f in crt {
            assert!(crate::modules::override_export(f).is_some(), "{f}");
            assert_eq!(crate::modules::override_export(f), rebuild_winfiles::override_export(f), "{f}");
        }
    }

    #[test]
    fn the_fidelityfx_dlls_are_ours() {
        for dll in ["amd_fidelityfx_loader_xbox.dll", "AMD_FidelityFX_Upscaler_Xbox.dll"] {
            assert!(is_console_dll(dll));
            for n in ["ffxCreateContext", "ffxDestroyContext", "ffxDispatch", "ffxQuery", "ffxConfigure"] {
                assert_eq!(console_export(dll, n), rebuild_ffx::export(n), "{dll}!{n}");
            }
        }
        assert_eq!(replaces_title_dll("amd_fidelityfx_loader_xbox.dll"), rebuild_ffx::enabled());
        assert!(!replaces_title_dll("bink2scarlett.dll"));
    }

    #[test]
    fn configure_virtual_memory_fills_the_graphics_range() {
        let mut desc = [0u8; 0x20];
        desc[4] = 4; // page_table_pages, as the title passes
        assert_eq!(D3D12ConfigureVirtualMemoryX(desc.as_mut_ptr()), 0);
        let lowest = u64::from_le_bytes(desc[0x10..0x18].try_into().unwrap());
        let highest = u64::from_le_bytes(desc[0x18..0x20].try_into().unwrap());
        assert_eq!((lowest, highest), (crate::xmem::WINDOW, USER_SPACE_TOP));
        assert_eq!(desc[4], 4, "the inputs are left as they were");
        assert_eq!(D3D12ConfigureVirtualMemoryX(std::ptr::null_mut()), 0x8007_0057u32 as i32);
    }

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
    fn virtual_alloc_with_gpu_bits_lands_in_one_window_and_plain_memory_stays_the_hosts() {
        let size = 4 << 20;
        let g = VirtualAlloc(std::ptr::null_mut(), size, (MEM_RESERVE | MEM_COMMIT).0, PAGE_READWRITE.0 | 0x800) as u64;
        assert!(g >= xmem::WINDOW && g >> 32 == (g + size as u64 - 1) >> 32, "GPU memory in one window: {g:#x}");
        assert!(graphics_regions().iter().any(|r| r.base == g));
        let p = VirtualAlloc(std::ptr::null_mut(), 65536, (MEM_RESERVE | MEM_COMMIT).0, PAGE_READWRITE.0) as u64;
        assert!(p != 0 && !graphics_regions().iter().any(|r| r.base == p), "plain memory is not graphics");
        assert_eq!(VirtualFree(g as *mut c_void, 0, MEM_RELEASE.0).0, 1);
        assert_eq!(VirtualFree(p as *mut c_void, 0, MEM_RELEASE.0).0, 1);
        set_command_line(r"G:\title.exe");
        let c = unsafe { std::ffi::CStr::from_ptr(GetCommandLineA() as *const std::ffi::c_char) };
        assert_eq!(c.to_str().unwrap(), r"G:\title.exe");
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
