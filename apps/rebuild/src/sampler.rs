//! A stack sampler for the title's threads: suspend a thread, read its registers, walk its stack with the
//! unwind tables Windows registered for every image (the title's prepared copies included), resume it.
//! Each frame is named as `image+rva`, from the title's images or the host's modules.
//!
//! While a thread is suspended this only copies: its registers and the top of its stack. Nothing allocates,
//! prints or takes a lock (a suspended thread may hold the heap lock or the function-table lock that
//! `RtlLookupFunctionEntry` takes). The stack is walked on the copy after the thread runs again.

#![cfg(windows)]

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::Debug::{
    GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT, CONTEXT_FLAGS, UNW_FLAG_NHANDLER,
};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION,
    THREAD_SUSPEND_RESUME,
};

/// One WriteFile to stderr: no Rust lock, no allocation (safe while a thread is suspended).
fn beat(b: &[u8]) {
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    if std::env::var_os("REBUILD_SAMPLER_BEATS").is_none() {
        return;
    }
    unsafe {
        if let Ok(h) = GetStdHandle(STD_ERROR_HANDLE) {
            let _ = WriteFile(h, Some(b), None, None);
        }
    }
}

/// CONTEXT_AMD64 | CONTROL | INTEGER
const CONTEXT_CONTROL_INTEGER: u32 = 0x0010_0003;
pub const MAX_FRAMES: usize = 24;

/// A 16-byte aligned CONTEXT, as GetThreadContext requires.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

/// Where a thread was: its return addresses, innermost first.
pub struct Sample {
    pub frames: [u64; MAX_FRAMES],
    pub len: usize,
}

/// Bytes of the stack copied from RSP up.
const STACK_COPY: usize = 256 << 10;

/// Samples one thread (never the calling one). `None` if it could not be suspended or read.
pub fn sample(thread: HANDLE) -> Option<Sample> {
    sample_copy(thread, STACK_COPY)
}

/// [`sample`] with `stack_copy` bytes of stack copied (the profiler copies less, to stop threads for less time).
pub fn sample_copy(thread: HANDLE, stack_copy: usize) -> Option<Sample> {
    let stack_copy = stack_copy.clamp(4096, STACK_COPY);
    let s = Sample { frames: [0; MAX_FRAMES], len: 0 };
    let mut c = Box::new(Aligned(CONTEXT::default())); // allocated before the thread stops
    c.0.ContextFlags = CONTEXT_FLAGS(CONTEXT_CONTROL_INTEGER);
    // the copy lives in a buffer with megabytes of slack after it: unwinding a frame that allocates a large stack
    // area moves RSP far past the copy, and the unwinder reads there before the walk can stop
    STACK.with_borrow_mut(|stack| sample_into(thread, stack_copy, stack, s, c))
}

thread_local! {
    static STACK: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0u8; STACK_COPY + SLACK]);
}

