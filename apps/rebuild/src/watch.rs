//! `REBUILD_WATCH=<address>`: a hardware write watchpoint (debug register 0, 8 bytes) on every thread of the
//! run, for finding who stores a value the title later trips over. A watcher thread arms each thread as it
//! appears (suspend, set Dr0/Dr7, resume); a vectored handler reports each write: the value now there, the
//! writing instruction (the one before the reported address) and the unwound call chain. The first 40 writes
//! are printed, writes of zero always marked. Diagnostic only: it suspends threads briefly and allocates.

#![cfg(windows)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::Win32::Foundation::{CloseHandle, EXCEPTION_SINGLE_STEP};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, GetThreadContext, SetThreadContext, CONTEXT, CONTEXT_FLAGS, EXCEPTION_POINTERS,
};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SET_CONTEXT,
    THREAD_SUSPEND_RESUME,
};

static ADDRESS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU32 = AtomicU32::new(0);
/// CONTEXT_AMD64 | CONTEXT_DEBUG_REGISTERS
const DEBUG_REGISTERS: CONTEXT_FLAGS = CONTEXT_FLAGS(0x0010_0010);
/// L0 enabled, RW0 = write, LEN0 = 8 bytes
const DR7_WRITE8: u64 = 1 | (1 << 16) | (3 << 18);

#[repr(align(16))]
struct Ctx(CONTEXT);

/// Start watching if `REBUILD_WATCH` names an address. Call before the title runs.
pub fn install() {
    // an absolute address, or `exe+0x...` (the title image moves between runs), or a preferred address 0x14...
    let Some(a) = std::env::var("REBUILD_WATCH").ok().and_then(|s| {
        let s = s.trim().to_ascii_lowercase();
        let hex = |t: &str| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok();
        match s.strip_prefix("exe+") {
            Some(off) => Some(crate::crash::title_base()? + hex(off)?),
            None => hex(&s).map(|v| if (0x1_4000_0000..0x1_5000_0000).contains(&v) { crate::crash::title_base().map_or(v, |b| b + v - 0x1_4000_0000) } else { v }),
        }
    }) else {
        return;
    };
    ADDRESS.store(a & !7, Ordering::Relaxed);
    // a hit can land inside the host heap: reports must not allocate (no DbgHelp names for host frames)
    crate::crash::NO_SYMBOLS.store(true, Ordering::Relaxed);
    unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    }
    eprintln!("watch     writes to {a:#x} on every thread (debug register 0)");
    std::thread::Builder::new()
        .name("watch-arm".into())
        .spawn(|| {
            let mut armed: HashSet<u32> = HashSet::new();
            let me = unsafe { GetCurrentThreadId() };
            let start = std::time::Instant::now();
            while start.elapsed().as_secs() < 120 {
                arm_new(&mut armed, me);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
        .ok();
}

fn arm_new(armed: &mut HashSet<u32>, me: u32) {
    let pid = unsafe { GetCurrentProcessId() };
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else { return };
    let mut te = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
    let mut ok = unsafe { Thread32First(snap, &mut te) }.is_ok();
    while ok {
        if te.th32OwnerProcessID == pid && te.th32ThreadID != me && armed.insert(te.th32ThreadID) {
            if let Ok(h) = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_SET_CONTEXT, false, te.th32ThreadID) } {
                if unsafe { SuspendThread(h) } != u32::MAX {
                    let mut c = Ctx(CONTEXT { ContextFlags: DEBUG_REGISTERS, ..Default::default() });
                    if unsafe { GetThreadContext(h, &mut c.0) }.is_ok() {
                        c.0.Dr0 = ADDRESS.load(Ordering::Relaxed);
                        c.0.Dr7 = (c.0.Dr7 & !0xF_0003) | DR7_WRITE8;
                        c.0.ContextFlags = DEBUG_REGISTERS;
                        let _ = unsafe { SetThreadContext(h, &c.0) };
                    }
                    unsafe { ResumeThread(h) };
                }
                let _ = unsafe { CloseHandle(h) };
            }
        }
        ok = unsafe { Thread32Next(snap, &mut te) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snap) };
}

unsafe extern "system" fn handler(p: *mut EXCEPTION_POINTERS) -> i32 {
    let rec = unsafe { &*(*p).ExceptionRecord };
    if rec.ExceptionCode != EXCEPTION_SINGLE_STEP {
        return 0; // EXCEPTION_CONTINUE_SEARCH
    }
    let ctx = unsafe { &mut *(*p).ContextRecord };
    if ctx.Dr6 & 1 == 0 {
        return 0;
    }
    ctx.Dr6 = 0;
    let n = HITS.fetch_add(1, Ordering::Relaxed);
    if n < 40 {
        let a = ADDRESS.load(Ordering::Relaxed);
        let v = unsafe { (a as *const u64).read_volatile() };
        let mut out = crate::crash::Line::new();
        let _ = write!(out, "watch     #{n} {:#x} = {v:#x}{} after ", a, if v == 0 { "  <-- ZERO" } else { "" });
        crate::crash::chain(&mut out, ctx, 10);
        out.flush();
    }
    -1 // EXCEPTION_CONTINUE_EXECUTION
}
