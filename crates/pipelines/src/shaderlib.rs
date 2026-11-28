//! The shader library, built on the user's machine from the user's own game (owner decision 2026-11-28): the
//! launcher runs [`build`] on first Play into the game's per-build cache (`GameStore::shaders_dir()`,
//! `docs/design/storage.md`), and the layer loads what it made ([`choose`]). Translated shaders are never
//! shipped: they are derived from the game's own. A program the library lacks is translated on demand at
//! pipeline creation (`crate::ondemand`), so a missing or partial library never blocks a launch.
//!
//! ```text
//! <game dir>\shaders.rpf ─ census walk ─ every VS, PS and CS program ─┬ lift + HLSL (translate::lift_hlsl, v1)
//!   (read only)                                                       ├ key = BLAKE3(stage, DXC args, HLSL, DXC build)
//!                                                                     ├ same key on disk: kept, no DXC
//!                                                                     └ else DXC ─ programs\<hash>.<stage>.rec
//! programs\*.rec ─ layouts (DXIL bindings + sampler words) ─ library.bin ─ manifest.toml (state = "complete")
//! ```
//!
//! - **Resumable.** Every program's outcome is its own record file (written beside, then renamed), and a line
//!   in `journal.txt` once it is on disk. A build stopped anywhere (Cancel, the launcher closed, a crash)
//!   starts again where it stopped: journaled programs whose record is intact are not lifted or compiled
//!   again.
//! - **Versioned.** `manifest.toml` names the translator version ([`VERSION`], a hash of the translator's
//!   sources made by `build.rs`), the game build and the corpus (size and time). Any of them different: the
//!   library is stale and the next build rebuilds it.
//! - **Incremental.** A rebuild lifts every program again (a few seconds for the whole corpus) but sends only
//!   programs whose key changed through DXC: a translator change that alters ten programs' HLSL compiles ten.
//!   A new program (a game patch) compiles alone; records of programs gone from the corpus are deleted.
//! - **Parallel.** `workers` threads (default: logical processors less two), each lifting and running one
//!   DXC process at a time, at below-normal priority.
//!
//! The library holds exactly what `translate v2` + `pipelines build` produce for the same corpus (the same
//! lift, the same DXC arguments, the same layouts): this module adds no translation of its own.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rebuild_shader::blake3;
use rebuild_shader::census;

use crate::layout::{StageLayout, USER_DATA_DWORDS};
use crate::translate::{self, Hlsl};
use crate::{parse_hash, Stage};

/// The translator version this binary was built with (`build.rs`).
pub const VERSION: &str = env!("REBUILD_TRANSLATOR_VERSION");
/// The record, journal and manifest format.
const FORMAT: u32 = 1;
pub const LIBRARY: &str = "library.bin";
pub const MANIFEST: &str = "manifest.toml";
const JOURNAL: &str = "journal.txt";
const PROGRAMS: &str = "programs";
const WORK: &str = "work";
/// The corpus, in the game's folder.
pub const CORPUS: &str = "shaders.rpf";

/// Worker threads by default: logical processors less two (the desktop stays responsive), at least one.
pub fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get()).saturating_sub(2).max(1)
}

// ---------------------------------------------------------------------------------------------------------
// manifest

/// `manifest.toml`: what the library in this folder was built from, and how far the build got.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Manifest {
    pub format: u32,
    pub translator: String,
    pub game_build: String,
    pub corpus_size: u64,
    pub corpus_modified: u64,
    /// "building" or "complete"
    pub state: String,
    /// programs in the corpus the translator is asked for (VS, PS, CS)
    pub programs: u64,
    pub in_library: u64,
    pub refused: u64,
    pub failed: u64,
    pub library_bytes: u64,
    /// the library's content hash (its header), hex
    pub library_identity: String,
    pub dxc: String,
    pub build_seconds: f64,
    pub finished: u64,
}

impl Manifest {
    pub fn complete(&self) -> bool {
        self.state == "complete"
    }