fn sample_into(thread: HANDLE, stack_copy: usize, stack: &mut [u8], mut s: Sample, mut c: Box<Aligned>) -> Option<Sample> {
    let copied;
    unsafe {
        if SuspendThread(thread) == u32::MAX {
            return None;
        }
        beat(b"  s: suspended
");
        let ok = GetThreadContext(thread, &mut c.0).is_ok();
        // copy what is committed of the stack above RSP (ReadProcessMemory stops at an unreadable page)
        let mut n = 0usize;
        if ok {
            let _ = windows::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                windows::Win32::System::Threading::GetCurrentProcess(),
                c.0.Rsp as *const c_void,
                stack.as_mut_ptr() as *mut c_void,
                stack_copy,
                Some(&mut n),
            );
            if n == 0 {
                // partial reads fail whole: retry with what is left of the stack page by page
                let mut len = stack_copy;
                while len >= 4096 && n == 0 {
                    len /= 2;
                    let _ = windows::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                        windows::Win32::System::Threading::GetCurrentProcess(),
                        c.0.Rsp as *const c_void,
                        stack.as_mut_ptr() as *mut c_void,
                        len,
                        Some(&mut n),
                    );
                }
            }
        }
        beat(b"  s: copied
");
        copied = n;
        ResumeThread(thread);
        beat(b"  s: resumed
");
        if !ok {
            return None;
        }
    }
    // walk on the copy: stack addresses are moved into it, code addresses stay
    let (lo, hi) = (c.0.Rsp, c.0.Rsp + copied as u64);
    let delta = stack.as_ptr() as u64 as i64 - lo as i64;
    let moved = |a: u64| if a >= lo && a < hi { (a as i64 + delta) as u64 } else { a };
    c.0.Rsp = moved(c.0.Rsp);
    c.0.Rbp = moved(c.0.Rbp);
    unsafe { walk(&mut c.0, &mut s, stack.as_ptr() as u64, stack.as_ptr() as u64 + copied as u64, (lo, hi, delta)) };
    Some(s)
}

/// Readable bytes after the copied stack (stale data from earlier samples; never part of a walk's result).
const SLACK: usize = 8 << 20;

/// The frame register (x64 register number, 0 = none) a function's unwind data establishes, following chained
/// entries. `f` is a RUNTIME_FUNCTION in the image at `base`.
unsafe fn frame_register(base: u64, f: *const u32) -> u8 {
    let mut f = f;
    for _ in 0..8 {
        let mut data = *f.add(2);
        if data & 1 != 0 {
            // an indirect entry: the RUNTIME_FUNCTION it names
            f = (base + u64::from(data & !1)) as *const u32;
            data = *f.add(2);
        }
        let info = (base + u64::from(data)) as *const u8;
        let reg = *info.add(3) & 0x0f;
        if reg != 0 {
            return reg;
        }
        // UNW_FLAG_CHAININFO: the primary entry follows the (even-padded) unwind codes
        if (*info >> 3) & 0x4 == 0 {
            return 0;
        }
        let codes = usize::from(*info.add(2));
        f = info.add(4 + 2 * (codes + (codes & 1))) as *const u32;
    }
    0
}

fn register(c: &mut CONTEXT, n: u8) -> Option<&mut u64> {
    Some(match n {
        0 => &mut c.Rax,
        1 => &mut c.Rcx,
        2 => &mut c.Rdx,
        3 => &mut c.Rbx,
        4 => &mut c.Rsp,
        5 => &mut c.Rbp,
        6 => &mut c.Rsi,
        7 => &mut c.Rdi,
        8 => &mut c.R8,
        9 => &mut c.R9,
        10 => &mut c.R10,
        11 => &mut c.R11,
        12 => &mut c.R12,
        13 => &mut c.R13,
        14 => &mut c.R14,
        15 => &mut c.R15,
        _ => return None,
    })
}

/// Walks the stack in the copy `[lo, hi)`; stops at a frame whose stack leaves the copy. `real` is the copied
/// range in the thread's own stack and the distance to the copy: a frame register holding a real stack address is
/// moved into the copy, and a frame whose frame register points anywhere else ends the walk (the unwinder would
/// read live memory there, which another thread may be freeing).
unsafe fn walk(c: &mut CONTEXT, s: &mut Sample, lo: u64, hi: u64, real: (u64, u64, i64)) {
    while s.len < MAX_FRAMES && c.Rip != 0 {
        s.frames[s.len] = c.Rip;
        s.len += 1;
        if c.Rsp < lo || c.Rsp + 8 > hi {
            break;
        }
        let mut base = 0u64;
        beat(b"  s: lookup
");
        let f = RtlLookupFunctionEntry(c.Rip, &mut base, None);
        beat(if f.is_null() { b"  s: leaf
" } else { b"  s: unwind
" });
        if f.is_null() {
            // a leaf function: the return address is at [rsp]
            if c.Rsp == 0 {
                break;
            }
            c.Rip = (c.Rsp as *const u64).read_unaligned();
            c.Rsp += 8;
            continue;
        }
        let fr = frame_register(base, f as *const u32);
        if fr != 0 {
            let Some(r) = register(c, fr) else { break };
            if *r >= real.0 && *r < real.1 {
                *r = (*r as i64 + real.2) as u64;
            }
            if *r < lo || *r >= hi {
                break;
            }
        }
        let mut handler_data: *mut c_void = std::ptr::null_mut();
        let mut establisher = 0u64;
        RtlVirtualUnwind(UNW_FLAG_NHANDLER, base, c.Rip, f, c, &mut handler_data, &mut establisher, None);
    }
}

/// Every thread of this process except the caller's: (thread id, handle).
pub fn threads() -> Vec<(u32, HANDLE)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return out };
        let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let (pid, me) = (GetCurrentProcessId(), GetCurrentThreadId());
        let mut more = Thread32First(snap, &mut e).is_ok();
        while more {
            if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, false, e.th32ThreadID) {
                    out.push((e.th32ThreadID, h));
                }
            }
            more = Thread32Next(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Names an address: the title's image it falls in (`images`: name, base, size), else the host module.
pub fn name(addr: u64, images: &[(String, u64, u64)]) -> String {
    if let Some((n, b, _)) = images.iter().find(|(_, b, sz)| addr >= *b && addr < b + sz) {
        return format!("{n}+{:#x}", addr - b);
    }
    unsafe {
        let mut m = HMODULE::default();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(flags, windows::core::PCWSTR(addr as *const u16), &mut m).is_ok() {
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(m, &mut buf) as usize;
            let path = String::from_utf16_lossy(&buf[..n]);
            let file = path.rsplit('\\').next().unwrap_or(&path).to_string();
            return format!("{file}+{:#x}", addr - m.0 as u64);
        }
    }
    format!("{addr:#x}")
}
