//! Scripted pad input for unattended runs: `REBUILD_INPUT_SCRIPT`.
//!
//! A comma-separated list of `<when>:<what>` steps, for example
//! `40s:A,45s:A,50s:Down,52s:A,60s:cap` or `1200f:A,1300f:Down/8f`:
//!
//! - `<when>`: seconds from the start of the process (`40s`, `40.5s`) or a presented frame number (`1200f`).
//! - `<what>`: buttons joined by `+` (`A`, `View+Menu`), pressed then released, or `cap` (capture the next
//!   shown frame). An optional `/<n>ms` or `/<n>f` sets how long the press is held (default 200 ms: several
//!   of the title's frames at 30 fps, and long enough for a title that polls input on a slower thread).
//! - Names: `A B X Y Menu View Up Down Left Right LB RB LS RS` (buttons), `LT RT` (full triggers),
//!   `LUp LDown LLeft LRight RUp RDown RLeft RRight` (sticks at full travel). Case does not matter;
//!   `Start` = `Menu`, `Back` = `View`, `DUp`.. = `Up`...
//!
//! The press is added to what the title reads (the first connected pad, else the keyboard's stand-in pad);
//! real devices keep working alongside. Each step runs once. Off (no variable): nothing changes.
//!
//! The live control channel (`REBUILD_INPUT_PIPE`, `crates/runtime/src/control.rs`) takes the same `<what>` text
//! ([`parse_action`]) and adds it as a step due at once ([`Script::push`]).

use crate::pad::{buttons as b, PadState};
use crate::source::Frame;

/// When a step starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    /// Seconds from the start of the process.
    Seconds(f64),
    /// A presented frame number (from 1).
    Frame(u64),
}

/// How long a press is held.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hold {
    Ms(u64),
    Frames(u64),
}

pub const DEFAULT_HOLD: Hold = Hold::Ms(200);

/// What a step does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Press { pad: PadState, hold: Hold },
    Capture,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub at: At,
    pub action: Action,
    /// The text of the step, for the run log.
    pub text: String,
}

/// A step that started, with when (seconds, frame), for the run log.
#[derive(Clone, Debug, PartialEq)]
pub struct Fired {
    pub text: String,
    pub seconds: f64,
    pub frame: u64,
}

/// What one poll adds to the title's input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Poll {
    /// Held now; `None` when no press is active.
    pub pad: Option<PadState>,
    /// A `cap` step became due.
    pub capture: bool,
    /// Steps that started at this poll.
    pub fired: Vec<Fired>,
}

#[derive(Clone, Debug, Default)]
pub struct Script {
    steps: Vec<Step>,
    /// Per step: when it started (seconds, frame), once it has.
    started: Vec<Option<(f64, u64)>>,
}

fn name(n: &str) -> Option<PadState> {
    let mut p = PadState::default();
    let lower = n.to_ascii_lowercase();
    let bit = match lower.as_str() {
        "a" => b::A,
        "b" => b::B,
        "x" => b::X,
        "y" => b::Y,
        "menu" | "start" => b::MENU,
        "view" | "back" => b::VIEW,
        "up" | "dup" => b::DPAD_UP,
        "down" | "ddown" => b::DPAD_DOWN,
        "left" | "dleft" => b::DPAD_LEFT,
        "right" | "dright" => b::DPAD_RIGHT,
        "lb" => b::LEFT_SHOULDER,
        "rb" => b::RIGHT_SHOULDER,
        "ls" => b::LEFT_THUMB,
        "rs" => b::RIGHT_THUMB,
        _ => 0,
    };
    if bit != 0 {
        p.buttons = bit;
        return Some(p);
    }
    match lower.as_str() {
        "lt" => p.left_trigger = 1.0,
        "rt" => p.right_trigger = 1.0,
        "lup" => p.left_y = 1.0,
        "ldown" => p.left_y = -1.0,
        "lleft" => p.left_x = -1.0,
        "lright" => p.left_x = 1.0,
        "rup" => p.right_y = 1.0,
        "rdown" => p.right_y = -1.0,
        "rleft" => p.right_x = -1.0,
        "rright" => p.right_x = 1.0,
        _ => return None,
    }
    Some(p)
}

