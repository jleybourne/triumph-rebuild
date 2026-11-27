//! The interleaved rANS lanes.
//!
//! A stream is 12 or 16 lanes. Each lane is a run of fragments: a header byte (model, reset and long-length
//! bits), a length (one byte, or two for a long fragment) and a payload of rANS bytes, decoded with one of 128
//! static models at 16-bit precision, renormalised a byte at a time below 2^24. Lanes take input bytes in a
//! fixed schedule: each round the lanes step in order, and the first lanes that ask take at most 8 bytes
//! between them. Each lane delivers one symbol per position; a lane that is a position ahead waits. So the
//! byte that goes to a lane depends on every lane's timing, and the schedule is part of the format: it is
//! reproduced here exactly.
//!
//! Inside a round, though, each lane's step depends only on its own state: the lanes are coupled only through
//! the byte budget, and which lanes get bytes is a prefix count over the mask of lanes that ask. So a round is
//! data-parallel: [`avx2`] steps 16 lanes at once (two 8-lane vectors, gathers for the model tables, a rank
//! table for the bytes). [`run`] is the same schedule one lane at a time: the fallback without AVX2, the path
//! the synthetic encoder drives, and the reference the vector path is tested against.

use super::tables::{Tables, SLOTS};
use super::Error;

/// rANS: decode while the state is above this, read while below.
const LOW: u32 = 1 << 24;
/// Input bytes per schedule round.
const ROUND_BYTES: u32 = 8;
/// "No byte" / "no symbol".
const NONE: u16 = 0x100;

/// Where lanes take their bytes from.
pub(crate) trait Feed {
    /// The next byte of the stream for `lane`, or `None` past the end.
    fn next(&mut self, lane: usize) -> Option<u8>;
}

/// The plain case: the stream as one slice.
pub(crate) struct Slice<'a> {
    src: &'a [u8],
    at: usize,
}

impl<'a> Slice<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        Self { src, at: 0 }
    }
}

impl Feed for Slice<'_> {
    #[inline(always)]
    fn next(&mut self, _lane: usize) -> Option<u8> {
        let b = self.src.get(self.at).copied();
        self.at += usize::from(b.is_some());
        b
    }
}

/// Receives each block's record (`lanes x per` symbols, lane-major) as soon as it is complete.
pub(crate) trait Sink {
    fn block(&mut self, record: &[u8; 32]) -> Result<(), Error>;
}

/// One rANS decode of state `x` (above 2^24) with `model`: (symbol, next state).
#[inline(always)]
fn decode_symbol(t: &Tables, model: usize, x: u32) -> (u8, u32) {
    let slot = x & 0xffff;
    let w = t.window[(model * 256 + (slot >> 8) as usize) & (128 * 256 - 1)];
    let fine = (w >> 4) as usize;
    let b = if fine == 0 { (w & 15) as usize } else { usize::from(t.fine[(fine * 256 + (slot & 255) as usize) % t.fine.len()]) };
    let k = (model * SLOTS + b) & (128 * SLOTS - 1);
    let (sf, m) = (t.startf[k], t.recip[k]);
    let (start, f) = (sf & 0xffff, sf >> 16);
    let offset = slot.wrapping_sub(start);
    let q = if b != 0 && f == 1 { offset } else { ((u64::from(offset) * u64::from(m)) >> 32) as u32 };
    let rem = offset.wrapping_sub(q.wrapping_mul(f));
    let sym = ((1u32 << b) >> 1) + q;
    (sym as u8, (x >> 16).wrapping_mul(f).wrapping_add(rem))
}

#[derive(Clone, Copy)]
struct Lane {
    x: u32,
    /// Payload bytes left in the fragment.
    left: u32,
    model: u16,
    latch: u16,
    out: u16,
    /// Symbol bits - 1 at the full alphabet.
    width: u8,
    /// 0 header, 1 length (low byte), 2 length high byte, 3 payload.
    phase: u8,
    long: bool,
    reset: bool,
    need: bool,
    stalled: bool,
}

impl Lane {
    const EMPTY: Lane = Lane {
        x: 0,
        left: 0,
        model: 0,
        latch: NONE,
        out: NONE,
        width: 0,
        phase: 0,
        long: false,
        reset: false,
        need: true,
        stalled: false,
    };

