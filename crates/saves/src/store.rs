//! Save containers on disk: where they are, listing them, and every way the editor changes one. All of it
//! goes through [`write_container`], which refuses while the title runs, keeps a timestamped backup of
//! what it replaces, writes through the runtime's own all-or-nothing path
//! (`rebuild_gdk::areas::xgamesave::file::write`) and reads the result back.
//!
//! ```text
//! <overlay>/GameSave/<xuid>/<scid>/<container>.gsc            the runtime's containers
//! <overlay>/GameSave.backups/<xuid>/<scid>/<file>.<stamp>.gsc  the editor's backups (never read by the title)
//! ```

use crate::format::{Save, DATA_BLOB, HEADER_BLOB};
use crate::json::{Fields, Json};
use rebuild_gdk::areas::xgamesave::{file, valid_container_name, Stored};
use std::path::{Path, PathBuf};

/// The folder holding every user's containers for `game`. Today the runtime keeps one overlay for all
/// games (`rebuild_vfs::default_overlay_root`), so `game` is unused; the storage lane's
/// `storage::saves_dir(game, user)` replaces this one function when it lands.
pub fn saves_root(_game: &str) -> PathBuf {
    rebuild_vfs::default_overlay_root().join("GameSave")
}

/// Where the editor keeps backups for containers under `root`: beside it, never inside it.
pub fn backups_root(root: &Path) -> PathBuf {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "GameSave".into());
    root.with_file_name(format!("{name}.backups"))
}

/// One container file, as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub user: String,
    pub scid: String,
    /// The container's own name and display name; `None` if the file is not a container we can read.
    pub container: Option<(String, String)>,
    /// Seconds since 1970 (the container's own time), and the file's size in bytes.
    pub modified: i64,
    pub bytes: u64,
    pub blobs: usize,
}

/// Every container under `root`, by user, then service configuration, then name.
pub fn list(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    for user in dirs(root) {
        for scid in dirs(&user) {
            let Ok(files) = std::fs::read_dir(&scid) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_none_or(|x| x != file::EXTENSION) {
                    continue;
                }
                let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                let c = std::fs::read(&p).ok().and_then(|b| file::decode(&b).ok());
                out.push(Entry {
                    user: name_of(&user),
                    scid: name_of(&scid),
                    container: c.as_ref().map(|c| (c.name.clone(), c.display.clone())),
                    modified: c.as_ref().map_or(0, |c| c.modified),
                    blobs: c.as_ref().map_or(0, |c| c.blobs.len()),
                    bytes,
                    path: p,
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.user, &a.scid, a.container.as_ref().map(|c| &c.0)).cmp(&(&b.user, &b.scid, b.container.as_ref().map(|c| &c.0))));
    out
}

fn dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Reads one container file, strictly.
pub fn read(path: &Path) -> Result<Stored, String> {
    let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    file::decode(&b).map_err(|e| format!("{}: not a container: {e}", path.display()))
}

/// The decoded save in `c`, if it holds the title's two save blobs: `None` if it does not (another kind
/// of container), `Some(Err)` if it does but they are not ones this parser understands.
pub fn save_of(c: &Stored) -> Option<Result<Save, String>> {
    let h = c.blobs.iter().find(|b| b.0 == HEADER_BLOB)?;
    let d = c.blobs.iter().find(|b| b.0 == DATA_BLOB)?;
    Some(Save::parse(&h.1, &d.1).map_err(|e| e.0))
}

/// `c` with its save blobs replaced by `s` (in place, the blob order kept).
pub fn with_save(c: &Stored, s: &Save) -> Stored {
    let mut c = c.clone();
    for b in &mut c.blobs {
        if b.0 == HEADER_BLOB {
            b.1 = s.header.to_bytes();
        } else if b.0 == DATA_BLOB {
            b.1 = s.data();
        }
    }
    c
}

/// Names that look tied to online or multiplayer state. None of the title's containers is known to be,
/// but anything named like one is never written by the editor.
pub fn looks_online(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["online", "multiplayer", "mp_", "mp/", "network", "social", "cloud"].iter().any(|k| n.contains(k)) || n.starts_with("mp")
}

/// Why `c` may not be written, if it may not.
pub fn refuse(c: &Stored) -> Option<String> {
    if !valid_container_name(&c.name) {
        return Some(format!("{:?} is not a valid container name", c.name));
    }
    if looks_online(&c.name) {
        return Some(format!("{:?} looks online-related: the editor never writes it", c.name));
    }
    let mut names: Vec<&str> = c.blobs.iter().map(|b| b.0.as_str()).collect();
    names.sort();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Some("a blob name appears twice".into());
    }
    match save_of(c) {
        Some(Err(e)) => Some(format!("its save blobs are not ones this editor understands: {e}")),
        Some(Ok(s)) if !s.problems().is_empty() => Some(format!("the title would not load it: {}", s.problems().join("; "))),
        _ => None,
    }
}

/// UTC `yyyymmdd-hhmmss` for seconds since 1970 (no time crate: days to civil date, Howard Hinnant's method).
pub fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let s = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", s / 3600, s % 3600 / 60, s % 60)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The backup folder for the container file `path` (under `root`).
pub fn backup_dir(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let rel = path.parent().and_then(|p| p.strip_prefix(root).ok()).ok_or_else(|| format!("{} is not under {}", path.display(), root.display()))?;
    Ok(backups_root(root).join(rel))
}

/// Copies the container file `path` into its backup folder as `<file>.<stamp>.gsc`, flushed; never
/// overwrites an earlier backup.
pub fn backup(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let dir = backup_dir(root, path)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = stamp(now());
    for n in 0..1000 {
        let name = if n == 0 { format!("{stem}.{base}.{}", file::EXTENSION) } else { format!("{stem}.{base}-{n}.{}", file::EXTENSION) };
        let p = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(&bytes).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", p.display()))?;
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    }
    Err("too many backups this second".into())
}

/// The backups of the container file `path`, newest first.
pub fn backups(root: &Path, path: &Path) -> Vec<PathBuf> {
    let (Ok(dir), Some(stem)) = (backup_dir(root, path), path.file_stem().map(|s| s.to_string_lossy().into_owned())) else { return Vec::new() };
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&format!("{stem}.")))).collect())
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