/// `b` added to `a`: buttons or'ed, the larger trigger, the stick of larger travel.
fn merge(a: PadState, b: PadState) -> PadState {
    let pick = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: a.left_trigger.max(b.left_trigger),
        right_trigger: a.right_trigger.max(b.right_trigger),
        left_x: pick(a.left_x, b.left_x),
        left_y: pick(a.left_y, b.left_y),
        right_x: pick(a.right_x, b.right_x),
        right_y: pick(a.right_y, b.right_y),
    }
}

fn number<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, String> {
    s.trim().parse().map_err(|_| format!("{what} '{s}' is not a number"))
}

/// One step's `<what>`: buttons joined by `+` with an optional `/<n>ms` or `/<n>f` hold, or `cap`. The live control
/// channel (`REBUILD_INPUT_PIPE`) takes the same text for its `press`.
pub fn parse_action(what: &str) -> Result<Action, String> {
    let (what, hold) = match what.split_once('/') {
        None => (what.trim(), DEFAULT_HOLD),
        Some((w, h)) => {
            let h = h.trim().to_ascii_lowercase();
            let hold = if let Some(ms) = h.strip_suffix("ms") {
                Hold::Ms(number(ms, "hold")?)
            } else if let Some(f) = h.strip_suffix('f') {
                Hold::Frames(number(f, "hold")?)
            } else {
                return Err("the hold needs a unit, ms or f".into());
            };
            (w.trim(), hold)
        }
    };
    if what.eq_ignore_ascii_case("cap") || what.eq_ignore_ascii_case("capture") {
        return Ok(Action::Capture);
    }
    let mut pad = PadState::default();
    for n in what.split('+').map(str::trim) {
        pad = merge(pad, name(n).ok_or_else(|| format!("no button named '{n}'"))?);
    }
    Ok(Action::Press { pad, hold })
}

