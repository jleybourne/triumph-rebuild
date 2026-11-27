//! The format's constant tables (about 78 KB), read from a file outside the repository and turned into the
//! decoder's working forms: per-model bucket records for the rANS step, packed rank tables, packed dictionaries.
//!
//! File layout (little-endian, no padding): 128 x 9 u16 model frequencies (per bucket, the frequency of each of
//! its symbols); rank tables BC1 256 B, BC3 alpha 64 B, BC3 colour 256 B, BC4 64 B, BC5 64 B; 512 u16 BC7
//! control words; then 255 dictionary entries per format: BC1 6 + 16 B, BC3 8 + 32 B, BC4 2 + 16 B,
//! BC5 4 + 32 B, BC6H (u8, 12 u16, u8 mode, 16 i16), BC7 (u16 control, 24 u16, 2 u8 index bits, u16, 31 i16).

use super::Error;

pub(crate) const MODELS: usize = 128;
pub(crate) const BUCKETS: usize = 9;
pub(crate) const DICTIONARY: usize = 255;

/// Bytes of the table file.
pub const TABLES_SIZE: usize = MODELS * BUCKETS * 2
    + 256
    + 64
    + 256
    + 64
    + 64
    + 512 * 2
    + DICTIONARY * (6 + 16 + 8 + 32 + 2 + 16 + 4 + 32 + 25 + 33 + 50 + 66);

/// Buckets per model in the flat bucket arrays (9 used at most; the rest hold the 65536 sentinel).
pub(crate) const SLOTS: usize = 16;

