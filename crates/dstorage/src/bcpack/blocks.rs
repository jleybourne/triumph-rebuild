//! Blocks from records: two back references per block (to one of the last 256 blocks, or to the format's
//! dictionary, which stands in for blocks -1 to -255), plus the residuals of endpoints and indices against them.
//!
//! The plain formats (BC1, BC3, BC4, BC5) keep endpoints and indices packed in two words ([`Plain`]) so one
//! SWAR add sums every element modulo its own width, and the packed form is most of the output block already.
//! BC6H and BC7 keep left-aligned endpoints and signed indices and are written with a bit writer.

use super::consts::{
    ANCHOR2, ANCHOR3A, ANCHOR3B, BC6_BITS, BC6_CODE, BC6_LAYOUT, BC7_BITS, BC7_INDEX,
};
use super::lanes::Sink;
use super::tables::{Bc6Ep, Bc6Ix, Bc7Ep, Bc7Ix, Plain, Tables};
use super::Error;

const HISTORY: usize = 256;

/// The record's endpoint and index references.
#[inline(always)]
fn references(s: &[u8; 32], paired: bool) -> (usize, usize) {
    let (d, ix) = if paired { (interleave(s[0], s[1]), interleave(s[2], s[3])) } else { (s[0], s[1]) };
    let delta = (d >> 1) ^ (d & 1).wrapping_neg();
    (usize::from(ix.wrapping_add(delta)), usize::from(ix))
}

/// Bits 0-3 spread to the even bits.
const SPREAD: [u8; 16] = {
    let mut t = [0u8; 16];
    let mut v = 0;
    while v < 16 {
        t[v] = (v & 1 | (v & 2) << 1 | (v & 4) << 2 | (v & 8) << 3) as u8;
        v += 1;
    }
    t
};

/// Two 4-bit symbols, the first on the even bits.
#[inline(always)]
fn interleave(a: u8, b: u8) -> u8 {
    SPREAD[usize::from(a & 15)] | SPREAD[usize::from(b & 15)] << 1
}

/// Block `i`'s reference `r`: 0 none, else `r` blocks back, before the first block into the dictionary.
#[inline(always)]
fn pick<'a, T>(i: usize, r: usize, history: &'a [T; HISTORY], dictionary: &'a [T], zero: &'a T) -> &'a T {
    if r == 0 {
        zero
    } else if r <= i {
        &history[(i - r) % HISTORY]
    } else {
        // r <= 255, so r - i - 1 < 255.
        &dictionary[r - i - 1]
    }
}

/// Zigzag residual (0, -1, 1, -2, ...) modulo 2^bits.
#[inline(always)]
fn zz(v: u8, bits: u32) -> u64 {
    let mask = ((1u32 << bits) - 1) as u8;
    u64::from(((v >> 1) ^ (v & 1).wrapping_neg()) & mask)
}

/// Adds every field modulo its width; `h` marks each field's top bit.
#[inline(always)]
fn swar_add(a: u64, b: u64, h: u64) -> u64 {
    ((a & !h) + (b & !h)) ^ ((a ^ b) & h)
}

const fn every(step: u32, first: u32, count: u32) -> u64 {
    let mut m = 0u64;
    let mut k = 0;
    while k < count {
        m |= 1 << (first + step * k);
        k += 1;
    }
    m
}

/// Colour word: 2-bit indices (top bits odd), then B5 G6 R5 twice.
const H_COLOUR: u64 = every(2, 1, 16) | 0x8410_8410 << 32;
const IX_COLOUR: u64 = 0xffff_ffff;
/// Alpha word: 3-bit indices, then two bytes.
const L3: u64 = every(3, 0, 16);
const H3: u64 = every(3, 2, 16);
const H_ALPHA: u64 = H3 | 0x8080 << 48;
const IX_ALPHA: u64 = (1 << 48) - 1;

