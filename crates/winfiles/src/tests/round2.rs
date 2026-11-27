//! Round 2: F4 (the CRT) and F5 (copies, links, handle changes, directory attributes), side by side.

use super::*;
use crate::crt::{self, ucrt, Stat64};

// ---- F4: the CRT side by side with the host CRT --------------------------------------------------------

struct CrtSide {
    ours: bool,
    root: String,
}

unsafe fn errno() -> i32 {
    *(ucrt()._errno)()
}

unsafe fn clear_errno() {
    *(ucrt()._errno)() = 0;
}

impl CrtSide {
    fn p(&self, rel: &str) -> String {
        format!("{}\\{rel}", self.root)
    }
    fn strip(&self, s: &str) -> String {
        let s = s.strip_prefix(&self.root).map(|r| format!("<root>{r}")).unwrap_or_else(|| s.to_owned());
        s.trim_end_matches('\\').to_owned()
    }

    unsafe fn fopen(&self, rel: &str, mode: &str, write: &[u8], wide: bool, sh: Option<i32>) -> String {
        let u = ucrt();
        clear_errno();
        let (pa, ma, pw, mw) = (a(&self.p(rel)), a(mode), w(&self.p(rel)), w(mode));
        let f = match (self.ours, wide, sh) {
            (true, false, None) => crt::fopen(pa.as_ptr(), ma.as_ptr()),
            (true, true, None) => crt::_wfopen(pw.as_ptr(), mw.as_ptr()),
            (true, false, Some(s)) => crt::_fsopen(pa.as_ptr(), ma.as_ptr(), s),
            (true, true, Some(s)) => crt::_wfsopen(pw.as_ptr(), mw.as_ptr(), s),
            (false, false, None) => (u.fopen)(pa.as_ptr(), ma.as_ptr()),
            (false, true, None) => (u._wfopen)(pw.as_ptr(), mw.as_ptr()),
            (false, false, Some(s)) => (u._fsopen)(pa.as_ptr(), ma.as_ptr(), s),
            (false, true, Some(s)) => (u._wfsopen)(pw.as_ptr(), mw.as_ptr(), s),
        };
        let tag = format!("fopen{} {rel} {mode:?} {sh:?}", if wide { "W" } else { "" });
        if f.is_null() {
            return format!("{tag} -> NULL errno {}", errno());
        }
        if !write.is_empty() {
            (u.fwrite)(write.as_ptr() as *const c_void, 1, write.len(), f);
        }
        let tell = (u.ftell)(f);
        let mut buf = vec![0u8; 8192];
        let n = (u.fread)(buf.as_mut_ptr() as *mut c_void, 1, buf.len(), f);
        (u.fclose)(f);
        format!("{tag} -> ok tell {tell} read {:?}", String::from_utf8_lossy(&buf[..n]))
    }

    unsafe fn hold(&self, rel: &str, mode: &str, sh: i32) -> *mut c_void {
        let (pa, ma) = (a(&self.p(rel)), a(mode));
        if self.ours {
            crt::_fsopen(pa.as_ptr(), ma.as_ptr(), sh)
        } else {
            (ucrt()._fsopen)(pa.as_ptr(), ma.as_ptr(), sh)
        }
    }

    unsafe fn open(&self, rel: &str, oflag: i32, pmode: i32, write: &[u8], wide: bool) -> String {
        let u = ucrt();
        clear_errno();
        let (pa, pw) = (a(&self.p(rel)), w(&self.p(rel)));
        let fd = match (self.ours, wide) {
            (true, false) => crt::_open(pa.as_ptr(), oflag, pmode),
            (true, true) => crt::_wopen(pw.as_ptr(), oflag, pmode),
            (false, false) => (u._open)(pa.as_ptr(), oflag, pmode),
            (false, true) => (u._wopen)(pw.as_ptr(), oflag, pmode),
        };
        let tag = format!("_open{} {rel} {oflag:#x} {pmode:#o}", if wide { "W" } else { "" });
        if fd < 0 {
            return format!("{tag} -> -1 errno {}", errno());
        }
        let wrote = if write.is_empty() { 0 } else { (u._write)(fd, write.as_ptr() as *const c_void, write.len() as u32) };
        let mut buf = vec![0u8; 8192];
        let n = (u._read)(fd, buf.as_mut_ptr() as *mut c_void, buf.len() as u32).max(0) as usize;
        (u._close)(fd);
        format!("{tag} -> fd ok, wrote {wrote}, read {:?}", String::from_utf8_lossy(&buf[..n]))
    }

