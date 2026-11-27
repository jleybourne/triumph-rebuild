//! Every option the layer has, declared once (`docs/design/launcher.md`).
//!
//! The [`REGISTRY`] is the single source: the desktop launcher builds its widgets from it, the layer
//! validates `rebuild.toml` against it at start, and the in-game menu lists its live entries. An
//! unknown key is reported, a bad value is refused with its reason, and an absent key takes its default,
//! which is always the measured best.

use std::collections::BTreeMap;
use std::fmt;

pub mod profiles;
pub use profiles::{builtin, Builtin, BUILTIN_PROFILES};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Choice(String),
    Path(String),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Choice(s) | Value::Path(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int { min: i64, max: i64 },
    /// one of these
    Choice { options: &'static [&'static str] },
    Path,
}

/// When a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// at once, from the in-game menu too
    Live,
    /// on the next launch
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Stable,
    /// shown with a warning; not allowed in a profile marked stable
    Experimental,
}

#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub default: Default,
    pub apply: Apply,
    pub tier: Tier,
    /// values refused with a reason (known to break something)
    pub refused: &'static [(Default, &'static str)],
}

/// A default, as a constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Default {
    Bool(bool),
    Int(i64),
    Choice(&'static str),
    Path(&'static str),
}

impl Default {
    pub fn value(self) -> Value {
        match self {
            Default::Bool(b) => Value::Bool(b),
            Default::Int(i) => Value::Int(i),
            Default::Choice(s) => Value::Choice(s.into()),
            Default::Path(s) => Value::Path(s.into()),
        }
    }
}

impl Setting {
    pub fn group(&self) -> &'static str {
        self.key.split('.').next().unwrap_or("")
    }

    /// Check a value against this setting; the reason when refused.
    pub fn check(&self, v: &Value) -> Result<(), String> {
        match (self.kind, v) {
            (Kind::Bool, Value::Bool(_)) | (Kind::Path, Value::Path(_)) => {}
            (Kind::Int { min, max }, Value::Int(i)) => {
                if *i < min || *i > max {
                    return Err(format!("{}: {i} is outside {min}..={max}", self.key));
                }
            }
            (Kind::Choice { options }, Value::Choice(c)) => {
                if !options.contains(&c.as_str()) {
                    return Err(format!("{}: \"{c}\" is not one of {options:?}", self.key));
                }
            }
            _ => return Err(format!("{}: expected {:?}, got {v:?}", self.key, self.kind)),
        }
        for (bad, why) in self.refused {
            if bad.value() == *v {
                return Err(format!("{}: {v} is refused: {why}", self.key));
            }
        }
        Ok(())
    }
}

const fn s(
    key: &'static str,
    label: &'static str,
    help: &'static str,
    kind: Kind,
    default: Default,
    apply: Apply,
    tier: Tier,
) -> Setting {
    Setting { key, label, help, kind, default, apply, tier, refused: &[] }
}

use Apply::*;
use Tier::*;

