//! The overlay file system: the title's console paths over a read-only game folder (Build 3).
//!
//! ```text
//!   title path   G:\data\x.rpf   T:\cache\y   S:\save0\profile
//!        │  parse: drive letter, `.`/`..` resolved, case folded for lookup     (path.rs)
//!        ▼
//!   write overlay   <overlay>/<LETTER>/files   every create, write, rename, delete   (overlay.rs)
//!   mods            mods/<name>/ in load order, later wins (package drive only)   (mods.rs)
//!   base            the game folder, opened read-only, never written             (layer.rs)
//! ```
//!
//! Reads see the overlay first, then the mods (last enabled first), then the base. A write to a file
//! that lives below the overlay copies it up first; a delete of one records a whiteout. Nothing is
//! ever opened for writing outside the overlay folder, and the overlay folder may not sit inside the
//! game folder or a mod. There is no global state: a [`Vfs`] owns its drives and its handles, and is
//! shared between threads by reference.

pub mod mods;
pub mod path;

mod layer;
mod overlay;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use layer::ReadLayer;
use overlay::{ONode, Overlay};
use path::child_key;

pub use mods::{Conflict, Mod, ModSet};
pub use path::{fold, ConsolePath};

#[derive(Debug)]
pub enum Error {
    /// Not a path on a drive letter (relative, UNC, or no `X:`).
    NotOnDrive(String),
    /// A drive letter no drive is mounted at.
    NoDrive(char),
    InvalidPath(String),
    /// The file is absent, its directory exists (`ERROR_FILE_NOT_FOUND`).
    NotFound,
    /// A directory on the way is absent (`ERROR_PATH_NOT_FOUND`).
    PathNotFound,
    Exists,
    IsADirectory,
    NotADirectory,
    DirectoryNotEmpty,
    CrossDrive,
    BadHandle,
    ReadOnlyHandle,
    /// Refused at build: the overlay would write into the base or a mod.
    Unsafe(String),
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotOnDrive(p) => write!(f, "not on a console drive: {p}"),
            Error::NoDrive(l) => write!(f, "no drive mounted at {l}:"),
            Error::InvalidPath(p) => write!(f, "invalid path: {p}"),
            Error::NotFound => write!(f, "file not found"),
            Error::PathNotFound => write!(f, "path not found"),
            Error::Exists => write!(f, "already exists"),
            Error::IsADirectory => write!(f, "is a directory"),
            Error::NotADirectory => write!(f, "not a directory"),
            Error::DirectoryNotEmpty => write!(f, "directory not empty"),
            Error::CrossDrive => write!(f, "rename across drives"),
            Error::BadHandle => write!(f, "bad handle"),
            Error::ReadOnlyHandle => write!(f, "handle not open for writing"),
            Error::Unsafe(why) => write!(f, "refused: {why}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Which layer a path resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    Overlay,
    Mod(String),
    Base,
}

/// A console path resolved to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub host: PathBuf,
    pub layer: Layer,
    pub is_dir: bool,
    pub len: u64,
}

/// One directory entry in the merged view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub len: u64,
    pub layer: Layer,
}

/// As `CreateFile`'s creation disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    CreateNew,
    CreateAlways,
    OpenExisting,
    OpenAlways,
    TruncateExisting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    pub write: bool,
    pub disposition: Disposition,
}

impl OpenOptions {
    /// Open an existing file for reading.
    pub fn read() -> Self {
        Self { write: false, disposition: Disposition::OpenExisting }
    }
    /// Open for reading and writing with the given disposition.
    pub fn write(disposition: Disposition) -> Self {
        Self { write: true, disposition }
    }
}

/// An open file. Stale handles (closed, slot reused) are refused by generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(u64);

/// Where a path resolved, internally: the overlay or a lower layer by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Where {
    Overlay,
    Lower(usize),
}

struct Found {
    host: PathBuf,
    at: Where,
    is_dir: bool,
    len: u64,
}

struct Drive {
    letter: char,
    /// Top first: enabled mods, last first, then the base.
    lower: Vec<(Layer, ReadLayer)>,
    overlay: RwLock<Overlay>,
}

struct OpenFile {
    file: File,
    write: bool,
    /// Opened for overlapped I/O ([`Vfs::open_concurrent`]): reads at an offset run side by side.
    concurrent: bool,
}

#[derive(Default)]
struct Handles {
    slots: Vec<(u32, Option<Arc<OpenFile>>)>,
    free: Vec<u32>,
}