    pub fn to_toml(&self) -> String {
        let mut t = toml::Table::new();
        let int = |v: u64| toml::Value::Integer(v.min(i64::MAX as u64) as i64);
        t.insert("format".into(), int(u64::from(self.format)));
        t.insert("translator".into(), self.translator.clone().into());
        t.insert("game_build".into(), self.game_build.clone().into());
        t.insert("corpus_size".into(), int(self.corpus_size));
        t.insert("corpus_modified".into(), int(self.corpus_modified));
        t.insert("state".into(), self.state.clone().into());
        t.insert("programs".into(), int(self.programs));
        t.insert("in_library".into(), int(self.in_library));
        t.insert("refused".into(), int(self.refused));
        t.insert("failed".into(), int(self.failed));
        t.insert("library_bytes".into(), int(self.library_bytes));
        t.insert("library_identity".into(), self.library_identity.clone().into());
        t.insert("dxc".into(), self.dxc.clone().into());
        t.insert("build_seconds".into(), toml::Value::Float(self.build_seconds));
        t.insert("finished".into(), int(self.finished));
        format!("# the shader library in this folder (rebuild_pipelines::shaderlib); regenerable\n{t}")
    }

    pub fn parse(text: &str) -> Option<Manifest> {
        let t: toml::Table = text.parse().ok()?;
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let n = |k: &str| t.get(k).and_then(|v| v.as_integer()).unwrap_or(0).max(0) as u64;
        Some(Manifest {
            format: n("format") as u32,
            translator: s("translator"),
            game_build: s("game_build"),
            corpus_size: n("corpus_size"),
            corpus_modified: n("corpus_modified"),
            state: s("state"),
            programs: n("programs"),
            in_library: n("in_library"),
            refused: n("refused"),
            failed: n("failed"),
            library_bytes: n("library_bytes"),
            library_identity: s("library_identity"),
            dxc: s("dxc"),
            build_seconds: t.get("build_seconds").and_then(|v| v.as_float()).unwrap_or(0.0),
            finished: n("finished"),
        })
    }

    pub fn read(dir: &Path) -> Option<Manifest> {
        Manifest::parse(&std::fs::read_to_string(dir.join(MANIFEST)).ok()?)
    }

    fn write(&self, dir: &Path) -> Result<(), String> {
        write_atomic(&dir.join(MANIFEST), self.to_toml().as_bytes())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The corpus as the manifest names it: (size, modified in unix nanoseconds).
pub fn corpus_stat(corpus: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(corpus).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    Some((m.len(), t))
}

// ---------------------------------------------------------------------------------------------------------
// status

/// Where a game's library stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// never built (or the cache was cleared)
    Missing,
    /// a build started and stopped: `done` of `total` programs are on disk
    Partial { done: u64, total: u64 },
    /// built, but by another translator, for another game build, or from another corpus: rebuild
    Stale(String),
    /// built and current
    Ready(Manifest),
}

impl Status {
    pub fn ready(&self) -> bool {
        matches!(self, Status::Ready(_))
    }

    pub fn label(&self) -> String {
        match self {
            Status::Missing => "not built yet".into(),
            Status::Partial { done, total } => format!("{done} of {total} programs ready (paused)"),
            Status::Stale(why) => format!("out of date: {why}"),
            Status::Ready(m) => format!("ready: {} programs", m.in_library),
        }
    }
}

/// The library in `dir` against the game it is for (`corpus`, its build) and a translator version. Cheap: a
/// manifest read, a stat of the corpus and (part built) a count of journal lines.
pub fn status(dir: &Path, corpus: &Path, game_build: &str, version: &str) -> Status {
    let Some(m) = Manifest::read(dir) else { return Status::Missing };
    if m.format != FORMAT || m.translator != version {
        return Status::Stale("the translator changed".into());
    }
    if m.game_build != game_build {
        return Status::Stale(format!("the game build changed ({} → {game_build})", m.game_build));
    }
    if let Some((size, modified)) = corpus_stat(corpus) {
        if (size, modified) != (m.corpus_size, m.corpus_modified) {
            return Status::Stale(format!("{CORPUS} changed"));
        }
    }
    if !m.complete() {
        let done = std::fs::read_to_string(dir.join(JOURNAL)).map_or(0, |t| t.lines().filter(|l| parse_journal_line(l).is_some()).count() as u64);
        return Status::Partial { done: done.min(m.programs), total: m.programs };
    }
    if !dir.join(LIBRARY).is_file() {
        return Status::Missing;
    }
    Status::Ready(m)
}

/// The library the layer should load from a game's shaders folder: complete and of this translator.
pub fn usable(dir: &Path) -> Result<PathBuf, String> {
    let m = Manifest::read(dir).ok_or_else(|| format!("no shader library in {} yet (the launcher builds it)", dir.display()))?;
    if m.format != FORMAT || m.translator != VERSION {
        return Err(format!("the shader library in {} is from another translator version (the launcher rebuilds it)", dir.display()));
    }
    if !m.complete() {
        return Err(format!("the shader library in {} is part built ({} programs; the launcher finishes it)", dir.display(), m.programs));
    }
    let p = dir.join(LIBRARY);
    p.is_file().then_some(p).ok_or_else(|| format!("{} has a manifest but no {LIBRARY}", dir.display()))
}

/// Which program library the layer runs with.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    /// named by `REBUILD_PIPELINES_LIB` or `REBUILD_PIPELINES_OUT` (dev: `D:\victory-data\pipelines`)
    Explicit(PathBuf),
    /// the one the launcher built in the game's cache
    Storage(PathBuf),
    /// the developer machine's library ([`DEV_LIBRARY`]), when it exists and the game has no usable one
    Dev(PathBuf),
    /// none: every program is translated on demand; the reason
    None(String),
}

