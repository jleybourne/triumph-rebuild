//! The file overrides (Build 3): the host's path-taking file functions, over the title's console
//! drives through the overlay file system (`crates/vfs`).
//!
//! ```text
//!   title calls CreateFileW("G:\data\x.rpf", ...)          (our function, the host's exact signature)
//!        │  console path: relative paths against our current directory, `X:rel`, `\x`, `\\?\X:\x`
//!        │  unknown (UNC, devices, an unmounted letter, streams) → discovery entry, ERROR_PATH_NOT_FOUND
//!        ▼
//!   vfs   resolve (reads), or create / copy up into the overlay (writes), with vfs's bookkeeping
//!        ▼
//!   host  CreateFileW("\\?\<overlay or base>\data\x.rpf", ...) → a real host HANDLE
//! ```
//!
//! Handles are the host's, so `ReadFile`, `WriteFile`, `CloseHandle`, `GetFileSizeEx` and the rest
//! stay bound to the host. The base (game folder) and mods are only ever opened without write
//! access: any open that could write lands on a copy in the overlay first. Errors are the host's
//! codes through `SetLastError`, checked side by side against the host (`tests::f2_*`).
//!
//! The one exception to host handles is a directory search: a merged listing (overlay over mods
//! over base) is not one host directory, so `FindFirstFile*` return our own search handles, and
//! `FindNextFileW` / `FindNextFileA` / `FindClose` must be bound to ours too (they pass host
//! handles through to the host). Each entry's data is the host's own `WIN32_FIND_DATA`.
//!
//! State: one installed [`Files`] (a `Vfs` and the current directory), because the functions have
//! the host's signatures and no context argument. [`install`] it before binding.

#![cfg(windows)]
#![allow(non_snake_case, clippy::missing_safety_doc)]

pub mod host;

pub mod crt;
mod extra;
mod find;

use std::ffi::c_void;
use std::sync::{Arc, Mutex, RwLock};

use host::{HANDLE, INVALID_FILE_ATTRIBUTES, INVALID_HANDLE_VALUE};
use rebuild_vfs::{ConsolePath, Disposition, Error as VfsError, Layer, OpenOptions, Vfs};

pub use extra::{CopyFile2, CreateHardLinkW, CreateSymbolicLinkW, SetFileInformationByHandle, WerRegisterFile, ERROR_PRIVILEGE_NOT_HELD};
pub use find::{open_searches, FindClose, FindFirstFileA, FindFirstFileExA, FindFirstFileExW, FindFirstFileW, FindNextFileA, FindNextFileW};

// ---- Win32 constants ---------------------------------------------------------------------------

pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_NOT_SAME_DEVICE: u32 = 17;
pub const ERROR_NO_MORE_FILES: u32 = 18;
pub const ERROR_NOT_READY: u32 = 21;
pub const ERROR_GEN_FAILURE: u32 = 31;
pub const ERROR_FILE_EXISTS: u32 = 80;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INVALID_NAME: u32 = 123;
pub const ERROR_DIR_NOT_EMPTY: u32 = 145;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_DIRECTORY: u32 = 267;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_WRITE_DATA: u32 = 0x2;
/// Every access right that can change a file or its metadata. Any of them sends the open to the overlay.
const WRITE_ACCESS: u32 = GENERIC_WRITE
    | GENERIC_ALL
    | FILE_WRITE_DATA
    | 0x4 // FILE_APPEND_DATA
    | 0x10 // FILE_WRITE_EA
    | 0x40 // FILE_DELETE_CHILD
    | 0x100 // FILE_WRITE_ATTRIBUTES
    | 0x1_0000 // DELETE
    | 0x4_0000 // WRITE_DAC
    | 0x8_0000 // WRITE_OWNER
    | 0x200_0000; // MAXIMUM_ALLOWED

pub const CREATE_NEW: u32 = 1;
pub const CREATE_ALWAYS: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const OPEN_ALWAYS: u32 = 4;
pub const TRUNCATE_EXISTING: u32 = 5;

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// The attribute bits a new file takes from `CreateFile`.
const CREATE_ATTRIBUTES: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x1000 | 0x2000;
pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

pub const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
pub const MOVEFILE_COPY_ALLOWED: u32 = 0x2;
pub const MOVEFILE_DELAY_UNTIL_REBOOT: u32 = 0x4;

// ---- state ---------------------------------------------------------------------------------------

/// The file system the overrides use: the drives, and the title's current directory (a console path).
pub struct Files {
    vfs: Arc<Vfs>,
    cwd: Mutex<String>,
}

