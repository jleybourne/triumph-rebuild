//! C2: what the layer takes from `rebuild.toml`.
//!
//! The handoff is the file itself: the launcher starts `rebuild.exe --config <rebuild.toml>`, and the
//! layer reads it and sets, in its own process and before anything of the runtime starts, the `REBUILD_*`
//! variables the runtime already reads. One mapping ([`Config::runtime_env`]) and one rule ([`resolve`]):
//!
//! - a key the file sets explicitly wins over a variable the layer was started with;
//! - a key the file leaves at its default does not replace a variable that is already set, so
//!   `REBUILD_WARP=1 rebuild.exe --config x` still works with a file that never mentions WARP;
//! - an empty path setting leaves its variable alone (the registry's "Empty = REBUILD_*").
//!
//! Every registry key is in [`RUNTIME_KEYS`] (it reaches the runtime), or in [`NOT_RUNTIME`] with the
//! reason; a test keeps that true for every new key.

use crate::{find, Config};

/// What a variable becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    Set(String),
    /// the runtime tests the variable's presence: off removes it
    Remove,
}

/// One variable from one or two settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub key: &'static str,
    pub var: &'static str,
    pub value: EnvValue,
    /// set in the file (not a default)
    pub explicit: bool,
}

/// The keys that reach the runtime: as an environment variable ([`Config::runtime_env`]) or as an argument
/// (`dstorage.bcpack_tables`, [`Config::bcpack_tables`]). The frame rate is [`crate::frame_rate_env`].
pub const RUNTIME_KEYS: &[(&str, &str)] = &[
    ("game.dir", "REBUILD_GAME_DIR"),
    ("game.console", "REBUILD_CONSOLE"),
    ("display.vsync", "REBUILD_VSYNC"),
    ("display.frame_rate", "REBUILD_FRAME_RATE"),
    ("display.frame_cap", "REBUILD_FRAME_RATE"),
    ("upscaler.backend", "REBUILD_UPSCALER"),
    ("upscaler.dlss_dll", "REBUILD_DLSS_DLL"),
    ("upscaler.streamline_dir", "REBUILD_STREAMLINE_DIR"),
    ("upscaler.fidelityfx_dir", "REBUILD_FIDELITYFX_DIR"),
    ("dstorage.bcpack_tables", "(drives argument)"),
    ("diagnostics.frame_log", "REBUILD_FRAME_LOG"),
    ("diagnostics.frame_log_path", "REBUILD_FRAME_LOG"),
    ("diagnostics.d3d12_debug", "REBUILD_D3D_DEBUG"),
    ("diagnostics.warp", "REBUILD_WARP"),
    ("debug.offline_online_sandbox", "REBUILD_OFFLINE_SANDBOX"),
    ("limits.ram_mb", "REBUILD_LIMIT_RAM_MB"),
    ("limits.vram_mb", "REBUILD_LIMIT_VRAM_MB"),
    ("limits.cpu_cores", "REBUILD_LIMIT_CPU_CORES"),
    ("limits.cpu_rate", "REBUILD_LIMIT_CPU_RATE"),
];

/// The keys the runtime does not take, and why.
pub const NOT_RUNTIME: &[(&str, &str)] = &[
    ("title.fsr_quality", "the launcher writes it into the overlay copy of the title's settings.xml (L3)"),
    ("game.title_settings", "the launcher's source for that copy"),
    ("launcher.theme", "the launcher only"),
    ("diagnostics.retention_days", "the launcher prunes sessions"),
    ("menu.key", "the in-game menu (L5), not built"),
    ("display.width", "not read yet: the window takes the title's size"),
    ("display.height", "not read yet: the window takes the title's size"),
    ("display.window", "not read yet"),
    ("upscaler.mode", "not read yet: the upscale path is not in the runtime"),
    ("memory.upload_workers", "not read yet"),
    ("memory.labels", "not read yet"),
    ("memory.direct_read", "not read yet"),
    ("shaders.wave64", "not read yet: only doubled is built"),
    ("diagnostics.overlay", "not read yet"),
    ("diagnostics.capture_dir", "not read yet: captures go to REBUILD_CAPTURE_FRAMES / the session folder"),
];

