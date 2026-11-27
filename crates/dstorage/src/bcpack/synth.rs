//! Synthetic BCPack data, for tests, the benchmark and the comparison against the fork's decoder: tables we
//! make ourselves (no format constants), random records, and an encoder that packs records into a stream the
//! decoder's schedule takes back exactly. No game data anywhere.

use std::collections::VecDeque;

use super::lanes::{Feed, Sink};
use super::tables::{Tables, BUCKETS, MODELS, TABLES_SIZE};
use super::{consts, dispatch, Error};

/// xorshift64.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Lane widths and symbols per lane per block of BC number `bc`.
pub fn layout(bc: u8) -> (&'static [u8], usize) {
    match bc {
        1 => (&consts::BC1_LANES, 1),
        3 => (&consts::BC3_LANES, 2),
        4 => (&consts::BC4_LANES, 1),
        5 => (&consts::BC5_LANES, 2),
        6 => (&consts::BC6_LANES, 2),
        _ => (&consts::BC7_LANES, 2),
    }
}

/// A table file of our own, [`TABLES_SIZE`] bytes: random models that tile 65536 (symbol 0 always codable),
/// random rank tables, BC7 control words that name a mode with its subset count (one in eight unused), and
/// random dictionaries.
pub fn tables(seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut out = Vec::with_capacity(TABLES_SIZE);
    for m in 0..MODELS {
        let used = m / 16 + 2;
        let weights: Vec<u64> = (0..used).map(|_| 1 + rng.below(1000)).collect();
        let total: u64 = weights.iter().sum();
        let mut f = [0u32; BUCKETS];
        for b in 1..used {
            f[b] = ((weights[b] << 16) / total >> (b - 1)).max(1) as u32;
        }
        let others = |f: &[u32; BUCKETS]| (1..used).map(|b| f[b] << (b - 1)).sum::<u32>();
        while others(&f) > 65534 {
            let b = (1..used).rev().find(|&b| f[b] > 1).expect("a model can always shrink");
            f[b] -= 1;
        }
        f[0] = 65536 - others(&f);
        for v in f {
            out.extend_from_slice(&(v as u16).to_le_bytes());
        }
    }
    for _ in 0..256 + 64 + 256 + 64 + 64 {
        out.push(rng.next() as u8);
    }
    let subsets = [3u16, 2, 3, 2, 1, 1, 1, 2];
    for k in 0..512 {
        let mode = rng.below(8) as u16;
        let part = if mode == 0 { rng.below(16) } else { rng.below(64) } as u16;
        let rot = if mode == 4 || mode == 5 { rng.below(4) as u16 } else { 0 };
        let sel = if mode == 4 { rng.below(2) as u16 } else { 0 };
        let ctrl = mode | rot << 3 | sel << 5 | subsets[usize::from(mode)] << 6 | part << 8;
        let ctrl = if k != 0 && rng.below(8) == 0 { 0 } else { ctrl };
        out.extend_from_slice(&ctrl.to_le_bytes());
    }
    while out.len() < TABLES_SIZE {
        out.push(rng.next() as u8);
    }
    debug_assert_eq!(out.len(), TABLES_SIZE);
    out
}

/// `blocks` random records of format `bc` (lane-major, `lanes x per` symbols), each symbol in its lane's
/// alphabet; about half the symbols are small, as in real data. BC7 records name a used control word and BC6H
/// records a mode in range, so every record decodes.
pub fn records(bc: u8, blocks: usize, seed: u64, t: &Tables) -> Vec<Vec<u8>> {
    let (widths, per) = layout(bc);
    let mut rng = Rng(seed | 1);
    let used: Vec<usize> = (0..512).filter(|&c| t.ctrl7[c] != 0).collect();
    (0..blocks)
        .map(|_| {
            let mut r: Vec<u8> = (0..widths.len() * per)
                .map(|k| {
                    let alphabet = 1u64 << (widths[k / per] + 1);
                    let v = if rng.below(2) == 0 { rng.below(alphabet) } else { rng.below(4.min(alphabet)) };
                    v as u8
                })
                .collect();
            if bc == 7 && !used.is_empty() {
                let code = used[rng.below(used.len() as u64) as usize];
                r[30] = (code & 31) as u8;
                r[31] = (code >> 5) as u8;
            }
            if bc == 6 {
                r[30] = rng.below(14) as u8;
            }
            r
        })
        .collect()
}

