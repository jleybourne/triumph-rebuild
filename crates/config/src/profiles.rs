//! Built-in test profiles (owner request, 2026-11-27): named sets of registry values for testing the same
//! build under different machines, baked into the config so the launcher, the layer and a test matrix
//! all mean the same thing by a name. The values and the reason for each are in
//! `docs/design/test-profiles.md`.
//!
//! - **Read-only.** A built-in is code, never a file: it can't be saved over. Applying one sets its keys
//!   (and only its keys) in a [`Config`]; editing afterwards is the user's copy, saved under another name.
//! - **Complete over [`PROFILE_KEYS`].** Every built-in sets every profile key, so applying one after
//!   another leaves nothing of the first behind, and nothing outside those keys is ever touched.
//! - **Validated against the registry** at test time: every key exists, every value passes
//!   [`crate::Setting::check`].

use crate::{find, Config, Default, Tier, Value};

/// The keys a built-in profile sets: output, upscaler and the hardware limits. Nothing else.
pub const PROFILE_KEYS: &[&str] = &[
    "display.width",
    "display.height",
    "upscaler.backend",
    "upscaler.mode",
    "limits.ram_mb",
    "limits.vram_mb",
    "limits.cpu_cores",
    "limits.cpu_rate",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Builtin {
    /// the id, used as `profile` in `rebuild.toml` and as the matrix file name
    pub name: &'static str,
    pub label: &'static str,
    /// one line: what a run under this profile tells you
    pub purpose: &'static str,
    pub settings: &'static [(&'static str, Default)],
}

const fn p(
    name: &'static str,
    label: &'static str,
    purpose: &'static str,
    settings: &'static [(&'static str, Default)],
) -> Builtin {
    Builtin { name, label, purpose, settings }
}

use Default::{Choice as C, Int as I};

/// Shorthand for a profile's eight values, in [`PROFILE_KEYS`] order.
macro_rules! keys {
    ($w:expr, $h:expr, $backend:expr, $mode:expr, $ram:expr, $vram:expr, $cores:expr, $rate:expr) => {
        &[
            ("display.width", I($w)),
            ("display.height", I($h)),
            ("upscaler.backend", C($backend)),
            ("upscaler.mode", C($mode)),
            ("limits.ram_mb", I($ram)),
            ("limits.vram_mb", I($vram)),
            ("limits.cpu_cores", I($cores)),
            ("limits.cpu_rate", I($rate)),
        ]
    };
}

/// Every built-in. Order is the order the launcher lists them: the reference first, then machine
/// classes from the console down, then the one-cause stress profiles.
pub static BUILTIN_PROFILES: &[Builtin] = &[
    p(
        "dev-machine",
        "Dev machine (minimum spec)",
        "The reference: the minimum spec (RTX 4070 Ti, 4K, DLSS Quality) with no limits. Every other profile is read against it.",
        keys!(3840, 2160, "auto", "quality", 0, 0, 0, 0),
    ),
    p(
        "series-x",
        "Console class (Series X budget)",
        "The title's own home: 13.5 GiB title memory, 10 GiB GPU-optimal memory, 7 title cores (14 threads), 4K with a quality upscale.",
        keys!(3840, 2160, "auto", "quality", 13_824, 10_240, 14, 0),
    ),
    p(
        "series-s",
        "Console class (Series S budget)",
        "The small console: 8 GiB title memory, about 6 GiB of it for the GPU, the same 7 cores, 1440p with a performance upscale.",
        keys!(2560, 1440, "auto", "performance", 8_192, 6_144, 14, 0),
    ),
    p(
        "mid-pc",
        "Mid-range PC",
        "A 16 GiB, 6-core PC with an 8 GiB card at 1440p: about 12 GiB is left for the game after Windows.",
        keys!(2560, 1440, "auto", "quality", 12_288, 8_192, 12, 0),
    ),
    p(
        "low-pc",
        "Low-end PC",
        "A 16 GiB, 4-core PC with a 6 GiB card at 1080p, FSR Performance (the vendor-neutral path).",
        keys!(1920, 1080, "fsr", "performance", 12_288, 6_144, 8, 0),
    ),
    p(
        "stress-ram",
        "Stress: memory-starved",
        "Only RAM is limited (8 GiB, the Series S title budget); everything else is the dev machine. Blames a failure on memory alone.",
        keys!(3840, 2160, "auto", "quality", 8_192, 0, 0, 0),
    ),
    p(
        "stress-vram",
        "Stress: VRAM-starved",
        "Only VRAM is limited (4 GiB); everything else is the dev machine. Blames a failure on GPU memory alone (enforced from Build 5).",
        keys!(3840, 2160, "auto", "quality", 0, 4_096, 0, 0),
    ),
    p(
        "stress-cores",
        "Stress: few cores",
        "Only the core count is limited (4 logical processors, two cores with both threads); nothing else. Shows where the title's threads queue.",
        keys!(3840, 2160, "auto", "quality", 0, 0, 4, 0),
    ),
    p(
        "stress-cpu",
        "Stress: CPU-capped",
        "Only CPU time is capped (25 % of the machine, every core still visible); nothing else. A slow CPU without fewer threads.",
        keys!(3840, 2160, "auto", "quality", 0, 0, 0, 25),
    ),
];

