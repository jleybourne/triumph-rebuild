//! triumph-rebuild launcher: settings, profiles, title settings, mods, system check, and Play.
//! Every settings widget is generated from `rebuild_config::REGISTRY` (`docs/design/launcher.md`);
//! the look is `src/ui` (tokens in `ui/theme.rs`, the table in `agents/launcher.md`).
//!
//! `rebuild-launcher --check` prints the system check without opening a window.
//! `rebuild-launcher --shots <dir>` renders every page to PNGs in `<dir>` from a demo home, then exits.
//! `REBUILD_MEASURE_LOCK=<file>`: while that file exists, the check creates no D3D12 device.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod ui;

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Id, Key, Layout, Pos2, Rect, Sense, StrokeKind, Vec2};
use egui_phosphor::regular as icon;
use rebuild_config::{Config, Setting};
use rebuild_launcher::{
    gamemeta,
    home::{self, Home},
    model::{self, Edit, Model, Tab, Widget},
    mods, play, syscheck, title,
};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;
use ui::background::{self, Art};
use ui::theme::{self as th, alpha, ease_out, lerp};
use ui::widgets as w;

const TITLE: &str = "triumph-rebuild";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        attach_console();
        std::process::exit(print_check());
    }
    let shots = args.iter().position(|a| a == "--shots").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let home = match &shots {
        Some(_) => demo_home(),
        None => Home::locate(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 640.0])
            .with_decorations(false)
            .with_resizable(true),
        vsync: true,
        ..Default::default()
    };
    let r = eframe::run_native(TITLE, options, Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, home, shots)))));
    if let Err(e) = r {
        eprintln!("launcher: {e}");
    }
}

