//! Crash reports for title runs: where a fault happened, as image+rva, without touching the heap.
//!
//! A vectored handler notes the first few first-chance access violations (the title may handle some itself),
//! and the unhandled-exception filter reports the one that ends the run. Each report: the exception code,
//! the faulting instruction as image+rva, the address it touched, and the stack words that point into the
//! title's images (a heuristic call stack: return addresses are among them). Everything is formatted into a
//! fixed stack buffer and written with one WriteFile, so a crash inside the allocator cannot deadlock it.

#![cfg(windows)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use windows::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
};

/// (name, base, size) of each title image.
static IMAGES: OnceLock<Vec<(String, u64, u64)>> = OnceLock::new();
static FIRST_CHANCE: AtomicU32 = AtomicU32::new(0);
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

struct Buf {
    b: [u8; 2048],
    n: usize,
}

impl std::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

fn write_err(b: &[u8]) {
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    unsafe {
        for h in [STD_ERROR_HANDLE, STD_OUTPUT_HANDLE] {
            if let Ok(h) = GetStdHandle(h) {
                let _ = WriteFile(h, Some(b), None, None);
            }
        }
    }
}

fn name(out: &mut Buf, addr: u64) {
    if let Some((n, b, _)) = IMAGES.get().and_then(|v| v.iter().find(|(_, b, s)| addr >= *b && addr < b + s)) {
        let _ = write!(out, "{n}+{:#x}", addr - b);
    } else {
        let _ = write!(out, "{addr:#x}");
    }
}

fn in_title(addr: u64) -> bool {
    IMAGES.get().is_some_and(|v| v.iter().any(|(_, b, s)| addr >= *b && addr < b + s))
}

static REPORTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe fn report(kind: &str, p: *const EXCEPTION_POINTERS) {
    // a fault inside the report must not report again (and recurse)
    if REPORTING.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe { report_inner(kind, p) };
    REPORTING.store(false, Ordering::Release);
}

unsafe fn report_inner(kind: &str, p: *const EXCEPTION_POINTERS) {
    let mut out = Buf { b: [0; 2048], n: 0 };
    let (rec, ctx) = unsafe { (&*(*p).ExceptionRecord, &*(*p).ContextRecord) };
    let _ = write!(out, "CRASH     {kind}: code {:#010x} at ", rec.ExceptionCode.0 as u32);
    name(&mut out, rec.ExceptionAddress as u64);
    if rec.ExceptionCode == EXCEPTION_ACCESS_VIOLATION && rec.NumberParameters >= 2 {
        let op = ["read", "write", "?", "?", "?", "?", "?", "?", "execute"].get(rec.ExceptionInformation[0]).copied().unwrap_or("?");
        let target = rec.ExceptionInformation[1];
        let _ = write!(out, ", {op} of {target:#x}");
        // what is at that address (a syscall: safe here)
        use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
        let mut m = MEMORY_BASIC_INFORMATION::default();
        if unsafe { VirtualQuery(Some(target as *const _), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } != 0 {
            let state = match m.State.0 { 0x1000 => "committed", 0x2000 => "reserved", 0x10000 => "free", _ => "?" };
            let _ = write!(
                out,
                "
          that address: {state}, region {:#x} + {:#x}, allocation base {:#x}, type {:#x}, protect {:#x}",
                m.BaseAddress as u64, m.RegionSize, m.AllocationBase as u64, m.Type.0, m.Protect.0
            );
        }
        let _ = write!(
            out,
            "
          title budget: {} MiB used of {} MiB",
            rebuild_runtime::budget::used() >> 20,
            rebuild_runtime::budget::limit() >> 20
        );
    }
    let _ = write!(
        out,
        "\n          rax {:#x} rcx {:#x} rdx {:#x} r8 {:#x} r9 {:#x} rsp {:#x}\n          call chain (unwound):\n",
        ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.R8, ctx.R9, ctx.Rsp
    );
    // the facts first, so a fault while unwinding cannot lose them
    write_err(&out.b[..out.n]);
    // the real call chain: unwind a copy of the context with the registered unwind tables, one line per
    // frame as it is found; stop at the first frame no function table knows (no guessing from [rsp])
    use windows::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER};
    let mut c = *ctx;
    for _ in 0..16 {
        if c.Rip == 0 {
            break;
        }
        let mut line = Buf { b: [0; 2048], n: 0 };
        let _ = write!(line, "            ");
        name(&mut line, c.Rip);
        let _ = write!(line, "{}\n", if in_title(c.Rip) { "" } else { "  (host)" });
        write_err(&line.b[..line.n]);
        let mut base = 0u64;
        let f = unsafe { RtlLookupFunctionEntry(c.Rip, &mut base, None) };
        if f.is_null() {
            // a leaf (no unwind entry): its return address is at [rsp], read only inside this thread's stack
            let (mut lo, mut hi) = (0usize, 0usize);
            unsafe { windows::Win32::System::Threading::GetCurrentThreadStackLimits(&mut lo, &mut hi) };
            if (c.Rsp as usize) < lo || (c.Rsp as usize) + 8 > hi {
                break;
            }
            c.Rip = unsafe { (c.Rsp as *const u64).read_volatile() };
            c.Rsp += 8;
            continue;
        }
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut frame = 0u64;
        unsafe { RtlVirtualUnwind(UNW_FLAG_NHANDLER, base, c.Rip, f, &mut c, &mut data, &mut frame, None) };
    }
}

unsafe extern "system" fn first_chance(p: *mut EXCEPTION_POINTERS) -> i32 {
    let code = unsafe { (*(*p).ExceptionRecord).ExceptionCode };
    if code == EXCEPTION_ACCESS_VIOLATION && FIRST_CHANCE.fetch_add(1, Ordering::Relaxed) < 4 {
        unsafe { report("first-chance", p) };
    }
    EXCEPTION_CONTINUE_SEARCH
}

unsafe extern "system" fn unhandled(p: *const EXCEPTION_POINTERS) -> i32 {
    let code: NTSTATUS = unsafe { (*(*p).ExceptionRecord).ExceptionCode };
    let _ = code;
    unsafe { report("unhandled", p) };
    EXCEPTION_CONTINUE_SEARCH
}

/// Install both handlers; `images` names addresses in the reports.
pub fn install(images: Vec<(String, u64, u64)>) {
    let _ = IMAGES.set(images);
    unsafe {
        AddVectoredExceptionHandler(1, Some(first_chance));
        SetUnhandledExceptionFilter(Some(unhandled));
    }
}
