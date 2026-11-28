//! Saving a game's settings so they are never replaced or lost.
//!
//! - **Explicit values are kept.** A key the file held, or the user set, is written even when it equals
//!   today's default, so a later build with a new default never resets it. Defaults only fill keys that
//!   are missing. "Reset to default" is the one thing that removes a key.
//! - **Unknown keys are kept.** Settings keys this build's registry does not know (from a newer or older
//!   build), and unknown top-level keys, are written back as they were.
//! - **Safe writes.** Write to a temporary file, keep the previous file as `.bak`, then rename. A file that
//!   does not parse is recovered from `.bak` (and the caller is told, for a toast).
//! - **Schema.** `schema = 1` at the top. Migration only ever moves forward and never removes a value; a
//!   file from a newer schema is kept as it is (its number is not lowered).

use crate::model::Model;
use rebuild_config::{Config, Value, REGISTRY};
use std::path::{Path, PathBuf};

pub const SCHEMA: i64 = 1;
const KNOWN_TOP: &[&str] = &["profile", "stable_only", "settings", "mods", "schema"];

#[derive(Debug, Clone)]
pub struct Store {
    pub model: Model,
    extra_settings: toml::Table,
    extra_top: toml::Table,
    pub schema: i64,
    /// the main file did not parse and `.bak` was used
    pub recovered: bool,
}

