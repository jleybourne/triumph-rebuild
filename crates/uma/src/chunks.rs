//! What the CPU changed, in 64-byte chunks.
//!
//! Write-watch names the 4 KiB pages the CPU wrote. A written page is not a changed page: the
//! title rewrites values it already had, and the rest of the page may hold bytes the GPU side
//! owns. So each written page is compared 64 bytes at a time against what was last sent, and only
//! the chunks that differ go up. Adjacent changed chunks are joined into runs, so a run is one
//! copy on the GPU side.
//!
//! Two ways to know what was last sent, and Build 0 measures both:
//!
//! ```text
//! shadow   a full copy of the region as sent    compare is exact, memory is 100 % of the region
//! hash     one u64 per 64-byte chunk            memory is 12.5 % of the region, a hash per chunk
//! ```

pub const CHUNK: usize = 64;
pub const OS_PAGE: usize = 4096;
pub const CHUNKS_PER_PAGE: usize = OS_PAGE / CHUNK;

/// A changed byte range, relative to the start of the watched region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub offset: usize,
    pub len: usize,
}

/// Append a chunk to `out`, joining it to the last run when they touch.
#[inline]
fn push(out: &mut Vec<Run>, offset: usize, len: usize) {
    if let Some(last) = out.last_mut() {
        if last.offset + last.len == offset {
            last.len += len;
            return;
        }
    }
    out.push(Run { offset, len });
}

#[inline]
fn words(c: &[u8]) -> [u64; 8] {
    debug_assert_eq!(c.len(), CHUNK);
    let mut w = [0u64; 8];
    for (i, word) in w.iter_mut().enumerate() {
        *word = u64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap());
    }
    w
}

/// A 64-bit hash of one chunk: four independent multiply lanes, folded at the end, so the
/// multiplies do not wait on each other.
#[inline]
pub fn chunk_hash(c: &[u8]) -> u64 {
    const M: [u64; 4] = [
        0x9E37_79B9_7F4A_7C15,
        0xC2B2_AE3D_27D4_EB4F,
        0x1656_67B1_9E37_79F9,
        0x85EB_CA77_C2B2_AE63,
    ];
    let w = words(c);
    let mut lane = [0u64; 4];
    for i in 0..4 {
        let a = (w[i] ^ M[i]).wrapping_mul(M[(i + 1) & 3]);
        let b = (w[i + 4] ^ M[(i + 2) & 3]).wrapping_mul(M[(i + 3) & 3]);
        lane[i] = (a ^ b.rotate_left(31)).wrapping_mul(M[i]);
    }
    let h = lane[0] ^ lane[1].rotate_left(17) ^ lane[2].rotate_left(34) ^ lane[3].rotate_left(51);
    h ^ (h >> 29)
}

/// The hash of an all-zero chunk: what a never-written region was "sent" as.
pub fn zero_hash() -> u64 {
    chunk_hash(&[0u8; CHUNK])
}

/// Compare one written page against its shadow; append changed runs and bring the shadow up to
/// date. `page` and `shadow` are the same 4 KiB of the region, at `offset`.
pub fn diff_shadow(page: &[u8], shadow: &mut [u8], offset: usize, out: &mut Vec<Run>) {
    debug_assert_eq!(page.len(), OS_PAGE);
    debug_assert_eq!(shadow.len(), OS_PAGE);
    for k in 0..CHUNKS_PER_PAGE {
        let r = k * CHUNK..k * CHUNK + CHUNK;
        if page[r.clone()] != shadow[r.clone()] {
            shadow[r.clone()].copy_from_slice(&page[r]);
            push(out, offset + k * CHUNK, CHUNK);
        }
    }
}

/// Compare one written page against the hashes of what was sent; append changed runs and bring
/// the hashes up to date. `sent` holds the page's 64 chunk hashes.
pub fn diff_hash(page: &[u8], sent: &mut [u64], offset: usize, out: &mut Vec<Run>) {
    debug_assert_eq!(page.len(), OS_PAGE);
    debug_assert_eq!(sent.len(), CHUNKS_PER_PAGE);
    for (k, c) in page.chunks_exact(CHUNK).enumerate() {
        let h = chunk_hash(c);
        if h != sent[k] {
            sent[k] = h;
            push(out, offset + k * CHUNK, CHUNK);
        }
    }
}