impl Files {
    /// `cwd` is the title's starting directory, a console path such as `G:\`.
    /// `vfs` may be shared (an `Arc`) with DirectStorage, so both see one overlay.
    pub fn new(vfs: impl Into<Arc<Vfs>>, cwd: &str) -> Files {
        Files { vfs: vfs.into(), cwd: Mutex::new(cwd.to_owned()) }
    }

    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn current_dir(&self) -> String {
        self.cwd.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

static FILES: RwLock<Option<Arc<Files>>> = RwLock::new(None);

/// Makes `files` the file system every override uses (replacing any before it).
pub fn install(files: Files) -> Arc<Files> {
    let files = Arc::new(files);
    *FILES.write().unwrap_or_else(|e| e.into_inner()) = Some(files.clone());
    files
}

pub fn uninstall() {
    *FILES.write().unwrap_or_else(|e| e.into_inner()) = None;
}

fn installed() -> Option<Arc<Files>> {
    FILES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

// ---- F3: discovery -------------------------------------------------------------------------------

/// A path the title used that the overlay file system does not know. Never guessed: the call fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub function: &'static str,
    pub path: String,
    pub why: &'static str,
    /// How many times the title asked.
    pub count: u64,
}

static DISCOVERIES: Mutex<Vec<Discovery>> = Mutex::new(Vec::new());

fn discover(function: &'static str, path: &str, why: &'static str) {
    let mut d = DISCOVERIES.lock().unwrap_or_else(|e| e.into_inner());
    match d.iter_mut().find(|x| x.function == function && x.path == path && x.why == why) {
        Some(x) => x.count += 1,
        None => d.push(Discovery { function, path: path.to_owned(), why, count: 1 }),
    }
}

/// The discovery list so far, in first-seen order.
pub fn discoveries() -> Vec<Discovery> {
    DISCOVERIES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// ---- paths and errors ------------------------------------------------------------------------------

fn set_last_error(code: u32) {
    unsafe { host::SetLastError(code) }
}

fn is_device(raw: &str) -> bool {
    if raw.contains(['\\', '/']) {
        return false;
    }
    let stem = raw.split('.').next().unwrap_or(raw).trim_end_matches(':').to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

fn join(dir: &str, rel: &str) -> String {
    if dir.ends_with(['\\', '/']) {
        format!("{dir}{rel}")
    } else {
        format!("{dir}\\{rel}")
    }
}

fn drive_of(path: &str) -> Option<char> {
    let p = path.strip_prefix(r"\\?\").unwrap_or(path);
    let b = p.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()).then(|| (b[0] as char).to_ascii_uppercase())
}

impl Files {
    /// The absolute console form of a path the title passed, the way Win32 makes a path absolute,
    /// but against our current directory. `None`: not a console path.
    fn absolute(&self, raw: &str) -> Option<String> {
        if raw.is_empty() || is_device(raw) {
            return None;
        }
        if let Some(rest) = raw.strip_prefix(r"\\?\").or_else(|| raw.strip_prefix("//?/")) {
            return drive_of(rest).map(|_| raw.to_owned());
        }
        if raw.starts_with(r"\\") || raw.starts_with("//") {
            return None;
        }
        let cwd = self.current_dir();
        let b = raw.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            let rest = &raw[2..];
            if rest.starts_with(['\\', '/']) {
                Some(raw.to_owned())
            } else if drive_of(&cwd) == Some((b[0] as char).to_ascii_uppercase()) {
                // `X:rel` is relative to the current directory when that is on drive X
                Some(if rest.is_empty() { cwd } else { join(&cwd, rest) })
            } else {
                Some(format!("{}:\\{rest}", b[0] as char))
            }
        } else if raw.starts_with(['\\', '/']) {
            Some(format!("{}:{raw}", drive_of(&cwd)?))
        } else {
            Some(join(&cwd, raw))
        }
    }

    /// The console path for `raw`, or a discovery entry and `ERROR_PATH_NOT_FOUND`.
    fn path(&self, function: &'static str, raw: &str) -> Result<String, u32> {
        self.absolute(raw).ok_or_else(|| {
            discover(function, raw, "not a console path");
            ERROR_PATH_NOT_FOUND
        })
    }

    /// The host error for a `vfs` error; paths `vfs` does not know become discovery entries.
    fn fail(&self, function: &'static str, path: &str, e: VfsError) -> u32 {
        match e {
            VfsError::NotOnDrive(_) => {
                discover(function, path, "not a console path");
                ERROR_PATH_NOT_FOUND
            }
            VfsError::NoDrive(_) => {
                discover(function, path, "no drive mounted at this letter");
                ERROR_PATH_NOT_FOUND
            }
            VfsError::InvalidPath(_) => {
                discover(function, path, "a name the overlay file system refuses");
                ERROR_INVALID_NAME
            }
            e => code(&e),
        }
    }
}

/// The host's error code for a `vfs` error (context-free; callers refine `Exists`).
pub fn code(e: &VfsError) -> u32 {
    match e {
        VfsError::NotOnDrive(_) | VfsError::NoDrive(_) => ERROR_PATH_NOT_FOUND,
        VfsError::InvalidPath(_) => ERROR_INVALID_NAME,
        VfsError::NotFound => ERROR_FILE_NOT_FOUND,
        VfsError::PathNotFound => ERROR_PATH_NOT_FOUND,
        VfsError::Exists => ERROR_ALREADY_EXISTS,
        VfsError::IsADirectory => ERROR_ACCESS_DENIED,
        VfsError::NotADirectory => ERROR_DIRECTORY,
        VfsError::DirectoryNotEmpty => ERROR_DIR_NOT_EMPTY,
        VfsError::CrossDrive => ERROR_NOT_SAME_DEVICE,
        VfsError::BadHandle => ERROR_INVALID_HANDLE,
        VfsError::ReadOnlyHandle | VfsError::Unsafe(_) => ERROR_ACCESS_DENIED,
        VfsError::Io(e) => e.raw_os_error().map(|c| c as u32).unwrap_or(ERROR_GEN_FAILURE),
    }
}

/// Runs `f` against the installed file system; an error code goes to `SetLastError` and `fail` is returned.
fn with<T>(function: &'static str, fail: T, f: impl FnOnce(&Files) -> Result<T, u32>) -> T {
    let Some(files) = installed() else {
        discover(function, "", "no file system installed");
        set_last_error(ERROR_NOT_READY);
        return fail;
    };
    match f(&files) {
        Ok(v) => v,
        Err(code) => {
            set_last_error(code);
            fail
        }
    }
}

fn bool_result(r: Result<(), u32>) -> Result<i32, u32> {
    r.map(|_| 1)
}

// ---- CreateFile -----------------------------------------------------------------------------------

impl Files {
    /// Shared by the three `CreateFile`s. `open` calls the host with a host path and `OPEN_EXISTING`.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        function: &'static str,
        raw: &str,
        access: u32,
        disposition: u32,
        attributes: u32,
        flags: u32,
        open: impl FnOnce(&[u16]) -> HANDLE,
    ) -> Result<HANDLE, u32> {
        if !(CREATE_NEW..=TRUNCATE_EXISTING).contains(&disposition) {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let path = self.path(function, raw)?;
        let write = access & WRITE_ACCESS != 0 || flags & FILE_FLAG_DELETE_ON_CLOSE != 0;
        let existing = self.vfs.resolve(&path);
        if !write && disposition == OPEN_EXISTING {
            // read-only: the layer the file lives in, as it is
            let r = existing.map_err(|e| self.fail(function, &path, e))?;
            return Ok(open(&host::host_path(&r.host)));
        }
        if disposition == TRUNCATE_EXISTING && access & (GENERIC_WRITE | GENERIC_ALL | FILE_WRITE_DATA) == 0 {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let existed = match existing {
            Ok(r) => Some(r),
            Err(VfsError::NotFound | VfsError::PathNotFound) => None,
            Err(e) => return Err(self.fail(function, &path, e)),
        };
        // the host refuses to write or overwrite a read-only file; copy-up would clear the bit
        if let Some(r) = existed.as_ref().filter(|r| !r.is_dir) {
            if write || matches!(disposition, CREATE_ALWAYS | TRUNCATE_EXISTING) {
                let a = unsafe { host::GetFileAttributesW(host::host_path(&r.host).as_ptr()) };
                if a != INVALID_FILE_ATTRIBUTES && a & FILE_ATTRIBUTE_READONLY != 0 {
                    return Err(ERROR_ACCESS_DENIED);
                }
            }
        }
        if let Some(r) = existed.as_ref().filter(|r| r.is_dir) {
            return match disposition {
                OPEN_EXISTING | OPEN_ALWAYS => {
                    // a write handle to a directory below the overlay gets the overlay's copy of it
                    let host = if write && r.layer != Layer::Overlay { self.materialize(function, &path, r)? } else { r.host.clone() };
                    let h = open(&host::host_path(&host));
                    if h != INVALID_HANDLE_VALUE {
                        if flags & FILE_FLAG_DELETE_ON_CLOSE != 0 {
                            let _ = self.vfs.delete_pending(&path);
                        }
                        set_last_error(if disposition == OPEN_ALWAYS { ERROR_ALREADY_EXISTS } else { 0 });
                    }
                    Ok(h)
                }
                _ => Err(ERROR_ACCESS_DENIED),
            };
        }
        let d = match disposition {
            CREATE_NEW => Disposition::CreateNew,
            CREATE_ALWAYS => Disposition::CreateAlways,
            OPEN_EXISTING => Disposition::OpenExisting,
            OPEN_ALWAYS => Disposition::OpenAlways,
            _ => Disposition::TruncateExisting,
        };
        // vfs does the bookkeeping: creates in the overlay, or copies up, or truncates there
        let h = self.vfs.open(&path, OpenOptions { write, disposition: d }).map_err(|e| match e {
            VfsError::Exists => ERROR_FILE_EXISTS,
            e => self.fail(function, &path, e),
        })?;
        let _ = self.vfs.close(h);
        let r = self.vfs.resolve(&path).map_err(|e| self.fail(function, &path, e))?;
        let host = host::host_path(&r.host);
        let handle = open(&host);
        if handle == INVALID_HANDLE_VALUE {
            return Ok(handle);
        }
        if flags & FILE_FLAG_DELETE_ON_CLOSE != 0 {
            // the host deletes the overlay file at the last close; vfs settles it then
            let _ = self.vfs.delete_pending(&path);
        }
        let attrs = attributes & CREATE_ATTRIBUTES;
        if existed.is_none() && attrs & !FILE_ATTRIBUTE_ARCHIVE != 0 {
            unsafe { host::SetFileAttributesW(host.as_ptr(), attrs | FILE_ATTRIBUTE_ARCHIVE) };
        }
        let already = existed.is_some() && matches!(disposition, CREATE_ALWAYS | OPEN_ALWAYS);
        set_last_error(if already { ERROR_ALREADY_EXISTS } else { 0 });
        Ok(handle)
    }
}

/// `REBUILD_TRACE_FILES`: name the first 200 opens that fail (function, console path, error) on stderr.
fn trace_failed_open(function: &str, path: &str) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static N: AtomicU32 = AtomicU32::new(0);
    if !*ON.get_or_init(|| std::env::var_os("REBUILD_TRACE_FILES").is_some()) || N.fetch_add(1, Ordering::Relaxed) >= 200 {
        return;
    }
    let code = unsafe { host::GetLastError() };
    eprintln!("files     {function} failed ({code}): {path}");
}

pub unsafe extern "system" fn CreateFileW(
    name: *const u16,
    access: u32,
    share: u32,
    sa: *const c_void,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let raw = host::from_wide(name).unwrap_or_default();
    let h = with("CreateFileW", INVALID_HANDLE_VALUE, |f| {
        f.create("CreateFileW", &raw, access, disposition, flags, flags, |p| unsafe {
            host::CreateFileW(p.as_ptr(), access, share, sa, OPEN_EXISTING, flags, template)
        })
    });
    if h == INVALID_HANDLE_VALUE {
        trace_failed_open("CreateFileW", &raw);
    }
    h
}

pub unsafe extern "system" fn CreateFileA(
    name: *const u8,
    access: u32,
    share: u32,
    sa: *const c_void,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let raw = host::from_ansi(name).unwrap_or_default();
    let h = with("CreateFileA", INVALID_HANDLE_VALUE, |f| {
        f.create("CreateFileA", &raw, access, disposition, flags, flags, |p| unsafe {
            host::CreateFileW(p.as_ptr(), access, share, sa, OPEN_EXISTING, flags, template)
        })
    });
    if h == INVALID_HANDLE_VALUE {
        trace_failed_open("CreateFileA", &raw);
    }
    h
}

pub unsafe extern "system" fn CreateFile2(
    name: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    params: *const host::CREATEFILE2_EXTENDED_PARAMETERS,
) -> HANDLE {
    let raw = host::from_wide(name).unwrap_or_default();
    let (attrs, flags) = match params.as_ref() {
        Some(p) => (p.dwFileAttributes, p.dwFileFlags),
        None => (0, 0),
    };
    let h = with("CreateFile2", INVALID_HANDLE_VALUE, |f| {
        f.create("CreateFile2", &raw, access, disposition, attrs, flags, |p| unsafe {
            host::CreateFile2(p.as_ptr(), access, share, OPEN_EXISTING, params)
        })
    });
    if h == INVALID_HANDLE_VALUE {
        trace_failed_open("CreateFile2", &raw);
    }
    h
}

// ---- delete, directories, move ------------------------------------------------------------------------

impl Files {
    fn delete_file(&self, function: &'static str, raw: &str) -> Result<(), u32> {
        let path = self.path(function, raw)?;
        let r = self.vfs.resolve(&path).map_err(|e| self.fail(function, &path, e))?;
        if r.is_dir {
            return Err(ERROR_ACCESS_DENIED);
        }
        // the host refuses to delete a read-only file; a whiteout must too
        let attrs = unsafe { host::GetFileAttributesW(host::host_path(&r.host).as_ptr()) };
        if attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_READONLY != 0 {
            return Err(ERROR_ACCESS_DENIED);
        }
        self.vfs.delete(&path).map_err(|e| self.fail(function, &path, e))
    }

    fn create_dir(&self, function: &'static str, raw: &str) -> Result<(), u32> {
        let path = self.path(function, raw)?;
        self.vfs.create_dir(&path).map_err(|e| self.fail(function, &path, e))
    }

    fn remove_dir(&self, function: &'static str, raw: &str) -> Result<(), u32> {
        let path = self.path(function, raw)?;
        self.vfs.remove_dir(&path).map_err(|e| self.fail(function, &path, e))
    }

    fn move_file(&self, function: &'static str, from: &str, to: Option<&str>, flags: u32) -> Result<(), u32> {
        let src = self.path(function, from)?;
        if flags & MOVEFILE_DELAY_UNTIL_REBOOT != 0 {
            discover(function, &src, "MOVEFILE_DELAY_UNTIL_REBOOT");
            return Err(ERROR_ACCESS_DENIED);
        }
        let Some(to) = to else { return Err(ERROR_INVALID_PARAMETER) };
        let dst = self.path(function, to)?;
        let replace = flags & MOVEFILE_REPLACE_EXISTING != 0;
        let found = self.vfs.resolve(&src).map_err(|e| self.fail(function, &src, e))?;
        let same = ConsolePath::parse(&dst).ok().map(|d| d.key()) == ConsolePath::parse(&src).ok().map(|s| s.key());
        if same && found.layer != Layer::Overlay {
            if found.is_dir {
                discover(function, &src, "a change of case of a directory below the overlay");
                return Err(ERROR_ACCESS_DENIED);
            }
            // vfs renames case only in the overlay: copy the file up, then rename it there
            let h = self.vfs.open(&src, OpenOptions::write(Disposition::OpenExisting)).map_err(|e| self.fail(function, &src, e))?;
            let _ = self.vfs.close(h);
        }
        match self.vfs.rename(&src, &dst, replace) {
            Ok(()) => Ok(()),
            Err(VfsError::Exists) => Err(if replace { ERROR_ACCESS_DENIED } else { ERROR_ALREADY_EXISTS }),
            Err(VfsError::CrossDrive) if flags & MOVEFILE_COPY_ALLOWED != 0 && !found.is_dir => {
                self.copy_across(function, &src, &dst, replace)?;
                self.vfs.delete(&src).map_err(|e| self.fail(function, &src, e))
            }
            Err(e) => Err(self.fail(function, &dst, e)),
        }
    }

    /// `MOVEFILE_COPY_ALLOWED` between drives: a copy through vfs.
    fn copy_across(&self, function: &'static str, src: &str, dst: &str, replace: bool) -> Result<(), u32> {
        let from = self.vfs.open(src, OpenOptions::read()).map_err(|e| self.fail(function, src, e))?;
        let disposition = if replace { Disposition::CreateAlways } else { Disposition::CreateNew };
        let to = match self.vfs.open(dst, OpenOptions::write(disposition)) {
            Ok(h) => h,
            Err(e) => {
                let _ = self.vfs.close(from);
                return Err(match e {
                    VfsError::Exists => ERROR_ALREADY_EXISTS,
                    e => self.fail(function, dst, e),
                });
            }
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0u64;
        let result = loop {
            match self.vfs.read_at(from, at, &mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if let Err(e) = self.vfs.write_at(to, at, &buf[..n]) {
                        break Err(code(&e));
                    }
                    at += n as u64;
                }
                Err(e) => break Err(code(&e)),
            }
        };
        let _ = self.vfs.close(from);
        let _ = self.vfs.close(to);
        result
    }
}

pub unsafe extern "system" fn DeleteFileW(name: *const u16) -> i32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("DeleteFileW", 0, |f| bool_result(f.delete_file("DeleteFileW", &raw)))
}

pub unsafe extern "system" fn DeleteFileA(name: *const u8) -> i32 {
    let raw = host::from_ansi(name).unwrap_or_default();
    with("DeleteFileA", 0, |f| bool_result(f.delete_file("DeleteFileA", &raw)))
}

/// Security attributes are ignored: the overlay is the process's own.
pub unsafe extern "system" fn CreateDirectoryW(name: *const u16, _sa: *const c_void) -> i32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("CreateDirectoryW", 0, |f| bool_result(f.create_dir("CreateDirectoryW", &raw)))
}

pub unsafe extern "system" fn CreateDirectoryA(name: *const u8, _sa: *const c_void) -> i32 {
    let raw = host::from_ansi(name).unwrap_or_default();
    with("CreateDirectoryA", 0, |f| bool_result(f.create_dir("CreateDirectoryA", &raw)))
}

pub unsafe extern "system" fn RemoveDirectoryW(name: *const u16) -> i32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("RemoveDirectoryW", 0, |f| bool_result(f.remove_dir("RemoveDirectoryW", &raw)))
}