pub struct Vfs {
    drives: Vec<Drive>,
    handles: Mutex<Handles>,
    /// Overlay files the host will delete behind our back (delete-on-close, a delete disposition set
    /// through a handle): settled by the next operation once the host file is gone.
    pending: Mutex<Vec<ConsolePath>>,
    has_pending: std::sync::atomic::AtomicBool,
}

// The overlay root is the caller's: the game's `overlay` folder from `rebuild_storage` (the layer's drives).

/// Where `p` is or would be once created: its deepest existing ancestor canonicalized, the rest appended.
fn resolve(p: &Path) -> io::Result<PathBuf> {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir()?.join(p) };
    let mut rest = Vec::new();
    let mut at = abs.as_path();
    loop {
        match fs::canonicalize(at) {
            Ok(mut real) => {
                for c in rest.iter().rev() {
                    real.push(c);
                }
                return Ok(real);
            }
            Err(e) => match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_os_string());
                    at = parent;
                }
                _ => return Err(e),
            },
        }
    }
}

/// Whether one path holds the other (component-wise; letter case ignored on Windows, as the file system does).
fn overlaps(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| -> Vec<String> {
        p.components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .map(|c| {
                let s = c.as_os_str().to_string_lossy().into_owned();
                if cfg!(windows) {
                    s.to_lowercase()
                } else {
                    s
                }
            })
            .collect()
    };
    let (a, b) = (key(a), key(b));
    a.starts_with(&b) || b.starts_with(&a)
}

pub struct VfsBuilder {
    overlay_root: PathBuf,
    drives: Vec<(char, Option<(PathBuf, ModSet)>)>,
    /// drives whose overlay folder is given rather than `<overlay root>\<letter>`
    folders: Vec<(char, PathBuf)>,
}

impl VfsBuilder {
    pub fn new(overlay_root: impl Into<PathBuf>) -> Self {
        Self { overlay_root: overlay_root.into(), drives: Vec::new(), folders: Vec::new() }
    }

    /// A drive over a read-only base folder (the package, `G:`), with mods layered on it.
    pub fn package(mut self, letter: char, base: impl Into<PathBuf>, mods: &ModSet) -> Self {
        self.drives.push((letter.to_ascii_uppercase(), Some((base.into(), mods.clone()))));
        self
    }

    /// A drive that is only the overlay: temporary files (`T:`) or persistent storage.
    pub fn writable(mut self, letter: char) -> Self {
        self.drives.push((letter.to_ascii_uppercase(), None));
        self
    }

    /// A drive that is only the overlay, in a folder of its own (the title's persistent storage, `S:`, kept
    /// with the game's data rather than inside the overlay).
    pub fn writable_at(mut self, letter: char, folder: impl Into<PathBuf>) -> Self {
        let letter = letter.to_ascii_uppercase();
        self.drives.push((letter, None));
        self.folders.push((letter, folder.into()));
        self
    }

