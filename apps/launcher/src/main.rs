//! Triumph Rebuild launcher (L6, owner-approved Direction B): a top bar with Play, Profiles, Settings,
//! Runs, Mods and Diagnostics; dark and light themes; a session folder per launch. Every settings widget
//! is generated from `rebuild_config::REGISTRY` (`docs/design/launcher.md`); the look is `src/ui`
//! (tokens in `ui/theme.rs`).
//!
//! `rebuild-launcher --check` prints the system check for the selected game without opening a window.
//! `rebuild-launcher --shots <dir> [--size WxH]` renders every screen in both themes from a fixture
//! (placeholder names, demo data) to PNGs in `<dir>`, then exits.
//! `rebuild-launcher --matrix <dir> [--profiles a,b,...]` writes one `rebuild.toml` per built-in test
//! profile (all of them, or the named ones) into `<dir>`, from the selected game's settings, and exits.
//! Nothing is run: the files are for running the same build under each profile in turn.
//! `REBUILD_MEASURE_LOCK=<file>`: while that file exists, the check creates no D3D12 device.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod screens;
mod ui;

use eframe::egui::{self, Align2, Color32, CornerRadius, Id, Key, Pos2, Rect, Sense, Vec2};
use rebuild_config::profiles::Active;
use rebuild_config::{Config, Value, BUILTIN_PROFILES};
use rebuild_launcher::{
    frameline::{self, Frame, Summary},
    gamemeta,
    home::{self, Home},
    library::{self, Detection, Library},
    model::{Edit, Group, Tab},
    mods, play, prefs, runs, session,
    store::Store,
    syscheck, title, verify,
};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;
use ui::background::Art;
use ui::icons as icon;
use ui::theme::{self as th, alpha, lerp};
use ui::widgets::{self as w, Tone};

const TITLE: &str = "Triumph Rebuild";
/// The goal every run is read against, and where the fork stood (Series X title, the owner's machine).
pub const TARGET_FPS: f64 = 30.0;
pub const BASELINE_FPS: f64 = 7.6;
pub const FRAME_BUDGET_MS: f64 = 1000.0 / TARGET_FPS;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        attach_console();
        std::process::exit(print_check());
    }
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    if let Some(dir) = arg("--matrix") {
        attach_console();
        std::process::exit(print_matrix(&PathBuf::from(dir), arg("--profiles").as_deref().unwrap_or("")));
    }
    let shots = arg("--shots").map(PathBuf::from);
    let size = arg("--size")
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some([a.parse::<f32>().ok()?, b.parse::<f32>().ok()?])))
        .unwrap_or([1280.0, 800.0]);
    if let Some(dir) = &shots {
        let dir = dir.clone();
        let _ = std::fs::create_dir_all(&dir);
        std::panic::set_hook(Box::new(move |info| {
            let _ = std::fs::write(dir.join("panic.txt"), format!("{info}"));
        }));
    }
    let root = match &shots {
        Some(_) => fixture_root(),
        None => Home::locate().dir,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(size)
            .with_min_inner_size([960.0, 640.0])
            .with_decorations(false)
            .with_resizable(true),
        vsync: true,
        ..Default::default()
    };
    let fixture = shots.is_some();
    let r = eframe::run_native(TITLE, options, Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, root, shots, fixture)))));
    if let Err(e) = r {
        eprintln!("launcher: {e}");
    }
}

fn print_check() -> i32 {
    let root = Home::locate().dir;
    let lib = Library::load(&root).unwrap_or(Library { root: root.clone(), games: Vec::new(), selected: None });
    let home = lib.selected.as_deref().map(|id| lib.home(id)).unwrap_or(Home { dir: root });
    let cfg = Store::load(&home.rebuild_toml()).map(|s| s.model.config).unwrap_or_else(|e| {
        eprintln!("{e}");
        Config::parse("").unwrap()
    });
    let checks = syscheck::run(&probe(&cfg));
    let mut fails = 0;
    for c in &checks {
        let s = match c.status {
            syscheck::Status::Pass => "pass",
            syscheck::Status::Fail => {
                fails += 1;
                "FAIL"
            }
            syscheck::Status::Skipped => "skip",
        };
        println!("{s:4}  {:16} {}", c.name, c.detail);
        if !c.fix.is_empty() {
            println!("      {:16} fix: {}", "", c.fix);
        }
    }
    println!("{} checks, {} failed", checks.len(), fails);
    if fails == 0 {
        0
    } else {
        1
    }
}

