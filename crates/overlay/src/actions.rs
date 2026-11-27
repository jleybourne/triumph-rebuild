//! The Developer page's actions: a registry of named actions with typed parameters.
//!
//! The actions themselves (teleport, spawn a vehicle, time, weather, ...) come from the dev-menu research
//! (`agent/devmenu-re`, `docs/research/devmenu.md`). Until an action is bound to the title it is listed as a
//! [`Placeholder`], shown dimmed, and refuses to run. Any action that changes the title's world is
//! **offline only**: it is disabled whenever the [`online`] probe says the title is online.

use std::fmt;

/// Is the title online? Stub: the title has no network path in the rebuild yet, so this is `false`. The real probe
/// (the title's own connection state, from the RE work) replaces it.
pub fn online() -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    Int { min: i64, max: i64, step: i64 },
    Float { min: f64, max: f64, step: f64 },
    Choice(&'static [&'static str]),
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamValue {
    Int(i64),
    Float(f64),
    /// an index into the choices
    Choice(usize),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Param {
    pub name: &'static str,
    pub kind: ParamKind,
    pub default: ParamValue,
}

impl Param {
    pub const fn int(name: &'static str, min: i64, max: i64, step: i64, default: i64) -> Param {
        Param { name, kind: ParamKind::Int { min, max, step }, default: ParamValue::Int(default) }
    }
    pub const fn float(name: &'static str, min: f64, max: f64, step: f64, default: f64) -> Param {
        Param { name, kind: ParamKind::Float { min, max, step }, default: ParamValue::Float(default) }
    }
    pub const fn choice(name: &'static str, options: &'static [&'static str], default: usize) -> Param {
        Param { name, kind: ParamKind::Choice(options), default: ParamValue::Choice(default) }
    }
    pub const fn bool(name: &'static str, default: bool) -> Param {
        Param { name, kind: ParamKind::Bool, default: ParamValue::Bool(default) }
    }

    /// `v` moved one step in `dir` (-1 or +1): clamped for numbers, wrapping for choices, flipped for a bool.
    pub fn step(&self, v: ParamValue, dir: i32) -> ParamValue {
        match (self.kind, v) {
            (ParamKind::Int { min, max, step }, ParamValue::Int(i)) => ParamValue::Int((i + step * i64::from(dir)).clamp(min, max)),
            (ParamKind::Float { min, max, step }, ParamValue::Float(f)) => ParamValue::Float((f + step * f64::from(dir)).clamp(min, max)),
            (ParamKind::Choice(o), ParamValue::Choice(i)) if !o.is_empty() => ParamValue::Choice((i as i64 + i64::from(dir)).rem_euclid(o.len() as i64) as usize),
            (ParamKind::Bool, ParamValue::Bool(b)) => ParamValue::Bool(!b),
            _ => self.default,
        }
    }

    /// The value as the menu shows it.
    pub fn show(&self, v: ParamValue) -> String {
        match (self.kind, v) {
            (ParamKind::Choice(o), ParamValue::Choice(i)) => o.get(i).copied().unwrap_or("?").to_string(),
            (_, v) => v.to_string(),
        }
    }
}

impl fmt::Display for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamValue::Int(i) => write!(f, "{i}"),
            ParamValue::Float(v) => write!(f, "{v:.2}"),
            ParamValue::Choice(i) => write!(f, "#{i}"),
            ParamValue::Bool(b) => f.write_str(if *b { "on" } else { "off" }),
        }
    }
}

/// One Developer action. Runs on the overlay's input thread, never on the title's threads.
pub trait Action: Send {
    fn name(&self) -> &str;
    /// A heading the page groups actions under.
    fn group(&self) -> &str {
        "General"
    }
    fn params(&self) -> &[Param] {
        &[]
    }
    /// Disabled while [`online`] (default: yes; only actions that never touch the title's world say no).
    fn offline_only(&self) -> bool {
        true
    }
    /// Bound to the title (a [`Placeholder`] is not).
    fn bound(&self) -> bool {
        true
    }
    /// Run with one value per parameter; a short line for the menu, or why it failed.
    fn run(&mut self, args: &[ParamValue]) -> Result<String, String>;
}

/// An action named by the research but not bound to the title yet: listed, dimmed, refuses to run.
pub struct Placeholder {
    pub name: &'static str,
    pub group: &'static str,
    pub params: &'static [Param],
}

impl Action for Placeholder {
    fn name(&self) -> &str {
        self.name
    }
    fn group(&self) -> &str {
        self.group
    }
    fn params(&self) -> &[Param] {
        self.params
    }
    fn bound(&self) -> bool {
        false
    }
    fn run(&mut self, _: &[ParamValue]) -> Result<String, String> {
        Err(format!("{}: not bound to the title yet (docs/research/devmenu.md)", self.name))
    }
}

/// The page's list, in registration order, and the probe that gates it.
pub struct Registry {
    actions: Vec<Box<dyn Action>>,
    online: fn() -> bool,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new(online)
    }
}

/// Why an action can't run now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    Online,
    Unbound,
}

impl Registry {
    pub fn new(online: fn() -> bool) -> Registry {
        Registry { actions: Vec::new(), online }
    }