/// The packed colour word as a BC1 block: line index -> code 0 2 3 1.
#[inline(always)]
fn colour_block(v: u64) -> [u8; 8] {
    let ix = v & IX_COLOUR;
    let code = ((ix ^ ix << 1) & every(2, 1, 16)) | (ix >> 1 & every(2, 0, 16));
    ((v >> 32) | code << 32).to_le_bytes()
}

/// The packed alpha word as a BC4 block: endpoints stored 128 up; line index -> code 0 2 3 4 5 6 7 1.
#[inline(always)]
fn alpha_block(v: u64) -> [u8; 8] {
    let ix = v & IX_ALPHA;
    let plus = ((ix & !H3) + L3) ^ (ix & H3);
    let nonzero = (ix | ix >> 1 | ix >> 2) & L3;
    let seven = ix & ix >> 1 & ix >> 2 & L3;
    let code = (plus & nonzero * 7) | seven;
    (((v >> 48) ^ 0x8080) | code << 16).to_le_bytes()
}

/// Two-bit index residuals, one symbol per 2x2 quad.
#[inline(always)]
fn quads(rank: &[u32; 256], s: &[u8]) -> u64 {
    u64::from(rank[usize::from(s[0])] | rank[usize::from(s[1])] << 4 | rank[usize::from(s[2])] << 16 | rank[usize::from(s[3])] << 20)
}

/// Three-bit index residuals, one symbol per two pixels in raster order.
#[inline(always)]
fn pairs(rank: &[u64; 64], s: &[u8]) -> u64 {
    s.iter().enumerate().fold(0, |acc, (k, &v)| acc | rank[usize::from(v & 63)] << (6 * k))
}

#[inline(always)]
fn colour_ep(s: &[u8; 32], r0: usize, g0: usize, b0: usize, r1: usize, g1: usize, b1: usize) -> u64 {
    let c0 = zz(s[r0], 5) << 11 | zz(s[g0], 6) << 5 | zz(s[b0], 5);
    let c1 = zz(s[r1], 5) << 11 | zz(s[g1], 6) << 5 | zz(s[b1], 5);
    (c0 | c1 << 16) << 32
}

#[inline(always)]
fn alpha_ep(s: &[u8; 32], e0: usize, e1: usize) -> u64 {
    zz(s[e0], 8) << 48 | zz(s[e1], 8) << 56
}

pub(crate) struct PlainSink<'a, const K: u8> {
    t: &'a Tables,
    out: &'a mut [u8],
    i: usize,
    history: Box<[Plain; HISTORY]>,
}

impl<'a, const K: u8> PlainSink<'a, K> {
    pub fn new(t: &'a Tables, out: &'a mut [u8]) -> Self {
        Self { t, out, i: 0, history: Box::new([Plain::default(); HISTORY]) }
    }
}

pub(crate) const BC1: u8 = 1;
pub(crate) const BC3: u8 = 3;
pub(crate) const BC4: u8 = 4;
pub(crate) const BC5: u8 = 5;

