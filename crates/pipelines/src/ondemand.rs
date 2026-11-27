//! Programs the library does not hold, translated when the title first creates them (the BVH builder's compute
//! programs live in `xboxbvhbuilder.dll`, not in `shaders.rpf`, so the offline build never sees them).
//!
//! - The title's create call [`OnDemand::offer`]s the container (one hash lookup; the bytes are copied once per
//!   program) and returns at once; a pool worker calls [`OnDemand::get`] when it creates the pipeline.
//! - `get` translates through [`crate::translate`] (lift, HLSL, DXC) and writes the result to the disk cache,
//!   `%LOCALAPPDATA%\triumph-rebuild\pipelines\ondemand\<hash>.od`. The next session reads it there:
//!   the program is lifted again (about a millisecond) and its HLSL compared with the one the cached DXIL was
//!   compiled from, so a translator change can never serve stale DXIL, and DXC runs only when the HLSL changed.
//! - The cache file carries the container too, so a pipeline prewarmed from the last session's record is
//!   created before the title offers it.
//!
//! ```text
//! file  "RBONDEMD" · version u32 · stage u32 · container length u32 · DXIL length u32
//!       · BLAKE3 of (DXC arguments, HLSL) 32 · BLAKE3 of (container, DXIL) 32 · container · DXIL
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rebuild_shader::blake3;

use crate::layout::StageLayout;
use crate::translate::{self, Hlsl};
use crate::Stage;

const MAGIC: &[u8; 8] = b"RBONDEMD";
const VERSION: u32 = 1;
const HEADER: usize = 8 + 4 * 4 + 32 + 32;

/// A translated program, as the library would hold it.
#[derive(Debug)]
pub struct Program {
    pub hash: u64,
    pub stage: Stage,
    pub dxil: Vec<u8>,
    pub layout: StageLayout,
}

enum Slot {
    Offered(Arc<[u8]>),
    Ready(Arc<Program>),
    Failed(String),
}

/// Counters and time per step (summed over the threads that ran them).
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub offered: u64,
    /// Compiled by DXC this session.
    pub translated: u64,
    /// From the disk cache with no DXC call.
    pub from_disk: u64,
    /// On disk, but compiled from other HLSL (the translator changed): compiled again and rewritten.
    pub stale: u64,
    /// Cache files refused (damaged, or another program under the name).
    pub damaged: u64,
    pub failed: u64,
    /// Container parse, native program, wave class.
    pub parse: Duration,
    /// Decode, lift, HLSL.
    pub lift: Duration,
    /// DXC, process start to output read.
    pub dxc: Duration,
    /// Reading and checking cache files, and writing them.
    pub disk: Duration,
}

const OFFERED: usize = 0;
const TRANSLATED: usize = 1;
const FROM_DISK: usize = 2;
const STALE: usize = 3;
const DAMAGED: usize = 4;
const FAILED: usize = 5;
const PARSE_NS: usize = 6;
const LIFT_NS: usize = 7;
const DXC_NS: usize = 8;
const DISK_NS: usize = 9;

/// The programs translated on demand this session.
pub struct OnDemand {
    dir: Option<PathBuf>,
    dxc: OnceLock<Option<PathBuf>>,
    work: PathBuf,
    map: Mutex<HashMap<u64, Slot>>,
    n: [AtomicU64; 10],
}

/// The disk cache's default folder: `<pipelines dir>\ondemand` ([`crate::session::default_dir`]).
pub fn default_dir() -> PathBuf {
    crate::session::default_dir().join("ondemand")
}

fn hlsl_hash(h: &Hlsl) -> [u8; 32] {
    let mut x = blake3::Hasher::new();
    x.update(h.stage.name().as_bytes());
    for a in translate::DXC_ARGS {
        x.update(a.as_bytes());
        x.update(&[0]);
    }
    x.update(h.src.as_bytes());
    *x.finalize().as_bytes()
}

/// A cache file's parts, after its checks.
struct Cached {
    stage: u32,
    hlsl: [u8; 32],
    container: Vec<u8>,
    dxil: Vec<u8>,
}

fn encode(stage: Stage, hlsl: &[u8; 32], container: &[u8], dxil: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER + container.len() + dxil.len());
    v.extend(MAGIC);
    for w in [VERSION, stage as u32, container.len() as u32, dxil.len() as u32] {
        v.extend(w.to_le_bytes());
    }
    v.extend(hlsl);
    let mut x = blake3::Hasher::new();
    x.update(container);
    x.update(dxil);
    v.extend(x.finalize().as_bytes());
    v.extend(container);
    v.extend(dxil);
    v
}