/// The developer machine's library (`pipelines build` over `D:\victory-data`): used when it exists and the
/// current game has no usable library of its own. Never written by the build.
pub const DEV_LIBRARY: &str = r"D:\victory-data\pipelines\library.bin";

/// `REBUILD_PIPELINES_LIB` (a file), else `REBUILD_PIPELINES_OUT` (a folder holding `library.bin`), else the
/// current game's `shaders_dir` when [`usable`], else `dev` (the developer machine's library) when it is a
/// file. A dev override always wins over the storage library; the storage library wins over the dev one.
pub fn choose(lib_env: Option<PathBuf>, out_env: Option<PathBuf>, shaders_dir: Option<&Path>, dev: Option<&Path>) -> Choice {
    if let Some(p) = lib_env {
        return Choice::Explicit(p);
    }
    if let Some(d) = out_env {
        return Choice::Explicit(d.join(LIBRARY));
    }
    let why = match shaders_dir {
        Some(d) => match usable(d) {
            Ok(p) => return Choice::Storage(p),
            Err(e) => e,
        },
        None => "no game (REBUILD_GAME_ID or REBUILD_GAME_DIR) to find a shader library for".into(),
    };
    match dev {
        Some(p) if p.is_file() => Choice::Dev(p.to_path_buf()),
        _ => Choice::None(why),
    }
}

/// [`choose`] for this process: its environment, `rebuild_storage::current()` and [`DEV_LIBRARY`].
pub fn choose_for_process() -> Choice {
    let shaders = rebuild_storage::current().map(|g| g.shaders_dir());
    choose(std::env::var_os("REBUILD_PIPELINES_LIB").map(PathBuf::from), std::env::var_os("REBUILD_PIPELINES_OUT").map(PathBuf::from), shaders.as_deref(), Some(Path::new(DEV_LIBRARY)))
}

// ---------------------------------------------------------------------------------------------------------
// records

const REC_MAGIC: &[u8; 8] = b"RBSLREC\0";
const REC_HEAD: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Outcome {
    /// compiled: in the library
    Ok = 0,
    /// the translator refused it (or it does not decode)
    Refused = 1,
    /// DXC refused the HLSL
    Failed = 2,
}

