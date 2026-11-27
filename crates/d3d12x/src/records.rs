//! What our descriptors hold in the title's memory.
//!
//! A view is 32 bytes (`GetDescriptorHandleIncrementSize` of views; RDNA 2's T#/V# size), a
//! sampler 16, a render target or depth view 128 (its first 32 bytes are used). The title copies
//! them as bytes (between heaps, into rings), so a record must be meaningful wherever it lands:
//!
//! ```text
//! word 0   TAG | kind          'XD' in the high half: a slot never written (0) or the title's own
//!                              bytes decode to nothing
//! buffers (CBV, SRV, UAV of a buffer or of placed memory): named whole, no table
//!   w1 format  w2-3 GPU address of the first byte  w4 elements  w5 structure stride  w6 flags
//! textures, samplers, RTV, DSV: a table entry
//!   w1 entry   w2 generation (a released resource's entries change generation: stale records
//!              decode to nothing instead of to whatever reuses the entry)
//! null views (a create call with no resource): w1 the D3D12 view dimension
//! last word  a check over the others, so stray bytes never pass for a record
//! ```
//!
//! Buffer views carry their address because the title makes ~14,000 constant buffer views a
//! frame at new addresses (fork run 65: a table of them grew by 11,000 entries a second); the
//! record is enough to rebuild the host view at any index.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};

use windows::Win32::Graphics::Direct3D12::*;

use crate::views::HostView;
use crate::Raw;

pub const TAG: u32 = 0x5844_0000;
pub const TAG_MASK: u32 = 0xffff_0000;

const K_CBV: u32 = 1;
const K_SRV_BUFFER: u32 = 2;
const K_UAV_BUFFER: u32 = 3;
const K_ENTRY: u32 = 4;
const K_NULL_SRV: u32 = 5;
const K_NULL_UAV: u32 = 6;
const K_NULL_CBV: u32 = 7;

/// Buffer view flags (word 6).
pub const RAW: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    Cbv { va: u64, size: u32 },
    /// A view of buffer memory: `stride` > 0 structured, `RAW` in `flags` raw, else typed by
    /// `format` (its element size times `elements` is the length).
    Buffer { uav: bool, va: u64, format: u32, elements: u32, stride: u32, flags: u32 },
    /// A texture view, sampler, RTV or DSV by table entry.
    Entry { index: u32, generation: u32 },
    /// A null view of a D3D12 dimension (`uav`: an unordered-access one).
    Null { uav: bool, dimension: u32 },
    NullCbv,
}

const CHECK: u32 = 0x9e37_79b9;

fn check(w: &[u32]) -> u32 {
    w.iter().fold(CHECK, |a, &x| a.rotate_left(5) ^ x)
}

impl Record {
    /// The 32-byte form.
    pub fn encode(&self) -> [u32; 8] {
        let mut w = [0u32; 8];
        match *self {
            Record::Cbv { va, size } => {
                w[0] = TAG | K_CBV;
                w[1] = size;
                (w[2], w[3]) = (va as u32, (va >> 32) as u32);
            }
            Record::Buffer { uav, va, format, elements, stride, flags } => {
                w[0] = TAG | if uav { K_UAV_BUFFER } else { K_SRV_BUFFER };
                w[1] = format;
                (w[2], w[3]) = (va as u32, (va >> 32) as u32);
                w[4] = elements;
                w[5] = stride;
                w[6] = flags;
            }
            Record::Entry { index, generation } => {
                w[0] = TAG | K_ENTRY;
                w[1] = index;
                w[2] = generation;
            }
            Record::Null { uav, dimension } => {
                w[0] = TAG | if uav { K_NULL_UAV } else { K_NULL_SRV };
                w[1] = dimension;
            }
            Record::NullCbv => w[0] = TAG | K_NULL_CBV,
        }
        w[7] = check(&w[..7]);
        w
    }