impl<const K: u8> Sink for PlainSink<'_, K> {
    #[inline(always)]
    fn block(&mut self, s: &[u8; 32]) -> Result<(), Error> {
        let t = self.t;
        let i = self.i;
        let zero = Plain::default();
        let (dictionary, paired) = match K {
            BC1 => (&t.bc1, false),
            BC3 => (&t.bc3, true),
            BC4 => (&t.bc4, false),
            _ => (&t.bc5, true),
        };
        let (ep_ref, ix_ref) = references(s, paired);
        let e = *pick(i, ep_ref, &self.history, dictionary, &zero);
        let x = *pick(i, ix_ref, &self.history, dictionary, &zero);
        let v = match K {
            BC1 => {
                let res = quads(&t.quad1, &s[8..12]) | colour_ep(s, 2, 3, 4, 5, 6, 7);
                let base = (x.a & IX_COLOUR) | (e.a & !IX_COLOUR);
                Plain { a: swar_add(res, base, H_COLOUR), b: 0 }
            }
            BC3 => {
                let ra = pairs(&t.pair3a, &s[6..14]) | alpha_ep(s, 4, 5);
                let rc = quads(&t.quad3c, &s[20..24]) | colour_ep(s, 14, 16, 18, 15, 17, 19);
                let ba = (x.a & IX_ALPHA) | (e.a & !IX_ALPHA);
                let bc = (x.b & IX_COLOUR) | (e.b & !IX_COLOUR);
                Plain { a: swar_add(ra, ba, H_ALPHA), b: swar_add(rc, bc, H_COLOUR) }
            }
            BC4 => {
                let res = pairs(&t.pair4, &s[4..12]) | alpha_ep(s, 2, 3);
                let base = (x.a & IX_ALPHA) | (e.a & !IX_ALPHA);
                Plain { a: swar_add(res, base, H_ALPHA), b: 0 }
            }
            _ => {
                let r0 = pairs(&t.pair5, &s[8..16]) | alpha_ep(s, 4, 5);
                let r1 = pairs(&t.pair5, &s[16..24]) | alpha_ep(s, 6, 7);
                let b0 = (x.a & IX_ALPHA) | (e.a & !IX_ALPHA);
                let b1 = (x.b & IX_ALPHA) | (e.b & !IX_ALPHA);
                Plain { a: swar_add(r0, b0, H_ALPHA), b: swar_add(r1, b1, H_ALPHA) }
            }
        };
        self.history[i % HISTORY] = v;
        match K {
            BC1 => self.out[i * 8..i * 8 + 8].copy_from_slice(&colour_block(v.a)),
            BC3 => {
                self.out[i * 16..i * 16 + 8].copy_from_slice(&alpha_block(v.a));
                self.out[i * 16 + 8..i * 16 + 16].copy_from_slice(&colour_block(v.b));
            }
            BC4 => self.out[i * 8..i * 8 + 8].copy_from_slice(&alpha_block(v.a)),
            _ => {
                self.out[i * 16..i * 16 + 8].copy_from_slice(&alpha_block(v.a));
                self.out[i * 16 + 8..i * 16 + 16].copy_from_slice(&alpha_block(v.b));
            }
        }
        self.i = i + 1;
        Ok(())
    }
}

/// A byte's even and odd bits as two zigzag-coded 4-bit residuals: (even, odd).
const NIBBLES: [[i16; 2]; 256] = {
    let mut t = [[0i16; 2]; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut even = 0u8;
        let mut odd = 0u8;
        let mut k = 0;
        while k < 4 {
            even |= (((b >> (2 * k)) & 1) as u8) << k;
            odd |= (((b >> (2 * k + 1)) & 1) as u8) << k;
            k += 1;
        }
        t[b as usize] =
            [((even >> 1) as i16) ^ -((even & 1) as i16), ((odd >> 1) as i16) ^ -((odd & 1) as i16)];
        b += 1;
    }
    t
};

/// Index residual plus reference, both left-aligned to 16 bits at their own widths, back to `bits` bits
/// (signed). `bits` is never 0 here.
#[inline(always)]
fn add_index(cur: i16, bits: u8, refv: i16, ref_bits: u8) -> i16 {
    let up = |v: i16, b: u8| if b == 0 || b > 16 { 0 } else { (v as u16) << (16 - b) };
    let sum = up(cur, bits).wrapping_add(up(refv, ref_bits)) as i16;
    sum >> (16 - bits)
}

/// Little-endian bit writer for one 128-bit block.
struct Bits {
    v: u128,
    at: u32,
}

impl Bits {
    #[inline(always)]
    fn put(&mut self, value: u64, n: u32) {
        if n > 0 && self.at < 128 {
            self.v |= (u128::from(value) & ((1u128 << n) - 1)) << self.at;
        }
        self.at += n;
    }
}

fn pbits(mode: u16) -> bool {
    matches!(mode, 0 | 1 | 3 | 6 | 7)
}

