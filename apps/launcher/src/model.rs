//! U1: the settings widgets, generated from the registry. Nothing here names a setting: every widget,
//! badge and range comes from `rebuild_config::REGISTRY`, so a new registry entry appears in the
//! launcher without a line changed here.

use rebuild_config::{Apply, Config, Kind, Setting, Tier, Value, REGISTRY};

/// The launcher's sections: the seven in the top bar, and the game library (reached from Play).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Play,
    Profiles,
    Settings,
    Runs,
    Mods,
    Diagnostics,
    Saves,
    Library,
}

impl Tab {
    /// The top bar, in order.
    pub const TOP: [Tab; 7] = [Tab::Play, Tab::Profiles, Tab::Settings, Tab::Runs, Tab::Mods, Tab::Saves, Tab::Diagnostics];
    pub const ALL: [Tab; 8] = [Tab::Play, Tab::Profiles, Tab::Settings, Tab::Runs, Tab::Mods, Tab::Saves, Tab::Diagnostics, Tab::Library];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Play => "Play",
            Tab::Profiles => "Profiles",
            Tab::Settings => "Settings",
            Tab::Runs => "Runs",
            Tab::Mods => "Mods",
            Tab::Saves => "Saves",
            Tab::Diagnostics => "Diagnostics",
            Tab::Library => "Library",
        }
    }
}

/// The group pills on Settings. Every registry entry is in exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Display,
    Limits,
    Title,
    MemoryShaders,
    Menu,
    Diagnostics,
    Files,
    Launcher,
}

impl Group {
    pub const ALL: [Group; 8] = [Group::Display, Group::Limits, Group::Title, Group::MemoryShaders, Group::Menu, Group::Diagnostics, Group::Files, Group::Launcher];

    pub fn label(self) -> &'static str {
        match self {
            Group::Display => "Display & upscaler",
            Group::Limits => "Hardware limits",
            Group::Title => "Title settings",
            Group::MemoryShaders => "Memory & shaders",
            Group::Menu => "In-game menu",
            Group::Diagnostics => "Diagnostics",
            Group::Files => "Files & paths",
            Group::Launcher => "Launcher",
        }
    }
}

/// The pill a setting is under: every file or folder setting is under Files & paths; the rest by their
/// registry group. A group the launcher doesn't know yet goes to Memory & shaders, so no entry is hidden.
pub fn group_of(s: &Setting) -> Group {
    if s.kind == Kind::Path {
        return Group::Files;
    }
    match s.group() {
        "display" | "upscaler" => Group::Display,
        "limits" => Group::Limits,
        "title" => Group::Title,
        "menu" => Group::Menu,
        "diagnostics" | "debug" => Group::Diagnostics,
        "game" => Group::Files,
        "launcher" => Group::Launcher,
        _ => Group::MemoryShaders,
    }
}

pub fn settings_in(g: Group) -> impl Iterator<Item = &'static Setting> {
    REGISTRY.iter().filter(move |s| group_of(s) == g)
}

/// Settings search: every word of `query` in the label, key or help (any case). Empty matches all.
pub fn matches(s: &Setting, query: &str) -> bool {
    let hay = format!("{} {} {}", s.label, s.key, s.help).to_lowercase();
    query.split_whitespace().all(|w| hay.contains(&w.to_lowercase()))
}

/// What a save would change: (keys whose effective value differs from `saved`, of those how many apply
/// on the next launch).
pub fn pending(saved: &Config, now: &Config) -> (Vec<&'static str>, usize) {
    let keys: Vec<&'static str> = REGISTRY.iter().filter(|s| saved.get(s.key) != now.get(s.key)).map(|s| s.key).collect();
    let restart = keys.iter().filter(|k| rebuild_config::find(k).is_some_and(|s| s.apply == Apply::Restart)).count();
    (keys, restart)
}

/// The widget a setting gets, from its kind alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    Checkbox,
    Slider { min: i64, max: i64 },
    Dropdown { options: &'static [&'static str] },
    PathPicker,
}

pub fn widget(s: &Setting) -> Widget {
    match s.kind {
        Kind::Bool => Widget::Checkbox,
        Kind::Int { min, max } => Widget::Slider { min, max },
        Kind::Choice { options } => Widget::Dropdown { options },
        Kind::Path => Widget::PathPicker,
    }
}

/// What the UI shows beside a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badges {
    /// "live" or "restart"
    pub apply: &'static str,
    /// shown with a warning
    pub experimental: bool,
    /// differs from the default: the reset button is enabled
    pub changed: bool,
}

/// A widget's state, as the UI holds it while drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Toggle(bool),
    Slide(i64),
    /// index into the dropdown's options
    Pick(usize),
    Text(String),
}

/// The settings being edited: a `Config` plus the last refusal, shown under the widget that caused it.
#[derive(Debug, Clone)]
pub struct Model {
    pub config: Config,
    pub refusal: Option<(&'static str, String)>,
    /// keys the user set (or the file held): saved explicitly even when equal to today's default, so a
    /// later change of default never resets them (`store`)
    pub explicit: std::collections::BTreeSet<&'static str>,
}

impl Model {
    pub fn new(config: Config) -> Model {
        Model { config, refusal: None, explicit: Default::default() }
    }

