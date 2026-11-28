//! Kernel writes into the title's memory, guarded for cold slices (`rebuild_uma::cold`).
//!
//! A cold slice is read-only so that the first CPU write faults. A *kernel* write there (a `ReadFile` into a
//! buffer in the title's graphics memory: DirectStorage's reads, Bink's, the title's own) does not fault: the
//! syscall fails. So the syscalls that write a caller's buffer for I/O are hooked in `ntdll` and call
//! [`rebuild_uma::cold::kernel_write`] on their output buffers first, which makes those ranges writable and
//! their slices hot (pinned for the call, and for a grace after it for asynchronous completions; a pending
//! file read has its buffer locked by then).
//!
//! Hooked: `NtReadFile`, `NtReadFileScatter`, `NtDeviceIoControlFile`, `NtFsControlFile` (and their I/O status
//! blocks). Every caller in the process goes through them (kernel32, the CRT, our DirectStorage). A stub is
//! hooked only if it is the exact x64 syscall stub this expects; if any is not, nothing is hooked and slices
//! never go cold (the ring then reads all watched memory every pass, as before).
//!
//! The stub's first 14 bytes become `jmp [rip+0]; dq hook`; the hook calls a copy of the whole original stub
//! (24 bytes, position-independent: its one branch is inside it). Installed before the title runs.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::sync::OnceLock;

use windows::core::s;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Memory::*;

use rebuild_uma::cold::kernel_write;

const STUB: usize = 24;

/// The x64 syscall stub: `mov r10,rcx; mov eax,N; test byte [7FFE0308h],1; jne +3; syscall; ret; int 2Eh; ret`.
fn is_stub(b: &[u8; STUB]) -> bool {
    b[0..4] == [0x4C, 0x8B, 0xD1, 0xB8] && b[8..18] == [0xF6, 0x04, 0x25, 0x08, 0x03, 0xFE, 0x7F, 0x01, 0x75, 0x03] && b[18..24] == [0x0F, 0x05, 0xC3, 0xCD, 0x2E, 0xC3]
}

static READ: AtomicUsize = AtomicUsize::new(0);
static SCATTER: AtomicUsize = AtomicUsize::new(0);
static IOCTL: AtomicUsize = AtomicUsize::new(0);
static FSCTL: AtomicUsize = AtomicUsize::new(0);

type Nt9 = unsafe extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize, usize) -> i32;
type Nt10 = unsafe extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize, usize, usize) -> i32;

/// `NtReadFile(file, event, apc, apc_context, iosb, buffer, length, offset, key)`
unsafe extern "system" fn read_file(a0: usize, a1: usize, a2: usize, a3: usize, iosb: usize, buf: usize, len: usize, a7: usize, a8: usize) -> i32 {
    let _b = kernel_write(buf as u64, len as u32 as u64);
    let _s = kernel_write(iosb as u64, 16);
    let f: Nt9 = unsafe { std::mem::transmute(READ.load(Relaxed)) };
    unsafe { f(a0, a1, a2, a3, iosb, buf, len, a7, a8) }
}

/// `NtReadFileScatter(file, event, apc, apc_context, iosb, segments, length, offset, key)`: `segments` is an
/// array of page addresses (one 64-bit element a page), `length` bytes in all.
unsafe extern "system" fn read_file_scatter(a0: usize, a1: usize, a2: usize, a3: usize, iosb: usize, seg: usize, len: usize, a7: usize, a8: usize) -> i32 {
    let pages = (len as u32 as usize).div_ceil(4096);
    let mut guards = Vec::new();
    if seg != 0 {
        for i in 0..pages {
            // SAFETY: the caller's segment array, `pages` elements (the syscall reads it the same way)
            let p = unsafe { (seg as *const u64).add(i).read_unaligned() };
            guards.push(kernel_write(p, 4096));
        }
    }
    let _s = kernel_write(iosb as u64, 16);
    let f: Nt9 = unsafe { std::mem::transmute(SCATTER.load(Relaxed)) };
    let r = unsafe { f(a0, a1, a2, a3, iosb, seg, len, a7, a8) };
    drop(guards);
    r
}

/// `NtDeviceIoControlFile(file, event, apc, apc_context, iosb, code, in, in_len, out, out_len)`
unsafe extern "system" fn device_io_control(a0: usize, a1: usize, a2: usize, a3: usize, iosb: usize, a5: usize, a6: usize, a7: usize, out: usize, out_len: usize) -> i32 {
    let _o = kernel_write(out as u64, out_len as u32 as u64);
    let _s = kernel_write(iosb as u64, 16);
    let f: Nt10 = unsafe { std::mem::transmute(IOCTL.load(Relaxed)) };
    unsafe { f(a0, a1, a2, a3, iosb, a5, a6, a7, out, out_len) }
}

/// `NtFsControlFile`: the same arguments as `NtDeviceIoControlFile`.
unsafe extern "system" fn fs_control(a0: usize, a1: usize, a2: usize, a3: usize, iosb: usize, a5: usize, a6: usize, a7: usize, out: usize, out_len: usize) -> i32 {
    let _o = kernel_write(out as u64, out_len as u32 as u64);
    let _s = kernel_write(iosb as u64, 16);
    let f: Nt10 = unsafe { std::mem::transmute(FSCTL.load(Relaxed)) };
    unsafe { f(a0, a1, a2, a3, iosb, a5, a6, a7, out, out_len) }
}