impl Config {
    /// Whether the file sets `key` (even to its default).
    pub fn is_explicit(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    /// The `dstorage.bcpack_tables` path, when set (the drives' BCPack tables argument).
    pub fn bcpack_tables(&self) -> Option<std::path::PathBuf> {
        let p = self.get("dstorage.bcpack_tables").to_string();
        (!p.trim().is_empty()).then(|| p.into())
    }

    /// Every variable the runtime reads that a setting decides. Paths left empty are not listed.
    pub fn runtime_env(&self) -> Vec<EnvEntry> {
        let mut out = Vec::new();
        let mut put = |key: &'static str, var: &'static str, value: Option<EnvValue>, explicit: bool| {
            if let Some(value) = value {
                out.push(EnvEntry { key, var, value, explicit });
            }
        };
        let set = |s: String| Some(EnvValue::Set(s));
        let path = |key: &str| {
            let p = self.get(key).to_string();
            (!p.trim().is_empty()).then_some(EnvValue::Set(p))
        };
        let flag = |key: &str| Some(if self.bool(key) { EnvValue::Set("1".into()) } else { EnvValue::Remove });
        let ex = |key: &str| self.is_explicit(key);

        put("game.dir", "REBUILD_GAME_DIR", path("game.dir"), ex("game.dir"));
        put("game.console", "REBUILD_CONSOLE", set(self.choice("game.console")), ex("game.console"));
        put("display.vsync", "REBUILD_VSYNC", set(if self.bool("display.vsync") { "1" } else { "0" }.into()), ex("display.vsync"));
        let rate_ex = ex("display.frame_rate") || ex("display.frame_cap");
        put("display.frame_rate", "REBUILD_FRAME_RATE", set(crate::frame_rate_env(self)), rate_ex);
        put("upscaler.backend", "REBUILD_UPSCALER", set(self.choice("upscaler.backend")), ex("upscaler.backend"));
        for (key, var) in [
            ("upscaler.dlss_dll", "REBUILD_DLSS_DLL"),
            ("upscaler.streamline_dir", "REBUILD_STREAMLINE_DIR"),
            ("upscaler.fidelityfx_dir", "REBUILD_FIDELITYFX_DIR"),
        ] {
            put(key, var, path(key), ex(key));
        }
        // the frame line: off removes the log; a path set here replaces the launcher's session log
        if !self.bool("diagnostics.frame_log") {
            put("diagnostics.frame_log", "REBUILD_FRAME_LOG", Some(EnvValue::Remove), ex("diagnostics.frame_log"));
        } else {
            put("diagnostics.frame_log_path", "REBUILD_FRAME_LOG", path("diagnostics.frame_log_path"), ex("diagnostics.frame_log_path"));
        }
        put("diagnostics.d3d12_debug", "REBUILD_D3D_DEBUG", flag("diagnostics.d3d12_debug"), ex("diagnostics.d3d12_debug"));
        put("diagnostics.warp", "REBUILD_WARP", flag("diagnostics.warp"), ex("diagnostics.warp"));
        put("debug.offline_online_sandbox", "REBUILD_OFFLINE_SANDBOX", flag("debug.offline_online_sandbox"), ex("debug.offline_online_sandbox"));
        for (key, var) in [
            ("limits.ram_mb", "REBUILD_LIMIT_RAM_MB"),
            ("limits.vram_mb", "REBUILD_LIMIT_VRAM_MB"),
            ("limits.cpu_cores", "REBUILD_LIMIT_CPU_CORES"),
            ("limits.cpu_rate", "REBUILD_LIMIT_CPU_RATE"),
        ] {
            put(key, var, set(self.int(key).to_string()), ex(key));
        }
        out
    }
}

/// The changes to make to the environment: `(variable, Some(value))` to set, `(variable, None)` to remove.
/// `current` reads the environment as it is. Nothing is listed that would not change it.
pub fn resolve(entries: &[EnvEntry], current: impl Fn(&str) -> Option<String>) -> Vec<(&'static str, Option<String>)> {
    let mut out = Vec::new();
    for e in entries {
        let now = current(e.var);
        if !e.explicit && now.is_some() {
            continue; // a default never replaces a variable the layer was started with
        }
        match &e.value {
            EnvValue::Set(v) if now.as_deref() != Some(v.as_str()) => out.push((e.var, Some(v.clone()))),
            EnvValue::Remove if now.is_some() => out.push((e.var, None)),
            _ => {}
        }
    }
    out
}

/// A setting's value as one line for the layer's log.
pub fn describe(e: &EnvEntry) -> String {
    match &e.value {
        EnvValue::Set(v) => format!("{}={v} ({})", e.var, e.key),
        EnvValue::Remove => format!("{} removed ({})", e.var, e.key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, REGISTRY};
    use std::collections::HashMap;

    fn env_of(c: &Config) -> HashMap<&'static str, EnvValue> {
        c.runtime_env().into_iter().map(|e| (e.var, e.value)).collect()
    }

    #[test]
    fn every_registry_key_is_classified() {
        for s in REGISTRY {
            let r = RUNTIME_KEYS.iter().any(|(k, _)| *k == s.key);
            let n = NOT_RUNTIME.iter().any(|(k, _)| *k == s.key);
            assert!(r ^ n, "{}: in exactly one of RUNTIME_KEYS / NOT_RUNTIME", s.key);
        }
        for (k, _) in NOT_RUNTIME {
            assert!(find(k).is_some(), "{k} is not in the registry");
        }
        for (k, _) in RUNTIME_KEYS {
            assert!(find(k).is_some(), "{k} is not in the registry");
        }
    }

    #[test]
    fn defaults_map_to_what_the_runtime_reads() {
        let c = Config::parse("").unwrap();
        let e = env_of(&c);
        assert_eq!(e.get("REBUILD_CONSOLE"), Some(&EnvValue::Set("series-x".into())));
        assert_eq!(e.get("REBUILD_VSYNC"), Some(&EnvValue::Set("0".into())));
        assert_eq!(e.get("REBUILD_UPSCALER"), Some(&EnvValue::Set("auto".into())));
        assert_eq!(e.get("REBUILD_WARP"), Some(&EnvValue::Remove));
        assert_eq!(e.get("REBUILD_D3D_DEBUG"), Some(&EnvValue::Remove));
        assert_eq!(e.get("REBUILD_LIMIT_RAM_MB"), Some(&EnvValue::Set("0".into())));
        // empty paths: the variable the layer was started with stays
        for v in ["REBUILD_GAME_DIR", "REBUILD_DLSS_DLL", "REBUILD_STREAMLINE_DIR", "REBUILD_FIDELITYFX_DIR", "REBUILD_FRAME_LOG"] {
            assert!(!e.contains_key(v), "{v}");
        }
        assert!(c.runtime_env().iter().all(|x| !x.explicit));
        assert_eq!(c.bcpack_tables(), None);
    }

    #[test]
    fn user_settings_map_to_their_variables() {
        let c = Config::parse(
            r#"
            [settings]
            "game.dir" = 'D:\games\title'
            "game.console" = "series-s"
            "display.vsync" = true
            "display.frame_rate" = "capped"
            "display.frame_cap" = 60
            "upscaler.backend" = "fsr"
            "upscaler.fidelityfx_dir" = 'C:\ffx'
            "dstorage.bcpack_tables" = 'C:\t\bcpack.bin'
            "diagnostics.frame_log_path" = 'C:\logs\f.log'
            "diagnostics.d3d12_debug" = true
            "diagnostics.warp" = true
            "limits.ram_mb" = 8192
            "limits.cpu_cores" = 7
            "#,
        )
        .unwrap();
        assert!(c.problems.is_empty(), "{:#?}", c.problems);
        let e = env_of(&c);
        let set = |v: &str| Some(EnvValue::Set(v.into()));
        assert_eq!(e.get("REBUILD_GAME_DIR").cloned(), set(r"D:\games\title"));
        assert_eq!(e.get("REBUILD_CONSOLE").cloned(), set("series-s"));
        assert_eq!(e.get("REBUILD_VSYNC").cloned(), set("1"));
        assert_eq!(e.get("REBUILD_UPSCALER").cloned(), set("fsr"));
        assert_eq!(e.get("REBUILD_FIDELITYFX_DIR").cloned(), set(r"C:\ffx"));
        assert_eq!(e.get("REBUILD_FRAME_LOG").cloned(), set(r"C:\logs\f.log"));
        assert_eq!(e.get("REBUILD_D3D_DEBUG").cloned(), set("1"));
        assert_eq!(e.get("REBUILD_WARP").cloned(), set("1"));
        assert_eq!(e.get("REBUILD_LIMIT_RAM_MB").cloned(), set("8192"));
        assert_eq!(e.get("REBUILD_LIMIT_CPU_CORES").cloned(), set("7"));
        assert_eq!(c.bcpack_tables(), Some(r"C:\t\bcpack.bin".into()));
        assert_eq!(e.get("REBUILD_FRAME_RATE").cloned(), set("60"));
        // the frame log off removes the variable, whatever the path
        let off = Config::parse("[settings]\n\"diagnostics.frame_log\" = false\n\"diagnostics.frame_log_path\" = 'C:\\x.log'").unwrap();
        assert_eq!(env_of(&off).get("REBUILD_FRAME_LOG"), Some(&EnvValue::Remove));
    }

    #[test]
    fn the_frame_rate_is_frame_rate_env_and_either_key_makes_it_explicit() {
        let rate = |c: &Config| c.runtime_env().into_iter().find(|e| e.var == "REBUILD_FRAME_RATE").unwrap();
        let d = Config::parse("").unwrap();
        assert_eq!(rate(&d).value, EnvValue::Set("title".into()));
        assert!(!rate(&d).explicit);
        for (mode, want) in [("title", "title"), ("unlimited", "unlimited"), ("capped", "144")] {
            let mut c = d.clone();
            c.set("display.frame_cap", Value::Int(144)).unwrap();
            c.set("display.frame_rate", Value::Choice(mode.into())).unwrap();
            assert_eq!(rate(&c).value, EnvValue::Set(want.into()), "{mode}");
            assert_eq!(crate::frame_rate_env(&c), want);
        }
        let mut cap_only = d.clone();
        cap_only.set("display.frame_cap", Value::Int(90)).unwrap();
        assert!(rate(&cap_only).explicit, "the cap alone counts as set");
    }

    #[test]
    fn explicit_settings_win_and_defaults_leave_the_environment() {
        let c = Config::parse("[settings]\n\"display.vsync\" = false\n\"limits.ram_mb\" = 4096").unwrap();
        let started: HashMap<&str, &str> = [("REBUILD_WARP", "1"), ("REBUILD_VSYNC", "1"), ("REBUILD_LIMIT_RAM_MB", "9"), ("REBUILD_CONSOLE", "series-x")].into();
        let changes = resolve(&c.runtime_env(), |v| started.get(v).map(|s| s.to_string()));
        let m: HashMap<_, _> = changes.into_iter().collect();
        assert!(!m.contains_key("REBUILD_WARP"), "WARP from the environment stays: the file leaves it at its default");
        assert_eq!(m.get("REBUILD_VSYNC"), Some(&Some("0".to_string())), "the file sets vsync: it wins");
        assert_eq!(m.get("REBUILD_LIMIT_RAM_MB"), Some(&Some("4096".to_string())));
        assert!(!m.contains_key("REBUILD_CONSOLE"), "already the value: no change");
        assert_eq!(m.get("REBUILD_UPSCALER"), Some(&Some("auto".to_string())), "unset: the default is set");
        // an explicit off removes a variable that is present
        let off = Config::parse("[settings]\n\"diagnostics.warp\" = false").unwrap();
        let m: HashMap<_, _> = resolve(&off.runtime_env(), |v| started.get(v).map(|s| s.to_string())).into_iter().collect();
        assert_eq!(m.get("REBUILD_WARP"), Some(&None));
    }
}
