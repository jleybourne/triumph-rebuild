//! D5: BCPack, our own decoder (plan decision 8).
//!
//! BCPack is the console's lossless repacking of BC1/3/4/5/6H/7 block streams, unpacked by its DirectStorage
//! hardware on the way to memory. A stream has no header: 12 or 16 interleaved rANS lanes ([`lanes`]) give one
//! record of symbols per block; each record holds two back references and the residuals of the block's
//! endpoints and indices against them ([`blocks`]). The output is ordinary BCn blocks, still in the console's
//! tiled layout (detiling is `crates/xg`'s or the GPU's).
//!
//! The ~78 KB of constant tables ([`Tables`]) are part of the format and can't be derived; they are never in
//! this repository, and are read at run time from the file named by `dstorage.bcpack_tables` /
//! `REBUILD_BCPACK_TABLES`. Without them BCPack requests stay refused (`E_NOTIMPL`), with one log line.
//!
//! Each texture is decoded once: [`Decoder`] keeps the output in an on-disk cache ([`crate::cache`]) keyed by a
//! hash of the packed bytes, and later loads read it back.

mod blocks;
mod consts;
mod lanes;
#[doc(hidden)]
pub mod synth;
mod tables;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub use tables::{Tables, TABLES_SIZE};

use crate::cache::Cache;
use blocks::{Bc6Sink, Bc7Sink, PlainSink, BC1, BC3, BC4, BC5};
use consts::{BC1_LANES, BC3_LANES, BC4_LANES, BC5_LANES, BC6_LANES, BC7_LANES};
use lanes::{run, Feed, Sink, Slice};

