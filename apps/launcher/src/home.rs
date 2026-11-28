//! A game's files as the launcher uses them, and profiles (named sets of settings, exported and imported
//! as `.toml`). Never the game folder. Where they are is `rebuild_storage`'s (`docs/design/storage.md`).
//!
//! ```text
//! <home>/settings.toml       the active settings, passed to the layer with --config (was rebuild.toml)
//! <home>/profiles/<n>.toml   saved profiles
//! <home>/mods/<name>/        mods: files that replace title files by path
//! <home>/overlay/            the game's write overlay (the copy of settings.xml, U3)
//! <home>/frame-line.log      the frame line of a proof run without a session (U5)
//! <sessions>/<stamp>-<profile>/   one folder per launch (L6, `session.rs`): <local>/logs/<game id>/
//! ```

use rebuild_config::Config;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
    /// where this game's session folders go
    pub sessions: PathBuf,
}

impl Home {
    /// A home with its sessions inside it (no game selected; tests).
    pub fn at(dir: impl Into<PathBuf>) -> Home {
        let dir = dir.into();
        Home { sessions: crate::session::sessions_dir(&dir), dir }
    }

    /// A game's home: its folder in the data root and its logs.
    pub fn for_game(store: &rebuild_storage::GameStore) -> Home {
        Home { dir: store.dir.clone(), sessions: store.sessions_dir() }
    }

    pub fn rebuild_toml(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.dir.join("profiles")
    }
    pub fn mods_dir(&self) -> PathBuf {
        self.dir.join("mods")
    }
    pub fn overlay_dir(&self) -> PathBuf {
        self.dir.join("overlay")
    }
    pub fn frame_log(&self) -> PathBuf {
        self.dir.join("frame-line.log")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.sessions.clone()
    }

    /// The active settings; a missing file is all defaults.
    pub fn load(&self) -> Result<Config, String> {
        load_file(&self.rebuild_toml()).or_else(|e| {
            if self.rebuild_toml().exists() {
                Err(e)
            } else {
                Config::parse("")
            }
        })
    }

    pub fn profiles(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.profiles_dir())
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                (p.extension().is_some_and(|x| x.eq_ignore_ascii_case("toml")))
                    .then(|| p.file_stem()?.to_str().map(String::from))
                    .flatten()
            })
            .collect();
        v.sort();
        v
    }

    /// Save the settings under a profile name (the file's `profile` field is set to it). A built-in's
    /// name is refused: built-ins are read-only, so an edited one is saved as a copy under a new name.
    pub fn save_profile(&self, name: &str, config: &Config) -> Result<PathBuf, String> {
        let name = profile_name(name)?;
        if let Some(b) = rebuild_config::builtin(name) {
            return Err(format!("\"{}\" is a built-in profile and can't be overwritten: save it under another name (e.g. \"{} copy\")", b.name, b.name));
        }
        let mut c = config.clone();
        c.profile = name.to_string();
        let path = self.profiles_dir().join(format!("{name}.toml"));
        crate::write_atomic(&path, &c.to_toml()).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    pub fn load_profile(&self, name: &str) -> Result<Config, String> {
        load_file(&self.profiles_dir().join(format!("{}.toml", profile_name(name)?)))
    }
}

/// The test matrix: one `rebuild.toml` per chosen built-in in `dir` (`<name>.toml`; all of them when
/// `names` is empty), each the user's settings in `base` with that profile applied, and its own
/// frame-line log beside it (`<name>.frame-line.log`) so runs in turn don't overwrite each other. Nothing
/// is run. Every file is checked by `Config::parse` (0 problems) before any is written.
pub fn write_matrix(dir: &Path, base: &Config, names: &[&str]) -> Result<Vec<PathBuf>, String> {
    let chosen: Vec<&rebuild_config::Builtin> = if names.is_empty() {
        rebuild_config::BUILTIN_PROFILES.iter().collect()
    } else {
        names
            .iter()
            .map(|n| rebuild_config::builtin(n).ok_or_else(|| format!("\"{n}\": no such built-in profile")))
            .collect::<Result<_, _>>()?
    };
    let mut files = Vec::new();
    for b in chosen {
        let mut c = rebuild_config::profiles::matrix_config(base, b)?;
        let log = dir.join(format!("{}.frame-line.log", b.name));
        c.set("diagnostics.frame_log_path", rebuild_config::Value::Path(log.display().to_string()))?;
        let text = c.to_toml();
        let back = Config::parse(&text)?;
        if !back.problems.is_empty() {
            return Err(format!("{}: {}", b.name, back.problems.join("; ")));
        }
        files.push((dir.join(format!("{}.toml", b.name)), text));
    }
    for (path, text) in &files {
        crate::write_atomic(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(files.into_iter().map(|(p, _)| p).collect())
}

/// A profile name is a file name: letters, digits, `-`, `_`, space.
fn profile_name(name: &str) -> Result<&str, String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' ')) {
        return Err(format!("\"{name}\": a profile name uses letters, digits, '-', '_' and spaces"));
    }
    Ok(n)
}

