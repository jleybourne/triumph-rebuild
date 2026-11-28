//! The process's resource use for the in-game monitor: CPU (share of the whole machine, as Task Manager shows),
//! memory (working set and private commit) and the card's VRAM (in use and the budget DXGI gives this process).
//! Sampled at most every 500 ms; reading it is a lock and a copy.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIAdapter3, IDXGIFactory4, DXGI_CREATE_FACTORY_FLAGS, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// One sample.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    /// CPU time of the process over the last interval, as a share of all logical processors (0-100)
    pub cpu_pct: Option<f64>,
    /// working set, MiB
    pub ram_mb: Option<f64>,
    /// private commit, MiB
    pub commit_mb: Option<f64>,
    /// the card's local memory: in use by this process and its budget, MiB
    pub vram_mb: Option<(f64, f64)>,
}

struct State {
    at: Instant,
    cpu_100ns: u64,
    last: Sample,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
const EVERY: Duration = Duration::from_millis(500);

fn ft(f: FILETIME) -> u64 {
    (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
}

fn cpu_100ns() -> Option<u64> {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: the current process's pseudo-handle, four out-parameters
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.ok()?;
    Some(ft(k) + ft(u))
}

fn memory() -> (Option<f64>, Option<f64>) {
    let mut m = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: the EX struct with its size in cb, passed as the base struct
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) }.is_ok();
    if !ok {
        return (None, None);
    }
    let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
    (Some(mib(m.WorkingSetSize)), Some(mib(m.PrivateUsage)))
}

fn vram() -> Option<(f64, f64)> {
    let d = crate::device::get()?;
    // SAFETY: plain DXGI calls on the device's own adapter
    unsafe {
        let luid = d.host.GetAdapterLuid();
        let f: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let a: IDXGIAdapter3 = f.EnumAdapterByLuid(luid).ok()?;
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).ok()?;
        let _ = a.as_raw();
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        Some((mib(info.CurrentUsage), mib(info.Budget)))
    }
}

/// CPU share from two process-time readings over `wall`, on `cpus` logical processors.
pub fn cpu_share(before_100ns: u64, after_100ns: u64, wall: Duration, cpus: usize) -> f64 {
    let busy = after_100ns.saturating_sub(before_100ns) as f64 * 100e-9;
    let total = wall.as_secs_f64() * cpus.max(1) as f64;
    if total <= 0.0 {
        0.0
    } else {
        (busy / total * 100.0).clamp(0.0, 100.0)
    }
}

/// The latest sample (a new one when the last is older than 500 ms).
pub fn sample() -> Sample {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if let Some(s) = g.as_ref() {
        if now.duration_since(s.at) < EVERY {
            return s.last;
        }
    }
    let cpu = cpu_100ns();
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let cpu_pct = match (g.as_ref(), cpu) {
        (Some(s), Some(c)) => Some(cpu_share(s.cpu_100ns, c, now.duration_since(s.at), cpus)),
        _ => None,
    };
    let (ram_mb, commit_mb) = memory();
    let last = Sample { cpu_pct, ram_mb, commit_mb, vram_mb: vram() };
    *g = Some(State { at: now, cpu_100ns: cpu.unwrap_or(0), last });
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_share_is_a_share_of_every_core() {
        // one core busy for the whole second on a 4-core machine: 25 %
        assert!((cpu_share(0, 10_000_000, Duration::from_secs(1), 4) - 25.0).abs() < 1e-9);
        assert_eq!(cpu_share(5, 5, Duration::from_secs(1), 8), 0.0);
        assert_eq!(cpu_share(0, 1, Duration::ZERO, 8), 0.0);
        // a sample right away has memory figures (CPU needs two)
        let s = sample();
        assert!(s.ram_mb.is_some_and(|m| m > 0.0));
    }
}