/// Packs `records` of format `bc` into a stream. Each lane's symbols are split into fragments of random length
/// (reset between fragments, short and long lengths), each coded with a random model that can code it; a
/// finished lane is parked by a fragment that holds its state at 2^24. `seed` picks the split and the models.
pub fn encode(bc: u8, records: &[Vec<u8>], t: &Tables, seed: u64) -> Vec<u8> {
    let (widths, per) = layout(bc);
    let mut rng = Rng(seed | 1);
    let max_fragment = [1usize, 7, 300, 5000, 1 << 20][rng.below(5) as usize];
    let queues = (0..widths.len())
        .map(|j| {
            let symbols: Vec<u8> = records.iter().flat_map(|r| r[j * per..j * per + per].to_vec()).collect();
            lane(t, widths[j], &symbols, max_fragment, &mut rng).into()
        })
        .collect();
    let mut feed = Queues { queues, out: Vec::new() };
    let mut sink = Records(Vec::new());
    dispatch(bc, t, records.len(), &mut feed, &mut sink).expect("our own stream decodes");
    assert!(sink.0.iter().zip(records).all(|(a, b)| a[..b.len()] == b[..]), "the lanes give back the records");
    feed.out
}

/// The records the lanes of `src` give (no block reconstruction).
pub fn lane_records(bc: u8, src: &[u8], blocks: usize, t: &Tables) -> Result<Vec<Vec<u8>>, Error> {
    let (widths, per) = layout(bc);
    let mut sink = Records(Vec::new());
    dispatch(bc, t, blocks, &mut super::lanes::Slice::new(src), &mut sink)?;
    Ok(sink.0.into_iter().map(|r| r[..widths.len() * per].to_vec()).collect())
}

struct Queues {
    queues: Vec<VecDeque<u8>>,
    out: Vec<u8>,
}

impl Feed for Queues {
    fn next(&mut self, lane: usize) -> Option<u8> {
        let b = self.queues[lane].pop_front()?;
        self.out.push(b);
        Some(b)
    }
}

struct Records(Vec<[u8; 32]>);

impl Sink for Records {
    fn block(&mut self, record: &[u8; 32]) -> Result<(), Error> {
        self.0.push(*record);
        Ok(())
    }
}

/// One lane's bytes: fragments, then the parking fragment.
fn lane(t: &Tables, width: u8, symbols: &[u8], max_fragment: usize, rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < symbols.len() {
        let mut n = (1 + rng.below(max_fragment as u64) as usize).min(symbols.len() - at);
        loop {
            // A fragment followed by another resets the state; the last keeps it at 2^24 for the parking one.
            let reset = at + n < symbols.len();
            if let Some(bytes) = fragment(t, width, &symbols[at..at + n], reset, rng) {
                out.extend(bytes);
                break;
            }
            n = n.div_ceil(2);
        }
        at += n;
    }
    // Parking: model `width` x 16, two payload bytes. With the state at 2^24 the lane takes the length and one
    // payload byte, then neither reads nor decodes again.
    out.extend([0, 1, 0, 0]);
    out
}

/// One fragment (header, length, rANS payload) for `symbols`, or `None` if its payload is too long.
fn fragment(t: &Tables, width: u8, symbols: &[u8], reset: bool, rng: &mut Rng) -> Option<Vec<u8>> {
    let top = symbols.iter().copied().max().unwrap_or(0);
    let need = if top == 0 { 0 } else { 7 - top.leading_zeros() as u8 };
    let zero = symbols.contains(&0);
    let candidates: Vec<(u8, u8)> = (need.max(width.saturating_sub(3))..=width)
        .flat_map(|g| (0..16).map(move |shape| (g, shape)))
        .filter(|&(g, shape)| !zero || t.freq[usize::from(g) * 16 + usize::from(shape)][0] >= 2)
        .collect();
    let (group, shape) = candidates[rng.below(candidates.len() as u64) as usize];
    let model = usize::from(group) * 16 + usize::from(shape);
    let freq = &t.freq[model];
    let mut starts = [0u32; BUCKETS];
    let mut acc = 0;
    for b in 0..usize::from(group) + 2 {
        starts[b] = acc;
        acc += u32::from(freq[b]) << b.saturating_sub(1);
    }
    let mut x = 1u32 << 24;
    let mut emitted = Vec::new();
    for &s in symbols.iter().rev() {
        let (f, start) = if s == 0 {
            (u32::from(freq[0]) - 1, 1)
        } else {
            let b = (8 - s.leading_zeros()) as usize;
            let f = u32::from(freq[b]);
            (f, starts[b] + (u32::from(s) - (1 << (b - 1))) * f)
        };
        while x >= f << 16 {
            emitted.push(x as u8);
            x >>= 8;
        }
        x = ((x / f) << 16) + x % f + start;
    }
    let mut payload = x.to_be_bytes().to_vec();
    payload.extend(emitted.iter().rev());
    let n = payload.len();
    if n > 65792 {
        return None;
    }
    let long = n > 256;
    let header = (width - group) << 6 | u8::from(long) << 5 | u8::from(reset) << 4 | shape;
    let mut out = vec![header];
    if long {
        out.extend_from_slice(&((n - 257) as u16).to_le_bytes());
    } else {
        out.push((n - 1) as u8);
    }
    out.extend(payload);
    Some(out)
}