/// `--matrix <dir>`: the test matrix from the selected game's settings (`home::write_matrix`).
fn print_matrix(dir: &Path, names: &str) -> i32 {
    let root = Home::locate().dir;
    let lib = Library::load(&root).unwrap_or(Library { root: root.clone(), games: Vec::new(), selected: None });
    let home = lib.selected.as_deref().map(|id| lib.home(id)).unwrap_or(Home { dir: root });
    let base = match Store::load(&home.rebuild_toml()) {
        Ok(s) => s.model.config,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let names: Vec<&str> = names.split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
    match home::write_matrix(dir, &base, &names) {
        Ok(files) => {
            for f in &files {
                let name = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let summary = rebuild_config::builtin(name).map(|b| b.summary()).unwrap_or_default();
                println!("{}  {summary}", f.display());
            }
            println!("{} test configurations written (from {})", files.len(), home.rebuild_toml().display());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// `--check` prints to the console it was started from (the app is a windows-subsystem binary).
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        AttachConsole(u32::MAX);
    }
}

fn probe(cfg: &Config) -> syscheck::SystemProbe {
    syscheck::SystemProbe {
        game_dir: PathBuf::from(cfg.get("game.dir").to_string()),
        lock: std::env::var_os("REBUILD_MEASURE_LOCK").map(PathBuf::from),
    }
}

/// A synthetic package folder for the fixture (placeholder names only).
fn fixture_package(root: &Path, folder: &str, name: &str, family: &str) -> PathBuf {
    let d = root.join("packages").join(folder);
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(
        d.join("MicrosoftGame.config"),
        format!(r#"<Game><Executable Name="placeholder.exe" TargetDeviceFamily="{family}" /><ShellVisuals DefaultDisplayName="{name}" PublisherDisplayName="Placeholder Studio" /></Game>"#),
    );
    let _ = std::fs::write(d.join("placeholder.exe"), b"MZ");
    d
}

/// The screenshot fixture: a library with placeholder names and demo data (sample mods, profiles,
/// sessions and a test matrix that never ran: every number in it is demo data, and the top bar says so).
/// When `REBUILD_GAME_DIR` is set, the first game points at that folder (read-only) so the header shows
/// the run-time key art path, still under a placeholder name.
fn fixture_root() -> PathBuf {
    let root = std::env::temp_dir().join("triumph-launcher-fixture");
    let _ = std::fs::remove_dir_all(&root);
    let second = fixture_package(&root, "second", "Second title", "Scarlett");
    fixture_package(&root, "third", "Third title", "Scarlett");
    fixture_package(&root, "older", "Older title", "XboxOne");
    let mut lib = Library { root: root.clone(), games: Vec::new(), selected: None };
    let first = std::env::var_os("REBUILD_GAME_DIR").map(PathBuf::from).unwrap_or_else(|| fixture_package(&root, "first", "Your title", "Scarlett"));
    for (dir, name) in [(first, "Your title"), (second, "Second title")] {
        let id = library::game_id(&dir);
        let home = lib.home(&id);
        let mut st = Store::load(&home.rebuild_toml()).unwrap();
        let set = |st: &mut Store, k: &str, e: Edit| {
            let _ = st.model.apply(rebuild_config::find(k).unwrap(), e);
        };
        set(&mut st, "game.dir", Edit::Text(dir.display().to_string()));
        if name == "Your title" {
            let put = |m: &str, rel: &str| {
                let p = home.mods_dir().join(m).join(rel);
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, m);
            };
            put("cinematic-grade", "common/data/timecycle.xml");
            put("cinematic-grade", "common/data/lut.dds");
            put("clean-hud", "ui/hud/minimap.xml");
            put("clean-hud", "common/data/timecycle.xml");
            put("wider-fov", "common/data/camera.xml");
            st.model.config.mods = vec![
                rebuild_config::Mod { name: "cinematic-grade".into(), enabled: true },
                rebuild_config::Mod { name: "clean-hud".into(), enabled: true },
                rebuild_config::Mod { name: "wider-fov".into(), enabled: false },
            ];
            let mut q = st.model.config.clone();
            let _ = q.set("display.width", Value::Int(2560));
            let _ = q.set("display.height", Value::Int(1440));
            let _ = home.save_profile("1440p quality", &q);
            let _ = st.model.apply_builtin(rebuild_config::builtin("series-s").unwrap());
            fixture_sessions(&home, &st.model.config);
        }
        let _ = st.save(&home.rebuild_toml());
        lib.games.push(library::Game { id: id.clone(), dir, name: name.into(), detector: "gdk-package".into() });
        if lib.selected.is_none() {
            lib.selected = Some(id);
        }
    }
    let _ = lib.save();
    root
}

/// Demo sessions for the screenshots (never real measurements: the top bar marks the fixture).
fn fixture_sessions(home: &Home, base: &Config) {
    let now = session::now_unix();
    let exe = PathBuf::from("rebuild.exe");
    let demo: [(&str, f64, f64, f64, f64, f64); 3] = [("series-x", 31.2, 28.4, 32.1, 21.5, 1.9), ("series-s", 34.0, 30.1, 29.4, 18.2, 1.7), ("low-pc", 27.5, 22.8, 36.4, 25.9, 2.4)];
    let matrix = session::stamp(now - 7200);
    for (i, (name, avg, low, ms, gpu, up)) in demo.iter().enumerate() {
        let b = rebuild_config::builtin(name).unwrap();
        let Ok(c) = rebuild_config::profiles::matrix_config(base, b) else { continue };
        let start = session::Start { profile: name, config: &c, exe: &exe, config_path: &exe, matrix: Some(&matrix), run_seconds: Some(180), measure_from: Some("after_loading") };
        let t0 = now - 7200 + i as u64 * 200;
        if let Ok(dir) = session::create(&home.sessions_dir(), &start, t0) {
            let _ = session::finish(&dir, &session::End { code: Some(0), status: "exit code: 0".into(), stopped_by_launcher: false }, t0 + 185);
            let _ = std::fs::write(dir.join(session::SUMMARY_TOML), format!("# fixture: demo data, not a measurement\n[summary]\navg_fps = {avg}\nlow1_fps = {low}\nframe_ms = {ms}\ngpu_ms = {gpu}\nupload_ms = {up}\nseconds = 180\nmeasured_from = \"after_loading\"\n"));
            let mut log = String::from("fixture: demo frame line, not a measurement\n");
            for k in 0..40 {
                let f = avg + ((k as f64) * 0.7).sin() * 1.6;
                log += &format!("t {:>3}s | fps {f:.1} | ms {:.1} | gpu {gpu:.1} ms | draws 4,512 | dropped 0 | upload {up:.1} ms | xlate 2.4 ms | upscale 1.1 ms | wait 3.0 ms | 1707x960 -> 2560x1440 dlss quality\n", k + 1, 1000.0 / f);
            }
            let _ = std::fs::write(dir.join(session::FRAME_LOG), log);
        }
    }
    let start = session::Start { profile: "series-s", config: base, exe: &exe, config_path: &exe, matrix: None, run_seconds: None, measure_from: None };
    if let Ok(dir) = session::create(&home.sessions_dir(), &start, now - 600) {
        let _ = session::finish(&dir, &session::End { code: Some(-1073741819), status: "exit code: 0xc0000005".into(), stopped_by_launcher: false }, now - 540);
        let _ = std::fs::write(dir.join("crash-0001.txt"), "fixture: demo crash report, not a real one\nexception 0xC0000005 (access violation) at title+0x1a2b3c\n");
        let _ = std::fs::write(dir.join(session::FRAME_LOG), "fixture: demo frame line\nconfig ok\n");
    }
}

#[derive(Clone)]
pub enum Dialog {
    Add { dir: PathBuf, detection: Detection, previous: bool, restore: bool },
    Remove { id: String, name: String, delete_data: bool },
    /// a text viewer (a log, a crash report), with an optional "copy for a bug report"
    Viewer { title: String, text: String, copy: Option<String> },
    DeleteSession { dir: PathBuf, name: String },
}

pub struct Toast {
    text: String,
    tone: Tone,
    at: f64,
}

/// The run in progress: its session and what the top bar shows.
#[derive(Clone)]
pub struct RunInfo {
    pub session: PathBuf,
    pub profile: String,
    pub log: PathBuf,
    pub matrix: bool,
}

#[derive(Clone)]
struct Step {
    name: String,
    tab: Tab,
    dark: bool,
    running: bool,
    dialog: Option<u8>,
}

struct Shots {
    dir: PathBuf,
    plan: Vec<Step>,
    step: usize,
    frames: u32,
    waiting: bool,
}

/// What Profiles has selected for its "limits in force" panel.
#[derive(Clone, PartialEq)]
pub enum Pick {
    Builtin(&'static str),
    User(String),
}

pub struct App {
    root: PathBuf,
    library: Library,
    detectors: Vec<Box<dyn library::Detector>>,
    detections: HashMap<String, Detection>,
    arts: HashMap<String, Rc<Art>>,
    home: Home,
    art: Rc<Art>,
    visuals: gamemeta::Visuals,
    tab: Tab,
    store: Store,
    /// the settings as last saved (what "N changes" is counted against)
    saved: Store,
    checks: Vec<syscheck::Check>,
    check_rx: Option<mpsc::Receiver<Vec<syscheck::Check>>>,
    mods: Vec<mods::Entry>,
    conflicts: Vec<mods::Conflict>,
    profiles: Vec<String>,
    profile_name: String,
    import_problems: Vec<String>,
    running: Option<play::Running>,
    run: Option<RunInfo>,
    tail: Option<play::LogTail>,
    log: Vec<String>,
    frames: Vec<Frame>,
    sessions: Vec<session::Entry>,
    proof_exe: String,
    proof_args: String,
    title_note: String,
    toasts: Vec<Toast>,
    dialog: Option<Dialog>,
    // Settings
    query: String,
    group: Group,
    // Profiles
    pick: Option<Pick>,
    // Runs
    matrix_pick: Vec<bool>,
    run_len: usize,
    measure_from: runs::MeasureFrom,
    plan: Option<runs::Plan>,
    compare: BTreeSet<PathBuf>,
    // Diagnostics
    log_tab: usize,
    log_session: Option<PathBuf>,
    verify_rx: Option<mpsc::Receiver<Result<verify::ReadOnly, String>>>,
    verified: Option<Result<verify::ReadOnly, String>>,
    started: f64,
    /// Minimise the launcher when the game starts, and bring it back when it exits, so it costs the game
    /// nothing while it runs.
    minimise_on_launch: bool,
    minimised_for_game: bool,
    rounded: bool,
    fixture: bool,
    /// fixture and tests: draw the Running view from the loaded log without a process
    preview_running: bool,
    shots: Option<Shots>,
    /// tests: a lock file that always exists, so no D3D12 device is ever created
    lock_override: Option<PathBuf>,
}

/// What the launcher is doing that needs frames without input.
#[derive(Clone, Copy, Default)]
struct RepaintNeeds {
    screenshots: bool,
    ambient: bool,
    toasts: bool,
    check_pending: bool,
    game_running: bool,
    minimised: bool,
}

/// Idle is the default: with nothing moving, the launcher waits for input (an hour is "never" to egui;
/// any event wakes it). The Direction B design has no ambient motion, so `ambient` is only ever set by
/// the screenshot run; toasts fade at 30 fps; a running game's log is read four times a second, or
/// once a second while the launcher is minimised.
fn repaint_after(n: RepaintNeeds) -> Duration {
    if n.screenshots {
        return Duration::from_millis(16);
    }
    let mut d = Duration::from_secs(3600);
    if n.ambient || n.toasts {
        d = d.min(Duration::from_millis(33));
    }
    if n.check_pending {
        d = d.min(Duration::from_millis(100));
    }
    if n.game_running {
        d = d.min(Duration::from_millis(if n.minimised { 1000 } else { 250 }));
    }
    d
}

/// Every line of a file (a session's frame line, for its averages).
fn read_lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p).unwrap_or_default().lines().map(String::from).collect()
}

/// Open a folder in Explorer.
fn open_folder(p: &Path) {
    let _ = std::fs::create_dir_all(p);
    let _ = std::process::Command::new("explorer").arg(p).spawn();
}

pub fn mmss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

pub fn bytes(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1u64 << 20) as f64)
    } else {
        format!("{} KB", n.div_ceil(1024))
    }
}

