//! D5c: the BCPack decode cache (plan decision 8): each texture is decoded once, and later loads read the
//! stored output.
//!
//! - **Key:** BLAKE3 over the decoder version, the table file's hash, the BC number, the output size and the
//!   packed bytes; the file name is its first 128 bits in hex. Output is deterministic, so a key names one
//!   output for good.
//! - **Entry:** a 32-byte header (magic, BC number, output size, CRC-32 of the output) and the output. A read
//!   goes straight into the destination and is checked against the size and the CRC; a corrupt or partial entry
//!   is deleted and counted, and the request decodes and stores it again.
//! - **Writes are atomic:** a temporary file in the same folder, renamed over the entry.
//! - **Size cap:** oldest first by last use (a hit refreshes the file's time) once the total passes the cap.
//! - **Small outputs are not cached:** below [`Cache::min_bytes`] decoding is cheaper than opening a file.
//!
//! Location: `REBUILD_BCPACK_CACHE`, default the game's `cache\bcpack` (`rebuild_storage`; content-keyed, so kept
//! across game builds) or the shared cache without a game (outside git);
//! `off` disables it. Cap: `REBUILD_BCPACK_CACHE_MB` (default 8192).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::bcpack::Tables;

/// Bump when the decoder's output changes: old entries then never match.
const DECODER_VERSION: u32 = 1;
const MAGIC: [u8; 8] = *b"RBCPACK1";
const HEADER: usize = 32;
/// The default size cap.
pub const DEFAULT_CAP: u64 = 8 << 30;
/// The default smallest output worth caching. A hit costs about 160 us whatever the size (opening the file);
/// decoding 32 KiB takes 330 us (BC1) to 1,200 us (BC7). Measured by `bcbench`, agents/dstorage.md D5c.
pub const DEFAULT_MIN_BYTES: usize = 32 << 10;

/// Opens an entry to read it and to refresh its time (`FILE_WRITE_ATTRIBUTES` on Windows).
fn open_for_read(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // GENERIC_READ | FILE_WRITE_ATTRIBUTES
        o.access_mode(0x8000_0000 | 0x100);
    }
    o.open(path)
}

/// A cache key: 128 bits of BLAKE3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    hash: [u8; 16],
    bc: u8,
}

impl Key {
    fn name(&self) -> String {
        let mut s = String::with_capacity(40);
        for b in self.hash {
            s.push_str(&format!("{b:02x}"));
        }
        s.push_str(&format!(".bc{}", self.bc));
        s
    }
}

struct Entry {
    size: u64,
    /// Larger is more recent.
    used: u64,
}

struct State {
    entries: HashMap<String, Entry>,
    total: u64,
    clock: u64,
}

/// Counters of a [`Cache`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    /// Entries found corrupt or partial (deleted, then rebuilt by the request).
    pub corrupt: u64,
    pub evicted: u64,
    /// Bytes of output the entries hold now.
    pub bytes: u64,
}

/// The on-disk decode cache. Shared by every worker; no global state.
pub struct Cache {
    dir: PathBuf,
    cap: u64,
    min_bytes: usize,
    state: Mutex<State>,
    temp: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    writes: AtomicU64,
    corrupt: AtomicU64,
    evicted: AtomicU64,
}

