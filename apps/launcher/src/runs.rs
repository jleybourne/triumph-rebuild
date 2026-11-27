//! Runs & test matrix: running chosen profiles one after another, each for a set time, each in its own
//! session. The plan is plain state so it is tested without a process; the UI starts and stops the layer.
//!
//! The layer is told the run length and where measuring starts (`REBUILD_RUN_SECONDS`,
//! `REBUILD_MEASURE_FROM`) and is expected to exit by itself; the launcher stops it after the length plus
//! [`GRACE`] seconds if it hasn't.

/// Seconds past the run length before the launcher stops a run itself.
pub const GRACE: f64 = 20.0;

pub const LENGTHS: [(u64, &str); 3] = [(60, "60 s"), (180, "3 min"), (600, "10 min")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureFrom {
    TitleStart,
    AfterLoading,
}

impl MeasureFrom {
    pub fn key(self) -> &'static str {
        match self {
            MeasureFrom::TitleStart => "title_start",
            MeasureFrom::AfterLoading => "after_loading",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            MeasureFrom::TitleStart => "title start",
            MeasureFrom::AfterLoading => "after loading",
        }
    }
}

/// What the UI should do now.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// start this built-in profile
    Start(String),
    /// a run is going; nothing to do
    Wait,
    /// the run is past its length and grace: stop it
    Stop,
    /// every profile has run
    Done,
}

#[derive(Debug, Clone)]
pub struct Plan {
    /// the matrix id (a session stamp), recorded in each run's `session.toml`
    pub id: String,
    pub profiles: Vec<String>,
    pub run_seconds: u64,
    pub measure_from: MeasureFrom,
    next: usize,
    /// the profile running and when it started (seconds on the UI's clock)
    pub current: Option<(String, f64)>,
    pub failed: Vec<(String, String)>,
}

impl Plan {
    pub fn new(id: String, profiles: Vec<String>, run_seconds: u64, measure_from: MeasureFrom) -> Plan {
        Plan { id, profiles, run_seconds, measure_from, next: 0, current: None, failed: Vec::new() }
    }

    /// Advance: `running` is whether the layer is still running.
    pub fn tick(&mut self, now: f64, running: bool) -> Step {
        if let Some((_, started)) = &self.current {
            if running {
                return if now - started > self.run_seconds as f64 + GRACE { Step::Stop } else { Step::Wait };
            }
            self.current = None;
        }
        if running {
            // something else is running (a launch from Play): wait for it
            return Step::Wait;
        }
        match self.profiles.get(self.next).cloned() {
            Some(p) => {
                self.next += 1;
                self.current = Some((p.clone(), now));
                Step::Start(p)
            }
            None => Step::Done,
        }
    }

    /// The start of the current run failed: record why and move on.
    pub fn start_failed(&mut self, why: String) {
        if let Some((p, _)) = self.current.take() {
            self.failed.push((p, why));
        }
    }

    /// (finished or running, total)
    pub fn progress(&self) -> (usize, usize) {
        (self.next, self.profiles.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_run_in_turn_and_overlong_runs_are_stopped() {
        let mut p = Plan::new("m".into(), vec!["series-x".into(), "low-pc".into()], 60, MeasureFrom::AfterLoading);
        assert_eq!(p.tick(0.0, false), Step::Start("series-x".into()));
        assert_eq!(p.progress(), (1, 2));
        assert_eq!(p.tick(30.0, true), Step::Wait);
        assert_eq!(p.tick(79.0, true), Step::Wait, "inside the grace");
        assert_eq!(p.tick(81.0, true), Step::Stop);
        // it exited: the next one starts
        assert_eq!(p.tick(82.0, false), Step::Start("low-pc".into()));
        // its start failed
        p.start_failed("rebuild.exe not found".into());
        assert_eq!(p.failed, vec![("low-pc".to_string(), "rebuild.exe not found".to_string())]);
        assert_eq!(p.tick(83.0, false), Step::Done);
    }

    #[test]
    fn a_launch_from_play_is_waited_for() {
        let mut p = Plan::new("m".into(), vec!["stress-cpu".into()], 60, MeasureFrom::TitleStart);
        assert_eq!(p.tick(0.0, true), Step::Wait);
        assert_eq!(p.tick(1.0, false), Step::Start("stress-cpu".into()));
        assert_eq!(MeasureFrom::TitleStart.key(), "title_start");
    }
}