std::thread_local! {
    static SUBMIT_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the calling thread as the submit thread: from now on, copying title memory on it is a bug,
/// and it panics if it happens (one thread-local read per gather; kept in every build).
pub fn mark_submit_thread() {
    SUBMIT_THREAD.with(|f| f.set(true));
}

pub fn is_submit_thread() -> bool {
    SUBMIT_THREAD.with(|f| f.get())
}

/// Copy every run's bytes out of `region` into `dst`, packed back to back. Returns the bytes
/// written. `dst` must hold at least the sum of the run lengths. Never on the submit thread.
pub fn gather(region: &[u8], runs: &[Run], dst: &mut [u8]) -> usize {
    assert!(!is_submit_thread(), "title memory copied on the submit thread");
    let mut at = 0;
    for run in runs {
        dst[at..at + run.len].copy_from_slice(&region[run.offset..run.offset + run.len]);
        at += run.len;
    }
    at
}

/// The chunk index (offset / 64) of every chunk in `runs`, in packed order: what a scatter
/// shader needs to put packed chunk `i` back at its address.
pub fn chunk_indices(runs: &[Run], dst: &mut [u32]) -> usize {
    let mut at = 0;
    for run in runs {
        let first = (run.offset / CHUNK) as u32;
        for i in 0..(run.len / CHUNK) as u32 {
            dst[at] = first + i;
            at += 1;
        }
    }
    at
}

pub fn bytes(runs: &[Run]) -> usize {
    runs.iter().map(|r| r.len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Vec<u8> {
        vec![0u8; OS_PAGE]
    }

    #[test]
    fn an_unchanged_page_sends_nothing() {
        let p = page();
        let mut shadow = page();
        let mut sent = vec![zero_hash(); CHUNKS_PER_PAGE];
        let mut out = Vec::new();
        diff_shadow(&p, &mut shadow, 0, &mut out);
        diff_hash(&p, &mut sent, 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn one_changed_byte_sends_its_chunk_only() {
        for diff in [0, 1] {
            let mut p = page();
            p[130] = 7;
            let mut out = Vec::new();
            if diff == 0 {
                diff_shadow(&p, &mut page(), 8192, &mut out);
            } else {
                diff_hash(&p, &mut vec![zero_hash(); CHUNKS_PER_PAGE], 8192, &mut out);
            }
            assert_eq!(out, vec![Run { offset: 8192 + 128, len: 64 }]);
        }
    }

    #[test]
    fn adjacent_chunks_join_into_one_run() {
        let mut p = page();
        p[0] = 1;
        p[64] = 1;
        p[200] = 1;
        let mut out = Vec::new();
        diff_shadow(&p, &mut page(), 0, &mut out);
        assert_eq!(
            out,
            vec![Run { offset: 0, len: 128 }, Run { offset: 192, len: 64 }]
        );
    }

    #[test]
    fn runs_join_across_a_page_boundary() {
        let mut a = page();
        let mut b = page();
        a[OS_PAGE - 1] = 1;
        b[0] = 1;
        let mut out = Vec::new();
        diff_shadow(&a, &mut page(), 0, &mut out);
        diff_shadow(&b, &mut page(), OS_PAGE, &mut out);
        assert_eq!(out, vec![Run { offset: OS_PAGE - 64, len: 128 }]);
    }

    #[test]
    fn a_sent_change_is_not_sent_twice() {
        let mut p = page();
        p[5] = 9;
        let mut shadow = page();
        let mut sent = vec![zero_hash(); CHUNKS_PER_PAGE];
        let mut out = Vec::new();
        diff_shadow(&p, &mut shadow, 0, &mut out);
        diff_hash(&p, &mut sent, 0, &mut out);
        out.clear();
        diff_shadow(&p, &mut shadow, 0, &mut out);
        diff_hash(&p, &mut sent, 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn gather_packs_and_indices_name_each_chunk() {
        let mut region = vec![0u8; 2 * OS_PAGE];
        region[64..128].fill(1);
        region[OS_PAGE..OS_PAGE + 128].fill(2);
        let runs = [Run { offset: 64, len: 64 }, Run { offset: OS_PAGE, len: 128 }];
        let mut packed = vec![0u8; 192];
        assert_eq!(gather(&region, &runs, &mut packed), 192);
        assert!(packed[..64].iter().all(|b| *b == 1));
        assert!(packed[64..].iter().all(|b| *b == 2));
        let mut idx = vec![0u32; 3];
        assert_eq!(chunk_indices(&runs, &mut idx), 3);
        assert_eq!(idx, vec![1, 64, 65]);
    }

    #[test]
    #[should_panic(expected = "title memory copied on the submit thread")]
    fn the_submit_thread_never_copies_title_memory() {
        mark_submit_thread();
        let region = vec![0u8; 128];
        gather(&region, &[Run { offset: 0, len: 64 }], &mut [0u8; 64]);
    }

    #[test]
    fn different_chunks_hash_differently() {
        let mut a = [0u8; CHUNK];
        let mut seen = std::collections::HashSet::new();
        for i in 0..CHUNK {
            a[i] = 1;
            assert!(seen.insert(chunk_hash(&a)));
            a[i] = 0;
        }
        assert!(!seen.contains(&zero_hash()));
    }
}
