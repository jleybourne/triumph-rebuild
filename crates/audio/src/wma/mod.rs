//! A5: our XMA2 frame decoder. An XMA2 frame is a WMA Pro frame of 512 samples per channel (at most two
//! channels, subframes of 512, 256 or 128 samples, no LFE). Written from the bitstream's description;
//! FFmpeg's `wmaprodec.c` and the fork's decoder were read as references, not copied. Only the code and
//! level tables (`tables.rs`, data) are vendored.
//!
//! One frame: a tile header (how each channel splits into subframes), a few frame flags, then each
//! subframe: channel transform, quantisation step, scale factors per band, coefficients (vector-coded
//! then run-level), inverse quantisation, an inverse MDCT, and a sine-window overlap with the previous
//! block. Output is the frame's 512 finished samples per channel; the second half of the last block is
//! kept for the next frame.

mod tables;
mod tx;

use std::sync::OnceLock;

use tables::*;

use crate::xma::FrameDecoder;

const FRAME: usize = 512;
const MIN_SUBFRAME: usize = 128;
const SIZES: usize = 3;
const MAX_BANDS: usize = 29;
const MAX_SUBFRAMES: usize = 32;
/// The output buffer: half a block of overlap, then the frame.
const OUT: usize = FRAME + FRAME / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);

// Bits.

struct Bits<'a> {
    /// The frame's bytes plus 8 bytes of zero padding.
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl Bits<'_> {
    #[inline]
    fn peek32(&self) -> u32 {
        let byte = (self.pos >> 3).min(self.data.len() - 8);
        let word = u64::from_be_bytes(self.data[byte..byte + 8].try_into().unwrap());
        ((word << (self.pos & 7)) >> 32) as u32
    }

    #[inline]
    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek32() >> (32 - n);
        self.pos += n as usize;
        v
    }

    #[inline]
    fn bit(&mut self) -> bool {
        self.read(1) == 1
    }

    fn signed(&mut self, n: u32) -> i32 {
        let v = self.read(n) as i32;
        (v << (32 - n)) >> (32 - n)
    }

    /// Up to 31 bits.
    fn read_long(&mut self, n: u32) -> u32 {
        if n <= 24 {
            self.read(n)
        } else {
            let hi = self.read(n - 16);
            (hi << 16) | self.read(16)
        }
    }

    fn past_end(&self) -> bool {
        self.pos > self.end
    }
}

// Prefix codes. Codes are handed out in table order, each the next free one of its length; decoding
// looks up the next `LUT` bits, and the few longer codes are found by search.

const LUT: u32 = 10;

struct Vlc {
    /// Per `LUT`-bit prefix: (symbol, length), or length 0 when the code is longer.
    table: Vec<(i32, u8)>,
    /// Left-aligned codes longer than `LUT`, sorted: (code, length, symbol).
    long: Vec<(u32, u8, i32)>,
}

impl Vlc {
    fn new(entries: impl Iterator<Item = (i32, u8)>) -> Vlc {
        let mut table = vec![(0, 0u8); 1 << LUT];
        let mut long = Vec::new();
        let mut code = 0u64;
        for (symbol, len) in entries {
            let left = code as u32;
            if u32::from(len) <= LUT {
                let first = (left >> (32 - LUT)) as usize;
                let count = 1usize << (LUT - u32::from(len));
                table[first..first + count].fill((symbol, len));
            } else {
                long.push((left, len, symbol));
            }
            code += 1u64 << (32 - len);
        }
        Vlc { table, long }
    }

    #[inline]
    fn read(&self, b: &mut Bits) -> i32 {
        let v = b.peek32();
        let (symbol, len) = self.table[(v >> (32 - LUT)) as usize];
        if len != 0 {
            b.pos += len as usize;
            return symbol;
        }
        let i = self.long.partition_point(|c| c.0 <= v).max(1) - 1;
        let (_, len, symbol) = self.long[i];
        b.pos += len as usize;
        symbol
    }
}