fn print_check() -> i32 {
    let home = Home::locate();
    let cfg = home.load().unwrap_or_else(|e| {
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
    if fails == 0 { 0 } else { 1 }
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

/// A home with sample mods, profiles and a last session, for `--shots`. The game folder comes from
/// `REBUILD_GAME_DIR` (read-only), so the hero shows the title's own art when it is set.
fn demo_home() -> Home {
    let dir = std::env::temp_dir().join("rebuild-launcher-demo-home");
    let _ = std::fs::remove_dir_all(&dir);
    let home = Home { dir };
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
    let mut c = Config::parse("").unwrap();
    if let Some(g) = std::env::var_os("REBUILD_GAME_DIR") {
        let _ = c.set("game.dir", rebuild_config::Value::Path(g.to_string_lossy().into()));
    }
    let _ = c.set("display.width", rebuild_config::Value::Int(2560));
    let _ = c.set("display.height", rebuild_config::Value::Int(1440));
    c.mods = vec![
        rebuild_config::Mod { name: "cinematic-grade".into(), enabled: true },
        rebuild_config::Mod { name: "clean-hud".into(), enabled: true },
        rebuild_config::Mod { name: "wider-fov".into(), enabled: false },
    ];
    let _ = home.save_profile("1440p quality", &c);
    let mut s = c.clone();
    s.stable_only = true;
    let _ = home.save_profile("stable", &s);
    c.profile = "1440p quality".into();
    let _ = home::export(&c, &home.rebuild_toml());
    let mut log = String::from("config profile=1440p quality problems=0\n");
    for (i, (fps, ms)) in [(30.8, 32.5), (31.2, 32.1), (30.4, 32.9), (31.6, 31.6)].iter().enumerate() {
        log += &format!("t {:>3}s | fps {fps:.1} | ms {ms:.1} | draws 4,512 | dropped 0 | upload 1.9 ms | 2560x1440 dlss quality\n", i + 1);
    }
    let _ = std::fs::write(home.frame_log(), log);
    home
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Good,
    Bad,
    Info,
}

struct Toast {
    text: String,
    tone: Tone,
    at: f64,
}

struct Shots {
    dir: PathBuf,
    plan: Vec<(String, Tab, bool)>,
    step: usize,
    frames: u32,
    waiting: bool,
}

struct App {
    home: Home,
    art: Rc<Art>,
    visuals: gamemeta::Visuals,
    tab: Tab,
    model: Model,
    saved: String,
    checks: Vec<syscheck::Check>,
    check_rx: Option<mpsc::Receiver<Vec<syscheck::Check>>>,
    checks_at: f64,
    mods: Vec<mods::Entry>,
    conflicts: Vec<mods::Conflict>,
    profiles: Vec<String>,
    profile_name: String,
    import_problems: Vec<String>,
    running: Option<play::Running>,
    tail: Option<play::LogTail>,
    log: Vec<String>,
    session: String,
    proof_exe: String,
    proof_args: String,
    title_note: String,
    collapsed: bool,
    toasts: Vec<Toast>,
    started: f64,
    rounded: bool,
    shots: Option<Shots>,
}

fn group_title(g: &str) -> (&'static str, String) {
    match g {
        "game" => (icon::FOLDER_OPEN, "Game".into()),
        "display" => (icon::MONITOR, "Display".into()),
        "upscaler" => (icon::SPARKLE, "Upscaler".into()),
        "title" => (icon::SLIDERS_HORIZONTAL, "The title's own settings".into()),
        "memory" => (icon::CPU, "Memory".into()),
        "shaders" => (icon::LIGHTNING, "Shaders".into()),
        "diagnostics" => (icon::PULSE, "Diagnostics".into()),
        "menu" => (icon::KEYBOARD, "In-game menu".into()),
        other => (icon::GEAR_SIX, other.to_string()),
    }
}

fn tab_icon(t: Tab) -> &'static str {
    match t {
        Tab::Play => icon::PLAY,
        Tab::Display => icon::MONITOR,
        Tab::Title => icon::SLIDERS_HORIZONTAL,
        Tab::Features => icon::FLASK,
        Tab::Mods => icon::PUZZLE_PIECE,
        Tab::Diagnostics => icon::PULSE,
        Tab::Profiles => icon::USER_CIRCLE,
        Tab::About => icon::INFO,
    }
}

fn tab_subtitle(t: Tab) -> &'static str {
    match t {
        Tab::Play => "",
        Tab::Display => "Output size, window, pacing, and how the picture is upscaled.",
        Tab::Title => "Written into a copy of the title's settings.xml at launch. The original is never touched.",
        Tab::Features => "Switches for the layer itself. Each default is the measured best.",
        Tab::Mods => "Folders whose files replace the title's by path. Later mods win.",
        Tab::Diagnostics => "The frame line, captures and the system check.",
        Tab::Profiles => "Named sets of settings. Export and import them as .toml.",
        Tab::About => "",
    }
}

impl App {
    fn new(ctx: &egui::Context, home: Home, shots: Option<PathBuf>) -> App {
        th::install(ctx);
        let (config, problems) = match home.load() {
            Ok(c) => {
                let p = c.problems.clone();
                (c, p)
            }
            Err(e) => (Config::parse("").unwrap(), vec![e]),
        };
        let mut art = Art::new(ctx);
        let game = PathBuf::from(config.get("game.dir").to_string());
        let visuals = if game.as_os_str().is_empty() { gamemeta::Visuals::default() } else { gamemeta::read(&game) };
        art.load_title(ctx, &visuals);
        let log = play::read_last(&home.frame_log());
        let mut app = App {
            profile_name: config.profile.clone(),
            saved: config.to_toml(),
            model: Model::new(config),
            art: Rc::new(art),
            visuals,
            tab: Tab::Play,
            checks: Vec::new(),
            check_rx: None,
            checks_at: 0.0,
            mods: Vec::new(),
            conflicts: Vec::new(),
            profiles: home.profiles(),
            import_problems: problems.clone(),
            running: None,
            tail: None,
            session: if log.is_empty() { "No session yet".into() } else { "Last session".into() },
            log,
            proof_exe: String::new(),
            proof_args: "--bare".into(),
            title_note: String::new(),
            collapsed: false,
            toasts: Vec::new(),
            started: -1.0,
            rounded: false,
            shots: shots.map(|dir| {
                let mut plan: Vec<(String, Tab, bool)> =
                    Tab::ALL.iter().enumerate().map(|(i, t)| (format!("{:02}-{}", i + 1, slug(t.label())), *t, false)).collect();
                plan.push(("09-play-sidebar-collapsed".into(), Tab::Play, true));
                Shots { dir, plan, step: 0, frames: 0, waiting: false }
            }),
            home,
        };
        if !problems.is_empty() {
            app.toast(Tone::Bad, format!("rebuild.toml: {}", problems.join("; ")));
        }
        app.refresh_mods();
        app.start_check();
        app
    }

    fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), tone, at: -1.0 });
    }

    fn start_check(&mut self) {
        let p = probe(&self.model.config);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(syscheck::run(&p));
        });
        self.check_rx = Some(rx);
    }

    fn dirty(&self) -> bool {
        self.model.config.to_toml() != self.saved
    }

    fn save(&mut self) -> bool {
        match self.model.save(&self.home.rebuild_toml()) {
            Ok(()) => {
                self.saved = self.model.config.to_toml();
                self.toast(Tone::Good, "Settings saved");
                true
            }
            Err(e) => {
                self.toast(Tone::Bad, e);
                false
            }
        }
    }

    fn refresh_mods(&mut self) {
        self.mods = mods::merge(&self.model.config.mods, &mods::scan(&self.home.mods_dir()));
        self.mods_changed();
    }

    fn mods_changed(&mut self) {
        self.model.config.mods = mods::to_config(&self.mods);
        self.conflicts = mods::conflicts(&self.home.mods_dir(), &self.mods);
    }

    fn game_dir(&self) -> PathBuf {
        PathBuf::from(self.model.config.get("game.dir").to_string())
    }

    fn title_source(&self) -> Option<PathBuf> {
        let explicit = std::env::var_os("REBUILD_TITLE_SETTINGS").map(PathBuf::from);
        title::find_source(explicit.as_deref(), &self.game_dir())
    }

    /// U3 at launch: the edited copy of the title's settings.xml, in the launcher's own overlay.
    fn write_title_copy(&mut self) -> bool {
        let game = self.game_dir();
        let (ok, note) = match self.title_source() {
            None => (false, "No settings.xml found: title settings not applied (set REBUILD_TITLE_SETTINGS).".to_string()),
            Some(src) => {
                let before = rebuild_launcher::hash_file(&src).ok();
                match title::write_copy(&src, Some(&game), &self.home.overlay_dir(), &self.model.config) {
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

    fn launch(&mut self, proof: bool) {
        if self.running.is_some() || !self.save() {
            return;
        }
        let log = self.home.frame_log();
        let started = if proof {
            let args: Vec<String> = self.proof_args.split_whitespace().map(String::from).collect();
            play::Running::start_proof(&PathBuf::from(self.proof_exe.trim()), &args, &log)
        } else {
            if self.write_title_copy() {
                self.toast(Tone::Info, "Title settings applied to the overlay copy");
            }
            match play::find_layer() {
                Some(exe) => play::Running::start(&exe, &self.home.rebuild_toml(), &log, &[]),
                None => Err("rebuild.exe not found beside the launcher (or set REBUILD_EXE)".into()),
            }
        };
        match started {
            Ok(r) => {
                self.running = Some(r);
                self.tail = Some(play::LogTail::new(log));
                self.log.clear();
                self.session = "Running".into();
                self.toast(Tone::Good, if proof { "Proof started" } else { "Launched" });
            }
            Err(e) => self.toast(Tone::Bad, e),
        }
    }

    fn poll(&mut self, now: f64) {
        if let Some(t) = &mut self.tail {
            self.log.extend(t.poll());
            let n = self.log.len();
            if n > 2000 {
                self.log.drain(..n - 2000);
            }
        }
        let mut exited = None;
        if let Some(r) = &mut self.running {
            if let Some(s) = r.poll() {
                exited = Some((s, r.started.elapsed().as_secs_f32()));
            }
        }
        if let Some((s, secs)) = exited {
            if let Some(t) = &mut self.tail {
                self.log.extend(t.poll());
            }
            self.running = None;
            self.session = format!("Exited after {secs:.1} s ({s})");
            self.toast(if s.success() { Tone::Info } else { Tone::Bad }, format!("Session ended: {s}"));
        }
        if let Some(rx) = &self.check_rx {
            if let Ok(c) = rx.try_recv() {
                self.checks = c;
                self.checks_at = now;
                self.check_rx = None;
            }
        }
    }

    fn set_tab(&mut self, ctx: &egui::Context, t: Tab) {
        if self.tab != t {
            self.tab = t;
            let now = ctx.input(|i| i.time);
            ctx.data_mut(|d| d.insert_temp(Id::new("page-t0"), now));
        }
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
}

fn slug(s: &str) -> String {
    s.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect::<String>().replace("--", "-").replace("--", "-")
}

// ---------------------------------------------------------------------------------------------- rows

/// One registry entry: label, pills, help on the left; the generated control and reset on the right.
fn setting_row(ui: &mut egui::Ui, m: &mut Model, s: &'static Setting, first: bool) {
    if !first {
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), th::stroke(1.0, th::HAIRLINE));
        ui.add_space(th::S2);
    }
    let b = m.badges(s);
    let full = ui.available_width();
    ui.horizontal_top(|ui| {
        let left = (full * 0.48).clamp(240.0, 440.0);
        ui.vertical(|ui| {
            ui.set_width(left);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(s.label).font(th::medium(14.5)).color(th::TEXT));
                ui.add_space(4.0);
                w::pill(ui, b.apply, if b.apply == "live" { th::SUCCESS } else { th::TEXT_2 }, false);
                if b.experimental {
                    w::pill(ui, &format!("{} experimental", icon::WARNING), th::WARNING, true);
                }
            });
            if !s.help.is_empty() {
                w::muted(ui, s.help);
            }
            ui.label(egui::RichText::new(s.key).font(th::mono(11.0)).color(th::TEXT_3));
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            let vis = if b.changed { egui::UiBuilder::new() } else { egui::UiBuilder::new().invisible() };
            let reset = ui.scope_builder(vis, |ui| w::button(ui, icon::ARROW_COUNTER_CLOCKWISE, "", b.changed)).inner;
            if reset.on_hover_text("Back to the default").clicked() {
                m.reset(s);
            }
            let mut e = m.current(s);
            let changed = match (&mut e, model::widget(s)) {
                (Edit::Toggle(v), _) => w::toggle(ui, v).changed(),
                (Edit::Slide(v), Widget::Slider { min, max }) => {
                    ui.add(egui::Slider::new(v, min..=max).trailing_fill(true)).changed()
                }
                (Edit::Pick(i), Widget::Dropdown { options }) => {
                    let short = options.len() <= 4 && options.iter().map(|o| o.len()).sum::<usize>() <= 34;
                    if short {
                        w::segmented(ui, Id::new(s.key), options, i)
                    } else {
                        let before = *i;
                        egui::ComboBox::from_id_salt(s.key).width(200.0).selected_text(options[*i]).show_ui(ui, |ui| {
                            for (k, o) in options.iter().enumerate() {
                                ui.selectable_value(i, k, *o);
                            }
                        });
                        *i != before
                    }
                }
                (Edit::Text(t), _) => {
                    let mut ch = false;
                    if w::button(ui, icon::FOLDER_OPEN, "Browse", true).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            *t = p.display().to_string();
                            ch = true;
                        }
                    }
                    let te = egui::TextEdit::singleline(t)
                        .font(th::mono(12.5))
                        .hint_text(egui::RichText::new("not set").color(th::TEXT_3))
                        .margin(egui::Margin::symmetric(10, 7))
                        .desired_width((ui.available_width() - 8.0).clamp(160.0, 380.0));
                    ch |= ui.add(te).changed();
                    ch
                }
                _ => false,
            };
            if changed {
                let _ = m.apply(s, e);
            }
        });
    });
    if let Some((k, why)) = &m.refusal {
        if *k == s.key {
            ui.label(egui::RichText::new(format!("{}  {why}", icon::X_CIRCLE)).font(th::small()).color(th::DANGER));
        }
    }
    ui.add_space(th::S2);
}