impl App {
    fn new(ctx: &egui::Context, root: PathBuf, shots: Option<PathBuf>, fixture: bool) -> App {
        th::install(ctx, prefs::theme(&root) != "light");
        let mut toasts = Vec::new();
        let library = Library::load(&root).unwrap_or_else(|e| {
            toasts.push(Toast { text: format!("library: {e}"), tone: Tone::Danger, at: -1.0 });
            Library { root: root.clone(), games: Vec::new(), selected: None }
        });
        let mut app = App {
            home: Home { dir: root.clone() },
            root,
            library,
            detectors: library::detectors(),
            detections: HashMap::new(),
            arts: HashMap::new(),
            art: Rc::new(Art::new()),
            visuals: gamemeta::Visuals::default(),
            tab: Tab::Play,
            store: Store::new(Config::parse("").unwrap()),
            saved: Store::new(Config::parse("").unwrap()),
            checks: Vec::new(),
            check_rx: None,
            mods: Vec::new(),
            conflicts: Vec::new(),
            profiles: Vec::new(),
            profile_name: String::new(),
            import_problems: Vec::new(),
            running: None,
            run: None,
            tail: None,
            log: Vec::new(),
            frames: Vec::new(),
            sessions: Vec::new(),
            proof_exe: String::new(),
            proof_args: "--bare".into(),
            title_note: String::new(),
            toasts,
            dialog: None,
            query: String::new(),
            group: Group::Display,
            pick: None,
            matrix_pick: BUILTIN_PROFILES.iter().map(|b| !b.name.starts_with("stress-")).collect(),
            run_len: 1,
            measure_from: runs::MeasureFrom::AfterLoading,
            plan: None,
            compare: BTreeSet::new(),
            log_tab: 0,
            log_session: None,
            verify_rx: None,
            verified: None,
            started: -1.0,
            minimise_on_launch: std::env::var_os("REBUILD_LAUNCHER_STAY_OPEN").is_none(),
            minimised_for_game: false,
            rounded: false,
            fixture,
            preview_running: false,
            lock_override: None,
            shots: shots.map(|dir| {
                let mut plan = Vec::new();
                for (theme, dark) in [("dark", true), ("light", false)] {
                    let st = |n: &str, tab, running, dialog| Step { name: format!("{theme}-{n}"), tab, dark, running, dialog };
                    plan.push(st("01-play", Tab::Play, false, None));
                    plan.push(st("02-running", Tab::Play, true, None));
                    plan.push(st("03-profiles", Tab::Profiles, false, None));
                    plan.push(st("04-settings", Tab::Settings, false, None));
                    plan.push(st("05-runs", Tab::Runs, false, None));
                    plan.push(st("06-mods", Tab::Mods, false, None));
                    plan.push(st("07-diagnostics", Tab::Diagnostics, false, None));
                    plan.push(st("08-library", Tab::Library, false, None));
                    plan.push(st("09-library-add", Tab::Library, false, Some(1)));
                }
                Shots { dir, plan, step: 0, frames: 0, waiting: false }
            }),
        };
        for g in app.library.games.clone() {
            let d = library::detect(&g.dir, &app.detectors).0;
            app.detections.insert(g.id.clone(), d);
        }
        app.prune_all();
        if app.library.games.is_empty() {
            app.tab = Tab::Library;
        }
        app.load_game(ctx);
        app
    }

    /// At start: every game's sessions past its `diagnostics.retention_days`.
    fn prune_all(&mut self) {
        let now = session::now_unix();
        let mut n = 0;
        for g in self.library.games.clone() {
            let home = self.library.home(&g.id);
            let days = Store::load(&home.rebuild_toml()).map(|s| s.model.config.int("diagnostics.retention_days")).unwrap_or(14);
            n += session::prune(&home.sessions_dir(), days, now, &[]).len();
        }
        if n > 0 {
            self.toast(Tone::Accent, format!("{n} session folder{} past the retention deleted", if n == 1 { "" } else { "s" }));
        }
    }

    /// After a run: this game's sessions past the retention (never the running one).
    fn prune_game(&mut self) {
        let keep: Vec<PathBuf> = self.run.iter().map(|r| r.session.clone()).collect();
        session::prune(&self.home.sessions_dir(), self.store.model.config.int("diagnostics.retention_days"), session::now_unix(), &keep);
    }

    fn refresh_sessions(&mut self) {
        let mut v = session::list(&self.home.sessions_dir());
        for e in &mut v {
            if e.summary.is_none() {
                e.summary = Summary::from_lines(&read_lines(&e.frame_log()));
            }
        }
        self.sessions = v;
        self.compare.retain(|d| d.exists());
    }

    fn game_art(&mut self, ctx: &egui::Context, id: &str, dir: &Path) -> Rc<Art> {
        if let Some(a) = self.arts.get(id) {
            return a.clone();
        }
        let mut art = Art::new();
        let mut v = gamemeta::read(dir);
        if self.fixture {
            v.name = None;
            v.publisher = None;
        }
        art.load_title(ctx, &v);
        let a = Rc::new(art);
        self.arts.insert(id.into(), a.clone());
        a
    }

    /// Point every section at the selected game: its settings, mods, sessions, art and system check.
    fn load_game(&mut self, ctx: &egui::Context) {
        if let Some(r) = &mut self.running {
            r.stop();
        }
        self.finish_run();
        self.plan = None;
        let sel = self.library.selected_game().cloned();
        self.home = match &sel {
            Some(g) => self.library.home(&g.id),
            None => Home { dir: self.root.clone() },
        };
        match Store::load(&self.home.rebuild_toml()) {
            Ok(s) => {
                if s.recovered {
                    self.toast(Tone::Accent, "Settings recovered from the backup copy (.bak)");
                }
                self.import_problems = s.model.config.problems.iter().filter(|p| !p.contains("unknown setting")).cloned().collect();
                self.store = s;
            }
            Err(e) => {
                self.toast(Tone::Danger, e);
                self.store = Store::new(Config::parse("").unwrap());
            }
        }
        // the theme is launcher-wide (prefs), shown in every game's settings
        let theme = if th::is_dark() { "dark" } else { "light" };
        let _ = self.store.model.config.set("launcher.theme", Value::Choice(theme.into()));
        self.saved = self.store.clone();
        self.profile_name = self.store.model.config.profile.clone();
        self.profiles = self.home.profiles();
        (self.art, self.visuals) = match &sel {
            Some(g) => {
                let a = self.game_art(ctx, &g.id, &g.dir.clone());
                let mut v = gamemeta::read(&g.dir);
                if self.fixture {
                    v.publisher = None;
                }
                v.name = Some(g.name.clone());
                (a, v)
            }
            None => (Rc::new(Art::new()), gamemeta::Visuals::default()),
        };
        self.refresh_sessions();
        self.log = self.sessions.first().map(|e| play::read_last(&e.frame_log())).unwrap_or_default();
        self.frames = self.log.iter().filter_map(|l| frameline::parse(l)).collect();
        self.log_session = None;
        self.pick = None;
        self.verified = None;
        self.title_note.clear();
        self.refresh_mods();
        self.start_check();
    }

    fn switch_game(&mut self, ctx: &egui::Context, id: &str) {
        if self.dirty() {
            self.save();
        }
        if let Err(e) = self.library.select(id) {
            self.toast(Tone::Danger, e);
            return;
        }
        self.load_game(ctx);
        let name = self.library.selected_game().map(|g| g.name.clone()).unwrap_or_default();
        self.toast(Tone::Accent, format!("Switched to {name}"));
    }