/// One program's outcome, as its record file holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub outcome: Outcome,
    pub stage: Stage,
    pub key: [u8; 32],
    pub words: Vec<[u32; 4]>,
    pub compare: u32,
    pub top_dword: Option<u32>,
    pub reason: String,
    pub dxil: Vec<u8>,
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = REC_MAGIC.to_vec();
        for w in [FORMAT, self.outcome as u32, self.stage as u32, 0] {
            v.extend(w.to_le_bytes());
        }
        v.extend(self.key);
        for w in [self.compare, self.top_dword.unwrap_or(u32::MAX), self.words.len() as u32] {
            v.extend(w.to_le_bytes());
        }
        for x in self.words.iter().flatten() {
            v.extend(x.to_le_bytes());
        }
        v.extend((self.reason.len() as u32).to_le_bytes());
        v.extend(self.reason.as_bytes());
        v.extend((self.dxil.len() as u32).to_le_bytes());
        v.extend(&self.dxil);
        let h = blake3::hash(&v);
        v.extend(h.as_bytes());
        v
    }

    pub fn decode(b: &[u8]) -> Result<Record, String> {
        if b.len() < REC_HEAD + 32 || &b[..8] != REC_MAGIC {
            return Err("not a record".into());
        }
        let (body, sum) = b.split_at(b.len() - 32);
        if blake3::hash(body).as_bytes() != sum {
            return Err("record damaged".into());
        }
        let (outcome, stage, key) = head(body).ok_or("record of another format")?;
        let mut at = REC_HEAD;
        let mut u = || -> Result<u32, String> {
            let w = body.get(at..at + 4).ok_or("record truncated")?;
            at += 4;
            Ok(u32::from_le_bytes(w.try_into().unwrap()))
        };
        let compare = u()?;
        let top = u()?;
        let n = u()? as usize;
        if n > 1024 {
            return Err("record: too many samplers".into());
        }
        let mut words = Vec::with_capacity(n);
        for _ in 0..n {
            words.push([u()?, u()?, u()?, u()?]);
        }
        let rl = u()? as usize;
        let reason = String::from_utf8_lossy(body.get(at..at + rl).ok_or("record truncated")?).into_owned();
        at += rl;
        let dl = {
            let w = body.get(at..at + 4).ok_or("record truncated")?;
            u32::from_le_bytes(w.try_into().unwrap()) as usize
        };
        at += 4;
        let dxil = body.get(at..at + dl).ok_or("record truncated")?.to_vec();
        Ok(Record { outcome, stage, key, words, compare, top_dword: (top != u32::MAX).then_some(top), reason, dxil })
    }
}

/// A record's outcome, stage and key, from its first bytes.
fn head(b: &[u8]) -> Option<(Outcome, Stage, [u8; 32])> {
    if b.len() < REC_HEAD || &b[..8] != REC_MAGIC {
        return None;
    }
    let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    if u(8) != FORMAT {
        return None;
    }
    let outcome = match u(12) {
        0 => Outcome::Ok,
        1 => Outcome::Refused,
        2 => Outcome::Failed,
        _ => return None,
    };
    Some((outcome, Stage::from_u32(u(16))?, b[24..56].try_into().unwrap()))
}

fn read_head(p: &Path) -> Option<(Outcome, Stage, [u8; 32])> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut b = [0u8; REC_HEAD];
    f.read_exact(&mut b).ok()?;
    head(&b)
}

fn record_name(hash: u64, stage: Stage) -> String {
    format!("{hash:016x}.{}.rec", stage.name())
}

/// The key of a lifted program: what DXC would be given, and which DXC.
pub fn program_key(h: &Hlsl, dxc_id: &[u8; 32]) -> [u8; 32] {
    let mut x = blake3::Hasher::new();
    x.update(&FORMAT.to_le_bytes());
    x.update(h.stage.name().as_bytes());
    for a in translate::DXC_ARGS {
        x.update(a.as_bytes());
        x.update(&[0]);
    }
    x.update(h.src.as_bytes());
    x.update(&[0]);
    for w in h.samplers.words.iter().flatten() {
        x.update(&w.to_le_bytes());
    }
    x.update(&h.samplers.compare.to_le_bytes());
    x.update(&h.top_dword.unwrap_or(u32::MAX).to_le_bytes());
    x.update(dxc_id);
    *x.finalize().as_bytes()
}