/// Every option. Add one only with its measured effect; its default is the measured best.
pub static REGISTRY: &[Setting] = &[
    s("game.dir", "Game folder", "The title's folder. Read-only: nothing is ever written into it.", Kind::Path, Default::Path(""), Restart, Stable),
    s("display.width", "Output width", "The size of the window or screen the frame is presented at.", Kind::Int { min: 640, max: 7680 }, Default::Int(3840), Restart, Stable),
    s("display.height", "Output height", "The height of the window or screen the frame is presented at.", Kind::Int { min: 360, max: 4320 }, Default::Int(2160), Restart, Stable),
    s("display.window", "Window mode", "borderless = a window the size of the screen (fastest to switch away from); fullscreen = exclusive.", Kind::Choice { options: &["windowed", "borderless", "fullscreen"] }, Default::Choice("borderless"), Restart, Stable),
    s("display.vsync", "Vsync", "Wait for the display's refresh before presenting.", Kind::Bool, Default::Bool(false), Live, Stable),
    s("display.frame_cap", "Frame cap", "0 = no cap. The title also caps itself at 30 in menus.", Kind::Int { min: 0, max: 360 }, Default::Int(0), Live, Stable),
    s("upscaler.backend", "Upscaler", "auto = DLSS on NVIDIA, FSR on AMD (Build 7). native = the title's own pass.", Kind::Choice { options: &["auto", "dlss", "fsr", "native"] }, Default::Choice("auto"), Restart, Stable),
    s("upscaler.mode", "Upscaler mode", "How far the internal resolution drops before upscaling.", Kind::Choice { options: &["dlaa", "quality", "balanced", "performance", "ultra_performance"] }, Default::Choice("quality"), Restart, Stable),
    Setting {
        refused: &[(Default::Int(4), "the title's FSR path gave a black screen at level 4 (fork, docs/settings.md)")],
        ..s("title.fsr_quality", "Title render scale (fsrQuality)", "Written to a copy of the title's settings.xml in the write overlay at launch; the original is never touched.", Kind::Int { min: 0, max: 4 }, Default::Int(3), Restart, Stable)
    },
    s("memory.upload_workers", "Upload workers", "Threads that diff and gather the title's CPU writes (Build 2: 4 workers land 34 MB in 19 ms).", Kind::Int { min: 1, max: 8 }, Default::Int(4), Restart, Stable),
    s("memory.labels", "GPU→CPU labels", "gpu = written by the GPU into title memory (65 µs before the fence); relay = a CPU worker (17 µs after).", Kind::Choice { options: &["gpu", "relay"] }, Default::Choice("gpu"), Restart, Stable),
    s("memory.direct_read", "Direct read of title memory", "Let the GPU read rarely-written title memory over PCIe instead of uploading it. Unmeasured: Build 5.", Kind::Bool, Default::Bool(false), Restart, Experimental),
    s("shaders.wave64", "Wave64 programs", "doubled = one host lane carries two console lanes (exact, proven); split = two host waves.", Kind::Choice { options: &["doubled", "split"] }, Default::Choice("doubled"), Restart, Stable),
    s("diagnostics.overlay", "Frame line overlay", "fps · ms · draws · dropped · upload · translation · resolution · upscaler, drawn on the output.", Kind::Bool, Default::Bool(false), Live, Stable),
    s("diagnostics.frame_log", "Frame line log", "Append the frame line once a second to a file the launcher reads.", Kind::Bool, Default::Bool(true), Live, Stable),
    s("diagnostics.capture_dir", "Capture folder", "Where frame captures go. Empty = off.", Kind::Path, Default::Path(""), Live, Stable),
    s("diagnostics.d3d12_debug", "D3D12 debug layer", "Names why a pipeline or command is refused. Slow: never for measuring.", Kind::Bool, Default::Bool(false), Restart, Experimental),
    // The "Empty = REBUILD_*" fallbacks below describe the layer once it reads this file (C2). Today the layer
    // reads no setting, and only `upscaletest` reads REBUILD_DLSS_DLL / _STREAMLINE_DIR / _FIDELITYFX_DIR.
    s("upscaler.dlss_dll", "DLSS DLL", "nvngx_dlss.dll, supplied by you; never shipped or committed. Empty = REBUILD_DLSS_DLL.", Kind::Path, Default::Path(""), Restart, Stable),
    s("upscaler.streamline_dir", "Streamline folder", "NVIDIA Streamline's runtime DLLs. Empty = REBUILD_STREAMLINE_DIR.", Kind::Path, Default::Path(""), Restart, Stable),
    s("upscaler.fidelityfx_dir", "FidelityFX folder", "AMD FidelityFX SDK runtime DLLs (FSR). Empty = REBUILD_FIDELITYFX_DIR.", Kind::Path, Default::Path(""), Restart, Stable),
    s("dstorage.bcpack_tables", "BCPack tables", "The ~78 KB BCPack format tables, a file kept outside the repository. Empty = REBUILD_BCPACK_TABLES; without it, BCPack textures are refused.", Kind::Path, Default::Path(""), Restart, Stable),
    s("game.title_settings", "Title settings file", "The title's own settings.xml (it lives in the title's storage, not the game folder). Empty = search.", Kind::Path, Default::Path(""), Restart, Stable),
    s("diagnostics.frame_log_path", "Frame line log file", "Where the frame line log is written. Empty = REBUILD_FRAME_LOG, else frame-line.log in the launcher's folder.", Kind::Path, Default::Path(""), Live, Stable),
    s("limits.ram_mb", "RAM limit (MiB)", "Test a smaller machine: the title and the layer together stay under this. 0 = no limit (the title's own budget still applies).", Kind::Int { min: 0, max: 1 << 20 }, Default::Int(0), Restart, Stable),
    s("limits.vram_mb", "VRAM limit (MiB)", "Test a smaller graphics card: our GPU heaps stay under this. 0 = no limit. Enforced from Build 5.", Kind::Int { min: 0, max: 1 << 18 }, Default::Int(0), Restart, Experimental),
    s("limits.cpu_cores", "CPU core limit", "Run on only the first N logical processors. 0 = all.", Kind::Int { min: 0, max: 1024 }, Default::Int(0), Restart, Stable),
    s("limits.cpu_rate", "CPU usage cap (%)", "A hard cap on the share of the whole CPU the process may use. 0 = no cap.", Kind::Int { min: 0, max: 100 }, Default::Int(0), Restart, Stable),
    s("diagnostics.retention_days", "Keep sessions for (days)", "Each launch gets a session folder (frame line, summary, crash reports, captures). Folders older than this are deleted when the launcher starts and after each run. 0 = keep forever.", Kind::Int { min: 0, max: 3650 }, Default::Int(14), Live, Stable),
    s("launcher.theme", "Launcher theme", "dark or light, for the launcher only (the title is not affected). Also switched from the top bar.", Kind::Choice { options: &["dark", "light"] }, Default::Choice("dark"), Live, Stable),
    s("menu.key", "In-game menu key", "Toggles the in-game menu. While it is open the title sees no input.", Kind::Choice { options: &["F10", "F11", "F12", "Insert", "Home"] }, Default::Choice("F10"), Restart, Stable),
];

