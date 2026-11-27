//! BCPack capture: the title's own BCPack requests, saved for checking our decoder against real data (D5b).
//!
//! The owner's choice of approach (2026-11-27): no helper reads the game folder; instead the requests the title
//! actually makes are captured while it runs, outside git, and compared offline against the fork's decoder. A
//! wrapper around the decoder: every request it sees (up to a byte cap) is written as one file
//! `<dir>/<n>-bc<bc>-<out bytes>.bcp` holding the packed input exactly as the decoder received it, after zlib.
//! Off unless `REBUILD_BCPACK_CAPTURE` names a folder; `REBUILD_BCPACK_CAPTURE_MB` caps it (default 512).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::engine::BcPack;

/// The decoder, with capture around it.
pub struct Capture {
    inner: Arc<dyn BcPack>,
    dir: PathBuf,
    cap: u64,
    written: AtomicU64,
    count: AtomicU64,
}

impl Capture {
    pub fn new(inner: Arc<dyn BcPack>, dir: PathBuf, cap_bytes: u64) -> Self {
        Self { inner, dir, cap: cap_bytes, written: AtomicU64::new(0), count: AtomicU64::new(0) }
    }

    /// Wraps `inner` when `REBUILD_BCPACK_CAPTURE` is set (creating the folder); otherwise returns it as it is.
    pub fn from_env(inner: Arc<dyn BcPack>) -> Arc<dyn BcPack> {
        let Some(dir) = std::env::var_os("REBUILD_BCPACK_CAPTURE").map(PathBuf::from) else { return inner };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("dstorage: BCPack capture off ({}: {e})", dir.display());
            return inner;
        }
        let mb = std::env::var("REBUILD_BCPACK_CAPTURE_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(512);
        eprintln!("dstorage: capturing BCPack requests to {} (up to {mb} MiB)", dir.display());
        Arc::new(Capture::new(inner, dir, mb << 20))
    }

    /// Requests saved so far.
    pub fn captured(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn save(&self, bc: u8, src: &[u8], out_len: usize) {
        let n = src.len() as u64;
        // reserve the bytes first, so concurrent workers never pass the cap
        if self.written.fetch_add(n, Ordering::Relaxed) + n > self.cap {
            self.written.fetch_sub(n, Ordering::Relaxed);
            return;
        }
        let k = self.count.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("{k:06}-bc{bc}-{out_len}.bcp"));
        if std::fs::write(&path, src).is_err() {
            self.written.fetch_sub(n, Ordering::Relaxed);
        }
    }
}

impl BcPack for Capture {
    fn decode(&self, bc: u8, src: &[u8], out: &mut [u8]) -> bool {
        self.save(bc, src, out.len());
        self.inner.decode(bc, src, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fill;
    impl BcPack for Fill {
        fn decode(&self, _bc: u8, src: &[u8], out: &mut [u8]) -> bool {
            out.fill(src.first().copied().unwrap_or(0));
            true
        }
    }

    #[test]
    fn requests_are_saved_as_given_and_the_cap_holds() {
        let dir = std::env::temp_dir().join(format!("bcp-capture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = Capture::new(Arc::new(Fill), dir.clone(), 10);
        let mut out = [0u8; 16];
        assert!(c.decode(7, &[9, 8, 7, 6], &mut out));
        assert_eq!(out, [9; 16], "the decoder still runs");
        assert!(c.decode(1, &[1, 2, 3, 4, 5], &mut out));
        assert!(c.decode(3, &[5, 5, 5, 5, 5], &mut out), "past the cap: decoded, not saved");
        assert_eq!(c.captured(), 2);
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["000000-bc7-16.bcp", "000001-bc1-16.bcp"]);
        assert_eq!(std::fs::read(dir.join("000000-bc7-16.bcp")).unwrap(), [9, 8, 7, 6]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
