//! Unified memory: the title's own addresses, backed by card memory.
//!
//! A console title reserves and commits memory itself and expects the GPU to
//! reach it at the addresses it chose. Mirroring that memory costs a copy in
//! and a copy back every frame, so this block gives the title addresses in a
//! sparse window instead, and backs them with card pages one page at a time:
//! one 64 KiB page of card memory for each 64 KiB page of the title's
//! addresses that a resource covers.
//!
//! Two resources at one address share the same page, as on the console, and a
//! page nobody covers costs nothing. This module is the bookkeeping only; the
//! Windows side maps the pages it names into a reserved (tiled) resource.

pub mod chunks;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub mod upload;

#[cfg(windows)]
pub mod tiles;

pub mod arena;

#[cfg(windows)]
pub mod tileproof;

#[cfg(windows)]
pub mod ring;

#[cfg(windows)]
pub mod labels;

#[cfg(windows)]
pub mod space;

pub const PAGE: usize = 64 << 10;
pub const WINDOW: usize = 4 << 30;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Refused {
    Unaligned,
    OutsideOneWindow,
    NoCardMemory,
    UnknownPlacement,
}

struct Page {
    backing: Option<u32>,
    users: u32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PlacementId(u32);

struct Placement {
    first: usize,
    len: usize,
}

/// A page of the title's addresses and the card page that backs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backed {
    pub page: usize,
    pub card: u32,
}

pub struct Uma {
    windows: usize,
    /// one page table per 4 GiB window, made on first use (a 16 TiB space costs nothing until placed)
    tables: Vec<Option<Box<[Page]>>>,
    card_pages: usize,
    used_card_pages: usize,
    next_backing: u32,
    free: Vec<u32>,
    placements: Vec<Option<Placement>>,
}

impl Uma {
    pub fn new(windows: usize, card_bytes: usize) -> Uma {
        assert!(windows > 0);
        Uma {
            windows,
            tables: (0..windows).map(|_| None).collect(),
            card_pages: card_bytes / PAGE,
            used_card_pages: 0,
            next_backing: 0,
            free: Vec::new(),
            placements: Vec::new(),
        }
    }

    fn page(&self, page: usize) -> Option<&Page> {
        let per = WINDOW / PAGE;
        self.tables.get(page / per)?.as_ref().map(|t| &t[page % per])
    }

    fn page_mut(&mut self, page: usize) -> &mut Page {
        let per = WINDOW / PAGE;
        let t = self.tables[page / per].get_or_insert_with(|| (0..per).map(|_| Page { backing: None, users: 0 }).collect());
        &mut t[page % per]
    }

    pub fn bytes(&self) -> usize {
        self.windows * WINDOW
    }

    pub fn card_pages_used(&self) -> usize {
        self.used_card_pages
    }

    pub fn address_of(&self, page: usize) -> u64 {
        let window = page / (WINDOW / PAGE);
        let offset = (page % (WINDOW / PAGE)) * PAGE;
        (window * WINDOW + offset) as u64
    }

    /// Place a resource at `address`. Returns its id and the pages that were not backed before
    /// (the card pages the GPU side must now map); pages already backed are shared.
    pub fn place(&mut self, address: u64, len: usize) -> Result<(PlacementId, Vec<Backed>), Refused> {
        if address % PAGE as u64 != 0 || len == 0 || len % PAGE != 0 {
            return Err(Refused::Unaligned);
        }
        let window = address as usize / WINDOW;
        if window >= self.windows || (address as usize % WINDOW) + len > WINDOW {
            return Err(Refused::OutsideOneWindow);
        }
        let first = address as usize / PAGE;
        let count = len / PAGE;
        let fresh = (first..first + count)
            .filter(|page| self.page(*page).is_none_or(|p| p.backing.is_none()))
            .count();
        if self.used_card_pages + fresh > self.card_pages {
            return Err(Refused::NoCardMemory);
        }
        let mut mapped = Vec::with_capacity(fresh);
        for page in first..first + count {
            if self.page_mut(page).backing.is_none() {
                let card = match self.free.pop() {
                    Some(c) => c,
                    None => {
                        self.next_backing += 1;
                        self.next_backing - 1
                    }
                };
                self.page_mut(page).backing = Some(card);
                self.used_card_pages += 1;
                mapped.push(Backed { page, card });
            }
            self.page_mut(page).users += 1;
        }
        let id = PlacementId(self.placements.len() as u32);
        self.placements.push(Some(Placement { first, len: count }));
        Ok((id, mapped))
    }

    /// Retire a placement. Returns the pages whose last user it was (the GPU side unmaps them;
    /// their card pages go back on the free list).
    pub fn retire(&mut self, id: PlacementId) -> Result<Vec<Backed>, Refused> {
        let placement = self
            .placements
            .get_mut(id.0 as usize)
            .and_then(Option::take)
            .ok_or(Refused::UnknownPlacement)?;
        let mut released = Vec::new();
        for page in placement.first..placement.first + placement.len {
            let slot = self.page_mut(page);
            slot.users -= 1;
            if slot.users == 0 {
                let card = slot.backing.take().expect("a used page is backed");
                self.free.push(card);
                self.used_card_pages -= 1;
                released.push(Backed { page, card });
            }
        }
        Ok(released)
    }

