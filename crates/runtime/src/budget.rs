//! The title's memory budget: a hard cap on what the title can take from the host.
//!
//! A console title asks for its whole budget at start (physical pages, ~13 GiB on the big console) and
//! may keep asking until a call fails. On the host, physical pages are pagefile sections: their commit
//! is charged to the **system**, not to the process, so a job's per-process limit does not see it, and
//! an unbounded title exhausts the system commit limit. Then Windows cannot start any process (Task
//! Manager included) until a reboot. So every physical-page section is charged here first, and a
//! request past the budget fails as the console fails one past its budget.
//!
//! Default: the smaller of 12 GiB and a third of the host's physical memory. `REBUILD_MEMORY_BUDGET_MB`
//! lowers or raises it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

const GIB: u64 = 1 << 30;

static USED: AtomicU64 = AtomicU64::new(0);
static LIMIT: OnceLock<u64> = OnceLock::new();

/// The host's physical memory, in bytes (0 if unknown).
#[cfg(windows)]
pub fn host_physical() -> u64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullTotalPhys } else { 0 }
}

/// What the system can still commit (RAM plus pagefile not yet promised), in bytes.
#[cfg(windows)]
pub fn host_commit_available() -> u64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullAvailPageFile } else { 0 }
}

/// The default budget for a host with `physical` bytes of memory.
pub fn default_limit(physical: u64) -> u64 {
    let third = if physical == 0 { 4 * GIB } else { physical / 3 };
    third.min(12 * GIB)
}

/// The budget in bytes. A user RAM limit ([`crate::limits`]) gives the physical pages at most half of it;
/// the process's own memory gets the rest ([`private_cap`]).
pub fn limit() -> u64 {
    *LIMIT.get_or_init(|| {
        let base = if let Some(mb) = std::env::var("REBUILD_MEMORY_BUDGET_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
            mb << 20
        } else {
            #[cfg(windows)]
            let physical = host_physical();
            #[cfg(not(windows))]
            let physical = 0;
            default_limit(physical)
        };
        match crate::limits::get().ram_bytes() {
            Some(ram) => base.min(ram / 2),
            None => base,
        }
    })
}

/// The cap on the process's own committed memory: with a RAM limit, what the physical pages leave of it;
/// otherwise the budget plus 4 GiB (Rust, our heaps, the title's private allocations).
pub fn private_cap() -> u64 {
    match crate::limits::get().ram_bytes() {
        Some(ram) => ram.saturating_sub(limit()).max(1 << 30),
        None => limit() + 4 * GIB,
    }
}

/// Bytes charged so far.
pub fn used() -> u64 {
    USED.load(Ordering::Relaxed)
}

/// Charge `bytes`; false (and nothing charged) if that would pass the budget.
pub fn charge(bytes: u64) -> bool {
    let limit = limit();
    let mut u = USED.load(Ordering::Acquire);
    loop {
        let Some(n) = u.checked_add(bytes).filter(|n| *n <= limit) else { return false };
        match USED.compare_exchange_weak(u, n, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => u = now,
        }
    }
}

/// Give back `bytes` charged earlier.
pub fn refund(bytes: u64) {
    let mut u = USED.load(Ordering::Acquire);
    while let Err(now) = USED.compare_exchange_weak(u, u.saturating_sub(bytes), Ordering::AcqRel, Ordering::Acquire) {
        u = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_a_third_of_memory_capped_at_12_gib() {
        assert_eq!(default_limit(32 * GIB), 32 * GIB / 3);
        assert_eq!(default_limit(64 * GIB), 12 * GIB);
        assert_eq!(default_limit(0), 4 * GIB);
    }

    #[test]
    fn a_charge_past_the_budget_fails_and_charges_nothing() {
        // other tests charge in parallel, so only what holds whatever they hold
        assert!(!charge(limit() + 1));
        assert!(!charge(u64::MAX));
        assert!(used() <= limit());
    }
}
