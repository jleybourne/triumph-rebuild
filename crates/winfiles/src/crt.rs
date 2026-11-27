//! F4: the C runtime's path-taking file functions the title imports.
//!
//! The title's CRT is the host's `ucrtbase.dll` (through `api-ms-win-crt-*`), and its own `fopen`
//! calls the host's `CreateFileW`, so it would never see the overlay. These overrides open through
//! **our** `CreateFile` core, with the flags the CRT itself derives from `oflag` / the mode string,
//! then hand the host handle to the **same** `ucrtbase.dll` with `_open_osfhandle` and `_fdopen`.
//! The descriptor and the `FILE*` are the host CRT's own, so `fread`, `fwrite`, `fclose`, `_read`,
//! `_write` and `_close` stay bound to the host. `errno` and `_doserrno` are set as the CRT sets them.
//! Checked side by side against the host CRT on the same tree (`tests::f4_*`).

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::host::{self, HANDLE, INVALID_HANDLE_VALUE};
use crate::{
    discover, installed, Files, ERROR_NOT_READY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_DELETE_ON_CLOSE, GENERIC_READ, GENERIC_WRITE, MOVEFILE_COPY_ALLOWED,
};

/// `struct _stat64`, as `ucrtbase` lays it out.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Stat64 {
    pub st_dev: u32,
    pub st_ino: u16,
    pub st_mode: u16,
    pub st_nlink: i16,
    pub st_uid: i16,
    pub st_gid: i16,
    pub st_rdev: u32,
    pub st_size: i64,
    pub st_atime: i64,
    pub st_mtime: i64,
    pub st_ctime: i64,
}

macro_rules! ucrt_table {
    ($($name:ident : fn($($a:ty),*) -> $r:ty;)*) => {
        /// Functions of the host's `ucrtbase.dll`: the CRT instance the title is bound to.
        #[allow(non_snake_case)]
        pub struct Ucrt { $(pub $name: unsafe extern "C" fn($($a),*) -> $r,)* }

        pub fn ucrt() -> &'static Ucrt {
            static U: OnceLock<Ucrt> = OnceLock::new();
            U.get_or_init(|| unsafe {
                let m = host::LoadLibraryExW(host::wide("ucrtbase.dll").as_ptr(), std::ptr::null_mut(), 0x800);
                assert!(!m.is_null(), "ucrtbase.dll");
                Ucrt { $($name: {
                    let p = host::GetProcAddress(m, concat!(stringify!($name), "\0").as_ptr());
                    assert!(!p.is_null(), concat!("ucrtbase!", stringify!($name)));
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($a),*) -> $r>(p)
                },)* }
            })
        }
    };
}

ucrt_table! {
    _open_osfhandle: fn(isize, i32) -> i32;
    _fdopen: fn(i32, *const u8) -> *mut c_void;
    _close: fn(i32) -> i32;
    _errno: fn() -> *mut i32;
    __doserrno: fn() -> *mut u32;
    _get_fmode: fn(*mut i32) -> i32;
    malloc: fn(usize) -> *mut c_void;
    free: fn(*mut c_void) -> ();
    _wstat64: fn(*const u16, *mut Stat64) -> i32;
    // the host's own versions, for the side-by-side tests and nothing else
    fopen: fn(*const u8, *const u8) -> *mut c_void;
    _wfopen: fn(*const u16, *const u16) -> *mut c_void;
    _fsopen: fn(*const u8, *const u8, i32) -> *mut c_void;
    _wfsopen: fn(*const u16, *const u16, i32) -> *mut c_void;
    _open: fn(*const u8, i32, i32) -> i32;
    _wopen: fn(*const u16, i32, i32) -> i32;
    remove: fn(*const u8) -> i32;
    _wremove: fn(*const u16) -> i32;
    _wrename: fn(*const u16, *const u16) -> i32;
    _wrmdir: fn(*const u16) -> i32;
    _wchdir: fn(*const u16) -> i32;
    _wgetcwd: fn(*mut u16, i32) -> *mut u16;
    fread: fn(*mut c_void, usize, usize, *mut c_void) -> usize;
    fwrite: fn(*const c_void, usize, usize, *mut c_void) -> usize;
    fclose: fn(*mut c_void) -> i32;
    ftell: fn(*mut c_void) -> i32;
    _read: fn(i32, *mut c_void, u32) -> i32;
    _write: fn(i32, *const c_void, u32) -> i32;
}