/// A built-in by name (case-insensitive).
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    let n = name.trim();
    BUILTIN_PROFILES.iter().find(|b| b.name.eq_ignore_ascii_case(n))
}

/// Which profile a configuration is on.
#[derive(Debug, Clone, PartialEq)]
pub enum Active {
    /// a built-in, with every one of its values still in place
    Builtin(&'static Builtin),
    /// a built-in whose values were changed afterwards: save it as a user copy
    Edited(&'static Builtin),
    User(String),
}

impl Active {
    pub fn of(c: &Config) -> Active {
        match builtin(&c.profile) {
            Some(b) if b.matches(c) => Active::Builtin(b),
            Some(b) => Active::Edited(b),
            None => Active::User(if c.profile.is_empty() { "default".into() } else { c.profile.clone() }),
        }
    }

    /// For the launcher: "Series X budget (built-in)", "… (built-in, edited)", or the user's name.
    pub fn describe(&self) -> String {
        match self {
            Active::Builtin(b) => format!("{} (built-in)", b.label),
            Active::Edited(b) => format!("{} (built-in, edited)", b.label),
            Active::User(n) => n.clone(),
        }
    }
}

impl Builtin {
    /// Stable unless it sets an experimental key away from its default. A profile that isn't stable can't
    /// be applied to a configuration marked stable-only.
    pub fn stable(&self) -> bool {
        self.settings.iter().all(|(k, v)| find(k).is_none_or(|s| s.tier == Tier::Stable || s.default == *v))
    }

    /// The experimental keys this profile sets away from their defaults.
    pub fn experimental_keys(&self) -> Vec<&'static str> {
        self.settings
            .iter()
            .filter(|(k, v)| find(k).is_some_and(|s| s.tier == Tier::Experimental && s.default != *v))
            .map(|(k, _)| *k)
            .collect()
    }

    pub fn value(&self, key: &str) -> Option<Value> {
        self.settings.iter().find(|(k, _)| *k == key).map(|(_, v)| v.value())
    }

    /// Set this profile's keys in `c`, and `c.profile` to its name. Every other value, explicit or not,
    /// is left exactly as it was. All or nothing: on a refusal `c` is unchanged.
    pub fn apply(&self, c: &mut Config) -> Result<(), String> {
        let mut next = c.clone();
        for (k, v) in self.settings {
            next.set(k, v.value()).map_err(|e| format!("{}: {e}", self.label))?;
        }
        next.profile = self.name.to_string();
        *c = next;
        Ok(())
    }

    /// Every value of this profile is in force in `c`.
    pub fn matches(&self, c: &Config) -> bool {
        self.settings.iter().all(|(k, v)| find(k).is_some() && c.get(k) == v.value())
    }

    /// One line for the picker: output, upscaler, limits.
    pub fn summary(&self) -> String {
        let mut c = Config::default();
        c.values.clear();
        for (k, v) in self.settings {
            if let Some(s) = find(k) {
                c.values.insert(s.key, v.value());
            }
        }
        let mut parts = vec![format!(
            "{} x {} · {} {}",
            c.int("display.width"),
            c.int("display.height"),
            c.choice("upscaler.backend").to_uppercase(),
            c.choice("upscaler.mode").replace('_', " ")
        )];
        let l = limits_in_force(&c);
        parts.push(if l.is_empty() { "no limits".into() } else { l.join(" · ") });
        parts.join(" · ")
    }
}

/// The hardware limits a configuration puts in force, one short phrase each (empty: none).
pub fn limits_in_force(c: &Config) -> Vec<String> {
    let mib = |m: i64| {
        if m % 1024 == 0 {
            format!("{} GiB", m / 1024)
        } else {
            format!("{:.1} GiB", m as f64 / 1024.0)
        }
    };
    let mut v = Vec::new();
    let ram = c.int("limits.ram_mb");
    if ram > 0 {
        v.push(format!("RAM {}", mib(ram)));
    }
    let vram = c.int("limits.vram_mb");
    if vram > 0 {
        v.push(format!("VRAM {} (from Build 5)", mib(vram)));
    }
    let cores = c.int("limits.cpu_cores");
    if cores > 0 {
        v.push(format!("{cores} logical processors"));
    }
    let rate = c.int("limits.cpu_rate");
    if rate > 0 {
        v.push(format!("CPU {rate} %"));
    }
    v
}

/// The configuration a test-matrix run uses for `b`: the user's `base` (game folder, paths, mods and
/// every other setting kept) with the profile applied. A matrix file is a test file, so a built-in that
/// isn't stable clears `stable_only` in it rather than being refused.
pub fn matrix_config(base: &Config, b: &Builtin) -> Result<Config, String> {
    let mut c = base.clone();
    c.problems.clear();
    if !b.stable() {
        c.stable_only = false;
    }
    b.apply(&mut c)?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_validates_against_the_registry() {
        let mut names = std::collections::HashSet::new();
        for b in BUILTIN_PROFILES {
            assert!(names.insert(b.name), "{} twice", b.name);
            assert!(!b.label.is_empty() && !b.purpose.is_empty(), "{}", b.name);
            assert!(
                b.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}: a name is a file name",
                b.name
            );
            let keys: Vec<&str> = b.settings.iter().map(|(k, _)| *k).collect();
            assert_eq!(keys, PROFILE_KEYS, "{}: sets exactly the profile keys", b.name);
            for (k, v) in b.settings {
                let s = find(k).unwrap_or_else(|| panic!("{}: {k} is not in the registry", b.name));
                s.check(&v.value()).unwrap_or_else(|e| panic!("{}: {e}", b.name));
            }
            // and it applies to an empty configuration, which then reads back through the file
            let mut c = Config::parse("").unwrap();
            b.apply(&mut c).unwrap();
            assert!(b.matches(&c));
            let back = Config::parse(&c.to_toml()).unwrap();
            assert!(back.problems.is_empty(), "{}: {:#?}", b.name, back.problems);
            assert_eq!(Active::of(&back), Active::Builtin(b));
        }
    }