/// One card per registry group on a page.
fn settings_cards(ui: &mut egui::Ui, art: &Art, m: &mut Model, tab: Tab, first_index: usize) -> usize {
    let mut groups: Vec<&'static str> = Vec::new();
    for s in model::settings_in(tab) {
        if !groups.contains(&s.group()) {
            groups.push(s.group());
        }
    }
    for (gi, g) in groups.iter().enumerate() {
        let (ic, name) = group_title(g);
        w::card(ui, art, first_index + gi, Some((ic, &name)), |ui| {
            for (i, s) in model::settings_in(tab).filter(|s| s.group() == *g).enumerate() {
                setting_row(ui, m, s, i == 0);
            }
        });
    }
    first_index + groups.len()
}

fn page_header(ui: &mut egui::Ui, t: Tab) {
    ui.add_space(th::S1);
    w::text(ui, t.label(), th::h1(), th::TEXT);
    let sub = tab_subtitle(t);
    if !sub.is_empty() {
        ui.label(egui::RichText::new(sub).font(th::regular(14.0)).color(th::TEXT_2));
    }
    ui.add_space(th::S3);
}

fn empty_state(ui: &mut egui::Ui, ic: &str, head: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(th::S3);
        let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
        w::glow(ui.painter(), r.shrink(8.0), th::R_PILL, th::ACCENT, 0.5);
        ui.painter().circle_filled(r.center(), 30.0, alpha(th::ACCENT, 0.12));
        ui.painter().circle_stroke(r.center(), 30.0, th::stroke(1.0, alpha(th::ACCENT, 0.4)));
        ui.painter().text(r.center(), Align2::CENTER_CENTER, ic, th::regular(26.0), th::ACCENT);
        ui.add_space(th::S2);
        w::text(ui, head, th::h2(), th::TEXT);
        ui.add_space(4.0);
        ui.label(egui::RichText::new(body).font(th::regular(13.5)).color(th::TEXT_2));
        ui.add_space(th::S3);
    });
}

// ---------------------------------------------------------------------------------------------- frame

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, _] = th::BG_BASE.to_normalized_gamma_f32();
        [r, g, b, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = ctx.input(|i| i.time);
        if self.started < 0.0 {
            self.started = now;
            ctx.data_mut(|d| d.insert_temp(Id::new("page-t0"), now + 0.35));
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
        ctx.request_repaint_after(Duration::from_millis(16));

        let screen = ctx.screen_rect();
        let t = (now - self.started) as f32;
        background::paint(&ctx.layer_painter(egui::LayerId::background()), screen, t, 1.0);

        self.sidebar(ctx, now);
        egui::CentralPanel::default().frame(egui::Frame::new()).show(ctx, |ui| {
            let full = ui.max_rect();
            if self.tab == Tab::Play {
                let hero = Rect::from_min_size(full.min, Vec2::new(full.width(), (full.height() * 0.56).clamp(360.0, 520.0)));
                let p = ui.painter().with_clip_rect(full);
                background::hero(&p, &self.art, hero, t);
            }
            self.title_bar(ui, ctx);
            let body = Rect::from_min_max(Pos2::new(full.left(), full.top() + 40.0), full.max);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
            egui::ScrollArea::vertical().auto_shrink(false).show(&mut child, |ui| {
                let margin = egui::Margin { left: th::S6 as i8, right: th::S6 as i8, top: th::S1 as i8, bottom: th::S6 as i8 };
                egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                    ui.set_max_width(1180.0);
                    match self.tab {
                        Tab::Play => self.play(ui, t),
                        Tab::Display | Tab::Features => {
                            page_header(ui, self.tab);
                            let art = self.art.clone();
                            settings_cards(ui, &art, &mut self.model, self.tab, 0);
                        }
                        Tab::Title => self.title_page(ui),
                        Tab::Mods => self.mods_page(ui),
                        Tab::Diagnostics => self.diagnostics(ui, now),
                        Tab::Profiles => self.profiles_page(ui),
                        Tab::About => self.about(ui),
                    }
                });
            });
        });
        self.save_bar(ctx);
        self.draw_toasts(ctx, now);
        self.resize_edges(ctx);
        // fade-in splash
        let s = ((now - self.started) as f32 - 0.25) / 0.6;
        if s < 1.0 {
            let a = 1.0 - ease_out(s);
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Debug, Id::new("splash")));
            p.rect_filled(screen, 0.0, alpha(th::BG_BASE, a));
            let mark = Rect::from_center_size(screen.center() - Vec2::new(0.0, 16.0), Vec2::splat(56.0));
            w::glow(&p, mark, th::R_LG, th::ACCENT, a);
            w::gradient_rrect(&p, mark, th::R_LG as f32, alpha(th::ACCENT, a), alpha(th::ACCENT_2, a));
            p.text(mark.center(), Align2::CENTER_CENTER, icon::ROCKET_LAUNCH, th::regular(26.0), alpha(Color32::WHITE, a));
            p.text(mark.center_bottom() + Vec2::new(0.0, 24.0), Align2::CENTER_CENTER, TITLE, th::semibold(15.0), alpha(th::TEXT, a));
        }
    }
}

impl App {
    fn keys(&mut self, ctx: &egui::Context) {
        let (tabs, save, launch, check, collapse) = ctx.input_mut(|i| {
            let mut tab = None;
            let nums = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8];
            for (k, key) in nums.iter().enumerate() {
                if i.consume_key(egui::Modifiers::COMMAND, *key) {
                    tab = Some(Tab::ALL[k]);
                }
            }
            (
                tab,
                i.consume_key(egui::Modifiers::COMMAND, Key::S),
                i.consume_key(egui::Modifiers::COMMAND, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::F5),
                i.consume_key(egui::Modifiers::COMMAND, Key::B),
            )
        });
        if let Some(t) = tabs {
            self.set_tab(ctx, t);
        }
        if save {
            self.save();
        }
        if launch && !self.game_dir().as_os_str().is_empty() {
            self.launch(false);
        }
        if check && self.check_rx.is_none() {
            self.start_check();
        }
        if collapse {
            self.collapsed = !self.collapsed;
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context, now: f64) {
        let wide = ctx.animate_bool_with_time(Id::new("sidebar-wide"), !self.collapsed, th::T_PAGE);
        let width = egui::lerp(76.0..=240.0, ease_out(wide));
        let label_a = ((wide - 0.4) / 0.6).clamp(0.0, 1.0);
        egui::SidePanel::left("nav")
            .exact_width(width)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(th::BG_SIDEBAR))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().vline(r.right() - 0.5, r.y_range(), th::stroke(1.0, th::HAIRLINE));
                background::grain(ui.painter(), &self.art, r, 0.35);
                // logo mark and wordmark (a drag area too)
                let head = Rect::from_min_size(r.min, Vec2::new(r.width(), 72.0));
                let drag = ui.interact(head, Id::new("drag-side"), Sense::click_and_drag());
                if drag.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                let mark = Rect::from_min_size(Pos2::new(r.left() + 22.0, r.top() + 20.0), Vec2::splat(32.0));
                w::glow(ui.painter(), mark, th::R_MD, th::ACCENT, 0.6);
                w::gradient_rrect(ui.painter(), mark, th::R_MD as f32, th::ACCENT, th::ACCENT_2);
                ui.painter().text(mark.center(), Align2::CENTER_CENTER, icon::ROCKET_LAUNCH, th::regular(17.0), Color32::WHITE);
                if label_a > 0.0 {
                    let pos = Pos2::new(mark.right() + 12.0, mark.center().y);
                    let g = ui.painter().layout_job(th::job("triumph", (th::bold(16.0), -0.3), alpha(th::TEXT, label_a)));
                    let gw = g.size().x;
                    ui.painter().galley(Pos2::new(pos.x, pos.y - g.size().y / 2.0), g, th::TEXT);
                    ui.painter().text(Pos2::new(pos.x + gw + 3.0, pos.y), Align2::LEFT_CENTER, "rebuild", th::regular(16.0), alpha(th::TEXT_2, label_a));
                }