impl Script {
    /// Parse a script; an empty string gives an empty script.
    pub fn parse(text: &str) -> Result<Script, String> {
        let mut steps = Vec::new();
        for raw in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (when, what) = raw.split_once(':').ok_or_else(|| format!("step '{raw}': want <when>:<what>"))?;
            let when = when.trim().to_ascii_lowercase();
            let at = if let Some(s) = when.strip_suffix('s') {
                let v: f64 = number(s, "seconds")?;
                if !(v.is_finite() && v >= 0.0) {
                    return Err(format!("step '{raw}': seconds must be 0 or more"));
                }
                At::Seconds(v)
            } else if let Some(f) = when.strip_suffix('f') {
                At::Frame(number(f, "frame")?)
            } else {
                return Err(format!("step '{raw}': the time needs a unit, s (seconds) or f (frame)"));
            };
            let action = parse_action(what).map_err(|e| format!("step '{raw}': {e}"))?;
            steps.push(Step { at, action, text: raw.to_string() });
        }
        let started = vec![None; steps.len()];
        Ok(Script { steps, started })
    }

    /// `REBUILD_INPUT_SCRIPT`, if set and not empty. A script that does not parse is reported and ignored.
    pub fn from_env() -> Option<Script> {
        let text = std::env::var("REBUILD_INPUT_SCRIPT").ok()?;
        match Script::parse(&text) {
            Ok(s) if !s.is_empty() => Some(s),
            Ok(_) => None,
            Err(e) => {
                eprintln!("input     REBUILD_INPUT_SCRIPT ignored: {e}");
                None
            }
        }
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Add a step (the live control channel adds presses due at once: `At::Seconds(0.0)`).
    pub fn push(&mut self, at: At, action: Action, text: &str) {
        self.steps.push(Step { at, action, text: text.to_string() });
        self.started.push(None);
    }

    /// Drop the steps that have started and are no longer held at `seconds` / `frame`.
    pub fn prune(&mut self, seconds: f64, frame: u64) {
        let done: Vec<bool> = self.steps.iter().zip(&self.started).map(|(s, st)| st.is_some_and(|st| !held(s, st, seconds, frame))).collect();
        let mut i = 0;
        self.steps.retain(|_| {
            i += 1;
            !done[i - 1]
        });
        let mut i = 0;
        self.started.retain(|_| {
            i += 1;
            !done[i - 1]
        });
    }

    /// Every step has started and no press is held at `seconds` / `frame`.
    pub fn finished(&self, seconds: f64, frame: u64) -> bool {
        self.steps.iter().zip(&self.started).all(|(s, st)| st.is_some_and(|st| !held(s, st, seconds, frame)))
    }

    /// Advance to `seconds` from the process start and `frame` presented frames.
    pub fn poll(&mut self, seconds: f64, frame: u64) -> Poll {
        let mut out = Poll::default();
        for (step, started) in self.steps.iter().zip(self.started.iter_mut()) {
            if started.is_none() {
                let due = match step.at {
                    At::Seconds(s) => seconds >= s,
                    At::Frame(f) => frame >= f,
                };
                if !due {
                    continue;
                }
                *started = Some((seconds, frame));
                out.fired.push(Fired { text: step.text.clone(), seconds, frame });
                if step.action == Action::Capture {
                    out.capture = true;
                }
            }
            let st = started.expect("set above");
            if let Action::Press { pad, .. } = step.action {
                if held(step, st, seconds, frame) {
                    out.pad = Some(out.pad.map_or(pad, |p| merge(p, pad)));
                }
            }
        }
        out
    }
}

fn held(step: &Step, (s0, f0): (f64, u64), seconds: f64, frame: u64) -> bool {
    match step.action {
        Action::Capture => false,
        Action::Press { hold: Hold::Ms(ms), .. } => (seconds - s0) * 1000.0 < ms as f64,
        Action::Press { hold: Hold::Frames(n), .. } => frame < f0 + n,
    }
}