    fn pick_game_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().set_title("Choose a game folder").pick_folder() {
            self.open_add(dir);
        }
    }

    fn open_add(&mut self, dir: PathBuf) {
        let (detection, _) = library::detect(&dir, &self.detectors);
        let previous = self.library.has_previous(&dir);
        self.dialog = Some(Dialog::Add { dir, detection, previous, restore: true });
    }

    fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), tone, at: -1.0 });
    }

    fn start_check(&mut self) {
        let mut p = probe(&self.store.model.config);
        if let Some(l) = &self.lock_override {
            p.lock = Some(l.clone());
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(syscheck::run(&p));
        });
        self.check_rx = Some(rx);
    }

    /// Mods: "Verify now", on a thread (metadata only).
    fn start_verify(&mut self) {
        let dir = self.game_dir();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(verify::check(&dir, 500_000));
        });
        self.verify_rx = Some(rx);
        self.verified = None;
    }

    fn dirty(&self) -> bool {
        self.library.selected.is_some() && self.store.to_toml() != self.saved.to_toml()
    }

    fn save(&mut self) -> bool {
        if self.library.selected.is_none() {
            return false;
        }
        match self.store.save(&self.home.rebuild_toml()) {
            Ok(()) => {
                self.saved = self.store.clone();
                true
            }
            Err(e) => {
                self.toast(Tone::Danger, e);
                false
            }
        }
    }

    /// The save bar's "Save to profile": `rebuild.toml`, and the active user profile's file too.
    fn save_to_profile(&mut self) {
        if !self.save() {
            return;
        }
        let name = self.store.model.config.profile.clone();
        let user = matches!(Active::of(&self.store.model.config), Active::User(_)) && self.profiles.contains(&name);
        if user {
            match self.home.save_profile(&name, &self.store.model.config) {
                Ok(_) => self.toast(Tone::Ok, format!("Saved to “{name}”")),
                Err(e) => self.toast(Tone::Danger, e),
            }
        } else {
            self.toast(Tone::Ok, "Settings saved");
        }
    }

    fn discard(&mut self) {
        self.store = self.saved.clone();
        self.refresh_mods();
        self.toast(Tone::Accent, "Changes discarded");
    }

    /// Switch the theme: apply, save launcher-wide, and keep it out of the "N changes" count.
    fn set_theme(&mut self, ctx: &egui::Context, dark: bool) {
        th::set_dark(ctx, dark);
        let v = Value::Choice(if dark { "dark" } else { "light" }.into());
        let _ = self.store.model.config.set("launcher.theme", v.clone());
        let _ = self.saved.model.config.set("launcher.theme", v);
        if !self.fixture {
            if let Err(e) = prefs::save_theme(&self.root, if dark { "dark" } else { "light" }) {
                self.toast(Tone::Danger, e);
            }
        }
    }

    fn refresh_mods(&mut self) {
        self.mods = mods::merge(&self.store.model.config.mods, &mods::scan(&self.home.mods_dir()));
        self.mods_changed();
    }

    fn mods_changed(&mut self) {
        self.store.model.config.mods = mods::to_config(&self.mods);
        self.conflicts = mods::conflicts(&self.home.mods_dir(), &self.mods);
    }

    fn game_dir(&self) -> PathBuf {
        PathBuf::from(self.store.model.config.get("game.dir").to_string())
    }

    fn title_source(&self) -> Option<PathBuf> {
        let explicit = title::explicit_source(&self.store.model.config, std::env::var_os("REBUILD_TITLE_SETTINGS"));
        title::find_source(explicit.as_deref(), &self.game_dir())
    }

    /// U3 at every launch: the copy of the title's settings.xml is regenerated from this game's saved
    /// values (the source of truth), in its own overlay. The original is only read.
    fn write_title_copy(&mut self) -> bool {
        let game = self.game_dir();
        let (ok, note) = match self.title_source() {
            None => (false, "No settings.xml found: title settings not applied (set Title settings file, or REBUILD_TITLE_SETTINGS).".to_string()),
            Some(src) => {
                let before = rebuild_launcher::hash_file(&src).ok();
                match title::write_copy(&src, Some(&game), &self.home.overlay_dir(), &self.store.model.config) {
                    Ok(dst) => {
                        let same = before == rebuild_launcher::hash_file(&src).ok();
                        (same, format!("Copy written to {}. Original {}.", dst.display(), if same { "unchanged" } else { "CHANGED" }))
                    }
                    Err(e) => (false, e),
                }
            }
        };
        self.title_note = note;
        ok
    }

    /// Launch from Play: the saved settings, in a new session.
    fn launch(&mut self) {
        if let Err(e) = self.start_run(None, false) {
            self.toast(Tone::Danger, e);
        } else {
            self.toast(Tone::Ok, "Launched");
        }
    }

    /// Start the layer (or a proof binary) in a new session folder. `matrix` runs a built-in profile over
    /// the saved settings with the matrix's length and measuring point.
    fn start_run(&mut self, matrix: Option<&'static rebuild_config::Builtin>, proof: bool) -> Result<(), String> {
        if self.running.is_some() {
            return Err("a run is already going".into());
        }
        if !self.save() {
            return Err("settings not saved".into());
        }
        let mut config = self.store.model.config.clone();
        let mut env: Vec<(String, String)> = Vec::new();
        let matrix_id = self.plan.as_ref().map(|p| p.id.clone());
        let profile = match (matrix, proof) {
            (Some(b), _) => {
                config = rebuild_config::profiles::matrix_config(&config, b)?;
                config.reset("diagnostics.frame_log_path");
                if let Some(p) = &self.plan {
                    env.push(("REBUILD_RUN_SECONDS".into(), p.run_seconds.to_string()));
                    env.push(("REBUILD_MEASURE_FROM".into(), p.measure_from.key().into()));
                    // the layer records it in the session's summary.toml (`matrix = "<id>"`)
                    env.push(("REBUILD_MATRIX_ID".into(), p.id.clone()));
                }
                b.name.to_string()
            }
            (None, true) => "proof".into(),
            (None, false) => config.profile.clone(),
        };
        let exe = if proof {
            PathBuf::from(self.proof_exe.trim())
        } else {
            play::find_layer().ok_or("rebuild.exe not found beside the launcher (or set REBUILD_EXE)")?
        };
        let plan = self.plan.as_ref();
        let home_toml = self.home.rebuild_toml();
        let start = session::Start {
            profile: &profile,
            config: &config,
            exe: &exe,
            config_path: &home_toml,
            matrix: matrix.and(matrix_id.as_deref()),
            run_seconds: matrix.and(plan.map(|p| p.run_seconds)),
            measure_from: matrix.and(plan.map(|p| p.measure_from.key())),
        };
        let dir = session::create(&self.home.sessions_dir(), &start, session::now_unix())?;
        let config_path = if matrix.is_some() {
            let p = dir.join("rebuild.toml");
            rebuild_launcher::write_atomic(&p, &config.to_toml()).map_err(|e| format!("{}: {e}", p.display()))?;
            p
        } else {
            self.home.rebuild_toml()
        };
        let custom = config.get("diagnostics.frame_log_path").to_string();
        let log = if custom.trim().is_empty() { dir.join(session::FRAME_LOG) } else { PathBuf::from(custom) };
        if !proof && self.write_title_copy() {
            self.toast(Tone::Accent, "Title settings applied to the overlay copy");
        }
        let started = if proof {
            let args: Vec<String> = self.proof_args.split_whitespace().map(String::from).collect();
            play::Running::start_proof_in(&exe, &args, &log, Some(&dir))
        } else {
            play::Running::start_in(&exe, &config_path, &log, Some(&dir), &env, &[])
        };
        match started {
            Ok(r) => {
                self.running = Some(r);
                self.run = Some(RunInfo { session: dir, profile, log: log.clone(), matrix: matrix.is_some() });
                self.tail = Some(play::LogTail::new(log));
                self.log.clear();
                self.frames.clear();
                self.refresh_sessions();
                Ok(())
            }
            Err(e) => {
                let _ = session::finish(&dir, &session::End { code: None, status: format!("did not start: {e}"), stopped_by_launcher: false }, session::now_unix());
                self.refresh_sessions();
                Err(e)
            }
        }
    }

    /// The child exited (or the game changed): record the end in `session.toml`, prune, refresh.
    fn finish_run(&mut self) {
        let Some(mut r) = self.running.take() else {
            self.run = None;
            self.tail = None;
            return;
        };
        if let Some(t) = &mut self.tail {
            let more = t.poll();
            self.frames.extend(more.iter().filter_map(|l| frameline::parse(l)));
            self.log.extend(more);
        }
        let status = r.poll();
        let secs = r.started.elapsed().as_secs();
        if let Some(info) = self.run.take() {
            let end = session::End {
                code: status.and_then(|s| s.code()),
                status: status.map(|s| s.to_string()).unwrap_or_else(|| "unknown".into()),
                stopped_by_launcher: r.stopped,
            };
            if let Err(e) = session::finish(&info.session, &end, session::now_unix()) {
                self.toast(Tone::Danger, e);
            }
            let ok = status.is_some_and(|s| s.success()) || r.stopped;
            self.toast(if ok { Tone::Accent } else { Tone::Danger }, format!("{} ended after {}: {}", info.profile, mmss(secs), if r.stopped { "stopped".into() } else { end.status }));
        }
        self.tail = None;
        self.prune_game();
        self.refresh_sessions();
    }

    fn stop(&mut self) {
        if let Some(r) = &mut self.running {
            r.stop();
        }
    }

    fn start_matrix(&mut self) {
        let names: Vec<String> = BUILTIN_PROFILES.iter().zip(&self.matrix_pick).filter(|(_, on)| **on).map(|(b, _)| b.name.to_string()).collect();
        if names.is_empty() {
            return;
        }
        let secs = runs::LENGTHS[self.run_len.min(runs::LENGTHS.len() - 1)].0;
        self.plan = Some(runs::Plan::new(session::stamp(session::now_unix()), names, secs, self.measure_from));
    }

    fn poll(&mut self, now: f64) {
        if let Some(t) = &mut self.tail {
            let more = t.poll();
            self.frames.extend(more.iter().filter_map(|l| frameline::parse(l)));
            self.log.extend(more);
            let n = self.log.len();
            if n > 2000 {
                self.log.drain(..n - 2000);
            }
            let n = self.frames.len();
            if n > 600 {
                self.frames.drain(..n - 600);
            }
        }
        if self.running.as_mut().is_some_and(|r| r.poll().is_some()) {
            self.finish_run();
        }
        let running = self.running.is_some();
        if let Some(step) = self.plan.as_mut().map(|p| p.tick(now, running)) {
            match step {
                runs::Step::Start(name) => {
                    let b = rebuild_config::builtin(&name);
                    match b.ok_or_else(|| format!("{name}: no such built-in")).and_then(|b| self.start_run(Some(b), false)) {
                        Ok(()) => {}
                        Err(e) => {
                            self.toast(Tone::Danger, format!("{name}: {e}"));
                            if let Some(p) = &mut self.plan {
                                p.start_failed(e);
                            }
                        }
                    }
                }
                runs::Step::Stop => self.stop(),
                runs::Step::Done => {
                    let failed = self.plan.as_ref().map(|p| p.failed.len()).unwrap_or(0);
                    self.plan = None;
                    self.toast(if failed == 0 { Tone::Ok } else { Tone::Warn }, format!("Test matrix finished{}", if failed > 0 { format!(", {failed} did not start") } else { String::new() }));
                }
                runs::Step::Wait => {}
            }
        }
        if let Some(rx) = &self.check_rx {
            if let Ok(c) = rx.try_recv() {
                self.checks = c;
                self.check_rx = None;
            }
        }
        if let Some(rx) = &self.verify_rx {
            if let Ok(r) = rx.try_recv() {
                self.verified = Some(r);
                self.verify_rx = None;
            }
        }
    }

    /// When the next frame is needed if nothing else asks (input, and egui's own hover animations,
    /// request their frames themselves).
    fn next_repaint(&self, minimised: bool) -> Duration {
        repaint_after(RepaintNeeds {
            screenshots: self.shots.is_some(),
            ambient: false,
            toasts: !self.toasts.is_empty(),
            check_pending: self.check_rx.is_some() || self.verify_rx.is_some(),
            game_running: self.running.is_some() || self.plan.is_some(),
            minimised,
        })
    }

    /// Get out of the game's way while it runs: minimise on start, come back when it exits.
    fn game_window_state(&mut self, ctx: &egui::Context) {
        if self.shots.is_some() || !self.minimise_on_launch {
            return;
        }
        if self.running.is_some() && !self.minimised_for_game {
            self.minimised_for_game = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        } else if self.running.is_none() && self.plan.is_none() && self.minimised_for_game {
            self.minimised_for_game = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
    }

    fn is_running(&self) -> bool {
        self.running.is_some() || self.preview_running
    }

    fn system_ready(&self) -> Option<bool> {
        if self.checks.is_empty() {
            return None;
        }
        Some(self.checks.iter().all(|c| c.status != syscheck::Status::Fail))
    }

    fn adapter_detail(&self, name: &str) -> Option<String> {
        self.checks.iter().find(|c| c.name == name && c.status == syscheck::Status::Pass).map(|c| c.detail.clone())
    }

    /// Warnings the launcher can tell before a launch, beyond the system check.
    fn launch_warnings(&self) -> Vec<String> {
        let c = &self.store.model.config;
        let mut v: Vec<String> = self.checks.iter().filter(|c| c.status == syscheck::Status::Fail).map(|c| format!("{}: {}", c.name, c.detail)).collect();
        let backend = c.choice("upscaler.backend");
        let nvidia = self.adapter_detail("GPU").is_some_and(|g| g.contains("NVIDIA"));
        let dlss_set = !c.get("upscaler.dlss_dll").to_string().trim().is_empty() || std::env::var_os("REBUILD_DLSS_DLL").is_some();
        if (backend == "dlss" || (backend == "auto" && nvidia)) && !dlss_set {
            v.push("DLSS DLL not set → FSR".into());
        }
        if self.game_dir().as_os_str().is_empty() {
            v.insert(0, "Game folder not set".into());
        }
        v
    }

    /// The profile line: "Profile: <name> · 2560×1440 · AUTO quality · RAM 8 GiB · …" or "no limits".
    fn profile_line(&self) -> String {
        let c = &self.store.model.config;
        let limits = rebuild_config::profiles::limits_in_force(c);
        format!(
            "Profile: {} · {}×{} · {} {} · {}",
            Active::of(c).describe(),
            c.int("display.width"),
            c.int("display.height"),
            c.choice("upscaler.backend").to_uppercase(),
            c.choice("upscaler.mode").replace('_', " "),
            if limits.is_empty() { "no limits".into() } else { limits.join(" · ") }
        )
    }
}

