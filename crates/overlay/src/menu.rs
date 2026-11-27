//! The menu's state: three pages, a selection on each, and what each [`Nav`] does.
//!
//! - **Settings**: every `Live` entry of the config [`REGISTRY`] the host applies ([`Backend::applies`]), in
//!   registry order. Left / Right step the value (a bool flips, a choice cycles, a number moves through its
//!   presets, else by 1), Accept steps forward. Each change goes to the [`Backend`] at once and marks the config
//!   dirty, saved when the menu closes.
//! - **Stats**: read-only, from [`Backend::stats`].
//! - **Developer**: the [`Registry`]'s actions. Accept runs an action without parameters, or opens its
//!   parameters (rows plus "Run"); Back leaves them.
//!
//! Back at the top level and Close (Esc, Start) close the menu; LB / RB (Q / E) change page.

use rebuild_config::{Apply, Config, Kind, Setting, Value, REGISTRY};

use crate::actions::{Blocked, ParamValue, Registry};

/// What the host does with settings and what it measures.
pub trait Backend {
    /// Does the host apply `key` while running? Only those are listed.
    fn applies(&self, key: &str) -> bool;
    /// The value in effect now, when the host knows it (it wins over the file's).
    fn current(&self, key: &str) -> Option<Value>;
    /// `key` changed; `config` holds every value after the change.
    fn apply(&mut self, key: &str, config: &Config);
    fn stats(&self) -> Stats;
}

/// The stats readout. `seq` changes whenever anything else does (the layout is redone only then).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub seq: u64,
    pub fps: Option<f64>,
    pub ms: Option<f64>,
    pub gpu: Option<f64>,
    pub draws: Option<u64>,
    pub dispatches: Option<u64>,
    pub dropped: Option<u64>,
    pub late: Option<u64>,
    pub wait: Option<f64>,
    pub internal: Option<(u32, u32)>,
    pub output: Option<(u32, u32)>,
    /// the overlay's own cost a frame, µs: CPU on the present thread, GPU from timestamps
    pub overlay_cpu_us: Option<f64>,
    pub overlay_gpu_us: Option<f64>,
    /// the pacing in words (`frame::describe`)
    pub pacing: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Settings,
    Stats,
    Developer,
}

pub const PAGES: [Page; 3] = [Page::Settings, Page::Stats, Page::Developer];

impl Page {
    pub fn title(self) -> &'static str {
        match self {
            Page::Settings => "Settings",
            Page::Stats => "Stats",
            Page::Developer => "Developer",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
    PrevPage,
    NextPage,
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// nothing happened (a key with no meaning here)
    None,
    /// the selection or page moved
    Moved,
    /// a setting changed and was applied
    Changed(&'static str),
    /// an action ran: its line
    Ran(String),
    /// an action or change was refused: why
    Refused(String),
    /// close the menu
    Close,
}

/// The parameters of the action being set up.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub action: usize,
    pub args: Vec<ParamValue>,
    /// 0..params = a parameter, params = "Run"
    pub row: usize,
}

/// One row as the layout draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub label: String,
    pub value: String,
    pub enabled: bool,
    /// a value that Left / Right change (drawn with arrows when selected)
    pub adjustable: bool,
}

pub struct Menu {
    page: usize,
    selected: [usize; 3],
    settings: Vec<&'static Setting>,
    config: Config,
    dirty: bool,
    actions: Registry,
    edit: Option<Edit>,
    message: Option<(String, bool)>,
}

impl Menu {
    /// The menu over `config` (the loaded `rebuild.toml`, or defaults), with the host's current values.
    pub fn new(config: Config, backend: &dyn Backend, actions: Registry) -> Menu {
        let settings: Vec<&'static Setting> =
            REGISTRY.iter().filter(|s| s.apply == Apply::Live && s.kind != Kind::Path && backend.applies(s.key)).collect();
        let mut m = Menu { page: 0, selected: [0; 3], settings, config, dirty: false, actions, edit: None, message: None };
        m.sync(backend);
        m
    }

    /// Take the host's current values (the menu opened: something else may have changed them).
    pub fn sync(&mut self, backend: &dyn Backend) {
        for s in &self.settings {
            if let Some(v) = backend.current(s.key) {
                if self.config.get(s.key) != v && s.check(&v).is_ok() {
                    let _ = self.config.set(s.key, v);
                }
            }
        }
    }