/// An `upscaler.mode` choice in the upscaler's spelling (see [`Config::upscaler_mode`]).
pub fn upscaler_mode_name(choice: &str) -> String {
    match choice {
        "dlaa" => "native-aa".into(),
        c => c.replace('_', "-"),
    }
}

pub fn find(key: &str) -> Option<&'static Setting> {
    REGISTRY.iter().find(|s| s.key == key)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mod {
    pub name: String,
    pub enabled: bool,
}

/// The `schema` this code writes. A file from a newer schema loads; whatever this code doesn't know is kept.
pub const SCHEMA: i64 = 1;

/// A loaded `rebuild.toml`: the values set, the mods in load order, and what was wrong with the file.
///
/// **A user's settings are never replaced** (owner rule): every value set explicitly is kept and written back
/// even when it equals today's default (so a later change of default never moves it), and every entry this
/// code can't use (an unknown key, a wrong type, a refused value, an unknown top-level key) is kept as written
/// and written back unchanged.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    /// The file's schema: never lowered on save, so a newer version's file stays marked as newer.
    pub schema: i64,
    pub profile: String,
    pub stable_only: bool,
    values: BTreeMap<&'static str, Value>,
    pub mods: Vec<Mod>,
    /// unknown keys and refused values found while loading (each kept out of `values`)
    pub problems: Vec<String>,
    /// `[settings]` entries not applied (unknown, wrong type, refused), exactly as written
    kept: toml::Table,
    /// top-level keys this code doesn't know, exactly as written
    kept_top: toml::Table,
}

impl Config {
    /// The effective value of a key: what the file set, else the default.
    pub fn get(&self, key: &str) -> Value {
        let s = find(key).unwrap_or_else(|| panic!("{key} is not in the registry"));
        self.values.get(s.key).cloned().unwrap_or_else(|| s.default.value())
    }

    pub fn int(&self, key: &str) -> i64 {
        match self.get(key) {
            Value::Int(i) => i,
            v => panic!("{key} is {v:?}, not an int"),
        }
    }