/// Channel precisions: the p-bit is red's lowest bit; rotation swaps alpha with the rotated channel.
#[inline(always)]
fn bc7_precision(ctrl: u16) -> [u8; 4] {
    let (mode, rot) = (ctrl & 7, usize::from(ctrl >> 3 & 3));
    let mut p = BC7_BITS[usize::from(mode)];
    if rot != 0 {
        p.swap(3, rot - 1);
    } else if pbits(mode) {
        p[0] += 1;
    }
    p
}

pub(crate) struct Bc7Sink<'a> {
    t: &'a Tables,
    out: &'a mut [u8],
    i: usize,
    history: Box<[(Bc7Ep, Bc7Ix); HISTORY]>,
}

impl<'a> Bc7Sink<'a> {
    pub fn new(t: &'a Tables, out: &'a mut [u8]) -> Self {
        Self { t, out, i: 0, history: Box::new([(Bc7Ep::default(), Bc7Ix::default()); HISTORY]) }
    }
}

impl Sink for Bc7Sink<'_> {
    fn block(&mut self, s: &[u8; 32]) -> Result<(), Error> {
        let t = self.t;
        let i = self.i;
        let zero = (Bc7Ep::default(), Bc7Ix::default());
        let (ep_ref, ix_ref) = references(s, true);
        let ctrl = t.ctrl7[usize::from(s[31] & 15) << 5 | usize::from(s[30] & 31)];
        if ctrl == 0 {
            return Err(Error::Stream("unused BC7 control code"));
        }
        let mode = usize::from(ctrl & 7);
        let subsets = usize::from((ctrl & 0xff) >> 6);
        let prec = bc7_precision(ctrl);
        let three = subsets == 3;
        let (or0, if3) = if three { (0u8, 0xffu8) } else { (0xff, 0) };
        let stored = [
            [s[4], s[5], s[6], s[7], s[8], s[9], s[10] & or0, s[11] & or0],
            [s[12], s[13], s[14], s[15], s[16], s[17], s[18] & or0, s[19] & or0],
            [s[20], s[21], s[10] & if3, s[11] & if3, s[18] & if3, s[19] & if3, 0, 0],
        ];
        let r = &pick(i, ep_ref, &self.history, &t.bc7, &zero).0;
        let mut ep = Bc7Ep { ctrl, ..Default::default() };
        // Rows the reference lacks build on its first row.
        let common = subsets.min(usize::from((r.ctrl & 0xff) >> 6));
        let columns = if prec[3] > 0 { 8 } else { 6 };
        for c in 0..columns {
            let p = u32::from(prec[c / 2]);
            if p == 0 {
                continue;
            }
            let mask = 0xffffu16 << (16 - p);
            for row in 0..subsets {
                let v = stored[row][c];
                let residual = (u16::from(v >> 1) ^ u16::from(v & 1).wrapping_neg()) << (16 - p);
                let base = r.rows[if row < common { row } else { 0 }][c] & mask;
                ep.rows[row][c] = base.wrapping_add(residual);
            }
        }
        let bits = BC7_INDEX[mode];
        let rx = &pick(i, ix_ref, &self.history, &t.bc7, &zero).1;
        let mut ix = Bc7Ix { bits, ..Default::default() };
        for (set, offsets) in [[22usize, 24, 26, 28], [12, 14, 16, 20]].into_iter().enumerate() {
            if bits[set] == 0 {
                continue;
            }
            let (out, refs, rb) = (&mut ix.ix[set], &rx.ix[set], rx.bits[set]);
            for (k, o) in offsets.into_iter().enumerate() {
                for b in 0..2 {
                    let [even, odd] = NIBBLES[usize::from(s[o + b])];
                    let pair = if k < 3 { [odd, even] } else { [even, odd] };
                    for (h, v) in pair.into_iter().enumerate() {
                        let p = 4 * k + 2 * b + h;
                        out[p] = add_index(v, bits[set], refs[p], rb);
                    }
                }
            }
        }
        self.history[i % HISTORY] = (ep, ix);
        let block = bc7_pack(&ep, &ix, prec);
        self.out[i * 16..i * 16 + 16].copy_from_slice(&block);
        self.i = i + 1;
        Ok(())
    }
}

