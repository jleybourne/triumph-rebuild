//! The title's static TLS in slot 0, at no cost per access.
//!
//! An exe always gets TLS index 0, so the title's code reaches its thread-local block as `gs:[0x58]` (the
//! thread's TLS array) then `[array]`, without reading `_tls_index` (found at `malloc`: the allocator lives at
//! block + 0x13A8). Loaded as a DLL beside our host exe, the title gets another index k, and slot 0 is our
//! host's block: the title read our memory as its allocator and crashed.
//!
//! Our host code (Rust through LLVM) always reads `_tls_index`. So instead of patching the title's code (the
//! fork turns each `gs:[0x58]` load into a thunk call), we swap the two blocks in every thread's TLS array and
//! swap the indices: the title's `_tls_index` becomes 0 and ours becomes k. Existing threads are swapped once;
//! each new thread is swapped by a TLS callback of our exe, which the loader runs before the thread's own
//! code. The same callback then runs the title exe's TLS callbacks with `DLL_THREAD_ATTACH`, which the
//! loader cannot (they are cleared in the prepared copy).

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// The title's former index k (0: no swap in force).
static SWAP: AtomicU32 = AtomicU32::new(0);
/// The title exe's base and its TLS callbacks (absolute addresses), for new threads.
static TITLE_BASE: AtomicUsize = AtomicUsize::new(0);
static CALLBACKS: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];

extern "C" {
    /// The CRT's TLS index of our own exe (written by the loader, read by every thread-local access).
    static mut _tls_index: u32;
}

#[repr(C)]
struct ThreadBasicInformation {
    exit_status: i32,
    teb: *mut c_void,
    client_id: [usize; 2],
    affinity: usize,
    priority: i32,
    base_priority: i32,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationThread(thread: *mut c_void, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

/// The TLS array of the current thread (TEB + 0x58).
#[inline(always)]
unsafe fn my_array() -> *mut *mut c_void {
    let teb: *mut u8;
    unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags)) };
    unsafe { *(teb.add(0x58) as *const *mut *mut c_void) }
}

unsafe fn swap(array: *mut *mut c_void, k: usize) {
    if array.is_null() {
        return;
    }
    unsafe { std::ptr::swap(array, array.add(k)) };
}

type Callback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);

/// Our exe's TLS callback: on every new thread, put the title's block in slot 0 first (no thread-local is
/// touched before), then run the title's thread-attach callbacks.
unsafe extern "system" fn on_thread(_module: *mut c_void, reason: u32, _r: *mut c_void) {
    const DLL_THREAD_ATTACH: u32 = 2;
    let k = SWAP.load(Ordering::Acquire);
    if reason != DLL_THREAD_ATTACH || k == 0 {
        return;
    }
    unsafe { swap(my_array(), k as usize) };
    let base = TITLE_BASE.load(Ordering::Acquire);
    for c in &CALLBACKS {
        let f = c.load(Ordering::Acquire);
        if f == 0 {
            break;
        }
        let f: Callback = unsafe { std::mem::transmute(f) };
        unsafe { f(base as *mut c_void, DLL_THREAD_ATTACH, std::ptr::null_mut()) };
    }
}

#[used]
#[link_section = ".CRT$XLY"]
static TLS_CALLBACK: Callback = on_thread;

/// Give the title slot 0. `title_index` is the title exe's `_tls_index` (where its TLS directory points);
/// `base` and `callbacks` are the title exe's base and TLS callbacks (absolute), run for each new thread.
/// Call once, right after the title is loaded and before any other thread runs our or the title's code.
/// Returns the title's former index (0: nothing to do).
///
/// # Safety
/// `title_index` points at the loaded title's `_tls_index`.
pub unsafe fn give_slot0(title_index: *mut u32, base: usize, callbacks: &[usize]) -> Result<u32, String> {
    // keep the callback in the link
    let _ = unsafe { std::ptr::read_volatile(&TLS_CALLBACK) };
    let k = unsafe { title_index.read_volatile() };
    if k == 0 {
        return Ok(0);
    }
    if callbacks.len() > CALLBACKS.len() {
        return Err(format!("{} TLS callbacks (room for {})", callbacks.len(), CALLBACKS.len()));
    }
    // the other threads first (the loader's workers and any other): their TEBs by thread id
    let me = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    for id in threads() {
        if id == me {
            continue;
        }
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
        let Ok(h) = (unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, id) }) else { continue };
        let mut info = ThreadBasicInformation { exit_status: 0, teb: std::ptr::null_mut(), client_id: [0; 2], affinity: 0, priority: 0, base_priority: 0 };
        let r = unsafe { NtQueryInformationThread(h.0, 0, &mut info as *mut _ as *mut c_void, std::mem::size_of::<ThreadBasicInformation>() as u32, std::ptr::null_mut()) };
        unsafe {
            let _ = CloseHandle(h);
        }
        if r == 0 && !info.teb.is_null() {
            let array = unsafe { *((info.teb as *mut u8).add(0x58) as *const *mut *mut c_void) };
            unsafe { swap(array, k as usize) };
        }
    }
    // this thread and the indices together, with no thread-local access in between
    unsafe {
        swap(my_array(), k as usize);
        _tls_index = k;
        title_index.write_volatile(0);
    }
    TITLE_BASE.store(base, Ordering::Release);
    for (slot, f) in CALLBACKS.iter().zip(callbacks) {
        slot.store(*f, Ordering::Release);
    }
    SWAP.store(k, Ordering::Release);
    Ok(k)
}

/// Thread ids of this process.
fn threads() -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
    let mut v = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return v };
        let pid = windows::Win32::System::Threading::GetCurrentProcessId();
        let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut more = Thread32First(snap, &mut e).is_ok();
        while more {
            if e.th32OwnerProcessID == pid {
                v.push(e.th32ThreadID);
            }
            more = Thread32Next(snap, &mut e).is_ok();
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
    }
    v
}
