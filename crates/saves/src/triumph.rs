//! Import from the owner's older build (the fork's `victory-hle` XGameSave, read for its layout only):
//!
//! ```text
//! <saves>/<xuid>/<scid>/<name, made safe>~<FNV-1a 64 of the exact name>/v<N>/{0, 1, …, manifest}
//! ```
//!
//! The highest `v<N>` is the committed container (`v<N>.new` is an update that never finished);
//! `manifest` is length-prefixed (u32 LE) strings: container name, display name, then blob names, blob
//! `i` being file `i`. Nothing here writes, renames or deletes anything in the fork's folder: it is read,
//! every container is checked, and only then is each written as our `.gsc` through
//! [`crate::store::write_container`] (backup first, refused while the title runs).

use crate::store::{self, Written};
use rebuild_gdk::areas::xgamesave::Stored;
use std::path::{Path, PathBuf};

/// One committed container found in the fork's folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub user: String,
    pub scid: String,
    /// The `v<N>` folder read.
    pub version: PathBuf,
    pub stored: Stored,
}

fn fnv64(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn parse_manifest(b: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = b;
    while !rest.is_empty() {
        let (n, tail) = rest.split_first_chunk::<4>().ok_or("manifest cut short")?;
        let n = u32::from_le_bytes(*n) as usize;
        let s = tail.get(..n).ok_or("manifest cut short")?;
        out.push(String::from_utf8(s.to_vec()).map_err(|_| "manifest name not UTF-8")?);
        rest = &tail[n..];
    }
    if out.len() < 2 {
        return Err("manifest without a name and display name".into());
    }
    Ok(out)
}

/// The fork's `saves` folder for `src`: `src` itself, or its `saves` subfolder.
pub fn saves_dir(src: &Path) -> PathBuf {
    let sub = src.join("saves");
    if sub.is_dir() { sub } else { src.to_path_buf() }
}

/// A folder name our runtime would use as it is (lower-case letters, digits, `_`, `-`).
fn plain(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One container folder's committed version, read.
fn read_container(dir: &Path) -> Result<Option<(PathBuf, Stored)>, String> {
    let folder = name_of(dir);
    let Some((_, hash)) = folder.rsplit_once('~') else { return Ok(None) };
    let mut best: Option<u64> = None;
    for v in dirs(dir) {
        if let Some(n) = name_of(&v).strip_prefix('v').and_then(|n| n.parse::<u64>().ok()) {
            best = best.max(Some(n));
        }
    }
    let Some(n) = best else { return Ok(None) };
    let version = dir.join(format!("v{n}"));
    let man = version.join("manifest");
    let names = parse_manifest(&std::fs::read(&man).map_err(|e| format!("{}: {e}", man.display()))?).map_err(|e| format!("{}: {e}", man.display()))?;
    let (name, display) = (names[0].clone(), names[1].clone());
    if format!("{:016x}", fnv64(&name)) != hash {
        return Err(format!("{}: the folder's hash is not of the manifest's name {name:?}", dir.display()));
    }
    let mut blobs = Vec::new();
    for (i, b) in names[2..].iter().enumerate() {
        let f = version.join(i.to_string());
        blobs.push((b.clone(), std::fs::read(&f).map_err(|e| format!("{}: {e}", f.display()))?));
    }
    let modified = std::fs::metadata(&man)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let mut stored = Stored { name, display, modified, blobs };
    // the save's own stamp is seconds since 1970 (real saves: it equals the version folder's time to the
    // second), and survives copying, which a file time does not
    if let Some(Ok(s)) = store::save_of(&stored) {
        if s.header.stamp > 0 && s.header.stamp < 1 << 40 {
            stored.modified = s.header.stamp as i64;
        }
    }
    Ok(Some((version, stored)))
}

/// Every committed container under the fork's folder `src`; refuses on the first one it cannot read.
pub fn scan(src: &Path) -> Result<Vec<Found>, String> {
    let saves = saves_dir(src);
    let mut out = Vec::new();
    for user in dirs(&saves) {
        for scid in dirs(&user) {
            for c in dirs(&scid) {
                if let Some((version, stored)) = read_container(&c)? {
                    out.push(Found { user: name_of(&user), scid: name_of(&scid), version, stored });
                }
            }
        }
    }
    Ok(out)
}

/// Imports everything [`scan`] finds into `root` (ours), all or nothing: every container is checked
/// (names our runtime would use, `store::refuse`) before any is written.
pub fn import(root: &Path, src: &Path, running: &dyn Fn() -> bool) -> Result<Vec<Written>, String> {
    let found = scan(src)?;
    if found.is_empty() {
        return Err(format!("no containers under {}", saves_dir(src).display()));
    }
    for f in &found {
        if !plain(&f.user) || !plain(&f.scid) {
            return Err(format!("{}/{}: folder names our runtime would not use as they are", f.user, f.scid));
        }
        if let Some(why) = store::refuse(&f.stored) {
            return Err(format!("{}: {why}", f.version.display()));
        }
    }
    if running() {
        return Err("the title is running: saves are only written while it is closed".into());
    }
    found.iter().map(|f| store::write_container(root, &root.join(&f.user).join(&f.scid), &f.stored, running)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{build, DATA_BLOB, HEADER_BLOB};
    use rebuild_gdk::areas::xgamesave::file;

    fn manifest(name: &str, display: &str, blobs: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for s in [name, display].into_iter().chain(blobs.iter().copied()) {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        out
    }

    /// A fork folder as the fork writes one (synthetic saves): two versions of one container, a version
    /// that never committed, data before header as the fork orders them.
    fn fork(base: &Path) -> (PathBuf, Vec<u8>, Vec<u8>) {
        let s = build(2, "Profile Settings", 0x6b07_786d, "SettingsProfileSettings", br#"{"a": 1}"#).unwrap();
        let (h, d) = (s.header.to_bytes(), s.data());
        let scid = base.join("saves").join("0009000000000001").join("00000000-0000-0000-0000-00007952987a");
        let name = "SettingsProfileSettings";
        let c = scid.join(format!("{name}~{:016x}", fnv64(name)));
        for v in ["v1", "v2", "v3.new"] {
            std::fs::create_dir_all(c.join(v)).unwrap();
            std::fs::write(c.join(v).join("manifest"), manifest(name, name, &[DATA_BLOB, HEADER_BLOB])).unwrap();
            std::fs::write(c.join(v).join("0"), if v == "v2" { &d[..] } else { b"old" }).unwrap();
            std::fs::write(c.join(v).join("1"), if v == "v2" { &h[..] } else { b"old" }).unwrap();
        }
        (base.to_path_buf(), h, d)
    }

    fn snapshot(p: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![p.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push((e.path(), std::fs::read(e.path()).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rebuild-saves-triumph-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn imports_the_committed_version_and_leaves_the_fork_alone() {
        let base = temp("ok");
        let (src, h, d) = fork(&base.join("fork"));
        let before = snapshot(&src);
        let root = base.join("ours").join("GameSave");
        let found = scan(&src).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].version.ends_with("v2"));
        let w = import(&root, &src, &|| false).unwrap();
        assert_eq!(w.len(), 1);
        let c = file::decode(&std::fs::read(&w[0].path).unwrap()).unwrap();
        assert_eq!(c.name, "SettingsProfileSettings");
        assert_eq!(c.modified, 0x6b07_786d);
        assert_eq!(c.blobs, vec![(DATA_BLOB.to_string(), d), (HEADER_BLOB.to_string(), h)]);
        assert!(w[0].path.starts_with(root.join("0009000000000001").join("00000000-0000-0000-0000-00007952987a")));
        assert_eq!(snapshot(&src), before);
        // again: the container now exists, so it is backed up first
        let w = import(&root, &src.join("saves"), &|| false).unwrap();
        assert!(w[0].backup.is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn refusals_write_nothing() {
        let base = temp("bad");
        let (src, _, _) = fork(&base.join("fork"));
        let root = base.join("ours").join("GameSave");
        assert!(import(&root, &src, &|| true).is_err());
        assert!(!root.exists());
        // a broken save: the data hash fails
        let c = std::fs::read_dir(src.join("saves/0009000000000001/00000000-0000-0000-0000-00007952987a")).unwrap().next().unwrap().unwrap().path();
        let f = c.join("v2").join("0");
        let mut d = std::fs::read(&f).unwrap();
        let n = d.len();
        d[n - 1] ^= 1;
        std::fs::write(&f, d).unwrap();
        assert!(import(&root, &src, &|| false).unwrap_err().contains("would not load"));
        assert!(!root.exists());
        // a folder hash that is not of the name
        std::fs::rename(&c, c.with_file_name("SettingsProfileSettings~0000000000000000")).unwrap();
        assert!(import(&root, &src, &|| false).is_err());
        assert!(import(&root, &base.join("nothing"), &|| false).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