    /// Every drive is checked before anything is created: an overlay that would sit inside the base or a mod
    /// (or hold one) is refused without making a folder there.
    pub fn build(self) -> Result<Vfs> {
        let mut planned: Vec<(char, PathBuf, Vec<(Layer, PathBuf)>)> = Vec::new();
        for (letter, base) in self.drives {
            if !letter.is_ascii_alphabetic() {
                return Err(Error::NoDrive(letter));
            }
            if planned.iter().any(|d| d.0 == letter) {
                return Err(Error::Unsafe(format!("drive {letter}: mounted twice")));
            }
            let dir = self.folders.iter().find(|f| f.0 == letter).map(|f| f.1.clone()).unwrap_or_else(|| self.overlay_root.join(letter.to_string()));
            let mut lower = Vec::new();
            if let Some((base, mods)) = base {
                let mut roots: Vec<(Layer, PathBuf)> =
                    mods.enabled().map(|m| (Layer::Mod(m.name.clone()), m.root.clone())).collect();
                roots.reverse();
                roots.push((Layer::Base, base));
                let overlay_real = resolve(&dir)?;
                for (layer, root) in roots {
                    let real = fs::canonicalize(&root)?;
                    if overlaps(&overlay_real, &real) {
                        return Err(Error::Unsafe(format!(
                            "overlay {} and {:?} layer {} overlap",
                            dir.display(),
                            layer,
                            root.display()
                        )));
                    }
                    lower.push((layer, real));
                }
            }
            planned.push((letter, dir, lower));
        }
        let mut drives = Vec::new();
        for (letter, dir, lower) in planned {
            fs::create_dir_all(&dir)?;
            // again on the real folder (a link on the way could lead elsewhere than the plan)
            let overlay_real = fs::canonicalize(&dir)?;
            if let Some((layer, real)) = lower.iter().find(|(_, real)| overlaps(&overlay_real, real)) {
                return Err(Error::Unsafe(format!("overlay {} and {:?} layer {} overlap", dir.display(), layer, real.display())));
            }
            let lower = lower.into_iter().map(|(layer, real)| (layer, ReadLayer::new(real))).collect();
            drives.push(Drive { letter, lower, overlay: RwLock::new(Overlay::open(&dir)?) });
        }
        Ok(Vfs {
            drives,
            handles: Mutex::new(Handles::default()),
            pending: Mutex::new(Vec::new()),
            has_pending: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

fn read_lock(d: &Drive) -> RwLockReadGuard<'_, Overlay> {
    d.overlay.read().unwrap_or_else(|e| e.into_inner())
}

fn write_lock(d: &Drive) -> RwLockWriteGuard<'_, Overlay> {
    d.overlay.write().unwrap_or_else(|e| e.into_inner())
}

impl Vfs {
    fn drive(&self, letter: char) -> Result<&Drive> {
        self.drives.iter().find(|d| d.letter == letter).ok_or(Error::NoDrive(letter))
    }

    fn parse(&self, path: &str) -> Result<(&Drive, ConsolePath)> {
        let cp = ConsolePath::parse(path)?;
        Ok((self.drive(cp.drive)?, cp))
    }

    fn layer(d: &Drive, at: Where) -> Layer {
        match at {
            Where::Overlay => Layer::Overlay,
            Where::Lower(i) => d.lower[i].0.clone(),
        }
    }

    fn find(d: &Drive, ov: &Overlay, cp: &ConsolePath) -> Option<Found> {
        if cp.is_root() {
            return Some(Found { host: ov.dir_host("")?, at: Where::Overlay, is_dir: true, len: 0 });
        }
        let key = cp.key();
        if let Some(n) = ov.get(&key) {
            // An overlay file's length can change through any handle: read it only when asked.
            return Some(Found { host: n.host.clone(), at: Where::Overlay, is_dir: n.is_dir, len: 0 });
        }
        if ov.hidden(&key) {
            return None;
        }
        d.lower.iter().enumerate().find_map(|(i, (_, l))| {
            l.lookup(&cp.parts, &key).map(|(host, is_dir, len)| Found { host, at: Where::Lower(i), is_dir, len })
        })
    }

    fn lower_has(d: &Drive, cp: &ConsolePath) -> bool {
        let key = cp.key();
        d.lower.iter().any(|(_, l)| l.lookup(&cp.parts, &key).is_some())
    }

    /// `NotFound` when the parent directory exists, else `PathNotFound`.
    fn missing(d: &Drive, ov: &Overlay, cp: &ConsolePath) -> Error {
        match Self::find(d, ov, &cp.parent()) {
            Some(p) if p.is_dir => Error::NotFound,
            _ => Error::PathNotFound,
        }
    }

    /// Makes the directory at `cp` exist in the overlay (it must exist in the merged view), with
    /// every directory above it, in the case the layers below spell them. Returns its host path.
    fn overlay_dir(d: &Drive, ov: &mut Overlay, cp: &ConsolePath) -> Result<PathBuf> {
        let mut key = String::new();
        let mut host = ov.dir_host("").expect("overlay root");
        for i in 0..cp.parts.len() {
            key = child_key(&key, &fold(&cp.parts[i]));
            if let Some(h) = ov.dir_host(&key) {
                host = h;
                continue;
            }
            let prefix = ConsolePath { drive: cp.drive, parts: cp.parts[..=i].to_vec() };
            let found = Self::find(d, ov, &prefix).ok_or(Error::PathNotFound)?;
            if !found.is_dir {
                return Err(Error::PathNotFound);
            }
            let name = found.host.file_name().and_then(|n| n.to_str()).unwrap_or(&cp.parts[i]).to_owned();
            host = host.join(&name);
            match fs::create_dir(&host) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                _ => {}
            }
            ov.insert(&key, ONode { host: host.clone(), name, is_dir: true });
        }
        Ok(host)
    }

    /// The overlay host path for the file at `cp`, recorded in the overlay index (the file itself
    /// is the caller's to create). `name` is the case to create it in.
    fn overlay_file(d: &Drive, ov: &mut Overlay, cp: &ConsolePath, name: &str) -> Result<PathBuf> {
        let key = cp.key();
        if let Some(n) = ov.get(&key) {
            return Ok(n.host.clone());
        }
        let host = Self::overlay_dir(d, ov, &cp.parent())?.join(name);
        ov.insert(&key, ONode { host: host.clone(), name: name.to_owned(), is_dir: false });
        ov.unwhiteout(&key)?;
        Ok(host)
    }

    /// Copies a lower file into the overlay; returns the overlay host path.
    fn copy_up(d: &Drive, ov: &mut Overlay, cp: &ConsolePath, found: &Found) -> Result<PathBuf> {
        if found.at == Where::Overlay {
            return Ok(found.host.clone());
        }
        let name = found.host.file_name().and_then(|n| n.to_str()).unwrap_or(cp.parts.last().unwrap()).to_owned();
        let host = Self::overlay_file(d, ov, cp, &name)?;
        copy_writable(&found.host, &host)?;
        Ok(host)
    }

    fn add_handle(&self, file: File, write: bool) -> Handle {
        self.add_open(OpenFile { file, write, concurrent: false })
    }

    fn add_open(&self, open: OpenFile) -> Handle {
        let mut h = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        let open = Some(Arc::new(open));
        let index = match h.free.pop() {
            Some(i) => {
                let slot = &mut h.slots[i as usize];
                slot.0 = slot.0.wrapping_add(1);
                slot.1 = open;
                i
            }
            None => {
                h.slots.push((0, open));
                (h.slots.len() - 1) as u32
            }
        };
        let generation = h.slots[index as usize].0;
        Handle((u64::from(generation) << 32) | u64::from(index))
    }

    fn get(&self, handle: Handle) -> Result<Arc<OpenFile>> {
        let h = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        match h.slots.get((handle.0 & 0xffff_ffff) as usize) {
            Some((generation, Some(f))) if u64::from(*generation) == handle.0 >> 32 => Ok(f.clone()),
            _ => Err(Error::BadHandle),
        }
    }

    // ---- V1: console path → host path --------------------------------------------------------

    /// Where a console path lives on the host, and in which layer.
    pub fn resolve(&self, path: &str) -> Result<Resolved> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let ov = read_lock(d);
        match Self::find(d, &ov, &cp) {
            Some(f) => {
                let len = if f.at == Where::Overlay && !f.is_dir { fs::metadata(&f.host)?.len() } else { f.len };
                Ok(Resolved { host: f.host, layer: Self::layer(d, f.at), is_dir: f.is_dir, len })
            }
            None => Err(Self::missing(d, &ov, &cp)),
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_ok()
    }

    // ---- V4: the handle API ------------------------------------------------------------------

    pub fn open(&self, path: &str, opts: OpenOptions) -> Result<Handle> {
        self.settle();
        use Disposition::*;
        let (d, cp) = self.parse(path)?;
        if !opts.write && opts.disposition == OpenExisting {
            // The common case: read-only, nothing can change, so the shared lock is enough.
            let ov = read_lock(d);
            let found = Self::find(d, &ov, &cp).ok_or_else(|| Self::missing(d, &ov, &cp))?;
            if found.is_dir {
                return Err(Error::IsADirectory);
            }
            return Ok(self.add_handle(File::open(&found.host)?, false));
        }
        if cp.is_root() {
            return Err(Error::IsADirectory);
        }
        let mut ov = write_lock(d);
        let found = Self::find(d, &ov, &cp);
        let host = match (found, opts.disposition) {
            (Some(f), _) if f.is_dir => return Err(Error::IsADirectory),
            (Some(_), CreateNew) => return Err(Error::Exists),
            (None, OpenExisting | TruncateExisting) => return Err(Self::missing(d, &ov, &cp)),
            (Some(f), OpenExisting | OpenAlways) => {
                if !opts.write {
                    return Ok(self.add_handle(File::open(&f.host)?, false));
                }
                Self::copy_up(d, &mut ov, &cp, &f)?
            }
            (Some(f), CreateAlways | TruncateExisting) => {
                let name = f.host.file_name().and_then(|n| n.to_str()).unwrap_or(cp.parts.last().unwrap()).to_owned();
                let host = Self::overlay_file(d, &mut ov, &cp, &name)?;
                File::create(&host)?;
                host
            }
            (None, CreateNew | CreateAlways | OpenAlways) => {
                let host = Self::overlay_file(d, &mut ov, &cp, cp.parts.last().unwrap())?;
                if let Err(e) = File::create(&host) {
                    ov.remove(&cp.key());
                    return Err(e.into());
                }
                host
            }
        };
        let file = fs::OpenOptions::new().read(true).write(opts.write).open(&host)?;
        Ok(self.add_handle(file, opts.write))
    }

    /// Opens an existing file for reading by many threads at once (DirectStorage's workers).
    ///
    /// A handle from [`Vfs::open`] is synchronous, and Windows runs the reads of one synchronous file object one
    /// at a time (each holds the file object's lock), so workers sharing it wait on each other. This one is
    /// opened for overlapped I/O: each [`Vfs::read_at`] on it is its own request, waited for by the calling
    /// thread alone. Read only.
    pub fn open_concurrent(&self, path: &str) -> Result<Handle> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let ov = read_lock(d);
        let found = Self::find(d, &ov, &cp).ok_or_else(|| Self::missing(d, &ov, &cp))?;
        if found.is_dir {
            return Err(Error::IsADirectory);
        }
        let mut o = fs::OpenOptions::new();
        o.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            o.custom_flags(concurrent::FILE_FLAG_OVERLAPPED);
        }
        let file = o.open(&found.host)?;
        Ok(self.add_open(OpenFile { file, write: false, concurrent: cfg!(windows) }))
    }

    /// Reads at `offset` until `buf` is full or the file ends; returns the bytes read.
    pub fn read_at(&self, handle: Handle, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let f = self.get(handle)?;
        let mut done = 0;
        while done < buf.len() {
            #[cfg(windows)]
            let n = if f.concurrent {
                concurrent::read(&f.file, &mut buf[done..], offset + done as u64)?
            } else {
                pread(&f.file, &mut buf[done..], offset + done as u64)?
            };
            #[cfg(not(windows))]
            let n = pread(&f.file, &mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    pub fn write_at(&self, handle: Handle, offset: u64, data: &[u8]) -> Result<()> {
        let f = self.get(handle)?;
        if !f.write {
            return Err(Error::ReadOnlyHandle);
        }
        let mut done = 0;
        while done < data.len() {
            let n = pwrite(&f.file, &data[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero).into());
            }
            done += n;
        }
        Ok(())
    }

    pub fn size(&self, handle: Handle) -> Result<u64> {
        Ok(self.get(handle)?.file.metadata()?.len())
    }

    pub fn set_size(&self, handle: Handle, len: u64) -> Result<()> {
        let f = self.get(handle)?;
        if !f.write {
            return Err(Error::ReadOnlyHandle);
        }
        Ok(f.file.set_len(len)?)
    }

    pub fn flush(&self, handle: Handle) -> Result<()> {
        Ok(self.get(handle)?.file.sync_data()?)
    }

    pub fn close(&self, handle: Handle) -> Result<()> {
        let mut h = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        let index = (handle.0 & 0xffff_ffff) as u32;
        match h.slots.get_mut(index as usize) {
            Some((generation, open @ Some(_))) if u64::from(*generation) == handle.0 >> 32 => {
                *open = None;
                h.free.push(index);
                Ok(())
            }
            _ => Err(Error::BadHandle),
        }
    }

    pub fn open_handles(&self) -> usize {
        let h = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        h.slots.iter().filter(|s| s.1.is_some()).count()
    }

    /// The merged listing of a directory, sorted by folded name.
    pub fn enumerate(&self, path: &str) -> Result<Vec<Entry>> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let ov = read_lock(d);
        Self::list(d, &ov, &cp)
    }

    fn list(d: &Drive, ov: &Overlay, cp: &ConsolePath) -> Result<Vec<Entry>> {
        let found = Self::find(d, ov, cp).ok_or_else(|| Self::missing(d, ov, cp))?;
        if !found.is_dir {
            return Err(Error::NotADirectory);
        }
        let key = cp.key();
        let mut out: BTreeMap<String, Entry> = BTreeMap::new();
        for (folded, n) in ov.children(&key) {
            let len = if n.is_dir { 0 } else { fs::metadata(&n.host).map(|m| m.len()).unwrap_or(0) };
            out.insert(folded, Entry { name: n.name, is_dir: n.is_dir, len, layer: Layer::Overlay });
        }
        for (layer, l) in &d.lower {
            let Some(dir) = l.list(&cp.parts, &key) else { continue };
            for (folded, node) in dir.entries.iter() {
                if out.contains_key(folded) || ov.hidden(&child_key(&key, folded)) {
                    continue;
                }
                out.insert(
                    folded.clone(),
                    Entry { name: node.name.clone(), is_dir: node.is_dir, len: node.len, layer: layer.clone() },
                );
            }
        }
        Ok(out.into_values().collect())
    }

    // ---- V2: mutations, all into the overlay -------------------------------------------------

    pub fn delete(&self, path: &str) -> Result<()> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let mut ov = write_lock(d);
        let found = Self::find(d, &ov, &cp).ok_or_else(|| Self::missing(d, &ov, &cp))?;
        if found.is_dir {
            return Err(Error::IsADirectory);
        }
        Self::remove_found(d, &mut ov, &cp, &found)
    }

    fn remove_found(d: &Drive, ov: &mut Overlay, cp: &ConsolePath, found: &Found) -> Result<()> {
        let key = cp.key();
        if found.at == Where::Overlay {
            if found.is_dir {
                fs::remove_dir(&found.host)?;
            } else {
                fs::remove_file(&found.host)?;
            }
            ov.remove(&key);
        }
        if Self::lower_has(d, cp) {
            ov.whiteout(&key)?;
        }
        Ok(())
    }

    pub fn create_dir(&self, path: &str) -> Result<()> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let mut ov = write_lock(d);
        Self::make_dir(d, &mut ov, &cp)
    }

    fn make_dir(d: &Drive, ov: &mut Overlay, cp: &ConsolePath) -> Result<()> {
        if cp.is_root() || Self::find(d, ov, cp).is_some() {
            return Err(Error::Exists);
        }
        let name = cp.parts.last().unwrap().clone();
        let host = Self::overlay_dir(d, ov, &cp.parent())?.join(&name);
        fs::create_dir(&host)?;
        let key = cp.key();
        ov.insert(&key, ONode { host, name, is_dir: true });
        ov.unwhiteout(&key)?;
        Ok(())
    }

    pub fn remove_dir(&self, path: &str) -> Result<()> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        if cp.is_root() {
            return Err(Error::DirectoryNotEmpty);
        }
        let mut ov = write_lock(d);
        let found = Self::find(d, &ov, &cp).ok_or_else(|| Self::missing(d, &ov, &cp))?;
        if !found.is_dir {
            return Err(Error::NotADirectory);
        }
        if !Self::list(d, &ov, &cp)?.is_empty() {
            return Err(Error::DirectoryNotEmpty);
        }
        Self::remove_found(d, &mut ov, &cp, &found)
    }

    /// Renames a file or directory on one drive. `replace` lets a file replace a file.
    pub fn rename(&self, from: &str, to: &str, replace: bool) -> Result<()> {
        self.settle();
        let (d, src) = self.parse(from)?;
        let dst = ConsolePath::parse(to)?;
        if dst.drive != src.drive {
            return Err(Error::CrossDrive);
        }
        if src.is_root() || dst.is_root() {
            return Err(Error::InvalidPath(to.to_owned()));
        }
        let (src_key, dst_key) = (src.key(), dst.key());
        if dst_key.starts_with(&format!("{src_key}/")) {
            return Err(Error::InvalidPath(to.to_owned()));
        }
        let mut ov = write_lock(d);
        let found = Self::find(d, &ov, &src).ok_or_else(|| Self::missing(d, &ov, &src))?;
        if src_key == dst_key {
            // A change of case only.
            if found.at == Where::Overlay {
                let name = dst.parts.last().unwrap().clone();
                let host = found.host.with_file_name(&name);
                fs::rename(&found.host, &host)?;
                ov.insert(&dst_key, ONode { host, name, is_dir: found.is_dir });
            }
            return Ok(());
        }
        match Self::find(d, &ov, &dst) {
            Some(existing) if !replace || existing.is_dir || found.is_dir => return Err(Error::Exists),
            _ => {}
        }
        match Self::find(d, &ov, &dst.parent()) {
            Some(p) if p.is_dir => {}
            _ => return Err(Error::PathNotFound),
        }
        Self::move_found(d, &mut ov, &src, &found, &dst)
    }

    fn move_found(d: &Drive, ov: &mut Overlay, src: &ConsolePath, found: &Found, dst: &ConsolePath) -> Result<()> {
        if found.is_dir {
            Self::make_dir(d, ov, dst)?;
            for child in Self::list(d, ov, src)? {
                let from = src.join(&child.name);
                let f = Self::find(d, ov, &from).ok_or(Error::NotFound)?;
                Self::move_found(d, ov, &from, &f, &dst.join(&child.name))?;
            }
            let found = Self::find(d, ov, src).ok_or(Error::NotFound)?;
            return Self::remove_found(d, ov, src, &found);
        }
        let host = Self::overlay_file(d, ov, dst, dst.parts.last().unwrap())?;
        if found.at == Where::Overlay {
            fs::rename(&found.host, &host)?;
            ov.remove(&src.key());
        } else {
            copy_writable(&found.host, &host)?;
        }
        if Self::lower_has(d, src) {
            ov.whiteout(&src.key())?;
        }
        Ok(())
    }

    // ---- changes the host makes behind our back (delete-on-close, handle renames) -------------

    /// The file at `path` (in the overlay) will be deleted by the host when its last handle closes,
    /// or already is: the next operation drops it from the index, and hides the layers below.
    pub fn delete_pending(&self, path: &str) -> Result<()> {
        let cp = ConsolePath::parse(path)?;
        self.drive(cp.drive)?;
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).push(cp);
        self.has_pending.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn settle(&self) {
        if !self.has_pending.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|cp| {
            let Ok(d) = self.drive(cp.drive) else { return false };
            let mut ov = write_lock(d);
            let key = cp.key();
            let Some(n) = ov.get(&key).cloned() else { return false };
            if fs::symlink_metadata(&n.host).is_ok() {
                return true; // still there: a handle is open
            }
            if n.is_dir {
                ov.remove_tree(&key);
            } else {
                ov.remove(&key);
            }
            if Self::lower_has(d, cp) {
                let _ = ov.whiteout(&key);
            }
            false
        });
        self.has_pending.store(!pending.is_empty(), std::sync::atomic::Ordering::Release);
    }

    /// The overlay host path a file at `path` would have, with its directories made in the overlay
    /// (the file itself is not created or indexed): the target of a rename the host performs.
    pub fn overlay_path_for(&self, path: &str) -> Result<PathBuf> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        if cp.is_root() {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        let mut ov = write_lock(d);
        if let Some(n) = ov.get(&cp.key()) {
            return Ok(n.host.clone());
        }
        Ok(Self::overlay_dir(d, &mut ov, &cp.parent())?.join(cp.parts.last().unwrap()))
    }

    /// Records a rename the host made of an overlay file or directory, from `from` to the host path
    /// [`Vfs::overlay_path_for`] gave for `to`.
    pub fn note_renamed(&self, from: &str, to: &str) -> Result<()> {
        self.settle();
        let (d, src) = self.parse(from)?;
        let dst = ConsolePath::parse(to)?;
        if dst.drive != src.drive {
            return Err(Error::CrossDrive);
        }
        let mut ov = write_lock(d);
        let (src_key, dst_key) = (src.key(), dst.key());
        let node = ov.get(&src_key).cloned().ok_or(Error::NotFound)?;
        let name = dst.parts.last().ok_or_else(|| Error::InvalidPath(to.to_owned()))?.clone();
        let host = match ov.dir_host(&dst.parent().key()) {
            Some(h) => h.join(&name),
            None => return Err(Error::PathNotFound),
        };
        if node.is_dir {
            ov.remove_tree(&src_key);
            ov.remove_tree(&dst_key);
            ov.insert(&dst_key, ONode { host: host.clone(), name, is_dir: true });
            ov.index(&host, &dst_key)?;
        } else {
            ov.remove(&src_key);
            ov.insert(&dst_key, ONode { host, name, is_dir: false });
        }
        ov.unwhiteout(&dst_key)?;
        if Self::lower_has(d, &src) {
            ov.whiteout(&src_key)?;
        }
        Ok(())
    }

    /// Records a file or directory the host created at the path [`Vfs::overlay_path_for`] gave
    /// (a hard link, a copy).
    pub fn note_created(&self, path: &str) -> Result<()> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let name = cp.parts.last().ok_or_else(|| Error::InvalidPath(path.to_owned()))?.clone();
        let mut ov = write_lock(d);
        let host = Self::overlay_dir(d, &mut ov, &cp.parent())?.join(&name);
        let is_dir = fs::metadata(&host)?.is_dir();
        let key = cp.key();
        ov.insert(&key, ONode { host: host.clone(), name, is_dir });
        if is_dir {
            ov.index(&host, &key)?;
        }
        ov.unwhiteout(&key)?;
        Ok(())
    }

    /// The console path of a host path inside a drive's overlay (`None` elsewhere, such as the base).
    pub fn console_path_of(&self, host: &Path) -> Option<String> {
        fn plain(p: &Path) -> String {
            let s = p.to_string_lossy();
            let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
            s.trim_end_matches('\\').to_ascii_lowercase()
        }
        let want = fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf());
        let want_plain = plain(&want);
        let original = want.to_string_lossy().into_owned();
        let original = original.strip_prefix(r"\\?\").unwrap_or(&original).to_owned();
        for d in &self.drives {
            let root = read_lock(d).dir_host("")?;
            let root = plain(&fs::canonicalize(&root).unwrap_or(root));
            if want_plain == root {
                return Some(format!("{}:\\", d.letter));
            }
            if let Some(rest) = want_plain.strip_prefix(&format!("{root}\\")) {
                // keep the disk's case
                let rest = &original[original.len() - rest.len()..];
                return Some(format!("{}:\\{rest}", d.letter));
            }
        }
        None
    }

    /// Makes the directory at `path` exist in the overlay (so its own attributes or a write handle
    /// land there, never below); returns its overlay host path.
    pub fn materialize_dir(&self, path: &str) -> Result<PathBuf> {
        self.settle();
        let (d, cp) = self.parse(path)?;
        let mut ov = write_lock(d);
        let found = Self::find(d, &ov, &cp).ok_or_else(|| Self::missing(d, &ov, &cp))?;
        if !found.is_dir {
            return Err(Error::NotADirectory);
        }
        Self::overlay_dir(d, &mut ov, &cp)
    }
}