    pub fn bool(&self, key: &str) -> bool {
        matches!(self.get(key), Value::Bool(true))
    }

    pub fn choice(&self, key: &str) -> String {
        self.get(key).to_string()
    }

    /// `upscaler.mode` in the upscaler's own spelling (`rebuild_upscale::Mode::parse`): `dlaa` is `native-aa`,
    /// `ultra_performance` is `ultra-performance`.
    pub fn upscaler_mode(&self) -> String {
        upscaler_mode_name(&self.choice("upscaler.mode"))
    }

    /// Set a value, checked against the registry.
    pub fn set(&mut self, key: &str, v: Value) -> Result<(), String> {
        let s = find(key).ok_or_else(|| format!("{key}: unknown setting"))?;
        s.check(&v)?;
        if self.stable_only && s.tier == Tier::Experimental && v != s.default.value() {
            return Err(format!("{key} is experimental; this profile is stable-only"));
        }
        // explicit, even when equal to today's default: a later change of default never moves it
        self.values.insert(s.key, v);
        self.kept.remove(s.key);
        Ok(())
    }

    /// Back to the default: the key is removed from the file, so it follows the default from now on.
    pub fn reset(&mut self, key: &str) {
        if let Some(s) = find(key) {
            self.values.remove(s.key);
        }
        self.kept.remove(key);
    }

    /// The values that differ from their defaults (what the launcher marks as changed).
    pub fn changed(&self) -> impl Iterator<Item = (&'static str, &Value)> {
        self.values.iter().filter(|(k, v)| find(k).is_some_and(|s| s.default.value() != **v)).map(|(k, v)| (*k, v))
    }

    /// Every value set explicitly, including those equal to the default.
    pub fn explicit(&self) -> impl Iterator<Item = (&'static str, &Value)> {
        self.values.iter().map(|(k, v)| (*k, v))
    }