pub unsafe extern "system" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let src = host::from_wide(from).unwrap_or_default();
    let dst = host::from_wide(to);
    with("MoveFileExW", 0, |f| bool_result(f.move_file("MoveFileExW", &src, dst.as_deref(), flags)))
}

// ---- attributes -------------------------------------------------------------------------------------

impl Files {
    fn resolve(&self, function: &'static str, raw: &str) -> Result<rebuild_vfs::Resolved, u32> {
        let path = self.path(function, raw)?;
        self.vfs.resolve(&path).map_err(|e| self.fail(function, &path, e))
    }

    fn attributes(&self, function: &'static str, raw: &str) -> Result<u32, u32> {
        let r = self.resolve(function, raw)?;
        let a = unsafe { host::GetFileAttributesW(host::host_path(&r.host).as_ptr()) };
        if a == INVALID_FILE_ATTRIBUTES {
            return Err(unsafe { host::GetLastError() });
        }
        Ok(a)
    }

    fn attributes_ex(&self, function: &'static str, raw: &str, level: i32, info: *mut c_void) -> Result<i32, u32> {
        if level != 0 {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let r = self.resolve(function, raw)?;
        if unsafe { host::GetFileAttributesExW(host::host_path(&r.host).as_ptr(), 0, info) } == 0 {
            return Err(unsafe { host::GetLastError() });
        }
        Ok(1)
    }

    /// Makes a directory below the overlay exist in the overlay, with the attributes it has below;
    /// returns its overlay host path. The base keeps its own.
    fn materialize(&self, function: &'static str, path: &str, below: &rebuild_vfs::Resolved) -> Result<std::path::PathBuf, u32> {
        let attrs = unsafe { host::GetFileAttributesW(host::host_path(&below.host).as_ptr()) };
        let host = self.vfs.materialize_dir(path).map_err(|e| self.fail(function, path, e))?;
        if attrs != INVALID_FILE_ATTRIBUTES {
            unsafe { host::SetFileAttributesW(host::host_path(&host).as_ptr(), attrs & !FILE_ATTRIBUTE_DIRECTORY) };
        }
        Ok(host)
    }

    fn set_attributes(&self, function: &'static str, raw: &str, attrs: u32) -> Result<i32, u32> {
        let path = self.path(function, raw)?;
        let mut r = self.vfs.resolve(&path).map_err(|e| self.fail(function, &path, e))?;
        if r.layer != Layer::Overlay && r.is_dir {
            r.host = self.materialize(function, &path, &r)?;
        } else if r.layer != Layer::Overlay {
            // copy up first: the base is never written
            let h = self.vfs.open(&path, OpenOptions::write(Disposition::OpenExisting)).map_err(|e| self.fail(function, &path, e))?;
            let _ = self.vfs.close(h);
            r = self.vfs.resolve(&path).map_err(|e| self.fail(function, &path, e))?;
        }
        if unsafe { host::SetFileAttributesW(host::host_path(&r.host).as_ptr(), attrs) } == 0 {
            return Err(unsafe { host::GetLastError() });
        }
        Ok(1)
    }
}

pub unsafe extern "system" fn GetFileAttributesW(name: *const u16) -> u32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("GetFileAttributesW", INVALID_FILE_ATTRIBUTES, |f| f.attributes("GetFileAttributesW", &raw))
}