/// What happened in a write.
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    pub path: PathBuf,
    /// The backup of what was there; `None` if nothing was.
    pub backup: Option<PathBuf>,
}

/// The one way the editor changes a container: `c` into folder `dir` (under `root`), in the file the
/// runtime would use for its name. Refused while `running()` says the title runs, while an update of the
/// runtime is in flight in that folder, or if [`refuse`] objects. What it replaces is backed up first.
pub fn write_container(root: &Path, dir: &Path, c: &Stored, running: &dyn Fn() -> bool) -> Result<Written, String> {
    if running() {
        return Err("the title is running: saves are only written while it is closed".into());
    }
    if let Some(why) = refuse(c) {
        return Err(why);
    }
    if !dir.starts_with(root) || dir == root {
        return Err(format!("{} is not a provider folder under {}", dir.display(), root.display()));
    }
    if let Ok(r) = std::fs::read_dir(dir) {
        if r.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".tmp")) {
            return Err("an update is half written in this folder (a .tmp file): start and close the title once, then retry".into());
        }
    }
    let bytes = file::encode(c);
    if file::decode(&bytes).ok().as_ref() != Some(c) {
        return Err("the container does not encode back to itself".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = file::path(dir, &c.name);
    let backup = if path.exists() { Some(backup(root, &path)?) } else { None };
    // the last moment to notice a start
    if running() {
        return Err("the title started: nothing written".into());
    }
    file::write(&path, c).map_err(|e| format!("{}: {e}", path.display()))?;
    let back = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if back != bytes {
        return Err(format!("{} reads back different bytes; the backup is {:?}", path.display(), backup));
    }
    Ok(Written { path, backup })
}

/// Undo: the backup `b` written back over the container it came from (itself backed up first).
pub fn restore(root: &Path, path: &Path, b: &Path, running: &dyn Fn() -> bool) -> Result<Written, String> {
    let c = read(b)?;
    let dir = path.parent().ok_or("no folder")?;
    if file::path(dir, &c.name) != path {
        return Err(format!("{} is a backup of container {:?}, not of {}", b.display(), c.name, path.display()));
    }
    write_container(root, dir, &c, running)
}

// Import and export of one container: the file and a manifest beside it.

pub const MANIFEST_FORMAT: &str = "rebuild-save-container";

/// FNV-1a 64: catches a file changed or cut since export (not a defence against a deliberate forger:
/// the parser's own checks are that).
pub fn fnv64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3))
}