    /// The 16-byte form (samplers): only entries fit.
    pub fn encode16(&self) -> [u32; 4] {
        let Record::Entry { index, generation } = *self else { return [0; 4] };
        let mut w = [TAG | K_ENTRY, index, generation, 0];
        w[3] = check(&w[..3]);
        w
    }

    pub fn decode(w: &[u32; 8]) -> Option<Record> {
        if w[0] & TAG_MASK != TAG || w[7] != check(&w[..7]) {
            return None;
        }
        let va = u64::from(w[2]) | u64::from(w[3]) << 32;
        Some(match w[0] & !TAG_MASK {
            K_CBV => Record::Cbv { va, size: w[1] },
            k @ (K_SRV_BUFFER | K_UAV_BUFFER) => Record::Buffer { uav: k == K_UAV_BUFFER, va, format: w[1], elements: w[4], stride: w[5], flags: w[6] },
            K_ENTRY => Record::Entry { index: w[1], generation: w[2] },
            k @ (K_NULL_SRV | K_NULL_UAV) => Record::Null { uav: k == K_NULL_UAV, dimension: w[1] },
            K_NULL_CBV => Record::NullCbv,
            _ => return None,
        })
    }

    pub fn decode16(w: &[u32; 4]) -> Option<Record> {
        (w[0] == TAG | K_ENTRY && w[3] == check(&w[..3])).then_some(Record::Entry { index: w[1], generation: w[2] })
    }
}