    pub fn badges(&self, s: &Setting) -> Badges {
        Badges {
            apply: match s.apply {
                Apply::Live => "live",
                Apply::Restart => "restart",
            },
            experimental: s.tier == Tier::Experimental,
            changed: self.config.get(s.key) != s.default.value(),
        }
    }

    /// The widget state for a setting's current value.
    pub fn current(&self, s: &Setting) -> Edit {
        match (widget(s), self.config.get(s.key)) {
            (Widget::Checkbox, Value::Bool(b)) => Edit::Toggle(b),
            (Widget::Slider { .. }, Value::Int(i)) => Edit::Slide(i),
            (Widget::Dropdown { options }, Value::Choice(c)) => {
                Edit::Pick(options.iter().position(|o| *o == c).unwrap_or(0))
            }
            (Widget::PathPicker, Value::Path(p)) => Edit::Text(p),
            (w, v) => unreachable!("{}: widget {w:?} holds {v:?}", s.key),
        }
    }

    /// Apply a widget edit. A refused value keeps the old one and records the reason for the UI.
    pub fn apply(&mut self, s: &'static Setting, e: Edit) -> Result<(), String> {
        let v = match (widget(s), e) {
            (Widget::Checkbox, Edit::Toggle(b)) => Value::Bool(b),
            (Widget::Slider { .. }, Edit::Slide(i)) => Value::Int(i),
            (Widget::Dropdown { options }, Edit::Pick(i)) => {
                Value::Choice(options.get(i).ok_or_else(|| format!("{}: no option {i}", s.key))?.to_string())
            }
            (Widget::PathPicker, Edit::Text(t)) => Value::Path(t),
            (w, e) => return Err(format!("{}: {e:?} does not fit a {w:?}", s.key)),
        };
        match self.config.set(s.key, v) {
            Ok(()) => {
                self.explicit.insert(s.key);
                if self.refusal.as_ref().is_some_and(|(k, _)| *k == s.key) {
                    self.refusal = None;
                }
                Ok(())
            }
            Err(why) => {
                self.refusal = Some((s.key, why.clone()));
                Err(why)
            }
        }
    }

    /// Apply a built-in test profile: its keys are set (and become explicit), `profile` is its name, and
    /// every other setting is left as it was. All or nothing; the refusal is returned.
    pub fn apply_builtin(&mut self, b: &rebuild_config::Builtin) -> Result<(), String> {
        b.apply(&mut self.config)?;
        for (k, _) in b.settings {
            if let Some(s) = rebuild_config::find(k) {
                self.explicit.insert(s.key);
            }
        }
        Ok(())
    }

    pub fn reset(&mut self, s: &Setting) {
        self.config.reset(s.key);
        self.explicit.remove(s.key);
        if self.refusal.as_ref().is_some_and(|(k, _)| *k == s.key) {
            self.refusal = None;
        }
    }