struct Codes {
    scale: Vlc,
    scale_rl: Vlc,
    coef: [Vlc; 2],
    vec4: Vlc,
    vec2: Vlc,
    vec1: Vlc,
}

fn codes() -> &'static Codes {
    static CODES: OnceLock<Codes> = OnceLock::new();
    CODES.get_or_init(|| {
        let pairs = |t: &'static [(u16, u8)], bias: i32| Vlc::new(t.iter().map(move |&(s, l)| (i32::from(s) + bias, l)));
        Codes {
            scale: pairs(&SCALE, -60),
            scale_rl: pairs(&SCALE_RL, 0),
            coef: [
                Vlc::new(COEF0_SYMS.iter().zip(COEF0_LENS).map(|(&s, l)| (i32::from(s), l))),
                pairs(&COEF1, 0),
            ],
            // Symbol 0 is the escape in both vector codes; it reads as -1.
            vec4: Vlc::new(VEC4_SYMS.iter().zip(VEC4_LENS).map(|(&s, l)| (i32::from(s) - 1, l))),
            vec2: pairs(&VEC2, -1),
            vec1: pairs(&VEC1, 0),
        }
    })
}

/// A value of 8, 16, 24 or 31 bits, its width given by up to three leading flags.
fn large(b: &mut Bits) -> u32 {
    let mut n = 8;
    if b.bit() {
        n += 8;
        if b.bit() {
            n += 8;
            if b.bit() {
                n += 7;
            }
        }
    }
    b.read_long(n)
}

fn log2(v: usize) -> u32 {
    usize::BITS - 1 - v.max(1).leading_zeros()
}

/// Sine windows of 128, 256 and 512 points: `w[i] = sin((i + 0.5) * pi / (2n))`.
fn windows() -> &'static [Vec<f32>; SIZES] {
    static W: OnceLock<[Vec<f32>; SIZES]> = OnceLock::new();
    W.get_or_init(|| {
        [128, 256, 512].map(|n: usize| {
            (0..n).map(|i| ((i as f64 + 0.5) * std::f64::consts::PI / (2.0 * n as f64)).sin() as f32).collect()
        })
    })
}

// The decoder.

#[derive(Clone)]
struct Channel {
    prev_block: usize,
    transmit: bool,
    subframes: usize,
    lens: [usize; MAX_SUBFRAMES],
    cur: usize,
    decoded: usize,
    quant_step: i32,
    reuse_sf: bool,
    sf_step: i32,
    max_sf: i32,
    /// Two sets of scale factors: the current one, and the one it may be built from.
    sf: [[i32; MAX_BANDS]; 2],
    /// The set last written, and the set this subframe uses.
    sf_last: usize,
    sf_use: usize,
    /// Block-size index the last written set belongs to.
    sf_table: usize,
    vec_coeffs: usize,
    out: [f32; OUT],
}

impl Channel {
    fn new() -> Channel {
        Channel {
            prev_block: FRAME,
            transmit: false,
            subframes: 0,
            lens: [0; MAX_SUBFRAMES],
            cur: 0,
            decoded: 0,
            quant_step: 0,
            reuse_sf: false,
            sf_step: 1,
            max_sf: 0,
            sf: [[0; MAX_BANDS]; 2],
            sf_last: 0,
            sf_use: 0,
            sf_table: 0,
            vec_coeffs: 0,
            out: [0.0; OUT],
        }
    }
}

/// Decodes the frames of one mono or stereo XMA2 stream.
#[derive(Clone)]
pub struct Decoder {
    channels: usize,
    num_sfb: [usize; SIZES],
    sfb: [[usize; MAX_BANDS + 1]; SIZES],
    /// For block size `i`, band `b`: the band of block size `x` holding the same frequency.
    sf_map: [[[usize; MAX_BANDS]; SIZES]; SIZES],
    ch: [Channel; 2],
    // The subframe being decoded.
    len: usize,
    in_sub: usize,
    idx: [usize; 2],
    bands: usize,
    table: usize,
    esc_len: u32,
    transmit_vec_count: bool,
    transform: bool,
    transform_band: [bool; MAX_BANDS],
    done: bool,
    tmp: [f32; FRAME],
    frame: Vec<u8>,
}