                // items, with an indicator that slides between them
                let top = r.top() + 96.0;
                let item_h = 44.0;
                let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0) as f32;
                let y = ctx.animate_value_with_time(Id::new("nav-ind"), idx * (item_h + 4.0), th::T_PAGE * 0.8) + top;
                let ind = Rect::from_min_size(Pos2::new(r.left() + 12.0, y), Vec2::new(r.width() - 24.0, item_h));
                ui.painter().rect_filled(ind, CornerRadius::same(th::R_MD), alpha(th::ACCENT, 0.14));
                ui.painter().rect_stroke(ind, CornerRadius::same(th::R_MD), th::stroke(1.0, alpha(th::ACCENT, 0.28)), StrokeKind::Inside);
                let bar = Rect::from_min_size(Pos2::new(r.left() + 12.0, y + 12.0), Vec2::new(3.0, item_h - 24.0));
                w::glow(ui.painter(), bar, 2, th::ACCENT, 0.9);
                ui.painter().rect_filled(bar, CornerRadius::same(2), th::ACCENT);
                let mut pick = None;
                for (i, t) in Tab::ALL.iter().enumerate() {
                    let ir = Rect::from_min_size(Pos2::new(r.left() + 12.0, top + i as f32 * (item_h + 4.0)), Vec2::new(r.width() - 24.0, item_h));
                    let resp = ui.interact(ir, Id::new(("nav", i)), Sense::click());
                    let resp = if label_a < 0.5 { resp.on_hover_text(t.label()) } else { resp };
                    let active = *t == self.tab;
                    let h = ctx.animate_bool_with_time(resp.id.with("h"), resp.hovered() || resp.has_focus(), th::T_HOVER);
                    if !active && h > 0.0 {
                        ui.painter().rect_filled(ir, CornerRadius::same(th::R_MD), alpha(Color32::WHITE, 0.04 * h));
                    }
                    let col = if active { th::TEXT } else { lerp(th::TEXT_3, th::TEXT_2, h) };
                    let ic_col = if active { th::ACCENT } else { col };
                    ui.painter().text(Pos2::new(ir.left() + 26.0, ir.center().y), Align2::CENTER_CENTER, tab_icon(*t), th::regular(19.0), ic_col);
                    if label_a > 0.0 {
                        ui.painter().text(
                            Pos2::new(ir.left() + 48.0, ir.center().y),
                            Align2::LEFT_CENTER,
                            t.label(),
                            if active { th::semibold(14.0) } else { th::medium(14.0) },
                            alpha(col, label_a),
                        );
                        if label_a > 0.9 && i < 8 {
                            ui.painter().text(Pos2::new(ir.right() - 10.0, ir.center().y), Align2::RIGHT_CENTER, format!("^{}", i + 1), th::mono(10.0), alpha(th::TEXT_3, 0.7 * h));
                        }
                    }
                    w::focus_ring(ui, &resp, ir, th::R_MD);
                    if resp.clicked() {
                        pick = Some(*t);
                    }
                }
                if let Some(t) = pick {
                    self.set_tab(ctx, t);
                }

                // bottom: profile chip and collapse
                let bottom = Rect::from_min_max(Pos2::new(r.left() + 12.0, r.bottom() - 108.0), Pos2::new(r.right() - 12.0, r.bottom() - 60.0));
                ui.painter().rect_filled(bottom, CornerRadius::same(th::R_MD), alpha(th::SURFACE_2, 0.8));
                ui.painter().rect_stroke(bottom, CornerRadius::same(th::R_MD), th::stroke(1.0, th::HAIRLINE), StrokeKind::Inside);
                let av = Pos2::new(bottom.left() + 24.0, bottom.center().y);
                ui.painter().circle_filled(av, 14.0, alpha(th::ACCENT, 0.2));
                ui.painter().text(av, Align2::CENTER_CENTER, icon::USER_CIRCLE, th::regular(18.0), th::ACCENT);
                if label_a > 0.0 {
                    ui.painter().text(Pos2::new(av.x + 22.0, av.y - 8.0), Align2::LEFT_CENTER, "PROFILE", th::semibold(9.5), alpha(th::TEXT_3, label_a));
                    let name = if self.model.config.profile.is_empty() { "default".to_string() } else { self.model.config.profile.clone() };
                    ui.painter().text(Pos2::new(av.x + 22.0, av.y + 8.0), Align2::LEFT_CENTER, name, th::medium(13.0), alpha(th::TEXT, label_a));
                }
                let chip = ui.interact(bottom, Id::new("profile-chip"), Sense::click());
                w::focus_ring(ui, &chip, bottom, th::R_MD);
                if chip.clicked() {
                    self.set_tab(ctx, Tab::Profiles);
                }
                let cr = Rect::from_min_size(Pos2::new(r.left() + 12.0, r.bottom() - 48.0), Vec2::new(r.width() - 24.0, 36.0));
                let c = ui.interact(cr, Id::new("collapse"), Sense::click());
                let h = ctx.animate_bool_with_time(c.id.with("h"), c.hovered() || c.has_focus(), th::T_HOVER);
                ui.painter().rect_filled(cr, CornerRadius::same(th::R_MD), alpha(Color32::WHITE, 0.04 * h));
                let ic = if self.collapsed { icon::CARET_DOUBLE_RIGHT } else { icon::CARET_DOUBLE_LEFT };
                ui.painter().text(Pos2::new(cr.left() + 26.0, cr.center().y), Align2::CENTER_CENTER, ic, th::regular(16.0), lerp(th::TEXT_3, th::TEXT, h));
                if label_a > 0.0 {
                    ui.painter().text(Pos2::new(cr.left() + 48.0, cr.center().y), Align2::LEFT_CENTER, "Collapse", th::medium(13.0), alpha(lerp(th::TEXT_3, th::TEXT_2, h), label_a));
                }
                w::focus_ring(ui, &c, cr, th::R_MD);
                if c.clicked() {
                    self.collapsed = !self.collapsed;
                }
                let _ = now;
            });
    }

    fn title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let full = ui.max_rect();
        let bar = Rect::from_min_size(full.min, Vec2::new(full.width(), 40.0));
        let bw = 46.0;
        let drag_r = Rect::from_min_max(bar.min, Pos2::new(bar.right() - 3.0 * bw, bar.bottom()));
        let drag = ui.interact(drag_r, Id::new("titlebar"), Sense::click_and_drag());
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        if drag.double_clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        } else if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if self.tab != Tab::Play {
            ui.painter().text(Pos2::new(bar.left() + th::S6, bar.center().y), Align2::LEFT_CENTER, format!("{TITLE}  /  {}", self.tab.label()), th::medium(12.0), th::TEXT_3);
        }
        let buttons = [
            (icon::MINUS, "Minimise", 0),
            (if maximized { icon::CORNERS_IN } else { icon::SQUARE }, if maximized { "Restore" } else { "Maximise" }, 1),
            (icon::X, "Close", 2),
        ];
        for (k, (ic, tip, what)) in buttons.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(bar.right() - (3 - k) as f32 * bw, bar.top()), Vec2::new(bw, bar.height()));
            let resp = ui.interact(r, Id::new(("winbtn", k)), Sense::click()).on_hover_text(*tip);
            let h = ctx.animate_bool_with_time(resp.id.with("h"), resp.hovered(), th::T_HOVER);
            let fill = if *what == 2 { alpha(th::DANGER, 0.9 * h) } else { alpha(Color32::WHITE, 0.07 * h) };
            ui.painter().rect_filled(r, 0.0, fill);
            let col = if *what == 2 { lerp(th::TEXT_2, Color32::WHITE, h) } else { lerp(th::TEXT_2, th::TEXT, h) };
            ui.painter().text(r.center(), Align2::CENTER_CENTER, *ic, th::regular(14.0), col);
            if resp.clicked() {
                match what {
                    0 => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                    1 => ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                    _ => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                }
            }
        }
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

    fn save_bar(&mut self, ctx: &egui::Context) {
        let a = ctx.animate_bool_with_time(Id::new("savebar"), self.dirty(), th::T_PAGE);
        if a <= 0.0 {
            return;
        }
        let screen = ctx.screen_rect();
        let size = Vec2::new(420.0, 56.0);
        let pos = Pos2::new(screen.center().x + 60.0 - size.x / 2.0, screen.bottom() - 24.0 - size.y + (1.0 - ease_out(a)) * 40.0);
        egui::Area::new(Id::new("savebar-area")).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            ui.multiply_opacity(a);
            egui::Frame::new()
                .fill(alpha(th::SURFACE_2, 0.96))
                .stroke(th::stroke(1.0, th::HAIRLINE_HI))
                .corner_radius(CornerRadius::same(th::R_LG))
                .shadow(th::card_shadow())
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.set_width(size.x - 32.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::FLOPPY_DISK).font(th::regular(17.0)).color(th::ACCENT));
                        ui.label(egui::RichText::new("Unsaved changes").font(th::medium(14.0)).color(th::TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::primary_button(ui, "", "Save   Ctrl+S", Vec2::new(132.0, 32.0), true).clicked() {
                                self.save();
                            }
                            if w::button(ui, "", "Discard", true).clicked() {
                                if let Ok(c) = Config::parse(&self.saved) {
                                    self.model = Model::new(c);
                                    self.refresh_mods();
                                    self.toast(Tone::Info, "Changes discarded");
                                }
                            }
                        });
                    });
                });
        });
    }

    fn draw_toasts(&mut self, ctx: &egui::Context, now: f64) {
        let life = 3.2;
        self.toasts.retain(|t| now - t.at < life);
        let screen = ctx.screen_rect();
        let mut y = screen.bottom() - 24.0;
        for (i, t) in self.toasts.iter().rev().enumerate() {
            let age = (now - t.at) as f32;
            let a = ease_out(age / 0.25).min(ease_out((life as f32 - age) / 0.4));
            let (ic, col) = match t.tone {
                Tone::Good => (icon::CHECK_CIRCLE, th::SUCCESS),
                Tone::Bad => (icon::WARNING_CIRCLE, th::DANGER),
                Tone::Info => (icon::INFO, th::ACCENT),
            };
            let width = 360.0;
            let x = screen.right() - 24.0 - width + (1.0 - a) * 24.0;
            let resp = egui::Area::new(Id::new(("toast", i))).order(egui::Order::Tooltip).fixed_pos(Pos2::new(x, y - 52.0)).show(ctx, |ui| {
                ui.multiply_opacity(a);
                egui::Frame::new()
                    .fill(alpha(th::SURFACE_2, 0.97))
                    .stroke(th::stroke(1.0, alpha(col, 0.35)))
                    .corner_radius(CornerRadius::same(th::R_MD))
                    .shadow(th::card_shadow())
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_width(width - 28.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(ic).font(th::regular(17.0)).color(col));
                            ui.add(egui::Label::new(egui::RichText::new(&t.text).font(th::medium(13.0)).color(th::TEXT)).wrap());
                        });
                    })
            });
            y -= resp.response.rect.height() + 8.0;
        }
    }

    // ------------------------------------------------------------------------------------------ pages

    fn play(&mut self, ui: &mut egui::Ui, t: f32) {
        let art = self.art.clone();
        let full_h = ui.ctx().screen_rect().height();
        let hero_h = (full_h * 0.56).clamp(360.0, 520.0) - 40.0;
        let first_run = self.game_dir().as_os_str().is_empty();
        // hero
        let a = ease_out(((ui.input(|i| i.time) - w::page_t0(ui.ctx())) as f32) / 0.5);
        ui.allocate_ui(Vec2::new(ui.available_width(), hero_h), |ui| {
            ui.set_min_height(hero_h);
            ui.multiply_opacity(a);
            ui.add_space(hero_h * 0.22 + (1.0 - a) * 16.0);
            ui.horizontal(|ui| {
                if let Some((tex, _)) = &art.logo {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::hover());
                    w::glow(ui.painter(), r, th::R_LG, Color32::BLACK, 1.0);
                    ui.painter().add(egui::epaint::RectShape::filled(r, CornerRadius::same(th::R_LG), Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
                    ui.painter().rect_stroke(r, CornerRadius::same(th::R_LG), th::stroke(1.0, th::HAIRLINE_HI), StrokeKind::Inside);
                    ui.add_space(th::S1);
                }
                ui.vertical(|ui| {
                    let caption = self.visuals.publisher.clone().unwrap_or_else(|| "triumph-rebuild".into());
                    w::text(ui, &caption.to_uppercase(), th::caption(), th::TEXT_2);
                    let name = self.visuals.name.clone().unwrap_or_else(|| if first_run { "Welcome".into() } else { "Your title".into() });
                    w::text(ui, &name, th::display(), th::TEXT);
                });
            });
            ui.add_space(4.0);
            let c = &self.model.config;
            let sub = if first_run {
                "Pick the game folder below to get started. It is only ever read.".to_string()
            } else {
                format!(
                    "{} x {}  ·  {}  ·  {} {}  ·  profile {}",
                    c.int("display.width"),
                    c.int("display.height"),
                    c.choice("display.window"),
                    c.choice("upscaler.backend").to_uppercase(),
                    c.choice("upscaler.mode").replace('_', " "),
                    if c.profile.is_empty() { "default" } else { &c.profile }
                )
            };
            ui.label(egui::RichText::new(sub).font(th::regular(14.5)).color(th::TEXT_2));
            ui.add_space(th::S3);
            ui.horizontal(|ui| {
                let running = self.running.is_some();
                let (ic, label) = if running { (icon::CIRCLE_NOTCH, "Running") } else { (icon::PLAY, "Launch") };
                if w::primary_button(ui, ic, label, Vec2::new(220.0, 56.0), !running && !first_run).on_hover_text("Ctrl+Enter").clicked() {
                    self.launch(false);
                }
                ui.add_space(th::S1);
                if running {
                    if w::danger_button(ui, icon::STOP, "Stop", true).clicked() {
                        if let Some(r) = &mut self.running {
                            r.stop();
                        }
                    }
                }
                ui.add_space(th::S1);
                ui.vertical(|ui| {
                    ui.add_space(13.0);
                    match self.system_ready() {
                        None => {
                            w::skeleton(ui, Vec2::new(150.0, 30.0));
                        }
                        Some(true) => {
                            w::status_pill(ui, "System ready", th::SUCCESS);
                        }
                        Some(false) => {
                            let n = self.checks.iter().filter(|c| c.status == syscheck::Status::Fail).count();
                            if w::status_pill(ui, &format!("{n} check{} need attention", if n == 1 { "" } else { "s" }), th::WARNING)
                                .interact(Sense::click())
                                .on_hover_text("Open the system check")
                                .clicked()
                            {
                                let ctx = ui.ctx().clone();
                                self.set_tab(&ctx, Tab::Diagnostics);
                            }
                        }
                    }
                });
            });
        });
        ui.add_space(th::S2);

        // stats
        let fps = play::last_fps(&self.log);
        let vram = self.adapter_detail("VRAM").map(|d| d.split_whitespace().next().unwrap_or("").to_string());
        let driver = self.adapter_detail("Driver").map(|d| d.split_whitespace().next().unwrap_or("").to_string());
        let gpu = self.adapter_detail("GPU").map(|d| d.split(" (").next().unwrap_or("").to_string());
        let stats: [(&str, Option<String>, &str, String); 3] = [
            (icon::GAUGE, fps.map(|f| format!("{f:.1}")), "fps", if self.running.is_some() { "this session".into() } else { "last session".into() }),
            (icon::GRAPHICS_CARD, vram, "GiB VRAM", gpu.clone().unwrap_or_default()),
            (icon::WRENCH, driver, "driver", "graphics driver".into()),
        ];
        let checking = self.check_rx.is_some();
        ui.columns(3, |cols| {
            for (i, (ic, value, unit, sub)) in stats.iter().enumerate() {
                let ui = &mut cols[i];
                w::card(ui, &art, i, None, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(*ic).font(th::regular(15.0)).color(th::ACCENT));
                        w::text(ui, &unit.to_uppercase(), th::caption(), th::TEXT_2);
                    });
                    ui.add_space(4.0);
                    match value {
                        Some(v) => {
                            w::text(ui, v, th::numerals(), th::TEXT);
                        }
                        None if checking && i > 0 => w::skeleton(ui, Vec2::new(110.0, 40.0)),
                        None => {
                            w::text(ui, "—", th::numerals(), th::TEXT_3);
                        }
                    }
                    ui.label(egui::RichText::new(if sub.is_empty() { " " } else { sub }).font(th::small()).color(th::TEXT_3));
                });
            }
        });

        if first_run {
            w::card(ui, &art, 3, Some((icon::FOLDER_OPEN, "First run: where is the game?")), |ui| {
                w::muted(ui, "Pick the folder the title was installed to. The launcher only reads it; everything it writes goes to its own folder.");
                ui.add_space(th::S1);
                for (i, s) in model::settings_in(Tab::Play).enumerate() {
                    setting_row(ui, &mut self.model, s, i == 0);
                }
            });
        }

        // console
        let status = if self.running.is_some() { ("live", th::SUCCESS) } else { ("idle", th::TEXT_3) };
        let session = self.session.clone();
        let log_path = self.home.frame_log();
        let mut run_proof = false;
        w::card(ui, &art, 4, None, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::TERMINAL_WINDOW).font(th::regular(17.0)).color(th::ACCENT));
                w::text(ui, "Frame line", th::h2(), th::TEXT);
                w::pill(ui, status.0, status.1, self.running.is_some());
                ui.label(egui::RichText::new(&session).font(th::small()).color(th::TEXT_3));
            });
            ui.label(egui::RichText::new(log_path.display().to_string()).font(th::mono(11.0)).color(th::TEXT_3));
            ui.add_space(th::S1);
            egui::Frame::new()
                .fill(Color32::from_rgba_premultiplied(3, 4, 6, 220))
                .stroke(th::stroke(1.0, th::HAIRLINE))
                .corner_radius(CornerRadius::same(th::R_MD))
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if self.log.is_empty() {
                        ui.label(egui::RichText::new("$ waiting for a session").font(th::mono(12.5)).color(th::TEXT_3));
                        ui.label(
                            egui::RichText::new("Launch, and the layer's frame line lands here once a second: fps · ms · draws · dropped.")
                                .font(th::regular(12.5))
                                .color(th::TEXT_3),
                        );
                    } else {
                        egui::ScrollArea::vertical().id_salt("log").max_height(220.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                            for l in &self.log {
                                let col = if l.contains("fps") { th::TEXT } else { th::TEXT_2 };
                                ui.label(egui::RichText::new(l).font(th::mono(12.5)).color(col));
                            }
                        });
                    }
                    let blink = (t * 2.0).sin() > 0.0;
                    if self.running.is_some() && blink {
                        ui.label(egui::RichText::new("▌").font(th::mono(12.5)).color(th::ACCENT));
                    }
                });
            ui.add_space(th::S1);
            egui::CollapsingHeader::new(egui::RichText::new("Run a proof binary instead (until the layer runs the title)").font(th::medium(13.0)).color(th::TEXT_2))
                .id_salt("proof")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.proof_exe).font(th::mono(12.5)).hint_text("path to a proof .exe").desired_width(380.0).margin(egui::Margin::symmetric(10, 7)));
                        if w::button(ui, icon::FOLDER_OPEN, "Browse", true).clicked() {
                            if let Some(p) = rfd::FileDialog::new().add_filter("exe", &["exe"]).pick_file() {
                                self.proof_exe = p.display().to_string();
                            }
                        }
                        ui.add(egui::TextEdit::singleline(&mut self.proof_args).font(th::mono(12.5)).desired_width(120.0).margin(egui::Margin::symmetric(10, 7)));
                        if w::button(ui, icon::PLAY, "Run", self.running.is_none() && !self.proof_exe.trim().is_empty()).clicked() {
                            run_proof = true;
                        }
                    });
                });
        });
        if run_proof {
            self.launch(true);
        }
        if !self.title_note.is_empty() {
            ui.label(egui::RichText::new(&self.title_note).font(th::small()).color(th::TEXT_3));
        }
    }

    fn title_page(&mut self, ui: &mut egui::Ui) {
        let art = self.art.clone();
        page_header(ui, Tab::Title);
        let next = settings_cards(ui, &art, &mut self.model, Tab::Title, 0);
        let src = self.title_source();
        let mut write = false;
        w::card(ui, &art, next, Some((icon::FILE_TEXT, "Where it is written")), |ui| {
            let row = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.horizontal(|ui| {
                    ui.add_sized([120.0, 20.0], egui::Label::new(egui::RichText::new(k).font(th::medium(13.0)).color(th::TEXT_2)));
                    ui.label(egui::RichText::new(v).font(th::mono(12.0)).color(th::TEXT));
                });
            };
            match &src {
                Some(p) => row(ui, "title's file", p.display().to_string()),
                None => row(ui, "title's file", "not found under the game folder: set REBUILD_TITLE_SETTINGS".into()),
            }
            row(ui, "copy", title::overlay_copy(&self.home.overlay_dir()).display().to_string());
            ui.add_space(th::S1);
            ui.horizontal(|ui| {
                w::pill(ui, &format!("{} original never written", icon::SHIELD_CHECK), th::SUCCESS, false);
                ui.add_space(th::S1);
                if w::button(ui, icon::FLOPPY_DISK, "Write the copy now", src.is_some()).clicked() {
                    write = true;
                }
            });
            if !self.title_note.is_empty() {
                ui.add_space(4.0);
                w::muted(ui, &self.title_note);
            }
        });
        if write {
            let ok = self.write_title_copy();
            self.toast(if ok { Tone::Good } else { Tone::Bad }, if ok { "Copy written; the original is unchanged" } else { "Copy not written" });
        }
    }

    fn mods_page(&mut self, ui: &mut egui::Ui) {
        let art = self.art.clone();
        page_header(ui, Tab::Mods);
        let mods_dir = self.home.mods_dir();
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(mods_dir.display().to_string()).font(th::mono(12.0)).color(th::TEXT_3));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, icon::ARROWS_CLOCKWISE, "Refresh", true).clicked() {
                    refresh = true;
                }
            });
        });
        ui.add_space(th::S2);
        if refresh {
            self.refresh_mods();
            self.toast(Tone::Info, format!("{} mods found", self.mods.iter().filter(|m| m.present).count()));
        }
        if self.mods.is_empty() {
            w::card(ui, &art, 0, None, |ui| {
                empty_state(
                    ui,
                    icon::PUZZLE_PIECE,
                    "No mods yet",
                    "Make a folder in the mods folder above: each folder is one mod, and its files replace the title's by path.",
                );
            });
            return;
        }
        // conflicts per mod
        let count = |name: &str| self.conflicts.iter().filter(|c| c.providers.iter().any(|p| p == name)).count();
        let wins = |name: &str| self.conflicts.iter().filter(|c| c.winner == name).count();
        let mut changed = false;
        let mut moved: Option<(usize, usize)> = None;
        let n = self.mods.len();
        let dragging = egui::DragAndDrop::has_any_payload(ui.ctx());
        for i in 0..n {
            let (c, wn) = (count(&self.mods[i].name), wins(&self.mods[i].name));
            let m = &mut self.mods[i];
            let resp = w::card(ui, &art, i, None, |ui| {
                ui.horizontal(|ui| {
                    let handle = ui.dnd_drag_source(Id::new(("mod-drag", i)), i, |ui| {
                        ui.label(egui::RichText::new(icon::DOTS_SIX_VERTICAL).font(th::regular(20.0)).color(th::TEXT_3));
                    });
                    handle.response.on_hover_cursor(egui::CursorIcon::Grab).on_hover_text("Drag to reorder");
                    ui.label(egui::RichText::new(format!("{:02}", i + 1)).font(th::mono(13.0)).color(th::TEXT_3));
                    ui.add_space(4.0);
                    changed |= w::toggle(ui, &mut m.enabled).changed();
                    ui.add_space(4.0);
                    let name_col = if m.present { th::TEXT } else { th::TEXT_3 };
                    ui.label(egui::RichText::new(&m.name).font(th::semibold(15.0)).color(name_col));
                    if !m.present {
                        w::pill(ui, "folder missing", th::DANGER, false);
                    } else if !m.enabled {
                        w::pill(ui, "off", th::TEXT_3, false);
                    }
                    if m.enabled && c > 0 {
                        w::pill(ui, &format!("{} {c} conflict{}", icon::WARNING, if c == 1 { "" } else { "s" }), th::WARNING, false);
                        if wn > 0 {
                            w::pill(ui, &format!("wins {wn}"), th::SUCCESS, false);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if w::button(ui, icon::CARET_DOUBLE_RIGHT, "", i + 1 < n).on_hover_text("Later (wins more)").clicked() {
                            moved = Some((i, i + 1));
                        }
                        if w::button(ui, icon::CARET_DOUBLE_LEFT, "", i > 0).on_hover_text("Earlier").clicked() {
                            moved = Some((i, i - 1));
                        }
                    });
                });
                ui.min_rect()
            });
            let zone = Rect::from_min_max(Pos2::new(resp.left() - th::S3, resp.top() - th::S3), Pos2::new(resp.right() + th::S3, resp.bottom() + th::S3));
            let z = ui.interact(zone, Id::new(("mod-drop", i)), Sense::hover());
            if dragging && z.dnd_hover_payload::<usize>().is_some() {
                ui.painter().rect_stroke(zone, CornerRadius::same(th::R_LG), th::stroke(1.5, th::ACCENT), StrokeKind::Inside);
            }
            if let Some(from) = z.dnd_release_payload::<usize>() {
                moved = Some((*from, i));
            }
        }
        if let Some((from, to)) = moved {
            if from != to && from < n && to < n {
                let m = self.mods.remove(from);
                self.mods.insert(to, m);
                changed = true;
            }
        }
        if changed {
            self.mods_changed();
        }
        w::card(ui, &art, n, Some((icon::STACK, "Contested files")), |ui| {
            if self.conflicts.is_empty() {
                w::muted(ui, "No two enabled mods replace the same file.");
            }
            for (k, c) in self.conflicts.iter().enumerate() {
                if k > 0 {
                    let r = ui.available_rect_before_wrap();
                    ui.painter().hline(r.x_range(), r.top(), th::stroke(1.0, th::HAIRLINE));
                    ui.add_space(th::S1);
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&c.path).font(th::mono(12.5)).color(th::TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        w::pill(ui, &format!("{} wins", c.winner), th::SUCCESS, false);
                        ui.label(egui::RichText::new(c.providers.join("  →  ")).font(th::small()).color(th::TEXT_2));
                    });
                });
            }
        });
    }

    fn diagnostics(&mut self, ui: &mut egui::Ui, now: f64) {
        let art = self.art.clone();
        page_header(ui, Tab::Diagnostics);
        let next = settings_cards(ui, &art, &mut self.model, Tab::Diagnostics, 0);
        let mut rerun = false;
        let running = self.check_rx.is_some();
        w::card(ui, &art, next, Some((icon::SHIELD_CHECK, "System check")), |ui| {
            ui.horizontal(|ui| {
                w::muted(ui, "GPU, driver, tiled resources, shader model, VRAM, Resizable BAR and DXC.");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button(ui, icon::ARROWS_CLOCKWISE, "Run again   F5", !running).clicked() {
                        rerun = true;
                    }
                });
            });
            ui.add_space(th::S2);
            if running || self.checks.is_empty() {
                for _ in 0..8 {
                    ui.horizontal(|ui| {
                        w::skeleton(ui, Vec2::new(24.0, 24.0));
                        w::skeleton(ui, Vec2::new(140.0, 16.0));
                        w::skeleton(ui, Vec2::new(260.0, 16.0));
                    });
                    ui.add_space(th::S1);
                }
                return;
            }
            for (i, c) in self.checks.iter().enumerate() {
                let age = (now - self.checks_at) as f32 - 0.06 * i as f32;
                let k = (age / 0.35).clamp(0.0, 1.0);
                if k < 1.0 {
                    ui.ctx().request_repaint();
                }
                // a pop with a little overshoot
                let pop = if k <= 0.0 { 0.0 } else { 1.0 + 0.25 * (std::f32::consts::PI * k).sin() * (1.0 - k) };
                let (ic, col) = match c.status {
                    syscheck::Status::Pass => (icon::CHECK, th::SUCCESS),
                    syscheck::Status::Fail => (icon::X, th::DANGER),
                    syscheck::Status::Skipped => (icon::MINUS, th::WARNING),
                };
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                    let rad = 12.0 * pop.min(1.25) * ease_out(k);
                    if rad > 0.5 {
                        ui.painter().circle_filled(r.center(), rad + 5.0, alpha(col, 0.10 * ease_out(k)));
                        ui.painter().circle_filled(r.center(), rad, alpha(col, 0.22));
                        ui.painter().circle_stroke(r.center(), rad, th::stroke(1.0, alpha(col, 0.6)));
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, ic, th::regular(14.0 * pop.min(1.2)), alpha(col, ease_out(k)));
                    }
                    ui.vertical(|ui| {
                        ui.multiply_opacity(ease_out(k));
                        ui.horizontal(|ui| {
                            ui.add_sized([150.0, 20.0], egui::Label::new(egui::RichText::new(c.name).font(th::semibold(14.0)).color(th::TEXT)));
                            ui.label(egui::RichText::new(&c.detail).font(th::regular(13.5)).color(th::TEXT_2));
                        });
                        if !c.fix.is_empty() {
                            ui.label(egui::RichText::new(format!("{}  {}", icon::WRENCH, c.fix)).font(th::small()).color(col));
                        }
                    });
                });
                ui.add_space(th::S1);
            }
        });
        if rerun {
            self.start_check();
        }
    }

    fn load_profile(&mut self, name: &str) {
        match self.home.load_profile(name) {
            Ok(c) => {
                self.import_problems = c.problems.clone();
                self.model = Model::new(c);
                self.refresh_mods();
                self.toast(Tone::Info, format!("Profile “{name}” loaded. Save to make it active."));
            }
            Err(e) => self.toast(Tone::Bad, e),
        }
    }

    fn profiles_page(&mut self, ui: &mut egui::Ui) {
        let art = self.art.clone();
        page_header(ui, Tab::Profiles);
        let mut load = None;
        let mut save_as = false;
        let mut export = false;
        let mut import = false;
        w::card(ui, &art, 0, Some((icon::FLOPPY_DISK, "Save these settings as a profile")), |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.profile_name).hint_text("profile name").desired_width(280.0).margin(egui::Margin::symmetric(10, 8)));
                if w::primary_button(ui, icon::FLOPPY_DISK, "Save profile", Vec2::new(160.0, 34.0), true).clicked() {
                    save_as = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button(ui, icon::DOWNLOAD_SIMPLE, "Import", true).clicked() {
                        import = true;
                    }
                    if w::button(ui, icon::UPLOAD_SIMPLE, "Export", true).clicked() {
                        export = true;
                    }
                });
            });
            ui.add_space(th::S2);
            let r = ui.available_rect_before_wrap();
            ui.painter().hline(r.x_range(), r.top(), th::stroke(1.0, th::HAIRLINE));
            ui.add_space(th::S2);
            ui.horizontal(|ui| {
                let mut stable = self.model.config.stable_only;
                if w::toggle(ui, &mut stable).changed() {
                    let experimental_set = rebuild_config::REGISTRY
                        .iter()
                        .any(|s| s.tier == rebuild_config::Tier::Experimental && self.model.config.get(s.key) != s.default.value());
                    if stable && experimental_set {
                        self.toasts.push(Toast { text: "Reset the experimental switches first".into(), tone: Tone::Bad, at: -1.0 });
                    } else {
                        self.model.config.stable_only = stable;
                    }
                }
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Stable only").font(th::medium(14.5)).color(th::TEXT));
                    w::muted(ui, "Experimental switches are refused in this profile.");
                });
            });
        });
        if self.profiles.is_empty() {
            w::card(ui, &art, 1, None, |ui| {
                empty_state(ui, icon::USER_CIRCLE, "No profiles yet", "Save the settings above under a name, and switch between them here or on Play.");
            });
        } else {
            let current = self.model.config.profile.clone();
            let per_row = ((ui.available_width() + th::S2) / (260.0 + th::S2)).floor().max(1.0) as usize;
            for (row, chunk) in self.profiles.chunks(per_row).enumerate() {
                ui.columns(per_row, |cols| {
                    for (k, p) in chunk.iter().enumerate() {
                        let ui = &mut cols[k];
                        w::card(ui, &art, 1 + row * per_row + k, None, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(icon::USER_CIRCLE).font(th::regular(22.0)).color(th::ACCENT));
                                ui.label(egui::RichText::new(p).font(th::semibold(15.0)).color(th::TEXT));
                                if *p == current {
                                    w::pill(ui, "active", th::SUCCESS, true);
                                }
                            });
                            ui.add_space(th::S1);
                            if w::button(ui, icon::ARROWS_CLOCKWISE, "Load", *p != current).clicked() {
                                load = Some(p.clone());
                            }
                        });
                    }
                });
            }
        }
        for p in self.import_problems.clone() {
            ui.label(egui::RichText::new(format!("{}  {p}", icon::WARNING_CIRCLE)).font(th::small()).color(th::DANGER));
        }
        if save_as {
            match self.home.save_profile(&self.profile_name, &self.model.config) {
                Ok(_) => {
                    self.model.config.profile = self.profile_name.trim().to_string();
                    self.profiles = self.home.profiles();
                    self.toast(Tone::Good, format!("Profile “{}” saved", self.profile_name.trim()));
                }
                Err(e) => self.toast(Tone::Bad, e),
            }
        }
        if let Some(p) = load {
            self.load_profile(&p);
        }
        if export {
            if let Some(p) = rfd::FileDialog::new().add_filter("toml", &["toml"]).set_file_name("rebuild.toml").save_file() {
                match home::export(&self.model.config, &p) {
                    Ok(()) => self.toast(Tone::Good, format!("Exported {}", p.display())),
                    Err(e) => self.toast(Tone::Bad, e),
                }
            }
        }
        if import {
            if let Some(p) = rfd::FileDialog::new().add_filter("toml", &["toml"]).pick_file() {
                match home::load_file(&p) {
                    Ok(c) => {
                        self.import_problems = c.problems.clone();
                        self.model = Model::new(c);
                        self.refresh_mods();
                        self.toast(Tone::Info, format!("Imported {}", p.display()));
                    }
                    Err(e) => self.toast(Tone::Bad, e),
                }
            }
        }
    }

    fn about(&mut self, ui: &mut egui::Ui) {
        let art = self.art.clone();
        ui.add_space(th::S3);
        ui.horizontal(|ui| {
            let (mark, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
            w::glow(ui.painter(), mark, th::R_LG, th::ACCENT, 0.8);
            w::gradient_rrect(ui.painter(), mark, th::R_LG as f32, th::ACCENT, th::ACCENT_2);
            ui.painter().text(mark.center(), Align2::CENTER_CENTER, icon::ROCKET_LAUNCH, th::regular(30.0), Color32::WHITE);
            ui.add_space(th::S2);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                w::text(ui, "triumph-rebuild", th::display(), th::TEXT);
                ui.label(egui::RichText::new(format!("Launcher {}  ·  GPL-3.0-only", env!("CARGO_PKG_VERSION"))).font(th::regular(14.0)).color(th::TEXT_2));
            });
        });
        ui.add_space(th::S3);
        w::card(ui, &art, 0, Some((icon::SHIELD_CHECK, "What it never does")), |ui| {
            for line in [
                "No network and no telemetry: the launcher makes no connection of any kind.",
                "Never writes into the game folder: the title's settings are applied to a copy in its own overlay.",
                "Creates no GPU device while a measurement is running (REBUILD_MEASURE_LOCK).",
            ] {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon::CHECK).font(th::regular(15.0)).color(th::SUCCESS));
                    w::muted(ui, line);
                });
            }
        });
        let paths = [
            ("settings", self.home.rebuild_toml()),
            ("profiles", self.home.profiles_dir()),
            ("mods", self.home.mods_dir()),
            ("overlay", self.home.overlay_dir()),
            ("frame log", self.home.frame_log()),
        ];
        w::card(ui, &art, 1, Some((icon::FOLDER_OPEN, "Where things live")), |ui| {
            for (k, p) in paths {
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(k).font(th::medium(13.0)).color(th::TEXT_2)));
                    ui.label(egui::RichText::new(p.display().to_string()).font(th::mono(12.0)).color(th::TEXT));
                });
            }
        });
        w::card(ui, &art, 2, Some((icon::INFO, "Credits")), |ui| {
            for line in [
                "Inter 3.019, The Inter Project Authors (rsms/inter), SIL Open Font License 1.1",
                "Phosphor Icons via egui-phosphor, MIT",
                "Hack, Source Foundry (bundled with egui), MIT and Bitstream Vera licence",
                "egui and eframe, Emil Ernerfeldt and contributors, MIT or Apache-2.0",
            ] {
                w::muted(ui, line);
            }
            ui.add_space(4.0);
            ui.label(egui::RichText::new(format!("{} settings in the registry  ·  Ctrl+1…8 pages  ·  Ctrl+S save  ·  Ctrl+Enter launch  ·  F5 check  ·  Ctrl+B sidebar", rebuild_config::REGISTRY.len())).font(th::small()).color(th::TEXT_3));
        });
    }

    // ------------------------------------------------------------------------------------------ shots

    fn shots_step(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.shots else { return };
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let (name, ..) = &s.plan[s.step];
            let path = s.dir.join(format!("{name}.png"));
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
        let (_, tab, collapsed) = s.plan[s.step].clone();
        s.frames += 1;
        let first = s.frames == 1;
        // the first page waits out the splash and the check
        let wait = if s.step == 0 { 150 } else { 75 };
        let ready = s.frames >= wait;
        if ready {
            s.waiting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        if first {
            self.collapsed = collapsed;
            self.set_tab(ctx, tab);
            if tab == Tab::Play && collapsed {
                let now = ctx.input(|i| i.time);
                ctx.data_mut(|d| d.insert_temp(Id::new("page-t0"), now));
            }
        }
    }
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> Result<(), String> {
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
        if let Ok(hwnd) = FindWindowW(None, w!("triumph-rebuild")) {
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const _ as *const _, std::mem::size_of_val(&pref) as u32);
        }
    }
}

