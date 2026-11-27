//! `REBUILD_TRACE_ALLOC`: every new range our allocators hand the title (XMemVirtualAlloc / VirtualAlloc
//! reservations, the XMem default heap) is checked against the live ones; an overlap is named with both callers.
//! A commit inside an existing reservation is not a new range and is not checked. Diagnostic only.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

struct Live {
    size: u64,
    what: &'static str,
    caller: String,
}

static LIVE: Mutex<BTreeMap<u64, Live>> = Mutex::new(BTreeMap::new());
static REPORTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One clock for the trace lines of this run (map calls, polls).
pub fn epoch() -> std::time::Instant {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    *T0.get_or_init(std::time::Instant::now)
}

/// `REBUILD_TRACE_ALLOC`: a physical-page map call (address, count, first frame; frames null = unmap).
pub fn map(what: &'static str, address: u64, n: usize, first_frame: Option<usize>) {
    if !on() {
        return;
    }
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3000 {
        eprintln!(
            "pmap      {:>9.3} ms {what} {address:#x} x{n} frames {}",
            epoch().elapsed().as_secs_f64() * 1e3,
            first_frame.map_or("none (unmap)".to_string(), |f| format!("{f:#x}.."))
        );
    }
}

/// An address that already showed a frame when the title mapped it again elsewhere (the first such), for a
/// second watchpoint.
pub static ALIAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_TRACE_ALLOC").is_some())
}

fn caller() -> String {
    let mut frames = [std::ptr::null_mut::<std::ffi::c_void>(); 6];
    let n = unsafe { windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(2, &mut frames, None) } as usize;
    frames[..n].iter().map(|&f| crate::device::module_offset(f as usize)).collect::<Vec<_>>().join(" ")
}

/// A new range `[base, base+size)` handed out by `what`.
pub fn alloc(what: &'static str, base: u64, size: u64) {
    if !on() || base == 0 || size == 0 {
        return;
    }
    let c = caller();
    if size >= 16 << 20 {
        eprintln!("bigalloc  {what} [{base:#x}+{size:#x}) from {c}");
    }
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    // the nearest live range starting at or below the end of the new one
    if let Some((&b, l)) = live.range(..base + size).next_back() {
        if b + l.size > base && REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 30 {
            eprintln!(
                "overlap   {what} [{base:#x}+{size:#x}) from {c}\n          overlaps {} [{b:#x}+{:#x}) from {}",
                l.what, l.size, l.caller
            );
        }
    }
    live.insert(base, Live { size, what, caller: c });
}

/// The range at `base` is given back.
pub fn free(base: u64) {
    if on() {
        LIVE.lock().unwrap_or_else(|e| e.into_inner()).remove(&base);
    }
}
