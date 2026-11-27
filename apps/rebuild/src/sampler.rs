//! A stack sampler for the title's threads: suspend a thread, read its registers, walk its stack with the
//! unwind tables Windows registered for every image (the title's prepared copies included), resume it.
//! Each frame is named as `image+rva`, from the title's images or the host's modules.
//!
//! While a thread is suspended nothing here allocates or prints: a suspended thread may hold the heap lock.

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

/// Samples one thread (never the calling one). `None` if it could not be suspended or read.
pub fn sample(thread: HANDLE) -> Option<Sample> {
    let mut s = Sample { frames: [0; MAX_FRAMES], len: 0 };
    let mut c = Box::new(Aligned(CONTEXT::default())); // allocated before the thread stops
    c.0.ContextFlags = CONTEXT_FLAGS(CONTEXT_CONTROL_INTEGER);
    unsafe {
        if SuspendThread(thread) == u32::MAX {
            return None;
        }
        let ok = GetThreadContext(thread, &mut c.0).is_ok();
        if ok {
            walk(&mut c.0, &mut s);
        }
        ResumeThread(thread);
        ok.then_some(s)
    }
}

unsafe fn walk(c: &mut CONTEXT, s: &mut Sample) {
    while s.len < MAX_FRAMES && c.Rip != 0 {
        s.frames[s.len] = c.Rip;
        s.len += 1;
        let mut base = 0u64;
        let f = RtlLookupFunctionEntry(c.Rip, &mut base, None);
        if f.is_null() {
            // a leaf function: the return address is at [rsp]
            if c.Rsp == 0 {
                break;
            }
            c.Rip = (c.Rsp as *const u64).read_unaligned();
            c.Rsp += 8;
            continue;
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