    pub fn page(&self) -> Page {
        PAGES[self.page]
    }

    pub fn selected(&self) -> usize {
        self.selected[self.page]
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn actions(&self) -> &Registry {
        &self.actions
    }

    pub fn edit(&self) -> Option<&Edit> {
        self.edit.as_ref()
    }

    /// The last action's or refusal's line, and whether it was an error.
    pub fn message(&self) -> Option<(&str, bool)> {
        self.message.as_ref().map(|(s, e)| (s.as_str(), *e))
    }

    /// A change not saved yet; [`Menu::take_dirty`] clears it.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn settings(&self) -> &[&'static Setting] {
        &self.settings
    }

    /// The rows of the current page.
    pub fn rows(&self) -> Vec<Row> {
        match self.page() {
            Page::Settings => self
                .settings
                .iter()
                .map(|s| Row { label: s.label.to_string(), value: show(s, &self.config.get(s.key)), enabled: true, adjustable: true })
                .collect(),
            Page::Stats => Vec::new(),
            Page::Developer => match &self.edit {
                Some(e) => {
                    let a = self.actions.get(e.action).expect("edited action");
                    let mut rows: Vec<Row> = a
                        .params()
                        .iter()
                        .zip(&e.args)
                        .map(|(p, v)| Row { label: p.name.to_string(), value: p.show(*v), enabled: true, adjustable: true })
                        .collect();
                    rows.push(Row { label: format!("Run {}", a.name()), value: String::new(), enabled: self.actions.blocked(e.action).is_none(), adjustable: false });
                    rows
                }
                None => (0..self.actions.len())
                    .map(|i| {
                        let a = self.actions.get(i).expect("action");
                        let value = match self.actions.blocked(i) {
                            Some(Blocked::Online) => "online: off".to_string(),
                            Some(Blocked::Unbound) => "not bound".to_string(),
                            None if a.params().is_empty() => String::new(),
                            None => "...".to_string(),
                        };
                        Row { label: a.name().to_string(), value, enabled: self.actions.blocked(i).is_none(), adjustable: false }
                    })
                    .collect(),
            },
        }
    }

    /// The help line for the selection: a setting's help, an action's group.
    pub fn help(&self) -> Option<String> {
        match self.page() {
            Page::Settings => self.settings.get(self.selected()).map(|s| s.help.to_string()),
            Page::Stats => None,
            Page::Developer => {
                let i = self.edit.as_ref().map_or(self.selected(), |e| e.action);
                let a = self.actions.get(i)?;
                Some(match self.actions.blocked(i) {
                    Some(Blocked::Online) => format!("{}: disabled while the title is online.", a.group()),
                    Some(Blocked::Unbound) => format!("{}: waits for the dev-menu research (docs/research/devmenu.md).", a.group()),
                    None => a.group().to_string(),
                })
            }
        }
    }

    fn row_count(&self) -> usize {
        match self.page() {
            Page::Settings => self.settings.len(),
            Page::Stats => 0,
            Page::Developer => match &self.edit {
                Some(e) => e.args.len() + 1,
                None => self.actions.len(),
            },
        }
    }

    fn cursor(&mut self) -> &mut usize {
        match &mut self.edit {
            Some(e) if self.page == 2 => &mut e.row,
            _ => &mut self.selected[self.page],
        }
    }

