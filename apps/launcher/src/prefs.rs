//! Launcher-wide preferences: the `launcher.*` registry entries (today `launcher.theme`) and
//! `storage.cache_dir` (read by `rebuild_storage` at start), saved once
//! for the whole launcher in `<app home>/launcher.toml`, not per game, so switching games never
//! switches the theme. The file is an ordinary `rebuild.toml`-shaped file, validated by
//! `rebuild_config::Config` (a bad value is refused and the default used).

use rebuild_config::{Config, Value};
use std::path::{Path, PathBuf};

pub fn file(root: &Path) -> PathBuf {
    root.join("launcher.toml")
}

fn load(root: &Path) -> Config {
    std::fs::read_to_string(file(root)).ok().and_then(|t| Config::parse(&t).ok()).unwrap_or_else(|| Config::parse("").expect("empty config"))
}

/// "dark" (the default) or "light".
pub fn theme(root: &Path) -> String {
    load(root).choice("launcher.theme")
}

pub fn save_theme(root: &Path, theme: &str) -> Result<(), String> {
    let mut c = load(root);
    c.set("launcher.theme", Value::Choice(theme.into()))?;
    crate::write_atomic(&file(root), &c.to_toml()).map_err(|e| format!("{}: {e}", file(root).display()))
}

/// `storage.cache_dir` ("" = the default place).
pub fn cache_dir(root: &Path) -> String {
    load(root).get(rebuild_storage::CACHE_DIR_KEY).to_string()
}

/// Save `storage.cache_dir`; it takes effect when the launcher next starts (the roots are resolved once).
pub fn save_cache_dir(root: &Path, dir: &str) -> Result<(), String> {
    let mut c = load(root);
    c.set(rebuild_storage::CACHE_DIR_KEY, Value::Path(dir.trim().into()))?;
    crate::write_atomic(&file(root), &c.to_toml()).map_err(|e| format!("{}: {e}", file(root).display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_round_trips_and_defaults_to_dark() {
        let d = crate::temp_dir("prefs");
        assert_eq!(theme(&d), "dark");
        save_theme(&d, "light").unwrap();
        assert_eq!(theme(&d), "light");
        let back = Config::parse(&std::fs::read_to_string(file(&d)).unwrap()).unwrap();
        assert!(back.problems.is_empty());
        assert_eq!(back.choice("launcher.theme"), "light");
        assert!(save_theme(&d, "purple").is_err(), "refused by the registry");
        assert_eq!(theme(&d), "light", "and the saved value kept");
        save_theme(&d, "dark").unwrap();
        assert_eq!(theme(&d), "dark");
        std::fs::write(file(&d), "[settings]\n\"launcher.theme\" = \"neon\"\n").unwrap();
        assert_eq!(theme(&d), "dark", "a bad value falls back to the default");
    }

    #[test]
    fn the_cache_folder_is_saved_where_storage_reads_it() {
        let d = crate::temp_dir("prefs-cache");
        assert_eq!(cache_dir(&d), "");
        save_theme(&d, "light").unwrap();
        let big = d.join("big-drive");
        save_cache_dir(&d, &big.display().to_string()).unwrap();
        assert_eq!(theme(&d), "light", "the theme is kept");
        assert_eq!(rebuild_storage::cache_dir_setting(&d), Some(big.clone()));
        let st = rebuild_storage::Storage::resolve_with(&rebuild_storage::Env { data_dir: Some(d.clone()), temp: d.clone(), ..Default::default() });
        assert_eq!((st.cache, st.cache_from), (big, rebuild_storage::CacheFrom::Setting));
        save_cache_dir(&d, "").unwrap();
        assert_eq!(rebuild_storage::cache_dir_setting(&d), None);
    }
}
