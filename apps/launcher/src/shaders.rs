//! "Preparing shaders": the launcher builds each game's shader library from the user's own copy of the
//! game, on first Play or when the game is added (`rebuild_pipelines::shaderlib`, owner decision 2026-11-28).
//! The build runs on a thread of the launcher; its progress is read from a shared [`Control`]. Closing the
//! launcher, Cancel or "Play now" stops it at the next program; what is on disk stays, and the next build
//! resumes there. Without a library (or with half of one) the layer translates programs on demand, so a
//! launch is never blocked by it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub use rebuild_pipelines::shaderlib::{Phase, Progress, Report, Status};
use rebuild_pipelines::shaderlib::{self, Config, Control};
use rebuild_storage::GameStore;

/// Worker threads for a build: `REBUILD_SHADER_WORKERS`, else logical processors less two.
pub fn default_workers() -> usize {
    std::env::var("REBUILD_SHADER_WORKERS").ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or_else(shaderlib::default_workers)
}

/// The most workers the launcher offers (each runs one DXC process).
pub fn max_workers() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get()).max(1)
}

/// What a game needs before Play.
#[derive(Debug, Clone, PartialEq)]
pub enum Need {
    /// the library is current
    Ready(Status),
    /// it must be built (or finished, or rebuilt)
    Build(Status),
    /// it cannot be built here; the layer translates on demand. The reason.
    OnDemand(String),
}

impl Need {
    pub fn status(&self) -> Option<&Status> {
        match self {
            Need::Ready(s) | Need::Build(s) => Some(s),
            Need::OnDemand(_) => None,
        }
    }
}

/// Where a game's library stands. `dxc`: the compiler the launcher found, or why there is none.
pub fn need(game_dir: &Path, store: &GameStore, dxc: &Result<PathBuf, String>) -> Need {
    let corpus = game_dir.join(shaderlib::CORPUS);
    if !corpus.is_file() {
        return Need::OnDemand(format!("no {} in the game folder", shaderlib::CORPUS));
    }
    let st = shaderlib::status(&store.shaders_dir(), &corpus, &store.build, shaderlib::VERSION);
    if st.ready() {
        return Need::Ready(st);
    }
    if let Err(e) = dxc {
        return Need::OnDemand(e.clone());
    }
    Need::Build(st)
}

/// DXC for the launcher: bundled beside it (`<exe dir>\dxc\`), `REBUILD_DXC`, or the Windows SDK.
pub fn find_dxc() -> Result<PathBuf, String> {
    rebuild_pipelines::dxc::locate().map(|d| d.exe)
}

/// A build in progress, for one game.
pub struct Job {
    pub game: String,
    pub dir: PathBuf,
    pub workers: usize,
    ctl: Arc<Control>,
    handle: Option<JoinHandle<Result<Report, String>>>,
}

impl Job {
    /// Starts (or resumes) the build of `store`'s library from `game_dir\shaders.rpf`.
    pub fn start(game_id: &str, game_dir: &Path, store: &GameStore, dxc: PathBuf, workers: usize) -> Job {
        let mut cfg = Config::for_game(game_dir, store, dxc);
        cfg.workers = workers.max(1);
        Job::start_with(game_id, cfg)
    }

    pub fn start_with(game_id: &str, cfg: Config) -> Job {
        let ctl = Arc::new(Control::new());
        let c = ctl.clone();
        let dir = cfg.dir.clone();
        let workers = cfg.workers;
        let handle = std::thread::Builder::new().name("shader-library".into()).spawn(move || shaderlib::build(&cfg, &c)).ok();
        Job { game: game_id.to_string(), dir, workers, ctl, handle }
    }

    pub fn progress(&self) -> Progress {
        self.ctl.progress()
    }

    /// Stop at the next program (resumable).
    pub fn cancel(&self) {
        self.ctl.cancel();
    }

