//! XGameSave offline: the signed-in user's game saves (containers of named blobs), kept on the host
//! in the profile's saves folder (the game's `saves\` from `rebuild_storage`, beside its storage folder), one folder per
//! user and service configuration. Nothing goes to a cloud: every container is in sync, `needsSync` is
//! always false, and no call ever conflicts or asks the user anything.
//!
//! A container is one file (`<name, escaped>.gsc`: its exact name, display name, modification time, a
//! blob index, then the blobs' bytes). An update is all or nothing, as documented: the whole new
//! container is written beside the old one, flushed, and renamed over it, so a crash leaves the old
//! container or the new one. A container exists once its first update is submitted (documented: "not
//! finalized until" then).
//!
//! The slots are the title's. Its 20 thunks (`QueryApiImpl(704c3f58…, 1bfff3af…)` then one slot each;
//! image `0x1455df210`-`0x1455dfcf4`) call 0x20-0x30, 0x40, 0x48, 0x58-0x70, 0x80, 0x88, 0x98-0xc8,
//! 0xe0 and 0xe8, and each passes the public API's arguments in order (a `bool` third to 0x20, a
//! 32-bit count third to 0xa8, one argument to the close slots). The rest (0x18, 0x38, 0x50, 0x78,
//! 0x90, 0xd0, 0xd8) are the public API's remaining functions in the same pattern (the blocking form
//! before each `Async` / `Result` pair, the by-name enumeration after the full one): served, not seen
//! called. Asynchronous calls do their file work on the block's work port.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put, text, InterfaceDef};
use crate::profile;
use crate::xasync::{self, ProviderData, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("704c3f58-e629-4cc2-b197-30511b996fe2");
/// The interface id this title asks with (its run report; its thunks).
pub const IID: Guid = Guid::parse("1bfff3af-f14a-40a3-8e35-9ada906593f9");

/// Bytes of blob data one user may keep per title (the GDK's connected storage quota: 256 MB).
pub const QUOTA: u64 = 256 << 20;
/// `GS_MAX_BLOB_SIZE`: one blob, and one whole update (documented), at most 16 MB.
pub const MAX_UPDATE: u64 = 16 << 20;
/// Container names: 256 characters at most (documented).
const MAX_NAME: usize = 256;

// The public structures.

/// `XGameSaveContainerInfo`, 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ContainerInfo {
    pub name: *const c_char,
    pub display_name: *const c_char,
    pub blob_count: u32,
    pub total_size: u64,
    /// `time_t`: seconds since 1970.
    pub last_modified_time: i64,
    pub needs_sync: bool,
}

/// `XGameSaveBlobInfo`, 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BlobInfo {
    pub name: *const c_char,
    pub size: u32,
}

/// `XGameSaveBlob`, 24 bytes: its info, then its bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Blob {
    pub info: BlobInfo,
    pub data: *mut u8,
}

/// `XGameSaveContainerInfoCallback`: true to go on.
pub type ContainerCallback = unsafe extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
/// `XGameSaveBlobInfoCallback`: true to go on.
pub type BlobCallback = unsafe extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

// Names.

/// Documented: the path part (up to and including the last `/`) letters, digits, `_` and `/`; the file
/// part letters, digits, `_`, `.` and `-`, not empty, not ending in `.`, no `..`; 256 characters at most.
pub fn valid_container_name(name: &str) -> bool {
    let (path, file) = match name.rfind('/') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => ("", name),
    };
    name.len() <= MAX_NAME
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
        && !file.is_empty()
        && !file.ends_with('.')
        && !file.contains("..")
        && file.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A name as one host file name: lower-case letters, digits, `_` and `-` stay; every other byte
/// (capitals too, so names differing in case never share a file on a case-blind disk) becomes `%XX`.
/// Long results keep a readable start and end in `~` and a hash of the exact name.
fn escaped(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    if out.len() > 150 {
        // 64-bit FNV-1a: stable across builds and platforms.
        let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        let mut cut = 120;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out = format!("{}~{hash:016x}", &out[..cut]);
    }
    out
}

const EXT: &str = "gsc";
const MAGIC: &[u8; 8] = b"RBGSAVE1";

fn container_file(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.{EXT}", escaped(name)))
}

// Containers on disk.

/// A stored container, its blobs in the order first written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stored {
    pub name: String,
    pub display: String,
    pub modified: i64,
    pub blobs: Vec<(String, Vec<u8>)>,
}

/// A stored container without its bytes: (blob name, size).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub display: String,
    pub modified: i64,
    pub blobs: Vec<(String, u64)>,
}

impl Index {
    fn total(&self) -> u64 {
        self.blobs.iter().map(|b| b.1).sum()
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn encode(c: &Stored) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&c.modified.to_le_bytes());
    put_str(&mut out, &c.name);
    put_str(&mut out, &c.display);
    out.extend_from_slice(&(c.blobs.len() as u32).to_le_bytes());
    for (name, data) in &c.blobs {
        put_str(&mut out, name);
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    }
    for (_, data) in &c.blobs {
        out.extend_from_slice(data);
    }
    out
}

fn bad(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_str(r: &mut impl Read) -> std::io::Result<String> {
    let n = read_u32(r)? as usize;
    if n > 4096 {
        return Err(bad("name too long"));
    }
    let mut b = vec![0; n];
    r.read_exact(&mut b)?;
    String::from_utf8(b).map_err(|_| bad("name not UTF-8"))
}

fn read_index(r: &mut impl Read) -> std::io::Result<Index> {
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("not a container"));
    }
    let modified = read_u64(r)? as i64;
    let name = read_str(r)?;
    let display = read_str(r)?;
    let count = read_u32(r)?;
    let mut blobs = Vec::new();
    for _ in 0..count {
        let blob = read_str(r)?;
        let size = read_u64(r)?;
        if size > MAX_UPDATE {
            return Err(bad("blob too big"));
        }
        blobs.push((blob, size));
    }
    Ok(Index { name, display, modified, blobs })
}

/// The container in `path`, without its bytes; `None` if there is none (or it is not one of ours).
fn load_index(path: &Path) -> Option<Index> {
    let f = std::fs::File::open(path).ok()?;
    read_index(&mut std::io::BufReader::new(f)).ok()
}

fn load(path: &Path) -> Option<Stored> {
    let f = std::fs::File::open(path).ok()?;
    let mut r = std::io::BufReader::new(f);
    let index = read_index(&mut r).ok()?;
    let mut blobs = Vec::new();
    for (name, size) in index.blobs {
        let mut data = vec![0; size as usize];
        r.read_exact(&mut data).ok()?;
        blobs.push((name, data));
    }
    Some(Stored { name: index.name, display: index.display, modified: index.modified, blobs })
}

/// Writes the whole container beside its file, flushed, then renames it over: all or nothing.
fn store(path: &Path, c: &Stored) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{EXT}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&encode(c))?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// The container file for tools that read and write saves while the title is not running (the
/// launcher's save editor, `crates/saves`): the runtime's own encoding, file naming and all-or-nothing
/// write, so a tool can never produce a file the runtime reads differently.
pub mod file {
    use super::{disk, read_index, store, Stored};
    use std::path::{Path, PathBuf};

    /// The container files' extension.
    pub const EXTENSION: &str = super::EXT;

    /// The file's bytes for `c`, exactly as the runtime writes them.
    pub fn encode(c: &Stored) -> Vec<u8> {
        super::encode(c)
    }