fn decode(b: &[u8]) -> Result<Cached, String> {
    if b.len() < HEADER || &b[..8] != MAGIC {
        return Err("not an on-demand program file".into());
    }
    let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    if u(8) != VERSION {
        return Err(format!("version {} (this build reads {VERSION})", u(8)));
    }
    let (c, d) = (u(16) as usize, u(20) as usize);
    if HEADER + c + d != b.len() {
        return Err("length does not match its header".into());
    }
    if blake3::hash(&b[HEADER..]).as_bytes()[..] != b[56..88] {
        return Err("content hash mismatch".into());
    }
    Ok(Cached { stage: u(12), hlsl: b[24..56].try_into().unwrap(), container: b[HEADER..HEADER + c].to_vec(), dxil: b[HEADER + c..].to_vec() })
}

impl OnDemand {
    /// On-demand translation with its disk cache in `dir` (`None`: no disk cache).
    pub fn new(dir: Option<PathBuf>) -> OnDemand {
        OnDemand {
            dir,
            dxc: OnceLock::new(),
            work: std::env::temp_dir().join(format!("rebuild-ondemand-{}", std::process::id())),
            map: Mutex::default(),
            n: Default::default(),
        }
    }

    /// The same, with `dxc` instead of [`translate::dxc_path`].
    pub fn with_dxc(self, dxc: Option<PathBuf>) -> OnDemand {
        let _ = self.dxc.set(dxc);
        self
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    fn add(&self, i: usize, v: u64) {
        self.n[i].fetch_add(v, Ordering::Relaxed);
    }

    fn time(&self, i: usize, t: Instant) {
        self.add(i, t.elapsed().as_nanos() as u64);
    }

    /// The title created a program the library does not hold: keep its container for a worker. One lookup;
    /// the bytes are copied the first time only. Returns whether this offer was the first.
    pub fn offer(&self, hash: u64, bytes: &[u8]) -> bool {
        let mut m = self.map.lock().unwrap();
        if m.contains_key(&hash) {
            return false;
        }
        let b = translate::container_bytes(bytes).unwrap_or(bytes);
        m.insert(hash, Slot::Offered(b.into()));
        drop(m);
        self.add(OFFERED, 1);
        true
    }

    /// The program, translated (or read from disk) on this thread if it is not ready yet. Called by pool
    /// workers, never by the title's create call.
    pub fn get(&self, hash: u64) -> Result<Arc<Program>, String> {
        let offered = match self.map.lock().unwrap().get(&hash) {
            Some(Slot::Ready(p)) => return Ok(p.clone()),
            Some(Slot::Failed(e)) => return Err(e.clone()),
            Some(Slot::Offered(b)) => Some(b.clone()),
            None => None,
        };
        let r = self.build(hash, offered.as_deref());
        let slot = match &r {
            Ok(p) => Slot::Ready(p.clone()),
            Err(e) => {
                self.add(FAILED, 1);
                Slot::Failed(e.clone())
            }
        };
        self.map.lock().unwrap().insert(hash, slot);
        r
    }

    /// Programs that failed, with the reason.
    pub fn failures(&self) -> Vec<(u64, String)> {
        let mut v: Vec<_> = self.map.lock().unwrap().iter().filter_map(|(h, s)| if let Slot::Failed(e) = s { Some((*h, e.clone())) } else { None }).collect();
        v.sort();
        v
    }

    pub fn stats(&self) -> Stats {
        let n = |i: usize| self.n[i].load(Ordering::Relaxed);
        let d = |i: usize| Duration::from_nanos(n(i));
        Stats {
            offered: n(OFFERED),
            translated: n(TRANSLATED),
            from_disk: n(FROM_DISK),
            stale: n(STALE),
            damaged: n(DAMAGED),
            failed: n(FAILED),
            parse: d(PARSE_NS),
            lift: d(LIFT_NS),
            dxc: d(DXC_NS),
            disk: d(DISK_NS),
        }
    }

    fn file(&self, hash: u64) -> Option<PathBuf> {
        // one name per program: the stage is in the file (the title creates compute programs on demand today)
        self.dir.as_ref().map(|d| d.join(format!("{hash:016x}.od")))
    }

    fn read_cached(&self, hash: u64) -> Option<Cached> {
        let path = self.file(hash)?;
        let t = Instant::now();
        let r = match std::fs::read(&path) {
            Ok(b) => decode(&b).and_then(|c| {
                if translate::program_hash(&c.container) == Some(hash) {
                    Ok(c)
                } else {
                    Err("holds another program".into())
                }
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.time(DISK_NS, t);
                return None;
            }
            Err(e) => Err(e.to_string()),
        };
        self.time(DISK_NS, t);
        match r {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("pipelines: on-demand cache {}: {e}; translating again", path.display());
                self.add(DAMAGED, 1);
                None
            }
        }
    }

    fn write_cached(&self, hash: u64, stage: Stage, hlsl: &[u8; 32], container: &[u8], dxil: &[u8]) {
        let Some(path) = self.file(hash) else { return };
        let t = Instant::now();
        let r = (|| {
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d)?;
            }
            // written beside, then renamed over: a reader never sees half a file
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, encode(stage, hlsl, container, dxil))?;
            std::fs::rename(&tmp, &path)
        })();
        self.time(DISK_NS, t);
        if let Err(e) = r {
            eprintln!("pipelines: on-demand cache {}: {e}", path.display());
        }
    }

    fn build(&self, hash: u64, offered: Option<&[u8]>) -> Result<Arc<Program>, String> {
        let cached = self.read_cached(hash).filter(|c| offered.is_none_or(|o| o == c.container));
        let (from_file, cached) = match cached {
            Some(c) => (Some(c.container), Some((c.stage, c.hlsl, c.dxil))),
            None => (None, None),
        };
        let container: &[u8] = match (offered, &from_file) {
            (Some(o), _) => o,
            (None, Some(c)) => c,
            (None, None) => return Err(format!("cs {hash:016x} is not in the program library, and was neither offered nor translated before")),
        };
        // timed whether they succeed or not: a refusal costs too
        let t = Instant::now();
        let parsed = translate::native_of(container).and_then(|x| if cached.is_none() { translate::check_kind(&x.0).map(|_| x) } else { Ok(x) });
        self.time(PARSE_NS, t);
        let (p, isg1, osg1) = parsed?;
        let t = Instant::now();
        let h = translate::lift_hlsl(&p, &isg1, &osg1, true).map_err(|r| r.reason);
        self.time(LIFT_NS, t);
        let h = h?;
        let hh = hlsl_hash(&h);
        let dxil = match cached {
            Some((stage, hlsl, dxil)) if hlsl == hh && stage == h.stage as u32 => {
                self.add(FROM_DISK, 1);
                dxil
            }
            c => {
                if c.is_some() {
                    self.add(STALE, 1);
                }
                let dxc = self.dxc.get_or_init(translate::dxc_path).as_deref().ok_or("no dxc.exe (Windows SDK, or REBUILD_DXC)")?;
                let t = Instant::now();
                let dxil = translate::compile_hlsl(dxc, &self.work, hash, &h).map_err(|e| format!("dxc: {e}"))?;
                self.time(DXC_NS, t);
                self.add(TRANSLATED, 1);
                self.write_cached(hash, h.stage, &hh, container, &dxil);
                dxil
            }
        };
        let layout = translate::layout_of(&dxil, &h)?;
        Ok(Arc::new(Program { hash, stage: h.stage, dxil, layout }))
    }
}

impl Drop for OnDemand {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.work);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_files_round_trip_and_damage_is_refused() {
        let h = [7u8; 32];
        let b = encode(Stage::Cs, &h, b"container", b"dxil bytes");
        let c = decode(&b).unwrap();
        assert_eq!((c.stage, c.hlsl, &c.container[..], &c.dxil[..]), (Stage::Cs as u32, h, &b"container"[..], &b"dxil bytes"[..]));
        let mut bad = b.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(decode(&bad).err().as_deref(), Some("content hash mismatch"));
        assert!(decode(&b[..b.len() - 1]).is_err());
        assert!(decode(b"RBONDEMD").is_err());
    }

    #[test]
    fn a_program_never_offered_nor_cached_fails_with_its_reason() {
        let od = OnDemand::new(None);
        let e = od.get(0x1234).unwrap_err();
        assert!(e.contains("not in the program library"), "{e}");
        assert_eq!(od.failures().len(), 1);
        assert!(od.offer(1, b"junk") && !od.offer(1, b"junk"), "one copy per program");
        assert!(od.get(1).unwrap_err().contains("DXBC"));
        let s = od.stats();
        assert_eq!((s.offered, s.failed, s.translated), (1, 2, 0));
    }
}