    unsafe fn stat(&self, rel: &str) -> String {
        clear_errno();
        let p = w(&self.p(rel));
        let mut st = Stat64::default();
        let rc = if self.ours { crt::_wstat64(p.as_ptr(), &mut st) } else { (ucrt()._wstat64)(p.as_ptr(), &mut st) };
        if rc != 0 {
            return format!("_wstat64 {rel} -> {rc} errno {}", errno());
        }
        // the drive number and the times differ between the two trees by construction
        format!(
            "_wstat64 {rel} -> 0 mode {:#o} nlink {} size {} ino {} uid {} gid {}",
            st.st_mode, st.st_nlink, st.st_size, st.st_ino, st.st_uid, st.st_gid
        )
    }

    unsafe fn int_op(&self, what: &str, r: i32) -> String {
        format!("{what} -> {r} errno {}", if r != 0 { errno() } else { 0 })
    }

    unsafe fn remove(&self, rel: &str, wide: bool) -> String {
        clear_errno();
        let (pa, pw) = (a(&self.p(rel)), w(&self.p(rel)));
        let r = match (self.ours, wide) {
            (true, false) => crt::remove(pa.as_ptr()),
            (true, true) => crt::_wremove(pw.as_ptr()),
            (false, false) => (ucrt().remove)(pa.as_ptr()),
            (false, true) => (ucrt()._wremove)(pw.as_ptr()),
        };
        self.int_op(&format!("remove {rel} {wide}"), r)
    }

    unsafe fn rename(&self, from: &str, to: &str) -> String {
        clear_errno();
        let (f, t) = (w(&self.p(from)), w(&self.p(to)));
        let r = if self.ours { crt::_wrename(f.as_ptr(), t.as_ptr()) } else { (ucrt()._wrename)(f.as_ptr(), t.as_ptr()) };
        self.int_op(&format!("_wrename {from} {to}"), r)
    }

    unsafe fn rmdir(&self, rel: &str) -> String {
        clear_errno();
        let p = w(&self.p(rel));
        let r = if self.ours { crt::_wrmdir(p.as_ptr()) } else { (ucrt()._wrmdir)(p.as_ptr()) };
        self.int_op(&format!("_wrmdir {rel}"), r)
    }

    /// `to` as given: absolute in this side's namespace, or relative.
    unsafe fn chdir(&self, to: &str) -> String {
        clear_errno();
        let p = w(to);
        let r = if self.ours { crt::_wchdir(p.as_ptr()) } else { (ucrt()._wchdir)(p.as_ptr()) };
        format!("{} now {}", self.int_op(&format!("_wchdir {}", self.strip(to)), r), self.getcwd())
    }

    unsafe fn getcwd(&self) -> String {
        let u = ucrt();
        let call = |buf: *mut u16, n: i32| if self.ours { crt::_wgetcwd(buf, n) } else { (u._wgetcwd)(buf, n) };
        let mut buf = [0u16; 400];
        let p = call(buf.as_mut_ptr(), 400);
        let s = host::from_wide(p).unwrap_or_default();
        let alloc = call(std::ptr::null_mut(), 0);
        let s2 = host::from_wide(alloc).unwrap_or_default();
        (u.free)(alloc as *mut c_void);
        clear_errno();
        let mut small = [0u16; 2];
        let tiny = call(small.as_mut_ptr(), 2);
        format!("{} alloc-equal {} small-null {} errno {}", self.strip(&s), s == s2, tiny.is_null(), errno())
    }