    pub fn cancelled(&self) -> bool {
        self.ctl.cancelled()
    }

    pub fn finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// The outcome, once [`Job::finished`].
    pub fn take_result(&mut self) -> Option<Result<Report, String>> {
        if !self.finished() {
            return None;
        }
        Some(self.handle.take().map_or_else(|| Err("the build thread did not start".into()), |h| h.join().unwrap_or_else(|_| Err("the build panicked".into()))))
    }

    /// Cancel and wait until it has stopped (before the cache is cleared under it).
    pub fn stop(mut self) -> Option<Result<Report, String>> {
        self.cancel();
        self.handle.take().map(|h| h.join().unwrap_or_else(|_| Err("the build panicked".into())))
    }
}

/// "12 min", "40 s", "1 h 05 min".
pub fn eta_text(eta: Option<Duration>) -> String {
    let Some(d) = eta else { return "estimating…".into() };
    let s = d.as_secs();
    if s < 60 {
        format!("about {s} s left")
    } else if s < 3600 {
        format!("about {} min left", s.div_ceil(60))
    } else {
        format!("about {} h {:02} min left", s / 3600, (s % 3600) / 60)
    }
}

/// One line for a status (Storage panel, the Play band).
pub fn status_line(need: &Need, job: Option<&Progress>) -> String {
    if let Some(p) = job {
        return match p.phase {
            Phase::Starting | Phase::Reading => "Preparing shaders: reading the game's shaders…".into(),
            Phase::Translating => format!("Preparing shaders: {} of {} · {}", p.done, p.total, eta_text(p.eta)),
            Phase::Writing => "Preparing shaders: writing the library…".into(),
            Phase::Done => "Shaders ready".into(),
            Phase::Paused => format!("Shaders paused at {} of {}", p.done, p.total),
            Phase::Failed => format!("Shader library failed: {}", p.error.clone().unwrap_or_default()),
        };
    }
    match need {
        Need::Ready(Status::Ready(m)) => format!("Shaders ready: {} programs ({:.0} MB)", m.in_library, m.library_bytes as f64 / 1e6),
        Need::Ready(_) => "Shaders ready".into(),
        Need::Build(Status::Missing) => "Shaders not prepared yet".into(),
        Need::Build(Status::Partial { done, total }) => format!("Shaders paused at {done} of {total}"),
        Need::Build(Status::Stale(why)) => format!("Shaders out of date ({why})"),
        Need::Build(Status::Ready(_)) => "Shaders ready".into(),
        Need::OnDemand(why) => format!("Shaders translated while playing ({why})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_game_without_its_shader_archive_or_dxc_plays_on_demand() {
        let d = crate::temp_dir("shaders-need");
        let st = rebuild_storage::Storage::resolve_with(&rebuild_storage::Env { data_dir: Some(d.join("data")), temp: d.clone(), ..Default::default() });
        let store = st.game("some.game").with_build("1.0");
        let game = d.join("game");
        std::fs::create_dir_all(&game).unwrap();
        assert!(matches!(need(&game, &store, &Ok(PathBuf::from("dxc.exe"))), Need::OnDemand(w) if w.contains("shaders.rpf")));
        std::fs::write(game.join("shaders.rpf"), b"SGD5").unwrap();
        assert_eq!(need(&game, &store, &Ok(PathBuf::from("dxc.exe"))), Need::Build(Status::Missing));
        assert!(matches!(need(&game, &store, &Err("no DXC: put …".into())), Need::OnDemand(w) if w.contains("no DXC")));
        assert_eq!(status_line(&Need::Build(Status::Missing), None), "Shaders not prepared yet");
        let p = Progress { phase: Phase::Translating, done: 10, total: 40, eta: Some(Duration::from_secs(150)), ..Progress::default() };
        assert_eq!(status_line(&Need::Build(Status::Missing), Some(&p)), "Preparing shaders: 10 of 40 · about 3 min left");
        assert_eq!(eta_text(Some(Duration::from_secs(3900))), "about 1 h 05 min left");
    }

    /// A job over a corpus that is not a shader archive: it finishes (an empty library), and the game's
    /// status is then ready; clearing the cache makes it missing again, and the next build is clean.
    #[test]
    fn a_job_runs_to_the_end_and_a_cleared_cache_builds_again() {
        let d = crate::temp_dir("shaders-job");
        let st = rebuild_storage::Storage::resolve_with(&rebuild_storage::Env { data_dir: Some(d.join("data")), temp: d.clone(), ..Default::default() });
        let store = st.game("some.game").with_build("1.0");
        let game = d.join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("shaders.rpf"), b"no programs in here").unwrap();
        // any file stands in for DXC: nothing is compiled
        let dxc = d.join("dxc.exe");
        std::fs::write(&dxc, b"x").unwrap();
        for round in 0..2 {
            let mut job = Job::start("some.game", &game, &store, dxc.clone(), 2);
            while !job.finished() {
                std::thread::sleep(Duration::from_millis(5));
            }
            let r = job.take_result().unwrap().unwrap();
            assert!(r.complete && r.programs == 0, "round {round}: {r:?}");
            assert!(matches!(need(&game, &store, &Ok(dxc.clone())), Need::Ready(_)), "round {round}");
            store.clear_cache(None).unwrap();
            assert_eq!(need(&game, &store, &Ok(dxc.clone())), Need::Build(Status::Missing), "round {round}: cleared");
        }
    }

    /// A library left by another translator version is stale and the launcher rebuilds it; a part-built one
    /// (the launcher closed mid-build) is finished. Either way the game is then ready, of this translator.
    #[test]
    fn a_stale_or_part_built_library_is_built_again_by_the_launcher() {
        use rebuild_pipelines::shaderlib::{Manifest, MANIFEST, VERSION};
        let d = crate::temp_dir("shaders-stale");
        let st = rebuild_storage::Storage::resolve_with(&rebuild_storage::Env { data_dir: Some(d.join("data")), temp: d.clone(), ..Default::default() });
        let store = st.game("some.game").with_build("1.0");
        let game = d.join("game");
        std::fs::create_dir_all(&game).unwrap();
        let corpus = game.join("shaders.rpf");
        std::fs::write(&corpus, b"no programs in here").unwrap();
        let dxc = d.join("dxc.exe");
        std::fs::write(&dxc, b"x").unwrap();
        let (corpus_size, corpus_modified) = rebuild_pipelines::shaderlib::corpus_stat(&corpus).unwrap();
        let dir = store.shaders_dir();
        std::fs::create_dir_all(&dir).unwrap();
        for (translator, state, want) in [("an older translator", "complete", "translator"), (VERSION, "building", "paused")] {
            let m = Manifest { format: 1, translator: translator.into(), game_build: "1.0".into(), corpus_size, corpus_modified, state: state.into(), programs: 3, ..Manifest::default() };
            std::fs::write(dir.join(MANIFEST), m.to_toml()).unwrap();
            std::fs::write(dir.join("library.bin"), b"an old library").unwrap();
            let n = need(&game, &store, &Ok(dxc.clone()));
            assert!(matches!(&n, Need::Build(Status::Stale(_)) | Need::Build(Status::Partial { .. })), "{translator}/{state}: {n:?}");
            assert!(status_line(&n, None).contains(if want == "paused" { "paused" } else { "translator" }), "{}", status_line(&n, None));
            let mut job = Job::start("some.game", &game, &store, dxc.clone(), 2);
            while !job.finished() {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(job.take_result().unwrap().unwrap().complete);
            assert!(matches!(need(&game, &store, &Ok(dxc.clone())), Need::Ready(_)), "{translator}/{state}: rebuilt");
            assert_eq!(Manifest::read(&dir).unwrap().translator, VERSION);
        }
    }
}