pub const O_WRONLY: i32 = 0x1;
pub const O_RDWR: i32 = 0x2;
pub const O_APPEND: i32 = 0x8;
pub const O_RANDOM: i32 = 0x10;
pub const O_SEQUENTIAL: i32 = 0x20;
pub const O_TEMPORARY: i32 = 0x40;
pub const O_NOINHERIT: i32 = 0x80;
pub const O_CREAT: i32 = 0x100;
pub const O_TRUNC: i32 = 0x200;
pub const O_EXCL: i32 = 0x400;
pub const O_SHORT_LIVED: i32 = 0x1000;
pub const O_OBTAIN_DIR: i32 = 0x2000;
pub const O_TEXT: i32 = 0x4000;
pub const O_BINARY: i32 = 0x8000;
pub const O_WTEXT: i32 = 0x10000;
pub const O_U16TEXT: i32 = 0x20000;
pub const O_U8TEXT: i32 = 0x40000;
const UNICODE_TEXT: i32 = O_WTEXT | O_U16TEXT | O_U8TEXT;
pub const S_IWRITE: i32 = 0x80;
pub const SH_DENYNO: i32 = 0x40;

pub const ENOENT: i32 = 2;
pub const EACCES: i32 = 13;
pub const EEXIST: i32 = 17;
pub const EINVAL: i32 = 22;
pub const ERANGE: i32 = 34;

/// The CRT's own table from a Win32 error to `errno` (`_dosmaperr`).
pub fn errno_of(code: u32) -> i32 {
    match code {
        1 | 12 | 13 | 87 => EINVAL,
        2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => ENOENT,
        4 => 24,           // EMFILE
        5 | 16 | 65 | 82 | 83 | 108 | 132 | 158 => EACCES,
        6 | 114 | 130 => 9, // EBADF
        7..=9 | 1816 => 12, // ENOMEM
        10 => 7,           // E2BIG
        11 => 8,           // ENOEXEC
        17 => 18,          // EXDEV
        80 | 183 => EEXIST,
        89 | 164 | 215 => 11, // EAGAIN
        109 => 32,         // EPIPE
        112 => 28,         // ENOSPC
        128 | 129 => 10,   // ECHILD
        145 => 41,         // ENOTEMPTY
        19..=36 => EACCES,
        188..=202 => 8,
        _ => EINVAL,
    }
}

/// A failure, as the CRT reports one.
#[derive(Debug, Clone, Copy)]
struct Fail {
    doserrno: u32,
    errno: i32,
}

impl Fail {
    fn win(code: u32) -> Fail {
        Fail { doserrno: code, errno: errno_of(code) }
    }
    fn errno(errno: i32) -> Fail {
        Fail { doserrno: 0, errno }
    }
}

fn set_errno(f: Fail) {
    let u = ucrt();
    unsafe {
        *(u._errno)() = f.errno;
        *(u.__doserrno)() = f.doserrno;
    }
}

fn crt<T>(function: &'static str, fail: T, f: impl FnOnce(&Files) -> Result<T, Fail>) -> T {
    let Some(files) = installed() else {
        discover(function, "", "no file system installed");
        set_errno(Fail::win(ERROR_NOT_READY));
        return fail;
    };
    match f(&files) {
        Ok(v) => v,
        Err(e) => {
            set_errno(e);
            fail
        }
    }
}

fn last_fail() -> Fail {
    Fail::win(unsafe { host::GetLastError() })
}

// ---- open ------------------------------------------------------------------------------------------