pub unsafe extern "system" fn GetFileAttributesA(name: *const u8) -> u32 {
    let raw = host::from_ansi(name).unwrap_or_default();
    with("GetFileAttributesA", INVALID_FILE_ATTRIBUTES, |f| f.attributes("GetFileAttributesA", &raw))
}

pub unsafe extern "system" fn GetFileAttributesExW(name: *const u16, level: i32, info: *mut c_void) -> i32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("GetFileAttributesExW", 0, |f| f.attributes_ex("GetFileAttributesExW", &raw, level, info))
}

pub unsafe extern "system" fn GetFileAttributesExA(name: *const u8, level: i32, info: *mut c_void) -> i32 {
    let raw = host::from_ansi(name).unwrap_or_default();
    with("GetFileAttributesExA", 0, |f| f.attributes_ex("GetFileAttributesExA", &raw, level, info))
}

/// Not in `OVERRIDES` yet (proposed): the title imports it, and the host's would see a console path.
pub unsafe extern "system" fn SetFileAttributesW(name: *const u16, attrs: u32) -> i32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("SetFileAttributesW", 0, |f| f.set_attributes("SetFileAttributesW", &raw, attrs))
}

// ---- full path, current directory, free space ---------------------------------------------------------

impl Files {
    /// The full console path, made by the host's own lexical rules once it is absolute.
    fn full_path(&self, function: &'static str, raw: &str) -> Result<Vec<u16>, u32> {
        let abs = self.path(function, raw)?;
        let w = host::wide(&abs);
        let mut buf = vec![0u16; 512];
        loop {
            let n = unsafe { host::GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
            if n == 0 {
                return Err(unsafe { host::GetLastError() });
            }
            if n < buf.len() {
                buf.truncate(n);
                return Ok(buf);
            }
            buf.resize(n, 0);
        }
    }

    fn set_current_dir(&self, function: &'static str, raw: &str) -> Result<i32, u32> {
        let full = String::from_utf16_lossy(&self.full_path(function, raw)?);
        let r = self.vfs.resolve(&full).map_err(|e| self.fail(function, &full, e))?;
        if !r.is_dir {
            return Err(ERROR_DIRECTORY);
        }
        let mut cwd = full.strip_prefix(r"\\?\").unwrap_or(&full).to_owned();
        while cwd.len() > 3 && cwd.ends_with('\\') {
            cwd.pop();
        }
        *self.cwd.lock().unwrap_or_else(|e| e.into_inner()) = cwd;
        Ok(1)
    }

    fn disk_free(&self, function: &'static str, dir: Option<&str>, out: [*mut u64; 3]) -> Result<i32, u32> {
        let path = match dir {
            Some(d) => self.path(function, d)?,
            None => self.current_dir(),
        };
        let r = self.vfs.resolve(&path).map_err(|e| match self.fail(function, &path, e) {
            ERROR_FILE_NOT_FOUND => ERROR_PATH_NOT_FOUND,
            c => c,
        })?;
        if !r.is_dir {
            return Err(ERROR_DIRECTORY);
        }
        // every write lands in the drive's overlay, so its volume is the space the title has
        let drive = ConsolePath::parse(&path).map_err(|e| self.fail(function, &path, e))?.drive;
        let root = self.vfs.resolve(&format!("{drive}:\\")).map_err(|e| self.fail(function, &path, e))?;
        let mut w = host::host_path(&root.host);
        w.pop();
        w.extend_from_slice(&[u16::from(b'\\'), 0]);
        if unsafe { host::GetDiskFreeSpaceExW(w.as_ptr(), out[0], out[1], out[2]) } == 0 {
            return Err(unsafe { host::GetLastError() });
        }
        Ok(1)
    }
}

/// Copies `s` and a NUL into `buf` when it fits (returns its length), else returns the size needed
/// with the NUL: the convention of `GetCurrentDirectory`, `GetFullPathName` and `GetTempPath`.
unsafe fn copy_out<T: Copy + Default>(s: &[T], buf: *mut T, len: u32) -> u32 {
    if (len as usize) > s.len() && !buf.is_null() {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf, s.len());
        *buf.add(s.len()) = T::default();
        s.len() as u32
    } else {
        s.len() as u32 + 1
    }
}

/// Where the last component starts in a full path, or `None` when it ends in a separator.
fn file_part<T: Copy + Into<u32>>(s: &[T]) -> Option<usize> {
    let sep = |c: T| matches!(c.into(), 0x5C | 0x2F);
    if s.last().is_none_or(|&c| sep(c)) {
        return None;
    }
    Some(s.iter().rposition(|&c| sep(c)).map_or(0, |i| i + 1))
}

/// The host's own call on the absolute console path: lexical rules, buffer sizes and the file part are the host's.
pub unsafe extern "system" fn GetFullPathNameW(name: *const u16, len: u32, buf: *mut u16, part: *mut *mut u16) -> u32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("GetFullPathNameW", 0, |f| {
        let abs = host::wide(&f.path("GetFullPathNameW", &raw)?);
        match host::GetFullPathNameW(abs.as_ptr(), len, buf, part) {
            0 => Err(host::GetLastError()),
            n => Ok(n),
        }
    })
}

