//! The engine's audio container (`ADAT`), as far as A5's tests need it: find each stream's format and
//! data chunks. Read at test time from the title's archive; nothing from it is kept.
//!
//! ```text
//! "ADAT", u32 version (low 16) | flags (high 16), u32 streams, u32 header bytes,
//! [flags & 1] u16 first chunk per stream, u32 per stream (id), then a table of u64 chunks:
//! tag (8 bits) | size (28) | offset (28), offsets from the start of the container.
//! ```
//! Format chunk (tag 0x76, 32 bytes): u32 samples, i32 loop, u16 rate, ..., u8 codec at 28 (5 = XMA2,
//! 11 = the title's Opus framing), u8 channels at 29. Data chunk: tag 0x55.

pub const TAG_DATA: u8 = 0x55;
pub const TAG_FORMAT: u8 = 0x76;
pub const CODEC_XMA2: u8 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub samples: u32,
    pub rate: u32,
    pub codec: u8,
    pub channels: u8,
    /// The data chunk, from the start of the container.
    pub data: std::ops::Range<usize>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The streams of the container starting at `b[0]` (`b` may run on past it).
pub fn streams(b: &[u8]) -> Vec<Stream> {
    let mut out = Vec::new();
    if b.get(..4) != Some(b"ADAT") {
        return out;
    }
    let (Some(ver), Some(n), Some(header)) = (u32_at(b, 4), u32_at(b, 8), u32_at(b, 12)) else {
        return out;
    };
    let (n, header) = (n as usize, header as usize);
    if n == 0 || n > 4096 || header > b.len() || header > 1 << 20 {
        return out;
    }
    // Then n + 1 more u32 (meaning unknown), then the chunk table, which ends the header. If that
    // does not parse, the first place a table parses cleanly.
    let ids_end = 16 + if (ver >> 16) & 1 != 0 { 2 * n } else { 0 } + 4 * n;
    let first = ids_end + 4 * (n + 1);
    let chunk = |v: u64| ((v >> 56) as u8, ((v >> 28) & 0xFFF_FFFF) as usize, (v & 0xFFF_FFFF) as usize);
    let table = std::iter::once(first).chain((ids_end..header).step_by(2)).filter(|&p| p < header && (header - p) % 8 == 0).find_map(|p| {
        let entries: Vec<_> = (p..header)
            .step_by(8)
            .map(|q| chunk(u64::from_le_bytes(b[q..q + 8].try_into().unwrap())))
            .collect();
        (!entries.is_empty() && entries.iter().all(|&(_, size, at)| at >= header && at + size <= 1 << 28)).then_some(entries)
    });
    let Some(table) = table else {
        return out;
    };
    // Each stream's chunks run format .. data in table order; pair each format with the next data chunk.
    let mut format: Option<&[u8]> = None;
    let mut pending_data: Option<std::ops::Range<usize>> = None;
    let flush = |f: Option<&[u8]>, d: &mut Option<std::ops::Range<usize>>, out: &mut Vec<Stream>| {
        if let (Some(f), Some(range)) = (f, d.take()) {
            out.push(Stream {
                samples: u32::from_le_bytes(f[0..4].try_into().unwrap()),
                rate: u32::from(u16::from_le_bytes([f[8], f[9]])),
                codec: f[28],
                channels: f[29],
                data: range,
            });
        }
    };
    for &(tag, size, at) in &table {
        match tag {
            TAG_FORMAT if size == 32 && at + 32 <= b.len() => format = Some(&b[at..at + 32]),
            TAG_DATA => pending_data = Some(at..at + size),
            _ => {}
        }
        // A stream has four chunks in the title's containers; close a stream once both are seen.
        if format.is_some() && pending_data.is_some() {
            flush(format.take(), &mut pending_data, &mut out);
        }
    }
    out
}
