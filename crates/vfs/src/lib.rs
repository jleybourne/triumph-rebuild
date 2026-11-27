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
}

#[derive(Default)]
struct Handles {
    slots: Vec<(u32, Option<Arc<OpenFile>>)>,
    free: Vec<u32>,
}

pub struct Vfs {
    drives: Vec<Drive>,
    handles: Mutex<Handles>,
}

/// `%APPDATA%\triumph-rebuild\overlay` (or `./overlay` without `APPDATA`).
pub fn default_overlay_root() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(appdata) => Path::new(&appdata).join("triumph-rebuild").join("overlay"),
        None => PathBuf::from("overlay"),
    }
}

pub struct VfsBuilder {
    overlay_root: PathBuf,
    drives: Vec<(char, Option<(PathBuf, ModSet)>)>,
}

impl VfsBuilder {
    pub fn new(overlay_root: impl Into<PathBuf>) -> Self {
        Self { overlay_root: overlay_root.into(), drives: Vec::new() }
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

    pub fn build(self) -> Result<Vfs> {
        let mut drives = Vec::new();
        for (letter, base) in self.drives {
            if !letter.is_ascii_alphabetic() {
                return Err(Error::NoDrive(letter));
            }
            if drives.iter().any(|d: &Drive| d.letter == letter) {
                return Err(Error::Unsafe(format!("drive {letter}: mounted twice")));
            }
            let dir = self.overlay_root.join(letter.to_string());
            fs::create_dir_all(&dir)?;
            let overlay_real = fs::canonicalize(&dir)?;
            let mut lower = Vec::new();
            if let Some((base, mods)) = base {
                let mut roots: Vec<(Layer, PathBuf)> =
                    mods.enabled().map(|m| (Layer::Mod(m.name.clone()), m.root.clone())).collect();
                roots.reverse();
                roots.push((Layer::Base, base));
                for (layer, root) in roots {
                    let real = fs::canonicalize(&root)?;
                    if overlay_real.starts_with(&real) || real.starts_with(&overlay_real) {
                        return Err(Error::Unsafe(format!(
                            "overlay {} and {:?} layer {} overlap",
                            dir.display(),
                            layer,
                            root.display()
                        )));
                    }
                    lower.push((layer, ReadLayer::new(real)));
                }
            }
            drives.push(Drive { letter, lower, overlay: RwLock::new(Overlay::open(&dir)?) });
        }
        Ok(Vfs { drives, handles: Mutex::new(Handles::default()) })
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
        let mut h = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        let open = Some(Arc::new(OpenFile { file, write }));
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

    /// Reads at `offset` until `buf` is full or the file ends; returns the bytes read.
    pub fn read_at(&self, handle: Handle, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let f = self.get(handle)?;
        let mut done = 0;
        while done < buf.len() {
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
