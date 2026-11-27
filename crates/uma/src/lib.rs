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

#[cfg(windows)]
pub mod windows;

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
    address: u64,
}

pub struct Uma {
    windows: usize,
    pages: Vec<Page>,
    card_pages: usize,
    used_card_pages: usize,
    next_backing: u32,
    placements: Vec<Option<Placement>>,
}

impl Uma {
    pub fn new(windows: usize, card_bytes: usize) -> Uma {
        assert!(windows > 0);
        let pages = windows * (WINDOW / PAGE);
        Uma {
            windows,
            pages: (0..pages)
                .map(|_| Page {
                    backing: None,
                    users: 0,
                })
                .collect(),
            card_pages: card_bytes / PAGE,
            used_card_pages: 0,
            next_backing: 0,
            placements: Vec::new(),
        }
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

    pub fn place(&mut self, address: u64, len: usize) -> Result<PlacementId, Refused> {
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
            .filter(|page| self.pages[*page].backing.is_none())
            .count();
        if self.used_card_pages + fresh > self.card_pages {
            return Err(Refused::NoCardMemory);
        }
        for page in first..first + count {
            let slot = &mut self.pages[page];
            if slot.backing.is_none() {
                slot.backing = Some(self.next_backing);
                self.next_backing += 1;
                self.used_card_pages += 1;
            }
            slot.users += 1;
        }
        let id = PlacementId(self.placements.len() as u32);
        self.placements.push(Some(Placement {
            first,
            len: count,
            address,
        }));
        Ok(id)
    }

    pub fn retire(&mut self, id: PlacementId) -> Result<(), Refused> {
        let placement = self
            .placements
            .get_mut(id.0 as usize)
            .and_then(Option::take)
            .ok_or(Refused::UnknownPlacement)?;
        for page in placement.first..placement.first + placement.len {
            let slot = &mut self.pages[page];
            slot.users -= 1;
            if slot.users == 0 {
                slot.backing = None;
                self.used_card_pages -= 1;
            }
        }
        Ok(())
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
        let first = uma.place(PAGE as u64, PAGE).unwrap();
        let second = uma.place(PAGE as u64, PAGE).unwrap();
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
        let id = uma.place(0, 2 * PAGE).unwrap();
        assert_eq!(
            uma.place(2 * PAGE as u64, 4 * PAGE).unwrap_err(),
            Refused::NoCardMemory
        );
        assert_eq!(uma.card_pages_used(), 2);
        uma.retire(id).unwrap();
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