/// The key of a refusal (no DXC involved).
fn refusal_key(reason: &str) -> [u8; 32] {
    let mut x = blake3::Hasher::new();
    x.update(&FORMAT.to_le_bytes());
    x.update(b"refused\0");
    x.update(reason.as_bytes());
    *x.finalize().as_bytes()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse_journal_line(l: &str) -> Option<((u64, Stage), [u8; 32])> {
    let (name, key) = l.split_once(' ')?;
    let (h, s) = name.split_once('.')?;
    let key = key.trim();
    if key.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(key.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(((parse_hash(h)?, Stage::from_name(s)?), k))
}

/// A program's layout, from its compiled DXIL and its record's sampler words (as `translate::layout_of`).
fn layout_of_record(r: &Record) -> Result<StageLayout, String> {
    let c = crate::bindings::read(&r.dxil)?;
    let l = StageLayout::from_bindings(&c.bindings, &r.words, r.compare)?;
    if let Some(d) = r.top_dword.filter(|d| *d >= USER_DATA_DWORDS) {
        if l.user_data {
            return Err(format!("reads user-data dword {d}, past the {USER_DATA_DWORDS} root constants"));
        }
    }
    Ok(l)
}

// ---------------------------------------------------------------------------------------------------------
// build

/// What to build, from what, where.
#[derive(Debug, Clone)]
pub struct Config {
    /// the game's `shaders.rpf` (read only)
    pub corpus: PathBuf,
    /// the game's shaders folder (`GameStore::shaders_dir()`)
    pub dir: PathBuf,
    pub game_build: String,
    pub workers: usize,
    pub dxc: PathBuf,
    /// [`VERSION`] (tests give others)
    pub version: String,
    /// DXC at below-normal priority
    pub background: bool,
    /// tests: stop as if cancelled after this many DXC compiles in the run
    pub stop_after: Option<usize>,
}

impl Config {
    pub fn new(corpus: PathBuf, dir: PathBuf, game_build: &str, dxc: PathBuf) -> Config {
        Config { corpus, dir, game_build: game_build.to_string(), workers: default_workers(), dxc, version: VERSION.to_string(), background: true, stop_after: None }
    }

    /// For a game's folder and its storage: the corpus is `<game dir>\shaders.rpf`.
    pub fn for_game(game_dir: &Path, store: &rebuild_storage::GameStore, dxc: PathBuf) -> Config {
        Config::new(game_dir.join(CORPUS), store.shaders_dir(), &store.build, dxc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Starting,
    /// reading the corpus and finding its programs
    Reading,
    /// lifting and compiling
    Translating,
    /// writing the library
    Writing,
    Done,
    /// stopped by Cancel (resumes where it stopped)
    Paused,
    Failed,
}

/// How far a build is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub phase: Phase,
    pub total: usize,
    /// programs with an outcome on disk (reused or new)
    pub done: usize,
    /// of which kept from an earlier build (no DXC)
    pub reused: usize,
    pub compiled: usize,
    pub refused: usize,
    pub failed: usize,
    pub elapsed: Duration,
    /// the rest of the translation, from this run's rate
    pub eta: Option<Duration>,
    pub error: Option<String>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        match self.phase {
            Phase::Done => 1.0,
            _ if self.total == 0 => 0.0,
            _ => self.done as f32 / self.total as f32,
        }
    }
}

/// Shared between a build and whoever watches it.
#[derive(Debug, Default)]
pub struct Control {
    cancel: AtomicBool,
    progress: Mutex<Progress>,
}

impl Control {
    pub fn new() -> Control {
        Control::default()
    }
    /// Stop at the next program (what is on disk stays; the next build resumes).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn progress(&self) -> Progress {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut self.progress.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

/// What a build did.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// false: cancelled (resumable)
    pub complete: bool,
    pub programs: usize,
    pub in_library: usize,
    pub reused: usize,
    pub compiled: usize,
    pub refused: usize,
    pub failed: usize,
    pub library_bytes: u64,
    pub read: Duration,
    pub translate: Duration,
    pub write: Duration,
    pub total: Duration,
    /// summed over workers
    pub lift_cpu: Duration,
    pub dxc_cpu: Duration,
    /// DXC failures by first line (at most 20 kept)
    pub failures: Vec<(String, String)>,
}

/// The build's counters.
#[derive(Default)]
struct Counts {
    reused: AtomicUsize,
    compiled: AtomicUsize,
    refused: AtomicUsize,
    failed: AtomicUsize,
    /// DXC died or did not start: no record, tried again next time
    transient: AtomicUsize,
    done: AtomicUsize,
}

#[derive(Clone, Copy)]
struct Snapshot {
    reused: usize,
    compiled: usize,
    refused: usize,
    failed: usize,
    transient: usize,
    done: usize,
}

impl Counts {
    fn add(&self, o: Outcome, reused: bool) {
        if reused {
            self.reused.fetch_add(1, Ordering::Relaxed);
        }
        match o {
            Outcome::Refused => self.refused.fetch_add(1, Ordering::Relaxed),
            Outcome::Failed => self.failed.fetch_add(1, Ordering::Relaxed),
            Outcome::Ok => 0,
        };
        self.done.fetch_add(1, Ordering::Relaxed);
    }
    fn snapshot(&self) -> Snapshot {
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        Snapshot { reused: l(&self.reused), compiled: l(&self.compiled), refused: l(&self.refused), failed: l(&self.failed), transient: l(&self.transient), done: l(&self.done) }
    }
}

struct Job<'a> {
    hash: u64,
    stage: Stage,
    p: &'a census::Program,
}

/// A failure that may not happen again (DXC died, or could not start): not journaled, so the next build
/// tries the program again.
fn transient(e: &str) -> bool {
    e.starts_with("dxc failed (exit") || e.starts_with("run ")
}

/// Builds (or resumes, or brings up to date) the library in `cfg.dir`.
pub fn build(cfg: &Config, ctl: &Control) -> Result<Report, String> {
    let r = build_inner(cfg, ctl);
    if let Err(e) = &r {
        ctl.set(|p| {
            p.phase = Phase::Failed;
            p.error = Some(e.clone());
        });
    }
    r
}

fn build_inner(cfg: &Config, ctl: &Control) -> Result<Report, String> {
    let t0 = Instant::now();
    ctl.set(|p| *p = Progress { phase: Phase::Reading, ..Progress::default() });
    let dir = &cfg.dir;
    let programs_dir = dir.join(PROGRAMS);
    let work = dir.join(WORK);
    std::fs::create_dir_all(&programs_dir).map_err(|e| format!("{}: {e}", programs_dir.display()))?;
    let _ = std::fs::remove_dir_all(&work);
    if !cfg.dxc.is_file() {
        return Err(format!("no DXC at {}", cfg.dxc.display()));
    }
    let dxc_id = crate::dxc::identity(&cfg.dxc);

    // the journal is valid only for the same translator, game build and corpus
    let (corpus_size, corpus_modified) = corpus_stat(&cfg.corpus).ok_or_else(|| format!("{}: not found", cfg.corpus.display()))?;
    let old = Manifest::read(dir);
    let same = old.as_ref().is_some_and(|m| m.format == FORMAT && m.translator == cfg.version && m.game_build == cfg.game_build && (m.corpus_size, m.corpus_modified) == (corpus_size, corpus_modified));
    let journal_path = dir.join(JOURNAL);
    let mut journal: HashMap<(u64, Stage), [u8; 32]> = HashMap::new();
    if same {
        for l in std::fs::read_to_string(&journal_path).unwrap_or_default().lines() {
            if let Some((k, v)) = parse_journal_line(l) {
                journal.insert(k, v);
            }
        }
    } else {
        let _ = std::fs::remove_file(&journal_path);
    }

    // the corpus: every host-stage program, once per (hash, stage), in file order
    let data = std::fs::read(&cfg.corpus).map_err(|e| format!("{}: {e}", cfg.corpus.display()))?;
    let walk = census::walk(&data, cfg.workers.clamp(1, 16));
    drop(data);
    let mut seen = HashSet::new();
    let jobs: Vec<Job> = walk
        .programs
        .iter()
        .filter_map(|p| {
            let stage = translate::host_stage(&p.native)?;
            let hash = parse_hash(&p.hash)?;
            seen.insert((hash, stage)).then_some(Job { hash, stage, p })
        })
        .collect();
    let read = t0.elapsed();
    let total = jobs.len();
    Manifest {
        format: FORMAT,
        translator: cfg.version.clone(),
        game_build: cfg.game_build.clone(),
        corpus_size,
        corpus_modified,
        state: "building".into(),
        programs: total as u64,
        dxc: cfg.dxc.display().to_string(),
        ..Manifest::default()
    }
    .write(dir)?;
    ctl.set(|p| {
        p.phase = Phase::Translating;
        p.total = total;
    });

    let journal_file = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&journal_path).map_err(|e| format!("{}: {e}", journal_path.display()))?);
    let next = AtomicUsize::new(0);
    let counts = Counts::default();
    let (lift_ns, dxc_ns) = (std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0));
    let failures: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let stop = AtomicBool::new(false);
    let errors: Mutex<Option<String>> = Mutex::new(None);
    let t_tr = Instant::now();
    let old_limit = translate::set_dxc_limit(cfg.workers.max(1));
    translate::set_dxc_below_normal(cfg.background);
    let progress = || {
        let c = counts.snapshot();
        let elapsed = t_tr.elapsed();
        // the rate of this run's own work (programs kept from before are nearly free)
        let fresh = c.done.saturating_sub(c.reused);
        let eta = (fresh >= 8 && c.done < total).then(|| elapsed.mul_f64((total - c.done) as f64 / fresh as f64));
        ctl.set(|p| {
            p.done = c.done;
            p.reused = c.reused;
            p.compiled = c.compiled;
            p.refused = c.refused;
            p.failed = c.failed + c.transient;
            p.elapsed = t0.elapsed();
            p.eta = eta;
        });
    };
    // one program's outcome is on disk: journal it (unless it already is) and count it
    let finish = |job: &Job, key: &[u8; 32], o: Outcome, reused: bool, journaled: bool| {
        if !journaled {
            let line = format!("{:016x}.{} {}\n", job.hash, job.stage.name(), hex(key));
            let _ = journal_file.lock().unwrap_or_else(|e| e.into_inner()).write_all(line.as_bytes());
        }
        counts.add(o, reused);
        progress();
    };
    let fail = |job: &Job, e: String| {
        let mut f = failures.lock().unwrap_or_else(|e| e.into_inner());
        if f.len() < 20 {
            f.push((format!("{:016x}.{}", job.hash, job.stage.name()), e));
        }
    };
    let write_rec = |path: &Path, rec: &Record| -> bool {
        match write_atomic(path, &rec.encode()) {
            Ok(()) => true,
            Err(e) => {
                *errors.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                stop.store(true, Ordering::Relaxed);
                false
            }
        }
    };
    std::thread::scope(|s| {
        for _ in 0..cfg.workers.max(1) {
            s.spawn(|| loop {
                if ctl.cancelled() || stop.load(Ordering::Relaxed) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(i) else { break };
                let rec_path = programs_dir.join(record_name(job.hash, job.stage));
                let on_disk = read_head(&rec_path).filter(|(_, st, _)| *st == job.stage);
                // resumed: journaled for this translator, and its record still says the same
                if let (Some(k), Some((o, _, rk))) = (journal.get(&(job.hash, job.stage)), on_disk) {
                    if &rk == k {
                        finish(job, k, o, true, true);
                        continue;
                    }
                }
                let t = Instant::now();
                let lifted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Hlsl, String> {
                    translate::check_kind(&job.p.native)?;
                    translate::lift_hlsl(&job.p.native, &job.p.isg1, &job.p.osg1, true).map_err(|r| r.reason)
                }))
                .unwrap_or_else(|e| Err(format!("translator panicked: {}", e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())));
                lift_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                let h = match lifted {
                    Ok(h) => h,
                    Err(reason) => {
                        let key = refusal_key(&reason);
                        let same = on_disk.is_some_and(|(_, _, k)| k == key);
                        let rec = Record { outcome: Outcome::Refused, stage: job.stage, key, words: Vec::new(), compare: 0, top_dword: None, reason, dxil: Vec::new() };
                        if !same && !write_rec(&rec_path, &rec) {
                            break;
                        }
                        finish(job, &key, Outcome::Refused, same, false);
                        continue;
                    }
                };
                let key = program_key(&h, &dxc_id);
                // the same HLSL compiled before (a translator change elsewhere, or a game patch): kept
                if let Some((o, _, rk)) = on_disk {
                    if rk == key {
                        finish(job, &key, o, true, false);
                        continue;
                    }
                }
                let t = Instant::now();
                let compiled = translate::compile_hlsl(&cfg.dxc, &work, job.hash, &h);
                dxc_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                let mut rec = Record { outcome: Outcome::Ok, stage: job.stage, key, words: h.samplers.words.clone(), compare: h.samplers.compare, top_dword: h.top_dword, reason: String::new(), dxil: Vec::new() };
                match compiled {
                    Ok(dxil) => {
                        rec.dxil = dxil;
                        if let Err(e) = layout_of_record(&rec) {
                            // a layout the library refuses: never in the library, and the reason kept
                            rec.outcome = Outcome::Refused;
                            rec.reason = format!("layout: {e}");
                            rec.dxil.clear();
                        }
                    }
                    Err(e) if transient(&e) => {
                        // not journaled: tried again by the next build
                        fail(job, e);
                        counts.transient.fetch_add(1, Ordering::Relaxed);
                        progress();
                        continue;
                    }
                    Err(e) => {
                        fail(job, e.clone());
                        rec.outcome = Outcome::Failed;
                        rec.reason = e;
                    }
                }
                if !write_rec(&rec_path, &rec) {
                    break;
                }
                counts.compiled.fetch_add(1, Ordering::Relaxed);
                finish(job, &key, rec.outcome, false, false);
                if cfg.stop_after.is_some_and(|n| counts.compiled.load(Ordering::Relaxed) >= n) {
                    stop.store(true, Ordering::Relaxed);
                }
            });
        }
    });
    translate::set_dxc_limit(old_limit);
    translate::set_dxc_below_normal(false);
    let _ = std::fs::remove_dir_all(&work);
    drop(journal_file);
    progress();
    if let Some(e) = errors.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(e);
    }
    let c = counts.snapshot();
    let mut report = Report {
        programs: total,
        reused: c.reused,
        compiled: c.compiled,
        refused: c.refused,
        failed: c.failed + c.transient,
        read,
        translate: t_tr.elapsed(),
        lift_cpu: Duration::from_nanos(lift_ns.load(Ordering::Relaxed)),
        dxc_cpu: Duration::from_nanos(dxc_ns.load(Ordering::Relaxed)),
        failures: failures.into_inner().unwrap_or_else(|e| e.into_inner()),
        ..Report::default()
    };
    if ctl.cancelled() || stop.load(Ordering::Relaxed) || next.load(Ordering::Relaxed) < total {
        ctl.set(|p| p.phase = Phase::Paused);
        report.total = t0.elapsed();
        return Ok(report);
    }

    // the library, from every record
    ctl.set(|p| p.phase = Phase::Writing);
    let t_w = Instant::now();
    let inputs: Vec<Result<Option<crate::library::Input>, String>> = crate::library::parallel_map(&jobs, cfg.workers.clamp(1, 16), |j| {
        let p = programs_dir.join(record_name(j.hash, j.stage));
        let Ok(b) = std::fs::read(&p) else { return Ok(None) };
        let r = Record::decode(&b).map_err(|e| format!("{}: {e}", p.display()))?;
        if r.outcome != Outcome::Ok {
            return Ok(None);
        }
        let layout = layout_of_record(&r)?;
        Ok(Some(crate::library::Input { hash: j.hash, stage: j.stage, dxil: r.dxil, layout }))
    });
    let mut programs = Vec::new();
    for r in inputs {
        if let Some(i) = r? {
            programs.push(i);
        }
    }
    let in_library = programs.len();
    let lib_path = dir.join(LIBRARY);
    #[cfg(windows)]
    let st = crate::library::build(&lib_path, USER_DATA_DWORDS, programs, cfg.workers.clamp(1, 16), crate::d3d::serialize)?;
    #[cfg(not(windows))]
    let st = crate::library::build(&lib_path, USER_DATA_DWORDS, programs, 1, |_| Err("no root signature serializer off Windows".to_string()))?;
    let identity = crate::library::Library::load(&lib_path, 4, false).map(|(l, _)| hex(&l.identity())).unwrap_or_default();
    // records of programs no longer in the corpus
    let keep: HashSet<String> = jobs.iter().map(|j| record_name(j.hash, j.stage)).collect();
    for e in std::fs::read_dir(&programs_dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !keep.contains(&n) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    report.write = t_w.elapsed();
    report.in_library = in_library;
    report.library_bytes = st.file_bytes as u64;
    report.complete = true;
    report.total = t0.elapsed();
    Manifest {
        format: FORMAT,
        translator: cfg.version.clone(),
        game_build: cfg.game_build.clone(),
        corpus_size,
        corpus_modified,
        state: "complete".into(),
        programs: total as u64,
        in_library: in_library as u64,
        refused: report.refused as u64,
        failed: report.failed as u64,
        library_bytes: st.file_bytes as u64,
        library_identity: identity,
        dxc: cfg.dxc.display().to_string(),
        build_seconds: report.total.as_secs_f64(),
        finished: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    }
    .write(dir)?;
    ctl.set(|p| {
        p.phase = Phase::Done;
        p.done = total;
        p.eta = None;
        p.elapsed = t0.elapsed();
    });
    Ok(report)
}

#[cfg(test)]
mod tests;