impl Decoder {
    pub fn new(channels: usize, sample_rate: u32) -> Decoder {
        let channels = channels.clamp(1, 2);
        // XMA's band layout uses one of four rates.
        let rate = match sample_rate {
            r if r > 44_100 => 48_000,
            r if r > 32_000 => 44_100,
            r if r > 24_000 => 32_000,
            _ => 24_000,
        };
        let mut sfb = [[0usize; MAX_BANDS + 1]; SIZES];
        let mut num_sfb = [0usize; SIZES];
        for (i, offsets) in sfb.iter_mut().enumerate() {
            let len = FRAME >> i;
            let mut band = 1;
            for &freq in &CRITICAL_FREQ {
                if offsets[band - 1] >= len {
                    break;
                }
                let at = ((len * 2 * freq as usize) / rate + 2) & !3;
                if at > offsets[band - 1] {
                    offsets[band] = at;
                    band += 1;
                }
                if at >= len {
                    break;
                }
            }
            offsets[band - 1] = len;
            num_sfb[i] = band - 1;
        }
        let mut sf_map = [[[0usize; MAX_BANDS]; SIZES]; SIZES];
        for i in 0..SIZES {
            for b in 0..num_sfb[i] {
                let centre = ((sfb[i][b] + sfb[i][b + 1] - 1) << i) >> 1;
                for x in 0..SIZES {
                    let mut v = 0;
                    while v + 1 < MAX_BANDS && (sfb[x][v + 1] << x) < centre {
                        v += 1;
                    }
                    sf_map[i][x][b] = v;
                }
            }
        }
        Decoder {
            channels,
            num_sfb,
            sfb,
            sf_map,
            ch: [Channel::new(), Channel::new()],
            len: FRAME,
            in_sub: 0,
            idx: [0, 1],
            bands: 0,
            table: 0,
            esc_len: 0,
            transmit_vec_count: false,
            transform: false,
            transform_band: [false; MAX_BANDS],
            done: false,
            tmp: [0.0; FRAME],
            frame: Vec::with_capacity(1 << 12),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Forgets the overlap (a new stream).
    pub fn reset(&mut self) {
        self.ch = [Channel::new(), Channel::new()];
    }

    /// Decodes one frame (`bits` bits from its 15-bit length field on, MSB first) and appends 512
    /// samples per channel, interleaved, as floats in [-1, 1). A bad frame gives silence and an error.
    pub fn decode_f32(&mut self, data: &[u8], bits: usize, out: &mut Vec<f32>) -> Result<(), Error> {
        let mut frame = std::mem::take(&mut self.frame);
        frame.clear();
        frame.extend_from_slice(&data[..bits.div_ceil(8).min(data.len())]);
        frame.extend_from_slice(&[0; 8]);
        let mut b = Bits { data: &frame, pos: 0, end: bits };
        let len = b.read(15) as usize;
        let result = self.frame(&mut b, len);
        self.frame = frame;
        if result.is_err() {
            for c in 0..self.channels {
                self.ch[c] = Channel::new();
            }
        }
        let n = self.channels;
        let start = out.len();
        out.resize(start + FRAME * n, 0.0);
        for c in 0..n {
            for (i, &v) in self.ch[c].out[..FRAME].iter().enumerate() {
                out[start + i * n + c] = v;
            }
            self.ch[c].out.copy_within(FRAME.., 0);
        }
        result
    }

    fn frame(&mut self, b: &mut Bits, len: usize) -> Result<(), Error> {
        self.tile_header(b)?;
        // A post-processing matrix (not applied by decoders).
        if self.channels > 1 && b.bit() && b.bit() {
            b.pos += 4 * self.channels * self.channels;
        }
        // Dynamic range compression gain (not applied).
        b.pos += 8;
        // Samples to trim at the stream's start or end (the container's business).
        if b.bit() {
            if b.bit() {
                b.pos += log2(FRAME * 2) as usize;
            }
            if b.bit() {
                b.pos += log2(FRAME * 2) as usize;
            }
        }
        self.done = false;
        for c in 0..self.channels {
            let ch = &mut self.ch[c];
            ch.decoded = 0;
            ch.cur = 0;
            ch.reuse_sf = false;
        }
        while !self.done {
            self.subframe(b)?;
        }
        if b.past_end() || b.pos > len {
            return Err(Error("frame overread"));
        }
        Ok(())
    }

    /// How each channel splits into subframes.
    fn tile_header(&mut self, b: &mut Bits) -> Result<(), Error> {
        let n = self.channels;
        let mut samples = [0usize; 2];
        let mut contains = [false; 2];
        let mut in_sub = n;
        let mut min_len = 0;
        for c in 0..n {
            self.ch[c].subframes = 0;
        }
        let fixed = b.bit();
        loop {
            for c in 0..n {
                contains[c] = samples[c] == min_len
                    && (fixed || in_sub == 1 || min_len == FRAME - MIN_SUBFRAME || b.bit());
            }
            let len = if min_len == FRAME - MIN_SUBFRAME {
                MIN_SUBFRAME
            } else {
                // Not the longest: one more bit picks half or a quarter.
                let shift = if b.bit() { 1 + b.read(1) } else { 0 };
                FRAME >> shift
            };
            min_len += len;
            for c in 0..n {
                let ch = &mut self.ch[c];
                if contains[c] {
                    if ch.subframes >= MAX_SUBFRAMES {
                        return Err(Error("too many subframes"));
                    }
                    ch.lens[ch.subframes] = len;
                    ch.subframes += 1;
                    samples[c] += len;
                    if samples[c] > FRAME {
                        return Err(Error("channel longer than its frame"));
                    }
                } else if samples[c] <= min_len {
                    if samples[c] < min_len {
                        in_sub = 0;
                        min_len = samples[c];
                    }
                    in_sub += 1;
                }
            }
            if min_len >= FRAME {
                break;
            }
            if b.past_end() {
                return Err(Error("tile header"));
            }
        }
        Ok(())
    }

    fn subframe(&mut self, b: &mut Bits) -> Result<(), Error> {
        let n = self.channels;
        // The next block: the earliest undecoded subframe of any channel.
        let mut offset = FRAME;
        let mut len = FRAME;
        for c in 0..n {
            let ch = &self.ch[c];
            if ch.decoded < offset {
                offset = ch.decoded;
                len = ch.lens[ch.cur.min(MAX_SUBFRAMES - 1)];
            }
        }
        let mut remaining = FRAME * n;
        self.in_sub = 0;
        for c in 0..n {
            let ch = &mut self.ch[c];
            remaining -= ch.decoded;
            if ch.decoded == offset && ch.cur < ch.subframes && ch.lens[ch.cur] == len {
                remaining -= len;
                ch.decoded += len;
                self.idx[self.in_sub] = c;
                self.in_sub += 1;
            }
        }
        if self.in_sub == 0 {
            return Err(Error("no channel in subframe"));
        }
        self.done = remaining == 0;
        self.table = log2(FRAME / len) as usize;
        self.bands = self.num_sfb[self.table];
        self.len = len;
        self.esc_len = log2(len - 1) + 1;

        // Fill bits.
        if b.bit() {
            let mut fill = b.read(2) as usize;
            if fill == 0 {
                let w = b.read(4);
                fill = b.read(w) as usize + 1;
            }
            b.pos += fill;
        }
        if b.bit() {
            return Err(Error("reserved bit"));
        }
        self.channel_transform(b)?;

        let mut any = false;
        for i in 0..self.in_sub {
            let c = self.idx[i];
            self.ch[c].transmit = b.bit();
            any |= self.ch[c].transmit;
        }
        if any {
            self.transmit_vec_count = b.bit();
            if self.transmit_vec_count {
                let w = log2(len.div_ceil(4)) + 1;
                for i in 0..self.in_sub {
                    let v = (b.read(w) as usize) << 2;
                    if v > len {
                        return Err(Error("vector coefficient count"));
                    }
                    self.ch[self.idx[i]].vec_coeffs = v;
                }
            } else {
                for i in 0..self.in_sub {
                    self.ch[self.idx[i]].vec_coeffs = len;
                }
            }
            // Quantisation step: 90 for 16-bit output, plus a 6-bit signed step, extended in 5-bit pieces.
            let mut step = b.signed(6);
            let mut quant = 90 + step;
            if step == -32 || step == 31 {
                let negative = step == -32;
                let mut more = 0;
                while b.pos + 5 < b.end {
                    step = b.read(5) as i32;
                    if step != 31 {
                        break;
                    }
                    more += 31;
                }
                quant += if negative { -(more + step) } else { more + step };
            }
            if self.in_sub == 1 {
                self.ch[self.idx[0]].quant_step = quant;
            } else {
                let w = b.read(3);
                for i in 0..self.in_sub {
                    let ch = &mut self.ch[self.idx[i]];
                    ch.quant_step = quant;
                    if b.bit() {
                        ch.quant_step += if w > 0 { b.read(w) as i32 + 1 } else { 1 };
                    }
                }
            }
            self.scale_factors(b)?;
        }

        // Coefficients go straight into the output, where the inverse transform will put the samples.
        let at = offset + FRAME / 2;
        for i in 0..self.in_sub {
            let c = self.idx[i];
            if self.ch[c].transmit && b.pos < b.end {
                self.coefficients(b, c, at);
            } else {
                self.ch[c].out[at..at + len].fill(0.0);
            }
        }

        if any {
            self.inverse_transform(at);
            let scale = 2.0 / len as f32 / 32768.0;
            let sfb = self.sfb[self.table];
            for i in 0..self.in_sub {
                let ch = &mut self.ch[self.idx[i]];
                let sf = &ch.sf[ch.sf_use];
                for band in 0..self.bands {
                    let (s, e) = (sfb[band], sfb[band + 1].min(len));
                    let exp = ch.quant_step - (ch.max_sf - sf[band]) * ch.sf_step;
                    let q = 10f64.powf(f64::from(exp) / 20.0) as f32;
                    for (t, &v) in self.tmp[s..e].iter_mut().zip(&ch.out[at + s..at + e]) {
                        *t = v * q;
                    }
                }
                tx::imdct_half(&self.tmp[..len], &mut ch.out[at..at + len], scale);
            }
        }

        // Window and overlap with the previous block.
        let w = windows();
        for i in 0..self.in_sub {
            let ch = &mut self.ch[self.idx[i]];
            let mut win = ch.prev_block;
            let mut start = at - win / 2;
            if len < win {
                start += (win - len) / 2;
                win = len;
            }
            overlap(&mut ch.out[start..start + win], &w[log2(win) as usize - 7]);
            ch.prev_block = len;
        }
        for i in 0..self.in_sub {
            let ch = &mut self.ch[self.idx[i]];
            if ch.cur >= ch.subframes {
                return Err(Error("subframe overrun"));
            }
            ch.cur += 1;
        }
        if b.past_end() {
            return Err(Error("subframe overread"));
        }
        Ok(())
    }

    fn channel_transform(&mut self, b: &mut Bits) -> Result<(), Error> {
        self.transform = false;
        if self.channels == 1 {
            return Ok(());
        }
        if b.bit() {
            return Err(Error("channel transform bit"));
        }
        if self.in_sub == 2 {
            // 0: mid/side; 10: none; 11: not defined for two channels.
            if !b.bit() {
                self.transform = true;
            } else if b.bit() {
                return Err(Error("channel transform type"));
            }
        }
        if self.transform {
            if b.bit() {
                self.transform_band = [true; MAX_BANDS];
            } else {
                for i in 0..self.bands {
                    self.transform_band[i] = b.bit();
                }
            }
        }
        Ok(())
    }

    fn inverse_transform(&mut self, at: usize) {
        if !self.transform || self.in_sub != 2 {
            return;
        }
        let sfb = self.sfb[self.table];
        let [a, b] = &mut self.ch;
        for band in 0..self.bands {
            let range = at + sfb[band]..at + sfb[band + 1].min(self.len);
            if self.transform_band[band] {
                for y in range {
                    let (m, s) = (a.out[y], b.out[y]);
                    a.out[y] = m - s;
                    b.out[y] = m + s;
                }
            } else {
                for y in range {
                    a.out[y] *= 181.0 / 128.0;
                    b.out[y] *= 181.0 / 128.0;
                }
            }
        }
    }

    fn scale_factors(&mut self, b: &mut Bits) -> Result<(), Error> {
        let codes = codes();
        for i in 0..self.in_sub {
            let c = self.idx[i];
            let map = self.sf_map[self.table][self.ch[c].sf_table];
            let ch = &mut self.ch[c];
            let target = 1 - ch.sf_last;
            ch.sf_use = target;
            if ch.reuse_sf {
                // Start from the last set, resampled to this block size's bands.
                let from = ch.sf[ch.sf_last];
                for band in 0..self.bands {
                    ch.sf[target][band] = from[map[band]];
                }
            }
            if ch.cur == 0 || b.bit() {
                if !ch.reuse_sf {
                    ch.sf_step = b.read(2) as i32 + 1;
                    let mut v = 45 / ch.sf_step;
                    for band in 0..self.bands {
                        v += codes.scale.read(b);
                        ch.sf[target][band] = v;
                    }
                } else {
                    // Run-level changes to the resampled set.
                    let mut band = 0;
                    while band < self.bands {
                        let code = codes.scale_rl.read(b);
                        let (skip, level, negative) = match code {
                            0 => {
                                let v = b.read(14);
                                ((v & 0x3f) >> 1, (v >> 6) as i32, v & 1 == 0)
                            }
                            1 => break,
                            _ => (u32::from(SCALE_RL_RUN[code as usize]), i32::from(SCALE_RL_LEVEL[code as usize]), !b.bit()),
                        };
                        band += skip as usize;
                        if band >= self.bands {
                            return Err(Error("scale factor run"));
                        }
                        ch.sf[target][band] += if negative { -level } else { level };
                        band += 1;
                    }
                }
                ch.sf_last = target;
                ch.sf_table = self.table;
                ch.reuse_sf = true;
            }
            ch.max_sf = ch.sf[ch.sf_use][..self.bands].iter().copied().max().unwrap_or(0);
        }
        Ok(())
    }

    /// One channel's coefficients into `out[at..at + len]`: vector-coded until the zeros run long,
    /// then run-level coded. Errors end the channel's coefficients early (the rest stay zero).
    fn coefficients(&mut self, b: &mut Bits, c: usize, at: usize) {
        let codes = codes();
        let table = usize::from(b.bit());
        let (run, level): (&[u16], &[f32]) = if table == 1 { (&COEF1_RUN, &COEF1_LEVEL) } else { (&COEF0_RUN, &COEF0_LEVEL) };
        let len = self.len;
        let rl_after = len >> 8;
        let vec_count = self.transmit_vec_count;
        let esc_len = self.esc_len;
        let ch = &mut self.ch[c];
        let coeffs = &mut ch.out[at..at + len];
        let mut rl = false;
        let mut cur = 0;
        let mut zeros = 0;
        while (vec_count || !rl) && cur + 3 < ch.vec_coeffs {
            let mut vals = [0u32; 4];
            let v4 = codes.vec4.read(b);
            if v4 < 0 {
                for i in [0, 2] {
                    let v2 = codes.vec2.read(b);
                    if v2 < 0 {
                        for v in &mut vals[i..i + 2] {
                            let mut x = codes.vec1.read(b) as u32;
                            if x as usize == VEC1.len() - 1 {
                                x += large(b);
                            }
                            *v = x;
                        }
                    } else {
                        vals[i] = (v2 >> 4) as u32;
                        vals[i + 1] = (v2 & 15) as u32;
                    }
                }
            } else {
                vals = [(v4 >> 12) as u32, (v4 >> 8 & 15) as u32, (v4 >> 4 & 15) as u32, (v4 & 15) as u32];
            }
            for v in vals {
                if v != 0 {
                    let f = v as f32;
                    coeffs[cur] = if b.bit() { f } else { -f };
                    zeros = 0;
                } else {
                    coeffs[cur] = 0.0;
                    zeros += 1;
                    rl |= zeros > rl_after;
                }
                cur += 1;
            }
            if b.past_end() {
                coeffs[cur..].fill(0.0);
                return;
            }
        }
        if cur >= len {
            return;
        }
        coeffs[cur..].fill(0.0);
        let mask = len - 1;
        let mut pos = cur;
        while pos < len {
            let code = codes.coef[table].read(b);
            if code > 1 {
                pos += run[code as usize] as usize;
                let v = level[code as usize];
                coeffs[pos & mask] = if b.bit() { v } else { -v };
            } else if code == 1 {
                break;
            } else {
                let v = large(b) as i32;
                if b.bit() {
                    if b.bit() {
                        if b.bit() {
                            return;
                        }
                        pos += b.read(esc_len) as usize + 4;
                    } else {
                        pos += b.read(2) as usize + 1;
                    }
                }
                coeffs[pos & mask] = if b.bit() { v as f32 } else { -(v as f32) };
            }
            if b.past_end() {
                return;
            }
            pos += 1;
        }
    }
}

/// The sine-window overlap of the previous block's tail (first half of `block`) and this block's head
/// (second half).
fn overlap(block: &mut [f32], w: &[f32]) {
    let half = block.len() / 2;
    let n = block.len();
    for k in 0..half {
        let j = n - 1 - k;
        let (s0, s1) = (block[k], block[j]);
        let (wk, wj) = (w[k], w[j]);
        block[k] = s0 * wj - s1 * wk;
        block[j] = s0 * wk + s1 * wj;
    }
}

/// Float to 16-bit as FFmpeg's converter rounds it (to nearest, ties to even).
pub fn to_i16(v: f32) -> i16 {
    (v * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16
}

/// `FrameDecoder` for the XMA contexts: one per voice.
pub struct Xma2 {
    dec: Decoder,
    pcm: Vec<f32>,
}

impl Xma2 {
    pub fn new(channels: usize, rate: u32) -> Xma2 {
        Xma2 { dec: Decoder::new(channels, rate), pcm: Vec::with_capacity(FRAME * 2) }
    }
}

impl FrameDecoder for Xma2 {
    fn decode(&mut self, frame: &[u8], bits: usize, out: &mut Vec<i16>) -> bool {
        self.pcm.clear();
        let ok = self.dec.decode_f32(frame, bits, &mut self.pcm).is_ok();
        out.extend(self.pcm.iter().map(|&v| to_i16(v)));
        ok
    }
}

/// The factory to hand `acp::Hal::set_xma_decoder`.
pub fn factory() -> crate::xma::DecoderFactory {
    std::sync::Arc::new(|channels, rate| Box::new(Xma2::new(channels, rate)))
}

#[cfg(test)]
mod tests;
