//! The arena: resources that cannot live in tiles, kept by console address.
//!
//! Some resources the title places at its own addresses cannot be reserved (tiled) resources on
//! the PC: a depth-stencil with stencil, MSAA targets, anything not 64 KiB aligned. Those get a
//! committed resource of their own, found again by address. Two placements of the same range
//! and kind share one resource; it lives until its last user goes. A placement that overlaps an
//! existing arena resource without matching it is refused: the arena never aliases partially.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub address: u64,
    pub len: u64,
    /// what the resource is (format, dimensions, flags) as one value; equal kinds may share
    pub kind: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// first user: the caller creates the PC resource for this slot
    New(usize),
    /// the resource already exists in this slot
    Shared(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaRefused {
    Empty,
    /// overlaps an arena resource that is not the same range and kind
    PartialOverlap { with: Key },
    UnknownSlot,
}

struct Entry {
    key: Key,
    users: u32,
}

#[derive(Default)]
pub struct Arena {
    entries: Vec<Option<Entry>>,
    free: Vec<usize>,
    bytes: u64,
}

impl Arena {
    pub fn new() -> Arena {
        Arena::default()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn live(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    pub fn acquire(&mut self, key: Key) -> Result<Acquired, ArenaRefused> {
        if key.len == 0 {
            return Err(ArenaRefused::Empty);
        }
        for (slot, e) in self.entries.iter_mut().enumerate() {
            let Some(e) = e else { continue };
            if e.key == key {
                e.users += 1;
                return Ok(Acquired::Shared(slot));
            }
            let overlap = key.address < e.key.address + e.key.len && e.key.address < key.address + key.len;
            if overlap {
                return Err(ArenaRefused::PartialOverlap { with: e.key });
            }
        }
        let entry = Some(Entry { key, users: 1 });
        let slot = match self.free.pop() {
            Some(s) => {
                self.entries[s] = entry;
                s
            }
            None => {
                self.entries.push(entry);
                self.entries.len() - 1
            }
        };
        self.bytes += key.len;
        Ok(Acquired::New(slot))
    }

    /// Returns true when this was the last user (the caller drops the PC resource).
    pub fn release(&mut self, slot: usize) -> Result<bool, ArenaRefused> {
        let e = self.entries.get_mut(slot).and_then(|e| e.as_mut()).ok_or(ArenaRefused::UnknownSlot)?;
        e.users -= 1;
        if e.users > 0 {
            return Ok(false);
        }
        let key = e.key;
        self.entries[slot] = None;
        self.free.push(slot);
        self.bytes -= key.len;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(address: u64, len: u64, kind: u64) -> Key {
        Key { address, len, kind }
    }

    #[test]
    fn the_same_range_and_kind_share_one_resource_until_the_last_user() {
        let mut a = Arena::new();
        let Acquired::New(s) = a.acquire(k(0x1000, 0x800, 7)).unwrap() else { panic!() };
        assert_eq!(a.acquire(k(0x1000, 0x800, 7)).unwrap(), Acquired::Shared(s));
        assert_eq!(a.release(s), Ok(false));
        assert_eq!(a.live(), 1);
        assert_eq!(a.release(s), Ok(true));
        assert_eq!(a.live(), 0);
        assert_eq!(a.bytes(), 0);
    }

    #[test]
    fn a_partial_overlap_is_refused() {
        let mut a = Arena::new();
        a.acquire(k(0x1000, 0x1000, 1)).unwrap();
        assert_eq!(
            a.acquire(k(0x1800, 0x1000, 1)),
            Err(ArenaRefused::PartialOverlap { with: k(0x1000, 0x1000, 1) })
        );
        // same range, different kind: also a partial alias, also refused
        assert!(matches!(a.acquire(k(0x1000, 0x1000, 2)), Err(ArenaRefused::PartialOverlap { .. })));
        // touching, not overlapping: fine
        assert!(matches!(a.acquire(k(0x2000, 0x100, 1)), Ok(Acquired::New(_))));
    }

    #[test]
    fn slots_are_reused_and_unknown_slots_refused() {
        let mut a = Arena::new();
        let Acquired::New(s) = a.acquire(k(0, 64, 1)).unwrap() else { panic!() };
        a.release(s).unwrap();
        assert_eq!(a.acquire(k(64, 64, 1)).unwrap(), Acquired::New(s));
        assert_eq!(a.release(99), Err(ArenaRefused::UnknownSlot));
        assert_eq!(a.acquire(k(0, 0, 1)), Err(ArenaRefused::Empty));
    }
}
