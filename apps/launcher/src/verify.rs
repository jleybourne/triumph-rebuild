//! "Game folder read-only" (Mods screen, Verify now): walks the game folder reading **metadata only**
//! (`symlink_metadata`: no file is opened, links are not followed) and counts files that are not
//! read-only. The folder is never written; this only reports.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadOnly {
    pub files: u64,
    pub dirs: u64,
    /// files without the read-only attribute
    pub writable: u64,
    /// the first few of them, to name in the UI
    pub examples: Vec<PathBuf>,
    /// the walk stopped at the file limit
    pub truncated: bool,
}

impl ReadOnly {
    pub fn ok(&self) -> bool {
        self.writable == 0 && self.files > 0
    }
}

/// Walk `dir` (at most `limit` files).
pub fn check(dir: &Path, limit: u64) -> Result<ReadOnly, String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let mut r = ReadOnly::default();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        r.dirs += 1;
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                r.files += 1;
                if !m.permissions().readonly() {
                    r.writable += 1;
                    if r.examples.len() < 5 {
                        r.examples.push(p);
                    }
                }
                if r.files >= limit {
                    r.truncated = true;
                    return Ok(r);
                }
            }
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_ro(p: &Path, ro: bool) {
        let mut perm = std::fs::metadata(p).unwrap().permissions();
        perm.set_readonly(ro);
        std::fs::set_permissions(p, perm).unwrap();
    }

    #[test]
    fn writable_files_are_counted_and_nothing_is_changed() {
        let d = crate::temp_dir("verify");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        let files = [d.join("x.bin"), d.join("a/y.bin"), d.join("a/b/z.bin")];
        for f in &files {
            std::fs::write(f, b"data").unwrap();
            set_ro(f, true);
        }
        let r = check(&d, 1000).unwrap();
        assert_eq!((r.files, r.writable, r.dirs), (3, 0, 3));
        assert!(r.ok());
        set_ro(&files[1], false);
        let before = std::fs::metadata(&files[1]).unwrap().modified().unwrap();
        let r = check(&d, 1000).unwrap();
        assert_eq!(r.writable, 1);
        assert_eq!(r.examples, vec![files[1].clone()]);
        assert!(!r.ok());
        assert_eq!(std::fs::metadata(&files[1]).unwrap().modified().unwrap(), before, "only read");
        assert!(check(&d, 2).unwrap().truncated);
        assert!(check(&d.join("missing"), 10).is_err());
        for f in &files {
            set_ro(f, false);
        }
    }
}