impl Files {
    /// `_wsopen`'s work: `oflag` to `CreateFile` as the CRT derives it, through our core, then the
    /// host CRT's descriptor for the host handle.
    fn crt_open(&self, function: &'static str, raw: &str, oflag: i32, pmode: i32, shflag: i32) -> Result<i32, Fail> {
        let mut access = match oflag & 3 {
            0 => GENERIC_READ,
            O_WRONLY => GENERIC_WRITE,
            O_RDWR => GENERIC_READ | GENERIC_WRITE,
            _ => return Err(Fail::errno(EINVAL)),
        };
        let disposition = match oflag & (O_CREAT | O_EXCL | O_TRUNC) {
            0 | O_EXCL => crate::OPEN_EXISTING,
            O_CREAT => crate::OPEN_ALWAYS,
            x if x == O_CREAT | O_EXCL || x == O_CREAT | O_TRUNC | O_EXCL => crate::CREATE_NEW,
            x if x == O_TRUNC || x == O_TRUNC | O_EXCL => crate::TRUNCATE_EXISTING,
            _ => crate::CREATE_ALWAYS, // O_CREAT | O_TRUNC
        };
        let mut share = match shflag {
            0x10 => 0,
            0x20 => 1,
            0x30 => 2,
            0x40 => 3,
            0x80 if access == GENERIC_READ => 1,
            0x80 => 0,
            _ => return Err(Fail::errno(EINVAL)),
        };
        let mut attributes = 0x80; // FILE_ATTRIBUTE_NORMAL
        if oflag & O_CREAT != 0 && pmode & S_IWRITE == 0 {
            attributes = crate::FILE_ATTRIBUTE_READONLY;
        }
        if oflag & O_SHORT_LIVED != 0 {
            attributes = (attributes & !0x80) | 0x100;
        }
        let mut flags = attributes;
        if oflag & O_TEMPORARY != 0 {
            flags |= FILE_FLAG_DELETE_ON_CLOSE;
            access |= 0x1_0000; // DELETE
            share |= 4;
        }
        if oflag & O_SEQUENTIAL != 0 {
            flags |= 0x0800_0000;
        } else if oflag & O_RANDOM != 0 {
            flags |= 0x1000_0000;
        }
        if oflag & O_OBTAIN_DIR != 0 {
            flags |= FILE_FLAG_BACKUP_SEMANTICS;
        }
        let h = self
            .create(function, raw, access, disposition, attributes, flags, |p| unsafe {
                host::CreateFileW(p.as_ptr(), access, share, std::ptr::null(), crate::OPEN_EXISTING, flags, std::ptr::null_mut())
            })
            .map_err(Fail::win)?;
        if h == INVALID_HANDLE_VALUE {
            return Err(last_fail());
        }
        let mut text = oflag & (O_TEXT | O_BINARY | UNICODE_TEXT);
        if text == 0 {
            let mut m = 0;
            unsafe { (ucrt()._get_fmode)(&mut m) };
            text = m & O_TEXT;
        }
        let text = text & !O_BINARY;
        if text & UNICODE_TEXT != 0 {
            discover(function, raw, "Unicode text mode: the byte-order mark is not read or written");
        }
        if text & O_TEXT != 0 && oflag & O_RDWR != 0 {
            unsafe { truncate_ctrl_z(h) };
        }
        let fd = unsafe { (ucrt()._open_osfhandle)(h as isize, (oflag & (O_APPEND | O_NOINHERIT)) | text) };
        if fd == -1 {
            unsafe { host::CloseHandle(h) };
            return Err(Fail { doserrno: 0, errno: unsafe { *(ucrt()._errno)() } });
        }
        Ok(fd)
    }

    fn crt_fopen(&self, function: &'static str, raw: &str, mode: &str, shflag: i32) -> Result<*mut c_void, Fail> {
        let Some((oflag, fdmode)) = parse_mode(mode) else {
            // the host CRT calls the invalid-parameter handler here; we fail cleanly
            discover(function, mode, "invalid fopen mode");
            return Err(Fail::errno(EINVAL));
        };
        let fd = self.crt_open(function, raw, oflag, 0x180, shflag)?;
        let m: Vec<u8> = fdmode.bytes().chain(std::iter::once(0)).collect();
        let f = unsafe { (ucrt()._fdopen)(fd, m.as_ptr()) };
        if f.is_null() {
            let e = unsafe { *(ucrt()._errno)() };
            unsafe { (ucrt()._close)(fd) };
            return Err(Fail::errno(e));
        }
        Ok(f)
    }
}

/// In text mode opened for reading and writing, the CRT drops a final Ctrl-Z.
unsafe fn truncate_ctrl_z(h: HANDLE) {
    let mut end = 0i64;
    if host::SetFilePointerEx(h, -1, &mut end, 2) == 0 {
        return;
    }
    let mut b = 0u8;
    let mut n = 0u32;
    if host::ReadFile(h, &mut b as *mut u8 as *mut c_void, 1, &mut n, std::ptr::null_mut()) != 0 && n == 1 && b == 0x1A {
        host::SetFilePointerEx(h, end, std::ptr::null_mut(), 0);
        host::SetEndOfFile(h);
    }
    host::SetFilePointerEx(h, 0, std::ptr::null_mut(), 0);
}