    /// The card page backing the page that covers `address`, if any.
    pub fn backing(&self, address: u64) -> Option<u32> {
        self.page(address as usize / PAGE).and_then(|p| p.backing)
    }

    /// The most card pages ever in use at once (how far the GPU side's heaps must reach).
    pub fn card_pages_reached(&self) -> u32 {
        self.next_backing
    }

    pub fn covering(&self, address: u64) -> u64 {
        let page = address as usize / PAGE;
        self.address_of(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: usize = 512 << 20;

    #[test]
    fn one_page_of_card_memory_per_page_of_address() {
        let mut uma = Uma::new(1, CARD);
        assert_eq!(uma.card_pages_used(), 0);
        uma.place(0, 4 * PAGE).unwrap();
        assert_eq!(uma.card_pages_used(), 4);
    }

    #[test]
    fn two_resources_at_one_address_share_the_page() {
        let mut uma = Uma::new(1, CARD);
        uma.place(2 * PAGE as u64, 2 * PAGE).unwrap();
        uma.place(2 * PAGE as u64, 2 * PAGE).unwrap();
        assert_eq!(uma.card_pages_used(), 2);
    }

    #[test]
    fn a_page_lives_until_the_last_resource_goes() {
        let mut uma = Uma::new(1, CARD);
        let (first, _) = uma.place(PAGE as u64, PAGE).unwrap();
        let (second, _) = uma.place(PAGE as u64, PAGE).unwrap();
        uma.retire(first).unwrap();
        assert_eq!(uma.card_pages_used(), 1);
        uma.retire(second).unwrap();
        assert_eq!(uma.card_pages_used(), 0);
    }

    #[test]
    fn a_range_that_is_not_whole_pages_cannot_be_tiled() {
        let mut uma = Uma::new(1, CARD);
        assert_eq!(uma.place(16, PAGE).unwrap_err(), Refused::Unaligned);
        assert_eq!(uma.place(0, PAGE - 1).unwrap_err(), Refused::Unaligned);
    }

    #[test]
    fn a_range_across_two_windows_is_refused() {
        let mut uma = Uma::new(2, CARD);
        let last = (WINDOW - PAGE) as u64;
        assert_eq!(
            uma.place(last, 2 * PAGE).unwrap_err(),
            Refused::OutsideOneWindow
        );
    }

    #[test]
    fn a_refusal_leaves_the_block_untouched() {
        let pages = 4;
        let mut uma = Uma::new(1, pages * PAGE);
        let (id, _) = uma.place(0, 2 * PAGE).unwrap();
        assert_eq!(
            uma.place(2 * PAGE as u64, 4 * PAGE).unwrap_err(),
            Refused::NoCardMemory
        );
        assert_eq!(uma.card_pages_used(), 2);
        uma.retire(id).unwrap();
        assert_eq!(uma.card_pages_used(), 0);
    }

    #[test]
    fn freed_card_pages_are_reused() {
        let mut uma = Uma::new(1, 4 * PAGE);
        for round in 0..100u64 {
            let (id, mapped) = uma.place((round % 7) * 4 * PAGE as u64, 4 * PAGE).unwrap();
            assert_eq!(mapped.len(), 4);
            assert!(mapped.iter().all(|b| b.card < 4));
            assert_eq!(uma.retire(id).unwrap().len(), 4);
        }
        assert_eq!(uma.card_pages_reached(), 4);
    }

    #[test]
    fn shared_pages_are_mapped_once_and_unmapped_once() {
        let mut uma = Uma::new(1, CARD);
        let (a, m1) = uma.place(0, 3 * PAGE).unwrap();
        let (b, m2) = uma.place(PAGE as u64, 3 * PAGE).unwrap();
        assert_eq!(m1.len(), 3);
        assert_eq!(m2.iter().map(|b| b.page).collect::<Vec<_>>(), vec![3]);
        assert_eq!(uma.backing(PAGE as u64 + 5), Some(m1[1].card));
        assert_eq!(uma.retire(a).unwrap().iter().map(|b| b.page).collect::<Vec<_>>(), vec![0]);
        assert_eq!(uma.retire(b).unwrap().len(), 3);
        assert_eq!(uma.card_pages_used(), 0);
    }

    #[test]
    fn addresses_are_window_relative() {
        let uma = Uma::new(2, CARD);
        assert_eq!(uma.address_of(0), 0);
        assert_eq!(uma.address_of(1), PAGE as u64);
        assert_eq!(uma.address_of(WINDOW / PAGE), WINDOW as u64);
    }

    #[test]
    fn a_cpu_address_maps_to_the_page_that_covers_it() {
        let uma = Uma::new(1, CARD);
        assert_eq!(uma.covering(0), 0);
        assert_eq!(uma.covering(4096), 0);
        assert_eq!(uma.covering(PAGE as u64 + 4096), PAGE as u64);
    }
}