pub unsafe extern "system" fn GetFullPathNameA(name: *const u8, len: u32, buf: *mut u8, part: *mut *mut u8) -> u32 {
    let raw = host::from_ansi(name).unwrap_or_default();
    with("GetFullPathNameA", 0, |f| {
        let abs = f.path("GetFullPathNameA", &raw)?;
        let full = host::to_ansi(&String::from_utf16_lossy(&f.full_path("GetFullPathNameA", &raw)?));
        if (len as usize) <= full.len() {
            // as the host: the size of the path before normalising, with the NUL
            return Ok(host::to_ansi(&abs).len().max(full.len()) as u32 + 1);
        }
        let n = copy_out(&full, buf, len);
        if !part.is_null() {
            *part = file_part(&full).map_or(std::ptr::null_mut(), |i| buf.add(i));
        }
        Ok(n)
    })
}

pub unsafe extern "system" fn GetCurrentDirectoryW(len: u32, buf: *mut u16) -> u32 {
    with("GetCurrentDirectoryW", 0, |f| {
        let w: Vec<u16> = f.current_dir().encode_utf16().collect();
        Ok(copy_out(&w, buf, len))
    })
}

pub unsafe extern "system" fn SetCurrentDirectoryW(name: *const u16) -> i32 {
    let raw = host::from_wide(name).unwrap_or_default();
    with("SetCurrentDirectoryW", 0, |f| f.set_current_dir("SetCurrentDirectoryW", &raw))
}