    /// `[settings]` entries kept as written because this code couldn't use them.
    pub fn kept(&self) -> &toml::Table {
        &self.kept
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("rebuild.toml: {e}"))?;
        let mut c = Config {
            profile: doc.get("profile").and_then(|v| v.as_str()).unwrap_or("default").to_string(),
            stable_only: doc.get("stable_only").and_then(|v| v.as_bool()).unwrap_or(false),
            ..Config::default()
        };
        c.schema = doc.get("schema").and_then(|v| v.as_integer()).unwrap_or(SCHEMA).max(SCHEMA);
        for (k, v) in &doc {
            if !matches!(k.as_str(), "schema" | "profile" | "stable_only" | "settings" | "mods") {
                c.kept_top.insert(k.clone(), v.clone());
            }
        }
        if let Some(settings) = doc.get("settings").and_then(|v| v.as_table()) {
            for (k, v) in settings {
                let Some(s) = find(k) else {
                    c.problems.push(format!("{k}: unknown setting (kept in the file, not used)"));
                    c.kept.insert(k.clone(), v.clone());
                    continue;
                };
                let value = match (s.kind, v) {
                    (Kind::Bool, toml::Value::Boolean(b)) => Value::Bool(*b),
                    (Kind::Int { .. }, toml::Value::Integer(i)) => Value::Int(*i),
                    (Kind::Choice { .. }, toml::Value::String(x)) => Value::Choice(x.clone()),
                    (Kind::Path, toml::Value::String(x)) => Value::Path(x.clone()),
                    _ => {
                        c.problems.push(format!("{k}: wrong type {v} (default used; the value is kept in the file)"));
                        c.kept.insert(k.clone(), v.clone());
                        continue;
                    }
                };
                if let Err(e) = c.set(k, value) {
                    c.problems.push(format!("{e} (default used; the value is kept in the file)"));
                    c.kept.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(mods) = doc.get("mods").and_then(|v| v.as_array()) {
            for m in mods {
                match (m.get("name").and_then(|v| v.as_str()), m.get("enabled").and_then(|v| v.as_bool())) {
                    (Some(n), e) => c.mods.push(Mod { name: n.into(), enabled: e.unwrap_or(true) }),
                    _ => c.problems.push("a [[mods]] entry has no name (ignored)".into()),
                }
            }
        }
        Ok(c)
    }

    pub fn to_toml(&self) -> String {
        let mut doc = self.kept_top.clone();
        doc.insert("schema".into(), toml::Value::Integer(self.schema.max(SCHEMA)));
        doc.insert("profile".into(), toml::Value::String(self.profile.clone()));
        if self.stable_only {
            doc.insert("stable_only".into(), toml::Value::Boolean(true));
        }
        let mut settings = self.kept.clone();
        for (k, v) in &self.values {
            settings.insert(
                (*k).into(),
                match v {
                    Value::Bool(b) => toml::Value::Boolean(*b),
                    Value::Int(i) => toml::Value::Integer(*i),
                    Value::Choice(s) | Value::Path(s) => toml::Value::String(s.clone()),
                },
            );
        }
        doc.insert("settings".into(), toml::Value::Table(settings));
        let mods: Vec<toml::Value> = self
            .mods
            .iter()
            .map(|m| {
                let mut t = toml::Table::new();
                t.insert("name".into(), toml::Value::String(m.name.clone()));
                t.insert("enabled".into(), toml::Value::Boolean(m.enabled));
                toml::Value::Table(t)
            })
            .collect();
        if !mods.is_empty() {
            doc.insert("mods".into(), toml::Value::Array(mods));
        }
        toml::to_string_pretty(&doc).expect("serialise rebuild.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_is_valid_and_every_key_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for s in REGISTRY {
            assert!(seen.insert(s.key), "{} twice", s.key);
            assert!(s.key.contains('.'), "{} has no group", s.key);
            s.check(&s.default.value()).unwrap_or_else(|e| panic!("default of {}: {e}", s.key));
        }
    }

    #[test]
    fn every_upscaler_mode_choice_parses_in_the_upscaler() {
        let Kind::Choice { options } = find("upscaler.mode").unwrap().kind else { panic!("upscaler.mode is a choice") };
        for o in options {
            let name = upscaler_mode_name(o);
            assert!(rebuild_upscale::Mode::parse(&name).is_some(), "{o} -> {name}");
        }
        let mut c = Config::parse("").unwrap();
        c.set("upscaler.mode", Value::Choice("dlaa".into())).unwrap();
        assert_eq!(c.upscaler_mode(), "native-aa");
    }

    #[test]
    fn every_menu_key_choice_names_a_key_the_input_takes() {
        let Kind::Choice { options } = find("menu.key").unwrap().kind else { panic!("menu.key is a choice") };
        for o in options {
            assert!(rebuild_input::menu_key_named(o).is_some(), "menu.key {o}");
        }
        let c = Config::parse("").unwrap();
        assert_eq!(rebuild_input::menu_key_named(&c.choice("menu.key")), Some(rebuild_input::MENU_KEY));
    }

    #[test]
    fn retention_and_theme_have_their_defaults_and_ranges() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.int("diagnostics.retention_days"), 14);
        assert_eq!(c.choice("launcher.theme"), "dark");
        let mut c = Config::parse(
            r#"
            [settings]
            "launcher.theme" = "light"
            "diagnostics.retention_days" = 0
            "#,
        )
        .unwrap();
        assert!(c.problems.is_empty(), "{:#?}", c.problems);
        assert_eq!(c.choice("launcher.theme"), "light");
        assert_eq!(c.int("diagnostics.retention_days"), 0, "0 = keep forever is allowed");
        assert!(c.set("launcher.theme", Value::Choice("blue".into())).is_err());
        assert!(c.set("diagnostics.retention_days", Value::Int(-1)).is_err());
        let back = Config::parse(&c.to_toml()).unwrap();
        assert_eq!(back.choice("launcher.theme"), "light");
    }

