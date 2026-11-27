use super::*;
use crate::find::open_searches;
use crate::host::{WIN32_FILE_ATTRIBUTE_DATA, WIN32_FIND_DATAA, WIN32_FIND_DATAW};
use rebuild_vfs::{Mod, ModSet, VfsBuilder};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The overrides share one installed file system: tests take turns.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir()
            .join(format!("rebuild-winfiles-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn put(root: &Path, rel: &str, body: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn w(s: &str) -> Vec<u16> {
    host::wide(s)
}

fn a(s: &str) -> Vec<u8> {
    let mut v = host::to_ansi(s);
    v.push(0);
    v
}

fn last() -> u32 {
    unsafe { host::GetLastError() }
}

/// Every name, size, time, read-only bit and byte under `root`.
fn hash_tree(root: &Path) -> u64 {
    fn walk(dir: &Path, rel: &str, h: &mut DefaultHasher) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let rel = format!("{rel}/{}", e.file_name().to_string_lossy());
            let meta = fs::metadata(e.path()).unwrap();
            h.write(rel.as_bytes());
            h.write_u64(meta.len());
            h.write_u8(meta.permissions().readonly() as u8);
            h.write_u128(meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
            if meta.is_dir() {
                walk(&e.path(), &rel, h);
            } else {
                h.write(&fs::read(e.path()).unwrap());
            }
        }
    }
    let mut h = DefaultHasher::new();
    walk(root, "", &mut h);
    h.finish()
}