    /// One navigation event.
    pub fn nav(&mut self, nav: Nav, backend: &mut dyn Backend) -> Outcome {
        if nav != Nav::Up && nav != Nav::Down {
            self.message = None;
        }
        match nav {
            Nav::Close => return Outcome::Close,
            Nav::PrevPage | Nav::NextPage => {
                let n = PAGES.len();
                self.page = if nav == Nav::NextPage { (self.page + 1) % n } else { (self.page + n - 1) % n };
                self.edit = None;
                return Outcome::Moved;
            }
            Nav::Back => {
                return if self.edit.take().is_some() { Outcome::Moved } else { Outcome::Close };
            }
            Nav::Up | Nav::Down => {
                let n = self.row_count();
                if n == 0 {
                    return Outcome::None;
                }
                let c = self.cursor();
                *c = if nav == Nav::Down { (*c + 1) % n } else { (*c + n - 1) % n };
                return Outcome::Moved;
            }
            Nav::Left | Nav::Right | Nav::Accept => {}
        }
        let dir = if nav == Nav::Left { -1 } else { 1 };
        match self.page() {
            Page::Settings => {
                let Some(&s) = self.settings.get(self.selected()) else { return Outcome::None };
                let v = step(s, &self.config.get(s.key), dir);
                if v == self.config.get(s.key) {
                    return Outcome::None;
                }
                match self.config.set(s.key, v) {
                    Ok(()) => {
                        self.dirty = true;
                        backend.apply(s.key, &self.config);
                        Outcome::Changed(s.key)
                    }
                    Err(e) => self.refuse(e),
                }
            }
            Page::Stats => Outcome::None,
            Page::Developer => self.developer(nav, dir),
        }
    }

    fn developer(&mut self, nav: Nav, dir: i32) -> Outcome {
        if let Some(e) = &mut self.edit {
            let a = self.actions.get(e.action).expect("edited action");
            let params = a.params();
            if e.row < params.len() {
                if nav == Nav::Accept {
                    e.row += 1;
                    return Outcome::Moved;
                }
                e.args[e.row] = params[e.row].step(e.args[e.row], dir);
                return Outcome::Moved;
            }
            if nav != Nav::Accept {
                return Outcome::None;
            }
            let (i, args) = (e.action, e.args.clone());
            return self.run(i, &args);
        }
        if nav != Nav::Accept {
            return Outcome::None;
        }
        let i = self.selected();
        let Some(a) = self.actions.get(i) else { return Outcome::None };
        if a.params().is_empty() || self.actions.blocked(i).is_some() {
            return self.run(i, &[]);
        }
        self.edit = Some(Edit { action: i, args: a.params().iter().map(|p| p.default).collect(), row: 0 });
        Outcome::Moved
    }

    fn run(&mut self, i: usize, args: &[ParamValue]) -> Outcome {
        match self.actions.run(i, args) {
            Ok(line) => {
                self.message = Some((line.clone(), false));
                Outcome::Ran(line)
            }
            Err(e) => self.refuse(e),
        }
    }

    fn refuse(&mut self, why: String) -> Outcome {
        self.message = Some((why.clone(), true));
        Outcome::Refused(why)
    }
}

/// A setting's value as the menu shows it.
pub fn show(s: &Setting, v: &Value) -> String {
    match v {
        Value::Bool(b) => (if *b { "On" } else { "Off" }).into(),
        Value::Int(i) if s.key == "display.frame_cap" => format!("{i} fps"),
        v => v.to_string(),
    }
}

