//! The queue's page mapping batch (0xf8, `CopyPageMappingsBatchX(count, *batches, pool, pool pages, flags)`): the
//! title maps 64 KiB pages of a pool (memory it mapped physical pages into, registered with device 0x250) at GPU
//! addresses of its own. Bink's frame buffers live this way: the decoder writes the pool's CPU view (e.g. `0x1aa…`),
//! the video textures sit at the GPU addresses the pages are mapped to (`0x1b0…`), memory the CPU never sees.
//!
//! ```text
//! batch    { destination u64, range count u32, *ranges }                       24 bytes
//! range    { kind u32, first pool page u32, page count u32 }                   12 bytes
//! kind     0 pool pages first.. in order   1 pool page first, every page   2 unmapped   3 left as they are
//! ```
//!
//! Layout from the fork's notes (`victory-gpu` queue, read for design). Here the mapping is bookkeeping: which CPU
//! address holds a GPU page's bytes, for `crate::cputex` to copy textures from. The GPU side of these addresses is
//! not placed on tiles yet (a texture over them is a host texture of its own).

#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::RwLock;

const PAGE: u64 = 64 << 10;

/// Batches, pages mapped, pages unmapped, ranges refused.
pub static BATCHES: AtomicU64 = AtomicU64::new(0);
pub static MAPPED: AtomicU64 = AtomicU64::new(0);
pub static UNMAPPED: AtomicU64 = AtomicU64::new(0);
pub static REFUSED: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
pub struct Range {
    pub kind: u32,
    pub start: u32,
    pub count: u32,
}

#[repr(C)]
pub struct Batch {
    pub destination: u64,
    pub count: u32,
    pub ranges: *const Range,
}

const _: () = assert!(std::mem::size_of::<Range>() == 12);
const _: () = assert!(std::mem::size_of::<Batch>() == 24);

/// GPU page -> the CPU address of its bytes.
static PAGES: RwLock<BTreeMap<u64, u64>> = RwLock::new(BTreeMap::new());

/// Apply `batches` over `pool` (`pool_pages` pages) to `pages`. Returns false if some range was refused (its pages
/// keep their mapping).
pub fn apply(pages: &mut BTreeMap<u64, u64>, batches: &[Batch], pool: u64, pool_pages: u32) -> bool {
    let mut ok = true;
    for b in batches {
        if b.ranges.is_null() || b.destination % PAGE != 0 {
            ok &= b.count == 0;
            continue;
        }
        // SAFETY: each batch holds `count` ranges (the caller's contract)
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let inside = |n: u32| r.start.checked_add(n).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            match r.kind {
                0 if inside(r.count) => {
                    for k in 0..u64::from(r.count) {
                        pages.insert(at + k * PAGE, source + k * PAGE);
                    }
                    MAPPED.fetch_add(u64::from(r.count), Relaxed);
                }
                1 if inside(1) => {
                    for k in 0..u64::from(r.count) {
                        pages.insert(at + k * PAGE, source);
                    }
                    MAPPED.fetch_add(u64::from(r.count), Relaxed);
                }
                2 => {
                    for k in 0..u64::from(r.count) {
                        pages.remove(&(at + k * PAGE));
                    }
                    UNMAPPED.fetch_add(u64::from(r.count), Relaxed);
                }
                3 => {}
                _ => {
                    REFUSED.fetch_add(1, Relaxed);
                    ok = false;
                }
            }
            at += u64::from(r.count) * PAGE;
        }
    }
    ok
}