/// A base, one mod and an overlay; `G:` the package over them, `T:` and `S:` writable.
fn setup(s: &Scratch) -> (PathBuf, PathBuf) {
    let base = s.0.join("base");
    let m = s.0.join("mods").join("m1");
    put(&base, "a.txt", b"base a");
    put(&base, r"dir\x.txt", b"xx");
    put(&base, r"dir\y.txt", b"base y");
    put(&base, r"dir\z.txt", b"zzz");
    put(&base, r"Data\Big.RPF", &vec![7u8; 5000]);
    put(&m, r"dir\y.txt", b"mod y, longer");
    put(&m, r"dir\m.txt", b"from the mod");
    let mods = ModSet { mods: vec![Mod { name: "m1".into(), root: m.clone(), enabled: true }] };
    let vfs = VfsBuilder::new(s.0.join("overlay"))
        .package('G', &base, &mods)
        .writable('T')
        .writable('S')
        .build()
        .unwrap();
    install(Files::new(vfs, r"G:\"));
    (base, m)
}

unsafe fn read_all(h: HANDLE) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut n = 0u32;
        if host::ReadFile(h, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut n, std::ptr::null_mut()) == 0 || n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
}

unsafe fn write_all(h: HANDLE, data: &[u8]) {
    let mut n = 0u32;
    assert_ne!(host::WriteFile(h, data.as_ptr() as *const c_void, data.len() as u32, &mut n, std::ptr::null_mut()), 0);
    assert_eq!(n as usize, data.len());
}

unsafe fn names_w(pattern: &str) -> Result<Vec<String>, u32> {
    let mut d = WIN32_FIND_DATAW::default();
    let h = FindFirstFileW(w(pattern).as_ptr(), &mut d);
    if h == INVALID_HANDLE_VALUE {
        return Err(last());
    }
    let mut out = vec![d.name()];
    while FindNextFileW(h, &mut d) != 0 {
        out.push(d.name());
    }
    assert_eq!(last(), ERROR_NO_MORE_FILES);
    assert_eq!(FindClose(h), 1);
    Ok(out)
}

// ---- F1: each function over the layers -----------------------------------------------------------

#[test]
fn f1_create_file_reads_the_layers_and_writes_only_the_overlay() {
    let _g = lock();
    let s = Scratch::new("create");
    let (base, m) = setup(&s);
    let (hb, hm) = (hash_tree(&base), hash_tree(&m));
    unsafe {
        // reads: base, mod, case-insensitive, relative to the current directory; host handles
        for (p, want) in [(r"G:\a.txt", &b"base a"[..]), (r"g:\DIR\Y.TXT", b"mod y, longer"), ("dir/x.txt", b"xx")] {
            let h = CreateFileW(w(p).as_ptr(), GENERIC_READ, 1, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            assert_ne!(h, INVALID_HANDLE_VALUE, "{p}");
            assert_eq!(read_all(h), want, "{p}");
            assert_ne!(host::CloseHandle(h), 0, "the host's CloseHandle takes it");
        }
        // a write over a base file: copied up, the base keeps its bytes
        let h = CreateFileW(w(r"G:\a.txt").as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_ALWAYS, 0, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(last(), ERROR_ALREADY_EXISTS);
        assert_eq!(read_all(h), b"base a", "copy-up keeps the contents");
        write_all(h, b"+more");
        host::CloseHandle(h);
        assert_eq!(fs::read(base.join("a.txt")).unwrap(), b"base a");
        assert_eq!(fs::read(s.0.join(r"overlay\G\files\a.txt")).unwrap(), b"base a+more");
        // CreateFileA and CreateFile2 create in the overlay
        let h = CreateFileA(a(r"T:\ansi.txt").as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_NEW, 0, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(last(), 0);
        write_all(h, b"ansi");
        host::CloseHandle(h);
        let params = host::CREATEFILE2_EXTENDED_PARAMETERS {
            dwSize: std::mem::size_of::<host::CREATEFILE2_EXTENDED_PARAMETERS>() as u32,
            dwFileAttributes: 0x2, // hidden
            dwFileFlags: 0,
            dwSecurityQosFlags: 0,
            lpSecurityAttributes: std::ptr::null_mut(),
            hTemplateFile: std::ptr::null_mut(),
        };
        let h = CreateFile2(w(r"S:\save\p.dat").as_ptr(), GENERIC_WRITE, 0, CREATE_NEW, &params);
        assert_eq!(h, INVALID_HANDLE_VALUE, "no directory yet");
        assert_eq!(last(), ERROR_PATH_NOT_FOUND);
        assert_eq!(CreateDirectoryW(w(r"S:\save").as_ptr(), std::ptr::null()), 1);
        let h = CreateFile2(w(r"S:\save\p.dat").as_ptr(), GENERIC_WRITE, 0, CREATE_NEW, &params);
        assert_ne!(h, INVALID_HANDLE_VALUE);
        host::CloseHandle(h);
        assert_eq!(GetFileAttributesW(w(r"S:\save\p.dat").as_ptr()) & 0x22, 0x22, "the new file's attributes are the title's");
        assert_eq!(fs::read(s.0.join(r"overlay\T\files\ansi.txt")).unwrap(), b"ansi");
        // a mod's file written over: copied up from the mod
        let h = CreateFileW(w(r"G:\dir\y.txt").as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        host::CloseHandle(h);
        assert_eq!(fs::read(s.0.join(r"overlay\G\files\dir\y.txt")).unwrap(), b"mod y, longer");
        // a directory below the overlay: read opens it where it is; write gets the overlay's copy (F5)
        let h = CreateFileW(w(r"G:\dir").as_ptr(), GENERIC_READ, 7, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        host::CloseHandle(h);
        assert!(!s.0.join(r"overlay\G\files\Data").exists());
        let h = CreateFileW(w(r"G:\Data").as_ptr(), GENERIC_WRITE, 7, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        host::CloseHandle(h);
        assert!(s.0.join(r"overlay\G\files\Data").is_dir(), "materialized in the overlay");
        assert_eq!(names_w(r"G:\Data\*").unwrap(), [".", "..", "Big.RPF"], "its files still merge from below");
    }
    assert_eq!((hash_tree(&base), hash_tree(&m)), (hb, hm), "base and mod unchanged");
}

#[test]
fn f1_find_merges_overlay_mod_and_base() {
    let _g = lock();
    let s = Scratch::new("find");
    let (base, _) = setup(&s);
    unsafe {
        assert_eq!(DeleteFileW(w(r"G:\dir\z.txt").as_ptr()), 1, "a whiteout");
        let h = CreateFileW(w(r"G:\dir\new.txt").as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_NEW, 0, std::ptr::null_mut());
        host::CloseHandle(h);
        assert_eq!(names_w(r"G:\dir\*").unwrap(), [".", "..", "m.txt", "new.txt", "x.txt", "y.txt"]);
        assert_eq!(names_w(r"G:\DIR\?.txt").unwrap(), ["m.txt", "x.txt", "y.txt"], "host wildcards, per layer");
        assert_eq!(names_w(r"G:\dir\z.txt"), Err(ERROR_FILE_NOT_FOUND), "whiteouts never show");
        assert_eq!(names_w(r"G:\nodir\*"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(names_w(r"G:\*").unwrap(), ["a.txt", "Data", "dir"], "a drive root has no dots");
        assert!(z_exists(&base), "the base keeps the deleted file");

        // the winning layer's own find data: y.txt's size is the mod's
        let mut d = WIN32_FIND_DATAW::default();
        let h = FindFirstFileExW(w(r"G:\dir\y.txt").as_ptr(), 1, &mut d as *mut _ as *mut c_void, 0, std::ptr::null(), 2);
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!((d.name(), d.size()), ("y.txt".to_string(), 13));
        assert_eq!(FindClose(h), 1);

        // A forms
        let mut da = WIN32_FIND_DATAA::default();
        let h = FindFirstFileA(a(r"G:\Data\*.rpf").as_ptr(), &mut da);
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(&da.cFileName[..8], b"Big.RPF\0");
        assert_eq!(FindNextFileA(h, &mut da), 0);
        assert_eq!(last(), ERROR_NO_MORE_FILES);
        assert_eq!(FindClose(h), 1);
        let h = FindFirstFileExA(a(r"G:\Data\*").as_ptr(), 0, &mut da as *mut _ as *mut c_void, 0, std::ptr::null(), 0);
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(FindClose(h), 1);
        assert_eq!(open_searches(), 0, "every search closed");
        assert_eq!(FindClose(h), 0, "a closed search is refused");
    }
}

fn z_exists(base: &Path) -> bool {
    base.join(r"dir\z.txt").exists()
}

#[test]
fn f1_delete_directories_and_move() {
    let _g = lock();
    let s = Scratch::new("move");
    let (base, _) = setup(&s);
    let hb = hash_tree(&base);
    unsafe {
        assert_eq!(DeleteFileA(a(r"G:\a.txt").as_ptr()), 1);
        assert_eq!(GetFileAttributesA(a(r"G:\a.txt").as_ptr()), u32::MAX);
        assert_eq!(last(), ERROR_FILE_NOT_FOUND);
        assert_eq!(CreateDirectoryA(a(r"G:\new").as_ptr(), std::ptr::null()), 1);
        assert_eq!(CreateDirectoryW(w(r"G:\new").as_ptr(), std::ptr::null()), 0);
        assert_eq!(last(), ERROR_ALREADY_EXISTS);
        assert_eq!(RemoveDirectoryW(w(r"G:\dir").as_ptr()), 0);
        assert_eq!(last(), ERROR_DIR_NOT_EMPTY);
        assert_eq!(MoveFileExW(w(r"G:\dir\x.txt").as_ptr(), w(r"G:\new\x2.txt").as_ptr(), 0), 1);
        assert_eq!(MoveFileExW(w(r"G:\dir").as_ptr(), w(r"G:\dir2").as_ptr(), 0), 1, "a directory from base and mod");
        assert_eq!(names_w(r"G:\dir2\*").unwrap(), [".", "..", "m.txt", "y.txt", "z.txt"]);
        // across drives: only with MOVEFILE_COPY_ALLOWED
        assert_eq!(MoveFileExW(w(r"G:\dir2\z.txt").as_ptr(), w(r"T:\z.txt").as_ptr(), 0), 0);
        assert_eq!(last(), ERROR_NOT_SAME_DEVICE);
        assert_eq!(MoveFileExW(w(r"G:\dir2\z.txt").as_ptr(), w(r"T:\z.txt").as_ptr(), MOVEFILE_COPY_ALLOWED), 1);
        assert_eq!(fs::read(s.0.join(r"overlay\T\files\z.txt")).unwrap(), b"zzz");
        assert_eq!(GetFileAttributesW(w(r"G:\dir2\z.txt").as_ptr()), u32::MAX);
        assert_eq!(MoveFileExW(w(r"T:\z.txt").as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT), 0);
        // read-only: SetFileAttributesW copies up, and the delete is refused as the host refuses it
        assert_eq!(SetFileAttributesW(w(r"G:\dir2\m.txt").as_ptr(), FILE_ATTRIBUTE_READONLY), 1);
        assert_eq!(DeleteFileW(w(r"G:\dir2\m.txt").as_ptr()), 0);
        assert_eq!(last(), ERROR_ACCESS_DENIED);
        assert_eq!(SetFileAttributesW(w(r"G:\dir2\m.txt").as_ptr(), 0x80), 1);
        assert_eq!(DeleteFileW(w(r"G:\dir2\m.txt").as_ptr()), 1);
        assert_eq!(RemoveDirectoryW(w(r"G:\dir2\y.txt").as_ptr()), 0);
        assert_eq!(last(), ERROR_DIRECTORY);
    }
    assert_eq!(hash_tree(&base), hb, "the base is unchanged");
}

#[test]
fn f1_full_paths_current_directory_disk_and_temp() {
    let _g = lock();
    let s = Scratch::new("paths");
    setup(&s);
    unsafe {
        let full = |p: &str| -> String {
            let mut buf = [0u16; 300];
            let mut part: *mut u16 = std::ptr::null_mut();
            let n = GetFullPathNameW(w(p).as_ptr(), 300, buf.as_mut_ptr(), &mut part) as usize;
            let s = String::from_utf16_lossy(&buf[..n]);
            if !part.is_null() {
                assert_eq!(part.offset_from(buf.as_ptr()) as usize, s.rfind('\\').unwrap() + 1);
            }
            s
        };
        assert_eq!(full(r"G:\dir\..\Data\.\Big.RPF"), r"G:\Data\Big.RPF");
        assert_eq!(full("dir"), r"G:\dir");
        assert_eq!(SetCurrentDirectoryW(w(r"G:\DIR").as_ptr()), 1);
        assert_eq!(full("y.txt"), r"G:\DIR\y.txt", "relative to our current directory");
        assert_eq!(full(r"\a.txt"), r"G:\a.txt", "rooted: the current directory's drive");
        assert_eq!(full("G:y.txt"), r"G:\DIR\y.txt", "X:rel on the current drive");
        assert_eq!(full("T:y.txt"), r"T:\y.txt", "X:rel on another drive: its root");
        assert_eq!(full(r"G:\dir\"), r"G:\dir\", "no file part");
        let mut buf = [0u16; 8];
        assert_eq!(GetCurrentDirectoryW(3, buf.as_mut_ptr()), 7, "too small: the size with the NUL");
        assert_eq!(GetCurrentDirectoryW(8, buf.as_mut_ptr()), 6);
        assert_eq!(String::from_utf16_lossy(&buf[..6]), r"G:\DIR");
        let h = CreateFileW(w("y.txt").as_ptr(), GENERIC_READ, 1, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
        assert_eq!(read_all(h), b"mod y, longer");
        host::CloseHandle(h);
        assert_eq!(SetCurrentDirectoryW(w("y.txt").as_ptr()), 0);
        assert_eq!(last(), ERROR_DIRECTORY);
        assert_eq!(SetCurrentDirectoryW(w("..").as_ptr()), 1);
        assert_eq!(GetCurrentDirectoryW(8, buf.as_mut_ptr()), 3);
        assert_eq!(String::from_utf16_lossy(&buf[..3]), r"G:\");
        let mut ab = [0u8; 64];
        let mut part: *mut u8 = std::ptr::null_mut();
        let n = GetFullPathNameA(a(r"dir\x.txt").as_ptr(), 64, ab.as_mut_ptr(), &mut part) as usize;
        assert_eq!(&ab[..n], br"G:\dir\x.txt");
        assert_eq!(part.offset_from(ab.as_ptr()), 7);
        assert_eq!(GetFullPathNameA(a(r"dir\x.txt").as_ptr(), 4, ab.as_mut_ptr(), &mut part), 13);

        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        assert_eq!(GetDiskFreeSpaceExW(w(r"G:\dir").as_ptr(), &mut avail, &mut total, &mut free), 1);
        assert!(total > 0 && avail > 0 && free >= avail);
        assert_eq!(GetDiskFreeSpaceExW(std::ptr::null(), &mut avail, &mut total, &mut free), 1, "NULL: the current directory");
        assert_eq!(GetDiskFreeSpaceExW(w(r"G:\nodir").as_ptr(), &mut avail, &mut total, &mut free), 0);
        assert_eq!(last(), ERROR_PATH_NOT_FOUND);
        let mut t = [0u16; 8];
        assert_eq!(GetTempPathW(8, t.as_mut_ptr()), 3);
        assert_eq!(String::from_utf16_lossy(&t[..3]), r"T:\");
        let mut info = WIN32_FILE_ATTRIBUTE_DATA::default();
        assert_eq!(GetFileAttributesExA(a(r"G:\Data\big.rpf").as_ptr(), 0, &mut info as *mut _ as *mut c_void), 1);
        assert_eq!(info.nFileSizeLow, 5000);
        assert_eq!(GetFileAttributesExW(w(r"G:\Data").as_ptr(), 0, &mut info as *mut _ as *mut c_void), 1);
        assert_ne!(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY, 0);
    }
}

#[test]
fn f1_every_files_override_is_implemented() {
    // the runtime checks its OVERRIDES "files" list equals FUNCTIONS (crates/runtime exports tests)
    assert_eq!(FUNCTIONS.len(), 32);
    assert_eq!(crt::CRT_FUNCTIONS.len(), 13);
    for f in crt::CRT_FUNCTIONS {
        assert!(override_export(f).is_some(), "{f}");
    }
    for f in FUNCTIONS {
        assert!(override_export(f).is_some(), "{f}");
    }
    assert!(override_export("ReadFile").is_none(), "handle functions stay the host's");
}

// ---- F3: unknown paths are discovery entries, never guessed ------------------------------------------

#[test]
fn f3_unknown_paths_are_logged_and_refused() {
    let _g = lock();
    let s = Scratch::new("discovery");
    setup(&s);
    // a real host file: its host path must not be opened (C: is not a console drive here)
    let host_file = s.0.join("host-only.txt");
    fs::write(&host_file, b"host").unwrap();
    let host_str = host_file.to_string_lossy().into_owned();
    let cases = [
        (r"\\server\share\x.txt", ERROR_PATH_NOT_FOUND),
        (r"\\.\pipe\title", ERROR_PATH_NOT_FOUND),
        ("CONOUT$", ERROR_PATH_NOT_FOUND),
        ("NUL", ERROR_PATH_NOT_FOUND),
        (r"Q:\nothing\mounted.txt", ERROR_PATH_NOT_FOUND),
        (host_str.as_str(), ERROR_PATH_NOT_FOUND),
        (r"G:\a.txt:stream", ERROR_INVALID_NAME),
        ("", ERROR_PATH_NOT_FOUND),
    ];
    unsafe {
        for (p, err) in cases {
            let h = CreateFileW(w(p).as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_ALWAYS, 0, std::ptr::null_mut());
            assert_eq!((h, last()), (INVALID_HANDLE_VALUE, err), "{p}");
        }
        assert_eq!(GetFileAttributesW(w(&host_str).as_ptr()), u32::MAX);
        assert_eq!(DeleteFileW(w(&host_str).as_ptr()), 0);
        let mut d = WIN32_FIND_DATAW::default();
        assert_eq!(FindFirstFileW(w(r"\\server\share\*").as_ptr(), &mut d), INVALID_HANDLE_VALUE);
    }
    assert_eq!(fs::read(&host_file).unwrap(), b"host", "the host file was never touched");
    let d = discoveries();
    for (p, _) in cases.iter().filter(|(p, _)| !p.is_empty()) {
        assert!(d.iter().any(|x| x.function == "CreateFileW" && x.path.contains(p.trim_start_matches(r"\\"))), "{p}: {d:?}");
    }
    assert!(d.iter().any(|x| x.function == "DeleteFileW" && x.path == host_str));
    assert!(d.iter().any(|x| x.function == "FindFirstFileW"));
    let n = d.len();
    unsafe { GetFileAttributesW(w(&host_str).as_ptr()) };
    let d = discoveries();
    assert_eq!(d.len(), n, "a repeat is counted, not listed again");
    assert!(d.iter().any(|x| x.function == "GetFileAttributesW" && x.count >= 2));

    uninstall();
    unsafe {
        let h = CreateFileW(w(r"G:\a.txt").as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
        assert_eq!((h, last()), (INVALID_HANDLE_VALUE, ERROR_NOT_READY), "nothing installed: refused, never the host");
    }
}

// ---- F2: side by side with the host on the same tree -------------------------------------------------

/// One side of the comparison: our functions on `G:\…`, or the host's on the same tree at its host path.
struct Side {
    ours: bool,
    root: String,
}

impl Side {
    fn p(&self, rel: &str) -> String {
        if rel.is_empty() {
            format!("{}\\", self.root)
        } else {
            format!("{}\\{rel}", self.root)
        }
    }
    fn strip(&self, s: &str) -> String {
        s.strip_prefix(&self.root).map(|r| format!("<root>{r}")).unwrap_or_else(|| s.to_owned())
    }

    unsafe fn create(&self, rel: &str, access: u32, disp: u32, flags: u32, write: &[u8]) -> String {
        let p = w(&self.p(rel));
        let h = if self.ours {
            CreateFileW(p.as_ptr(), access, 7, std::ptr::null(), disp, flags, std::ptr::null_mut())
        } else {
            host::CreateFileW(p.as_ptr(), access, 7, std::ptr::null(), disp, flags, std::ptr::null_mut())
        };
        let e = last();
        if h == INVALID_HANDLE_VALUE {
            return format!("CreateFileW {rel} {access:#x} {disp} -> error {e}");
        }
        if !write.is_empty() {
            write_all(h, write);
        }
        let body = if access & GENERIC_READ != 0 && write.is_empty() { read_all(h) } else { Vec::new() };
        host::CloseHandle(h);
        format!("CreateFileW {rel} {access:#x} {disp} -> ok, last error {e}, read {:?}", String::from_utf8_lossy(&body))
    }

    unsafe fn create2(&self, rel: &str, access: u32, disp: u32) -> String {
        let p = w(&self.p(rel));
        let h = if self.ours {
            CreateFile2(p.as_ptr(), access, 7, disp, std::ptr::null())
        } else {
            host::CreateFile2(p.as_ptr(), access, 7, disp, std::ptr::null())
        };
        let e = last();
        if h != INVALID_HANDLE_VALUE {
            host::CloseHandle(h);
            return format!("CreateFile2 {rel} {disp} -> ok, last error {e}");
        }
        format!("CreateFile2 {rel} {disp} -> error {e}")
    }

    unsafe fn create_a(&self, rel: &str, access: u32, disp: u32) -> String {
        let p = a(&self.p(rel));
        let h = if self.ours {
            CreateFileA(p.as_ptr(), access, 7, std::ptr::null(), disp, 0, std::ptr::null_mut())
        } else {
            host::CreateFileA(p.as_ptr(), access, 7, std::ptr::null(), disp, 0, std::ptr::null_mut())
        };
        let e = last();
        if h != INVALID_HANDLE_VALUE {
            host::CloseHandle(h);
            return format!("CreateFileA {rel} {disp} -> ok, last error {e}");
        }
        format!("CreateFileA {rel} {disp} -> error {e}")
    }

    unsafe fn attrs(&self, rel: &str) -> String {
        let (r, ra) = if self.ours {
            (GetFileAttributesW(w(&self.p(rel)).as_ptr()), GetFileAttributesA(a(&self.p(rel)).as_ptr()))
        } else {
            (host::GetFileAttributesW(w(&self.p(rel)).as_ptr()), host::GetFileAttributesA(a(&self.p(rel)).as_ptr()))
        };
        let e = if r == u32::MAX { last() } else { 0 };
        assert_eq!(r, ra, "A and W agree on {rel}");
        format!("GetFileAttributes {rel} -> {r:#x} error {e}")
    }

    unsafe fn attrs_ex(&self, rel: &str, level: i32) -> String {
        let mut i = WIN32_FILE_ATTRIBUTE_DATA::default();
        let mut ia = WIN32_FILE_ATTRIBUTE_DATA::default();
        let ip = &mut i as *mut _ as *mut c_void;
        let iap = &mut ia as *mut _ as *mut c_void;
        let (r, ra) = if self.ours {
            (GetFileAttributesExW(w(&self.p(rel)).as_ptr(), level, ip), GetFileAttributesExA(a(&self.p(rel)).as_ptr(), level, iap))
        } else {
            (host::GetFileAttributesExW(w(&self.p(rel)).as_ptr(), level, ip), host::GetFileAttributesExA(a(&self.p(rel)).as_ptr(), level, iap))
        };
        let e = if r == 0 { last() } else { 0 };
        assert_eq!((r, i.dwFileAttributes, i.nFileSizeLow), (ra, ia.dwFileAttributes, ia.nFileSizeLow));
        format!("GetFileAttributesEx {rel} {level} -> {r} {:#x} {} error {e}", i.dwFileAttributes, i.nFileSizeLow)
    }

    unsafe fn set_attrs(&self, rel: &str, v: u32) -> String {
        let p = w(&self.p(rel));
        let r = if self.ours { SetFileAttributesW(p.as_ptr(), v) } else { host::SetFileAttributesW(p.as_ptr(), v) };
        format!("SetFileAttributesW {rel} {v:#x} -> {r} error {}", if r == 0 { last() } else { 0 })
    }

    unsafe fn bool_op(&self, what: &str, rel: &str, f: unsafe extern "system" fn(*const u16) -> i32, h: unsafe extern "system" fn(*const u16) -> i32) -> String {
        let p = w(&self.p(rel));
        let r = if self.ours { f(p.as_ptr()) } else { h(p.as_ptr()) };
        format!("{what} {rel} -> {r} error {}", if r == 0 { last() } else { 0 })
    }

    unsafe fn delete(&self, rel: &str) -> String {
        self.bool_op("DeleteFileW", rel, DeleteFileW, host::DeleteFileW)
    }
    unsafe fn delete_a(&self, rel: &str) -> String {
        let p = a(&self.p(rel));
        let r = if self.ours { DeleteFileA(p.as_ptr()) } else { host::DeleteFileA(p.as_ptr()) };
        format!("DeleteFileA {rel} -> {r} error {}", if r == 0 { last() } else { 0 })
    }
    unsafe fn rmdir(&self, rel: &str) -> String {
        self.bool_op("RemoveDirectoryW", rel, RemoveDirectoryW, host::RemoveDirectoryW)
    }
    unsafe fn mkdir(&self, rel: &str) -> String {
        let p = w(&self.p(rel));
        let r = if self.ours { CreateDirectoryW(p.as_ptr(), std::ptr::null()) } else { host::CreateDirectoryW(p.as_ptr(), std::ptr::null()) };
        format!("CreateDirectoryW {rel} -> {r} error {}", if r == 0 { last() } else { 0 })
    }
    unsafe fn mkdir_a(&self, rel: &str) -> String {
        let p = a(&self.p(rel));
        let r = if self.ours { CreateDirectoryA(p.as_ptr(), std::ptr::null()) } else { host::CreateDirectoryA(p.as_ptr(), std::ptr::null()) };
        format!("CreateDirectoryA {rel} -> {r} error {}", if r == 0 { last() } else { 0 })
    }
    unsafe fn mv(&self, from: &str, to: Option<&str>, flags: u32) -> String {
        let f = w(&self.p(from));
        let t = to.map(|t| w(&self.p(t)));
        let tp = t.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
        let r = if self.ours { MoveFileExW(f.as_ptr(), tp, flags) } else { host::MoveFileExW(f.as_ptr(), tp, flags) };
        format!("MoveFileExW {from} {to:?} {flags} -> {r} error {}", if r == 0 { last() } else { 0 })
    }

    unsafe fn find(&self, rel: &str, level: i32, flags: u32) -> String {
        let p = w(&self.p(rel));
        let mut d = WIN32_FIND_DATAW::default();
        let dp = &mut d as *mut _ as *mut c_void;
        let h = if self.ours {
            FindFirstFileExW(p.as_ptr(), level, dp, 0, std::ptr::null(), flags)
        } else {
            host::FindFirstFileExW(p.as_ptr(), level, dp, 0, std::ptr::null(), flags)
        };
        if h == INVALID_HANDLE_VALUE {
            return format!("FindFirstFileExW {rel} -> error {}", last());
        }
        let mut out = Vec::new();
        loop {
            // times differ between the two trees; everything else must match
            let alt = String::from_utf16_lossy(&d.cAlternateFileName[..d.cAlternateFileName.iter().position(|&c| c == 0).unwrap_or(14)]);
            let size = if d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 { 0 } else { d.size() };
            out.push(format!("{}|{alt}|{:#x}|{size}", d.name(), d.dwFileAttributes));
            let r = if self.ours { FindNextFileW(h, &mut d) } else { host::FindNextFileW(h, &mut d) };
            if r == 0 {
                break;
            }
        }
        let e = last();
        let c = if self.ours { FindClose(h) } else { host::FindClose(h) };
        format!("FindFirstFileExW {rel} {level} {flags} -> {} end {e} close {c}", out.join(" "))
    }

    unsafe fn find_a(&self, rel: &str) -> String {
        let p = a(&self.p(rel));
        let mut d = WIN32_FIND_DATAA::default();
        let h = if self.ours { FindFirstFileA(p.as_ptr(), &mut d) } else { host::FindFirstFileA(p.as_ptr(), &mut d) };
        if h == INVALID_HANDLE_VALUE {
            return format!("FindFirstFileA {rel} -> error {}", last());
        }
        let mut out = Vec::new();
        loop {
            let n = d.cFileName.iter().position(|&c| c == 0).unwrap();
            out.push(String::from_utf8_lossy(&d.cFileName[..n]).into_owned());
            let r = if self.ours { FindNextFileA(h, &mut d) } else { host::FindNextFileA(h, &mut d) };
            if r == 0 {
                break;
            }
        }
        if self.ours { FindClose(h) } else { host::FindClose(h) };
        format!("FindFirstFileA {rel} -> {}", out.join(" "))
    }

    unsafe fn full(&self, rel: &str, abs: bool) -> String {
        let p = w(&if abs { self.p(rel) } else { rel.to_owned() });
        let mut buf = [0u16; 400];
        let mut part: *mut u16 = std::ptr::null_mut();
        let n = if self.ours {
            GetFullPathNameW(p.as_ptr(), 400, buf.as_mut_ptr(), &mut part)
        } else {
            host::GetFullPathNameW(p.as_ptr(), 400, buf.as_mut_ptr(), &mut part)
        } as usize;
        let s = String::from_utf16_lossy(&buf[..n]);
        let tail = if part.is_null() { -1 } else { n as isize - part.offset_from(buf.as_ptr()) };
        let small = if self.ours {
            GetFullPathNameW(p.as_ptr(), 2, buf.as_mut_ptr(), &mut part)
        } else {
            host::GetFullPathNameW(p.as_ptr(), 2, buf.as_mut_ptr(), &mut part)
        };
        format!("GetFullPathNameW {rel} -> {} part {tail} too-small {}", self.strip(&s), small as isize - n as isize)
    }

    unsafe fn cd(&self, rel: &str) -> String {
        let p = w(rel);
        let r = if self.ours { SetCurrentDirectoryW(p.as_ptr()) } else { host::SetCurrentDirectoryW(p.as_ptr()) };
        let e = if r == 0 { last() } else { 0 };
        let mut buf = [0u16; 400];
        let n = if self.ours { GetCurrentDirectoryW(400, buf.as_mut_ptr()) } else { host::GetCurrentDirectoryW(400, buf.as_mut_ptr()) } as usize;
        // a console drive root keeps its `\`; the host tree is not a drive root
        let now = self.strip(&String::from_utf16_lossy(&buf[..n]));
        format!("SetCurrentDirectoryW {} -> {r} error {e}, now {}", self.strip(rel), now.trim_end_matches('\\'))
    }

    unsafe fn disk(&self, rel: &str) -> String {
        let p = w(&self.p(rel));
        let (mut av, mut total, mut free) = (0u64, 0u64, 0u64);
        let r = if self.ours {
            GetDiskFreeSpaceExW(p.as_ptr(), &mut av, &mut total, &mut free)
        } else {
            host::GetDiskFreeSpaceExW(p.as_ptr(), &mut av, &mut total, &mut free)
        };
        // the overlay and the host tree share the temp volume: the total must match
        format!("GetDiskFreeSpaceExW {rel} -> {r} error {} total {total}", if r == 0 { last() } else { 0 })
    }
}

/// The same script on both sides; every line is one compared outcome.
unsafe fn script(s: &Side) -> Vec<String> {
    let mut o = Vec::new();
    let r = GENERIC_READ;
    let rw = GENERIC_READ | GENERIC_WRITE;
    // reads
    for p in ["a.txt", "sub", r"sub\deep\e.txt", "missing.txt", r"nodir\x.txt", r"a.txt\x", "hidden.txt", "MIXED case.txt", r"sub\deep\"] {
        o.push(s.attrs(p));
        o.push(s.attrs_ex(p, 0));
    }
    o.push(s.attrs_ex("a.txt", 1));
    for p in ["a.txt", "b.bin", "missing.txt", r"nodir\x.txt", r"SUB\C.TXT", "sub", r"a.txt\x"] {
        o.push(s.create(p, r, OPEN_EXISTING, 0, b""));
    }
    o.push(s.create("sub", r, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, b""));
    for p in [r"sub\*", r"sub\*.txt", r"sub\*.dat", r"sub\nomatch*", r"missing\*", r"a.txt\*", r"sub\deep\*", r"empty\*", r"sub\", r"sub\c.txt", r"sub\?.txt", r"sub\*.", r"SUB\*.TXT", r"sub\deep"] {
        o.push(s.find(p, 0, 0));
    }
    o.push(s.find(r"sub\*", 1, 2));
    o.push(s.find_a(r"sub\*"));
    for p in ["a.txt", r"sub\..\a.txt", r"sub\.\deep\", r"sub\deep\..\..\x.txt", r"sub\x. ", r"sub//deep"] {
        o.push(s.full(p, true));
    }
    for p in ["", "sub", "missing", "a.txt"] {
        o.push(s.disk(p));
    }
    // CreateFile dispositions and last errors
    o.push(s.create("new1.txt", rw, CREATE_NEW, 0, b"abc"));
    o.push(s.create("a.txt", rw, CREATE_NEW, 0, b""));
    o.push(s.create("a.txt", rw, CREATE_ALWAYS, 0, b"xyz"));
    o.push(s.create("new2.txt", rw, OPEN_ALWAYS, 0, b""));
    o.push(s.create("b.bin", rw, OPEN_ALWAYS, 0, b""));
    o.push(s.create(r"sub\c.txt", rw, TRUNCATE_EXISTING, 0, b""));
    o.push(s.create("missing.txt", rw, TRUNCATE_EXISTING, 0, b""));
    o.push(s.create(r"sub\d.dat", r, TRUNCATE_EXISTING, 0, b""));
    o.push(s.create("a.txt", r, 0, 0, b""));
    o.push(s.create("a.txt", r, 6, 0, b""));
    o.push(s.create(r"nodir\x.txt", rw, CREATE_NEW, 0, b""));
    o.push(s.create("hidden2.txt", rw, CREATE_NEW, 0x2, b""));
    o.push(s.create("sub", rw, CREATE_ALWAYS, 0, b""));
    o.push(s.create("sub", rw, CREATE_NEW, 0, b""));
    o.push(s.create("hidden.txt", r, OPEN_ALWAYS, 0, b""));
    o.push(s.create("fresh.txt", r, CREATE_ALWAYS, 0, b""));
    for p in ["a.txt", r"sub\c.txt", "new1.txt", "hidden2.txt", "b.bin"] {
        o.push(s.create(p, r, OPEN_EXISTING, 0, b""));
        o.push(s.attrs_ex(p, 0));
    }
    o.push(s.create2("a.txt", r, OPEN_EXISTING));
    o.push(s.create2("new3.txt", rw, CREATE_NEW));
    o.push(s.create2("new3.txt", rw, OPEN_ALWAYS));
    o.push(s.create2("new3.txt", rw, CREATE_NEW));
    o.push(s.create_a("a.txt", r, OPEN_EXISTING));
    o.push(s.create_a("ansi.txt", rw, CREATE_NEW));
    // delete
    for p in ["new2.txt", "new2.txt", "sub", r"nodir\x.txt", "b.bin", "b.bin"] {
        o.push(s.delete(p));
    }
    o.push(s.delete_a("new3.txt"));
    o.push(s.attrs("b.bin"));
    o.push(s.set_attrs("new1.txt", FILE_ATTRIBUTE_READONLY));
    o.push(s.set_attrs(r"sub\d.dat", FILE_ATTRIBUTE_READONLY));
    o.push(s.delete("new1.txt"));
    o.push(s.delete(r"sub\d.dat"));
    o.push(s.create(r"sub\d.dat", rw, OPEN_EXISTING, 0, b""));
    o.push(s.set_attrs("new1.txt", 0x80));
    o.push(s.set_attrs(r"sub\d.dat", 0x80));
    o.push(s.set_attrs("missing.txt", 0x80));
    o.push(s.delete("new1.txt"));
    // directories
    for p in ["newdir", "newdir", r"nodir\x", "sub", r"sub\deep\deeper"] {
        o.push(s.mkdir(p));
    }
    o.push(s.mkdir_a("newdirA"));
    for p in ["empty", "empty", "sub", "a.txt", "newdir", r"sub\deep\deeper", "missing"] {
        o.push(s.rmdir(p));
    }
    // move
    o.push(s.mv("a.txt", Some("moved.txt"), 0));
    o.push(s.mv(r"sub\d.dat", Some("moved.txt"), 0));
    o.push(s.mv(r"sub\d.dat", Some("moved.txt"), MOVEFILE_REPLACE_EXISTING));
    o.push(s.mv("missing.txt", Some("x.txt"), 0));
    o.push(s.mv("moved.txt", Some(r"nodir\y.txt"), 0));
    o.push(s.mv(r"sub\deep", Some("deep2"), 0));
    o.push(s.mkdir("d3"));
    o.push(s.mv("deep2", Some("d3"), 0));
    o.push(s.mv("Mixed Case.TXT", Some("mixed case.txt"), 0));
    o.push(s.mv("moved.txt", Some("newdirA"), MOVEFILE_REPLACE_EXISTING));
    o.push(s.mv("moved.txt", Some("ansi.txt"), MOVEFILE_REPLACE_EXISTING));
    o.push(s.mv("ansi.txt", Some(r"newdirA\in.txt"), MOVEFILE_COPY_ALLOWED));
    for p in [r"sub\*", r"deep2\*", r"newdirA\*", r"d3\*"] {
        o.push(s.find(p, 0, 0));
    }
    for p in ["moved.txt", "ansi.txt", r"newdirA\in.txt", r"deep2\e.txt", "mixed case.txt"] {
        o.push(s.create(p, r, OPEN_EXISTING, 0, b""));
    }
    o.push(s.find("mixed*", 0, 0));
    // current directory and relative paths
    o.push(s.cd(&s.p("sub")));
    o.push(s.full("c.txt", false));
    o.push(s.full(r"..\hidden.txt", false));
    o.push(s.create("c.txt", r, OPEN_EXISTING, 0, b"").replace("c.txt", "rel"));
    o.push(s.cd(&s.p(r"newdirA\in.txt")));
    o.push(s.cd(&s.p("missing")));
    o.push(s.cd(&s.p(r"nodir\deeper")));
    o.push(s.cd(".."));
    o
}

fn make_tree(root: &Path) {
    put(root, "a.txt", b"hello");
    put(root, "b.bin", &(0..3000u32).map(|i| i as u8).collect::<Vec<_>>());
    put(root, "Mixed Case.TXT", b"mc");
    put(root, r"sub\c.txt", b"c");
    put(root, r"sub\d.dat", b"dddd");
    put(root, r"sub\long name file.text", b"long");
    put(root, r"sub\deep\e.txt", b"e");
    put(root, "hidden.txt", b"h");
    fs::create_dir_all(root.join("empty")).unwrap();
    unsafe { host::SetFileAttributesW(w(&root.join("hidden.txt").to_string_lossy()).as_ptr(), 0x2) };
}

#[test]
fn f2_side_by_side_with_the_host() {
    let _g = lock();
    let s = Scratch::new("f2");
    let host_root = s.0.join("host");
    let base = s.0.join("base");
    make_tree(&host_root);
    make_tree(&base);
    let before = hash_tree(&base);
    let vfs = VfsBuilder::new(s.0.join("overlay")).package('G', &base, &ModSet::default()).build().unwrap();
    install(Files::new(vfs, r"G:\"));
    let host_cwd = std::env::current_dir().unwrap();

    let ours = unsafe { script(&Side { ours: true, root: "G:".into() }) };
    let theirs = unsafe { script(&Side { ours: false, root: host_root.to_string_lossy().into_owned() }) };
    std::env::set_current_dir(&host_cwd).unwrap();

    let diffs: Vec<String> =
        ours.iter().zip(&theirs).filter(|(a, b)| a != b).map(|(a, b)| format!("  ours: {a}\n  host: {b}")).collect();
    assert_eq!(ours.len(), theirs.len());
    assert!(diffs.is_empty(), "{} of {} outcomes differ:\n{}", diffs.len(), ours.len(), diffs.join("\n"));
    println!("f2: {} outcomes compared with the host, 0 differ", ours.len());
    assert_eq!(hash_tree(&base), before, "the base folder is unchanged after the whole script");
}

mod round2;