    /// One whole container file: refused if anything is malformed, or if bytes follow its last blob.
    pub fn decode(bytes: &[u8]) -> std::io::Result<Stored> {
        let mut r = bytes;
        let index = read_index(&mut r)?;
        let mut blobs = Vec::with_capacity(index.blobs.len());
        for (name, size) in index.blobs {
            let size = usize::try_from(size).map_err(|_| super::bad("blob too big"))?;
            if r.len() < size {
                return Err(super::bad("file ends inside a blob"));
            }
            let (data, rest) = r.split_at(size);
            blobs.push((name, data.to_vec()));
            r = rest;
        }
        if !r.is_empty() {
            return Err(super::bad("bytes after the last blob"));
        }
        Ok(Stored { name: index.name, display: index.display, modified: index.modified, blobs })
    }

    /// The file the runtime keeps container `name` in, inside a provider folder.
    pub fn path(dir: &Path, name: &str) -> PathBuf {
        super::container_file(dir, name)
    }

    /// Writes `c` to `path` as the runtime does: beside it, flushed, renamed over (all or nothing).
    pub fn write(path: &Path, c: &Stored) -> std::io::Result<()> {
        let _disk = disk();
        store(path, c)
    }
}

/// Every container in `dir`, by name.
fn containers(dir: &Path) -> Vec<Index> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Index> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == EXT))
        .filter_map(|e| load_index(&e.path()))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn used(dir: &Path) -> u64 {
    containers(dir).iter().map(Index::total).sum()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Every file operation, one at a time, so none sees another's half-made container.
static DISK: Mutex<()> = Mutex::new(());

fn disk() -> MutexGuard<'static, ()> {
    DISK.lock().unwrap_or_else(|e| e.into_inner())
}

// Handles.

#[derive(Clone, Debug)]
struct Container {
    dir: PathBuf,
    name: String,
}

#[derive(Clone, Debug)]
struct Update {
    container: Container,
    display: Option<String>,
    /// (blob, bytes to write or `None` to delete), one per blob.
    ops: Vec<(String, Option<Vec<u8>>)>,
}

#[derive(Default)]
struct Handles {
    next: usize,
    providers: HashMap<usize, PathBuf>,
    containers: HashMap<usize, Container>,
    updates: HashMap<usize, Update>,
}

static HANDLES: Mutex<Option<Handles>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Handles) -> R) -> R {
    let mut h = HANDLES.lock().unwrap_or_else(|e| e.into_inner());
    f(h.get_or_insert_with(Handles::default))
}

/// A new handle value: never null, never reused.
fn fresh(h: &mut Handles) -> usize {
    h.next += 1;
    0x6a5e_0000 + h.next * 0x10
}

fn provider_dir(handle: *const c_void) -> Option<PathBuf> {
    with(|h| h.providers.get(&(handle as usize)).cloned())
}

fn container_of(handle: *const c_void) -> Option<Container> {
    with(|h| h.containers.get(&(handle as usize)).cloned())
}

/// The folder for the signed-in user and `scid`, made if missing; leftovers of an interrupted update
/// go. The title may pass an empty configuration id; offline it still gets its own folder.
fn open_provider(scid: &str) -> Result<PathBuf, HRESULT> {
    let p = profile::current();
    let Some(root) = p.saves_folder() else { return Err(E_GS_NO_ACCESS) };
    let scid = if scid.is_empty() { "default" } else { scid };
    let dir = root.join(format!("{:016x}", p.xuid)).join(escaped(scid));
    let _disk = disk();
    std::fs::create_dir_all(&dir).map_err(|_| E_GS_NO_ACCESS)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().to_string_lossy().ends_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(dir)
}

fn new_provider(dir: PathBuf) -> usize {
    with(|h| {
        let id = fresh(h);
        h.providers.insert(id, dir);
        id
    })
}

unsafe fn check_name(name: *const c_char) -> Result<String, HRESULT> {
    let Some(name) = (unsafe { text(name) }) else { return Err(E_INVALIDARG) };
    if !valid_container_name(&name) {
        crate::discovery::note(format!("game save: invalid container name {name:?}"));
        return Err(E_GS_INVALID_CONTAINER_NAME);
    }
    Ok(name)
}

// Operations, as the slots and their asynchronous work share them.

fn remaining(dir: &Path) -> i64 {
    let _disk = disk();
    QUOTA.saturating_sub(used(dir)) as i64
}

fn delete(dir: &Path, name: &str) -> HRESULT {
    let _disk = disk();
    match std::fs::remove_file(container_file(dir, name)) {
        Ok(()) => S_OK,
        // Deleting what is not there leaves what was asked for.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => S_OK,
        Err(_) => E_GS_NO_ACCESS,
    }
}

/// The blobs `names` asks for (`None`: all), in that order.
fn read(c: &Container, names: Option<Vec<String>>) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
    let stored = {
        let _disk = disk();
        load(&container_file(&c.dir, &c.name))
    };
    let blobs = stored.map(|s| s.blobs).unwrap_or_default();
    match names {
        None => Ok(blobs),
        Some(names) => names
            .into_iter()
            .map(|n| blobs.iter().find(|b| b.0 == n).cloned().ok_or(E_GS_BLOB_NOT_FOUND))
            .collect(),
    }
}

fn submit(u: Update) -> HRESULT {
    let written: u64 = u.ops.iter().filter_map(|o| o.1.as_ref()).map(|d| d.len() as u64).sum();
    if written > MAX_UPDATE {
        return E_GS_UPDATE_TOO_BIG;
    }
    let dir = &u.container.dir;
    let _disk = disk();
    let path = container_file(dir, &u.container.name);
    let old = load(&path);
    let old_total = old.as_ref().map_or(0, |s| s.blobs.iter().map(|b| b.1.len() as u64).sum());
    let mut c = old.unwrap_or_else(|| Stored { name: u.container.name.clone(), ..Stored::default() });
    for (name, data) in u.ops {
        let at = c.blobs.iter().position(|b| b.0 == name);
        match (data, at) {
            (Some(d), Some(i)) => c.blobs[i].1 = d,
            (Some(d), None) => c.blobs.push((name, d)),
            (None, Some(i)) => drop(c.blobs.remove(i)),
            (None, None) => {}
        }
    }
    let new_total: u64 = c.blobs.iter().map(|b| b.1.len() as u64).sum();
    if used(dir) - old_total + new_total > QUOTA {
        return E_GS_QUOTA_EXCEEDED;
    }
    if let Some(d) = u.display.filter(|d| !d.is_empty()) {
        c.display = d;
    }
    if c.display.is_empty() {
        c.display = c.name.clone();
    }
    c.modified = now();
    match store(&path, &c) {
        Ok(()) => S_OK,
        Err(_) => E_GS_NO_ACCESS,
    }
}

/// `XGameSaveBlob`s, then each blob's name (with its NUL) and bytes, as the title sizes the buffer:
/// 24 bytes, the name's length + 1 and the blob's size, per blob.
fn layout_size(blobs: &[(String, Vec<u8>)]) -> usize {
    blobs.iter().map(|(n, d)| size_of::<Blob>() + n.len() + 1 + d.len()).sum()
}

/// # Safety
/// `buffer` holds `layout_size(blobs)` bytes.
unsafe fn lay_out(blobs: &[(String, Vec<u8>)], buffer: *mut u8) {
    let heads = buffer.cast::<Blob>();
    let mut at = unsafe { buffer.add(size_of::<Blob>() * blobs.len()) };
    for (i, (name, data)) in blobs.iter().enumerate() {
        unsafe {
            let name_at = at;
            name_at.copy_from_nonoverlapping(name.as_ptr(), name.len());
            name_at.add(name.len()).write(0);
            let data_at = name_at.add(name.len() + 1);
            data_at.copy_from_nonoverlapping(data.as_ptr(), data.len());
            at = data_at.add(data.len());
            let blob = Blob { info: BlobInfo { name: name_at.cast(), size: data.len() as u32 }, data: data_at };
            heads.add(i).write_unaligned(blob);
        }
    }
}

