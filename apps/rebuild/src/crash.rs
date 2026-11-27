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

/// `<REBUILD_SESSION_DIR>\crash-<pid>.txt` as a wide string, made at install (no allocation in the handler).
static CRASH_FILE: OnceLock<Vec<u16>> = OnceLock::new();

fn write_err(b: &[u8]) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, WriteFile, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_ALWAYS,
    };
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    unsafe {
        for h in [STD_ERROR_HANDLE, STD_OUTPUT_HANDLE] {
            if let Ok(h) = GetStdHandle(h) {
                let _ = WriteFile(h, Some(b), None, None);
            }
        }
        // the session folder's crash report: opened per write, appended (kernel calls only)
        if let Some(path) = CRASH_FILE.get() {
            if let Ok(f) = CreateFileW(
                windows::core::PCWSTR(path.as_ptr()),
                FILE_APPEND_DATA.0,
                FILE_SHARE_READ,
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            ) {
                let _ = WriteFile(f, Some(b), None, None);
                let _ = CloseHandle(f);
            }
        }
    }
}

fn name(out: &mut Buf, addr: u64) {
    if let Some((n, b, _)) = IMAGES.get().and_then(|v| v.iter().find(|(_, b, s)| addr >= *b && addr < b + s)) {
        let _ = write!(out, "{n}+{:#x}", addr - b);
    } else if !host_symbol(out, addr) {
        let _ = write!(out, "{addr:#x}");
    }
}

/// A host address as `function+offset` from its module's symbols (DbgHelp, loaded at the first crash; ours have
/// a PDB beside the exe). Diagnostic only: DbgHelp allocates.
fn host_symbol(out: &mut Buf, addr: u64) -> bool {
    use windows::Win32::System::Diagnostics::Debug::{SymFromAddr, SymInitialize, SymSetOptions, SYMBOL_INFO};
    use windows::Win32::System::Threading::GetCurrentProcess;
    static READY: OnceLock<bool> = OnceLock::new();
    let ready = *READY.get_or_init(|| unsafe {
        SymSetOptions(0x0000_0004 | 0x0000_0002 | 0x0000_0200); // DEFERRED_LOADS | UNDNAME | FAIL_CRITICAL_ERRORS
        SymInitialize(GetCurrentProcess(), None, true).is_ok()
    });
    if !ready {
        return false;
    }
    #[repr(C)]
    struct Sym {
        info: SYMBOL_INFO,
        name: [u8; 256],
    }
    let mut s: Sym = unsafe { std::mem::zeroed() };
    s.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
    s.info.MaxNameLen = 256;
    let mut disp = 0u64;
    if unsafe { SymFromAddr(GetCurrentProcess(), addr, Some(&mut disp), &mut s.info) }.is_err() {
        // no symbol: the module and the offset into it
        use windows::Win32::System::LibraryLoader::{GetModuleFileNameA, GetModuleHandleExA, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT};
        let mut m = windows::Win32::Foundation::HMODULE::default();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if unsafe { GetModuleHandleExA(flags, windows::core::PCSTR(addr as *const u8), &mut m) }.is_err() {
            return false;
        }
        let mut path = [0u8; 260];
        let n = unsafe { GetModuleFileNameA(m, &mut path) } as usize;
        let file = path[..n].rsplit(|&b| b == b'\\').next().unwrap_or(&[]);
        let _ = write!(out, "{}+{:#x}", String::from_utf8_lossy(file), addr - m.0 as u64);
        return true;
    }
    let len = (s.info.NameLen as usize).min(255);
    // SAFETY: the name continues past SYMBOL_INFO's one-byte array into `name`
    let name = unsafe { std::slice::from_raw_parts(s.info.Name.as_ptr() as *const u8, len) };
    let _ = write!(out, "{}+{disp:#x}", String::from_utf8_lossy(name));
    true
}

fn in_title(addr: u64) -> bool {
    IMAGES.get().is_some_and(|v| v.iter().any(|(_, b, s)| addr >= *b && addr < b + s))
}