/// The CRT's mode string: `oflag`, and the mode `_fdopen` takes for the stream.
pub fn parse_mode(mode: &str) -> Option<(i32, String)> {
    let s = mode.trim_start_matches(' ');
    let mut it = s.char_indices();
    let (_, first) = it.next()?;
    let mut oflag = match first {
        'r' => 0,
        'w' => O_WRONLY | O_CREAT | O_TRUNC,
        'a' => O_WRONLY | O_CREAT | O_APPEND,
        _ => return None,
    };
    let mut fd = first.to_string();
    let (mut plus, mut tb, mut commit, mut sr, mut t, mut d, mut n, mut x) = (false, false, false, false, false, false, false, false);
    while let Some((i, c)) = it.next() {
        match c {
            '+' if !plus => {
                plus = true;
                oflag = (oflag & !3) | O_RDWR;
                fd.push('+');
            }
            't' if !tb => {
                tb = true;
                oflag |= O_TEXT;
                fd.push('t');
            }
            'b' if !tb => {
                tb = true;
                oflag |= O_BINARY;
                fd.push('b');
            }
            'c' | 'n' if !commit => {
                commit = true;
                fd.push(c);
            }
            'S' | 'R' if !sr => {
                sr = true;
                oflag |= if c == 'S' { O_SEQUENTIAL } else { O_RANDOM };
            }
            'T' if !t => {
                t = true;
                oflag |= O_SHORT_LIVED;
            }
            'D' if !d => {
                d = true;
                oflag |= O_TEMPORARY;
            }
            'N' if !n => {
                n = true;
                oflag |= O_NOINHERIT;
            }
            'x' if first == 'w' && !x => {
                x = true;
                oflag |= O_EXCL;
            }
            ' ' => {}
            ',' => {
                let rest = s[i + 1..].trim_start_matches(' ');
                let rest = rest.strip_prefix("ccs")?.trim_start_matches(' ').strip_prefix('=')?.trim_start_matches(' ');
                oflag |= match rest.to_ascii_uppercase().as_str() {
                    "UNICODE" => O_WTEXT,
                    "UTF-8" => O_U8TEXT,
                    "UTF-16LE" => O_U16TEXT,
                    _ => return None,
                };
                break;
            }
            _ => return None,
        }
    }
    Some((oflag, fd))
}

unsafe fn ansi(p: *const u8) -> String {
    host::from_ansi(p).unwrap_or_default()
}

unsafe fn wide(p: *const u16) -> String {
    host::from_wide(p).unwrap_or_default()
}

pub unsafe extern "C" fn fopen(name: *const u8, mode: *const u8) -> *mut c_void {
    let (raw, mode) = (ansi(name), ansi(mode));
    crt("fopen", std::ptr::null_mut(), |f| f.crt_fopen("fopen", &raw, &mode, SH_DENYNO))
}

pub unsafe extern "C" fn _wfopen(name: *const u16, mode: *const u16) -> *mut c_void {
    let (raw, mode) = (wide(name), wide(mode));
    crt("_wfopen", std::ptr::null_mut(), |f| f.crt_fopen("_wfopen", &raw, &mode, SH_DENYNO))
}

pub unsafe extern "C" fn _fsopen(name: *const u8, mode: *const u8, shflag: i32) -> *mut c_void {
    let (raw, mode) = (ansi(name), ansi(mode));
    crt("_fsopen", std::ptr::null_mut(), |f| f.crt_fopen("_fsopen", &raw, &mode, shflag))
}

pub unsafe extern "C" fn _wfsopen(name: *const u16, mode: *const u16, shflag: i32) -> *mut c_void {
    let (raw, mode) = (wide(name), wide(mode));
    crt("_wfsopen", std::ptr::null_mut(), |f| f.crt_fopen("_wfsopen", &raw, &mode, shflag))
}

/// `int _open(const char *, int, ...)`: on x64 the variadic `pmode` arrives as the third argument.
pub unsafe extern "C" fn _open(name: *const u8, oflag: i32, pmode: i32) -> i32 {
    let raw = ansi(name);
    crt("_open", -1, |f| f.crt_open("_open", &raw, oflag, pmode, SH_DENYNO))
}

pub unsafe extern "C" fn _wopen(name: *const u16, oflag: i32, pmode: i32) -> i32 {
    let raw = wide(name);
    crt("_wopen", -1, |f| f.crt_open("_wopen", &raw, oflag, pmode, SH_DENYNO))
}

// ---- stat, remove, rename, directories -----------------------------------------------------------