#[inline(always)]
fn bc7_pack(ep: &Bc7Ep, ix: &Bc7Ix, prec: [u8; 4]) -> [u8; 16] {
    let ctrl = ep.ctrl;
    let mode = ctrl & 7;
    let subsets = usize::from((ctrl & 0xff) >> 6);
    let part = usize::from(ctrl >> 8 & 63);
    let rot = usize::from(ctrl >> 3 & 3);
    let mut w = Bits { v: 0, at: 0 };
    w.put(1 << mode, u32::from(mode) + 1);
    match mode {
        0 => w.put(part as u64, 4),
        1 | 2 | 3 | 7 => w.put(part as u64, 6),
        4 => {
            w.put(rot as u64, 2);
            w.put(u64::from(ctrl >> 5 & 1), 1);
        }
        5 => w.put(rot as u64, 2),
        _ => {}
    }
    let ends = 2 * subsets;
    let field = |row: usize, col: usize, p: u8| u64::from(ep.rows[row][col] >> (16 - u32::from(p)));
    let mut order = [0usize, 1, 2, 3];
    if rot != 0 {
        order.swap(3, rot - 1);
    }
    for ch in order {
        let p = prec[ch];
        if p == 0 {
            continue;
        }
        let drop = u32::from(ch == 0 && pbits(mode));
        for e in 0..ends {
            w.put(field(e / 2, 2 * ch + e % 2, p) >> drop, u32::from(p) - drop);
        }
    }
    if mode == 1 {
        for row in 0..subsets {
            w.put(field(row, 0, prec[0]) & 1, 1);
        }
    } else if pbits(mode) {
        for e in 0..ends {
            w.put(field(e / 2, e % 2, prec[0]) & 1, 1);
        }
    }
    let anchors: [u8; 3] = match subsets {
        2 => [0, ANCHOR2[part], 0],
        3 => [0, ANCHOR3A[part], ANCHOR3B[part]],
        _ => [0, 0, 0],
    };
    let mut indices = |set: usize| {
        let bits = u32::from(ix.bits[set]);
        for (p, &v) in ix.ix[set].iter().enumerate() {
            let anchor = anchors.contains(&(p as u8));
            w.put(u64::from(v as u16), bits - u32::from(anchor));
        }
    };
    match mode {
        4 => {
            indices(1);
            indices(0);
        }
        5 => {
            indices(0);
            indices(1);
        }
        _ => indices(0),
    }
    w.v.to_le_bytes()
}

fn bc6_index_bits(mode: u8) -> u8 {
    match mode {
        1..=10 => 3,
        11..=14 => 4,
        _ => 0,
    }
}

pub(crate) struct Bc6Sink<'a> {
    t: &'a Tables,
    out: &'a mut [u8],
    i: usize,
    history: Box<[(Bc6Ep, Bc6Ix); HISTORY]>,
}

impl<'a> Bc6Sink<'a> {
    pub fn new(t: &'a Tables, out: &'a mut [u8]) -> Self {
        Self { t, out, i: 0, history: Box::new([(Bc6Ep::default(), Bc6Ix::default()); HISTORY]) }
    }
}

/// 8 + 8 bits interleaved, the first byte on the even bits.
#[inline(always)]
fn wide(a: u8, b: u8) -> u16 {
    let spread = |v: u8| u16::from(SPREAD[usize::from(v & 15)]) | u16::from(SPREAD[usize::from(v >> 4)]) << 8;
    spread(a) | spread(b) << 1
}

#[inline(always)]
fn unzig16(v: u16) -> u16 {
    (v >> 1) ^ (v & 1).wrapping_neg()
}