    /// One step: take the latched byte (header, length or payload) and decode at most one symbol.
    #[inline(always)]
    fn step(&mut self, t: &Tables) {
        if self.phase == 3 {
            let mut x = self.x;
            if x < LOW && self.latch != NONE {
                self.left = self.left.wrapping_sub(1);
                x = x << 8 | u32::from(self.latch);
                self.latch = NONE;
            }
            self.need = self.left != 0 && self.latch == NONE;
            if x > LOW {
                let (sym, next) = decode_symbol(t, usize::from(self.model), x);
                self.x = next;
                self.out = u16::from(sym);
            } else if self.left != 0 {
                // At exactly 2^24 the lane neither reads nor decodes again.
                self.x = x;
            } else {
                // The fragment ends; a byte read in this step does not reach the carried state.
                self.phase = 0;
                self.need = self.latch == NONE;
                if self.reset {
                    self.x = 0;
                }
            }
        } else {
            self.header();
        }
    }

    /// A header or length byte (phase 0-2), if one is latched.
    #[inline(always)]
    fn header(&mut self) {
        if self.latch == NONE {
            return;
        }
        let b = self.latch as u8;
        self.latch = NONE;
        self.need = true;
        match self.phase {
            0 => {
                // The top two bits shrink the alphabet (width >= 3, so no underflow).
                let group = self.width - (b >> 6);
                self.model = u16::from(group) * 16 + u16::from(b & 15);
                self.reset = b & 0x10 != 0;
                self.long = b & 0x20 != 0;
                self.left = 0;
                self.phase = 1;
            }
            1 if self.long => {
                self.left = b.into();
                self.phase = 2;
            }
            1 => {
                self.left = u32::from(b) + 1;
                self.phase = 3;
            }
            _ => {
                self.left = (u32::from(b) << 8 | self.left) + 257;
                self.phase = 3;
            }
        }
    }
}

/// Runs the schedule for `blocks` blocks of `N` lanes (`widths`) with `PER` symbols per lane per block, one
/// lane at a time, handing each completed record to `sink`. Bytes after the last one the lanes need are
/// ignored; a stream that stops moving (runs out, or is corrupt) is an error, never a panic.
#[inline(always)]
pub(crate) fn run<const N: usize, const PER: usize, F: Feed, S: Sink>(
    t: &Tables,
    widths: &[u8; N],
    blocks: usize,
    feed: &mut F,
    sink: &mut S,
) -> Result<(), Error> {
    let mut lanes = [Lane::EMPTY; N];
    for (lane, &w) in lanes.iter_mut().zip(widths) {
        lane.width = w;
    }
    let full: u32 = (1 << N) - 1;
    let mut record = [0u8; 32];
    let (mut block, mut sub, mut done, mut idle) = (0usize, 0usize, 0u32, 0u32);
    while block < blocks {
        let mut taken = 0u32;
        let mut moved = false;
        for (j, lane) in lanes.iter_mut().enumerate() {
            let bit = 1u32 << j;
            let before = taken;
            if !lane.stalled {
                lane.step(t);
            }
            let waiting = lane.out != NONE && done & bit != 0;
            if lane.need && !waiting && taken < ROUND_BYTES {
                // Past the end nothing arrives; in a valid stream only finished lanes ask there.
                if let Some(b) = feed.next(j) {
                    lane.latch = u16::from(b);
                    taken += 1;
                }
            }
            lane.stalled = (lane.need && before >= ROUND_BYTES) || waiting;
            if !lane.stalled && lane.out != NONE {
                record[j * PER + sub] = lane.out as u8;
                lane.out = NONE;
                done |= bit;
                moved = true;
            }
        }
        if done == full {
            sub += 1;
            if sub == PER {
                sub = 0;
                block += 1;
                sink.block(&record)?;
            }
            done = 0;
            moved = true;
        }
        // Nothing read and nothing delivered for a few rounds: no lane can go on.
        idle = if taken > 0 || moved { 0 } else { idle + 1 };
        if idle > 3 {
            return Err(Error::Stream("the stream ends early or is corrupt"));
        }
    }
    Ok(())
}

/// The vector path: every lane of a round at once, 8 lanes per AVX2 register.
#[cfg(target_arch = "x86_64")]
pub(crate) mod avx2 {
    use std::arch::x86_64::*;

    use super::{Error, Lane, Sink, Tables, LOW, NONE, ROUND_BYTES};

    pub(crate) fn available() -> bool {
        std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("popcnt")
    }

    /// `RANK[m][j]`: how many of the bits of `m` below bit `j` are set.
    static RANK: [[u8; 8]; 256] = {
        let mut t = [[0u8; 8]; 256];
        let mut m = 0;
        while m < 256 {
            let mut j = 0;
            while j < 8 {
                t[m][j] = (m & ((1 << j) - 1)).count_ones() as u8;
                j += 1;
            }
            m += 1;
        }
        t
    };

