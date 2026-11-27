//! A3: XMA contexts, the console's hardware audio decoder as the title drives it through the
//! coprocessor (`acp`): enable, disable, and "advance the read block".
//!
//! **Stream format** (facts from the fork's notes, our own code). Input is a buffer of 2048-byte
//! packets. Each packet starts with a 32-bit header, most significant bit first: frames that start in
//! this packet (6 bits), bits at the start of the payload that finish the previous frame (15),
//! metadata (3), and how many packets of other streams follow before this stream's next one (8). Frames
//! run on across packets, skipping each header. A frame starts with its own length in bits (15) and
//! decodes to 512 samples per channel.
//!
//! **Context** (16 words in the SHAPE XMA array; bit ranges are low..high):
//!
//! | word | fields |
//! |:--|:--|
//! | 0 | input 0 size in packets (0..12), loop count (12..20, 255 = forever), valid inputs (20..22), output size in 256-byte blocks (22..27), write block (27..32, written here) |
//! | 1 | input 1 size (0..12), subframes kept in the loop's last frame - 1 (12..14), subframes skipped after a loop (17..20), subframes skipped at the start (24..27), rate (27..29: 24, 32, 44.1, 48 kHz), stereo (29), output writable (31) |
//! | 2 | read position in bits (0..26), error status (26..31), error (31) |
//! | 3 | loop start in bits (0..26) |
//! | 4 | loop end in bits (0..26), current input (31) |
//! | 5, 6 | input 0 and 1 device addresses · 7 output ring device address · 8 overlap buffer · 9 read block (0..5) |
//!
//! Output is 16-bit PCM, interleaved when stereo, into a ring of 256-byte blocks. The title takes what
//! lies between its read block and our write block, then advances its read block by command.
//!
//! **Decoding.** The frame decoder (WMA Pro's transform) is the `FrameDecoder` trait; this crate ships
//! `Silence`, which keeps every timing exact and writes zeros. See `agents/audio.md` for the decoder.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::heap::ApuHeap;
use crate::lock;

pub const PACKET_BYTES: usize = 2048;
const PACKET_BITS: usize = PACKET_BYTES * 8;
const HEADER_BITS: usize = 32;
const LENGTH_BITS: usize = 15;
pub const FRAME_SAMPLES: usize = 512;
pub const SUBFRAME_SAMPLES: usize = 128;
pub const BLOCK_BYTES: usize = 256;
const BLOCK_SAMPLES: usize = BLOCK_BYTES / 2;
pub const CONTEXT_BYTES: usize = 64;
pub const RATES: [u32; 4] = [24_000, 32_000, 44_100, 48_000];
/// Error status written when both inputs are used up.
pub const STATUS_INPUT_EXHAUSTED: u32 = 4;

// The packet and frame walk.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub frames: u32,
    pub first_frame_bit: u32,
    pub metadata: u32,
    pub skip: u32,
}

impl PacketHeader {
    pub fn parse(h: u32) -> PacketHeader {
        PacketHeader {
            frames: h >> 26,
            first_frame_bit: (h >> 11) & 0x7fff,
            metadata: (h >> 8) & 7,
            skip: h & 0xff,
        }
    }

    pub fn word(&self) -> u32 {
        self.frames << 26 | self.first_frame_bit << 11 | self.metadata << 8 | self.skip
    }
}

/// One frame found in a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Bit position of its length field, and of the next frame of the stream (`None`: no more).
    pub start: usize,
    pub next: Option<usize>,
    /// Its bits from the length field on, most significant first, and how many.
    pub data: Vec<u8>,
    pub bits: usize,
}

/// Bit positions in a buffer of packets. A position is an absolute bit offset; positions inside a
/// header are never produced.
pub struct Stream<'a> {
    buf: &'a [u8],
}