/// Copies a file out of a read-only layer and clears the read-only attribute on the copy.
fn copy_writable(from: &Path, to: &Path) -> io::Result<()> {
    fs::copy(from, to)?;
    let mut perms = fs::metadata(to)?.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(to, perms)?;
    }
    Ok(())
}

#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

/// Overlapped reads on a handle opened with `FILE_FLAG_OVERLAPPED` (no crate needed: kernel32 is always there).
#[cfg(windows)]
mod concurrent {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const ERROR_IO_PENDING: i32 = 997;
    const ERROR_HANDLE_EOF: i32 = 38;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn ReadFile(file: *mut c_void, buf: *mut u8, len: u32, read: *mut u32, ov: *mut Overlapped) -> i32;
        fn GetOverlappedResult(file: *mut c_void, ov: *mut Overlapped, read: *mut u32, wait: i32) -> i32;
        fn CreateEventW(attrs: *mut c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
    }

    /// One event per thread, reused: a thread has one read outstanding at a time.
    struct Event(*mut c_void);
    impl Drop for Event {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }
    thread_local! {
        static EVENT: Event = Event(unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) });
    }

    /// One read of up to `buf.len()` bytes at `offset`; 0 at the end of the file.
    pub fn read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let len = buf.len().min(1 << 30) as u32;
        EVENT.with(|e| {
            if e.0.is_null() {
                return Err(io::Error::last_os_error());
            }
            // the low bit of the event tells the system not to queue a completion packet to a port
            let mut ov = Overlapped { internal: 0, internal_high: 0, offset: offset as u32, offset_high: (offset >> 32) as u32, event: e.0 };
            let h = file.as_raw_handle() as *mut c_void;
            let mut got = 0u32;
            // SAFETY: `buf` and `ov` outlive the request: it is waited for below before either goes away.
            unsafe {
                if ReadFile(h, buf.as_mut_ptr(), len, &mut got, &mut ov) == 0 {
                    let err = io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(ERROR_IO_PENDING) => {}
                        Some(ERROR_HANDLE_EOF) => return Ok(0),
                        _ => return Err(err),
                    }
                }
                if GetOverlappedResult(h, &mut ov, &mut got, 1) == 0 {
                    let err = io::Error::last_os_error();
                    return if err.raw_os_error() == Some(ERROR_HANDLE_EOF) { Ok(0) } else { Err(err) };
                }
            }
            Ok(got as usize)
        })
    }
}

#[cfg(windows)]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(unix)]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(test)]
mod tests;
