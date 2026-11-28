//! A game's stable id and its build, from its own package manifest (`MicrosoftGame.config`), read-only at
//! run time. Nothing about any title is written in this code.
//!
//! - **id:** the package identity's `Name` (`<Identity Name="...">`), lower-cased: the same for every copy
//!   and every update of the title, so a game moved to another drive keeps its saves and settings. Without
//!   one, `title-<TitleId>`; without either, a hash of the folder's absolute path (the launcher's id before
//!   this layout).
//! - **build:** the identity's `Version`; without one, `m` and a hash of the manifest's bytes (it changes with
//!   the package); `unknown` without a manifest.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub build: String,
    /// where the id came from: "identity", "title-id" or "path"
    pub source: &'static str,
}

const MAX_ID: usize = 64;

/// An id is a folder name: 1-64 of `a-z 0-9 . - _`, starting with a letter or digit.
pub fn is_game_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ID
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        && !s.contains("..")
}

/// The id before this layout: FNV-1a 64 of the folder's lower-cased absolute path, 16 hex digits.
pub fn path_id(dir: &Path) -> String {
    let s = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()).to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    format!("{:016x}", fnv(s.as_bytes()))
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The value of `attr` in the first `<tag ...>` element (double or single quotes).
fn attr(text: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = text.match_indices(&open).map(|(i, _)| i).find(|&i| text[i + open.len()..].starts_with(|c: char| c.is_whitespace()))?;
    let el = &text[start..start + text[start..].find('>')?];
    for q in ['"', '\''] {
        for sep in ["=", " = "] {
            let key = format!("{attr}{sep}{q}");
            let mut from = 0;
            while let Some(i) = el[from..].find(&key).map(|i| i + from) {
                // a whole attribute name: preceded by whitespace
                if el[..i].ends_with(|c: char| c.is_whitespace()) {
                    let v = i + key.len();
                    let j = el[v..].find(q)?;
                    return Some(el[v..v + j].trim().to_string());
                }
                from = i + key.len();
            }
        }
    }
    None
}

/// The text of the first `<tag>...</tag>`.
fn element(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let i = text.find(&open)? + open.len();
    let j = text[i..].find('<')?;
    Some(text[i..i + j].trim().to_string())
}

/// Lower-case, keep `a-z 0-9 . - _`, at most 64 characters; `None` if nothing usable is left.
fn sanitize(s: &str) -> Option<String> {
    let v: String = s.trim().to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_')).take(MAX_ID).collect();
    let v = v.trim_start_matches(['.', '-', '_']).replace("..", ".");
    is_game_id(&v).then_some(v)
}

fn build_of(version: Option<String>, manifest: &[u8]) -> String {
    version
        .and_then(|v| {
            let s: String = v.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).take(40).collect();
            (!s.is_empty() && !s.starts_with('.')).then_some(s)
        })
        .unwrap_or_else(|| format!("m{:016x}", fnv(manifest)))
}

/// The identity from a manifest's text; `dir` is only used for the path fallback.
pub fn identify_manifest(text: &str, dir: &Path) -> Identity {
    let build = build_of(attr(text, "Identity", "Version"), text.as_bytes());
    if let Some(id) = attr(text, "Identity", "Name").and_then(|n| sanitize(&n)) {
        return Identity { id, build, source: "identity" };
    }
    if let Some(t) = element(text, "TitleId").map(|t| t.trim_start_matches("0x").to_lowercase()).filter(|t| !t.is_empty() && t.len() <= 16 && t.chars().all(|c| c.is_ascii_hexdigit())) {
        return Identity { id: format!("title-{t}"), build, source: "title-id" };
    }
    Identity { id: path_id(dir), build, source: "path" }
}

/// The identity of the game in a folder: `MicrosoftGame.config` (any case) read once, read-only.
pub fn identify(dir: &Path) -> Identity {
    let cfg = std::fs::read_dir(dir).into_iter().flatten().flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("MicrosoftGame.config"));
    match cfg.and_then(|e| std::fs::read_to_string(e.path()).ok()) {
        Some(text) => identify_manifest(&text, dir),
        None => Identity { id: path_id(dir), build: "unknown".into(), source: "path" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_the_package_identity_and_survives_a_move() {
        let m = r#"<Game configVersion="1"><Identity Name="Studio.SomeTitle" Publisher="CN=Placeholder, O=Name=x" Version="1.2.3.0" /><TitleId>ABCD1234</TitleId></Game>"#;
        let a = identify_manifest(m, Path::new(r"C:\games\one"));
        let b = identify_manifest(m, Path::new(r"E:\elsewhere\two"));
        assert_eq!(a, Identity { id: "studio.sometitle".into(), build: "1.2.3.0".into(), source: "identity" });
        assert_eq!(a, b, "the folder does not matter");
        // a new build of the same title: same id, new build
        let up = identify_manifest(&m.replace("1.2.3.0", "1.2.4.0"), Path::new(r"C:\games\one"));
        assert_eq!((up.id.as_str(), up.build.as_str()), ("studio.sometitle", "1.2.4.0"));
        // single quotes and spacing
        let q = identify_manifest("<Game><Identity  Version='2.0' Name='A_B-c' /></Game>", Path::new("x"));
        assert_eq!((q.id.as_str(), q.build.as_str()), ("a_b-c", "2.0"));
    }

    #[test]
    fn fallbacks_title_id_then_path() {
        let t = identify_manifest("<Game><TitleId>0x1A2B3C4D</TitleId></Game>", Path::new("x"));
        assert_eq!(t.id, "title-1a2b3c4d");
        assert_eq!(t.source, "title-id");
        assert!(t.build.starts_with('m') && t.build.len() == 17, "{}", t.build);
        let p = identify_manifest("<Game><Executable Name=\"x.exe\" /></Game>", Path::new(r"C:\Games\Title\"));
        assert_eq!(p.id, path_id(Path::new(r"c:\games\title")));
        assert_eq!(p.source, "path");
        // an attribute that only ends in Name is not Name
        let n = identify_manifest(r#"<Game><Identity DisplayName="Nope" /></Game>"#, Path::new("y"));
        assert_eq!(n.source, "path");
    }

    #[test]
    fn ids_are_safe_folder_names() {
        for ok in ["studio.title", "0123456789abcdef", "title-1a2b", "a"] {
            assert!(is_game_id(ok), "{ok}");
        }
        for bad in ["", "..", ".hidden", "A", "a/b", r"a\b", "a..b", "a b", &"x".repeat(65), "-x"] {
            assert!(!is_game_id(bad), "{bad}");
        }
        assert_eq!(sanitize("  Weird/Name\\..With Spaces "), Some("weirdname.withspaces".into()));
        assert_eq!(sanitize("../.."), None);
    }
}