    #[test]
    fn the_required_set_is_there_with_its_key_values() {
        let d = builtin("dev-machine").unwrap();
        assert_eq!(d.value("display.width"), Some(Value::Int(3840)));
        assert_eq!(d.value("upscaler.mode"), Some(Value::Choice("quality".into())));
        assert!(d.stable() && limits_in_force(&d.apply_to_empty()).is_empty(), "the reference has no limits");
        for n in ["series-x", "series-s", "mid-pc", "low-pc", "stress-ram", "stress-vram", "stress-cores", "stress-cpu"] {
            assert!(builtin(n).is_some(), "{n}");
        }
        assert_eq!(builtin("SERIES-X").map(|b| b.name), Some("series-x"), "case-insensitive");
        // stress profiles isolate one limit each
        for n in ["stress-ram", "stress-vram", "stress-cores", "stress-cpu"] {
            let c = builtin(n).unwrap().apply_to_empty();
            assert_eq!(limits_in_force(&c).len(), 1, "{n} limits one thing");
            for k in ["display.width", "display.height", "upscaler.backend", "upscaler.mode"] {
                assert_eq!(Some(c.get(k)), d.value(k), "{n}: {k} is the dev machine's");
            }
        }
    }

    #[test]
    fn profiles_with_experimental_keys_are_not_stable_and_refused_by_a_stable_config() {
        for b in BUILTIN_PROFILES {
            let uses_vram = b.value("limits.vram_mb") != Some(Value::Int(0));
            assert_eq!(b.stable(), !uses_vram, "{}", b.name);
            assert_eq!(b.experimental_keys().is_empty(), b.stable(), "{}", b.name);
            let mut c = Config::parse("stable_only = true").unwrap();
            let before = c.clone();
            match b.apply(&mut c) {
                Ok(()) => assert!(b.stable(), "{}", b.name),
                Err(e) => {
                    assert!(!b.stable() && e.contains("experimental"), "{}: {e}", b.name);
                    assert_eq!(c, before, "a refusal leaves the configuration unchanged");
                }
            }
        }
    }