    unsafe fn fopen_rel(&self, rel: &str, mode: &str) -> String {
        let (pa, ma) = (a(rel), a(mode));
        let f = if self.ours { crt::fopen(pa.as_ptr(), ma.as_ptr()) } else { (ucrt().fopen)(pa.as_ptr(), ma.as_ptr()) };
        if f.is_null() {
            return format!("fopen relative {rel} -> NULL errno {}", errno());
        }
        let mut buf = [0u8; 64];
        let n = (ucrt().fread)(buf.as_mut_ptr() as *mut c_void, 1, 64, f);
        (ucrt().fclose)(f);
        format!("fopen relative {rel} -> {:?}", String::from_utf8_lossy(&buf[..n]))
    }

    unsafe fn stat_rel(&self, rel: &str) -> String {
        let p = w(rel);
        let mut st = Stat64::default();
        let rc = if self.ours { crt::_wstat64(p.as_ptr(), &mut st) } else { (ucrt()._wstat64)(p.as_ptr(), &mut st) };
        format!("_wstat64 relative {rel} -> {rc} size {} mode {:#o}", st.st_size, st.st_mode)
    }
}

unsafe fn crt_script(s: &CrtSide) -> Vec<String> {
    use crt::*;
    let mut o = Vec::new();
    for p in ["a.txt", "sub", r"sub\", r"a.txt\", "missing", r"nodir\x", "a*", r"sub\deep\e.txt", "run.exe", "ro.txt", "crlf.txt"] {
        o.push(s.stat(p));
    }
    let cases: [(&str, &str, &[u8]); 26] = [
        ("a.txt", "r", b""),
        ("a.txt", "rb", b""),
        ("a.txt", "rt", b""),
        ("crlf.txt", "r", b""),
        ("crlf.txt", "rb", b""),
        ("missing", "r", b""),
        (r"nodir\x", "w", b""),
        ("sub", "r", b""),
        ("new1.txt", "w", b"hello\n"),
        ("new1.txt", "rb", b""),
        ("new1.txt", "a", b"more"),
        ("new1.txt", "rb", b""),
        ("new1.txt", "wx", b""),
        ("new2.txt", "wx", b"x"),
        ("b.bin", "r+b", b"XY"),
        ("b.bin", "rb", b""),
        ("ctrlz.txt", "r+", b""),
        ("ctrlz.txt", "rb", b""),
        ("a.txt", "w+", b"w plus"),
        ("a.txt", "a+", b"!"),
        ("a.txt", "rb", b""),
        ("ro.txt", "w", b""),
        ("ro.txt", "r", b""),
        ("a.txt", "rbS", b""),
        ("a.txt", "rN", b""),
        ("a.txt", "rbTc", b""),
    ];
    for (rel, mode, wr) in cases {
        o.push(s.fopen(rel, mode, wr, false, None));
    }
    o.push(s.fopen("a.txt", "rb", b"", true, None));
    o.push(s.fopen("wide.txt", "w", b"w", true, None));
    o.push(s.fopen("a.txt", "rb", b"", false, Some(0x10)));
    o.push(s.fopen("a.txt", "rb", b"", true, Some(0x40)));
    // sharing: a reader that denies writers
    let held = s.hold("a.txt", "r", 0x20);
    o.push(format!("hold {}", !held.is_null()));
    o.push(s.fopen("a.txt", "a", b"no", false, None));
    o.push(s.fopen("a.txt", "r", b"", false, None));
    o.push(s.fopen("a.txt", "r", b"", false, Some(0x10)));
    (ucrt().fclose)(held);

    let op = |rel: &str, oflag: i32, pmode: i32, wr: &[u8]| s.open(rel, oflag, pmode, wr, false);
    o.push(op("a.txt", O_BINARY, 0, b""));
    o.push(op("missing", 0, 0, b""));
    o.push(op("new3.bin", O_CREAT | O_EXCL | O_WRONLY | O_BINARY, 0o600, b"data"));
    o.push(op("new3.bin", O_CREAT | O_EXCL | O_WRONLY | O_BINARY, 0o600, b""));
    o.push(op("new3.bin", O_WRONLY | O_APPEND | O_BINARY, 0, b"+app"));
    o.push(op("new3.bin", O_BINARY, 0, b""));
    o.push(s.open("new3.bin", O_TRUNC | O_RDWR | O_BINARY, 0, b"t", true));
    o.push(op("new3.bin", O_BINARY, 0, b""));
    o.push(op("ro2.txt", O_CREAT | O_WRONLY, 0o400, b"x"));
    o.push(s.stat("ro2.txt"));
    o.push(op("ro2.txt", O_WRONLY, 0, b""));
    o.push(op("tmp.bin", O_CREAT | O_RDWR | O_TEMPORARY | O_BINARY, 0o600, b"gone"));
    o.push(s.stat("tmp.bin"));
    o.push(op("b.bin", O_RDWR | O_TEMPORARY | O_BINARY, 0, b""));
    o.push(s.stat("b.bin"));
    o.push(op("sub", 0, 0, b""));
    o.push(op("sub", O_OBTAIN_DIR, 0, b""));
    o.push(op("short.tmp", O_CREAT | O_WRONLY | O_SHORT_LIVED, 0o600, b"s"));
    o.push(op("crlf.txt", O_RDWR | O_TEXT, 0, b""));
    o.push(op("trunc.txt", O_CREAT | O_TRUNC | O_WRONLY | O_SEQUENTIAL, 0o600, b"tt"));
    o.push(op("trunc.txt", O_CREAT | O_TRUNC | O_RDWR | O_RANDOM | O_BINARY, 0o600, b""));

    for (rel, wide) in [("new2.txt", false), ("new2.txt", false), ("sub", false), ("ro2.txt", true), ("new1.txt", true), ("missing", true)] {
        o.push(s.remove(rel, wide));
    }
    for (from, to) in [("a.txt", "ren.txt"), ("ren.txt", "crlf.txt"), ("missing", "x"), (r"sub\deep", "deep2"), ("ren.txt", r"nodir\x")] {
        o.push(s.rename(from, to));
    }
    for rel in ["empty", "empty", "sub", "missing", "crlf.txt"] {
        o.push(s.rmdir(rel));
    }
    for p in ["ren.txt", r"deep2\e.txt", r"sub\deep", "b.bin"] {
        o.push(s.stat(p));
    }
    o.push(s.chdir(&s.p("sub")));
    o.push(s.fopen_rel("c.txt", "rb"));
    o.push(s.stat_rel("c.txt"));
    o.push(s.chdir(&s.p("missing")));
    o.push(s.chdir(&s.p("ren.txt")));
    o.push(s.chdir(".."));
    o
}

