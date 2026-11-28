//! GPU timestamps recorded in the title's lists: one ring of timestamp queries per device.
//!
//! Two users, one mechanism (a query taken at a point in a host list, resolved later in a batch):
//!
//! ```text
//! the title's timestamp labels    0x240 flag 1: a query at the label's point; at the host list's end it is
//!                                 resolved straight into the title's memory at the label's address
//! the title's GPU time            every direct or compute host list a query pair, first and last command;
//!                                 the submission resolves the pairs of the lists it executed into its own
//!                                 readback ring, read a few frames later (`crates/runtime/src/gputime.rs`)
//! ```
//!
//! **The label's format.** The console writes the 64-bit GPU counter at the bottom of the pipe; the title converts
//! it with the frequency its queue reports (queue slot 0x90, `GetTimestampFrequency`, called by the title at
//! `title+0x24841a6`). Our queue answers that slot with the host queue's own frequency, so the host's tick *is*
//! the tick the title was told about: the resolved value is written as is (ratio 1, [`console_ticks`]), exact
//! and on the GPU, with no CPU relay and no shader. Answering the console's 100 MHz instead would need a
//! conversion pass per label (a shader on direct/compute lists, impossible on copy lists).
//!
//! **Cost.** A query is one `EndQuery`; queries are taken with one atomic add, never a lock. A label's resolve is
//! deferred to its host list's end so a list with several labels drains the pipe once, not once a label. The ring
//! is large ([`SLOTS`]): a slot comes back after 32,768 host lists, seconds of frames, long after its resolve ran.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::Raw;

/// Timestamp queries in the direct/compute ring.
pub const SLOTS: u32 = 1 << 16;
/// Timestamp queries in the copy-queue ring.
pub const COPY_SLOTS: u32 = 1 << 12;
/// No query (a host list that is not timed).
pub const NONE: u32 = u32::MAX;

/// The rings of one device.
pub struct Stamps {
    /// `TIMESTAMP` queries, for direct and compute lists
    pub heap: ID3D12QueryHeap,
    /// `COPY_QUEUE_TIMESTAMP` queries, for copy lists (when the host has them)
    pub copy: Option<ID3D12QueryHeap>,
    next: AtomicU32,
    next_copy: AtomicU32,
}

// SAFETY: query heaps are free-threaded; the cursors are atomics.
unsafe impl Send for Stamps {}
unsafe impl Sync for Stamps {}

impl Stamps {
    pub fn new(device: &ID3D12Device) -> Option<Stamps> {
        let heap = |kind, count| {
            let mut h: Option<ID3D12QueryHeap> = None;
            unsafe { device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: kind, Count: count, NodeMask: 0 }, &mut h) }.ok().and(h)
        };
        let mut o3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
        let copy_ok = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS3, (&mut o3 as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS3).cast(), size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS3>() as u32) }.is_ok()
            && o3.CopyQueueTimestampQueriesSupported.as_bool();
        Some(Stamps {
            heap: heap(D3D12_QUERY_HEAP_TYPE_TIMESTAMP, SLOTS)?,
            copy: if copy_ok { heap(D3D12_QUERY_HEAP_TYPE_COPY_QUEUE_TIMESTAMP, COPY_SLOTS) } else { None },
            next: AtomicU32::new(0),
            next_copy: AtomicU32::new(0),
        })
    }

    /// `n` (1 or 2) consecutive queries for a list of type `kind` (0 direct, 2 compute, 3 copy): the heap (borrowed)
    /// and the first index. `None` for a bundle, or a copy list on a host without copy-queue timestamps.
    #[inline]
    pub fn take(&self, kind: u32, n: u32) -> Option<(Raw, u32)> {
        let (heap, next, slots) = match kind {
            0 | 2 => (&self.heap, &self.next, SLOTS),
            3 => (self.copy.as_ref()?, &self.next_copy, COPY_SLOTS),
            _ => return None,
        };
        Some((heap.as_raw(), take_in(next, slots, n)))
    }
}