/// Endpoints and indices of the plain formats, packed so that one SWAR add sums every element modulo its width.
///
/// - colour (BC1, BC3 colour): bits 0-31 the 16 two-bit indices (pixel p at 2p), bits 32-63 the two RGB565
///   endpoints exactly as the block stores them;
/// - alpha (BC4, BC5 halves, BC3 alpha): bits 0-47 the 16 three-bit indices, bits 48-63 the two endpoints.
#[derive(Clone, Copy, Default)]
pub(crate) struct Plain {
    pub a: u64,
    pub b: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Bc6Ep {
    /// Endpoint 0 R G B, then endpoints 1-3, left-aligned.
    pub e: [[u16; 3]; 4],
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Bc6Ix {
    pub mode: u8,
    pub ix: [i16; 16],
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Bc7Ep {
    pub ctrl: u16,
    /// Per subset: R0 R1 G0 G1 B0 B1 A0 A1, left-aligned.
    pub rows: [[u16; 8]; 3],
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Bc7Ix {
    pub bits: [u8; 2],
    pub ix: [[i16; 16]; 2],
}

/// The format's constant tables, parsed.
pub struct Tables {
    /// The rANS step's tables, flat so the vector path can gather from them. For model m and bucket b (index
    /// `m * SLOTS + b`):
    /// - `startf`: offset base | symbol frequency << 16. Bucket 0 is symbol 0 alone: it owns slots 1 .. f0 - 1,
    ///   so its base is 1 and its frequency f0 - 1 (slot 0 wraps exactly as the format does);
    /// - `recip`: ceil(2^32 / f), so offset x recip >> 32 divides exactly for offsets and frequencies below
    ///   2^16; 0 for f = 1 (the quotient is the offset) and for bucket 0 (the quotient is always 0).
    pub(crate) startf: Box<[u32]>,
    pub(crate) recip: Box<[u32]>,
    /// Per model and 256-slot window (`m * 256 + w`): the window's bucket | a fine table's number << 4. A window
    /// that holds a bucket start past its first slot has a fine table (number > 0): the bucket of each of its
    /// 256 slots, at `fine[number * 256 + slot % 256]`. So a slot's bucket is one or two lookups, no search.
    pub(crate) window: Box<[u32]>,
    pub(crate) fine: Box<[u8]>,
    /// Raw frequencies, for the synthetic encoder.
    pub(crate) freq: Box<[[u16; BUCKETS]; MODELS]>,
    /// BC1 / BC3 colour rank: low nibble -> pixels 0, 1; high nibble -> pixels 4, 5 (2 bits each).
    pub(crate) quad1: [u32; 256],
    pub(crate) quad3c: [u32; 256],
    /// Three-bit ranks: (low 3 bits) | (bits 4-6) << 3, two pixels per symbol.
    pub(crate) pair3a: [u64; 64],
    pub(crate) pair4: [u64; 64],
    pub(crate) pair5: [u64; 64],
    pub(crate) ctrl7: [u16; 512],
    pub(crate) bc1: Box<[Plain]>,
    pub(crate) bc3: Box<[Plain]>,
    pub(crate) bc4: Box<[Plain]>,
    pub(crate) bc5: Box<[Plain]>,
    pub(crate) bc6: Box<[(Bc6Ep, Bc6Ix)]>,
    pub(crate) bc7: Box<[(Bc7Ep, Bc7Ix)]>,
    /// BLAKE3 of the file: part of every cache key.
    pub(crate) hash: [u8; 32],
}

impl std::fmt::Debug for Tables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tables").field("fine_tables", &self.fine_tables()).finish_non_exhaustive()
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        let mut out = [0; N];
        out.copy_from_slice(head);
        out
    }
    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
}

/// RGB565 from three stored endpoint elements (masked to their widths).
fn rgb565(r: u8, g: u8, b: u8) -> u64 {
    u64::from(r & 31) << 11 | u64::from(g & 63) << 5 | u64::from(b & 31)
}

/// Sixteen 2-bit indices, pixel p at bit 2p.
fn pack2(ix: &[u8]) -> u64 {
    ix.iter().enumerate().fold(0, |acc, (p, &v)| acc | u64::from(v & 3) << (2 * p))
}

/// Sixteen 3-bit indices, pixel p at bit 3p.
fn pack3(ix: &[u8]) -> u64 {
    ix.iter().enumerate().fold(0, |acc, (p, &v)| acc | u64::from(v & 7) << (3 * p))
}

impl Tables {
    /// Parses the table file. Errors (never panics) on a wrong size or a model that does not tile 65536.
    pub fn parse(bytes: &[u8]) -> Result<Tables, Error> {
        if bytes.len() != TABLES_SIZE {
            return Err(Error::TablesSize(bytes.len()));
        }
        let mut r = Reader(bytes);
        let mut startf = vec![0u32; MODELS * SLOTS].into_boxed_slice();
        let mut recip = vec![0u32; MODELS * SLOTS].into_boxed_slice();
        let mut window = vec![0u32; MODELS * 256].into_boxed_slice();
        // Fine table 0 is never used; 3 spare bytes after the last one for 4-byte gathers.
        let mut fine = vec![0u8; 256];
        let mut freq = Box::new([[0u16; BUCKETS]; MODELS]);
        for m in 0..MODELS {
            for f in &mut freq[m] {
                *f = r.u16();
            }
            // Model 16 g + shape uses g + 2 buckets; bucket b > 0 holds 2^(b-1) symbols.
            let used = m / 16 + 2;
            let mut starts = [1u32 << 16; SLOTS + 1];
            let mut at = 0u32;
            for b in 0..used {
                let f = u32::from(freq[m][b]);
                if f == 0 {
                    return Err(Error::Tables("a model has an empty bucket"));
                }
                starts[b] = at;
                at += f << b.saturating_sub(1);
            }
            if at != 1 << 16 {
                return Err(Error::Tables("a model does not tile 65536"));
            }
            for b in 0..used {
                let f = u32::from(freq[m][b]);
                let k = m * SLOTS + b;
                if b == 0 {
                    startf[k] = 1 | (f - 1) << 16;
                } else {
                    // A used bucket holds at least one slot, so its start is below 65536.
                    startf[k] = starts[b] | f << 16;
                    recip[k] = if f == 1 { 0 } else { (1u64 << 32).div_ceil(u64::from(f)) as u32 };
                }
            }
            let bucket = |slot: u32| starts[1..].iter().filter(|&&s| s <= slot).count() as u8;
            for w in 0..256u32 {
                let (first, last) = (bucket(w << 8), bucket(w << 8 | 255));
                window[m * 256 + w as usize] = if first == last {
                    u32::from(first)
                } else {
                    let number = fine.len() / 256;
                    fine.extend((0..256).map(|s| bucket(w << 8 | s)));
                    u32::from(first) | (number as u32) << 4
                };
            }
        }
        fine.extend_from_slice(&[0; 3]);
        let rank1: [u8; 256] = r.take();
        let rank3a: [u8; 64] = r.take();
        let rank3c: [u8; 256] = r.take();
        let rank4: [u8; 64] = r.take();
        let rank5: [u8; 64] = r.take();
        let quad = |rank: &[u8; 256]| std::array::from_fn(|s| u32::from(rank[s] & 15) | u32::from(rank[s] >> 4) << 8);
        let pair = |rank: &[u8; 64]| std::array::from_fn(|s| u64::from(rank[s] & 7) | u64::from(rank[s] >> 4 & 7) << 3);
        let mut ctrl7 = [0u16; 512];
        for c in &mut ctrl7 {
            *c = r.u16();
        }
        let mut plain = |ep: usize, ix: usize, pack: &dyn Fn(&[u8], &[u8]) -> Plain| {
            (0..DICTIONARY)
                .map(|_| {
                    let mut e = [0u8; 8];
                    let mut x = [0u8; 32];
                    for v in &mut e[..ep] {
                        *v = r.u8();
                    }
                    for v in &mut x[..ix] {
                        *v = r.u8();
                    }
                    pack(&e, &x)
                })
                .collect::<Box<[Plain]>>()
        };
        let bc1 = plain(6, 16, &|e, x| Plain { a: pack2(&x[..16]) | (rgb565(e[0], e[1], e[2]) | rgb565(e[3], e[4], e[5]) << 16) << 32, b: 0 });
        let bc3 = plain(8, 32, &|e, x| Plain {
            a: pack3(&x[..16]) | u64::from(e[6]) << 48 | u64::from(e[7]) << 56,
            b: pack2(&x[16..32]) | (rgb565(e[0], e[1], e[2]) | rgb565(e[3], e[4], e[5]) << 16) << 32,
        });
        let bc4 = plain(2, 16, &|e, x| Plain { a: pack3(&x[..16]) | u64::from(e[0]) << 48 | u64::from(e[1]) << 56, b: 0 });
        let bc5 = plain(4, 32, &|e, x| Plain {
            a: pack3(&x[..16]) | u64::from(e[0]) << 48 | u64::from(e[1]) << 56,
            b: pack3(&x[16..32]) | u64::from(e[2]) << 48 | u64::from(e[3]) << 56,
        });
        let bc6 = (0..DICTIONARY)
            .map(|_| {
                // The endpoint entry's own mode byte plays no part in decoding.
                r.u8();
                let mut ep = Bc6Ep::default();
                for v in ep.e.iter_mut().flatten() {
                    *v = r.u16();
                }
                let mut ix = Bc6Ix { mode: r.u8(), ..Default::default() };
                for v in &mut ix.ix {
                    *v = r.u16() as i16;
                }
                (ep, ix)
            })
            .collect();
        let bc7 = (0..DICTIONARY)
            .map(|_| {
                let mut ep = Bc7Ep { ctrl: r.u16(), ..Default::default() };
                for v in ep.rows.iter_mut().flatten() {
                    *v = r.u16();
                }
                let mut ix = Bc7Ix { bits: [r.u8(), r.u8()], ..Default::default() };
                // The widths again; unused.
                r.u16();
                // 16 for set 0 and 15 for set 1 (the last index of set 1 is not stored).
                for v in ix.ix.iter_mut().flatten().take(31) {
                    *v = r.u16() as i16;
                }
                (ep, ix)
            })
            .collect();
        debug_assert!(r.0.is_empty());
        Ok(Tables {
            startf,
            recip,
            window,
            fine: fine.into_boxed_slice(),
            freq,
            quad1: quad(&rank1),
            quad3c: quad(&rank3c),
            pair3a: pair(&rank3a),
            pair4: pair(&rank4),
            pair5: pair(&rank5),
            ctrl7,
            bc1,
            bc3,
            bc4,
            bc5,
            bc6,
            bc7,
            hash: *blake3::hash(bytes).as_bytes(),
        })
    }

    /// Fine bucket tables (256 slots each) the models need: windows that hold a bucket start.
    pub fn fine_tables(&self) -> usize {
        self.fine.len() / 256 - 1
    }

    /// BLAKE3 of the table file.
    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }
}