/// `v` moved one step in `dir` (-1, +1).
pub fn step(s: &Setting, v: &Value, dir: i32) -> Value {
    match (s.kind, v) {
        (Kind::Bool, Value::Bool(b)) => Value::Bool(!b),
        (Kind::Choice { options }, Value::Choice(c)) => {
            let i = options.iter().position(|o| o == c).unwrap_or(0) as i64;
            Value::Choice(options[(i + i64::from(dir)).rem_euclid(options.len() as i64) as usize].into())
        }
        (Kind::Int { min, max }, Value::Int(i)) => {
            let i = *i;
            let next = if s.presets.is_empty() {
                i + i64::from(dir)
            } else if dir > 0 {
                s.presets.iter().copied().find(|&p| p > i).unwrap_or(i)
            } else {
                s.presets.iter().rev().copied().find(|&p| p < i).unwrap_or(i)
            };
            Value::Int(next.clamp(min, max))
        }
        (_, v) => v.clone(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::actions::{Action, Param};
    use std::collections::BTreeMap;

    /// A host that records what it was asked to apply.
    #[derive(Default)]
    pub struct Fake {
        pub applied: Vec<(String, Value)>,
        pub now: BTreeMap<&'static str, Value>,
    }
    impl Backend for Fake {
        fn applies(&self, key: &str) -> bool {
            matches!(key, "display.vsync" | "display.frame_rate" | "display.frame_cap" | "diagnostics.overlay")
        }
        fn current(&self, key: &str) -> Option<Value> {
            self.now.get(key).cloned()
        }
        fn apply(&mut self, key: &str, c: &Config) {
            self.applied.push((key.into(), c.get(key)));
        }
        fn stats(&self) -> Stats {
            Stats { seq: 1, fps: Some(30.0), ms: Some(33.3), ..Stats::default() }
        }
    }

    struct Hello;
    impl Action for Hello {
        fn name(&self) -> &str {
            "Hello"
        }
        fn offline_only(&self) -> bool {
            false
        }
        fn run(&mut self, _: &[ParamValue]) -> Result<String, String> {
            Ok("hello".into())
        }
    }

    struct Warp;
    impl Action for Warp {
        fn name(&self) -> &str {
            "Warp"
        }
        fn params(&self) -> &[Param] {
            static P: [Param; 2] = [Param::int("x", -10, 10, 5, 0), Param::choice("speed", &["slow", "fast"], 0)];
            &P
        }
        fn run(&mut self, a: &[ParamValue]) -> Result<String, String> {
            Ok(format!("warp {} {}", a[0], a[1]))
        }
    }

    pub fn menu(b: &Fake) -> Menu {
        let mut r = Registry::default();
        r.register(Box::new(Hello));
        r.register(Box::new(Warp));
        Menu::new(Config::default(), b, r.with_placeholders())
    }

    #[test]
    fn the_settings_page_lists_the_live_settings_the_host_applies_in_registry_order() {
        let b = Fake::default();
        let m = menu(&b);
        let keys: Vec<&str> = m.settings().iter().map(|s| s.key).collect();
        assert_eq!(keys, ["display.vsync", "display.frame_rate", "display.frame_cap", "diagnostics.overlay"]);
        let rows = m.rows();
        assert_eq!(rows[0], Row { label: "Vsync".into(), value: "Off".into(), enabled: true, adjustable: true });
        assert_eq!(rows[1].value, "title");
        assert_eq!(rows[2].value, "60 fps");
        assert!(m.help().unwrap().contains("refresh"));
    }

    #[test]
    fn the_hosts_current_values_win_over_the_file() {
        let mut b = Fake::default();
        b.now.insert("display.frame_rate", Value::Choice("unlimited".into()));
        b.now.insert("display.vsync", Value::Bool(true));
        let m = menu(&b);
        assert_eq!(m.config().choice("display.frame_rate"), "unlimited");
        assert!(m.config().bool("display.vsync"));
        assert!(!m.dirty(), "a sync is not a change of the user's");
    }

    #[test]
    fn navigation_wraps_and_pages_cycle() {
        let mut b = Fake::default();
        let mut m = menu(&b);
        assert_eq!(m.nav(Nav::Up, &mut b), Outcome::Moved);
        assert_eq!(m.selected(), 3, "up from the first row wraps to the last");
        m.nav(Nav::Down, &mut b);
        assert_eq!(m.selected(), 0);
        assert_eq!(m.nav(Nav::NextPage, &mut b), Outcome::Moved);
        assert_eq!(m.page(), Page::Stats);
        assert_eq!(m.nav(Nav::Down, &mut b), Outcome::None, "stats has no rows");
        m.nav(Nav::NextPage, &mut b);
        assert_eq!(m.page(), Page::Developer);
        m.nav(Nav::NextPage, &mut b);
        assert_eq!(m.page(), Page::Settings);
        m.nav(Nav::PrevPage, &mut b);
        assert_eq!(m.page(), Page::Developer);
        // each page keeps its own selection
        m.nav(Nav::Down, &mut b);
        m.nav(Nav::PrevPage, &mut b);
        m.nav(Nav::PrevPage, &mut b);
        assert_eq!((m.page(), m.selected()), (Page::Settings, 0));
        assert_eq!(m.nav(Nav::Back, &mut b), Outcome::Close);
        assert_eq!(m.nav(Nav::Close, &mut b), Outcome::Close);
    }

    #[test]
    fn settings_step_and_are_applied_at_once() {
        let mut b = Fake::default();
        let mut m = menu(&b);
        assert_eq!(m.nav(Nav::Accept, &mut b), Outcome::Changed("display.vsync"));
        assert!(m.config().bool("display.vsync"));
        m.nav(Nav::Down, &mut b);
        assert_eq!(m.nav(Nav::Right, &mut b), Outcome::Changed("display.frame_rate"));
        assert_eq!(m.config().choice("display.frame_rate"), "capped");
        assert_eq!(m.nav(Nav::Left, &mut b), Outcome::Changed("display.frame_rate"));
        m.nav(Nav::Left, &mut b);
        assert_eq!(m.config().choice("display.frame_rate"), "unlimited", "choices wrap");
        // the cap moves through its presets: 60 -> 90 -> 60 -> 40 -> 30, and stops at the ends
        m.nav(Nav::Down, &mut b);
        for want in [90, 60, 40, 30, 30] {
            let nav = if want == 90 { Nav::Right } else { Nav::Left };
            m.nav(nav, &mut b);
            assert_eq!(m.config().int("display.frame_cap"), want);
        }
        assert!(m.dirty());
        assert!(m.take_dirty());
        assert!(!m.dirty());
        let keys: Vec<&str> = b.applied.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["display.vsync", "display.frame_rate", "display.frame_rate", "display.frame_rate", "display.frame_cap", "display.frame_cap", "display.frame_cap", "display.frame_cap"]);
        assert_eq!(b.applied[0].1, Value::Bool(true));
    }

    #[test]
    fn developer_actions_run_take_parameters_and_report() {
        let mut b = Fake::default();
        let mut m = menu(&b);
        m.nav(Nav::PrevPage, &mut b);
        assert_eq!(m.page(), Page::Developer);
        let rows = m.rows();
        assert_eq!(rows.len(), 6);
        assert_eq!((rows[0].label.as_str(), rows[0].enabled), ("Hello", true));
        assert_eq!(rows[1].value, "...");
        assert_eq!((rows[2].value.as_str(), rows[2].enabled), ("not bound", false));
        assert_eq!(m.nav(Nav::Accept, &mut b), Outcome::Ran("hello".into()));
        assert_eq!(m.message(), Some(("hello", false)));
        // Warp opens its parameters
        m.nav(Nav::Down, &mut b);
        assert_eq!(m.nav(Nav::Accept, &mut b), Outcome::Moved);
        let e = m.edit().unwrap();
        assert_eq!((e.action, e.row, e.args.len()), (1, 0, 2));
        assert_eq!(m.rows().len(), 3);
        m.nav(Nav::Right, &mut b);
        m.nav(Nav::Right, &mut b);
        m.nav(Nav::Accept, &mut b); // to the next parameter
        m.nav(Nav::Left, &mut b); // choices wrap: slow -> fast
        assert_eq!(m.rows()[0].value, "10");
        assert_eq!(m.rows()[1].value, "fast");
        m.nav(Nav::Down, &mut b);
        assert_eq!(m.nav(Nav::Accept, &mut b), Outcome::Ran("warp 10 #1".into()));
        assert_eq!(m.nav(Nav::Back, &mut b), Outcome::Moved, "back leaves the parameters");
        assert!(m.edit().is_none());
        // a placeholder refuses
        m.nav(Nav::Down, &mut b);
        assert!(matches!(m.nav(Nav::Accept, &mut b), Outcome::Refused(r) if r.contains("not bound")));
        assert!(m.message().unwrap().1);
        assert!(m.help().unwrap().contains("devmenu"));
    }

    #[test]
    fn online_disables_world_actions_in_the_menu() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ON: AtomicBool = AtomicBool::new(true);
        let mut b = Fake::default();
        let mut r = Registry::new(|| ON.load(Ordering::Relaxed));
        r.register(Box::new(Hello));
        r.register(Box::new(Warp));
        let mut m = Menu::new(Config::default(), &b, r);
        m.nav(Nav::PrevPage, &mut b);
        let rows = m.rows();
        assert!(rows[0].enabled, "Hello never touches the world");
        assert_eq!((rows[1].value.as_str(), rows[1].enabled), ("online: off", false));
        m.nav(Nav::Down, &mut b);
        assert!(matches!(m.nav(Nav::Accept, &mut b), Outcome::Refused(r) if r.contains("online")));
        assert!(m.edit().is_none(), "a blocked action does not open its parameters");
        ON.store(false, Ordering::Relaxed);
        assert_eq!(m.nav(Nav::Accept, &mut b), Outcome::Moved);
    }
}