impl Sink for Bc6Sink<'_> {
    fn block(&mut self, s: &[u8; 32]) -> Result<(), Error> {
        let i = self.i;
        let zero = (Bc6Ep::default(), Bc6Ix { mode: 11, ..Default::default() });
        let (ep_ref, ix_ref) = references(s, true);
        let mode = s[30].wrapping_add(1);
        if !(1..=14).contains(&mode) {
            return Err(Error::Stream("BC6H mode out of range"));
        }
        let two = mode < 11;
        let shift = BC6_BITS[usize::from(mode)].map(|b| 16 - u32::from(b));
        let e0 = [wide(s[4], s[5]), wide(s[6], s[7]), wide(s[8], s[9])].map(unzig16);
        let e1 = [wide(s[10], s[11]), wide(s[12], s[13]), wide(s[14], s[15])].map(unzig16);
        let e2 = [s[16], s[18], s[20]].map(|v| unzig16(v.into()));
        let e3 = [s[17], s[19], s[21]].map(|v| unzig16(v.into()));
        let r = &pick(i, ep_ref, &self.history, &self.t.bc6, &zero).0;
        let mut ep = Bc6Ep::default();
        // Endpoint 0 at the base precision, the others per channel; the reference is cut by the mask after
        // the sum.
        let mask = |sh: u32| 0xffffu16 << sh;
        for c in 0..3 {
            ep.e[0][c] = r.e[0][c].wrapping_add(e0[c] << shift[0]) & mask(shift[0]);
            ep.e[1][c] = r.e[1][c].wrapping_add(e1[c] << shift[1 + c]) & mask(shift[1 + c]);
        }
        if two {
            for c in 0..3 {
                ep.e[2][c] = r.e[2][c].wrapping_add(e2[c] << shift[1 + c]) & mask(shift[1 + c]);
                ep.e[3][c] = r.e[3][c].wrapping_add(e3[c] << shift[1 + c]) & mask(shift[1 + c]);
            }
        } else {
            for c in 0..3 {
                ep.e[2][c] = (ep.e[0][c] >> shift[0]) << shift[1];
            }
            ep.e[3] = ep.e[1];
        }
        let rx = &pick(i, ix_ref, &self.history, &self.t.bc6, &zero).1;
        let bits = bc6_index_bits(mode);
        let ref_bits = bc6_index_bits(rx.mode);
        let mut ix = Bc6Ix { mode, ..Default::default() };
        for k in 0..8 {
            let [even, odd] = NIBBLES[usize::from(s[22 + k])];
            let pair = if k < 6 { [odd, even] } else { [even, odd] };
            for (h, v) in pair.into_iter().enumerate() {
                let p = 2 * k + h;
                ix.ix[p] = add_index(v, bits, rx.ix[p], ref_bits);
            }
        }
        self.history[i % HISTORY] = (ep, ix);
        // Fields: the working value has its top bit flipped.
        let field = |k: usize, c: usize| {
            let sh = if k == 0 { shift[0] } else { shift[1 + c] };
            u64::from((ep.e[k][c] ^ 0x8000) >> sh)
        };
        let mut w = Bits { v: 0, at: 0 };
        let (code, n) = BC6_CODE[usize::from(mode)];
        w.put(code, n);
        let part = s[31] & 31;
        for run in BC6_LAYOUT[usize::from(mode)] {
            if run.ch == 3 {
                w.put(part.into(), 5);
            } else {
                w.put(field(run.ep.into(), run.ch.into()) >> run.first, run.len.into());
            }
        }
        let anchor = if two { usize::from(ANCHOR2[usize::from(part)]) } else { 0 };
        for (p, &v) in ix.ix.iter().enumerate() {
            let short = p == 0 || p == anchor;
            let v = (v as u16).wrapping_add(1 << (bits - 1));
            w.put(v.into(), u32::from(bits) - u32::from(short));
        }
        self.out[i * 16..i * 16 + 16].copy_from_slice(&w.v.to_le_bytes());
        self.i = i + 1;
        Ok(())
    }
}