// Asynchronous calls: the work runs on the block's work port, then the call completes.

enum Out {
    Bytes(Vec<u8>),
    Blobs(Vec<(String, Vec<u8>)>),
}

impl Out {
    fn size(&self) -> usize {
        match self {
            Out::Bytes(b) => b.len(),
            Out::Blobs(b) if b.is_empty() => 0,
            Out::Blobs(b) => layout_size(b),
        }
    }
}

type Work = Box<dyn FnOnce() -> Result<Out, HRESULT> + Send>;

struct Job {
    work: Option<Work>,
    out: Option<Out>,
}

unsafe extern "system" fn job_provider(op: u32, data: *const ProviderData) -> HRESULT {
    let data = unsafe { &*data };
    let job = data.context.cast::<Job>();
    match op {
        xasync::OP_BEGIN => unsafe { xasync::schedule(data.async_block, 0) },
        xasync::OP_DO_WORK => {
            let Some(work) = (unsafe { (*job).work.take() }) else { return S_OK };
            match work() {
                Ok(out) => {
                    let size = out.size();
                    unsafe { (*job).out = Some(out) };
                    unsafe { xasync::complete(data.async_block, S_OK, size) };
                    E_PENDING
                }
                Err(hr) => hr,
            }
        }
        xasync::OP_GET_RESULT => {
            match unsafe { &(*job).out } {
                Some(Out::Bytes(b)) => unsafe { data.buffer.cast::<u8>().copy_from_nonoverlapping(b.as_ptr(), b.len()) },
                Some(Out::Blobs(b)) => unsafe { lay_out(b, data.buffer.cast()) },
                None => {}
            }
            S_OK
        }
        // SAFETY: boxed in `start`; nothing runs after cleanup.
        xasync::OP_CLEANUP => {
            drop(unsafe { Box::from_raw(job) });
            S_OK
        }
        // File work is short and not interruptible: it finishes.
        _ => S_OK,
    }
}

unsafe fn start(block: *mut XAsyncBlock, identity: &'static u8, work: Work) -> HRESULT {
    let job = Box::into_raw(Box::new(Job { work: Some(work), out: None }));
    let hr = unsafe { xasync::begin(block, job.cast(), (identity as *const u8).cast(), Some(job_provider)) };
    if failed(hr) {
        // Refused before the call existed: no cleanup will come.
        drop(unsafe { Box::from_raw(job) });
    }
    hr
}

unsafe fn result<T>(block: *mut XAsyncBlock, identity: &'static u8, out: *mut T) -> HRESULT {
    unsafe { xasync::get_result(block, (identity as *const u8).cast(), size_of::<T>(), out.cast(), std::ptr::null_mut()) }
}

unsafe fn status_result(block: *mut XAsyncBlock, identity: &'static u8) -> HRESULT {
    unsafe { xasync::get_result(block, (identity as *const u8).cast(), 0, std::ptr::null_mut(), std::ptr::null_mut()) }
}

static INIT: u8 = 0;
static QUOTA_CALL: u8 = 0;
static DELETE: u8 = 0;
static READ: u8 = 0;
static SUBMIT: u8 = 0;

// The slots, in table order.

type Handle = *const c_void;

/// `XGameSaveInitializeProvider(user, configurationId, syncOnDemand, *provider)`. Offline there is
/// nothing to sync, so `syncOnDemand` changes nothing.
unsafe extern "system" fn initialize(_: This, user: Handle, scid: *const c_char, _sync_on_demand: bool, out: *mut Handle) -> HRESULT {
    if user.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let scid = unsafe { text(scid) }.unwrap_or_default();
    match open_provider(&scid) {
        Ok(dir) => unsafe { put(out, new_provider(dir) as Handle) },
        Err(hr) => hr,
    }
}

/// `XGameSaveInitializeProviderAsync(user, configurationId, syncOnDemand, block)`.
unsafe extern "system" fn initialize_async(_: This, user: Handle, scid: *const c_char, _sync_on_demand: bool, block: *mut XAsyncBlock) -> HRESULT {
    if user.is_null() || block.is_null() {
        return E_INVALIDARG;
    }
    let scid = unsafe { text(scid) }.unwrap_or_default();
    let work: Work = Box::new(move || open_provider(&scid).map(|dir| Out::Bytes(new_provider(dir).to_ne_bytes().to_vec())));
    unsafe { start(block, &INIT, work) }
}

/// `XGameSaveInitializeProviderResult(block, *provider)`.
unsafe extern "system" fn initialize_result(_: This, block: *mut XAsyncBlock, out: *mut Handle) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { result(block, &INIT, out) }
}

/// `XGameSaveCloseProvider(provider)`.
unsafe extern "system" fn close_provider(_: This, provider: Handle) -> HRESULT {
    with(|h| h.providers.remove(&(provider as usize)));
    S_OK
}

/// `XGameSaveGetRemainingQuota(provider, *remainingQuota: i64)`.
unsafe extern "system" fn quota(_: This, provider: Handle, out: *mut i64) -> HRESULT {
    let Some(dir) = provider_dir(provider) else { return E_INVALIDARG };
    unsafe { put(out, remaining(&dir)) }
}

/// `XGameSaveGetRemainingQuotaAsync(provider, block)`.
unsafe extern "system" fn quota_async(_: This, provider: Handle, block: *mut XAsyncBlock) -> HRESULT {
    let Some(dir) = provider_dir(provider) else { return E_INVALIDARG };
    unsafe { start(block, &QUOTA_CALL, Box::new(move || Ok(Out::Bytes(remaining(&dir).to_ne_bytes().to_vec())))) }
}

/// `XGameSaveGetRemainingQuotaResult(block, *remainingQuota)`.
unsafe extern "system" fn quota_result(_: This, block: *mut XAsyncBlock, out: *mut i64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { result(block, &QUOTA_CALL, out) }
}

/// `XGameSaveDeleteContainer(provider, containerName)`: deleting one that is not there succeeds.
unsafe extern "system" fn delete_container(_: This, provider: Handle, name: *const c_char) -> HRESULT {
    let Some(dir) = provider_dir(provider) else { return E_INVALIDARG };
    match unsafe { check_name(name) } {
        Ok(name) => delete(&dir, &name),
        Err(hr) => hr,
    }
}

/// `XGameSaveDeleteContainerAsync(provider, containerName, block)`.
unsafe extern "system" fn delete_async(_: This, provider: Handle, name: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    let Some(dir) = provider_dir(provider) else { return E_INVALIDARG };
    let name = match unsafe { check_name(name) } {
        Ok(name) => name,
        Err(hr) => return hr,
    };
    let work: Work = Box::new(move || match delete(&dir, &name) {
        S_OK => Ok(Out::Bytes(Vec::new())),
        hr => Err(hr),
    });
    unsafe { start(block, &DELETE, work) }
}

/// `XGameSaveDeleteContainerResult(block)`.
unsafe extern "system" fn delete_result(_: This, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { status_result(block, &DELETE) }
}