/// A 64-bit fingerprint of a slot's bytes: a sync skips a slot whose fingerprint is unchanged.
#[inline]
pub fn fingerprint(w: &[u32]) -> u64 {
    // FNV-1a over the words; 0 is kept for "never mirrored"
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in w {
        h = (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h | 1
}

/// What a table entry names.
#[derive(Clone, Copy)]
pub enum EntryView {
    /// A texture view: the view, and the resource kept alive while the entry lives.
    Texture(HostView),
    Sampler(D3D12_SAMPLER_DESC),
    Rtv(HostView),
    Dsv(HostView),
}

struct Entry {
    view: EntryView,
    /// the console resource whose release frees the entry (0: kept for the run)
    owner: usize,
}

/// The table behind [`Record::Entry`]: bounded by what the title has (views of live resources,
/// distinct samplers, placed texture shapes).
#[derive(Default)]
pub struct Entries {
    list: RwLock<Vec<(AtomicU32, Option<Entry>)>>,
    free: Mutex<Vec<u32>>,
    samplers: Mutex<std::collections::HashMap<[u32; 13], u32>>,
}

impl Entries {
    /// A new entry; returns its record.
    pub fn add(&self, view: EntryView, owner: usize) -> Record {
        let reuse = self.free.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let mut list = self.list.write().unwrap_or_else(|e| e.into_inner());
        let index = match reuse {
            Some(i) => i,
            None => {
                list.push((AtomicU32::new(1), None));
                list.len() as u32 - 1
            }
        };
        let slot = &mut list[index as usize];
        slot.1 = Some(Entry { view, owner });
        Record::Entry { index, generation: slot.0.load(Ordering::Relaxed) }
    }

    /// A sampler's entry, one per distinct description.
    pub fn sampler(&self, desc: &D3D12_SAMPLER_DESC) -> Record {
        // SAFETY: 13 plain words.
        let key: [u32; 13] = unsafe { std::mem::transmute_copy(desc) };
        let mut known = self.samplers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&i) = known.get(&key) {
            let list = self.list.read().unwrap_or_else(|e| e.into_inner());
            return Record::Entry { index: i, generation: list[i as usize].0.load(Ordering::Relaxed) };
        }
        let r = self.add(EntryView::Sampler(*desc), 0);
        if let Record::Entry { index, .. } = r {
            known.insert(key, index);
        }
        r
    }

    /// The view of a live entry at this generation.
    #[inline]
    pub fn get(&self, index: u32, generation: u32) -> Option<EntryView> {
        let list = self.list.read().unwrap_or_else(|e| e.into_inner());
        let (g, e) = list.get(index as usize)?;
        (g.load(Ordering::Relaxed) == generation).then_some(e.as_ref()?.view)
    }

    /// The current generation of entry `index`.
    pub fn generation(&self, index: u32) -> u32 {
        let list = self.list.read().unwrap_or_else(|e| e.into_inner());
        list.get(index as usize).map_or(0, |(g, _)| g.load(Ordering::Relaxed))
    }

    /// Free every entry `owner` made (its console resource is gone).
    pub fn release(&self, owner: usize, indices: &[u32]) {
        let mut list = self.list.write().unwrap_or_else(|e| e.into_inner());
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        for &i in indices {
            if let Some((g, e)) = list.get_mut(i as usize) {
                if e.as_ref().is_some_and(|e| e.owner == owner) {
                    *e = None;
                    let next = (g.load(Ordering::Relaxed) + 1).max(1);
                    g.store(next, Ordering::Relaxed);
                    free.push(i);
                }
            }
        }
    }

    pub fn live(&self) -> usize {
        self.list.read().unwrap_or_else(|e| e.into_inner()).iter().filter(|(_, e)| e.is_some()).count()
    }
}

/// The resource a view keeps alive, if any.
pub fn view_resource(v: &HostView) -> Raw {
    match v {
        HostView::Srv { resource, .. } | HostView::Uav { resource, .. } | HostView::Rtv { resource, .. } | HostView::Dsv { resource, .. } => *resource,
        _ => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_record_round_trips_through_its_bytes() {
        let all = [
            Record::Cbv { va: 0xdd2b_00100, size: 256 },
            Record::Buffer { uav: false, va: 0x1_2345_6780, format: 0, elements: 100, stride: 16, flags: 0 },
            Record::Buffer { uav: true, va: 0x40, format: 0, elements: 64, stride: 0, flags: RAW },
            Record::Buffer { uav: false, va: 0x80, format: 41, elements: 7, stride: 0, flags: 0 },
            Record::Entry { index: 12, generation: 3 },
            Record::Null { uav: false, dimension: 4 },
            Record::Null { uav: true, dimension: 1 },
            Record::NullCbv,
        ];
        for r in all {
            assert_eq!(Record::decode(&r.encode()), Some(r));
        }
        let s = Record::Entry { index: 5, generation: 9 };
        assert_eq!(Record::decode16(&s.encode16()), Some(s));
    }

    #[test]
    fn stray_bytes_are_not_records() {
        assert_eq!(Record::decode(&[0; 8]), None);
        let mut w = Record::Cbv { va: 0x1000, size: 256 }.encode();
        w[2] ^= 1;
        assert_eq!(Record::decode(&w), None, "a changed word fails the check");
        assert_eq!(Record::decode16(&[0; 4]), None);
    }

    #[test]
    fn released_entries_go_stale() {
        let e = Entries::default();
        let r = e.add(EntryView::Sampler(D3D12_SAMPLER_DESC::default()), 7);
        let Record::Entry { index, generation } = r else { panic!() };
        assert!(e.get(index, generation).is_some());
        e.release(7, &[index]);
        assert!(e.get(index, generation).is_none());
        let r2 = e.add(EntryView::Sampler(D3D12_SAMPLER_DESC::default()), 8);
        assert_eq!(r2, Record::Entry { index, generation: generation + 1 }, "reused under a new generation");
    }

    #[test]
    fn equal_samplers_share_an_entry() {
        let e = Entries::default();
        let a = D3D12_SAMPLER_DESC { MaxLOD: 4.0, ..Default::default() };
        assert_eq!(e.sampler(&a), e.sampler(&a));
        assert_ne!(e.sampler(&a), e.sampler(&D3D12_SAMPLER_DESC::default()));
        assert_eq!(e.live(), 2);
    }
}