    #[test]
    fn an_empty_file_is_all_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.int("memory.upload_workers"), 4);
        assert_eq!(c.choice("upscaler.backend"), "auto");
        assert!(c.problems.is_empty());
    }

    #[test]
    fn a_file_round_trips() {
        let mut c = Config::parse("").unwrap();
        c.profile = "testing".into();
        c.set("display.width", Value::Int(2560)).unwrap();
        c.set("memory.labels", Value::Choice("relay".into())).unwrap();
        c.set("game.dir", Value::Path(r"D:\games\title".into())).unwrap();
        c.mods.push(Mod { name: "hud".into(), enabled: false });
        c.mods.push(Mod { name: "fov".into(), enabled: true });
        let back = Config::parse(&c.to_toml()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn bad_values_are_refused_with_a_reason_and_unknown_keys_reported() {
        let c = Config::parse(
            r#"
            [settings]
            "title.fsr_quality" = 4
            "memory.upload_workers" = 99
            "upscaler.backend" = "xess"
            "display.vsync" = "yes"
            "memory.no_such_thing" = 1
            "#,
        )
        .unwrap();
        assert_eq!(c.problems.len(), 5, "{:#?}", c.problems);
        assert!(c.problems.iter().any(|p| p.contains("black screen")));
        assert!(c.problems.iter().any(|p| p.contains("unknown setting")));
        // every refused key kept its default
        assert_eq!(c.int("title.fsr_quality"), 3);
        assert_eq!(c.int("memory.upload_workers"), 4);
    }

    #[test]
    fn a_stable_profile_refuses_experimental_switches() {
        let mut c = Config::parse("stable_only = true").unwrap();
        assert!(c.set("memory.direct_read", Value::Bool(true)).is_err());
        assert!(c.set("display.vsync", Value::Bool(true)).is_ok());
    }

    #[test]
    fn an_explicit_default_is_kept_so_a_new_default_never_moves_it() {
        let mut c = Config::parse("").unwrap();
        c.set("display.width", Value::Int(3840)).unwrap();
        assert_eq!(c.changed().count(), 0, "equal to the default: not marked changed");
        assert_eq!(c.explicit().count(), 1, "but kept");
        let back = Config::parse(&c.to_toml()).unwrap();
        assert_eq!(back.explicit().collect::<Vec<_>>(), vec![("display.width", &Value::Int(3840))]);
        c.reset("display.width");
        assert_eq!(c.explicit().count(), 0, "reset follows the default again");
    }

    #[test]
    fn unknown_wrong_and_refused_entries_survive_a_save_untouched() {
        let text = r#"
            schema = 1
            launcher_window = { x = 10, y = 20 }
            [settings]
            "display.width" = 2560
            "future.thing" = "on"
            "display.vsync" = "yes"
            "title.fsr_quality" = 4
        "#;
        let c = Config::parse(text).unwrap();
        assert_eq!(c.int("display.width"), 2560);
        assert_eq!(c.int("title.fsr_quality"), 3, "refused: the default is used");
        assert_eq!(c.problems.len(), 3, "{:#?}", c.problems);
        let saved = c.to_toml();
        let doc: toml::Table = saved.parse().unwrap();
        assert_eq!(doc["schema"].as_integer(), Some(SCHEMA));
        assert_eq!(doc["launcher_window"]["y"].as_integer(), Some(20), "an unknown top-level key is kept");
        let s = doc["settings"].as_table().unwrap();
        assert_eq!(s["future.thing"].as_str(), Some("on"));
        assert_eq!(s["display.vsync"].as_str(), Some("yes"), "a wrong type is kept as written");
        assert_eq!(s["title.fsr_quality"].as_integer(), Some(4), "a refused value is kept as written");
        // setting a kept key properly replaces the kept entry
        let mut c2 = Config::parse(&saved).unwrap();
        c2.set("display.vsync", Value::Bool(true)).unwrap();
        let s2: toml::Table = c2.to_toml().parse().unwrap();
        assert_eq!(s2["settings"]["display.vsync"].as_bool(), Some(true));
        // a newer schema loads, and is never lowered on save
        let newer = Config::parse("schema = 9").unwrap();
        assert_eq!(newer.schema, 9);
        assert!(newer.to_toml().contains("schema = 9"));
    }
}