// ---------------------------------------------------------------------------------------------- frame

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, _] = th::p().bg.to_normalized_gamma_f32();
        [r, g, b, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame(ctx);
    }
}

impl App {
    /// One frame of the whole UI (no eframe types, so the smoke test can run it headless).
    fn frame(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.started < 0.0 {
            self.started = now;
        }
        if !self.rounded {
            self.rounded = true;
            round_corners();
        }
        for t in &mut self.toasts {
            if t.at < 0.0 {
                t.at = now;
            }
        }
        self.poll(now);
        self.keys(ctx);
        self.shots_step(ctx);
        let minimised = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        self.game_window_state(ctx);
        ctx.request_repaint_after(self.next_repaint(minimised));

        self.top_bar(ctx);
        if self.dirty() {
            self.save_bar(ctx);
        }
        let c = th::p();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(c.bg)).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                // a centred column: equal margins, at most 1180 wide
                let side = ((ui.available_width() - 1180.0) / 2.0).max(th::S3);
                let margin = egui::Margin { left: side as i8, right: side as i8, top: th::S3 as i8, bottom: th::S4 as i8 };
                egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                    let wdt = ui.available_width();
                    ui.set_width(wdt);
                    let needs_game = !matches!(self.tab, Tab::Library) && self.library.selected.is_none();
                    if needs_game {
                        self.no_game(ui);
                        return;
                    }
                    match self.tab {
                        Tab::Play if self.is_running() => self.running_view(ui, now),
                        Tab::Play => self.play(ui),
                        Tab::Profiles => self.profiles_page(ui),
                        Tab::Settings => self.settings_page(ui),
                        Tab::Runs => self.runs_page(ui),
                        Tab::Mods => self.mods_page(ui),
                        Tab::Diagnostics => self.diagnostics_page(ui),
                        Tab::Library => self.library_page(ui),
                    }
                });
            });
        });
        // a theme chosen on Settings applies at once and is saved launcher-wide
        let want_dark = self.store.model.config.choice("launcher.theme") != "light";
        if want_dark != th::is_dark() {
            self.set_theme(ctx, want_dark);
        }
        self.dialogs(ctx);
        self.draw_toasts(ctx, now);
        self.resize_edges(ctx);
    }

    fn keys(&mut self, ctx: &egui::Context) {
        let (tab, save, launch, check) = ctx.input_mut(|i| {
            let mut tab = None;
            let nums = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6];
            for (k, key) in nums.iter().enumerate() {
                if i.consume_key(egui::Modifiers::COMMAND, *key) {
                    tab = Some(Tab::TOP[k]);
                }
            }
            (tab, i.consume_key(egui::Modifiers::COMMAND, Key::S), i.consume_key(egui::Modifiers::COMMAND, Key::Enter), i.consume_key(egui::Modifiers::NONE, Key::F5))
        });
        if let Some(t) = tab {
            self.set_tab(t);
        }
        if save && self.dirty() {
            self.save_to_profile();
        }
        if launch && self.library.selected.is_some() && self.running.is_none() {
            self.launch();
        }
        if check && self.check_rx.is_none() {
            self.start_check();
        }
    }

    /// The top bar (Direction B): logo and name, the six sections (the active one underlined in the
    /// accent), the status pill, the theme switch and the window buttons. The bar is the drag area.
    fn top_bar(&mut self, ctx: &egui::Context) {
        let c = th::p();
        let h = 52.0;
        egui::TopBottomPanel::top("topbar").exact_height(h).show_separator_line(false).frame(egui::Frame::new().fill(c.header)).show(ctx, |ui| {
            let bar = ui.max_rect();
            let p = ui.painter().clone();
            p.hline(bar.x_range(), bar.bottom() - 0.5, th::stroke(1.0, c.line));
            let drag = ui.interact(bar, Id::new("titlebar"), Sense::click_and_drag());
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            if drag.double_clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            } else if drag.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            // logo and name
            let mark = Rect::from_min_size(Pos2::new(bar.left() + 18.0, bar.center().y - 13.0), Vec2::splat(26.0));
            p.rect_filled(mark, CornerRadius::same(th::R_SM), c.accent);
            p.text(mark.center(), Align2::CENTER_CENTER, icon::ROCKET_LAUNCH, th::icon(15.0), Color32::WHITE);
            let name = p.layout_no_wrap(TITLE.into(), th::bold(15.0), c.text);
            let mut x = mark.right() + 10.0;
            p.galley(Pos2::new(x, bar.center().y - name.size().y / 2.0), name.clone(), c.text);
            x += name.size().x + 28.0;
            // sections
            let mut pick = None;
            for (i, t) in Tab::TOP.iter().enumerate() {
                let active = *t == self.tab || (*t == Tab::Play && self.tab == Tab::Library);
                let g = p.layout_no_wrap(t.label().into(), if active { th::semibold(13.5) } else { th::medium(13.5) }, c.text);
                let r = Rect::from_min_max(Pos2::new(x - 10.0, bar.top()), Pos2::new(x + g.size().x + 10.0, bar.bottom()));
                let resp = ui.interact(r, Id::new(("nav", i)), Sense::click()).on_hover_text(format!("Ctrl+{}", i + 1));
                let hov = ctx.animate_bool_with_time(resp.id.with("h"), resp.hovered() || resp.has_focus(), th::T_HOVER);
                let col = if active { c.text } else { lerp(c.muted, c.text, hov) };
                p.galley(Pos2::new(x, bar.center().y - g.size().y / 2.0), g.clone(), col);
                if active {
                    let u = Rect::from_min_max(Pos2::new(x - 2.0, bar.bottom() - 3.0), Pos2::new(x + g.size().x + 2.0, bar.bottom() - 1.0));
                    p.rect_filled(u, CornerRadius::same(1), c.accent);
                }
                w::focus_ring(ui, &resp, r.shrink(6.0), th::R_SM);
                if resp.clicked() {
                    pick = Some(*t);
                }
                x += g.size().x + 26.0;
            }
            if let Some(t) = pick {
                self.set_tab(t);
            }
            // window buttons
            let bw = 44.0;
            let buttons = [(icon::MINUS, "Minimise", 0), (if maximized { icon::CORNERS_IN } else { icon::SQUARE }, if maximized { "Restore" } else { "Maximise" }, 1), (icon::X, "Close", 2)];
            for (k, (ic, tip, what)) in buttons.iter().enumerate() {
                let r = Rect::from_min_size(Pos2::new(bar.right() - (3 - k) as f32 * bw, bar.top()), Vec2::new(bw, bar.height() - 1.0));
                let resp = ui.interact(r, Id::new(("winbtn", k)), Sense::click()).on_hover_text(*tip);
                let hv = ctx.animate_bool_with_time(resp.id.with("h"), resp.hovered(), th::T_HOVER);
                let fill = if *what == 2 { alpha(rgb_close(), hv) } else { alpha(c.line, hv) };
                p.rect_filled(r, 0.0, fill);
                let col = if *what == 2 { lerp(c.text_2, Color32::WHITE, hv) } else { c.text_2 };
                p.text(r.center(), Align2::CENTER_CENTER, *ic, th::icon(14.0), col);
                if resp.clicked() {
                    match what {
                        0 => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                        1 => ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                        _ => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    }
                }
            }
            // theme switch
            let tr = Rect::from_center_size(Pos2::new(bar.right() - 3.0 * bw - 26.0, bar.center().y), Vec2::splat(30.0));
            let tresp = ui.interact(tr, Id::new("theme"), Sense::click()).on_hover_text(if th::is_dark() { "Light theme" } else { "Dark theme" });
            let hv = ctx.animate_bool_with_time(tresp.id.with("h"), tresp.hovered() || tresp.has_focus(), th::T_HOVER);
            p.rect_filled(tr, CornerRadius::same(th::R_MD), alpha(c.line, hv));
            p.text(tr.center(), Align2::CENTER_CENTER, if th::is_dark() { icon::SUN } else { icon::MOON }, th::icon(16.0), lerp(c.text_2, c.text, hv));
            w::focus_ring(ui, &tresp, tr, th::R_MD);
            if tresp.clicked() {
                let dark = !th::is_dark();
                self.set_theme(ctx, dark);
            }
            // status pill, right-aligned before the theme switch
            let (text, tone) = self.status_text();
            let g = p.layout_no_wrap(text.clone(), th::medium(12.5), c.text);
            let pw = g.size().x + 32.0;
            let pr = Rect::from_min_size(Pos2::new(tr.left() - 12.0 - pw, bar.center().y - 13.0), Vec2::new(pw, 26.0));
            if pr.left() > x {
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(pr));
                w::status_pill(&mut child, &text, tone);
            }
            if self.fixture {
                let fr = Rect::from_min_size(Pos2::new(pr.left() - 200.0, bar.center().y - 10.0), Vec2::new(188.0, 20.0));
                if fr.left() > x {
                    p.rect_filled(fr, CornerRadius::same(th::R_SM), c.warn_bg);
                    p.text(fr.center(), Align2::CENTER_CENTER, "SCREENSHOT FIXTURE · DEMO DATA", th::semibold(9.5), c.warn);
                }
            }
        });
    }

    /// "Ready" or "Running · <profile> · mm:ss" (and the matrix's progress).
    fn status_text(&self) -> (String, Tone) {
        if let (Some(r), Some(info)) = (&self.running, &self.run) {
            let mut s = format!("Running · {} · {}", info.profile, mmss(r.started.elapsed().as_secs()));
            if let (Some(p), true) = (&self.plan, info.matrix) {
                let (n, total) = p.progress();
                s += &format!(" · {n}/{total}");
            }
            return (s, Tone::Accent);
        }
        if self.preview_running {
            return (format!("Running · {} · 02:41", self.store.model.config.profile), Tone::Accent);
        }
        if self.plan.is_some() {
            return ("Test matrix".into(), Tone::Accent);
        }
        if self.library.selected.is_none() {
            return ("No game".into(), Tone::Muted);
        }
        if self.game_dir().as_os_str().is_empty() {
            return ("Set the game folder".into(), Tone::Warn);
        }
        match self.system_ready() {
            Some(false) => ("Ready · check warnings".into(), Tone::Warn),
            _ => ("Ready".into(), Tone::Ok),
        }
    }

    /// The save bar: "N changes · M take effect on next launch", Discard, Save to profile.
    fn save_bar(&mut self, ctx: &egui::Context) {
        let c = th::p();
        let (keys, restart) = rebuild_launcher::model::pending(&self.saved.model.config, &self.store.model.config);
        let mods_changed = self.saved.model.config.mods != self.store.model.config.mods;
        let other = !mods_changed && keys.is_empty();
        let n = keys.len() + usize::from(mods_changed);
        egui::TopBottomPanel::bottom("savebar").exact_height(52.0).show_separator_line(false).frame(egui::Frame::new().fill(c.header)).show(ctx, |ui| {
            let r = ui.max_rect();
            ui.painter().hline(r.x_range(), r.top() + 0.5, th::stroke(1.0, c.line));
            let side = ((r.width() - 1180.0) / 2.0).max(th::S3);
            let inner = Rect::from_min_max(Pos2::new(r.left() + side, r.top()), Pos2::new(r.right() - side, r.bottom()));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let ui = &mut child;
            ui.label(egui::RichText::new(icon::FLOPPY_DISK).font(th::icon(16.0)).color(c.accent));
            let text = if other {
                "Unsaved changes".to_string()
            } else {
                let mut s = format!("{n} change{}", if n == 1 { "" } else { "s" });
                if restart > 0 || mods_changed {
                    s += &format!(" · {} take{} effect on next launch", restart + usize::from(mods_changed), if restart + usize::from(mods_changed) == 1 { "s" } else { "" });
                }
                s
            };
            ui.label(egui::RichText::new(text).font(th::medium(13.5)).color(c.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if w::primary_button(ui, "", "Save to profile", Vec2::new(140.0, 32.0), true).on_hover_text("Ctrl+S").clicked() {
                    self.save_to_profile();
                }
                if w::button(ui, "", "Discard", true).clicked() {
                    self.discard();
                }
            });
        });
    }

    fn resize_edges(&mut self, ctx: &egui::Context) {
        use egui::viewport::ResizeDirection as D;
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            return;
        }
        let screen = ctx.screen_rect();
        let Some(p) = ctx.input(|i| i.pointer.hover_pos()) else { return };
        let e = 5.0;
        let (l, r, t, b) = (p.x < screen.left() + e, p.x > screen.right() - e, p.y < screen.top() + e, p.y > screen.bottom() - e);
        let dir = match (l, r, t, b) {
            (true, _, true, _) => Some((D::NorthWest, egui::CursorIcon::ResizeNorthWest)),
            (_, true, true, _) => Some((D::NorthEast, egui::CursorIcon::ResizeNorthEast)),
            (true, _, _, true) => Some((D::SouthWest, egui::CursorIcon::ResizeSouthWest)),
            (_, true, _, true) => Some((D::SouthEast, egui::CursorIcon::ResizeSouthEast)),
            (true, ..) => Some((D::West, egui::CursorIcon::ResizeWest)),
            (_, true, ..) => Some((D::East, egui::CursorIcon::ResizeEast)),
            (_, _, true, _) => Some((D::North, egui::CursorIcon::ResizeNorth)),
            (_, _, _, true) => Some((D::South, egui::CursorIcon::ResizeSouth)),
            _ => None,
        };
        if let Some((d, cur)) = dir {
            ctx.set_cursor_icon(cur);
            if ctx.input(|i| i.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(d));
            }
        }
    }

    fn draw_toasts(&mut self, ctx: &egui::Context, now: f64) {
        let life = 3.2;
        self.toasts.retain(|t| now - t.at < life);
        let c = th::p();
        let screen = ctx.screen_rect();
        let mut y = screen.bottom() - if self.dirty() { 72.0 } else { 20.0 };
        for (i, t) in self.toasts.iter().rev().enumerate() {
            let age = (now - t.at) as f32;
            let a = th::ease_out(age / 0.2).min(th::ease_out((life as f32 - age) / 0.4));
            let (ic, col) = match t.tone {
                Tone::Ok => (icon::CHECK_CIRCLE, c.ok),
                Tone::Danger => (icon::WARNING_CIRCLE, c.danger),
                Tone::Warn => (icon::WARNING, c.warn),
                _ => (icon::INFO, c.accent),
            };
            let width = 360.0;
            let x = screen.right() - 20.0 - width;
            let resp = egui::Area::new(Id::new(("toast", i))).order(egui::Order::Tooltip).fixed_pos(Pos2::new(x, y - 48.0)).show(ctx, |ui| {
                ui.multiply_opacity(a);
                egui::Frame::new()
                    .fill(c.panel)
                    .stroke(th::stroke(1.0, c.line_hi))
                    .corner_radius(CornerRadius::same(th::R_MD))
                    .shadow(th::popup_shadow())
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_width(width - 28.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(ic).font(th::icon(16.0)).color(col));
                            ui.add(egui::Label::new(egui::RichText::new(&t.text).font(th::medium(13.0)).color(c.text)).wrap());
                        });
                    })
            });
            y -= resp.response.rect.height() + 8.0;
        }
    }

    // ------------------------------------------------------------------------------------------ shots

    fn shots_step(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.shots else { return };
        if s.step >= s.plan.len() {
            return;
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let path = s.dir.join(format!("{}.png", s.plan[s.step].name));
            let _ = std::fs::create_dir_all(&s.dir);
            if let Err(e) = save_png(&path, &img) {
                eprintln!("{}: {e}", path.display());
            }
            s.step += 1;
            s.frames = 0;
            s.waiting = false;
            if s.step == s.plan.len() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        if s.waiting {
            return;
        }
        let step = s.plan[s.step].clone();
        s.frames += 1;
        let first = s.frames == 1;
        // the first screen waits out the system check
        let wait = if s.step == 0 { 120 } else { 30 };
        if s.frames >= wait {
            s.waiting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        if first {
            if th::is_dark() != step.dark {
                self.set_theme(ctx, step.dark);
            }
            self.dialog = None;
            self.preview_running = step.running;
            if step.running {
                self.log = self.sessions.iter().find(|e| e.info.as_ref().is_some_and(|i| i.matrix.is_some())).map(|e| play::read_last(&e.frame_log())).unwrap_or_default();
                self.frames = self.log.iter().filter_map(|l| frameline::parse(l)).collect();
            }
            if step.dialog == Some(1) {
                self.open_add(self.root.join("packages").join("third"));
            }
            if step.tab == Tab::Profiles {
                self.pick = Some(Pick::Builtin("series-s"));
            }
            self.toasts.clear();
            self.tab = step.tab;
        }
    }
}

fn rgb_close() -> Color32 {
    th::rgb(0xC4, 0x2B, 0x1C)
}

fn save_png(path: &Path, img: &egui::ColorImage) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), img.size[0] as u32, img.size[1] as u32);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    e.write_header().map_err(|e| e.to_string())?.write_image_data(&data).map_err(|e| e.to_string())
}

