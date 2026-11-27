//! D3: zlib (RFC 1950) inflate for requests with option bit 0, through `zlib-rs` (Zlib licence).
//!
//! One inflate state per worker thread, reset per request: a request is one whole stream, inflated
//! straight into its destination in a single `Finish` call (no intermediate copy).

use std::cell::RefCell;

use zlib_rs::{Inflate, InflateFlush, Status};

/// Why a stream did not inflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    /// Not a zlib stream, a bad checksum, or a corrupt block.
    Corrupt,
    /// The stream needs more room than the destination has, or the source ends before the stream.
    Short,
}

thread_local! {
    static STATE: RefCell<Inflate> = RefCell::new(Inflate::new(true, 15));
}

/// Inflates the whole zlib stream `src` into `out`; returns the bytes written. A stream that ends
/// before `out` is full is accepted; the bytes of `out` past the stream's end are then unspecified
/// (zlib-rs may use them while copying), which stays inside the request's destination.
pub fn inflate(src: &[u8], out: &mut [u8]) -> Result<usize, InflateError> {
    STATE.with_borrow_mut(|state| {
        state.reset(true);
        let mut one = [0u8; 1];
        // An empty destination still has to see the stream end, so it gets a byte it may not use.
        let dst: &mut [u8] = if out.is_empty() { &mut one } else { out };
        let status = state.decompress(src, dst, InflateFlush::Finish);
        let written = state.total_out() as usize;
        match status {
            Ok(Status::StreamEnd) if out.is_empty() && written > 0 => Err(InflateError::Short),
            Ok(Status::StreamEnd) => Ok(written),
            Ok(_) => Err(InflateError::Short),
            Err(_) => Err(InflateError::Corrupt),
        }
    })
}

/// Deflates `src` as one zlib stream (tests and the benchmark make their synthetic data with it).
pub fn deflate(src: &[u8], level: i32) -> Vec<u8> {
    let mut state = zlib_rs::Deflate::new(level, true, 15);
    // compressBound for zlib: n + n/4096*... ; a generous bound is enough here.
    let mut out = vec![0u8; src.len() + src.len() / 8 + 64];
    match state.compress(src, &mut out, zlib_rs::DeflateFlush::Finish) {
        Ok(Status::StreamEnd) => {}
        other => panic!("deflate did not finish: {other:?}"),
    }
    out.truncate(state.total_out() as usize);
    out
}