    /// Save to `rebuild.toml`; the text written is checked by `Config::parse` before it lands.
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let text = self.config.to_toml();
        let back = Config::parse(&text)?;
        if !back.problems.is_empty() {
            return Err(format!("not saved: {}", back.problems.join("; ")));
        }
        crate::write_atomic(path, &text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// A valid value other than the default, for each widget: what a user could pick. Used by the tests.
pub fn other_value(s: &Setting) -> Edit {
    let refused = |v: &Value| s.refused.iter().any(|(r, _)| r.value() == *v);
    match (widget(s), s.default.value()) {
        (Widget::Checkbox, Value::Bool(b)) => Edit::Toggle(!b),
        (Widget::Slider { min, max }, Value::Int(d)) => {
            let pick = (min..=max).rev().find(|i| *i != d && !refused(&Value::Int(*i))).expect("a second value");
            Edit::Slide(pick)
        }
        (Widget::Dropdown { options }, Value::Choice(d)) => {
            Edit::Pick(options.iter().position(|o| *o != d).expect("a second option"))
        }
        (Widget::PathPicker, _) => Edit::Text(format!(r"C:\picked\{}", s.key)),
        (w, v) => unreachable!("{}: {w:?} with default {v:?}", s.key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_entry_has_a_widget_and_a_group() {
        let mut shown = 0;
        for g in Group::ALL {
            shown += settings_in(g).count();
        }
        assert_eq!(shown, REGISTRY.len(), "every entry under exactly one pill");
        for g in Group::ALL {
            assert!(settings_in(g).count() > 0, "{} is empty", g.label());
        }
        for s in REGISTRY {
            let m = Model::new(Config::parse("").unwrap());
            // the widget state for the default reads back as the default
            let e = m.current(s);
            let mut m2 = m.clone();
            m2.apply(s, e).unwrap();
            assert_eq!(m2.config.changed().count(), 0, "{}", s.key);
            assert!(!m.badges(s).changed);
        }
        assert_eq!(group_of(rebuild_config::find("diagnostics.retention_days").unwrap()), Group::Diagnostics);
        assert_eq!(group_of(rebuild_config::find("launcher.theme").unwrap()), Group::Launcher);
        assert_eq!(group_of(rebuild_config::find("upscaler.dlss_dll").unwrap()), Group::Files);
    }

    #[test]
    fn search_and_pending_changes() {
        let fsr = rebuild_config::find("title.fsr_quality").unwrap();
        assert!(matches(fsr, ""));
        assert!(matches(fsr, "fsrquality"));
        assert!(matches(fsr, "RENDER scale"));
        assert!(!matches(fsr, "vsync"));
        let found: Vec<_> = REGISTRY.iter().filter(|s| matches(s, "retention")).map(|s| s.key).collect();
        assert_eq!(found, vec!["diagnostics.retention_days"]);
        let saved = Config::parse("").unwrap();
        let mut now = saved.clone();
        now.set("display.vsync", Value::Bool(true)).unwrap();
        now.set("display.width", Value::Int(2560)).unwrap();
        now.set("display.height", Value::Int(2160)).unwrap(); // = the default: not a change
        let (keys, restart) = pending(&saved, &now);
        assert_eq!(keys, vec!["display.width", "display.vsync"]);
        assert_eq!(restart, 1, "the width applies on the next launch; vsync is live");
    }

    /// U1 closes here: every registry entry is edited through the UI model, saved, and read back by
    /// `Config::parse` with 0 problems and the same values.
    #[test]
    fn every_registry_entry_round_trips_through_the_ui_model() {
        let dir = crate::temp_dir("model");
        let path = dir.join("rebuild.toml");
        let mut m = Model::new(Config::parse("").unwrap());
        m.config.profile = "round-trip".into();
        for s in REGISTRY {
            let e = other_value(s);
            m.apply(s, e.clone()).unwrap_or_else(|why| panic!("{}: {why}", s.key));
            assert_eq!(m.current(s), e, "{} reads back", s.key);
            assert!(m.badges(s).changed, "{}", s.key);
        }
        assert_eq!(m.config.changed().count(), REGISTRY.len());
        m.save(&path).unwrap();
        let back = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(back.problems.is_empty(), "{:#?}", back.problems);
        assert_eq!(back, m.config);
        // and resetting every entry through the UI gives an all-default file
        for s in REGISTRY {
            m.reset(s);
        }
        m.save(&path).unwrap();
        let back = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.changed().count(), 0);
        assert!(back.problems.is_empty());
    }

    #[test]
    fn a_refused_value_keeps_the_old_one_and_shows_why() {
        let mut m = Model::new(Config::parse("").unwrap());
        let fsr = rebuild_config::find("title.fsr_quality").unwrap();
        assert!(m.apply(fsr, Edit::Slide(4)).is_err());
        assert_eq!(m.current(fsr), Edit::Slide(3));
        let (k, why) = m.refusal.clone().unwrap();
        assert_eq!(k, "title.fsr_quality");
        assert!(why.contains("black screen"));
        m.apply(fsr, Edit::Slide(2)).unwrap();
        assert!(m.refusal.is_none());
    }

    /// Applying a built-in through the launcher, then saving, keeps the user's unrelated explicit settings.
    #[test]
    fn applying_a_builtin_then_saving_keeps_unrelated_settings() {
        let dir = crate::temp_dir("builtin-apply");
        let path = dir.join("rebuild.toml");
        std::fs::write(&path, "profile = \"mine\"\n[settings]\n\"display.vsync\" = true\n\"memory.upload_workers\" = 4\n\"future.switch\" = 1\n").unwrap();
        let mut st = crate::store::Store::load(&path).unwrap();
        let b = rebuild_config::builtin("low-pc").unwrap();
        st.model.apply_builtin(b).unwrap();
        // the user then edits one setting outside the profile
        st.model.apply(rebuild_config::find("display.frame_cap").unwrap(), Edit::Slide(144)).unwrap();
        st.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let back = Config::parse(&text).unwrap();
        assert!(back.bool("display.vsync"));
        assert!(text.contains("\"memory.upload_workers\" = 4"), "an explicit default is kept:\n{text}");
        assert!(text.contains("future.switch"), "an unknown key is kept");
        assert_eq!(back.int("display.frame_cap"), 144);
        assert!(b.matches(&back));
        assert_eq!(rebuild_config::profiles::Active::of(&back), rebuild_config::profiles::Active::Builtin(b));
        // a stable-only file refuses a built-in that uses an experimental limit, and is unchanged
        let mut m = Model::new(Config::parse("stable_only = true").unwrap());
        assert!(m.apply_builtin(rebuild_config::builtin("series-x").unwrap()).is_err());
        assert_eq!(m.config.explicit().count(), 0);
        assert!(m.explicit.is_empty());
        assert!(m.apply_builtin(rebuild_config::builtin("stress-cores").unwrap()).is_ok());
    }

    #[test]
    fn a_stable_profile_refuses_experimental_widgets() {
        let mut m = Model::new(Config::parse("stable_only = true").unwrap());
        for s in REGISTRY.iter().filter(|s| s.tier == Tier::Experimental) {
            assert!(m.badges(s).experimental);
            assert!(m.apply(s, other_value(s)).is_err(), "{}", s.key);
        }
    }
}