/// Rounded window corners on Windows 11 (DWM); a no-op elsewhere.
fn round_corners() {
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
        use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
        if let Ok(hwnd) = FindWindowW(None, w!("Triumph Rebuild")) {
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const _ as *const _, std::mem::size_of_val(&pref) as u32);
        }
    }
}

#[cfg(test)]
mod smoke {
    //! The real app's flow, headless: every section in both themes, add two games, switch, change a
    //! setting, save, sessions listed and pruned, remove one, "restart" (a new App on the same folder),
    //! check the setting persisted, every section again. Runs egui frames without a renderer or a window,
    //! and creates no D3D12 device.
    use super::*;

    fn package(root: &Path, folder: &str, name: &str) -> PathBuf {
        let d = root.join(folder);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("MicrosoftGame.config"), format!(r#"<Game><Executable Name="placeholder.exe" TargetDeviceFamily="Scarlett" /><ShellVisuals DefaultDisplayName="{name}" /></Game>"#)).unwrap();
        std::fs::write(d.join("placeholder.exe"), b"MZ").unwrap();
        d
    }

    struct Run {
        ctx: egui::Context,
        t: f64,
    }
    impl Run {
        fn frames(&mut self, app: &mut App, n: usize) {
            for _ in 0..n {
                self.t += 1.0 / 60.0;
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0))), time: Some(self.t), ..Default::default() };
                let _ = self.ctx.run(input, |ctx| app.frame(ctx));
            }
        }
        /// Every section, the Running view, and a settings search and each pill, in both themes.
        fn every_page(&mut self, app: &mut App) {
            let ctx = self.ctx.clone();
            for dark in [true, false] {
                app.set_theme(&ctx, dark);
                assert_eq!(th::is_dark(), dark);
                for tab in Tab::ALL {
                    app.set_tab(tab);
                    self.frames(app, 2);
                }
                app.set_tab(Tab::Play);
                app.preview_running = true;
                self.frames(app, 2);
                app.preview_running = false;
                app.set_tab(Tab::Settings);
                for g in Group::ALL {
                    app.group = g;
                    self.frames(app, 1);
                }
                app.query = "retention".into();
                self.frames(app, 1);
                app.query.clear();
                app.set_tab(Tab::Profiles);
                for pick in [Some(Pick::Builtin("series-x")), Some(Pick::User("none".into())), None] {
                    app.pick = pick;
                    self.frames(app, 1);
                }
            }
            app.set_theme(&ctx, true);
        }
    }

    #[test]
    fn the_whole_flow_runs_without_a_panic_and_settings_persist() {
        let root = std::env::temp_dir().join(format!("triumph-launcher-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let games = root.join("games-on-disk");
        let a = package(&games, "a", "Placeholder A");
        let b = package(&games, "b", "Placeholder B");
        let app_root = root.join("app");
        let lock = root.join("MEASURING");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&lock, b"").unwrap();

        let ctx = egui::Context::default();
        let mut run = Run { ctx: ctx.clone(), t: 0.0 };
        let mut app = App::new(&ctx, app_root.clone(), None, false);
        app.lock_override = Some(lock.clone());
        app.start_check();
        // an empty library: every section shows its empty state
        assert_eq!(app.tab, Tab::Library);
        run.every_page(&mut app);

        // add two games through the same path as the dialog
        for dir in [&a, &b] {
            app.open_add(dir.clone());
            run.frames(&mut app, 2);
            let Some(Dialog::Add { detection, restore, .. }) = app.dialog.clone() else { panic!("no add dialog") };
            assert!(matches!(detection, Detection::Supported { .. }));
            assert!(app.confirm_add(&ctx, dir, &detection, restore));
            app.dialog = None;
            run.frames(&mut app, 2);
        }
        assert_eq!(app.library.games.len(), 2);
        assert_eq!(app.library.selected_game().unwrap().name, "Placeholder B");
        run.every_page(&mut app);

        // switch to A, change a setting, save
        let ida = app.library.games[0].id.clone();
        let idb = app.library.games[1].id.clone();
        app.switch_game(&ctx, &ida);
        assert_eq!(app.game_dir(), a);
        let wdt = rebuild_config::find("display.width").unwrap();
        app.store.model.apply(wdt, Edit::Slide(2560)).unwrap();
        assert!(app.dirty());
        let (keys, restart) = rebuild_launcher::model::pending(&app.saved.model.config, &app.store.model.config);
        assert_eq!((keys.len(), restart), (1, 1));
        run.frames(&mut app, 3);
        assert!(app.save());
        assert!(!app.dirty());
        // the theme switch is saved launcher-wide and is never an unsaved change
        app.set_theme(&ctx, false);
        assert!(!app.dirty());
        assert_eq!(prefs::theme(&app_root), "light");
        app.set_theme(&ctx, true);
        // B is untouched
        app.switch_game(&ctx, &idb);
        assert_eq!(app.store.model.config.int("display.width"), 3840);
        run.every_page(&mut app);

        // sessions: one old and one recent on B; a restart prunes the old one (retention 14 days)
        let sessions = app.home.sessions_dir();
        let now = session::now_unix();
        let cfg = app.store.model.config.clone();
        let x = PathBuf::from("x");
        let mk = |t| session::create(&sessions, &session::Start { profile: "default", config: &cfg, exe: &x, config_path: &x, matrix: None, run_seconds: None, measure_from: None }, t).unwrap();
        let old = mk(now - 30 * 86_400);
        let recent = mk(now - 3600);
        std::fs::write(recent.join(session::FRAME_LOG), "fps 30.0 | ms 33.3\nfps 20.0 | ms 50.0\n").unwrap();
        app.refresh_sessions();
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[0].dir, recent, "newest first");
        assert_eq!(app.sessions[0].summary.as_ref().and_then(|s| s.avg_fps), Some(25.0));
        run.every_page(&mut app);

        // a built-in test profile on B: its keys set, B's other settings kept, shown as active
        let vsync = rebuild_config::find("display.vsync").unwrap();
        app.store.model.apply(vsync, Edit::Toggle(true)).unwrap();
        let low = rebuild_config::builtin("low-pc").unwrap();
        app.apply_builtin(low);
        assert_eq!(app.store.model.config.int("display.width"), 1920);
        assert!(app.store.model.config.bool("display.vsync"));
        assert_eq!(Active::of(&app.store.model.config), Active::Builtin(low));
        assert_eq!(app.profile_name, "low-pc copy");
        run.frames(&mut app, 3);
        app.write_matrix(&root.join("matrix"));
        assert_eq!(std::fs::read_dir(root.join("matrix")).unwrap().count(), BUILTIN_PROFILES.len());
        assert!(app.save());
        app.switch_game(&ctx, &ida);
        app.switch_game(&ctx, &idb);
        assert_eq!(Active::of(&app.store.model.config), Active::Builtin(low));
        assert!(app.store.model.config.bool("display.vsync"));

        // remove B (keeping its data), which was selected: A becomes selected
        assert!(app.confirm_remove(&ctx, &idb, "Placeholder B", false));
        assert_eq!(app.library.selected.as_deref(), Some(ida.as_str()));
        assert_eq!(app.store.model.config.int("display.width"), 2560);
        run.every_page(&mut app);

        // restart: the theme comes back from prefs; re-add B and its old session is gone, the recent kept
        app.set_theme(&ctx, false);
        drop(app);
        let ctx2 = egui::Context::default();
        let mut run2 = Run { ctx: ctx2.clone(), t: 0.0 };
        let mut app = App::new(&ctx2, app_root.clone(), None, false);
        assert!(!th::is_dark(), "the theme persisted");
        app.lock_override = Some(lock.clone());
        assert_eq!(app.library.games.len(), 1);
        assert_eq!(app.library.selected.as_deref(), Some(ida.as_str()));
        assert_eq!(app.store.model.config.int("display.width"), 2560, "the setting persisted");
        assert_eq!(app.store.model.config.choice("launcher.theme"), "light");
        run2.every_page(&mut app);
        app.open_add(b.clone());
        let Some(Dialog::Add { previous, detection, .. }) = app.dialog.clone() else { panic!() };
        assert!(previous);
        assert!(app.confirm_add(&ctx2, &b, &detection, true));
        app.dialog = None;
        drop(app);
        let mut app = App::new(&ctx2, app_root.clone(), None, false);
        app.lock_override = Some(lock.clone());
        assert!(!old.exists(), "a session past the retention is deleted at start");
        assert!(recent.exists(), "a recent session is kept");
        run2.every_page(&mut app);

        // remove both with their data: the library is empty again and every section still draws
        for id in [ida.clone(), idb.clone()] {
            assert!(app.confirm_remove(&ctx2, &id, "Placeholder", true));
        }
        assert!(app.library.games.is_empty());
        run2.every_page(&mut app);
        assert!(a.join("MicrosoftGame.config").exists() && b.join("placeholder.exe").exists(), "game folders untouched");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod type_check {
    //! Every text style lays out a pangram with real glyphs: none blank, none missing. This catches a
    //! font family order that lets the icon font (which has blank Latin glyphs) win over Inter.
    use super::*;

    #[test]
    fn every_text_style_renders_every_glyph() {
        let ctx = egui::Context::default();
        th::install(&ctx, true);
        let input = || egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))), ..Default::default() };
        let _ = ctx.run(input(), |_| {});
        let _ = ctx.run(input(), |_| {});
        let text = "The quick brown fox jumps over the lazy dog 0123456789 ·&,.→— THE QUICK BROWN FOX";
        let styles = [
            ("body", th::body()),
            ("small", th::small()),
            ("regular 14", th::regular(14.0)),
            ("medium 13.5 (buttons, top bar)", th::medium(13.5)),
            ("semibold 15 (h2)", th::h2().0),
            ("bold 22 (h1)", th::h1().0),
            ("bold 30 (display)", th::display().0),
            ("bold 28 (numerals)", th::numerals().0),
            ("caption", th::caption().0),
            ("mono", th::mono(12.5)),
        ];
        let mut bad = Vec::new();
        ctx.fonts(|f| {
            for (name, font) in &styles {
                let g = f.layout_no_wrap(text.to_string(), font.clone(), Color32::WHITE);
                let glyphs: Vec<_> = g.rows.iter().flat_map(|r| r.glyphs.iter()).collect();
                if glyphs.len() != text.chars().count() {
                    bad.push(format!("{name}: {} glyphs for {} chars", glyphs.len(), text.chars().count()));
                }
                for gl in glyphs {
                    if gl.chr == ' ' {
                        continue;
                    }
                    if !f.has_glyph(font, gl.chr) {
                        bad.push(format!("{name}: no glyph for {:?}", gl.chr));
                    } else if gl.uv_rect.is_nothing() || gl.uv_rect.size.x <= 0.0 || gl.uv_rect.size.y <= 0.0 {
                        bad.push(format!("{name}: blank glyph for {:?}", gl.chr));
                    }
                }
            }
            // and the icon family draws every icon with a shape
            for (n, s) in ui::icons::ALL {
                let g = f.layout_no_wrap(s.to_string(), th::icon(16.0), Color32::WHITE);
                let gl = &g.rows[0].glyphs[0];
                if gl.uv_rect.is_nothing() {
                    bad.push(format!("icon {n}: blank"));
                }
            }
        });
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// The two palettes are the approved ones (Direction B), and text reads on every surface.
    #[test]
    fn both_themes_use_the_approved_tokens() {
        let hex = |c: Color32| format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b());
        let d = th::DARK;
        assert_eq!([hex(d.bg), hex(d.header), hex(d.panel), hex(d.line), hex(d.text), hex(d.muted), hex(d.accent)], ["#0E1014", "#12151B", "#151A23", "#262C37", "#E8EAED", "#8C95A3", "#3D7BFF"]);
        assert_eq!([hex(d.ok), hex(d.ok_bg), hex(d.warn), hex(d.warn_bg)], ["#9BE3B0", "#17321F", "#F5C36B", "#2E2714"]);
        let l = th::LIGHT;
        assert_eq!([hex(l.bg), hex(l.header), hex(l.panel), hex(l.line), hex(l.text), hex(l.muted), hex(l.accent)], ["#F5F6F8", "#FFFFFF", "#FFFFFF", "#DCE1E8", "#14171C", "#5B6472", "#2F6BEB"]);
        assert_eq!([hex(l.ok), hex(l.ok_bg), hex(l.warn), hex(l.warn_bg)], ["#1D6B36", "#E3F5E8", "#7A4F00", "#FFF2D6"]);
        // WCAG relative luminance contrast
        let lum = |c: Color32| {
            let f = |v: u8| {
                let s = v as f64 / 255.0;
                if s <= 0.03928 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        };
        let contrast = |a: Color32, b: Color32| {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        for p in [th::DARK, th::LIGHT] {
            assert!(contrast(p.text, p.panel) >= 7.0);
            assert!(contrast(p.muted, p.panel) >= 4.5, "muted text reads");
            assert!(contrast(p.ok, p.ok_bg) >= 4.5 && contrast(p.warn, p.warn_bg) >= 4.5, "chips read");
        }
    }
}