impl Files {
    fn crt_stat(&self, function: &'static str, raw: &str, out: *mut Stat64) -> Result<i32, Fail> {
        if raw.contains(['*', '?']) {
            return Err(Fail::errno(ENOENT));
        }
        let path = self.path(function, raw).map_err(Fail::win)?;
        let r = self.vfs.resolve(&path).map_err(|e| Fail::win(self.fail(function, &path, e)))?;
        // the host CRT on the host path: the same mode bits, sizes and times; a trailing separator kept
        let mut w = host::host_path_plain(&r.host);
        w.pop();
        if raw.ends_with(['\\', '/']) && w.last() != Some(&92) {
            w.push(92);
        }
        w.push(0);
        let rc = unsafe { (ucrt()._wstat64)(w.as_ptr(), out) };
        if rc == 0 {
            let drive = crate::drive_of(&path).map_or(0, |c| c as u32 - 'A' as u32);
            unsafe {
                (*out).st_dev = drive;
                (*out).st_rdev = drive;
            }
        }
        Ok(rc)
    }
}

pub unsafe extern "C" fn _wstat64(name: *const u16, out: *mut Stat64) -> i32 {
    let raw = wide(name);
    if out.is_null() {
        set_errno(Fail::errno(EINVAL));
        return -1;
    }
    crt("_wstat64", -1, |f| f.crt_stat("_wstat64", &raw, out))
}

fn zero_ok(r: Result<(), u32>) -> Result<i32, Fail> {
    r.map(|_| 0).map_err(Fail::win)
}

pub unsafe extern "C" fn remove(name: *const u8) -> i32 {
    let raw = ansi(name);
    crt("remove", -1, |f| zero_ok(f.delete_file("remove", &raw)))
}

pub unsafe extern "C" fn _wremove(name: *const u16) -> i32 {
    let raw = wide(name);
    crt("_wremove", -1, |f| zero_ok(f.delete_file("_wremove", &raw)))
}

/// As the CRT: `MoveFileExW(old, new, MOVEFILE_COPY_ALLOWED)`.
pub unsafe extern "C" fn _wrename(old: *const u16, new: *const u16) -> i32 {
    let (from, to) = (wide(old), wide(new));
    crt("_wrename", -1, |f| zero_ok(f.move_file("_wrename", &from, Some(&to), MOVEFILE_COPY_ALLOWED)))
}

pub unsafe extern "C" fn _wrmdir(name: *const u16) -> i32 {
    let raw = wide(name);
    crt("_wrmdir", -1, |f| zero_ok(f.remove_dir("_wrmdir", &raw)))
}

pub unsafe extern "C" fn _wchdir(name: *const u16) -> i32 {
    let raw = wide(name);
    crt("_wchdir", -1, |f| f.set_current_dir("_wchdir", &raw).map(|_| 0).map_err(Fail::win))
}

/// `buf` NULL: a buffer from the host CRT's `malloc` (the title frees it with the host's `free`),
/// at least `maxlen` characters.
pub unsafe extern "C" fn _wgetcwd(buf: *mut u16, maxlen: i32) -> *mut u16 {
    crt("_wgetcwd", std::ptr::null_mut(), |f| {
        let cwd: Vec<u16> = f.current_dir().encode_utf16().collect();
        let need = cwd.len() + 1;
        let out = if buf.is_null() {
            let n = need.max(maxlen.max(0) as usize);
            let p = (ucrt().malloc)(n * 2) as *mut u16;
            if p.is_null() {
                return Err(Fail::errno(12));
            }
            p
        } else {
            if maxlen <= 0 {
                return Err(Fail::errno(EINVAL));
            }
            if need > maxlen as usize {
                return Err(Fail::errno(ERANGE));
            }
            buf
        };
        std::ptr::copy_nonoverlapping(cwd.as_ptr(), out, cwd.len());
        *out.add(cwd.len()) = 0;
        Ok(out)
    })
}

/// The CRT functions this module overrides, by the name the title imports.
pub const CRT_FUNCTIONS: &[&str] =
    &["fopen", "_wfopen", "_fsopen", "_wfsopen", "_open", "_wopen", "_wstat64", "remove", "_wremove", "_wrename", "_wrmdir", "_wchdir", "_wgetcwd"];

pub fn crt_export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "fopen" => fopen as *const (),
        "_wfopen" => _wfopen as *const (),
        "_fsopen" => _fsopen as *const (),
        "_wfsopen" => _wfsopen as *const (),
        "_open" => _open as *const (),
        "_wopen" => _wopen as *const (),
        "_wstat64" => _wstat64 as *const (),
        "remove" => remove as *const (),
        "_wremove" => _wremove as *const (),
        "_wrename" => _wrename as *const (),
        "_wrmdir" => _wrmdir as *const (),
        "_wchdir" => _wchdir as *const (),
        "_wgetcwd" => _wgetcwd as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}