pub unsafe extern "system" fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32 {
    let d = host::from_wide(dir);
    with("GetDiskFreeSpaceExW", 0, |f| f.disk_free("GetDiskFreeSpaceExW", d.as_deref(), [avail, total, free]))
}

/// Not in `OVERRIDES` yet (proposed): the host's returns the host's temp folder. Ours is `T:\`.
pub unsafe extern "system" fn GetTempPathW(len: u32, buf: *mut u16) -> u32 {
    with("GetTempPathW", 0, |f| {
        f.vfs.resolve(r"T:\").map_err(|e| f.fail("GetTempPathW", r"T:\", e))?;
        let w: Vec<u16> = r"T:\".encode_utf16().collect();
        Ok(copy_out(&w, buf, len))
    })
}

// ---- binding --------------------------------------------------------------------------------------------

/// Every Win32 function this crate implements, by the name the title imports. The first 22 are the
/// `files` entries of `crates/runtime` `OVERRIDES`; then proposed additions. The CRT ones are
/// [`crt::CRT_FUNCTIONS`]; [`override_export`] answers for both.
pub const FUNCTIONS: &[&str] = &[
    "CreateFileW",
    "CreateFile2",
    "CreateFileA",
    "DeleteFileW",
    "DeleteFileA",
    "CreateDirectoryW",
    "CreateDirectoryA",
    "RemoveDirectoryW",
    "MoveFileExW",
    "GetFileAttributesW",
    "GetFileAttributesA",
    "GetFileAttributesExW",
    "GetFileAttributesExA",
    "FindFirstFileW",
    "FindFirstFileExW",
    "FindFirstFileA",
    "FindFirstFileExA",
    "GetFullPathNameW",
    "GetFullPathNameA",
    "GetDiskFreeSpaceExW",
    "GetCurrentDirectoryW",
    "SetCurrentDirectoryW",
    // proposed: our search handles need these; the host's would take console paths
    "FindNextFileW",
    "FindNextFileA",
    "FindClose",
    "SetFileAttributesW",
    "GetTempPathW",
    // F5 (round 2)
    "CopyFile2",
    "CreateHardLinkW",
    "CreateSymbolicLinkW",
    "WerRegisterFile",
    "SetFileInformationByHandle",
];

/// The address of our implementation of `name`, for the binding resolver.
pub fn override_export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "CreateFileW" => CreateFileW as *const (),
        "CreateFile2" => CreateFile2 as *const (),
        "CreateFileA" => CreateFileA as *const (),
        "DeleteFileW" => DeleteFileW as *const (),
        "DeleteFileA" => DeleteFileA as *const (),
        "CreateDirectoryW" => CreateDirectoryW as *const (),
        "CreateDirectoryA" => CreateDirectoryA as *const (),
        "RemoveDirectoryW" => RemoveDirectoryW as *const (),
        "MoveFileExW" => MoveFileExW as *const (),
        "GetFileAttributesW" => GetFileAttributesW as *const (),
        "GetFileAttributesA" => GetFileAttributesA as *const (),
        "GetFileAttributesExW" => GetFileAttributesExW as *const (),
        "GetFileAttributesExA" => GetFileAttributesExA as *const (),
        "FindFirstFileW" => FindFirstFileW as *const (),
        "FindFirstFileExW" => FindFirstFileExW as *const (),
        "FindFirstFileA" => FindFirstFileA as *const (),
        "FindFirstFileExA" => FindFirstFileExA as *const (),
        "GetFullPathNameW" => GetFullPathNameW as *const (),
        "GetFullPathNameA" => GetFullPathNameA as *const (),
        "GetDiskFreeSpaceExW" => GetDiskFreeSpaceExW as *const (),
        "GetCurrentDirectoryW" => GetCurrentDirectoryW as *const (),
        "SetCurrentDirectoryW" => SetCurrentDirectoryW as *const (),
        "FindNextFileW" => FindNextFileW as *const (),
        "FindNextFileA" => FindNextFileA as *const (),
        "FindClose" => FindClose as *const (),
        "SetFileAttributesW" => SetFileAttributesW as *const (),
        "GetTempPathW" => GetTempPathW as *const (),
        "CopyFile2" => CopyFile2 as *const (),
        "CreateHardLinkW" => CreateHardLinkW as *const (),
        "CreateSymbolicLinkW" => CreateSymbolicLinkW as *const (),
        "WerRegisterFile" => WerRegisterFile as *const (),
        "SetFileInformationByHandle" => SetFileInformationByHandle as *const (),
        other => return crt::crt_export(other),
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests;