/// Read a `rebuild.toml` (import). Problems in the file come back in `Config::problems`.
pub fn load_file(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Config::parse(&text)
}

/// Write the settings to any path (export).
pub fn export(config: &Config, path: &Path) -> Result<(), String> {
    crate::write_atomic(path, &config.to_toml()).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_config::Value;

    #[test]
    fn profiles_save_list_load_export_import() {
        let home = Home::at(crate::temp_dir("home"));
        let mut c = home.load().unwrap();
        assert_eq!(c.changed().count(), 0, "no file = defaults");
        c.set("display.width", Value::Int(2560)).unwrap();
        home.save_profile("1440p", &c).unwrap();
        home.save_profile("default", &Config::parse("").unwrap()).unwrap();
        assert_eq!(home.profiles(), vec!["1440p".to_string(), "default".to_string()]);
        let back = home.load_profile("1440p").unwrap();
        assert_eq!(back.int("display.width"), 2560);
        assert_eq!(back.profile, "1440p");
        assert!(back.problems.is_empty());

        let out = home.dir.join("exported.toml");
        export(&back, &out).unwrap();
        assert_eq!(load_file(&out).unwrap(), back);
        assert!(home.save_profile("../escape", &c).is_err());
    }

    #[test]
    fn builtins_cannot_be_overwritten() {
        let home = Home::at(crate::temp_dir("builtin"));
        let c = Config::parse("").unwrap();
        for b in rebuild_config::BUILTIN_PROFILES {
            assert!(home.save_profile(b.name, &c).unwrap_err().contains("built-in"), "{}", b.name);
            assert!(home.save_profile(&b.name.to_uppercase(), &c).is_err(), "{}: any case", b.name);
        }
        assert!(home.profiles().is_empty(), "nothing written");
        // an edited built-in is saved as a copy, and the built-in is still itself
        let mut edited = c.clone();
        rebuild_config::builtin("series-s").unwrap().apply(&mut edited).unwrap();
        edited.set("display.width", Value::Int(1920)).unwrap();
        home.save_profile("series-s copy", &edited).unwrap();
        assert_eq!(home.profiles(), vec!["series-s copy".to_string()]);
        assert_eq!(rebuild_config::builtin("series-s").unwrap().value("display.width"), Some(Value::Int(2560)));
    }

    #[test]
    fn the_matrix_writes_one_parsing_file_per_builtin() {
        let home = Home::at(crate::temp_dir("matrix"));
        let mut base = Config::parse("").unwrap();
        base.set("game.dir", Value::Path(r"D:\games\title".into())).unwrap();
        base.set("display.vsync", Value::Bool(true)).unwrap();
        let dir = home.dir.join("matrix");
        let all = write_matrix(&dir, &base, &[]).unwrap();
        assert_eq!(all.len(), rebuild_config::BUILTIN_PROFILES.len());
        for (p, b) in all.iter().zip(rebuild_config::BUILTIN_PROFILES) {
            let c = Config::parse(&std::fs::read_to_string(p).unwrap()).unwrap();
            assert!(c.problems.is_empty(), "{}: {:#?}", p.display(), c.problems);
            assert!(b.matches(&c) && c.profile == b.name, "{}", b.name);
            assert!(c.bool("display.vsync"), "the user's other settings are kept");
            assert_eq!(c.get("game.dir"), Value::Path(r"D:\games\title".into()));
            assert!(c.get("diagnostics.frame_log_path").to_string().ends_with(&format!("{}.frame-line.log", b.name)));
        }
        let some = write_matrix(&home.dir.join("two"), &base, &["series-x", "stress-cpu"]).unwrap();
        assert_eq!(some.len(), 2);
        assert!(write_matrix(&home.dir.join("bad"), &base, &["series-x", "no-such"]).is_err());
        assert!(!home.dir.join("bad").exists(), "nothing written for an unknown name");
    }

    #[test]
    fn an_imported_file_with_problems_reports_them() {
        let home = Home::at(crate::temp_dir("import"));
        let p = home.dir.join("bad.toml");
        std::fs::write(&p, "[settings]\n\"display.width\" = 1\n\"no.such\" = true\n").unwrap();
        let c = load_file(&p).unwrap();
        assert_eq!(c.problems.len(), 2);
        assert_eq!(c.int("display.width"), 3840);
    }
}