/// Calls `callback` for each container (stopping when it returns false), with no lock held: the
/// title may call back in.
unsafe fn report_containers(list: Vec<Index>, context: *mut c_void, callback: ContainerCallback) -> HRESULT {
    for c in list {
        let (Ok(name), Ok(display)) = (CString::new(c.name.clone()), CString::new(c.display.clone())) else { continue };
        let info = ContainerInfo {
            name: name.as_ptr(),
            display_name: display.as_ptr(),
            blob_count: c.blobs.len() as u32,
            total_size: c.total(),
            last_modified_time: c.modified,
            // Offline: always in sync.
            needs_sync: false,
        };
        if !unsafe { callback(&info, context) } {
            break;
        }
    }
    S_OK
}

/// `XGameSaveGetContainerInfo(provider, containerName, context, callback)`: the callback once if the
/// container exists, never if not (the call succeeds either way).
unsafe extern "system" fn container_info(_: This, provider: Handle, name: *const c_char, context: *mut c_void, callback: Option<ContainerCallback>) -> HRESULT {
    let (Some(dir), Some(callback)) = (provider_dir(provider), callback) else { return E_INVALIDARG };
    let name = match unsafe { check_name(name) } {
        Ok(name) => name,
        Err(hr) => return hr,
    };
    let found = {
        let _disk = disk();
        load_index(&container_file(&dir, &name)).filter(|i| i.name == name)
    };
    unsafe { report_containers(found.into_iter().collect(), context, callback) }
}

unsafe fn enumerate(provider: Handle, prefix: &str, context: *mut c_void, callback: Option<ContainerCallback>) -> HRESULT {
    let (Some(dir), Some(callback)) = (provider_dir(provider), callback) else { return E_INVALIDARG };
    let list: Vec<Index> = {
        let _disk = disk();
        containers(&dir).into_iter().filter(|c| c.name.starts_with(prefix)).collect()
    };
    unsafe { report_containers(list, context, callback) }
}

/// `XGameSaveEnumerateContainerInfo(provider, context, callback)`: every container, by name.
unsafe extern "system" fn enumerate_containers(_: This, provider: Handle, context: *mut c_void, callback: Option<ContainerCallback>) -> HRESULT {
    unsafe { enumerate(provider, "", context, callback) }
}

/// `XGameSaveEnumerateContainerInfoByName(provider, containerNamePrefix, context, callback)`.
unsafe extern "system" fn enumerate_containers_by_name(_: This, provider: Handle, prefix: *const c_char, context: *mut c_void, callback: Option<ContainerCallback>) -> HRESULT {
    let prefix = unsafe { text(prefix) }.unwrap_or_default();
    unsafe { enumerate(provider, &prefix, context, callback) }
}

/// `XGameSaveCreateContainer(provider, containerName, *container)`: a handle to the container by that
/// name, which need not exist yet (it does once an update is submitted).
unsafe extern "system" fn create_container(_: This, provider: Handle, name: *const c_char, out: *mut Handle) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { out.write_unaligned(std::ptr::null()) };
    let Some(dir) = provider_dir(provider) else { return E_INVALIDARG };
    let name = match unsafe { check_name(name) } {
        Ok(name) => name,
        Err(hr) => return hr,
    };
    let id = with(|h| {
        let id = fresh(h);
        h.containers.insert(id, Container { dir, name });
        id
    });
    unsafe { put(out, id as Handle) }
}

/// `XGameSaveCloseContainer(container)`.
unsafe extern "system" fn close_container(_: This, container: Handle) -> HRESULT {
    with(|h| h.containers.remove(&(container as usize)));
    S_OK
}

unsafe fn enumerate_blobs(container: Handle, prefix: &str, context: *mut c_void, callback: Option<BlobCallback>) -> HRESULT {
    let (Some(c), Some(callback)) = (container_of(container), callback) else { return E_INVALIDARG };
    let index = {
        let _disk = disk();
        load_index(&container_file(&c.dir, &c.name))
    };
    for (name, size) in index.map(|i| i.blobs).unwrap_or_default() {
        if !name.starts_with(prefix) {
            continue;
        }
        let Ok(cname) = CString::new(name) else { continue };
        let info = BlobInfo { name: cname.as_ptr(), size: size as u32 };
        if !unsafe { callback(&info, context) } {
            break;
        }
    }
    S_OK
}

/// `XGameSaveEnumerateBlobInfo(container, context, callback)`.
unsafe extern "system" fn enumerate_all_blobs(_: This, container: Handle, context: *mut c_void, callback: Option<BlobCallback>) -> HRESULT {
    unsafe { enumerate_blobs(container, "", context, callback) }
}

/// `XGameSaveEnumerateBlobInfoByName(container, blobNamePrefix, context, callback)`.
unsafe extern "system" fn enumerate_blobs_by_name(_: This, container: Handle, prefix: *const c_char, context: *mut c_void, callback: Option<BlobCallback>) -> HRESULT {
    let prefix = unsafe { text(prefix) }.unwrap_or_default();
    unsafe { enumerate_blobs(container, &prefix, context, callback) }
}

/// The names the title asks for; `None` (all blobs) for a null list or a count of 0.
unsafe fn names(list: *const *const c_char, count: u32) -> Result<Option<Vec<String>>, HRESULT> {
    if list.is_null() || count == 0 {
        return Ok(None);
    }
    (0..count as usize)
        .map(|i| unsafe { text(*list.add(i)) }.ok_or(E_INVALIDARG))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// `XGameSaveReadBlobData(container, blobNames, *countOfBlobs (in: names, out: blobs), blobsSize,
/// *blobData)`.
unsafe extern "system" fn read_blobs(_: This, container: Handle, list: *const *const c_char, count: *mut u32, size: usize, buffer: *mut Blob) -> HRESULT {
    let Some(c) = container_of(container) else { return E_INVALIDARG };
    if count.is_null() {
        return E_INVALIDARG;
    }
    let wanted = match unsafe { names(list, count.read_unaligned()) } {
        Ok(w) => w,
        Err(hr) => return hr,
    };
    let blobs = match read(&c, wanted) {
        Ok(b) => b,
        Err(hr) => return hr,
    };
    if !blobs.is_empty() {
        if buffer.is_null() {
            return E_INVALIDARG;
        }
        if size < layout_size(&blobs) {
            return E_GS_PROVIDED_BUFFER_TOO_SMALL;
        }
        unsafe { lay_out(&blobs, buffer.cast()) };
    }
    unsafe { count.write_unaligned(blobs.len() as u32) };
    S_OK
}

/// `XGameSaveReadBlobDataAsync(container, blobNames, countOfBlobs, block)`: the result's size is what
/// the blobs take laid out (`XAsyncGetResultSize`).
unsafe extern "system" fn read_async(_: This, container: Handle, list: *const *const c_char, count: u32, block: *mut XAsyncBlock) -> HRESULT {
    let Some(c) = container_of(container) else { return E_INVALIDARG };
    let wanted = match unsafe { names(list, count) } {
        Ok(w) => w,
        Err(hr) => return hr,
    };
    unsafe { start(block, &READ, Box::new(move || read(&c, wanted).map(Out::Blobs))) }
}

/// `XGameSaveReadBlobDataResult(block, blobsSize, *blobData, *countOfBlobs)`. The count is how many
/// `XGameSaveBlob`s precede the first name in the buffer.
unsafe extern "system" fn read_result(_: This, block: *mut XAsyncBlock, size: usize, buffer: *mut Blob, count: *mut u32) -> HRESULT {
    if count.is_null() {
        return E_INVALIDARG;
    }
    let mut need = 0usize;
    let status = unsafe { xasync::get_result_size(block, &mut need) };
    if failed(status) || status == E_PENDING {
        return status;
    }
    let hr = unsafe { xasync::get_result(block, (&READ as *const u8).cast(), size, buffer.cast(), std::ptr::null_mut()) };
    if hr == E_NOT_SUFFICIENT_BUFFER {
        return E_GS_PROVIDED_BUFFER_TOO_SMALL;
    }
    if failed(hr) {
        return hr;
    }
    let n = if need == 0 {
        0
    } else {
        let first = unsafe { buffer.read_unaligned() }.info.name as usize;
        ((first - buffer as usize) / size_of::<Blob>()) as u32
    };
    unsafe { count.write_unaligned(n) };
    hr
}

/// `XGameSaveCreateUpdate(container, containerDisplayName, *update)`.
unsafe extern "system" fn create_update(_: This, container: Handle, display: *const c_char, out: *mut Handle) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { out.write_unaligned(std::ptr::null()) };
    let Some(c) = container_of(container) else { return E_INVALIDARG };
    let display = unsafe { text(display) };
    let id = with(|h| {
        let id = fresh(h);
        h.updates.insert(id, Update { container: c, display, ops: Vec::new() });
        id
    });
    unsafe { put(out, id as Handle) }
}

