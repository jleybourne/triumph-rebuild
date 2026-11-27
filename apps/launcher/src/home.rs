//! Where the launcher keeps its files, and profiles (named sets of settings, exported and imported as
//! `.toml`). Never the game folder.
//!
//! ```text
//! <home>/rebuild.toml        the active settings, passed to the layer with --config
//! <home>/profiles/<n>.toml   saved profiles
//! <home>/mods/<name>/        mods: files that replace title files by path
//! <home>/overlay/            the launcher's write overlay (the copy of settings.xml, U3)
//! <home>/frame-line.log      the frame line the layer appends (U5)
//! ```

use rebuild_config::Config;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
}

impl Home {
    /// `REBUILD_LAUNCHER_HOME`; else beside the launcher if a `rebuild.toml` is there (portable);
    /// else `%APPDATA%\triumph-rebuild`.
    pub fn locate() -> Home {
        if let Some(d) = std::env::var_os("REBUILD_LAUNCHER_HOME") {
            return Home { dir: d.into() };
        }
        if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            if exe_dir.join("rebuild.toml").is_file() {
                return Home { dir: exe_dir };
            }
        }
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        Home { dir: base.join("triumph-rebuild") }
    }

    pub fn rebuild_toml(&self) -> PathBuf {
        self.dir.join("rebuild.toml")
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

    /// Save the settings under a profile name (the file's `profile` field is set to it).
    pub fn save_profile(&self, name: &str, config: &Config) -> Result<PathBuf, String> {
        let name = profile_name(name)?;
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
        let home = Home { dir: crate::temp_dir("home") };
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
    fn an_imported_file_with_problems_reports_them() {
        let home = Home { dir: crate::temp_dir("import") };
        let p = home.dir.join("bad.toml");
        std::fs::write(&p, "[settings]\n\"display.width\" = 1\n\"no.such\" = true\n").unwrap();
        let c = load_file(&p).unwrap();
        assert_eq!(c.problems.len(), 2);
        assert_eq!(c.int("display.width"), 3840);
    }
}
