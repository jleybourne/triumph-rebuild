//! Console paths: a drive letter and a normalised list of components.
//!
//! The title opens its package as `G:\..`, temporary files as `T:\..`, and its persistent local
//! storage as the root of a drive of its own (any letter the runtime gives it). Every form the
//! title uses is accepted: `X:\a`, `X:/a`, `X:a` and `X:` (both relative to the drive's root, as on
//! the console), and the verbatim `\\?\X:\a`. `.` is dropped, `..` stops at the drive's root, and
//! doubled separators collapse. Lookups are case-insensitive, like the console's file system:
//! [`ConsolePath::key`] is the folded form every layer is indexed by.

use crate::Error;

/// A path on one of the console's drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolePath {
    /// Upper-case drive letter.
    pub drive: char,
    /// Components as the title wrote them (case kept), with `.` and `..` resolved.
    pub parts: Vec<String>,
}

impl ConsolePath {
    pub fn parse(path: &str) -> Result<Self, Error> {
        let (verbatim, rest) = match path.strip_prefix(r"\\?\").or_else(|| path.strip_prefix("//?/")) {
            Some(rest) => (true, rest),
            None => (false, path),
        };
        let mut chars = rest.chars();
        let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
            return Err(Error::NotOnDrive(path.to_owned()));
        };
        if !letter.is_ascii_alphabetic() {
            return Err(Error::NotOnDrive(path.to_owned()));
        }
        let mut parts: Vec<String> = Vec::new();
        for raw in chars.as_str().split(['\\', '/']) {
            match raw {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                _ => {
                    // Win32 drops trailing dots and spaces outside the verbatim form.
                    let part = if verbatim { raw } else { raw.trim_end_matches(['.', ' ']) };
                    if part.is_empty() {
                        continue;
                    }
                    if part.chars().any(|c| matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') || c < ' ') {
                        return Err(Error::InvalidPath(path.to_owned()));
                    }
                    parts.push(part.to_owned());
                }
            }
        }
        Ok(Self { drive: letter.to_ascii_uppercase(), parts })
    }

    /// The case-folded key, components joined by `/`; the drive root is `""`.
    pub fn key(&self) -> String {
        key_of(&self.parts)
    }

    pub fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    /// The containing directory (the root's parent is the root).
    pub fn parent(&self) -> ConsolePath {
        let mut parts = self.parts.clone();
        parts.pop();
        ConsolePath { drive: self.drive, parts }
    }

    pub fn join(&self, name: &str) -> ConsolePath {
        let mut parts = self.parts.clone();
        parts.push(name.to_owned());
        ConsolePath { drive: self.drive, parts }
    }

    /// `G:\a\b` form.
    pub fn display(&self) -> String {
        format!("{}:\\{}", self.drive, self.parts.join("\\"))
    }
}

/// Case folding for lookups. ASCII paths (nearly all of them) take the fast path.
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}

pub(crate) fn key_of(parts: &[String]) -> String {
    let mut key = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            key.push('/');
        }
        key.push_str(&fold(p));
    }
    key
}

/// `a/b` + `c` → `a/b/c`; `""` + `c` → `c`.
pub(crate) fn child_key(parent: &str, folded_name: &str) -> String {
    if parent.is_empty() {
        folded_name.to_owned()
    } else {
        format!("{parent}/{folded_name}")
    }
}

/// Every proper ancestor key of `key`, nearest last (`a/b/c` → `a`, `a/b`).
pub(crate) fn ancestors(key: &str) -> impl Iterator<Item = &str> {
    key.match_indices('/').map(move |(i, _)| &key[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ConsolePath {
        ConsolePath::parse(s).unwrap()
    }

    #[test]
    fn package_drive_forms() {
        let want = ConsolePath { drive: 'G', parts: vec!["data".into(), "a.rpf".into()] };
        assert_eq!(p(r"G:\data\a.rpf"), want);
        assert_eq!(p("G:/data/a.rpf"), want);
        assert_eq!(p(r"g:\data\a.rpf"), want, "letter case");
        assert_eq!(p(r"G:data\a.rpf"), want, "X:name is relative to the root");
        assert_eq!(p(r"\\?\G:\data\a.rpf"), want, "verbatim");
        assert_eq!(p(r"G:\\data//a.rpf\"), want, "doubled and trailing separators");
        assert!(p("G:").is_root());
        assert!(p(r"G:\").is_root());
    }

    #[test]
    fn temp_and_storage_drives() {
        assert_eq!(p(r"T:\cache\x.bin").drive, 'T');
        assert_eq!(p(r"T:\cache\x.bin").key(), "cache/x.bin");
        // storage is a drive of its own; the title keeps what precedes the last `:\` and mounts it
        assert_eq!(p(r"S:\save0\profile.dat").drive, 'S');
        assert_eq!(p(r"s:/Save0/Profile.DAT").key(), "save0/profile.dat");
    }

    #[test]
    fn dots_stop_at_the_root() {
        assert_eq!(p(r"G:\a\.\b\..\c").key(), "a/c");
        assert_eq!(p(r"G:\..\..\x").key(), "x", ".. never leaves the drive");
        assert_eq!(p(r"G:\a\b..\c. ").key(), "a/b/c", "trailing dots and spaces dropped");
        assert_eq!(p(r"\\?\G:\a\b.").key(), "a/b.", "kept in the verbatim form");
    }

    #[test]
    fn case_folds_for_keys_only() {
        let a = p(r"G:\Data\Shaders.RPF");
        assert_eq!(a.key(), "data/shaders.rpf");
        assert_eq!(a.parts, vec!["Data", "Shaders.RPF"], "the title's case is kept for creation");
        assert_eq!(p(r"G:\ÄBC").key(), "äbc", "non-ASCII folds too");
    }

    #[test]
    fn refused() {
        assert!(matches!(ConsolePath::parse(r"data\a.rpf"), Err(Error::NotOnDrive(_))));
        assert!(matches!(ConsolePath::parse(r"\\server\share"), Err(Error::NotOnDrive(_))));
        assert!(matches!(ConsolePath::parse(r"1:\x"), Err(Error::NotOnDrive(_))));
        assert!(matches!(ConsolePath::parse(r"G:\a:stream"), Err(Error::InvalidPath(_))));
        assert!(matches!(ConsolePath::parse("G:\\a*"), Err(Error::InvalidPath(_))));
    }

    #[test]
    fn ancestor_keys() {
        assert_eq!(ancestors("a/b/c").collect::<Vec<_>>(), ["a", "a/b"]);
        assert_eq!(ancestors("a").count(), 0);
        assert_eq!(child_key("", "x"), "x");
        assert_eq!(child_key("a", "x"), "a/x");
    }
}