pub fn bak(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Forward migration. Version 0 (no `schema` key) to 1 adds the key and changes nothing else.
fn migrate(doc: &mut toml::Table) -> i64 {
    let from = doc.get("schema").and_then(|v| v.as_integer()).unwrap_or(0);
    if from < 1 {
        // 0 → 1: nothing renamed, nothing removed
    }
    from.max(SCHEMA)
}

impl Store {
    pub fn new(config: Config) -> Store {
        Store { model: Model::new(config), extra_settings: toml::Table::new(), extra_top: toml::Table::new(), schema: SCHEMA, recovered: false }
    }

    fn from_text(text: &str) -> Result<Store, String> {
        let mut doc: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
        let schema = migrate(&mut doc);
        let config = Config::parse(&toml::to_string(&doc).map_err(|e| e.to_string())?)?;
        let mut s = Store::new(config);
        s.schema = schema;
        for (k, v) in &doc {
            if !KNOWN_TOP.contains(&k.as_str()) {
                s.extra_top.insert(k.clone(), v.clone());
            }
        }
        if let Some(settings) = doc.get("settings").and_then(|v| v.as_table()) {
            for (k, v) in settings {
                match rebuild_config::find(k) {
                    None => {
                        s.extra_settings.insert(k.clone(), v.clone());
                    }
                    Some(reg) if !s.model.config.problems.iter().any(|p| p.starts_with(&format!("{k}:"))) => {
                        s.model.explicit.insert(reg.key);
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(s)
    }

    /// Load a settings file; a missing file is all defaults; a corrupt one is recovered from `.bak`.
    pub fn load(path: &Path) -> Result<Store, String> {
        let main = std::fs::read_to_string(path);
        if let Ok(text) = &main {
            if let Ok(s) = Store::from_text(text) {
                return Ok(s);
            }
        }
        if let Ok(text) = std::fs::read_to_string(bak(path)) {
            if let Ok(mut s) = Store::from_text(&text) {
                s.recovered = main.is_ok();
                return Ok(s);
            }
        }
        match main {
            Err(_) => Ok(Store::new(Config::parse("")?)),
            Ok(_) => Err(format!("{} and its .bak do not parse", path.display())),
        }
    }

    /// Unknown keys kept from the file, for the UI ("kept for other versions").
    pub fn unknown_keys(&self) -> Vec<String> {
        self.extra_settings.keys().cloned().collect()
    }

    /// A top-level string the launcher keeps for itself (outside the registry), e.g. the first-run
    /// recommendation marker.
    pub fn top_str(&self, key: &str) -> Option<String> {
        self.extra_top.get(key).and_then(|v| v.as_str()).map(String::from)
    }

    pub fn set_top_str(&mut self, key: &str, value: &str) {
        self.extra_top.insert(key.into(), toml::Value::String(value.into()));
    }

    pub fn to_toml(&self) -> String {
        let mut doc: toml::Table = self.model.config.to_toml().parse().expect("Config::to_toml is TOML");
        let settings = doc.entry("settings").or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(t) = settings {
            for s in REGISTRY {
                if self.model.explicit.contains(s.key) && !t.contains_key(s.key) {
                    t.insert(
                        s.key.into(),
                        match self.model.config.get(s.key) {
                            Value::Bool(b) => toml::Value::Boolean(b),
                            Value::Int(i) => toml::Value::Integer(i),
                            Value::Choice(x) | Value::Path(x) => toml::Value::String(x),
                        },
                    );
                }
            }
            for (k, v) in &self.extra_settings {
                t.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        // the same values Config kept from the file, unless the launcher set one since (`set_top_str`)
        for (k, v) in &self.extra_top {
            doc.insert(k.clone(), v.clone());
        }
        doc.insert("schema".into(), toml::Value::Integer(self.schema));
        toml::to_string_pretty(&doc).expect("serialise")
    }

    /// Save: temp file, previous version to `.bak` (only when it parses, so a good `.bak` is never
    /// replaced by a corrupt file), then rename.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = self.to_toml();
        let back = Config::parse(&text)?;
        let real: Vec<&String> = back.problems.iter().filter(|p| !p.contains("unknown setting")).collect();
        if !real.is_empty() {
            return Err(format!("not saved: {}", real.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ")));
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        if let Ok(old) = std::fs::read_to_string(path) {
            if old.parse::<toml::Table>().is_ok() && old != text {
                std::fs::write(bak(path), &old).map_err(|e| e.to_string())?;
            }
        }
        crate::write_atomic(path, &text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Edit;

    #[test]
    fn explicit_values_survive_a_change_of_default() {
        let d = crate::temp_dir("store-explicit");
        let p = d.join("rebuild.toml");
        // a file from an older build: one value equal to today's default, one not
        std::fs::write(&p, "[settings]\n\"memory.upload_workers\" = 4\n\"display.width\" = 2560\n").unwrap();
        let s = Store::load(&p).unwrap();
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"memory.upload_workers\" = 4"), "kept although it equals the default:\n{text}");
        assert!(text.contains("\"display.width\" = 2560"));
        assert!(text.contains("schema = 1"));
        // a value the user sets to the default is kept too; reset removes it
        let mut s = Store::load(&p).unwrap();
        let vs = rebuild_config::find("display.vsync").unwrap();
        s.model.apply(vs, Edit::Toggle(false)).unwrap();
        assert!(s.to_toml().contains("\"display.vsync\" = false"));
        s.model.reset(vs);
        assert!(!s.to_toml().contains("display.vsync"));
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let d = crate::temp_dir("store-unknown");
        let p = d.join("rebuild.toml");
        std::fs::write(&p, "future_top = \"x\"\nschema = 7\n[settings]\n\"future.switch\" = true\n\"display.width\" = 1920\n[future_table]\na = 1\n").unwrap();
        let mut s = Store::load(&p).unwrap();
        assert_eq!(s.unknown_keys(), vec!["future.switch".to_string()]);
        let w = rebuild_config::find("display.width").unwrap();
        s.model.apply(w, Edit::Slide(2560)).unwrap();
        s.save(&p).unwrap();
        let back: toml::Table = std::fs::read_to_string(&p).unwrap().parse().unwrap();
        assert_eq!(back["future_top"].as_str(), Some("x"));
        assert_eq!(back["future_table"]["a"].as_integer(), Some(1));
        assert_eq!(back["settings"]["future.switch"].as_bool(), Some(true));
        assert_eq!(back["settings"]["display.width"].as_integer(), Some(2560));
        assert_eq!(back["schema"].as_integer(), Some(7), "a newer schema is never lowered");
    }

    #[test]
    fn a_crash_during_save_is_recovered_from_bak() {
        let d = crate::temp_dir("store-bak");
        let p = d.join("rebuild.toml");
        let mut s = Store::load(&p).unwrap();
        assert!(!s.recovered);
        let w = rebuild_config::find("display.width").unwrap();
        s.model.apply(w, Edit::Slide(2560)).unwrap();
        s.save(&p).unwrap();
        s.model.apply(w, Edit::Slide(1920)).unwrap();
        s.save(&p).unwrap();
        assert!(std::fs::read_to_string(bak(&p)).unwrap().contains("2560"), ".bak is the previous version");
        // a crash mid-write: the main file is cut short
        std::fs::write(&p, "[settings]\n\"display.width\" = 19").unwrap();
        std::fs::write(&p, "[settings\n\"display.wi").unwrap();
        let r = Store::load(&p).unwrap();
        assert!(r.recovered);
        assert_eq!(r.model.config.int("display.width"), 2560);
        // saving now must not replace the good .bak with the corrupt file
        r.save(&p).unwrap();
        assert!(std::fs::read_to_string(bak(&p)).unwrap().contains("2560"));
        assert_eq!(Store::load(&p).unwrap().model.config.int("display.width"), 2560);
    }
}