fn make_crt_tree(root: &Path) {
    make_tree(root);
    put(root, "crlf.txt", b"one\r\ntwo\r\n");
    put(root, "ctrlz.txt", b"abc\x1a");
    put(root, "run.exe", b"MZ");
    put(root, "ro.txt", b"read only");
    unsafe { host::SetFileAttributesW(w(&root.join("ro.txt").to_string_lossy()).as_ptr(), FILE_ATTRIBUTE_READONLY) };
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            out.push(p.clone());
            if p.is_dir() {
                out.extend(walk_files(&p));
            }
        }
    }
    out
}

/// So the scratch folder can be removed.
fn clear_readonly(root: &Path) {
    for p in walk_files(root) {
        let a = unsafe { host::GetFileAttributesW(w(&p.to_string_lossy()).as_ptr()) };
        if a != u32::MAX && a & 1 != 0 {
            unsafe { host::SetFileAttributesW(w(&p.to_string_lossy()).as_ptr(), a & !1) };
        }
    }
}

fn compare(what: &str, ours: &[String], theirs: &[String]) {
    assert_eq!(ours.len(), theirs.len());
    let diffs: Vec<String> =
        ours.iter().zip(theirs).filter(|(a, b)| a != b).map(|(a, b)| format!("  ours: {a}\n  host: {b}")).collect();
    assert!(diffs.is_empty(), "{what}: {} of {} outcomes differ:\n{}", diffs.len(), ours.len(), diffs.join("\n"));
}