impl<'a> Stream<'a> {
    pub fn new(buf: &'a [u8]) -> Stream<'a> {
        Stream { buf }
    }

    fn packets(&self) -> usize {
        self.buf.len() / PACKET_BYTES
    }

    pub fn header(&self, packet: usize) -> Option<PacketHeader> {
        let b = self.buf.get(packet * PACKET_BYTES..)?.get(..4)?;
        Some(PacketHeader::parse(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    }

    /// The first frame start of a packet.
    fn first_frame(&self, packet: usize) -> Option<usize> {
        let h = self.header(packet)?;
        Some(packet * PACKET_BITS + HEADER_BITS + h.first_frame_bit as usize)
    }

    /// The stream's packet after `packet`.
    fn next_packet(&self, packet: usize) -> usize {
        packet + 1 + self.header(packet).map_or(0, |h| h.skip as usize)
    }

    /// `n` bits on from `pos`, skipping headers and other streams' packets; `None` past the buffer.
    fn step(&self, mut pos: usize, mut n: usize) -> Option<usize> {
        loop {
            let packet = pos / PACKET_BITS;
            if packet >= self.packets() {
                return None;
            }
            let room = (packet + 1) * PACKET_BITS - pos;
            if n < room {
                return Some(pos + n);
            }
            n -= room;
            pos = self.next_packet(packet) * PACKET_BITS + HEADER_BITS;
            if n == 0 {
                return (pos / PACKET_BITS < self.packets()).then_some(pos);
            }
        }
    }

    fn bit(&self, pos: usize) -> u32 {
        u32::from(self.buf[pos / 8] >> (7 - pos % 8) & 1)
    }

    /// `n` bits from `pos` into `out` (most significant first); the position after them, which may
    /// be the end of the buffer (`usize::MAX`).
    fn gather(&self, mut pos: usize, n: usize, out: &mut Vec<u8>) -> Option<usize> {
        out.clear();
        out.resize(n.div_ceil(8), 0);
        let mut i = 0;
        while i < n {
            let packet = pos / PACKET_BITS;
            if packet >= self.packets() {
                return None;
            }
            let run = ((packet + 1) * PACKET_BITS - pos).min(n - i);
            for k in 0..run {
                out[(i + k) / 8] |= (self.bit(pos + k) as u8) << (7 - (i + k) % 8);
            }
            i += run;
            pos += run;
            if pos % PACKET_BITS == 0 {
                pos = self.next_packet(packet) * PACKET_BITS + HEADER_BITS;
            }
        }
        Some(if pos / PACKET_BITS < self.packets() { pos } else { usize::MAX })
    }

    fn length_at(&self, pos: usize) -> Option<usize> {
        let mut b = Vec::new();
        self.gather(pos, LENGTH_BITS, &mut b)?;
        let len = (usize::from(b[0]) << 7) | usize::from(b[1] >> 1);
        // All ones is padding; a frame holds at least its own length field.
        (len > LENGTH_BITS && len != 0x7fff).then_some(len)
    }

    /// The frame at `pos` (a frame start, or the first frame when `pos` is before packet 0's first).
    pub fn frame(&self, pos: usize) -> Option<Frame> {
        let first0 = self.first_frame(0)?;
        let start = if pos < first0 { first0 } else { pos };
        let bits = self.length_at(start)?;
        let mut data = Vec::new();
        let end = self.gather(start, bits, &mut data)?;
        // Which frame of its packet is this? The header says how many start there.
        let packet = start / PACKET_BITS;
        let started = self.header(packet)?.frames as usize;
        let mut before = 0;
        let mut at = self.first_frame(packet)?;
        while at < start {
            match self.length_at(at).and_then(|l| self.step(at, l)) {
                Some(next) if next / PACKET_BITS == packet => {
                    at = next;
                    before += 1;
                }
                _ => break,
            }
        }
        let next = if before + 1 < started && end != usize::MAX {
            Some(end)
        } else {
            // The last frame starting in its packet: the next one is the first of a later packet.
            let mut q = if end != usize::MAX && end / PACKET_BITS != packet {
                end / PACKET_BITS
            } else {
                self.next_packet(packet)
            };
            loop {
                match self.header(q) {
                    Some(h) if h.frames > 0 => break self.first_frame(q),
                    Some(_) => q = self.next_packet(q),
                    None => break None,
                }
            }
        };
        Some(Frame {
            start,
            next,
            data,
            bits,
        })
    }
}

// Decoders.

/// Decodes one frame. `frame` holds `bits` bits from the 15-bit length field on, most significant
/// first. Appends `FRAME_SAMPLES` × channels samples, interleaved; false if the frame is bad (the
/// caller then appends silence of the same length).
pub trait FrameDecoder: Send {
    fn decode(&mut self, frame: &[u8], bits: usize, out: &mut Vec<i16>) -> bool;
}

/// Makes a decoder for a voice: (channels, sample rate).
pub type DecoderFactory = Arc<dyn Fn(usize, u32) -> Box<dyn FrameDecoder> + Send + Sync>;

/// Writes zeros: every voice keeps its exact timing, loops and end, and is silent.
pub struct Silence(pub usize);

impl FrameDecoder for Silence {
    fn decode(&mut self, _: &[u8], _: usize, out: &mut Vec<i16>) -> bool {
        out.resize(out.len() + FRAME_SAMPLES * self.0, 0);
        true
    }
}

pub fn silence() -> DecoderFactory {
    Arc::new(|channels, _| Box::new(Silence(channels)))
}

// Contexts.

/// A context's words in title memory, read and written as atomics (the title writes them too).
#[derive(Clone, Copy)]
struct Context(*const AtomicU32);

// SAFETY: points into the context array, which lives as long as the engine.
unsafe impl Send for Context {}

impl Context {
    fn word(self, i: usize) -> &'static AtomicU32 {
        // SAFETY: i < 16, inside the context.
        unsafe { &*self.0.add(i) }
    }

    fn get(self, i: usize) -> u32 {
        self.word(i).load(Ordering::Acquire)
    }

    fn field(self, i: usize, low: u32, high: u32) -> u32 {
        (self.get(i) >> low) & ((1u32 << (high - low)) - 1)
    }

    fn set(self, i: usize, low: u32, high: u32, value: u32) {
        let mask = (((1u64 << (high - low)) - 1) as u32) << low;
        let word = self.word(i);
        let mut w = word.load(Ordering::Acquire);
        while let Err(now) =
            word.compare_exchange_weak(w, (w & !mask) | ((value << low) & mask), Ordering::AcqRel, Ordering::Acquire)
        {
            w = now;
        }
    }

    fn channels(self) -> usize {
        1 + self.field(1, 29, 30) as usize
    }
}

struct Voice {
    decoder: Option<Box<dyn FrameDecoder>>,
    pcm: VecDeque<i16>,
    /// Samples per channel still to drop.
    skip: usize,
    ended: bool,
    frame: Vec<i16>,
}

struct Shared {
    heap: Arc<ApuHeap>,
    array: usize,
    voices: Box<[Mutex<Option<Voice>>]>,
    factory: DecoderFactory,
    stop: AtomicBool,
    frames: AtomicU64,
    bad_frames: AtomicU64,
    blocks: AtomicU64,
}

/// The XMA contexts of one SHAPE allocation, and the thread that fills their output rings.
pub struct Engine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// How often the decoder thread looks without being woken. A 256-byte block lasts 2.7 ms at 48 kHz.
const IDLE: Duration = Duration::from_millis(2);

impl Engine {
    /// `array` is the host address of `count` contexts, 64 bytes each.
    pub fn new(heap: Arc<ApuHeap>, array: *mut u8, count: usize, factory: DecoderFactory) -> Engine {
        let shared = Arc::new(Shared {
            heap,
            array: array as usize,
            voices: (0..count).map(|_| Mutex::new(None)).collect(),
            factory,
            stop: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            bad_frames: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rebuild XMA".into())
            .spawn(move || {
                crate::raise_priority();
                while !worker.stop.load(Ordering::Acquire) {
                    worker.pump_all();
                    std::thread::park_timeout(IDLE);
                }
            })
            .expect("start the XMA thread");
        Engine {
            shared,
            thread: Some(thread),
        }
    }

    fn wake(&self) {
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }

    pub fn enable(&self, index: u32) {
        if self.shared.enable(index) {
            self.wake();
        }
    }

    pub fn disable(&self, index: u32) {
        if let Some(slot) = self.shared.voices.get(index as usize) {
            *lock(slot) = None;
        }
    }

    /// Commands 6 and 8: one bit per context, 32 to a word.
    pub fn enable_mask(&self, word: impl Fn(usize) -> u32, on: bool) {
        for i in 0..self.shared.voices.len() {
            if word(i / 32) & (1 << (i % 32)) != 0 {
                if on {
                    self.shared.enable(i as u32);
                } else {
                    self.disable(i as u32);
                }
            }
        }
        self.wake();
    }

    /// The title took `blocks` blocks: its read block moves on and the ring has room again.
    pub fn advance_read(&self, index: u32, blocks: u32) {
        let Some(c) = self.shared.context(index) else {
            return;
        };
        // The decoder thread writes the same words.
        let guard = lock(&self.shared.voices[index as usize]);
        let size = c.field(0, 22, 27).max(1);
        c.set(9, 0, 5, (c.field(9, 0, 5) + blocks % size) % size);
        if blocks > 0 {
            c.set(1, 31, 32, 1);
        }
        drop(guard);
        self.wake();
    }

    pub fn is_enabled(&self, index: u32) -> bool {
        self.shared.voices.get(index as usize).is_some_and(|v| lock(v).is_some())
    }

    /// Frames decoded, frames the decoder refused, blocks written.
    pub fn counts(&self) -> (u64, u64, u64) {
        let s = &self.shared;
        (
            s.frames.load(Ordering::Relaxed),
            s.bad_frames.load(Ordering::Relaxed),
            s.blocks.load(Ordering::Relaxed),
        )
    }

    /// Fills every ring now, on the caller's thread (tests).
    pub fn pump_now(&self) {
        self.shared.pump_all();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl Shared {
    fn context(&self, index: u32) -> Option<Context> {
        ((index as usize) < self.voices.len())
            .then(|| Context((self.array + index as usize * CONTEXT_BYTES) as *const AtomicU32))
    }

    fn enable(&self, index: u32) -> bool {
        let Some(c) = self.context(index) else {
            return false;
        };
        c.set(2, 26, 32, 0);
        *lock(&self.voices[index as usize]) = Some(Voice {
            decoder: None,
            pcm: VecDeque::new(),
            skip: c.field(1, 24, 27) as usize * SUBFRAME_SAMPLES,
            ended: false,
            frame: Vec::with_capacity(FRAME_SAMPLES * 2),
        });
        true
    }

    /// Fills every ring as far as it has room. A voice's lock is held for one block at a time (at most
    /// one frame's decoding), so a title command for that voice waits no longer than that.
    fn pump_all(&self) {
        for (i, slot) in self.voices.iter().enumerate() {
            let c = Context((self.array + i * CONTEXT_BYTES) as *const AtomicU32);
            while lock(slot).as_mut().is_some_and(|voice| self.block(c, voice)) {}
        }
    }

    /// Writes one block if the ring has room and there is something to write; true if it did.
    fn block(&self, c: Context, v: &mut Voice) -> bool {
        let size = c.field(0, 22, 27);
        if size == 0 || c.field(1, 31, 32) == 0 {
            return false;
        }
        {
            while v.pcm.len() < BLOCK_SAMPLES && self.decode(c, v) {}
            if v.pcm.is_empty() {
                return false;
            }
            let write = c.field(0, 27, 32);
            let Some(out) = self.heap.span(c.get(7).wrapping_add(write * BLOCK_BYTES as u32), BLOCK_BYTES)
            else {
                return false;
            };
            // SAFETY: one block of the title's output ring, inside a live heap block.
            let out = unsafe { std::slice::from_raw_parts_mut(out.cast::<[u8; 2]>(), BLOCK_SAMPLES) };
            for o in out {
                *o = v.pcm.pop_front().unwrap_or(0).to_le_bytes();
            }
            self.blocks.fetch_add(1, Ordering::Relaxed);
            let write = (write + 1) % size;
            c.set(0, 27, 32, write);
            if write == c.field(9, 0, 5) {
                c.set(1, 31, 32, 0);
            }
        }
        true
    }

    /// Decodes the next frame into `v.pcm`; false when the voice has nothing more.
    fn decode(&self, c: Context, v: &mut Voice) -> bool {
        if v.ended {
            return false;
        }
        let channels = c.channels();
        let valid = c.field(0, 20, 22);
        let mut current = c.field(4, 31, 32);
        if valid & (1 << current) == 0 {
            if valid & (1 << (1 - current)) == 0 {
                v.ended = true;
                c.set(2, 26, 31, STATUS_INPUT_EXHAUSTED);
                c.set(2, 31, 32, 1);
                return false;
            }
            current = 1 - current;
            c.set(4, 31, 32, current);
            c.set(2, 0, 26, 0);
        }
        let (address, packets) = match current {
            0 => (c.get(5), c.field(0, 0, 12)),
            _ => (c.get(6), c.field(1, 0, 12)),
        };
        let len = packets as usize * PACKET_BYTES;
        let Some(buf) = self.heap.span(address & !(PACKET_BYTES as u32 - 1), len) else {
            c.set(0, 20 + current, 21 + current, 0);
            return true;
        };
        // SAFETY: the title's input packets, inside a live heap block.
        let stream = Stream::new(unsafe { std::slice::from_raw_parts(buf, len) });
        let Some(frame) = stream.frame(c.field(2, 0, 26) as usize) else {
            c.set(0, 20 + current, 21 + current, 0);
            return true;
        };
        let decoder = v.decoder.get_or_insert_with(|| {
            (self.factory)(channels, RATES[c.field(1, 27, 29) as usize])
        });
        v.frame.clear();
        self.frames.fetch_add(1, Ordering::Relaxed);
        if !decoder.decode(&frame.data, frame.bits, &mut v.frame) {
            self.bad_frames.fetch_add(1, Ordering::Relaxed);
            v.frame.clear();
        }
        v.frame.resize(FRAME_SAMPLES * channels, 0);
        let mut next = frame.next;
        let loops = c.field(0, 12, 20);
        let loop_end = c.field(4, 0, 26) as usize;
        let ends_loop = loop_end != 0 && frame.start <= loop_end && next.is_none_or(|n| loop_end < n);
        if loops > 0 && ends_loop {
            v.frame.truncate((c.field(1, 12, 14) as usize + 1) * SUBFRAME_SAMPLES * channels);
            next = Some(c.field(3, 0, 26) as usize);
            v.skip = c.field(1, 17, 20) as usize * SUBFRAME_SAMPLES;
            if loops != 255 {
                c.set(0, 12, 20, loops - 1);
            }
        }
        let skip = (v.skip * channels).min(v.frame.len());
        v.skip -= skip / channels;
        v.pcm.extend(&v.frame[skip..]);
        match next {
            Some(n) => c.set(2, 0, 26, n as u32),
            None => {
                c.set(0, 20 + current, 21 + current, 0);
                c.set(2, 0, 26, 0);
            }
        }
        true
    }
}

/// Builds packets the way the title's encoder would lay them out (tests and benchmarks): frames of
/// the given bit lengths, one stream, no skips. Each frame is its length field, a 16-bit tag, then ones.
pub fn synthetic_stream(frames: &[(usize, u16)]) -> Vec<u8> {
    let payload = PACKET_BITS - HEADER_BITS;
    let total: usize = frames.iter().map(|f| f.0).sum();
    let packets = total.div_ceil(payload).max(1);
    let mut buf = vec![0xffu8; packets * PACKET_BYTES];
    let mut put = |pos: usize, value: u32, width: usize| {
        for i in 0..width {
            let p = pos + i;
            let bit = ((value >> (width - 1 - i)) & 1) as u8;
            buf[p / 8] = (buf[p / 8] & !(0x80 >> (p % 8))) | (bit << (7 - p % 8));
        }
    };
    // Stream offset (headers excluded) -> absolute bit.
    let abs = |s: usize| (s / payload) * PACKET_BITS + HEADER_BITS + s % payload;
    let mut starts = vec![0u32; packets];
    let mut first = vec![None; packets];
    let mut s = 0;
    for &(bits, tag) in frames {
        assert!(bits >= LENGTH_BITS + 16 && bits < 0x7fff);
        let p = s / payload;
        starts[p] += 1;
        first[p].get_or_insert(s % payload);
        for (i, (v, w)) in [(bits as u32, LENGTH_BITS), (u32::from(tag), 16)].into_iter().enumerate() {
            let at = s + if i == 0 { 0 } else { LENGTH_BITS };
            for k in 0..w {
                put(abs(at + k), (v >> (w - 1 - k)) & 1, 1);
            }
        }
        s += bits;
    }
    for p in 0..packets {
        let h = PacketHeader {
            frames: starts[p],
            first_frame_bit: first[p].unwrap_or(0) as u32,
            metadata: 0,
            skip: 0,
        };
        put(p * PACKET_BITS, h.word(), 32);
    }
    buf
}

/// Reads the tag `synthetic_stream` wrote: every sample of the frame is the tag.
pub struct TagDecoder(pub usize);

impl FrameDecoder for TagDecoder {
    fn decode(&mut self, frame: &[u8], bits: usize, out: &mut Vec<i16>) -> bool {
        if bits < LENGTH_BITS + 16 {
            return false;
        }
        let word = u32::from_be_bytes([frame[1], frame[2], frame[3], 0]);
        let tag = ((word >> 9) & 0xffff) as u16 as i16;
        out.resize(out.len() + FRAME_SAMPLES * self.0, tag);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a3_frame_walk_finds_every_frame_across_packets() {
        // Lengths chosen so frames straddle packets, including a length field split by a header.
        let lens: Vec<(usize, u16)> =
            (0..40).map(|i| (3000 + (i * 997) % 5000, i as u16 + 1)).collect();
        let buf = synthetic_stream(&lens);
        let s = Stream::new(&buf);
        let mut at = 0;
        let mut seen = Vec::new();
        loop {
            let f = s.frame(at).expect("frame");
            let mut out = Vec::new();
            assert!(TagDecoder(1).decode(&f.data, f.bits, &mut out));
            seen.push((f.bits, out[0] as u16));
            match f.next {
                Some(n) => at = n,
                None => break,
            }
        }
        assert_eq!(seen, lens);
    }

    #[test]
    fn a3_padding_and_garbage_end_the_walk() {
        assert!(Stream::new(&[0xff; PACKET_BYTES]).frame(0).is_none());
        assert!(Stream::new(&[]).frame(0).is_none());
        assert!(Stream::new(&[0; 7]).frame(0).is_none());
    }
}