static PEEKS: std::sync::OnceLock<Vec<(bool, u64)>> = std::sync::OnceLock::new();

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
    // REBUILD_PEEK: title globals (preferred addresses, 0x14...) or rsp+N, eight bytes each, for chasing asserts;
    // after the facts, and each address checked readable first
    if let Some(peeks) = PEEKS.get() {
        use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
        let title = IMAGES.get().and_then(|v| v.iter().find(|i| i.0.ends_with(".exe"))).map(|i| i.1).unwrap_or(0);
        let mut pk = Buf { b: [0; 2048], n: 0 };
        for &(stack, off) in peeks {
            let at = if stack { ctx.Rsp + off } else { title + off };
            let mut m = MEMORY_BASIC_INFORMATION::default();
            let ok = unsafe { VirtualQuery(Some(at as *const _), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } != 0
                && m.State.0 == 0x1000
                && m.Protect.0 & 0x66 != 0;
            let tag = if stack { "rsp+" } else { "exe+" };
            if ok {
                let v = unsafe { (at as *const u64).read_unaligned() };
                let _ = write!(pk, "          peek {tag}{off:#x} = {v:#x}
");
            } else {
                let _ = write!(pk, "          peek {tag}{off:#x} unreadable
");
            }
        }
        write_err(&pk.b[..pk.n]);
    }
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

/// Called once at the first fatal-looking exception (a breakpoint the title raises, a fail-fast), after the
/// report: the caller prints what it knows then (the device's trap list). Set by the run.
static ON_FATAL: OnceLock<fn()> = OnceLock::new();

pub fn on_fatal(f: fn()) {
    let _ = ON_FATAL.set(f);
}

const STATUS_BREAKPOINT: u32 = 0x8000_0003;
const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409; // __fastfail

unsafe extern "system" fn first_chance(p: *mut EXCEPTION_POINTERS) -> i32 {
    let code = unsafe { (*(*p).ExceptionRecord).ExceptionCode };
    if code == EXCEPTION_ACCESS_VIOLATION && FIRST_CHANCE.fetch_add(1, Ordering::Relaxed) < 4 {
        unsafe { report("first-chance", p) };
    }
    // a title assert (int3) or a fail-fast ends the run without reaching the unhandled filter: report it here
    let c = code.0 as u32;
    if c == CPP_EXCEPTION && CPP.fetch_add(1, Ordering::Relaxed) < 6 {
        unsafe { cpp_throw(p) };
    }
    const ILLEGAL: u32 = 0xC000_001D;
    const PRIVILEGED: u32 = 0xC000_0096;
    if matches!(c, STATUS_BREAKPOINT | STATUS_STACK_BUFFER_OVERRUN | ILLEGAL | PRIVILEGED) && FATAL.fetch_add(1, Ordering::Relaxed) == 0 {
        let what = match c {
            STATUS_BREAKPOINT => "breakpoint (a title assert)",
            STATUS_STACK_BUFFER_OVERRUN => "fail-fast",
            ILLEGAL => "illegal instruction",
            _ => "privileged instruction",
        };
        unsafe { report(what, p) };
        if let Some(f) = ON_FATAL.get() {
            f();
        }
        if std::env::var_os("REBUILD_THREADS").is_some() {
            other_threads();
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}

static FATAL: AtomicU32 = AtomicU32::new(0);
static CPP: AtomicU32 = AtomicU32::new(0);
const CPP_EXCEPTION: u32 = 0xE06D_7363;

/// A C++ throw (first chance; the title may catch it): the thrown type's decorated name, from the throw
/// info the MSVC runtime passes (x64: image-relative offsets), and where it was thrown from.
unsafe fn cpp_throw(p: *const EXCEPTION_POINTERS) {
    let rec = unsafe { &*(*p).ExceptionRecord };
    let mut out = Buf { b: [0; 2048], n: 0 };
    let _ = write!(out, "c++ throw ");
    let ctx = unsafe { &*(*p).ContextRecord };
    // the caller of _CxxThrowException: the first title frame on the stack at the throw
    name(&mut out, ctx.Rip);
    if rec.NumberParameters >= 4 {
        let (info, base) = (rec.ExceptionInformation[2] as u64, rec.ExceptionInformation[3] as u64);
        let readable = |a: u64, n: usize| {
            use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
            let mut m = MEMORY_BASIC_INFORMATION::default();
            a != 0
                && unsafe { VirtualQuery(Some(a as *const _), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } != 0
                && m.State.0 == 0x1000
                && m.Protect.0 & 0x66 != 0
                && a + n as u64 <= m.BaseAddress as u64 + m.RegionSize as u64
        };
        if readable(info, 16) {
            let array = base + unsafe { ((info + 12) as *const u32).read_unaligned() } as u64;
            if readable(array, 8) {
                let first = base + unsafe { ((array + 4) as *const u32).read_unaligned() } as u64;
                if readable(first, 8) {
                    let ty = base + unsafe { ((first + 4) as *const u32).read_unaligned() } as u64;
                    if readable(ty + 16, 64) {
                        let _ = write!(out, " type ");
                        for i in 0..96u64 {
                            let b = unsafe { ((ty + 16 + i) as *const u8).read() };
                            if b == 0 || !readable(ty + 17 + i, 1) {
                                break;
                            }
                            let _ = out.write_char(b as char);
                        }
                        // a thrown integer (".H" int, ".J" long: an HRESULT, usually): its value
                        let obj = rec.ExceptionInformation[1] as u64;
                        if readable(obj, 4) {
                            let _ = write!(out, " value {:#x}", unsafe { (obj as *const u32).read_unaligned() });
                        }
                    }
                }
            }
        }
    }
    let _ = write!(out, "
          chain:");
    let mut c = *ctx;
    unwind_into(&mut out, &mut c, 14);
    let _ = write!(out, "
");
    write_err(&out.b[..out.n]);
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
    if let Ok(list) = std::env::var("REBUILD_PEEK") {
        let parse = |t: &str| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok();
        let peeks = list
            .split(',')
            .filter_map(|t| match t.trim().strip_prefix("rsp+") {
                Some(o) => parse(o).map(|o| (true, o)),
                None => parse(t.trim()).map(|a| (false, a.saturating_sub(0x1_4000_0000))),
            })
            .collect();
        let _ = PEEKS.set(peeks);
    }
    if let Some(dir) = std::env::var_os("REBUILD_SESSION_DIR") {
        let path = std::path::PathBuf::from(dir).join(format!("crash-{}.txt", std::process::id()));
        use std::os::windows::ffi::OsStrExt;
        let _ = CRASH_FILE.set(path.as_os_str().encode_wide().chain(std::iter::once(0)).collect());
    }
    unsafe {
        AddVectoredExceptionHandler(1, Some(first_chance));
        SetUnhandledExceptionFilter(Some(unhandled));
    }
}

/// REBUILD_THREADS: at the first fatal exception, where every other thread of the process stands (suspended,
/// unwound to 12 frames; threads with no title frame are counted, not printed). Diagnostic only: a thread
/// suspended inside the unwinder's lock could stall this, which the run's watchdog ends.
fn other_threads() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::Debug::{
        GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT, CONTEXT_FLAGS, UNW_FLAG_NHANDLER,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
    use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION,
        THREAD_SUSPEND_RESUME,
    };
    #[repr(align(16))]
    struct Ctx(CONTEXT);
    let pid = unsafe { GetCurrentProcessId() };
    let me = unsafe { GetCurrentThreadId() };
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else { return };
    let mut te = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
    let mut quiet = 0;
    let mut ok = unsafe { Thread32First(snap, &mut te) }.is_ok();
    while ok {
        if te.th32OwnerProcessID == pid && te.th32ThreadID != me {
            if let Ok(h) = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, false, te.th32ThreadID) } {
                if unsafe { SuspendThread(h) } != u32::MAX {
                    let mut cx = Ctx(CONTEXT { ContextFlags: CONTEXT_FLAGS(0x0010_000B), ..Default::default() });
                    if unsafe { GetThreadContext(h, &mut cx.0) }.is_ok() {
                        let mut out = Buf { b: [0; 2048], n: 0 };
                        let _ = write!(out, "thread    {}:", te.th32ThreadID);
                        let mut c = cx.0;
                        let mut titled = false;
                        for _ in 0..12 {
                            if c.Rip == 0 {
                                break;
                            }
                            let _ = write!(out, " ");
                            name(&mut out, c.Rip);
                            titled |= in_title(c.Rip);
                            let mut base = 0u64;
                            let f = unsafe { RtlLookupFunctionEntry(c.Rip, &mut base, None) };
                            if f.is_null() {
                                let mut m = MEMORY_BASIC_INFORMATION::default();
                                let readable = unsafe { VirtualQuery(Some(c.Rsp as *const _), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } != 0
                                    && m.State.0 == 0x1000
                                    && m.Protect.0 & 0x66 != 0;
                                if !readable {
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
                        let _ = write!(out, "
");
                        if titled {
                            write_err(&out.b[..out.n]);
                        } else {
                            quiet += 1;
                        }
                    }
                    unsafe { ResumeThread(h) };
                }
                let _ = unsafe { CloseHandle(h) };
            }
        }
        ok = unsafe { Thread32Next(snap, &mut te) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snap) };
    let mut out = Buf { b: [0; 2048], n: 0 };
    let _ = write!(out, "thread    {quiet} more threads with no title frame
");
    write_err(&out.b[..out.n]);
}

/// Unwind `c` up to `frames` frames, naming each (one line); stops at a frame no table knows and whose
/// [rsp] is not readable.
fn unwind_into(out: &mut Buf, c: &mut windows::Win32::System::Diagnostics::Debug::CONTEXT, frames: usize) {
    use windows::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER};
    use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
    for _ in 0..frames {
        if c.Rip == 0 {
            break;
        }
        let _ = write!(out, " ");
        name(out, c.Rip);
        let mut base = 0u64;
        let f = unsafe { RtlLookupFunctionEntry(c.Rip, &mut base, None) };
        if f.is_null() {
            let mut m = MEMORY_BASIC_INFORMATION::default();
            let ok = unsafe { VirtualQuery(Some(c.Rsp as *const _), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } != 0
                && m.State.0 == 0x1000
                && m.Protect.0 & 0x66 != 0;
            if !ok {
                break;
            }
            c.Rip = unsafe { (c.Rsp as *const u64).read_volatile() };
            c.Rsp += 8;
            continue;
        }
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut frame = 0u64;
        unsafe { RtlVirtualUnwind(UNW_FLAG_NHANDLER, base, c.Rip, f, c, &mut data, &mut frame, None) };
    }
}