impl Cache {
    /// Opens (creating it if needed) the cache in `dir` with a cap of `cap` bytes. Leftover temporary files are
    /// removed; existing entries are indexed by their file times (oldest first).
    pub fn open(dir: &Path, cap: u64) -> std::io::Result<Cache> {
        fs::create_dir_all(dir)?;
        let mut found = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            if name.contains(".tmp") {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            if name.contains(".bc") {
                found.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), name, meta.len().saturating_sub(HEADER as u64)));
            }
        }
        found.sort();
        let mut state = State { entries: HashMap::new(), total: 0, clock: 0 };
        for (_, name, size) in found {
            state.clock += 1;
            state.total += size;
            state.entries.insert(name, Entry { size, used: state.clock });
        }
        let cache = Cache {
            dir: dir.to_path_buf(),
            cap,
            min_bytes: DEFAULT_MIN_BYTES,
            state: Mutex::new(state),
            temp: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            corrupt: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
        };
        cache.evict();
        Ok(cache)
    }

    /// The cache from `REBUILD_BCPACK_CACHE` / `REBUILD_BCPACK_CACHE_MB` (defaults above); `None` when it is
    /// `off` or the folder can't be made (one log line).
    pub fn from_env() -> Option<Cache> {
        let dir = match std::env::var_os("REBUILD_BCPACK_CACHE") {
            Some(v) if v == "off" => return None,
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => rebuild_storage::cache_dir(rebuild_storage::CacheKind::BcPack),
        };
        let cap = std::env::var("REBUILD_BCPACK_CACHE_MB").ok().and_then(|v| v.parse::<u64>().ok()).map_or(DEFAULT_CAP, |mb| mb << 20);
        match Cache::open(&dir, cap) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("dstorage: BCPack cache {}: {e}; decoding without a cache", dir.display());
                None
            }
        }
    }

    /// Sets the smallest output that is cached.
    pub fn with_min_bytes(mut self, min_bytes: usize) -> Self {
        self.min_bytes = min_bytes;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn min_bytes(&self) -> usize {
        self.min_bytes
    }

    /// Whether an output of `len` bytes goes through the cache.
    pub fn wants(&self, len: usize) -> bool {
        len >= self.min_bytes
    }

    pub fn stats(&self) -> CacheStats {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        CacheStats {
            hits: l(&self.hits),
            misses: l(&self.misses),
            writes: l(&self.writes),
            corrupt: l(&self.corrupt),
            evicted: l(&self.evicted),
            bytes: self.lock().total,
        }
    }

    /// The key of `src` decoded as BC `bc` into `out_len` bytes with `tables`.
    pub fn key(&self, tables: &Tables, bc: u8, src: &[u8], out_len: usize) -> Key {
        let mut h = blake3::Hasher::new();
        h.update(&DECODER_VERSION.to_le_bytes());
        h.update(&tables.hash());
        h.update(&[bc]);
        h.update(&(out_len as u64).to_le_bytes());
        h.update(src);
        let mut hash = [0u8; 16];
        hash.copy_from_slice(&h.finalize().as_bytes()[..16]);
        Key { hash, bc }
    }

    /// The path of `key`'s entry.
    pub fn path(&self, key: &Key) -> PathBuf {
        self.dir.join(key.name())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fills `out` from `key`'s entry; false on a miss. A corrupt or partial entry is deleted and is a miss.
    pub fn get(&self, key: &Key, out: &mut [u8]) -> bool {
        let name = key.name();
        let path = self.dir.join(&name);
        let Ok(mut file) = open_for_read(&path) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let mut header = [0u8; HEADER];
        let ok = file.read_exact(&mut header).is_ok()
            && header[..8] == MAGIC
            && header[8] == key.bc
            && u64::from_le_bytes(header[16..24].try_into().unwrap_or_default()) == out.len() as u64
            && file.read_exact(out).is_ok()
            && file.read(&mut [0u8; 1]).ok() == Some(0)
            && crc32fast::hash(out).to_le_bytes() == header[24..28];
        if !ok {
            drop(file);
            self.corrupt.fetch_add(1, Ordering::Relaxed);
            self.misses.fetch_add(1, Ordering::Relaxed);
            let _ = fs::remove_file(&path);
            let mut s = self.lock();
            if let Some(e) = s.entries.remove(&name) {
                s.total -= e.size;
            }
            return false;
        }
        // Last use, in memory now and on disk for the next start.
        let _ = file.set_modified(SystemTime::now());
        let mut s = self.lock();
        s.clock += 1;
        let clock = s.clock;
        if let Some(e) = s.entries.get_mut(&name) {
            e.used = clock;
        } else {
            // Written by another process (or before an index rebuild): adopt it.
            s.total += out.len() as u64;
            s.entries.insert(name, Entry { size: out.len() as u64, used: clock });
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Stores `out` as `key`'s entry: a temporary file renamed over the entry. Failures are not errors (the
    /// output was decoded either way); they leave no partial entry behind.
    pub fn put(&self, key: &Key, out: &[u8]) {
        let name = key.name();
        let path = self.dir.join(&name);
        let temp = self.dir.join(format!("{name}.tmp{}-{}", std::process::id(), self.temp.fetch_add(1, Ordering::Relaxed)));
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(&MAGIC);
        header[8] = key.bc;
        header[16..24].copy_from_slice(&(out.len() as u64).to_le_bytes());
        header[24..28].copy_from_slice(&crc32fast::hash(out).to_le_bytes());
        let written = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&temp)?;
            f.write_all(&header)?;
            f.write_all(out)?;
            drop(f);
            fs::rename(&temp, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
            return;
        }
        self.writes.fetch_add(1, Ordering::Relaxed);
        {
            let mut s = self.lock();
            s.clock += 1;
            let clock = s.clock;
            if let Some(old) = s.entries.insert(name, Entry { size: out.len() as u64, used: clock }) {
                s.total -= old.size;
            }
            s.total += out.len() as u64;
        }
        self.evict();
    }

    /// Removes the least recently used entries until the total is at or below the cap.
    fn evict(&self) {
        let mut s = self.lock();
        if s.total <= self.cap {
            return;
        }
        let mut order: Vec<(u64, String)> = s.entries.iter().map(|(n, e)| (e.used, n.clone())).collect();
        order.sort_unstable();
        for (_, name) in order {
            if s.total <= self.cap {
                break;
            }
            if let Some(e) = s.entries.remove(&name) {
                s.total -= e.size;
                let _ = fs::remove_file(self.dir.join(&name));
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