/// `n` consecutive slots of a ring of `slots` (a power of two) that do not wrap.
#[inline]
pub(crate) fn take_in(next: &AtomicU32, slots: u32, n: u32) -> u32 {
    loop {
        let i = next.fetch_add(n, Ordering::Relaxed) & (slots - 1);
        if i + n <= slots {
            return i;
        }
    }
}

/// The rings of `device`, made on first use (one per device for the process: the lists record into them and the
/// submission resolves out of them). `None` if the host refuses the query heap.
pub fn for_device(device: &ID3D12Device) -> Option<Arc<Stamps>> {
    static ALL: Mutex<Vec<(usize, Option<Arc<Stamps>>)>> = Mutex::new(Vec::new());
    // the device's identity is its IUnknown pointer; the ring keeps the device alive, so it is never reused
    let key = device.cast::<windows::core::IUnknown>().ok()?.as_raw() as usize;
    let mut all = ALL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = all.iter().find(|(k, _)| *k == key) {
        return s.clone();
    }
    let s = Stamps::new(device).map(Arc::new);
    all.push((key, s.clone()));
    s
}

/// Per-list timing pairs are on unless `REBUILD_LIST_TIMESTAMPS=0` (labels are written either way).
pub fn list_timing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("REBUILD_LIST_TIMESTAMPS").map_or(true, |v| v.trim() != "0"))
}

/// A host GPU tick as the title reads it, at `console_hz` (what queue slot 0x90 answered), from `host_hz`. The
/// queue answers the host's frequency, so this is the identity on the label path; it stays exact (128-bit) for
/// any pair, so a future answer of the console's 100 MHz converts without overflow.
#[inline]
pub fn console_ticks(host: u64, host_hz: u64, console_hz: u64) -> u64 {
    if host_hz == console_hz || host_hz == 0 {
        return host;
    }
    (u128::from(host) * u128::from(console_hz) / u128::from(host_hz)) as u64
}

/// Nanoseconds of `ticks` at `hz` (0 for an unknown frequency).
#[inline]
pub fn ticks_ns(ticks: u64, hz: u64) -> u64 {
    if hz == 0 {
        return 0;
    }
    (u128::from(ticks) * 1_000_000_000 / u128::from(hz)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_convert_exactly_and_without_overflow() {
        // the queue answers the host's frequency: the label is the host tick as is
        assert_eq!(console_ticks(123_456_789, 10_000_000, 10_000_000), 123_456_789);
        // a 1 GHz card's ticks as the console's 100 MHz, and WARP's 10 MHz as 100 MHz
        assert_eq!(console_ticks(1_000_000_000, 1_000_000_000, 100_000_000), 100_000_000);
        assert_eq!(console_ticks(7, 10_000_000, 100_000_000), 70);
        // days of uptime at 1 GHz do not overflow the product
        let day = 86_400 * 1_000_000_000u64;
        assert_eq!(console_ticks(30 * day, 1_000_000_000, 100_000_000), 30 * day / 10);
        assert_eq!(console_ticks(5, 0, 100), 5, "no host frequency: unchanged");
        assert_eq!(ticks_ns(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(ticks_ns(33, 10_000_000), 3_300);
        assert_eq!(ticks_ns(u64::MAX / 2, 1_000_000_000), u64::MAX / 2);
        assert_eq!(ticks_ns(5, 0), 0);
    }

    #[test]
    fn slots_come_in_runs_that_never_wrap() {
        let next = AtomicU32::new(0);
        assert_eq!(take_in(&next, 8, 1), 0);
        assert_eq!(take_in(&next, 8, 2), 1);
        assert_eq!(take_in(&next, 8, 2), 3);
        assert_eq!(take_in(&next, 8, 2), 5);
        // 7 would straddle the end: the pair starts over at 0
        assert_eq!(take_in(&next, 8, 2), 1, "7..9 straddles; 9 & 7 = 1");
        for _ in 0..1000 {
            let i = take_in(&next, 8, 2);
            assert!(i + 2 <= 8);
        }
    }
}
