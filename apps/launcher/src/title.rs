//! U3: the title's own settings. Each `title.*` registry entry names one element of the title's
//! `settings.xml` (`<name value="..." />`). At launch the launcher reads the title's file, writes the
//! effective values into a **copy** under its own overlay folder, and never writes the original.
//!
//! Where the title's file is: `REBUILD_TITLE_SETTINGS` (a path), else the first `settings.xml` under
//! the game folder (shallowest wins). No path to a title file is written in this code.

use rebuild_config::{Config, REGISTRY};
use std::path::{Path, PathBuf};

/// Registry key → element name in `settings.xml`. Proposed to the lead as a registry field
/// (`agents/launcher.md`); until then a test makes sure every `title.*` entry has a row.
pub const XML_NAMES: &[(&str, &str)] = &[("title.fsr_quality", "fsrQuality")];

pub fn xml_name(key: &str) -> Option<&'static str> {
    XML_NAMES.iter().find(|(k, _)| *k == key).map(|(_, n)| *n)
}

/// Set `<name ... value="X" ...>` for each (name, value). Every name must be present exactly once.
pub fn edit_xml(text: &str, edits: &[(&str, String)]) -> Result<String, String> {
    let mut out = text.to_string();
    for (name, value) in edits {
        let open = format!("<{name}");
        let starts: Vec<usize> = out
            .match_indices(&open)
            .map(|(i, _)| i)
            .filter(|&i| out[i + open.len()..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>'))
            .collect();
        let start = match starts.as_slice() {
            [one] => *one,
            [] => return Err(format!("settings.xml has no <{name}> element")),
            _ => return Err(format!("settings.xml has {} <{name}> elements", starts.len())),
        };
        let end = start + out[start..].find('>').ok_or_else(|| format!("<{name}> is not closed"))?;
        let tag = &out[start..end];
        let attr = tag.find(" value=\"").ok_or_else(|| format!("<{name}> has no value attribute"))? + " value=\"".len();
        let vstart = start + attr;
        let vend = vstart + out[vstart..end].find('"').ok_or_else(|| format!("<{name}> value is not closed"))?;
        out.replace_range(vstart..vend, value);
    }
    Ok(out)
}

/// The edits the launcher makes: every `title.*` entry's effective value (defaults included, so a
/// refused value left in the title's file, like `fsrQuality` 4, is replaced by the default).
pub fn edits(config: &Config) -> Result<Vec<(&'static str, String)>, String> {
    REGISTRY
        .iter()
        .filter(|s| s.group() == "title")
        .map(|s| {
            let name = xml_name(s.key).ok_or_else(|| format!("{}: no settings.xml element known", s.key))?;
            Ok((name, config.get(s.key).to_string()))
        })
        .collect()
}

/// The explicit `settings.xml`: the `game.title_settings` setting when set, else `REBUILD_TITLE_SETTINGS`.
pub fn explicit_source(config: &rebuild_config::Config, env: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let set = config.get("game.title_settings").to_string();
    if !set.trim().is_empty() {
        return Some(PathBuf::from(set.trim()));
    }
    env.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Find the title's `settings.xml`: the explicit path if it is a file, else a search of the game folder.
pub fn find_source(explicit: Option<&Path>, game_dir: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut level = vec![game_dir.to_path_buf()];
    for _ in 0..6 {
        let mut next = Vec::new();
        let mut found: Vec<PathBuf> = Vec::new();
        for d in &level {
            for e in std::fs::read_dir(d).into_iter().flatten().filter_map(|e| e.ok()) {
                let p = e.path();
                let Ok(t) = e.file_type() else { continue };
                if t.is_dir() {
                    next.push(p);
                } else if t.is_file() && e.file_name().to_string_lossy().eq_ignore_ascii_case("settings.xml") {
                    found.push(p);
                }
            }
        }
        if !found.is_empty() {
            found.sort();
            return found.into_iter().next();
        }
        next.sort();
        level = next;
    }
    None
}

/// An absolute, lower-case, backslashed path, for comparing folders the way Windows does.
fn norm(p: &Path) -> String {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().replace('/', "\\").to_lowercase()
}

/// The copy's path under the overlay.
pub fn overlay_copy(overlay: &Path) -> PathBuf {
    overlay.join("title").join("settings.xml")
}

/// Read the title's file, write the edited copy into the overlay. The source is only ever opened
/// for reading, and the copy is refused if it would land inside the game folder or on the source.
pub fn write_copy(source: &Path, game_dir: Option<&Path>, overlay: &Path, config: &Config) -> Result<PathBuf, String> {
    let dst = overlay_copy(overlay);
    // checked on absolute paths before anything is created, so a refused overlay leaves no folder behind
    let dst_dir = norm(dst.parent().unwrap());
    if let Some(g) = game_dir.filter(|g| !g.as_os_str().is_empty()) {
        let g = norm(g);
        if dst_dir == g || dst_dir.starts_with(&format!("{}\\", g.trim_end_matches('\\'))) {
            return Err(format!("the overlay {} is inside the game folder: refused", overlay.display()));
        }
    }
    if source.parent().map(norm) == Some(dst_dir) {
        return Err("the overlay copy would replace the title's own file: refused".into());
    }
    let text = std::fs::read_to_string(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let edited = edit_xml(&text, &edits(config)?)?;
    crate::write_atomic(&dst, &edited).map_err(|e| format!("{}: {e}", dst.display()))?;
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_config::Value;

    /// The shape of the title's file (synthetic: element names only, no title content).
    const SAMPLE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root>\n  <graphics>\n    <fsrQualityX value=\"9\" />\n    <fsrQuality value=\"2\" />\n    <gamma value=\"15\" />\n  </graphics>\n</root>\n";

    #[test]
    fn every_title_entry_names_an_element() {
        for s in REGISTRY.iter().filter(|s| s.group() == "title") {
            assert!(xml_name(s.key).is_some(), "{} has no element", s.key);
        }
    }

    #[test]
    fn edit_changes_only_the_named_value() {
        let out = edit_xml(SAMPLE, &[("fsrQuality", "3".into())]).unwrap();
        assert!(out.contains("<fsrQuality value=\"3\" />"));
        assert!(out.contains("<fsrQualityX value=\"9\" />"));
        assert_eq!(out.len(), SAMPLE.len());
        assert!(edit_xml(SAMPLE, &[("missing", "1".into())]).is_err());
        assert!(edit_xml("<a><b value=\"1\"/><b value=\"2\"/></a>", &[("b", "3".into())]).is_err());
    }

    #[test]
    fn the_title_settings_setting_comes_before_the_variable() {
        let mut c = Config::parse("").unwrap();
        let env = || Some(std::ffi::OsString::from(r"C:\env\settings.xml"));
        assert_eq!(explicit_source(&c, env()), Some(PathBuf::from(r"C:\env\settings.xml")));
        assert_eq!(explicit_source(&c, None), None);
        assert_eq!(explicit_source(&c, Some("".into())), None);
        c.set("game.title_settings", Value::Path(r"C:\set\settings.xml".into())).unwrap();
        assert_eq!(explicit_source(&c, env()), Some(PathBuf::from(r"C:\set\settings.xml")));
    }

    /// U3 closes here: the original's hash is unchanged, the copy carries the value.
    #[test]
    fn the_original_is_never_written() {
        let root = crate::temp_dir("title");
        let game = root.join("game");
        std::fs::create_dir_all(game.join("storage").join("settings")).unwrap();
        let src = game.join("storage").join("settings").join("settings.xml");
        std::fs::write(&src, SAMPLE).unwrap();
        let before = crate::hash_file(&src).unwrap();
        let mtime = std::fs::metadata(&src).unwrap().modified().unwrap();

        assert_eq!(find_source(None, &game), Some(src.clone()));
        let mut c = Config::parse("").unwrap();
        c.set("title.fsr_quality", Value::Int(1)).unwrap();
        let overlay = root.join("home").join("overlay");
        let dst = write_copy(&src, Some(&game), &overlay, &c).unwrap();

        assert_eq!(crate::hash_file(&src).unwrap(), before);
        assert_eq!(std::fs::metadata(&src).unwrap().modified().unwrap(), mtime);
        assert!(std::fs::read_to_string(&dst).unwrap().contains("<fsrQuality value=\"1\" />"));

        // the default is written too, replacing what the title had
        let d = write_copy(&src, Some(&game), &overlay, &Config::parse("").unwrap()).unwrap();
        assert!(std::fs::read_to_string(&d).unwrap().contains("<fsrQuality value=\"3\" />"));
        assert_eq!(crate::hash_file(&src).unwrap(), before);

        // an overlay inside the game folder, or on the source's own folder, is refused
        assert!(write_copy(&src, Some(&game), &game.join("overlay"), &c).is_err());
        assert!(!game.join("overlay").exists(), "a refused overlay creates nothing");
        assert_eq!(crate::hash_file(&src).unwrap(), before);

        // an overlay whose copy would land on the source itself is refused, even outside a game folder
        let other = root.join("other");
        std::fs::create_dir_all(other.join("title")).unwrap();
        let own = overlay_copy(&other);
        std::fs::write(&own, SAMPLE).unwrap();
        let own_hash = crate::hash_file(&own).unwrap();
        assert!(write_copy(&own, None, &other, &c).is_err());
        assert_eq!(crate::hash_file(&own).unwrap(), own_hash);
    }

    /// The same, on the title's real file when `REBUILD_TITLE_SETTINGS` names it (skipped otherwise).
    #[test]
    fn the_real_file_is_never_written() {
        let Some(src) = std::env::var_os("REBUILD_TITLE_SETTINGS").map(PathBuf::from) else { return };
        let before = crate::hash_file(&src).unwrap();
        let overlay = crate::temp_dir("title-real");
        let dst = write_copy(&src, None, &overlay, &Config::parse("").unwrap()).unwrap();
        assert_eq!(crate::hash_file(&src).unwrap(), before);
        assert!(std::fs::read_to_string(&dst).unwrap().contains("<fsrQuality value=\"3\""));
        println!("title settings: original {before:016x} unchanged; copy {}", dst.display());
    }
}