/// Why a stream or a table file was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The table file is not [`TABLES_SIZE`] bytes.
    TablesSize(usize),
    /// The table file is malformed.
    Tables(&'static str),
    /// Not a BC number BCPack carries (1, 3, 4, 5, 6, 7).
    Format(u8),
    /// The destination is empty or not a whole number of blocks.
    Destination,
    /// The stream is corrupt or ends early.
    Stream(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TablesSize(n) => write!(f, "BCPack tables: {n} bytes, expected {TABLES_SIZE}"),
            Error::Tables(why) => write!(f, "BCPack tables: {why}"),
            Error::Format(bc) => write!(f, "BCPack: BC{bc} is not a packed format"),
            Error::Destination => write!(f, "BCPack: the destination is not a whole number of blocks"),
            Error::Stream(why) => write!(f, "BCPack: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// Bytes per block of BC number `bc`, if BCPack carries it.
pub fn block_size(bc: u8) -> Option<usize> {
    match bc {
        1 | 4 => Some(8),
        3 | 5 | 6 | 7 => Some(16),
        _ => None,
    }
}

/// Runs the lanes of format `bc` with the given feed and sink.
fn dispatch<F: Feed, S: Sink>(bc: u8, t: &Tables, blocks: usize, feed: &mut F, sink: &mut S) -> Result<(), Error> {
    match bc {
        1 => run::<12, 1, F, S>(t, &BC1_LANES, blocks, feed, sink),
        3 => run::<12, 2, F, S>(t, &BC3_LANES, blocks, feed, sink),
        4 => run::<12, 1, F, S>(t, &BC4_LANES, blocks, feed, sink),
        5 => run::<12, 2, F, S>(t, &BC5_LANES, blocks, feed, sink),
        6 => run::<16, 2, F, S>(t, &BC6_LANES, blocks, feed, sink),
        7 => run::<16, 2, F, S>(t, &BC7_LANES, blocks, feed, sink),
        _ => Err(Error::Format(bc)),
    }
}

/// Which lane machine runs: the vector one where the CPU has it, or the scalar one.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lanes {
    Best,
    Scalar,
}

/// Whether [`decode`] uses the vector lanes on this CPU (AVX2 and POPCNT).
pub fn vector_lanes() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        lanes::avx2::available()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

thread_local! {
    /// The stream with 3 spare bytes after it, for the vector path's gathers.
    static PADDED: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs the lanes of one format: the vector path where the CPU has it and `path` allows, else the scalar one.
fn lanes_for<const N: usize, const PER: usize, S: Sink>(
    t: &Tables,
    widths: &[u8; N],
    blocks: usize,
    src: &[u8],
    path: Lanes,
    sink: &mut S,
) -> Result<(), Error> {
    #[cfg(target_arch = "x86_64")]
    if path == Lanes::Best && lanes::avx2::available() {
        return PADDED.with_borrow_mut(|buf| {
            buf.clear();
            buf.extend_from_slice(src);
            buf.extend_from_slice(&[0; 3]);
            // SAFETY: AVX2 and POPCNT are present, and `buf` holds 3 bytes past the stream.
            unsafe { lanes::avx2::run::<N, PER, S>(t, widths, blocks, buf, src.len(), sink) }
        });
    }
    let _ = path;
    run::<N, PER, Slice, S>(t, widths, blocks, &mut Slice::new(src), sink)
}

/// Unpacks `src` (BC number `bc`: 1, 3-7) into `dst`, whose length gives the block count. The stream carries
/// no size: bytes after the last one the lanes need are ignored; a stream that runs out or is corrupt is an
/// error (never a panic), and `dst` is then partly written.
pub fn decode(bc: u8, src: &[u8], dst: &mut [u8], t: &Tables) -> Result<(), Error> {
    decode_with(Lanes::Best, bc, src, dst, t)
}

/// [`decode`] through a chosen lane machine (tests compare them).
#[doc(hidden)]
pub fn decode_with(path: Lanes, bc: u8, src: &[u8], dst: &mut [u8], t: &Tables) -> Result<(), Error> {
    let block = block_size(bc).ok_or(Error::Format(bc))?;
    if dst.is_empty() || dst.len() % block != 0 {
        return Err(Error::Destination);
    }
    let blocks = dst.len() / block;
    match bc {
        1 => lanes_for::<12, 1, _>(t, &BC1_LANES, blocks, src, path, &mut PlainSink::<BC1>::new(t, dst)),
        3 => lanes_for::<12, 2, _>(t, &BC3_LANES, blocks, src, path, &mut PlainSink::<BC3>::new(t, dst)),
        4 => lanes_for::<12, 1, _>(t, &BC4_LANES, blocks, src, path, &mut PlainSink::<BC4>::new(t, dst)),
        5 => lanes_for::<12, 2, _>(t, &BC5_LANES, blocks, src, path, &mut PlainSink::<BC5>::new(t, dst)),
        6 => lanes_for::<16, 2, _>(t, &BC6_LANES, blocks, src, path, &mut Bc6Sink::new(t, dst)),
        _ => lanes_for::<16, 2, _>(t, &BC7_LANES, blocks, src, path, &mut Bc7Sink::new(t, dst)),
    }
}

/// The table file's path: `configured` (the `dstorage.bcpack_tables` setting) if set, else
/// `REBUILD_BCPACK_TABLES`.
pub fn tables_path(configured: Option<&Path>) -> Option<PathBuf> {
    match configured {
        Some(p) if !p.as_os_str().is_empty() => Some(p.to_path_buf()),
        _ => std::env::var_os("REBUILD_BCPACK_TABLES").filter(|v| !v.is_empty()).map(PathBuf::from),
    }
}

/// Reads and parses the table file. A missing, unreadable, wrong-sized or malformed file is an error message
/// (one line), never a panic.
pub fn load_tables(path: Option<&Path>) -> Result<Tables, String> {
    let path = path.ok_or_else(|| "no BCPack tables (set dstorage.bcpack_tables or REBUILD_BCPACK_TABLES)".to_string())?;
    let bytes = std::fs::read(path).map_err(|e| format!("BCPack tables {}: {e}", path.display()))?;
    Tables::parse(&bytes).map_err(|e| format!("{e} ({})", path.display()))
}

/// Counters of a [`Decoder`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DecoderStats {
    /// Requests decoded (cache misses and uncached requests).
    pub decoded: u64,
    /// Requests served from the cache.
    pub cache_hits: u64,
    /// Requests refused: corrupt stream, unsupported format, bad destination.
    pub failed: u64,
    /// Output bytes decoded, and nanoseconds spent decoding them.
    pub decoded_bytes: u64,
    pub decode_ns: u64,
    /// Output bytes read from the cache, and nanoseconds spent on hits (hash, read, check).
    pub hit_bytes: u64,
    pub hit_ns: u64,
}

/// The engine's BCPack decoder: our decoder behind the cache.
pub struct Decoder {
    tables: Arc<Tables>,
    cache: Option<Cache>,
    decoded: AtomicU64,
    hits: AtomicU64,
    failed: AtomicU64,
    decoded_bytes: AtomicU64,
    decode_ns: AtomicU64,
    hit_bytes: AtomicU64,
    hit_ns: AtomicU64,
}

impl Decoder {
    pub fn new(tables: Arc<Tables>, cache: Option<Cache>) -> Self {
        Self {
            tables,
            cache,
            decoded: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            decoded_bytes: AtomicU64::new(0),
            decode_ns: AtomicU64::new(0),
            hit_bytes: AtomicU64::new(0),
            hit_ns: AtomicU64::new(0),
        }
    }

    /// The decoder the engine gets at start: tables from `tables` (the setting) or `REBUILD_BCPACK_TABLES`,
    /// the cache from [`Cache::from_env`]. Without usable tables: one log line, and `None` (BCPack stays
    /// refused).
    pub fn from_config(tables: Option<&Path>) -> Option<Arc<Decoder>> {
        let path = tables_path(tables);
        match load_tables(path.as_deref()) {
            Ok(t) => Some(Arc::new(Decoder::new(Arc::new(t), Cache::from_env()))),
            Err(why) => {
                eprintln!("dstorage: {why}; BCPack requests will be refused");
                None
            }
        }
    }

    pub fn tables(&self) -> &Tables {
        &self.tables
    }

    pub fn cache(&self) -> Option<&Cache> {
        self.cache.as_ref()
    }

    pub fn stats(&self) -> DecoderStats {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        DecoderStats {
            decoded: l(&self.decoded),
            cache_hits: l(&self.hits),
            failed: l(&self.failed),
            decoded_bytes: l(&self.decoded_bytes),
            decode_ns: l(&self.decode_ns),
            hit_bytes: l(&self.hit_bytes),
            hit_ns: l(&self.hit_ns),
        }
    }

    /// Decodes `src` into `out`, through the cache: a hit reads the stored output, a miss decodes and stores it.
    pub fn decode(&self, bc: u8, src: &[u8], out: &mut [u8]) -> Result<(), Error> {
        let started = Instant::now();
        let key = self.cache.as_ref().filter(|c| c.wants(out.len())).map(|c| (c, c.key(&self.tables, bc, src, out.len())));
        if let Some((cache, key)) = &key {
            if cache.get(key, out) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.hit_bytes.fetch_add(out.len() as u64, Ordering::Relaxed);
                self.hit_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                return Ok(());
            }
        }
        let started = Instant::now();
        if let Err(e) = decode(bc, src, out, &self.tables) {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        self.decoded.fetch_add(1, Ordering::Relaxed);
        self.decoded_bytes.fetch_add(out.len() as u64, Ordering::Relaxed);
        self.decode_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if let Some((cache, key)) = &key {
            cache.put(key, out);
        }
        Ok(())
    }
}

impl crate::engine::BcPack for Decoder {
    fn decode(&self, bc: u8, src: &[u8], out: &mut [u8]) -> bool {
        Decoder::decode(self, bc, src, out).is_ok()
    }
}