fn manifest(c: &Stored, bytes: &[u8], user: &str, scid: &str) -> String {
    Json::obj(vec![
        ("format", Json::str(MANIFEST_FORMAT)),
        ("version", Json::num(1)),
        ("container", Json::str(&c.name)),
        ("display", Json::str(&c.display)),
        ("modified", Json::num(c.modified)),
        ("user", Json::str(user)),
        ("scid", Json::str(scid)),
        ("file_bytes", Json::num(bytes.len())),
        ("file_fnv64", crate::json::hex64(fnv64(bytes))),
        (
            "blobs",
            Json::Arr(
                c.blobs
                    .iter()
                    .map(|(n, d)| Json::obj(vec![("name", Json::str(n)), ("bytes", Json::num(d.len())), ("fnv64", crate::json::hex64(fnv64(d)))]))
                    .collect(),
            ),
        ),
    ])
    .pretty()
}

/// Exports the container file `path` into `dest` as `<file>.gsc` and `<file>.gsc.json`; returns both.
pub fn export_container(path: &Path, dest: &Path) -> Result<(PathBuf, PathBuf), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let c = file::decode(&bytes).map_err(|e| format!("{}: not a container: {e}", path.display()))?;
    let scid_dir = path.parent().ok_or("no folder")?;
    let user = scid_dir.parent().map(name_of).unwrap_or_default();
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let name = path.file_name().ok_or("no file name")?;
    let out = dest.join(name);
    let man = PathBuf::from(format!("{}.json", out.display()));
    crate::write_new(&out, &bytes)?;
    crate::write_new(&man, manifest(&c, &bytes, &user, &name_of(scid_dir)).as_bytes())?;
    Ok((out, man))
}

/// Reads an exported container and its manifest, refusing anything that does not match: the file's
/// size and hash, every blob's, the names. Returns the container.
pub fn check_export(gsc: &Path) -> Result<Stored, String> {
    let man_path = PathBuf::from(format!("{}.json", gsc.display()));
    let text = std::fs::read_to_string(&man_path).map_err(|e| format!("{}: {e} (the manifest is required)", man_path.display()))?;
    let bytes = std::fs::read(gsc).map_err(|e| format!("{}: {e}", gsc.display()))?;
    let c = file::decode(&bytes).map_err(|e| format!("{}: not a container: {e}", gsc.display()))?;
    let mut f = Fields::of(Json::parse(&text)?, "manifest")?;
    if f.string("format")? != MANIFEST_FORMAT || f.u32("version")? != 1 {
        return Err("not a container manifest this editor reads".into());
    }
    let ok = |b: bool, what: &str| if b { Ok(()) } else { Err(format!("the manifest's {what} does not match the file: refused")) };
    ok(f.string("container")? == c.name, "container")?;
    ok(f.string("display")? == c.display, "display")?;
    ok(f.i64("modified")? == c.modified, "modified")?;
    f.string("user")?;
    f.string("scid")?;
    ok(f.i64("file_bytes")? == bytes.len() as i64, "file size")?;
    ok(f.hex64("file_fnv64")? == fnv64(&bytes), "file hash")?;
    let blobs = f.array("blobs")?;
    ok(blobs.len() == c.blobs.len(), "blob count")?;
    for (j, (n, d)) in blobs.into_iter().zip(&c.blobs) {
        let mut b = Fields::of(j, "manifest.blobs[]")?;
        ok(b.string("name")? == *n, "blob name")?;
        ok(b.i64("bytes")? == d.len() as i64, "blob size")?;
        ok(b.hex64("fnv64")? == fnv64(d), "blob hash")?;
        b.finish()?;
    }
    f.finish()?;
    if let Some(why) = refuse(&c) {
        return Err(why);
    }
    Ok(c)
}

/// Imports an exported container into provider folder `dir`, replacing (and backing up) the container
/// of the same name.
pub fn import_container(root: &Path, dir: &Path, gsc: &Path, running: &dyn Fn() -> bool) -> Result<Written, String> {
    let c = check_export(gsc)?;
    write_container(root, dir, &c, running)
}