/// Add a held press to the title's frame: the first connected pad, else the stand-in pad (which exists
/// whenever no pad does; it is made connected if it somehow is not).
pub fn apply(frame: &mut Frame, pad: PadState) {
    if let Some(p) = frame.pads.iter_mut().flatten().next() {
        *p = merge(*p, pad);
        return;
    }
    let s = frame.stand_in.get_or_insert(PadState::default());
    *s = merge(*s, pad);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::NEUTRAL;

    #[test]
    fn parses_times_frames_holds_and_names() {
        let s = Script::parse(" 40s:A, 45.5s:View+Menu , 1200f:Down/8f, 50s:LT/500ms, 60s:cap, 61s:lup").unwrap();
        let st = s.steps();
        assert_eq!(st.len(), 6);
        assert_eq!(st[0].at, At::Seconds(40.0));
        assert_eq!(st[0].action, Action::Press { pad: PadState { buttons: b::A, ..NEUTRAL }, hold: DEFAULT_HOLD });
        assert_eq!(st[1].action, Action::Press { pad: PadState { buttons: b::VIEW | b::MENU, ..NEUTRAL }, hold: DEFAULT_HOLD });
        assert_eq!(st[2].at, At::Frame(1200));
        assert_eq!(st[2].action, Action::Press { pad: PadState { buttons: b::DPAD_DOWN, ..NEUTRAL }, hold: Hold::Frames(8) });
        assert_eq!(st[3].action, Action::Press { pad: PadState { left_trigger: 1.0, ..NEUTRAL }, hold: Hold::Ms(500) });
        assert_eq!(st[4].action, Action::Capture);
        assert_eq!(st[5].action, Action::Press { pad: PadState { left_y: 1.0, ..NEUTRAL }, hold: DEFAULT_HOLD });
        assert!(Script::parse("").unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_steps() {
        for bad in ["40:A", "40s", "xs:A", "40s:Q", "40s:A/5", "-1s:A", "40s:A+Nope"] {
            assert!(Script::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_press_is_held_then_released_once() {
        let mut s = Script::parse("1s:A,1.5s:cap,10f:B/2f").unwrap();
        assert_eq!(s.poll(0.5, 0), Poll::default());
        let p = s.poll(1.0, 3);
        assert_eq!(p.pad.unwrap().buttons, b::A);
        assert_eq!(p.fired.len(), 1);
        assert_eq!(s.poll(1.1, 4).pad.unwrap().buttons, b::A, "held 200 ms");
        assert!(s.poll(1.1, 4).fired.is_empty(), "fires once");
        assert_eq!(s.poll(1.25, 5).pad, None, "released");
        let p = s.poll(1.6, 6);
        assert!(p.capture && p.pad.is_none());
        assert!(!s.poll(1.7, 7).capture, "captures once");
        assert_eq!(s.poll(2.0, 10).pad.unwrap().buttons, b::B);
        assert_eq!(s.poll(2.0, 11).pad.unwrap().buttons, b::B);
        assert!(!s.finished(2.0, 11));
        assert_eq!(s.poll(2.0, 12).pad, None, "two frames");
        assert!(s.finished(2.0, 12));
    }

    #[test]
    fn live_presses_fire_at_once_and_finished_ones_are_dropped() {
        let mut s = Script::default();
        s.push(At::Seconds(0.0), parse_action("A/2f").unwrap(), "A/2f");
        assert_eq!(parse_action("cap").unwrap(), Action::Capture);
        assert!(parse_action("Nope").is_err() && parse_action("A/5").is_err());
        let p = s.poll(30.0, 100);
        assert_eq!(p.pad.unwrap().buttons, b::A);
        assert_eq!(p.fired[0].text, "A/2f");
        s.push(At::Seconds(0.0), parse_action("Down+LT/100ms").unwrap(), "Down+LT/100ms");
        let p = s.poll(30.05, 101);
        assert_eq!(p.pad.unwrap().buttons, b::A | b::DPAD_DOWN, "both held");
        s.prune(30.05, 102);
        assert_eq!(s.steps().len(), 1, "the A press ended at frame 102");
        s.prune(30.2, 103);
        assert!(s.is_empty());
        assert_eq!(s.poll(30.3, 104), Poll::default());
    }

    #[test]
    fn overlapping_presses_merge() {
        let mut s = Script::parse("1s:A/1000ms,1.2s:Down").unwrap();
        assert_eq!(s.poll(1.3, 0).pad.unwrap().buttons, b::A | b::DPAD_DOWN);
    }

    #[test]
    fn applied_to_the_first_pad_else_the_stand_in() {
        let press = PadState { buttons: b::A, ..NEUTRAL };
        let mut f = Frame { stand_in: Some(PadState { buttons: b::B, ..NEUTRAL }), ..Frame::default() };
        apply(&mut f, press);
        assert_eq!(f.stand_in.unwrap().buttons, b::A | b::B, "the real key still counts");
        let mut f = Frame::default();
        f.pads[2] = Some(PadState { left_x: 0.5, ..NEUTRAL });
        apply(&mut f, PadState { left_x: -1.0, ..press });
        assert_eq!(f.pads[2].unwrap().buttons, b::A);
        assert_eq!(f.pads[2].unwrap().left_x, -1.0);
        assert_eq!(f.stand_in, None, "no stand-in added beside a real pad");
        let mut f = Frame::default();
        apply(&mut f, press);
        assert_eq!(f.stand_in.unwrap().buttons, b::A);
    }
}