/// `XGameSaveCloseUpdate(update)`: the update goes, submitted or not.
unsafe extern "system" fn close_update(_: This, update: Handle) -> HRESULT {
    with(|h| h.updates.remove(&(update as usize)));
    S_OK
}

/// Adds one blob's write or delete to the update. A second change to the same blob replaces the
/// first (the GDK allows one per blob per update; its code for a second is not documented).
fn add_op(update: Handle, name: String, data: Option<Vec<u8>>) -> HRESULT {
    with(|h| match h.updates.get_mut(&(update as usize)) {
        Some(u) => {
            u.ops.retain(|o| o.0 != name);
            u.ops.push((name, data));
            S_OK
        }
        None => E_INVALIDARG,
    })
}

/// `XGameSaveSubmitBlobWrite(update, blobName, data, byteCount)`: the bytes are copied now.
unsafe extern "system" fn write_blob(_: This, update: Handle, name: *const c_char, data: *const u8, size: usize) -> HRESULT {
    let Some(name) = (unsafe { text(name) }).filter(|n| !n.is_empty()) else { return E_INVALIDARG };
    if size as u64 > MAX_UPDATE {
        return E_GS_UPDATE_TOO_BIG;
    }
    if data.is_null() && size > 0 {
        return E_INVALIDARG;
    }
    let bytes = if size == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(data, size) }.to_vec() };
    add_op(update, name, Some(bytes))
}

/// `XGameSaveSubmitBlobDelete(update, blobName)`.
unsafe extern "system" fn delete_blob(_: This, update: Handle, name: *const c_char) -> HRESULT {
    let Some(name) = (unsafe { text(name) }).filter(|n| !n.is_empty()) else { return E_INVALIDARG };
    add_op(update, name, None)
}

fn update_of(update: Handle) -> Option<Update> {
    with(|h| h.updates.get(&(update as usize)).cloned())
}

/// `XGameSaveSubmitUpdate(update)`: all of it lands, or none.
unsafe extern "system" fn submit_update(_: This, update: Handle) -> HRESULT {
    match update_of(update) {
        Some(u) => submit(u),
        None => E_INVALIDARG,
    }
}

/// `XGameSaveSubmitUpdateAsync(update, block)`: the update as it is now.
unsafe extern "system" fn submit_async(_: This, update: Handle, block: *mut XAsyncBlock) -> HRESULT {
    let Some(u) = update_of(update) else { return E_INVALIDARG };
    let work: Work = Box::new(move || match submit(u) {
        S_OK => Ok(Out::Bytes(Vec::new())),
        hr => Err(hr),
    });
    unsafe { start(block, &SUBMIT, work) }
}

/// `XGameSaveSubmitUpdateResult(block)`.
unsafe extern "system" fn submit_result(_: This, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { status_result(block, &SUBMIT) }
}

