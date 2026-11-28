//! The page map of a shader-visible view heap: console descriptor indices to host ones.
//!
//! The title's view heap is larger than any host heap may be (1,245,184 descriptors; the host
//! takes at most 1,000,000, and a quarter of that is the null bank, alternates and windows), and
//! it uses only parts of it, some of them far up (the UI's tables near index 868,000). So console
//! indices are split into pages of `1 << shift` descriptors, and a console page gets a host page
//! the first time it is used: a descriptor written into it (`crates/d3d12x`), or a descriptor
//! table bound in it ([`super::HeapBinding::table_offset`]). Pages are never given back.
//!
//! A page is placed at its own index when that host page is free (so most of the heap is the
//! identity), else next to the host page of a console neighbour (a table that runs into the next
//! page stays contiguous), else at the start of the highest free run. Every place a console index
//! becomes a host one goes through [`PageMap::host`]: the mirror of records into the host heap,
//! the table offsets written into user data, the shadow table, and (for indices translated
//! shaders compute from the title's own data, the dynamic-resource heap) a copy of the map in the
//! shadow buffer that the shader reads ([`PageMap::set_gpu_table`]): one load per descriptor.

use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// A console page with no host page.
const NONE: u32 = u32::MAX;

/// Console pages to host pages, for one heap.
pub struct PageMap {
    /// a page is `1 << shift` descriptors
    pub shift: u32,
    /// console pages (the console heap's count, rounded up)
    pub console_pages: u32,
    /// host pages the heap's mirrored range holds
    pub host_pages: u32,
    /// the host index an unmapped page reads (the null bank: a translated shader reads nulls there)
    pub unmapped: u32,
    map: Box<[AtomicU32]>,
    /// host pages taken (one bit each), and how many
    taken: Mutex<(Vec<u64>, u32)>,
    /// the copy translated shaders read: per console page its first host index (`unmapped` when none), up to
    /// `gpu_pages` entries
    gpu: AtomicPtr<u32>,
    gpu_pages: AtomicU32,
    /// pages wanted while every host page was taken
    pub full: AtomicU64,
    /// tables whose descriptors run into a page the host does not hold right after the first one
    pub split_tables: AtomicU64,
    /// pages taken at their own index, and elsewhere
    pub identity: AtomicU32,
    pub moved: AtomicU32,
}

impl PageMap {
    /// A map of a heap of `console` descriptors onto `host` host descriptors (whole pages of them).
    pub fn new(shift: u32, console: u32, host: u32, unmapped: u32) -> PageMap {
        let console_pages = console.div_ceil(1 << shift);
        let host_pages = host >> shift;
        PageMap {
            shift,
            console_pages,
            host_pages,
            unmapped,
            map: (0..console_pages).map(|_| AtomicU32::new(NONE)).collect(),
            taken: Mutex::new((vec![0; host_pages.div_ceil(64) as usize], 0)),
            gpu: AtomicPtr::new(std::ptr::null_mut()),
            gpu_pages: AtomicU32::new(0),
            full: AtomicU64::new(0),
            split_tables: AtomicU64::new(0),
            identity: AtomicU32::new(0),
            moved: AtomicU32::new(0),
        }
    }

    /// The host index of console index `i`, if its page has one.
    #[inline]
    pub fn host(&self, i: u32) -> Option<u32> {
        let p = self.map.get((i >> self.shift) as usize)?.load(Ordering::Acquire);
        (p != NONE).then(|| (p << self.shift) | (i & ((1 << self.shift) - 1)))
    }

    /// The host index of console index `i`, giving its page a host page if it has none (`None`: past the
    /// console heap, or no host page left).
    #[inline]
    pub fn host_or_map(&self, i: u32) -> Option<u32> {
        match self.host(i) {
            Some(h) => Some(h),
            None => {
                self.map_page(i >> self.shift)?;
                self.host(i)
            }
        }
    }

    /// The host index of a table of `n` descriptors from console index `i`, its pages given host pages; the
    /// unmapped index when there is none. A table that runs into a page placed apart from its first is
    /// counted ([`PageMap::split_tables`]): its later descriptors read another page's.
    #[inline]
    pub fn table(&self, i: u32, n: u32) -> u32 {
        let Some(h) = self.host_or_map(i) else { return self.unmapped };
        let last = i.saturating_add(n.max(1) - 1);
        if last >> self.shift != i >> self.shift && self.host_or_map(last) != Some(h + (last - i)) {
            self.split_tables.fetch_add(1, Ordering::Relaxed);
        }
        h
    }