/// Hook the I/O syscalls (once). True when all are hooked; false (and none hooked) otherwise.
pub fn install() -> bool {
    static DONE: OnceLock<bool> = OnceLock::new();
    *DONE.get_or_init(|| unsafe { hook_all() })
}

unsafe fn hook_all() -> bool {
    let Ok(ntdll) = (unsafe { GetModuleHandleA(s!("ntdll.dll")) }) else { return false };
    let targets: [(windows::core::PCSTR, &AtomicUsize, usize); 4] = [
        (s!("NtReadFile"), &READ, read_file as *const () as usize),
        (s!("NtReadFileScatter"), &SCATTER, read_file_scatter as *const () as usize),
        (s!("NtDeviceIoControlFile"), &IOCTL, device_io_control as *const () as usize),
        (s!("NtFsControlFile"), &FSCTL, fs_control as *const () as usize),
    ];
    // every stub first: all or none
    let mut stubs = Vec::new();
    for (name, _, _) in &targets {
        let Some(p) = (unsafe { GetProcAddress(ntdll, *name) }) else { return false };
        let at = p as usize;
        // SAFETY: ntdll's code, mapped and readable
        let bytes: [u8; STUB] = unsafe { std::ptr::read(at as *const [u8; STUB]) };
        if !is_stub(&bytes) {
            eprintln!("kwrite    {} is not the syscall stub expected ({:02x?}): cold slices stay off", unsafe { name.to_string() }.unwrap_or_default(), &bytes[..8]);
            return false;
        }
        stubs.push((at, bytes));
    }
    // the copies the hooks call
    let code = unsafe { VirtualAlloc(None, 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as *mut u8;
    if code.is_null() {
        return false;
    }
    for (i, (_, bytes)) in stubs.iter().enumerate() {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), code.add(i * 32), STUB) };
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    if unsafe { VirtualProtect(code as *const c_void, 4096, PAGE_EXECUTE_READ, &mut old) }.is_err() {
        return false;
    }
    for (i, (_, slot, _)) in targets.iter().enumerate() {
        slot.store(code as usize + i * 32, SeqCst);
    }
    // then the jumps
    for ((at, _), (_, _, hook)) in stubs.iter().zip(targets.iter()) {
        let mut jump = [0u8; 14];
        jump[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
        jump[6..].copy_from_slice(&(*hook as u64).to_le_bytes());
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if unsafe { VirtualProtect(*at as *const c_void, 14, PAGE_EXECUTE_READWRITE, &mut old) }.is_err() {
            // a stub hooked before this one stays hooked: harmless (the hook only adds the guard)
            return false;
        }
        unsafe { std::ptr::copy_nonoverlapping(jump.as_ptr(), *at as *mut u8, 14) };
        let mut o2 = PAGE_PROTECTION_FLAGS(0);
        let _ = unsafe { VirtualProtect(*at as *const c_void, 14, old, &mut o2) };
        let _ = unsafe { windows::Win32::System::Diagnostics::Debug::FlushInstructionCache(windows::Win32::System::Threading::GetCurrentProcess(), Some(*at as *const c_void), 14) };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_uma::cold;

    /// A file read into a cold slice succeeds through the hook, and write-watch sees what it wrote.
    #[test]
    fn a_read_file_into_cold_memory_succeeds() {
        assert!(install(), "this Windows' ntdll stubs are the ones expected");
        cold::enable();
        let dir = std::env::temp_dir().join(format!("kwrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        std::fs::write(&path, vec![0xABu8; 8192]).unwrap();
        // aligned watched memory, one slice
        let r = unsafe { VirtualAlloc(None, 2 * cold::SLICE, MEM_RESERVE, PAGE_NOACCESS) } as u64;
        unsafe { VirtualFree(r as *mut c_void, 0, MEM_RELEASE).unwrap() };
        let p = r.div_ceil(cold::SLICE as u64) * cold::SLICE as u64;
        let ww = VIRTUAL_ALLOCATION_TYPE(0x0020_0000);
        assert_eq!(unsafe { VirtualAlloc(Some(p as *const c_void), cold::SLICE, MEM_RESERVE | MEM_COMMIT | ww, PAGE_READWRITE) } as u64, p);
        let s = cold::slice(p, true).unwrap();
        assert!(s.go_cold(&[(p, p + cold::SLICE as u64)], |_, _| {}));
        // std's File::read is ReadFile -> NtReadFile
        use std::io::Read;
        let mut f = std::fs::File::open(&path).unwrap();
        let buf = unsafe { std::slice::from_raw_parts_mut((p + 65536) as *mut u8, 8192) };
        f.read_exact(buf).expect("the read lands in cold memory");
        assert_eq!(buf[8191], 0xAB);
        assert_eq!(s.state(), cold::HOT);
        let mut a = vec![std::ptr::null_mut(); 64];
        let (mut n, mut g) = (64usize, 0u32);
        unsafe { GetWriteWatch(1, (p + 65536) as *const c_void, 8192, Some(a.as_mut_ptr()), Some(&mut n), Some(&mut g)) };
        assert_eq!(n, 2, "both pages the kernel wrote are in write-watch");
        cold::forget(p, cold::SLICE as u64);
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