    /// The placeholders the page shows until the research binds them (the four the task names).
    pub fn with_placeholders(mut self) -> Registry {
        const PLACES: &[&str] = &["(from research)"];
        const VEHICLES: &[&str] = &["(from research)"];
        const WEATHER: &[&str] = &["(from research)"];
        static TELEPORT: [Param; 1] = [Param::choice("to", PLACES, 0)];
        static SPAWN: [Param; 1] = [Param::choice("vehicle", VEHICLES, 0)];
        static TIME: [Param; 2] = [Param::int("hour", 0, 23, 1, 12), Param::int("minute", 0, 45, 15, 0)];
        static SKY: [Param; 1] = [Param::choice("weather", WEATHER, 0)];
        self.register(Box::new(Placeholder { name: "Teleport", group: "World", params: &TELEPORT }));
        self.register(Box::new(Placeholder { name: "Spawn vehicle", group: "World", params: &SPAWN }));
        self.register(Box::new(Placeholder { name: "Set time of day", group: "World", params: &TIME }));
        self.register(Box::new(Placeholder { name: "Set weather", group: "World", params: &SKY }));
        self
    }

    pub fn register(&mut self, a: Box<dyn Action>) {
        self.actions.push(a);
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&dyn Action> {
        self.actions.get(i).map(|a| a.as_ref())
    }

    pub fn online(&self) -> bool {
        (self.online)()
    }

    /// Why action `i` can't run now, if it can't.
    pub fn blocked(&self, i: usize) -> Option<Blocked> {
        let a = self.actions.get(i)?;
        if a.offline_only() && self.online() {
            Some(Blocked::Online)
        } else if !a.bound() {
            Some(Blocked::Unbound)
        } else {
            None
        }
    }

    /// Run action `i` with `args` (one per parameter; missing ones take their default).
    pub fn run(&mut self, i: usize, args: &[ParamValue]) -> Result<String, String> {
        let name = self.actions.get(i).ok_or("no such action")?.name().to_string();
        match self.blocked(i) {
            Some(Blocked::Online) => return Err(format!("{name}: disabled while online")),
            Some(Blocked::Unbound) => return Err(format!("{name}: not bound to the title yet")),
            None => {}
        }
        let a = &mut self.actions[i];
        let full: Vec<ParamValue> = a.params().iter().enumerate().map(|(k, p)| args.get(k).copied().unwrap_or(p.default)).collect();
        a.run(&full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct Count(Arc<std::sync::Mutex<Vec<ParamValue>>>, bool);
    impl Action for Count {
        fn name(&self) -> &str {
            "Count"
        }
        fn params(&self) -> &[Param] {
            static P: [Param; 2] = [Param::int("n", 0, 10, 2, 4), Param::bool("loud", false)];
            &P
        }
        fn offline_only(&self) -> bool {
            self.1
        }
        fn run(&mut self, args: &[ParamValue]) -> Result<String, String> {
            self.0.lock().unwrap().extend_from_slice(args);
            Ok("counted".into())
        }
    }

    static ONLINE: AtomicBool = AtomicBool::new(false);
    fn probe() -> bool {
        ONLINE.load(Ordering::Relaxed)
    }

    #[test]
    fn the_stub_probe_is_offline() {
        assert!(!online());
        assert!(!Registry::default().online());
    }

    #[test]
    fn actions_run_with_defaults_filled_and_are_gated_by_the_probe() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut r = Registry::new(probe);
        r.register(Box::new(Count(seen.clone(), true)));
        r.register(Box::new(Count(seen.clone(), false)));
        ONLINE.store(false, Ordering::Relaxed);
        assert_eq!(r.blocked(0), None);
        assert_eq!(r.run(0, &[ParamValue::Int(8)]), Ok("counted".into()));
        assert_eq!(*seen.lock().unwrap(), vec![ParamValue::Int(8), ParamValue::Bool(false)]);
        ONLINE.store(true, Ordering::Relaxed);
        assert_eq!(r.blocked(0), Some(Blocked::Online));
        assert!(r.run(0, &[]).unwrap_err().contains("online"));
        assert_eq!(seen.lock().unwrap().len(), 2, "nothing ran");
        // an action that never touches the world stays enabled online
        assert_eq!(r.blocked(1), None);
        assert!(r.run(1, &[]).is_ok());
        ONLINE.store(false, Ordering::Relaxed);
        assert!(r.run(7, &[]).is_err());
    }

    #[test]
    fn placeholders_are_listed_but_refuse_to_run() {
        let mut r = Registry::new(|| false).with_placeholders();
        assert_eq!(r.len(), 4);
        let names: Vec<&str> = (0..r.len()).map(|i| r.get(i).unwrap().name()).collect();
        assert_eq!(names, ["Teleport", "Spawn vehicle", "Set time of day", "Set weather"]);
        assert_eq!(r.blocked(2), Some(Blocked::Unbound));
        assert!(r.run(2, &[]).unwrap_err().contains("not bound"));
    }

    #[test]
    fn params_step_and_show() {
        let p = Param::int("hour", 0, 23, 1, 12);
        assert_eq!(p.step(ParamValue::Int(23), 1), ParamValue::Int(23));
        assert_eq!(p.step(ParamValue::Int(0), -1), ParamValue::Int(0));
        let c = Param::choice("w", &["sun", "rain", "fog"], 0);
        assert_eq!(c.step(ParamValue::Choice(0), -1), ParamValue::Choice(2));
        assert_eq!(c.show(ParamValue::Choice(1)), "rain");
        let f = Param::float("speed", 0.0, 2.0, 0.25, 1.0);
        assert_eq!(f.step(ParamValue::Float(2.0), 1), ParamValue::Float(2.0));
        assert_eq!(f.show(ParamValue::Float(0.5)), "0.50");
        assert_eq!(Param::bool("b", false).step(ParamValue::Bool(false), -1), ParamValue::Bool(true));
    }
}