    /// Gives console page `c` a host page (or returns the one it got meanwhile).
    #[cold]
    fn map_page(&self, c: u32) -> Option<u32> {
        let cell = self.map.get(c as usize)?;
        let mut t = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        let had = cell.load(Ordering::Acquire);
        if had != NONE {
            return Some(had);
        }
        let (bits, n) = &mut *t;
        let free = |bits: &Vec<u64>, h: u32| h < self.host_pages && bits[(h / 64) as usize] >> (h % 64) & 1 == 0;
        let neighbour = |k: u32| self.map.get(k as usize).map(|m| m.load(Ordering::Acquire)).filter(|&p| p != NONE);
        let at = if free(bits, c) {
            self.identity.fetch_add(1, Ordering::Relaxed);
            Some(c)
        } else {
            self.moved.fetch_add(1, Ordering::Relaxed);
            let after = c.checked_sub(1).and_then(neighbour).map(|p| p + 1).filter(|&p| free(bits, p));
            let before = || neighbour(c + 1).and_then(|p| p.checked_sub(1)).filter(|&p| free(bits, p));
            // the middle of the largest free run: the farthest from every page used so far (the likeliest never to
            // be wanted at its own index), with free pages on both sides for this page's console neighbours
            let largest = || {
                let (mut best, mut best_len, mut run, mut len) = (None, 0, 0, 0);
                for h in 0..self.host_pages {
                    if free(bits, h) {
                        if len == 0 {
                            run = h;
                        }
                        len += 1;
                        if len > best_len {
                            (best, best_len) = (Some(run), len);
                        }
                    } else {
                        len = 0;
                    }
                }
                best.map(|b| b + best_len / 2)
            };
            after.or_else(before).or_else(largest)
        };
        let Some(at) = at else {
            self.full.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        bits[(at / 64) as usize] |= 1 << (at % 64);
        *n += 1;
        let g = self.gpu.load(Ordering::Acquire);
        if !g.is_null() && c < self.gpu_pages.load(Ordering::Relaxed) {
            // SAFETY: inside the table `set_gpu_table` was given (the heap's shadow buffer, mapped for the heap's life)
            unsafe { g.add(c as usize).write_volatile(at << self.shift) };
        }
        cell.store(at, Ordering::Release);
        Some(at)
    }

    /// The copy of the map translated shaders read: `pages` u32 at `table` (CPU-visible GPU memory that lives as
    /// long as the heap), filled now and kept up to date. Entries past the console heap read the unmapped index.
    ///
    /// # Safety
    /// `table` is writable for `pages` u32 for as long as this map is used.
    pub unsafe fn set_gpu_table(&self, table: *mut u32, pages: u32) {
        let _t = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        for c in 0..pages {
            let v = match self.map.get(c as usize).map(|m| m.load(Ordering::Acquire)) {
                Some(p) if p != NONE => p << self.shift,
                _ => self.unmapped,
            };
            // SAFETY: the caller's
            unsafe { table.add(c as usize).write_volatile(v) };
        }
        self.gpu_pages.store(pages, Ordering::Relaxed);
        self.gpu.store(table, Ordering::Release);
    }

    /// Stops writing the shader copy (its memory is going away).
    pub fn drop_gpu_table(&self) {
        let _t = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        self.gpu.store(std::ptr::null_mut(), Ordering::Release);
    }

    /// Host pages taken.
    pub fn taken(&self) -> u32 {
        self.taken.lock().unwrap_or_else(|e| e.into_inner()).1
    }

    /// The console pages that have host pages, as runs `(first console page, pages, first host page)` (a run
    /// is consecutive on both sides).
    pub fn runs(&self) -> Vec<(u32, u32, u32)> {
        let mut out: Vec<(u32, u32, u32)> = Vec::new();
        for (c, m) in self.map.iter().enumerate() {
            let p = m.load(Ordering::Acquire);
            if p == NONE {
                continue;
            }
            match out.last_mut() {
                Some((c0, n, p0)) if *c0 + *n == c as u32 && *p0 + *n == p => *n += 1,
                _ => out.push((c as u32, 1, p)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_go_to_their_own_index_when_free_and_next_to_their_neighbour_else() {
        // 16 console pages of 4 onto 8 host pages
        let m = PageMap::new(2, 64, 32, 1000);
        assert_eq!(m.host(5), None);
        assert_eq!(m.host_or_map(5), Some(5), "page 1 at its own index");
        // console page 12 has no own host page: the middle of the largest free run (2..8)
        assert_eq!(m.host_or_map(49), Some(5 * 4 + 1));
        // page 13 goes after it, page 11 before it
        assert_eq!(m.host_or_map(52), Some(6 * 4));
        assert_eq!(m.host_or_map(47), Some(4 * 4 + 3));
        assert_eq!(m.runs(), vec![(1, 1, 1), (11, 3, 4)]);
        assert_eq!((m.identity.load(Ordering::Relaxed), m.moved.load(Ordering::Relaxed)), (1, 3));
        // a table running from page 12 into 13 is contiguous on the host
        assert_eq!(m.table(50, 4), 22);
        assert_eq!(m.split_tables.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_full_map_says_so_and_tables_read_the_unmapped_index() {
        let m = PageMap::new(2, 64, 8, 777);
        assert_eq!(m.host_or_map(0), Some(0));
        assert_eq!(m.host_or_map(4), Some(4));
        assert_eq!(m.host_or_map(8), None);
        assert_eq!(m.table(9, 1), 777);
        assert_eq!(m.full.load(Ordering::Relaxed), 2);
        assert_eq!(m.host_or_map(64), None, "past the console heap");
    }

    #[test]
    fn the_gpu_copy_follows_the_map() {
        let m = PageMap::new(2, 64, 32, 1000);
        m.host_or_map(4);
        let mut t = vec![0u32; 20];
        unsafe { m.set_gpu_table(t.as_mut_ptr(), 20) };
        assert_eq!(t[1], 4);
        assert_eq!(t[0], 1000);
        assert_eq!(t[19], 1000, "past the console heap");
        m.host_or_map(13);
        assert_eq!(t[3], 12);
        m.drop_gpu_table();
    }
}