    /// The lanes, one array per field (16 lanes; 12-lane formats park lanes 12-15).
    #[repr(C, align(32))]
    struct State {
        x: [u32; 16],
        left: [u32; 16],
        latch: [u32; 16],
        out: [u32; 16],
        model: [u32; 16],
        phase: [u32; 16],
        /// Masks: all ones or 0.
        need: [u32; 16],
        stalled: [u32; 16],
        reset: [u32; 16],
        long: [u32; 16],
        width: [u32; 16],
    }

    macro_rules! ld {
        ($a:expr, $h:expr) => {
            _mm256_load_si256($a.as_ptr().add(8 * $h).cast())
        };
    }
    macro_rules! st {
        ($a:expr, $h:expr, $v:expr) => {
            _mm256_store_si256($a.as_mut_ptr().add(8 * $h).cast(), $v)
        };
    }
    /// `mask ? a : b` per lane.
    macro_rules! sel {
        ($mask:expr, $a:expr, $b:expr) => {
            _mm256_blendv_epi8($b, $a, $mask)
        };
    }

    /// The schedule of [`super::run`], 16 lanes a round. `src[..len]` is the stream; the gather reads at most 3
    /// bytes past a byte it takes, so `src` holds at least 3 more.
    ///
    /// # Safety
    /// The CPU has AVX2 and POPCNT ([`available`]), and `src.len() >= len + 3`.
    #[target_feature(enable = "avx2,popcnt")]
    pub(crate) unsafe fn run<const N: usize, const PER: usize, S: Sink>(
        t: &Tables,
        widths: &[u8; N],
        blocks: usize,
        src: &[u8],
        len: usize,
        sink: &mut S,
    ) -> Result<(), Error> {
        debug_assert!(src.len() >= len + 3);
        let mut s = State {
            x: [0; 16],
            left: [0; 16],
            latch: [u32::from(NONE); 16],
            out: [u32::from(NONE); 16],
            model: [0; 16],
            phase: [0; 16],
            need: [!0; 16],
            stalled: [0; 16],
            reset: [0; 16],
            long: [0; 16],
            width: [0; 16],
        };
        for j in N..16 {
            // Parked for good: payload phase at exactly 2^24 with bytes left and a byte latched never reads,
            // decodes, asks or delivers.
            s.phase[j] = 3;
            s.x[j] = LOW;
            s.left[j] = 1;
            s.latch[j] = 0;
            s.need[j] = 0;
        }
        for (j, &w) in widths.iter().enumerate() {
            s.width[j] = u32::from(w);
        }
        let full: u32 = (1 << N) - 1;
        let lane_bit = _mm256_setr_epi32(1, 2, 4, 8, 16, 32, 64, 128);
        let none = _mm256_set1_epi32(i32::from(NONE));
        let zero = _mm256_setzero_si256();
        let ones = _mm256_set1_epi32(-1);
        let low = _mm256_set1_epi32(LOW as i32);
        let three = _mm256_set1_epi32(3);
        let ffff = _mm256_set1_epi32(0xffff);
        let ff = _mm256_set1_epi32(0xff);
        let fifteen = _mm256_set1_epi32(15);
        let one = _mm256_set1_epi32(1);
        let seven = _mm256_set1_epi32(ROUND_BYTES as i32 - 1);
        let window = t.window.as_ptr().cast::<i32>();
        let fine = t.fine.as_ptr().cast::<i32>();
        let startf = t.startf.as_ptr().cast::<i32>();
        let recip = t.recip.as_ptr().cast::<i32>();
        // Per sub-position, the symbols delivered for it so far (lane j in element j).
        let mut rec = [[0u32; 16]; 2];
        let (mut at, mut block, mut sub, mut done, mut idle) = (0usize, 0usize, 0usize, 0u32, 0u32);
        while block < blocks {
            // 1. Every unstalled lane steps: payload lanes here, header lanes below.
            let mut header = 0u32;
            for h in 0..2 {
                let x0 = ld!(s.x, h);
                let latch0 = ld!(s.latch, h);
                let active = _mm256_xor_si256(ld!(s.stalled, h), ones);
                let p3 = _mm256_cmpeq_epi32(ld!(s.phase, h), three);
                let latched = _mm256_xor_si256(_mm256_cmpeq_epi32(latch0, none), ones);
                let hdr = _mm256_andnot_si256(p3, _mm256_and_si256(active, latched));
                header |= (_mm256_movemask_ps(_mm256_castsi256_ps(hdr)) as u32) << (8 * h);
                let act = _mm256_and_si256(active, p3);
                // Below 2^24: the top byte is 0.
                let below = _mm256_cmpeq_epi32(_mm256_srli_epi32(x0, 24), zero);
                let read = _mm256_and_si256(_mm256_and_si256(act, below), latched);
                let x = sel!(read, _mm256_or_si256(_mm256_slli_epi32(x0, 8), latch0), x0);
                let left = _mm256_add_epi32(ld!(s.left, h), read);
                let latch = sel!(read, none, latch0);
                // Above 2^24: the top byte is not 0 and it is not exactly 2^24.
                let not_above = _mm256_or_si256(_mm256_cmpeq_epi32(_mm256_srli_epi32(x, 24), zero), _mm256_cmpeq_epi32(x, low));
                let dec = _mm256_andnot_si256(not_above, act);
                let model = ld!(s.model, h);
                let slot = _mm256_and_si256(x, ffff);
                let w = _mm256_i32gather_epi32::<4>(window, _mm256_add_epi32(_mm256_slli_epi32(model, 8), _mm256_srli_epi32(slot, 8)));
                let base = _mm256_slli_epi32(model, 4);
                let mut b = _mm256_and_si256(w, fifteen);
                // A window that holds a bucket start: the slot's bucket from its fine table.
                let number = _mm256_srli_epi32(w, 4);
                let needs = _mm256_andnot_si256(_mm256_cmpeq_epi32(number, zero), dec);
                // Masked, so lanes without a fine table load nothing (no branch: which lanes need one is data).
                let at = _mm256_add_epi32(_mm256_slli_epi32(number, 8), _mm256_and_si256(slot, ff));
                let fb = _mm256_mask_i32gather_epi32::<1>(zero, fine, at, needs);
                b = sel!(needs, _mm256_and_si256(fb, ff), b);
                let k = _mm256_add_epi32(base, b);
                let sf = _mm256_i32gather_epi32::<4>(startf, k);
                let m = _mm256_i32gather_epi32::<4>(recip, k);
                let f = _mm256_srli_epi32(sf, 16);
                let offset = _mm256_sub_epi32(slot, _mm256_and_si256(sf, ffff));
                // q = offset * m >> 32, per lane: even lanes, then odd lanes; offset itself where f = 1 (not bucket 0).
                let even = _mm256_srli_epi64(_mm256_mul_epu32(offset, m), 32);
                let odd = _mm256_mul_epu32(_mm256_srli_epi64(offset, 32), _mm256_srli_epi64(m, 32));
                let q = _mm256_blend_epi32(even, odd, 0b1010_1010);
                let unit = _mm256_andnot_si256(_mm256_cmpeq_epi32(b, zero), _mm256_cmpeq_epi32(f, one));
                let q = sel!(unit, offset, q);
                // q * f <= offset < 2^16 (bucket 0: q = 0), so a 16-bit multiply is exact.
                let rem = _mm256_sub_epi32(offset, _mm256_mullo_epi16(q, f));
                // (x >> 16) * f: the low 32 bits of two 32 x 32 multiplies, even and odd lanes.
                let hi = _mm256_srli_epi32(x, 16);
                let pe = _mm256_mul_epu32(hi, f);
                let po = _mm256_mul_epu32(_mm256_srli_epi64(hi, 32), _mm256_srli_epi64(f, 32));
                let decoded = _mm256_add_epi32(_mm256_blend_epi32(pe, _mm256_slli_epi64(po, 32), 0b1010_1010), rem);
                let sym = _mm256_and_si256(_mm256_add_epi32(_mm256_srli_epi32(_mm256_sllv_epi32(one, b), 1), q), ff);
                let left_zero = _mm256_cmpeq_epi32(left, zero);
                let end = _mm256_andnot_si256(dec, _mm256_and_si256(act, left_zero));
                let latch_none = _mm256_cmpeq_epi32(latch, none);
                let x_end = _mm256_andnot_si256(ld!(s.reset, h), x0);
                st!(s.x, h, sel!(dec, decoded, sel!(end, x_end, x)));
                st!(s.left, h, left);
                st!(s.latch, h, latch);
                st!(s.out, h, sel!(dec, sym, ld!(s.out, h)));
                let need_step = sel!(end, latch_none, _mm256_andnot_si256(left_zero, latch_none));
                st!(s.need, h, sel!(act, need_step, ld!(s.need, h)));
                st!(s.phase, h, _mm256_andnot_si256(end, ld!(s.phase, h)));
            }
            // Header and length bytes: a few per fragment, one lane at a time.
            while header != 0 {
                let j = header.trailing_zeros() as usize;
                header &= header - 1;
                let mut lane = Lane {
                    x: s.x[j],
                    left: s.left[j],
                    model: s.model[j] as u16,
                    latch: s.latch[j] as u16,
                    out: s.out[j] as u16,
                    width: s.width[j] as u8,
                    phase: s.phase[j] as u8,
                    long: s.long[j] != 0,
                    reset: s.reset[j] != 0,
                    need: s.need[j] != 0,
                    stalled: false,
                };
                lane.header();
                s.left[j] = lane.left;
                s.model[j] = u32::from(lane.model);
                s.latch[j] = u32::from(lane.latch);
                s.phase[j] = u32::from(lane.phase);
                s.long[j] = if lane.long { !0 } else { 0 };
                s.reset[j] = if lane.reset { !0 } else { 0 };
                s.need[j] = if lane.need { !0 } else { 0 };
            }
            // 2. Who asks: lanes that need a byte and are not a position ahead.
            let done_v = _mm256_set1_epi32(done as i32);
            let mut want = 0u32;
            let mut asks = [zero; 2];
            let mut waiting = [zero; 2];
            for h in 0..2 {
                let bits = _mm256_sllv_epi32(lane_bit, _mm256_set1_epi32(8 * h as i32));
                let has_out = _mm256_xor_si256(_mm256_cmpeq_epi32(ld!(s.out, h), none), ones);
                waiting[h] = _mm256_and_si256(has_out, _mm256_cmpeq_epi32(_mm256_and_si256(done_v, bits), bits));
                asks[h] = _mm256_andnot_si256(waiting[h], ld!(s.need, h));
                want |= (_mm256_movemask_ps(_mm256_castsi256_ps(asks[h])) as u32) << (8 * h);
            }
            // 3. The first `cnt` lanes that ask take the next bytes, in lane order; a lane that asks after the
            // round's eighth byte waits a round.
            let cnt = (len - at).min(ROUND_BYTES as usize) as i32;
            let taken = (want.count_ones() as i32).min(cnt);
            let cnt_v = _mm256_set1_epi32(cnt);
            let full_round = if cnt >= ROUND_BYTES as i32 { ones } else { zero };
            let mut delivered = 0u32;
            for h in 0..2 {
                let ranks = RANK[((want >> (8 * h)) & 0xff) as usize];
                let below = if h == 0 { 0 } else { (want & 0xff).count_ones() as i32 };
                let rank = _mm256_add_epi32(_mm256_cvtepu8_epi32(_mm_loadl_epi64(ranks.as_ptr().cast())), _mm256_set1_epi32(below));
                let fetched = _mm256_and_si256(asks[h], _mm256_cmpgt_epi32(cnt_v, rank));
                // Only fetched lanes read: at + rank < len.
                let byte = _mm256_and_si256(
                    _mm256_mask_i32gather_epi32::<1>(zero, src.as_ptr().add(at).cast(), rank, fetched),
                    ff,
                );
                st!(s.latch, h, sel!(fetched, byte, ld!(s.latch, h)));
                let need = ld!(s.need, h);
                let late = _mm256_and_si256(full_round, _mm256_cmpgt_epi32(rank, seven));
                let stalled = _mm256_or_si256(_mm256_and_si256(need, late), waiting[h]);
                st!(s.stalled, h, stalled);
                let out = ld!(s.out, h);
                let has_out = _mm256_xor_si256(_mm256_cmpeq_epi32(out, none), ones);
                let deliver = _mm256_andnot_si256(stalled, has_out);
                let r = rec[sub].as_mut_ptr().add(8 * h).cast();
                _mm256_storeu_si256(r, sel!(deliver, out, _mm256_loadu_si256(r)));
                st!(s.out, h, sel!(deliver, none, out));
                delivered |= (_mm256_movemask_ps(_mm256_castsi256_ps(deliver)) as u32) << (8 * h);
            }
            at += taken as usize;
            done |= delivered;
            let mut moved = delivered != 0;
            if done == full {
                sub += 1;
                if sub == PER {
                    sub = 0;
                    block += 1;
                    let mut record = [0u8; 32];
                    for j in 0..N {
                        for (p, r) in rec.iter().enumerate().take(PER) {
                            record[j * PER + p] = r[j] as u8;
                        }
                    }
                    sink.block(&record)?;
                }
                done = 0;
                moved = true;
            }
            // Nothing read and nothing delivered for a few rounds: no lane can go on.
            idle = if taken > 0 || moved { 0 } else { idle + 1 };
            if idle > 3 {
                return Err(Error::Stream("the stream ends early or is corrupt"));
            }
        }
        Ok(())
    }
}