#[test]
fn f4_crt_side_by_side_with_the_host_crt() {
    let _g = lock();
    let s = Scratch::new("f4");
    let host_root = s.0.join("host");
    let base = s.0.join("base");
    make_crt_tree(&host_root);
    make_crt_tree(&base);
    let before = hash_tree(&base);
    let vfs = VfsBuilder::new(s.0.join("overlay")).package('G', &base, &ModSet::default()).build().unwrap();
    install(Files::new(vfs, r"G:\"));
    let host_cwd = std::env::current_dir().unwrap();

    let ours = unsafe { crt_script(&CrtSide { ours: true, root: "G:".into() }) };
    let theirs = unsafe { crt_script(&CrtSide { ours: false, root: host_root.to_string_lossy().into_owned() }) };
    std::env::set_current_dir(&host_cwd).unwrap();

    let after = hash_tree(&base);
    clear_readonly(&s.0);
    assert_eq!(after, before, "the base folder is unchanged after the whole script");
    compare("f4", &ours, &theirs);
    println!("f4: {} CRT outcomes compared with the host CRT, 0 differ", ours.len());
}

#[test]
fn f4_mode_strings_and_exports() {
    use crt::*;
    assert_eq!(parse_mode("r"), Some((0, "r".into())));
    assert_eq!(parse_mode("w+b"), Some((O_RDWR | O_CREAT | O_TRUNC | O_BINARY, "w+b".into())));
    assert_eq!(parse_mode("  a+t"), Some((O_RDWR | O_CREAT | O_APPEND | O_TEXT, "a+t".into())));
    assert_eq!(parse_mode("wxD"), Some((O_WRONLY | O_CREAT | O_TRUNC | O_EXCL | O_TEMPORARY, "w".into())));
    assert_eq!(parse_mode("r, ccs=UTF-8"), Some((O_U8TEXT, "r".into())));
    assert_eq!(parse_mode("rx"), None, "x only after w");
    assert_eq!(parse_mode("rbb"), None);
    assert_eq!(parse_mode("q"), None);
    let imported = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../agents/loader-imports.csv")).unwrap();
    for f in CRT_FUNCTIONS {
        assert!(override_export(f).is_some(), "{f}");
        assert!(imported.lines().any(|l| l.split(',').nth(1) == Some(*f)), "{f} is imported by the title");
    }
    assert_eq!(errno_of(2), ENOENT);
    assert_eq!(errno_of(183), EEXIST);
    assert_eq!(errno_of(32), EACCES, "sharing violations");
    assert_eq!(errno_of(267), EINVAL);
}

// ---- F5: copies, links, handle renames and deletes, directory attributes ------------------------------

impl Side {
    unsafe fn copy2(&self, from: &str, to: &str, fail_if_exists: bool) -> String {
        let p = host::COPYFILE2_EXTENDED_PARAMETERS {
            dwSize: std::mem::size_of::<host::COPYFILE2_EXTENDED_PARAMETERS>() as u32,
            dwCopyFlags: fail_if_exists as u32,
            pfCancel: std::ptr::null_mut(),
            pProgressRoutine: std::ptr::null_mut(),
            pvCallbackContext: std::ptr::null_mut(),
        };
        let (f, t) = (w(&self.p(from)), w(&self.p(to)));
        let hr = if self.ours { CopyFile2(f.as_ptr(), t.as_ptr(), &p) } else { host::CopyFile2(f.as_ptr(), t.as_ptr(), &p) };
        format!("CopyFile2 {from} {to} {fail_if_exists} -> {hr:#x}")
    }

    unsafe fn hardlink(&self, new: &str, existing: &str) -> String {
        let (n, e) = (w(&self.p(new)), w(&self.p(existing)));
        let r = if self.ours {
            CreateHardLinkW(n.as_ptr(), e.as_ptr(), std::ptr::null())
        } else {
            host::CreateHardLinkW(n.as_ptr(), e.as_ptr(), std::ptr::null())
        };
        format!("CreateHardLinkW {new} {existing} -> {r} error {}", if r == 0 { last() } else { 0 })
    }

    /// `None` when the host is privileged and created the link (nothing comparable).
    unsafe fn symlink(&self, link: &str, target: &str) -> Option<String> {
        let (l, t) = (w(&self.p(link)), w(&self.p(target)));
        let r = if self.ours { CreateSymbolicLinkW(l.as_ptr(), t.as_ptr(), 0) } else { host::CreateSymbolicLinkW(l.as_ptr(), t.as_ptr(), 0) };
        let e = last();
        (r == 0 && e == ERROR_PRIVILEGE_NOT_HELD).then(|| format!("CreateSymbolicLinkW {link} -> {r} error {e}"))
    }

    unsafe fn open_raw(&self, rel: &str, access: u32, disp: u32, flags: u32) -> HANDLE {
        let p = w(&self.p(rel));
        if self.ours {
            CreateFileW(p.as_ptr(), access, 7, std::ptr::null(), disp, flags, std::ptr::null_mut())
        } else {
            host::CreateFileW(p.as_ptr(), access, 7, std::ptr::null(), disp, flags, std::ptr::null_mut())
        }
    }

    unsafe fn delete_on_close(&self, rel: &str, disp: u32) -> String {
        let h = self.open_raw(rel, GENERIC_READ | GENERIC_WRITE | 0x1_0000, disp, FILE_FLAG_DELETE_ON_CLOSE);
        if h == INVALID_HANDLE_VALUE {
            return format!("delete-on-close {rel} -> error {}", last());
        }
        write_all(h, b"temp");
        let during = self.attrs(rel);
        host::CloseHandle(h);
        format!("delete-on-close {rel} -> during [{during}] after [{}]", self.attrs(rel))
    }

    unsafe fn set_info(&self, h: HANDLE, class: i32, buf: &[u8]) -> String {
        let r = if self.ours {
            SetFileInformationByHandle(h, class, buf.as_ptr() as *const c_void, buf.len() as u32)
        } else {
            host::SetFileInformationByHandle(h, class, buf.as_ptr() as *const c_void, buf.len() as u32)
        };
        format!("{r} error {}", if r == 0 { last() } else { 0 })
    }

    unsafe fn rename_by_handle(&self, rel: &str, to: &str, flags: u32, ex: bool, full: bool) -> String {
        let h = self.open_raw(rel, GENERIC_READ | 0x1_0000, OPEN_EXISTING, 0);
        if h == INVALID_HANDLE_VALUE {
            return format!("rename-by-handle {rel} -> open error {}", last());
        }
        let name = if full { self.p(to) } else { to.to_owned() };
        let r = self.set_info(h, if ex { 22 } else { 3 }, &rename_info(&name, flags));
        host::CloseHandle(h);
        let to_rel = if full { to.to_owned() } else { format!("{}{to}", rel.rfind('\\').map_or("", |i| &rel[..=i])) };
        format!("rename-by-handle {rel} {to} {flags} {ex} -> {r}; from [{}] to [{}]", self.attrs(rel), self.attrs(&to_rel))
    }

    unsafe fn delete_by_handle(&self, rel: &str, ex: bool) -> String {
        let h = self.open_raw(rel, GENERIC_READ | 0x1_0000, OPEN_EXISTING, 0);
        if h == INVALID_HANDLE_VALUE {
            return format!("delete-by-handle {rel} -> open error {}", last());
        }
        let r = if ex { self.set_info(h, 21, &3u32.to_ne_bytes()) } else { self.set_info(h, 4, &[1u8]) };
        let during = self.attrs(rel);
        host::CloseHandle(h);
        format!("delete-by-handle {rel} {ex} -> {r}; during [{during}] after [{}]", self.attrs(rel))
    }
}

/// A `FILE_RENAME_INFO` (or `_EX`) for `name`.
fn rename_info(name: &str, flags: u32) -> Vec<u8> {
    let target: Vec<u16> = name.encode_utf16().collect();
    let mut buf = vec![0u8; 20 + target.len() * 2 + 2];
    buf[..4].copy_from_slice(&flags.to_ne_bytes());
    buf[16..20].copy_from_slice(&((target.len() * 2) as u32).to_ne_bytes());
    for (i, c) in target.iter().enumerate() {
        buf[20 + i * 2..22 + i * 2].copy_from_slice(&c.to_ne_bytes());
    }
    buf
}

unsafe fn f5_script(s: &Side) -> (Vec<String>, usize) {
    let mut o = Vec::new();
    let r = GENERIC_READ;
    o.push(s.copy2("a.txt", "copy.txt", false));
    o.push(s.copy2("a.txt", "copy.txt", true));
    o.push(s.copy2("b.bin", "copy.txt", false));
    o.push(s.copy2("missing.txt", "x.txt", false));
    o.push(s.copy2("sub", "x.txt", false));
    o.push(s.copy2("a.txt", r"nodir\x.txt", false));
    o.push(s.copy2("b.bin", r"sub\b2.bin", false));
    o.push(s.copy2("a.txt", "sub", false));
    for p in ["copy.txt", r"sub\b2.bin"] {
        o.push(s.create(p, r, OPEN_EXISTING, 0, b""));
        o.push(s.attrs_ex(p, 0));
    }
    o.push(s.hardlink("hl.txt", "a.txt"));
    o.push(s.create("hl.txt", GENERIC_WRITE, OPEN_EXISTING, 0, b"LINKED"));
    o.push(s.create("a.txt", r, OPEN_EXISTING, 0, b""));
    o.push(s.hardlink("hl.txt", "a.txt"));
    o.push(s.hardlink("x.txt", "missing.txt"));
    o.push(s.hardlink("y.txt", "sub"));
    o.push(s.hardlink(r"nodir\z.txt", "a.txt"));
    let mut skipped = 0;
    match s.symlink("sl.txt", "a.txt") {
        Some(line) => o.push(line),
        None => skipped += 1,
    }
    o.push(s.delete_on_close("b.bin", OPEN_EXISTING));
    o.push(s.delete_on_close("doc-new.txt", CREATE_NEW));
    o.push(s.delete_on_close(r"sub\c.txt", OPEN_ALWAYS));
    o.push(s.find(r"sub\*", 0, 0));
    o.push(s.rename_by_handle(r"sub\d.dat", r"sub\d2.dat", 0, false, true));
    o.push(s.rename_by_handle("Mixed Case.TXT", r"sub\mc.txt", 0, true, true));
    o.push(s.rename_by_handle("copy.txt", "a.txt", 0, false, true));
    o.push(s.rename_by_handle("copy.txt", "a.txt", 1, false, true));
    o.push(s.rename_by_handle(r"sub\d2.dat", r"sub\plain.dat", 0, false, true));
    o.push(s.rename_by_handle("hl.txt", r"nodir\q.txt", 0, false, true));
    o.push(s.create("a.txt", r, OPEN_EXISTING, 0, b""));
    o.push(s.create(r"sub\plain.dat", r, OPEN_EXISTING, 0, b""));
    o.push(s.delete_by_handle("hidden.txt", false));
    o.push(s.delete_by_handle(r"sub\b2.bin", true));
    o.push(s.delete_by_handle(r"sub\long name file.text", false));
    o.push(s.find(r"sub\*", 0, 0));
    o.push(s.set_attrs("sub", 0x2));
    o.push(s.attrs("sub"));
    o.push(s.set_attrs(r"sub\deep", 0x1));
    o.push(s.attrs(r"sub\deep"));
    o.push(s.find(r"sub\deep\*", 0, 0));
    o.push(s.create(r"sub\deep\new.txt", GENERIC_WRITE, CREATE_NEW, 0, b"in a read-only dir"));
    o.push(s.set_attrs(r"sub\deep", 0x80));
    o.push(s.set_attrs("empty", 0x4));
    o.push(s.rmdir("empty"));
    o.push(s.find(r"sub\*", 0, 0));
    (o, skipped)
}

#[test]
fn f5_copies_links_handle_changes_and_directory_attributes_match_the_host() {
    let _g = lock();
    let s = Scratch::new("f5");
    let host_root = s.0.join("host");
    let base = s.0.join("base");
    make_tree(&host_root);
    make_tree(&base);
    let before = hash_tree(&base);
    let vfs = VfsBuilder::new(s.0.join("overlay")).package('G', &base, &ModSet::default()).build().unwrap();
    install(Files::new(vfs, r"G:\"));

    let (ours, skipped) = unsafe { f5_script(&Side { ours: true, root: "G:".into() }) };
    let (theirs, _) = unsafe { f5_script(&Side { ours: false, root: host_root.to_string_lossy().into_owned() }) };
    let after = hash_tree(&base);
    clear_readonly(&s.0);
    assert_eq!(after, before, "the base folder is unchanged");
    compare("f5", &ours, &theirs);
    println!("f5: {} outcomes compared with the host, 0 differ ({skipped} symlink line skipped: the host is privileged)", ours.len());

    unsafe {
        assert_eq!(WerRegisterFile(w(r"G:\crash.dmp").as_ptr(), 1, 0), 0);
        assert_eq!(CreateSymbolicLinkW(w(r"G:\l").as_ptr(), w(r"G:\a.txt").as_ptr(), 2), 0);
        assert_eq!(last(), ERROR_PRIVILEGE_NOT_HELD);
    }
    let d = discoveries();
    assert!(d.iter().any(|x| x.function == "WerRegisterFile" && x.path == r"G:\crash.dmp"));
    assert!(d.iter().any(|x| x.function == "CreateSymbolicLinkW"));
}

#[test]
fn f5_delete_pending_settles_and_hides_the_base_file() {
    let _g = lock();
    let s = Scratch::new("pending");
    let (base, _) = setup(&s);
    let hb = hash_tree(&base);
    unsafe {
        // a base file opened delete-on-close: copied up, gone at close, and the base copy stays hidden
        let h = CreateFileW(w(r"G:\dir\x.txt").as_ptr(), GENERIC_READ | 0x1_0000, 7, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_DELETE_ON_CLOSE, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(names_w(r"G:\dir\x.txt").unwrap(), ["x.txt"], "still there while open");
        host::CloseHandle(h);
        assert_eq!(names_w(r"G:\dir\x.txt"), Err(ERROR_FILE_NOT_FOUND));
        assert_eq!(GetFileAttributesW(w(r"G:\dir\x.txt").as_ptr()), u32::MAX);
        let h = CreateFileW(w(r"G:\dir\x.txt").as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_NEW, 0, std::ptr::null_mut());
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(last(), 0, "created anew");
        host::CloseHandle(h);
        // rename through a handle across directories; the old directory is then empty in the index too
        assert_eq!(CreateDirectoryW(w(r"S:\a").as_ptr(), std::ptr::null()), 1);
        assert_eq!(CreateDirectoryW(w(r"S:\b").as_ptr(), std::ptr::null()), 1);
        let h = CreateFileW(w(r"S:\a\f.txt").as_ptr(), GENERIC_WRITE | 0x1_0000, 0, std::ptr::null(), CREATE_NEW, 0, std::ptr::null_mut());
        write_all(h, b"moved by handle");
        let info = rename_info(r"S:\b\g.txt", 0);
        assert_eq!(SetFileInformationByHandle(h, 3, info.as_ptr() as *const c_void, info.len() as u32), 1, "error {}", last());
        host::CloseHandle(h);
        assert_eq!(names_w(r"S:\b\*").unwrap(), [".", "..", "g.txt"]);
        assert_eq!(RemoveDirectoryW(w(r"S:\a").as_ptr()), 1);
        let h = CreateFileW(w(r"S:\b\g.txt").as_ptr(), GENERIC_READ, 1, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
        assert_eq!(read_all(h), b"moved by handle");
        host::CloseHandle(h);
        // a base directory's attributes land on its overlay copy
        assert_eq!(SetFileAttributesW(w(r"G:\Data").as_ptr(), 0x2), 1);
        assert_eq!(GetFileAttributesW(w(r"G:\Data").as_ptr()) & 0x12, 0x12);
        assert_eq!(names_w(r"G:\Data\*").unwrap(), [".", "..", "Big.RPF"]);
    }
    assert_eq!(hash_tree(&base), hb, "the base folder is unchanged");
}