/// The GPU pages `batches` map or unmap (sorted, once each): textures over them take their new bytes
/// (`crate::cputex`).
pub fn touched(batches: &[Batch]) -> Vec<u64> {
    let mut out = Vec::new();
    for b in batches {
        if b.ranges.is_null() || b.destination % PAGE != 0 {
            continue;
        }
        // SAFETY: each batch holds `count` ranges (the caller's contract)
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            if r.kind <= 2 {
                out.extend((0..u64::from(r.count)).map(|k| at + k * PAGE));
            }
            at += u64::from(r.count) * PAGE;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The CPU address holding `bytes` at GPU address `va`, when its pages are mapped from one contiguous run of a pool.
pub fn source(va: u64, bytes: u64) -> Option<u64> {
    let pages = PAGES.read().unwrap_or_else(|e| e.into_inner());
    source_in(&pages, va, bytes)
}

fn source_in(pages: &BTreeMap<u64, u64>, va: u64, bytes: u64) -> Option<u64> {
    let first = va / PAGE * PAGE;
    let base = *pages.get(&first)?;
    let mut expect = base;
    for p in (first..va + bytes).step_by(PAGE as usize) {
        if pages.get(&p) != Some(&expect) {
            return None;
        }
        expect += PAGE;
    }
    Some(base + (va - first))
}

/// 0xf8 `CopyPageMappingsBatchX(count, *batches, pool, pool pages, flags)`. Applied at once (the console orders it
/// on the queue; what the title maps is read by later copies only).
pub unsafe extern "system" fn CopyPageMappingsBatchX(_this: *mut c_void, count: u32, batches: *const Batch, pool: u64, pool_pages: u32, _flags: u32) -> i32 {
    const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    BATCHES.fetch_add(1, Relaxed);
    if count == 0 {
        return 0;
    }
    // SAFETY: the title passes `count` batches
    let batches = unsafe { std::slice::from_raw_parts(batches, count as usize) };
    {
        let mut pages = PAGES.write().unwrap_or_else(|e| e.into_inner());
        if !apply(&mut pages, batches, pool, pool_pages) && REFUSED.load(Relaxed) == 1 {
            eprintln!("pagemap   a page mapping range was refused (pool {pool:#x}, {pool_pages} pages)");
        }
    }
    // textures over the pages read other bytes now (after the lock: cputex looks sources up here)
    #[cfg(windows)]
    crate::cputex::remapped(&touched(batches));
    0
}

/// For the run report.
pub fn report() -> String {
    format!(
        "{} page mapping batches: {} pages mapped, {} unmapped, {} ranges refused; {} GPU pages mapped now",
        BATCHES.load(Relaxed),
        MAPPED.load(Relaxed),
        UNMAPPED.load(Relaxed),
        REFUSED.load(Relaxed),
        PAGES.read().unwrap_or_else(|e| e.into_inner()).len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_map_runs_single_pages_and_holes() {
        let mut m = BTreeMap::new();
        let pool = 0x1aa_5000_0000u64;
        let gpu = 0x1b0_5000_0000u64;
        let ranges = [Range { kind: 0, start: 2, count: 3 }, Range { kind: 3, start: 0, count: 1 }, Range { kind: 1, start: 7, count: 2 }];
        let b = [Batch { destination: gpu, count: 3, ranges: ranges.as_ptr() }];
        assert!(apply(&mut m, &b, pool, 16));
        assert_eq!(m.get(&gpu), Some(&(pool + 2 * PAGE)));
        assert_eq!(m.get(&(gpu + 2 * PAGE)), Some(&(pool + 4 * PAGE)));
        assert_eq!(m.get(&(gpu + 3 * PAGE)), None, "kind 3 leaves the page as it was");
        assert_eq!(m.get(&(gpu + 4 * PAGE)), Some(&(pool + 7 * PAGE)));
        assert_eq!(m.get(&(gpu + 5 * PAGE)), Some(&(pool + 7 * PAGE)));
        // one contiguous run: a CPU source; across the hole or the repeated page: none
        assert_eq!(source_in(&m, gpu + 0x100, 3 * PAGE - 0x100), Some(pool + 2 * PAGE + 0x100));
        assert_eq!(source_in(&m, gpu, 4 * PAGE), None);
        assert_eq!(source_in(&m, gpu + 4 * PAGE, 2 * PAGE), None);
        // unmapping, and a range past the pool refused
        let hole = [Range { kind: 2, start: 0, count: 1 }, Range { kind: 0, start: 15, count: 2 }];
        assert!(!apply(&mut m, &[Batch { destination: gpu, count: 2, ranges: hole.as_ptr() }], pool, 16));
        assert_eq!(m.get(&gpu), None);
        assert_eq!(m.get(&(gpu + PAGE)), Some(&(pool + 3 * PAGE)), "the refused range changed nothing");
        // the pages a batch touches: mapped, repeated and unmapped ones, not the ones left as they were
        assert_eq!(touched(&b), vec![gpu, gpu + PAGE, gpu + 2 * PAGE, gpu + 4 * PAGE, gpu + 5 * PAGE]);
        assert_eq!(touched(&[Batch { destination: gpu, count: 2, ranges: hole.as_ptr() }]), vec![gpu, gpu + PAGE, gpu + 2 * PAGE]);
    }
}