#[cfg(test)]
mod idle {
    use super::{repaint_after, RepaintNeeds};
    use std::time::Duration;

    #[test]
    fn an_idle_launcher_asks_for_no_frames() {
        assert!(repaint_after(RepaintNeeds::default()) >= Duration::from_secs(60));
    }

    #[test]
    fn unfocused_or_minimised_stops_ambient_motion() {
        // ambient is false whenever the window is unfocused or minimised (see `frame`)
        let n = RepaintNeeds { ambient: false, ..Default::default() };
        assert!(repaint_after(n) >= Duration::from_secs(60));
        let n = RepaintNeeds { ambient: true, ..Default::default() };
        assert_eq!(repaint_after(n), Duration::from_millis(33));
    }

    #[test]
    fn a_running_game_is_polled_slowly_and_more_slowly_when_minimised() {
        let n = RepaintNeeds { game_running: true, ..Default::default() };
        assert_eq!(repaint_after(n), Duration::from_millis(250));
        let n = RepaintNeeds { game_running: true, minimised: true, ..Default::default() };
        assert_eq!(repaint_after(n), Duration::from_secs(1));
    }

    #[test]
    fn toasts_and_screenshots_get_their_frames() {
        let n = RepaintNeeds { toasts: true, ..Default::default() };
        assert_eq!(repaint_after(n), Duration::from_millis(33));
        let n = RepaintNeeds { screenshots: true, ..Default::default() };
        assert_eq!(repaint_after(n), Duration::from_millis(16));
    }
}