pub static GAME_SAVE_IFACE: InterfaceDef = InterfaceDef {
    area: "game save",
    provider: PROVIDER,
    iids: &[IID, PROVIDER],
    len: 30,
    slots: &[
        slot!(0x18, "XGameSaveInitializeProvider", initialize, "user, configurationId: char*, syncOnDemand: bool, *provider -> HRESULT"),
        slot!(0x20, "XGameSaveInitializeProviderAsync", initialize_async, "user, configurationId: char*, syncOnDemand: bool, block -> HRESULT"),
        slot!(0x28, "XGameSaveInitializeProviderResult", initialize_result, "block, *provider -> HRESULT"),
        slot!(0x30, "XGameSaveCloseProvider", close_provider, "provider"),
        slot!(0x38, "XGameSaveGetRemainingQuota", quota, "provider, *remainingQuota: i64 -> HRESULT"),
        slot!(0x40, "XGameSaveGetRemainingQuotaAsync", quota_async, "provider, block -> HRESULT"),
        slot!(0x48, "XGameSaveGetRemainingQuotaResult", quota_result, "block, *remainingQuota: i64 -> HRESULT"),
        slot!(0x50, "XGameSaveDeleteContainer", delete_container, "provider, containerName: char* -> HRESULT"),
        slot!(0x58, "XGameSaveDeleteContainerAsync", delete_async, "provider, containerName: char*, block -> HRESULT"),
        slot!(0x60, "XGameSaveDeleteContainerResult", delete_result, "block -> HRESULT"),
        slot!(0x68, "XGameSaveGetContainerInfo", container_info, "provider, containerName: char*, context, callback -> HRESULT"),
        slot!(0x70, "XGameSaveEnumerateContainerInfo", enumerate_containers, "provider, context, callback -> HRESULT"),
        slot!(0x78, "XGameSaveEnumerateContainerInfoByName", enumerate_containers_by_name, "provider, containerNamePrefix: char*, context, callback -> HRESULT"),
        slot!(0x80, "XGameSaveCreateContainer", create_container, "provider, containerName: char*, *container -> HRESULT"),
        slot!(0x88, "XGameSaveCloseContainer", close_container, "container"),
        slot!(0x90, "XGameSaveEnumerateBlobInfo", enumerate_all_blobs, "container, context, callback -> HRESULT"),
        slot!(0x98, "XGameSaveEnumerateBlobInfoByName", enumerate_blobs_by_name, "container, blobNamePrefix: char*, context, callback -> HRESULT"),
        slot!(0xa0, "XGameSaveReadBlobData", read_blobs, "container, blobNames: char**, *countOfBlobs: u32, blobsSize, *blobData: XGameSaveBlob -> HRESULT"),
        slot!(0xa8, "XGameSaveReadBlobDataAsync", read_async, "container, blobNames: char**, countOfBlobs: u32, block -> HRESULT"),
        slot!(0xb0, "XGameSaveReadBlobDataResult", read_result, "block, blobsSize, *blobData: XGameSaveBlob, *countOfBlobs: u32 -> HRESULT"),
        slot!(0xb8, "XGameSaveCreateUpdate", create_update, "container, containerDisplayName: char*, *update -> HRESULT"),
        slot!(0xc0, "XGameSaveCloseUpdate", close_update, "update"),
        slot!(0xc8, "XGameSaveSubmitBlobWrite", write_blob, "update, blobName: char*, data: u8*, byteCount -> HRESULT"),
        slot!(0xd0, "XGameSaveSubmitBlobDelete", delete_blob, "update, blobName: char* -> HRESULT"),
        slot!(0xd8, "XGameSaveSubmitUpdate", submit_update, "update -> HRESULT"),
        slot!(0xe0, "XGameSaveSubmitUpdateAsync", submit_async, "update, block -> HRESULT"),
        slot!(0xe8, "XGameSaveSubmitUpdateResult", submit_result, "block -> HRESULT"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};
    use crate::profile::{Profile, TEST_LOCK};
    use crate::taskqueue::{self, Mode};

    type Init = unsafe extern "system" fn(This, Handle, *const c_char, bool, *mut Handle) -> HRESULT;
    type InitAsync = unsafe extern "system" fn(This, Handle, *const c_char, bool, *mut XAsyncBlock) -> HRESULT;
    type BlockOut<T> = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut T) -> HRESULT;
    type BlockOnly = unsafe extern "system" fn(This, *mut XAsyncBlock) -> HRESULT;
    type One = unsafe extern "system" fn(This, Handle) -> HRESULT;
    type HandleOut<T> = unsafe extern "system" fn(This, Handle, *mut T) -> HRESULT;
    type HandleBlock = unsafe extern "system" fn(This, Handle, *mut XAsyncBlock) -> HRESULT;
    type Named = unsafe extern "system" fn(This, Handle, *const c_char) -> HRESULT;
    type NamedBlock = unsafe extern "system" fn(This, Handle, *const c_char, *mut XAsyncBlock) -> HRESULT;
    type NamedOut = unsafe extern "system" fn(This, Handle, *const c_char, *mut Handle) -> HRESULT;
    type InfoOne = unsafe extern "system" fn(This, Handle, *const c_char, *mut c_void, Option<ContainerCallback>) -> HRESULT;
    type InfoAll = unsafe extern "system" fn(This, Handle, *mut c_void, Option<ContainerCallback>) -> HRESULT;
    type BlobsAll = unsafe extern "system" fn(This, Handle, *mut c_void, Option<BlobCallback>) -> HRESULT;
    type BlobsNamed = unsafe extern "system" fn(This, Handle, *const c_char, *mut c_void, Option<BlobCallback>) -> HRESULT;
    type Read = unsafe extern "system" fn(This, Handle, *const *const c_char, *mut u32, usize, *mut Blob) -> HRESULT;
    type ReadAsync = unsafe extern "system" fn(This, Handle, *const *const c_char, u32, *mut XAsyncBlock) -> HRESULT;
    type ReadResult = unsafe extern "system" fn(This, *mut XAsyncBlock, usize, *mut Blob, *mut u32) -> HRESULT;
    type Write = unsafe extern "system" fn(This, Handle, *const c_char, *const u8, usize) -> HRESULT;

    fn m<F: Copy>(offset: usize) -> F {
        method(&GAME_SAVE_IFACE, offset)
    }
    fn t() -> This {
        this(&GAME_SAVE_IFACE)
    }
    fn user() -> Handle {
        crate::areas::xuser::user()
    }

    /// A fresh saves folder for the test; the lock keeps the profile ours.
    fn folder(tag: &str) -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rebuild-gdk-saves-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        profile::configure(Profile { saves_host: Some(dir.clone()), ..Profile::default() }).unwrap();
        (guard, dir)
    }

    fn done(dir: &Path) {
        profile::configure(Profile::default()).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn provider() -> Handle {
        let mut p = std::ptr::null();
        assert_eq!(unsafe { m::<Init>(0x18)(t(), user(), c"scid-1".as_ptr(), false, &mut p) }, S_OK);
        assert!(!p.is_null());
        p
    }

    fn container(p: Handle, name: &std::ffi::CStr) -> Handle {
        let mut c = std::ptr::null();
        assert_eq!(unsafe { m::<NamedOut>(0x80)(t(), p, name.as_ptr(), &mut c) }, S_OK);
        c
    }

    /// Writes `blobs` to the container in one update, through the async submit.
    fn save(c: Handle, display: &std::ffi::CStr, blobs: &[(&std::ffi::CStr, &[u8])]) -> HRESULT {
        let mut u = std::ptr::null();
        assert_eq!(unsafe { m::<NamedOut>(0xb8)(t(), c, display.as_ptr(), &mut u) }, S_OK);
        for (name, data) in blobs {
            assert_eq!(unsafe { m::<Write>(0xc8)(t(), u, name.as_ptr(), data.as_ptr(), data.len()) }, S_OK);
        }
        let mut b = XAsyncBlock::new(std::ptr::null(), None);
        assert_eq!(unsafe { m::<HandleBlock>(0xe0)(t(), u, &mut b) }, S_OK);
        let hr = unsafe { xasync::get_status(&mut b, true) };
        assert_eq!(unsafe { m::<BlockOnly>(0xe8)(t(), &mut b) }, hr);
        assert_eq!(unsafe { m::<One>(0xc0)(t(), u) }, S_OK);
        hr
    }

    unsafe extern "system" fn collect_containers(info: *const ContainerInfo, ctx: *mut c_void) -> bool {
        let info = unsafe { &*info };
        let out = unsafe { &mut *ctx.cast::<Vec<(String, String, u32, u64, bool)>>() };
        let s = |p: *const c_char| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned();
        out.push((s(info.name), s(info.display_name), info.blob_count, info.total_size, info.needs_sync));
        assert!(info.last_modified_time > 1_600_000_000);
        true
    }

    unsafe extern "system" fn first_only(_: *const ContainerInfo, ctx: *mut c_void) -> bool {
        unsafe { *ctx.cast::<u32>() += 1 };
        false
    }

    unsafe extern "system" fn collect_blobs(info: *const BlobInfo, ctx: *mut c_void) -> bool {
        let info = unsafe { &*info };
        let out = unsafe { &mut *ctx.cast::<Vec<(String, u32)>>() };
        out.push((unsafe { std::ffi::CStr::from_ptr(info.name) }.to_string_lossy().into_owned(), info.size));
        true
    }

    fn infos(p: Handle) -> Vec<(String, String, u32, u64, bool)> {
        let mut out = Vec::new();
        let ctx = (&mut out as *mut Vec<_>).cast();
        assert_eq!(unsafe { m::<InfoAll>(0x70)(t(), p, ctx, Some(collect_containers)) }, S_OK);
        out
    }

    /// The blobs read back through the async read, as (name, bytes).
    fn read_back(c: Handle, names: &[&std::ffi::CStr]) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
        let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let list = if ptrs.is_empty() { std::ptr::null() } else { ptrs.as_ptr() };
        let mut b = XAsyncBlock::new(std::ptr::null(), None);
        assert_eq!(unsafe { m::<ReadAsync>(0xa8)(t(), c, list, ptrs.len() as u32, &mut b) }, S_OK);
        let hr = unsafe { xasync::get_status(&mut b, true) };
        if failed(hr) {
            assert_eq!(unsafe { m::<ReadResult>(0xb0)(t(), &mut b, 0, std::ptr::null_mut(), &mut 0) }, hr);
            return Err(hr);
        }
        let mut size = 0;
        assert_eq!(unsafe { xasync::get_result_size(&mut b, &mut size) }, S_OK);
        let mut buf = vec![0u64; size.div_ceil(8).max(1)];
        let mut count = 99;
        assert_eq!(unsafe { m::<ReadResult>(0xb0)(t(), &mut b, size, buf.as_mut_ptr().cast(), &mut count) }, S_OK);
        Ok(unsafe { blobs_in(buf.as_ptr().cast(), count) })
    }

    unsafe fn blobs_in(buf: *const Blob, count: u32) -> Vec<(String, Vec<u8>)> {
        (0..count as usize)
            .map(|i| {
                let blob = unsafe { buf.add(i).read_unaligned() };
                let name = unsafe { std::ffi::CStr::from_ptr(blob.info.name) }.to_string_lossy().into_owned();
                (name, unsafe { std::slice::from_raw_parts(blob.data, blob.info.size as usize) }.to_vec())
            })
            .collect()
    }

    #[test]
    fn layouts() {
        assert_eq!(size_of::<ContainerInfo>(), 48);
        assert_eq!(std::mem::offset_of!(ContainerInfo, total_size), 24);
        assert_eq!(std::mem::offset_of!(ContainerInfo, needs_sync), 40);
        assert_eq!(size_of::<BlobInfo>(), 16);
        assert_eq!(size_of::<Blob>(), 24);
    }

    #[test]
    fn container_names() {
        for ok in ["a", "Save_1", "slot/2", "dir/sub/file.dat", "a.b-c", &"x".repeat(256)] {
            assert!(valid_container_name(ok), "{ok}");
        }
        for bad in ["", "dir/", "a.", "a..b", "with space", "a-b/c", "é", &"x".repeat(257)] {
            assert!(!valid_container_name(bad), "{bad}");
        }
        // Names differing only in case get different files; long names stay short and distinct.
        assert_ne!(escaped("Save"), escaped("save"));
        let (a, b) = ("x".repeat(250) + "a", "x".repeat(250) + "b");
        assert!(escaped(&a).len() < 160);
        assert_ne!(escaped(&a), escaped(&b));
    }

    /// The round trip: written through one provider, read back through a new one (as on the next
    /// run), the file on disk under the user and configuration.
    #[test]
    fn saves_survive_a_new_provider() {
        let (_guard, dir) = folder("trip");
        let header = [7u8; 0x128];
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        {
            let p = provider();
            let c = container(p, c"profile");
            assert_eq!(save(c, c"Profile", &[(c"Save Game Header", &header), (c"Save Game Data", &data)]), S_OK);
            assert_eq!(unsafe { m::<One>(0x88)(t(), c) }, S_OK);
            assert_eq!(unsafe { m::<One>(0x30)(t(), p) }, S_OK);
        }
        let file = dir.join(format!("{:016x}", Profile::default().xuid)).join("scid-1").join("profile.gsc");
        assert!(file.is_file(), "{}", file.display());

        // A new provider, by the async path the title takes.
        let mut b = XAsyncBlock::new(std::ptr::null(), None);
        assert_eq!(unsafe { m::<InitAsync>(0x20)(t(), user(), c"scid-1".as_ptr(), false, &mut b) }, S_OK);
        assert_eq!(unsafe { xasync::get_status(&mut b, true) }, S_OK);
        let mut p = std::ptr::null();
        assert_eq!(unsafe { m::<BlockOut<Handle>>(0x28)(t(), &mut b, &mut p) }, S_OK);
        assert!(!p.is_null());
        let c = container(p, c"profile");
        let all = read_back(c, &[]).unwrap();
        assert_eq!(all, vec![("Save Game Header".into(), header.to_vec()), ("Save Game Data".into(), data.clone())]);
        assert_eq!(read_back(c, &[c"Save Game Data"]).unwrap(), vec![("Save Game Data".into(), data.clone())]);
        assert_eq!(read_back(c, &[c"missing"]), Err(E_GS_BLOB_NOT_FOUND));
        assert_eq!(infos(p), vec![("profile".into(), "Profile".into(), 2, (0x128 + 5000) as u64, false)]);
        done(&dir);
    }

    /// The title's own read: blob info by name sizes the buffer (24 + name + NUL + size), then the
    /// blocking read fills exactly that.
    #[test]
    fn the_titles_sized_read() {
        let (_guard, dir) = folder("sized");
        let p = provider();
        let c = container(p, c"slot0");
        assert_eq!(save(c, c"", &[(c"Save Game Header", b"hdr!"), (c"Save Game Data", b"data")]), S_OK);

        let mut seen: Vec<(String, u32)> = Vec::new();
        let ctx = (&mut seen as *mut Vec<_>).cast();
        assert_eq!(unsafe { m::<BlobsNamed>(0x98)(t(), c, c"Save Game H".as_ptr(), ctx, Some(collect_blobs)) }, S_OK);
        assert_eq!(seen, vec![("Save Game Header".into(), 4)]);
        seen.clear();
        assert_eq!(unsafe { m::<BlobsAll>(0x90)(t(), c, ctx, Some(collect_blobs)) }, S_OK);
        assert_eq!(seen.len(), 2);

        let size = 24 + "Save Game Header".len() + 1 + 4;
        let mut buf = vec![0u64; size.div_ceil(8)];
        let names = [c"Save Game Header".as_ptr()];
        let mut count = 1;
        assert_eq!(unsafe { m::<Read>(0xa0)(t(), c, names.as_ptr(), &mut count, size - 1, buf.as_mut_ptr().cast()) }, E_GS_PROVIDED_BUFFER_TOO_SMALL);
        assert_eq!(unsafe { m::<Read>(0xa0)(t(), c, names.as_ptr(), &mut count, size, buf.as_mut_ptr().cast()) }, S_OK);
        assert_eq!(count, 1);
        assert_eq!(unsafe { blobs_in(buf.as_ptr().cast(), 1) }, vec![("Save Game Header".into(), b"hdr!".to_vec())]);
        // No display name given: the container's name stands in.
        assert_eq!(infos(p)[0].1, "slot0");
        done(&dir);
    }

    /// A container exists once updated; info for a missing one succeeds without a callback; updates
    /// replace and delete blobs; deleting containers; quota.
    #[test]
    fn containers_updates_and_quota() {
        let (_guard, dir) = folder("ops");
        let p = provider();
        let c = container(p, c"b/slot");
        assert!(infos(p).is_empty(), "not finalized until submitted");
        let mut calls = 0u32;
        let one = (&mut calls as *mut u32).cast();
        assert_eq!(unsafe { m::<InfoOne>(0x68)(t(), p, c"b/slot".as_ptr(), one, Some(first_only)) }, S_OK);
        assert_eq!(calls, 0);
        assert!(read_back(c, &[]).unwrap().is_empty());

        assert_eq!(save(c, c"Slot", &[(c"x", b"12345"), (c"y", b"ab")]), S_OK);
        assert_eq!(unsafe { m::<InfoOne>(0x68)(t(), p, c"b/slot".as_ptr(), one, Some(first_only)) }, S_OK);
        assert_eq!(calls, 1);

        // Replace x, delete y, add z; the display name stays when none is given.
        let mut u = std::ptr::null();
        assert_eq!(unsafe { m::<NamedOut>(0xb8)(t(), c, std::ptr::null(), &mut u) }, S_OK);
        assert_eq!(unsafe { m::<Write>(0xc8)(t(), u, c"x".as_ptr(), b"9".as_ptr(), 1) }, S_OK);
        assert_eq!(unsafe { m::<Named>(0xd0)(t(), u, c"y".as_ptr()) }, S_OK);
        assert_eq!(unsafe { m::<Write>(0xc8)(t(), u, c"z".as_ptr(), std::ptr::null(), 0) }, S_OK);
        assert_eq!(unsafe { m::<One>(0xd8)(t(), u) }, S_OK);
        assert_eq!(unsafe { m::<One>(0xc0)(t(), u) }, S_OK);
        assert_eq!(read_back(c, &[]).unwrap(), vec![("x".into(), b"9".to_vec()), ("z".into(), Vec::new())]);
        assert_eq!(infos(p), vec![("b/slot".into(), "Slot".into(), 2, 1, false)]);

        // A second container; enumeration by prefix; stopping early.
        let c2 = container(p, c"a1");
        assert_eq!(save(c2, c"A", &[(c"k", &[1u8; 10])]), S_OK);
        assert_eq!(infos(p).iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), ["a1", "b/slot"]);
        let mut seen = Vec::new();
        let ctx = (&mut seen as *mut Vec<(String, String, u32, u64, bool)>).cast();
        type ByName = unsafe extern "system" fn(This, Handle, *const c_char, *mut c_void, Option<ContainerCallback>) -> HRESULT;
        assert_eq!(unsafe { m::<ByName>(0x78)(t(), p, c"b/".as_ptr(), ctx, Some(collect_containers)) }, S_OK);
        assert_eq!(seen.len(), 1);
        calls = 0;
        assert_eq!(unsafe { m::<InfoAll>(0x70)(t(), p, one, Some(first_only)) }, S_OK);
        assert_eq!(calls, 1, "false stops the enumeration");

        // Quota: blocking and async agree.
        let mut left = 0i64;
        assert_eq!(unsafe { m::<HandleOut<i64>>(0x38)(t(), p, &mut left) }, S_OK);
        assert_eq!(left, (QUOTA - 11) as i64);
        let mut b = XAsyncBlock::new(std::ptr::null(), None);
        assert_eq!(unsafe { m::<HandleBlock>(0x40)(t(), p, &mut b) }, S_OK);
        assert_eq!(unsafe { xasync::get_status(&mut b, true) }, S_OK);
        let mut left2 = 0i64;
        assert_eq!(unsafe { m::<BlockOut<i64>>(0x48)(t(), &mut b, &mut left2) }, S_OK);
        assert_eq!(left2, left);

        // Deleting: blocking, async, and a container that is not there.
        assert_eq!(unsafe { m::<Named>(0x50)(t(), p, c"a1".as_ptr()) }, S_OK);
        let mut b = XAsyncBlock::new(std::ptr::null(), None);
        assert_eq!(unsafe { m::<NamedBlock>(0x58)(t(), p, c"b/slot".as_ptr(), &mut b) }, S_OK);
        assert_eq!(unsafe { xasync::get_status(&mut b, true) }, S_OK);
        assert_eq!(unsafe { m::<BlockOnly>(0x60)(t(), &mut b) }, S_OK);
        assert_eq!(unsafe { m::<Named>(0x50)(t(), p, c"never".as_ptr()) }, S_OK);
        assert!(infos(p).is_empty());
        done(&dir);
    }

    #[test]
    fn refusals() {
        let (_guard, dir) = folder("refuse");
        let p = provider();
        let mut c = 1 as Handle;
        assert_eq!(unsafe { m::<NamedOut>(0x80)(t(), p, c"has space".as_ptr(), &mut c) }, E_GS_INVALID_CONTAINER_NAME);
        assert!(c.is_null(), "null on failure");
        assert!(crate::discovery::contains("game save: invalid container name \"has space\""));
        assert_eq!(unsafe { m::<NamedOut>(0x80)(t(), 0x1234 as Handle, c"ok".as_ptr(), &mut c) }, E_INVALIDARG);
        let mut q = std::ptr::null();
        assert_eq!(unsafe { m::<Init>(0x18)(t(), std::ptr::null(), c"s".as_ptr(), false, &mut q) }, E_INVALIDARG);

        // Too big for one update; too big for the quota is the same bytes many times over.
        let c = container(p, c"big");
        let mut u = std::ptr::null();
        assert_eq!(unsafe { m::<NamedOut>(0xb8)(t(), c, c"".as_ptr(), &mut u) }, S_OK);
        let half = vec![0u8; (MAX_UPDATE / 2 + 1) as usize];
        assert_eq!(unsafe { m::<Write>(0xc8)(t(), u, c"a".as_ptr(), half.as_ptr(), half.len()) }, S_OK);
        assert_eq!(unsafe { m::<Write>(0xc8)(t(), u, c"b".as_ptr(), half.as_ptr(), half.len()) }, S_OK);
        assert_eq!(unsafe { m::<One>(0xd8)(t(), u) }, E_GS_UPDATE_TOO_BIG);
        assert_eq!(unsafe { m::<One>(0xc0)(t(), u) }, S_OK);
        assert!(infos(p).is_empty(), "nothing of a failed update lands");

        // A closed provider is gone; no saves folder at all means no access.
        assert_eq!(unsafe { m::<One>(0x30)(t(), p) }, S_OK);
        let mut left = 0i64;
        assert_eq!(unsafe { m::<HandleOut<i64>>(0x38)(t(), p, &mut left) }, E_INVALIDARG);
        profile::configure(Profile::default()).unwrap();
        assert_eq!(unsafe { m::<Init>(0x18)(t(), user(), c"s".as_ptr(), false, &mut q) }, E_GS_NO_ACCESS);
        done(&dir);
    }

    /// The async calls complete on the block's own queue: nothing happens until its work port is
    /// dispatched, and the completion routine runs when its completion port is.
    #[test]
    fn async_work_runs_on_the_blocks_queue() {
        let (_guard, dir) = folder("queue");
        let mut q = std::ptr::null();
        assert_eq!(unsafe { taskqueue::create(Mode::Manual as u32, Mode::Manual as u32, &mut q) }, S_OK);
        static RAN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        unsafe extern "system" fn routine(_: *mut XAsyncBlock) {
            RAN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let mut b = XAsyncBlock::new(q, Some(routine));
        assert_eq!(unsafe { m::<InitAsync>(0x20)(t(), user(), c"q".as_ptr(), false, &mut b) }, S_OK);
        assert_eq!(unsafe { xasync::get_status(&mut b, false) }, E_PENDING);
        assert!(unsafe { taskqueue::dispatch(q, taskqueue::WORK, 0) });
        assert_eq!(unsafe { xasync::get_status(&mut b, false) }, S_OK);
        assert_eq!(RAN.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(unsafe { taskqueue::dispatch(q, taskqueue::COMPLETION, 0) });
        assert_eq!(RAN.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut p = std::ptr::null();
        assert_eq!(unsafe { m::<BlockOut<Handle>>(0x28)(t(), &mut b, &mut p) }, S_OK);
        assert!(!p.is_null());
        unsafe { taskqueue::close(q) };
        done(&dir);
    }

    /// The tools' view of a container file is the runtime's: decode inverts encode, refuses a file cut
    /// short or with bytes after its last blob, and `file::write` lands where the runtime reads.
    #[test]
    fn the_file_api_is_the_runtimes() {
        let c = Stored {
            name: "SettingsProfileSettings".into(),
            display: "Profile Settings".into(),
            modified: 1_764_000_000,
            blobs: vec![("Save Game Header".into(), vec![6; 0x128]), ("Save Game Data".into(), vec![1, 2, 3])],
        };
        let bytes = file::encode(&c);
        assert_eq!(file::decode(&bytes).unwrap(), c);
        assert!(file::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(file::decode(&longer).is_err());
        let dir = std::env::temp_dir().join(format!("rebuild-gdk-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = file::path(&dir, &c.name);
        assert_eq!(path, container_file(&dir, &c.name));
        file::write(&path, &c).unwrap();
        assert_eq!(load(&path).unwrap(), c);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