    #[test]
    fn applying_keeps_every_other_explicit_setting() {
        let mut c = Config::parse(
            r#"
            profile = "mine"
            [settings]
            "display.vsync" = true
            "memory.upload_workers" = 4
            "game.dir" = 'D:\games\title'
            "limits.cpu_rate" = 50
            "future.thing" = 1
            "#,
        )
        .unwrap();
        builtin("series-x").unwrap().apply(&mut c).unwrap();
        builtin("mid-pc").unwrap().apply(&mut c).unwrap();
        let back = Config::parse(&c.to_toml()).unwrap();
        assert_eq!(back.profile, "mid-pc");
        assert!(back.bool("display.vsync"));
        assert!(back.explicit().any(|(k, v)| k == "memory.upload_workers" && *v == Value::Int(4)), "explicit default kept");
        assert_eq!(back.get("game.dir"), Value::Path(r"D:\games\title".into()));
        assert_eq!(back.kept()["future.thing"].as_integer(), Some(1), "unknown key kept");
        // the profile's own keys are the last applied, nothing of series-x is left
        assert_eq!(back.int("limits.cpu_rate"), 0, "a profile key is the profile's");
        assert_eq!(back.int("limits.ram_mb"), 12_288);
        assert_eq!(back.int("limits.cpu_cores"), 12);
        assert_eq!(Active::of(&back), Active::Builtin(builtin("mid-pc").unwrap()));
    }

    #[test]
    fn editing_a_builtin_is_a_copy_and_the_builtin_is_unchanged() {
        let b = builtin("low-pc").unwrap();
        let mut c = Config::parse("").unwrap();
        b.apply(&mut c).unwrap();
        c.set("display.width", Value::Int(1600)).unwrap();
        assert_eq!(Active::of(&c), Active::Edited(b));
        assert!(Active::of(&c).describe().contains("edited"));
        // the built-in still holds its own value; applying it again restores it
        assert_eq!(b.value("display.width"), Some(Value::Int(1920)));
        b.apply(&mut c).unwrap();
        assert_eq!(Active::of(&c), Active::Builtin(b));
        c.profile = "low-pc copy".into();
        assert_eq!(Active::of(&c), Active::User("low-pc copy".into()));
    }

    #[test]
    fn matrix_configs_parse_and_keep_the_base() {
        let mut base = Config::parse("stable_only = true\n[settings]\n\"display.vsync\" = true\n").unwrap();
        base.mods.push(crate::Mod { name: "hud".into(), enabled: true });
        for b in BUILTIN_PROFILES {
            let c = matrix_config(&base, b).unwrap();
            let back = Config::parse(&c.to_toml()).unwrap();
            assert!(back.problems.is_empty(), "{}: {:#?}", b.name, back.problems);
            assert!(b.matches(&back), "{}", b.name);
            assert!(back.bool("display.vsync"));
            assert_eq!(back.mods, base.mods);
            assert_eq!(back.stable_only, b.stable(), "{}", b.name);
        }
    }

    impl Builtin {
        fn apply_to_empty(&self) -> Config {
            let mut c = Config::parse("").unwrap();
            self.apply(&mut c).unwrap();
            c
        }
    }
}