// Every save of a game at once. The storage lane's `storage::export` / `storage::import` (one archive
// of all saves and settings) replace these two when they land; until then a folder of per-container
// exports, each importable on its own.

/// Exports every readable container under `root` into `dest/<user>/<scid>/`; returns how many.
pub fn export_all(root: &Path, dest: &Path) -> Result<usize, String> {
    let mut n = 0;
    for e in list(root) {
        if e.container.is_none() {
            continue;
        }
        export_container(&e.path, &dest.join(&e.user).join(&e.scid))?;
        n += 1;
    }
    Ok(n)
}

/// Imports a folder made by [`export_all`]: every container is checked first, and nothing is written
/// unless all pass. Returns the writes.
pub fn import_all(root: &Path, src: &Path, running: &dyn Fn() -> bool) -> Result<Vec<Written>, String> {
    let mut plan = Vec::new();
    for user in dirs(src) {
        for scid in dirs(&user) {
            let Ok(files) = std::fs::read_dir(&scid) else { continue };
            let mut files: Vec<PathBuf> = files.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == file::EXTENSION)).collect();
            files.sort();
            for f in files {
                let c = check_export(&f).map_err(|e| format!("{}: {e}", f.display()))?;
                plan.push((root.join(name_of(&user)).join(name_of(&scid)), c));
            }
        }
    }
    if running() {
        return Err("the title is running: saves are only written while it is closed".into());
    }
    plan.iter().map(|(dir, c)| write_container(root, dir, c, running)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::build;

    fn temp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!("rebuild-saves-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn container(name: &str) -> Stored {
        let s = build(5, "Profile Settings", 0x99, "", &[7; 64]).unwrap();
        Stored { name: name.into(), display: name.into(), modified: 1_764_000_000, blobs: vec![(HEADER_BLOB.into(), s.header.to_bytes()), (DATA_BLOB.into(), s.data())] }
    }

    fn setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = temp(tag);
        let root = base.join("GameSave");
        let dir = root.join("0009000000000001").join("default");
        let c = container("SettingsProfileSettings");
        std::fs::create_dir_all(&dir).unwrap();
        file::write(&file::path(&dir, &c.name), &c).unwrap();
        (root, dir, base)
    }

    const IDLE: &dyn Fn() -> bool = &|| false;
    const RUNNING: &dyn Fn() -> bool = &|| true;

    #[test]
    fn stamps() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(951_782_400), "20000229-000000");
        assert_eq!(stamp(1_764_331_199), "20251128-115959");
    }

    #[test]
    fn listing() {
        let (root, dir, base) = setup("list");
        std::fs::write(dir.join("junk.gsc"), b"nope").unwrap();
        let l = list(&root);
        assert_eq!(l.len(), 2);
        let good = l.iter().find(|e| e.container.is_some()).unwrap();
        assert_eq!(good.container.as_ref().unwrap().0, "SettingsProfileSettings");
        assert_eq!((good.user.as_str(), good.scid.as_str(), good.blobs, good.modified), ("0009000000000001", "default", 2, 1_764_000_000));
        assert!(l.iter().any(|e| e.container.is_none()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn writes_back_up_first_and_refuse_while_running() {
        let (root, dir, base) = setup("write");
        let path = file::path(&dir, "SettingsProfileSettings");
        let before = std::fs::read(&path).unwrap();
        let mut c = read(&path).unwrap();
        let mut s = save_of(&c).unwrap().unwrap();
        s.set_name("Edited").unwrap();
        c = with_save(&c, &s);
        assert!(write_container(&root, &dir, &c, RUNNING).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let w = write_container(&root, &dir, &c, IDLE).unwrap();
        let b = w.backup.unwrap();
        assert!(b.starts_with(backups_root(&root)));
        assert_eq!(std::fs::read(&b).unwrap(), before);
        assert_eq!(save_of(&read(&path).unwrap()).unwrap().unwrap().header.name, "Edited");
        assert_eq!(backups(&root, &path), vec![b.clone()]);
        // undo
        let w2 = restore(&root, &path, &b, IDLE).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(backups(&root, &path).len(), 2);
        assert!(w2.backup.is_some());
        // a half-written runtime update blocks writes
        std::fs::write(dir.join("x.gsc.tmp"), b"").unwrap();
        assert!(write_container(&root, &dir, &c, IDLE).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn broken_or_online_containers_are_never_written() {
        let (root, dir, base) = setup("refuse");
        let mut c = container("SettingsProfileSettings");
        c.blobs[1].1[0x170 + 5] ^= 1;
        assert!(write_container(&root, &dir, &c, IDLE).unwrap_err().contains("would not load"));
        let mut c = container("SettingsProfileSettings");
        c.blobs[1].1[0x90] = b'x';
        assert!(write_container(&root, &dir, &c, IDLE).unwrap_err().contains("not ones this editor understands"));
        let c = container("OnlineProfile");
        assert!(write_container(&root, &dir, &c, IDLE).is_err());
        let c = container("bad name!");
        assert!(write_container(&root, &dir, &c, IDLE).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn container_export_import_round_trip() {
        let (root, dir, base) = setup("export");
        let path = file::path(&dir, "SettingsProfileSettings");
        let before = std::fs::read(&path).unwrap();
        let (gsc, man) = export_container(&path, &base.join("out")).unwrap();
        assert!(man.exists());
        assert_eq!(std::fs::read(&gsc).unwrap(), before);
        // into a fresh folder: byte-identical
        let other = root.join("0009000000000002").join("default");
        let w = import_container(&root, &other, &gsc, IDLE).unwrap();
        assert_eq!(std::fs::read(&w.path).unwrap(), before);
        assert!(w.backup.is_none());
        // over the original: backed up, same bytes
        let w = import_container(&root, &dir, &gsc, IDLE).unwrap();
        assert!(w.backup.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn tampered_container_imports_change_nothing() {
        let (root, dir, base) = setup("tamper");
        let path = file::path(&dir, "SettingsProfileSettings");
        let before = std::fs::read(&path).unwrap();
        let (gsc, man) = export_container(&path, &base.join("out")).unwrap();
        let good = std::fs::read(&gsc).unwrap();
        // a flipped byte in the file
        let mut t = good.clone();
        let n = t.len();
        t[n - 5] ^= 1;
        std::fs::write(&gsc, &t).unwrap();
        assert!(import_container(&root, &dir, &gsc, IDLE).is_err());
        // the manifest edited to match a tampered save: the save's own hashes refuse it
        let c = file::decode(&t).unwrap();
        std::fs::write(&man, manifest(&c, &t, "u", "s")).unwrap();
        assert!(import_container(&root, &dir, &gsc, IDLE).unwrap_err().contains("would not load"));
        // no manifest
        std::fs::write(&gsc, &good).unwrap();
        std::fs::remove_file(&man).unwrap();
        assert!(import_container(&root, &dir, &gsc, IDLE).is_err());
        // an unknown manifest field
        let c = file::decode(&good).unwrap();
        std::fs::write(&man, manifest(&c, &good, "u", "s").replacen("{", "{\n  \"extra\": 1,", 1)).unwrap();
        assert!(import_container(&root, &dir, &gsc, IDLE).is_err());
        // running
        std::fs::write(&man, manifest(&c, &good, "u", "s")).unwrap();
        assert!(import_container(&root, &dir, &gsc, RUNNING).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(backups(&root, &path).is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn whole_game_export_and_import() {
        let (root, dir, base) = setup("all");
        let c2 = container("Player");
        file::write(&file::path(&dir, &c2.name), &c2).unwrap();
        let out = base.join("all");
        assert_eq!(export_all(&root, &out).unwrap(), 2);
        let root2 = base.join("other").join("GameSave");
        let w = import_all(&root2, &out, IDLE).unwrap();
        assert_eq!(w.len(), 2);
        for e in list(&root) {
            let twin = root2.join(&e.user).join(&e.scid).join(e.path.file_name().unwrap());
            assert_eq!(std::fs::read(&twin).unwrap(), std::fs::read(&e.path).unwrap());
        }
        // one bad file: nothing at all is written
        let root3 = base.join("third").join("GameSave");
        let f = out.join("0009000000000001").join("default").join(file::path(Path::new(""), "Player"));
        let mut b = std::fs::read(&f).unwrap();
        b[20] ^= 1;
        std::fs::write(&f, b).unwrap();
        assert!(import_all(&root3, &out, IDLE).is_err());
        assert!(list(&root3).is_empty());
        let _ = std::fs::remove_dir_all(base);
    }
}
