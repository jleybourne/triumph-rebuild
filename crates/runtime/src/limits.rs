//! User-set hardware limits, so people can test how the title runs on less than their machine has
//! (owner request, 2026-11-27): RAM, VRAM, CPU cores and a CPU usage cap. 0 means no limit.
//!
//! Applied before the title starts:
//!
//! - **RAM**: the title's memory budget ([`crate::budget`]) and the job object's cap on the process's own
//!   commit both shrink to the limit, so the title sees an out-of-memory where a smaller machine would.
//! - **CPU cores**: the process (and every thread it makes) runs on the first N logical processors.
//! - **CPU rate**: a hard cap on the job's CPU time, in percent of the whole machine.
//! - **VRAM**: recorded here; our GPU heaps enforce it from Build 5 (the title's GPU memory goes through them).
//!
//! The launcher writes them as settings (`limits.*` in `crates/config`); tools read `REBUILD_LIMIT_RAM_MB`,
//! `REBUILD_LIMIT_VRAM_MB`, `REBUILD_LIMIT_CPU_CORES` and `REBUILD_LIMIT_CPU_RATE`.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub ram_mb: u64,
    pub vram_mb: u64,
    pub cpu_cores: u32,
    /// percent of the whole machine, 1-100
    pub cpu_rate: u32,
}

static ACTIVE: OnceLock<Limits> = OnceLock::new();

impl Limits {
    /// From the `REBUILD_LIMIT_*` environment variables (absent or unreadable = no limit).
    pub fn from_env() -> Limits {
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        Limits {
            ram_mb: get("REBUILD_LIMIT_RAM_MB"),
            vram_mb: get("REBUILD_LIMIT_VRAM_MB"),
            cpu_cores: get("REBUILD_LIMIT_CPU_CORES") as u32,
            cpu_rate: get("REBUILD_LIMIT_CPU_RATE").min(100) as u32,
        }
    }

    pub fn ram_bytes(&self) -> Option<u64> {
        (self.ram_mb > 0).then(|| self.ram_mb << 20)
    }

    pub fn vram_bytes(&self) -> Option<u64> {
        (self.vram_mb > 0).then(|| self.vram_mb << 20)
    }

    /// The affinity mask for the first `cpu_cores` of `available` logical processors (`None`: no limit).
    pub fn affinity(&self, available: u32) -> Option<usize> {
        let n = self.cpu_cores.min(available).min(usize::BITS);
        (self.cpu_cores > 0).then(|| if n >= usize::BITS { usize::MAX } else { (1usize << n) - 1 })
    }

    /// One line per limit in force, for the run's log.
    pub fn describe(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.ram_mb > 0 {
            v.push(format!("RAM {} MiB", self.ram_mb));
        }
        if self.vram_mb > 0 {
            v.push(format!("VRAM {} MiB (enforced by our heaps from Build 5)", self.vram_mb));
        }
        if self.cpu_cores > 0 {
            v.push(format!("{} CPU cores", self.cpu_cores));
        }
        if self.cpu_rate > 0 {
            v.push(format!("CPU {} %", self.cpu_rate));
        }
        v
    }
}

/// Set the limits for this run (once, before the title starts; later calls are ignored).
pub fn set(l: Limits) {
    let _ = ACTIVE.set(l);
}

/// The limits in force (none until [`set`]).
pub fn get() -> Limits {
    ACTIVE.get().copied().unwrap_or_default()
}

/// Apply the CPU limits to this process and its job (the RAM cap is part of the job's memory limit, set by
/// the caller from [`crate::budget`]). Returns what was applied.
#[cfg(windows)]
pub fn apply_cpu(job: windows::Win32::Foundation::HANDLE) -> Result<Vec<String>, String> {
    use windows::Win32::System::JobObjects::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessAffinityMask};
    let l = get();
    let mut done = Vec::new();
    let available = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    if let Some(mask) = l.affinity(available) {
        unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask) }.map_err(|e| format!("CPU cores: {e}"))?;
        done.push(format!("{} of {available} logical processors", l.cpu_cores.min(available)));
    }
    if l.cpu_rate > 0 {
        let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            ..Default::default()
        };
        info.Anonymous.CpuRate = l.cpu_rate * 100; // in 1/100 of a percent
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        }
        .map_err(|e| format!("CPU rate: {e}"))?;
        done.push(format!("CPU capped at {} %", l.cpu_rate));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_affinity_mask_takes_the_first_cores_and_never_more_than_exist() {
        let l = Limits { cpu_cores: 4, ..Default::default() };
        assert_eq!(l.affinity(16), Some(0b1111));
        assert_eq!(Limits { cpu_cores: 32, ..Default::default() }.affinity(8), Some(0xFF));
        assert_eq!(Limits::default().affinity(8), None);
    }

    #[test]
    fn zero_means_no_limit_and_each_limit_is_described() {
        assert!(Limits::default().describe().is_empty());
        assert_eq!(Limits::default().ram_bytes(), None);
        let l = Limits { ram_mb: 8192, vram_mb: 6144, cpu_cores: 6, cpu_rate: 50 };
        assert_eq!(l.ram_bytes(), Some(8 << 30));
        assert_eq!(l.describe().len(), 4);
    }
}
