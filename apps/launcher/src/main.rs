//! triumph-rebuild launcher: settings, profiles, title settings, mods, system check, and Play.
//! Every settings widget is generated from `rebuild_config::REGISTRY` (`docs/design/launcher.md`).
//!
//! `rebuild-launcher --check` prints the system check without opening a window.
//! `REBUILD_MEASURE_LOCK=<file>`: while that file exists, the check creates no D3D12 device.

#![cfg_attr(not(test), windows_subsystem = "windows")]

use eframe::egui;
use rebuild_config::{Config, Setting};
use rebuild_launcher::{
    home::{self, Home},
    model::{self, Edit, Model, Tab, Widget},
    mods, play, syscheck, title,
};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    if std::env::args().any(|a| a == "--check") {
        attach_console();
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
        std::process::exit(if fails == 0 { 0 } else { 1 });
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 680.0]).with_title("triumph-rebuild"),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("triumph-rebuild", options, Box::new(|_cc| Ok(Box::new(App::new())))) {
        eprintln!("launcher: {e}");
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

struct App {
    home: Home,
    tab: Tab,
    model: Model,
    saved: String,
    status: String,
    checks: Vec<syscheck::Check>,
    mods: Vec<mods::Entry>,
    conflicts: Vec<mods::Conflict>,
    profiles: Vec<String>,
    profile_name: String,
    import_problems: Vec<String>,
    running: Option<play::Running>,
    tail: Option<play::LogTail>,
    log: Vec<String>,
    proof_exe: String,
    proof_args: String,
    title_note: String,
}

impl App {
    fn new() -> App {
        let home = Home::locate();
        let (config, status) = match home.load() {
            Ok(c) if c.problems.is_empty() => (c, String::new()),
            Ok(c) => {
                let s = format!("rebuild.toml: {}", c.problems.join("; "));
                (c, s)
            }
            Err(e) => (Config::parse("").unwrap(), e),
        };
        let saved = config.to_toml();
        let mut app = App {
            profile_name: config.profile.clone(),
            model: Model::new(config),
            saved,
            status,
            checks: Vec::new(),
            mods: Vec::new(),
            conflicts: Vec::new(),
            profiles: home.profiles(),
            import_problems: Vec::new(),
            running: None,
            tail: None,
            log: Vec::new(),
            proof_exe: String::new(),
            proof_args: "--bare".into(),
            title_note: String::new(),
            tab: Tab::Play,
            home,
        };
        app.refresh_mods();
        if app.model.config.get("game.dir").to_string().is_empty() {
            app.checks = syscheck::run(&probe(&app.model.config));
        }
        app
    }

    fn dirty(&self) -> bool {
        self.model.config.to_toml() != self.saved
    }

    fn save(&mut self) -> bool {
        match self.model.save(&self.home.rebuild_toml()) {
            Ok(()) => {
                self.saved = self.model.config.to_toml();
                self.status = format!("saved {}", self.home.rebuild_toml().display());
                true
            }
            Err(e) => {
                self.status = e;
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

    fn title_source(&self) -> Option<PathBuf> {
        let explicit = std::env::var_os("REBUILD_TITLE_SETTINGS").map(PathBuf::from);
        let game = PathBuf::from(self.model.config.get("game.dir").to_string());
        title::find_source(explicit.as_deref(), &game)
    }

    /// U3 at launch: the edited copy of the title's settings.xml, in the launcher's own overlay.
    fn write_title_copy(&mut self) {
        let game = PathBuf::from(self.model.config.get("game.dir").to_string());
        self.title_note = match self.title_source() {
            None => "no settings.xml found: title settings not applied (set REBUILD_TITLE_SETTINGS)".into(),
            Some(src) => {
                let before = rebuild_launcher::hash_file(&src).ok();
                match title::write_copy(&src, Some(&game), &self.home.overlay_dir(), &self.model.config) {
                    Ok(dst) => {
                        let after = rebuild_launcher::hash_file(&src).ok();
                        format!(
                            "copy written to {}; original {} ({})",
                            dst.display(),
                            src.display(),
                            if before == after { "unchanged" } else { "CHANGED" }
                        )
                    }
                    Err(e) => e,
                }
            }
        };
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
            self.write_title_copy();
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
                self.status = "running".into();
            }
            Err(e) => self.status = e,
        }
    }

    fn poll_run(&mut self) {
        if let Some(t) = &mut self.tail {
            self.log.extend(t.poll());
            let n = self.log.len();
            if n > 2000 {
                self.log.drain(..n - 2000);
            }
        }
        if let Some(r) = &mut self.running {
            if let Some(s) = r.poll() {
                let secs = r.started.elapsed().as_secs_f32();
                if let Some(t) = &mut self.tail {
                    self.log.extend(t.poll());
                }
                self.status = format!("exited: {s} after {secs:.1} s");
                self.running = None;
            }
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui, tab: Tab) {
        for s in model::settings_in(tab) {
            setting_row(ui, &mut self.model, s);
        }
    }
}

fn setting_row(ui: &mut egui::Ui, m: &mut Model, s: &'static Setting) {
    let b = m.badges(s);
    ui.horizontal(|ui| {
        ui.add_sized([220.0, 18.0], egui::Label::new(s.label)).on_hover_text(s.key);
        let mut e = m.current(s);
        let changed = match (&mut e, model::widget(s)) {
            (Edit::Toggle(v), _) => ui.checkbox(v, "").changed(),
            (Edit::Slide(v), Widget::Slider { min, max }) => ui.add(egui::Slider::new(v, min..=max)).changed(),
            (Edit::Pick(i), Widget::Dropdown { options }) => {
                let before = *i;
                egui::ComboBox::from_id_salt(s.key).selected_text(options[*i]).show_ui(ui, |ui| {
                    for (k, o) in options.iter().enumerate() {
                        ui.selectable_value(i, k, *o);
                    }
                });
                *i != before
            }
            (Edit::Text(t), _) => {
                let mut ch = ui.add(egui::TextEdit::singleline(t).desired_width(320.0)).changed();
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        *t = p.display().to_string();
                        ch = true;
                    }
                }
                ch
            }
            _ => false,
        };
        if changed {
            let _ = m.apply(s, e);
        }
        ui.label(egui::RichText::new(b.apply).small().weak());
        if b.experimental {
            ui.colored_label(egui::Color32::from_rgb(230, 150, 0), "experimental");
        }
        if ui.add_enabled(b.changed, egui::Button::new("Reset").small()).on_hover_text("back to the default").clicked() {
            m.reset(s);
        }
    });
    if !s.help.is_empty() {
        ui.label(egui::RichText::new(s.help).small().weak());
    }
    if let Some((k, why)) = &m.refusal {
        if *k == s.key {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), why);
        }
    }
    ui.add_space(6.0);
}

fn check_table(ui: &mut egui::Ui, checks: &[syscheck::Check]) {
    egui::Grid::new("checks").striped(true).show(ui, |ui| {
        for c in checks {
            let (t, col) = match c.status {
                syscheck::Status::Pass => ("pass", egui::Color32::from_rgb(60, 170, 80)),
                syscheck::Status::Fail => ("fail", egui::Color32::from_rgb(220, 60, 60)),
                syscheck::Status::Skipped => ("skipped", egui::Color32::GRAY),
            };
            ui.colored_label(col, t);
            ui.label(c.name);
            ui.label(&c.detail);
            ui.label(egui::RichText::new(&c.fix).small());
            ui.end_row();
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_run();
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                for t in Tab::ALL {
                    ui.selectable_value(&mut self.tab, t, t.label());
                }
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let dirty = self.dirty();
                if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                    self.save();
                }
                ui.label(if dirty { "unsaved changes" } else { "saved" });
                ui.separator();
                ui.label(format!("profile: {}", self.model.config.profile));
                ui.separator();
                ui.label(&self.status);
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.tab {
                Tab::Play => self.play_tab(ui),
                Tab::Display => self.settings(ui, Tab::Display),
                Tab::Title => self.title_tab(ui),
                Tab::Features => self.settings(ui, Tab::Features),
                Tab::Mods => self.mods_tab(ui),
                Tab::Diagnostics => self.diagnostics_tab(ui),
                Tab::Profiles => self.profiles_tab(ui),
                Tab::About => about_tab(ui, &self.home),
            });
        });
    }
}

impl App {
    fn play_tab(&mut self, ui: &mut egui::Ui) {
        let first_run = self.model.config.get("game.dir").to_string().is_empty();
        if first_run {
            ui.heading("Setup");
            ui.label("Pick the title's folder. It is only read: nothing is ever written into it.");
        }
        self.settings(ui, Tab::Play);
        if ui.button("Run system check").clicked() {
            self.checks = syscheck::run(&probe(&self.model.config));
        }
        if !self.checks.is_empty() {
            check_table(ui, &self.checks);
        }
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Profile");
            let current = self.model.config.profile.clone();
            let mut pick: Option<String> = None;
            egui::ComboBox::from_id_salt("play-profile").selected_text(&current).show_ui(ui, |ui| {
                for p in &self.profiles {
                    if ui.selectable_label(*p == current, p).clicked() {
                        pick = Some(p.clone());
                    }
                }
            });
            if let Some(p) = pick {
                self.load_profile(&p);
            }
        });
        ui.horizontal(|ui| {
            let running = self.running.is_some();
            if ui.add_enabled(!running && !first_run, egui::Button::new("Launch")).clicked() {
                self.launch(false);
            }
            if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                if let Some(r) = &mut self.running {
                    r.stop();
                }
            }
        });
        ui.collapsing("Run a proof binary instead (until the layer runs the title)", |ui| {
            ui.horizontal(|ui| {
                ui.label("binary");
                ui.text_edit_singleline(&mut self.proof_exe);
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("exe", &["exe"]).pick_file() {
                        self.proof_exe = p.display().to_string();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("arguments");
                ui.text_edit_singleline(&mut self.proof_args);
            });
            if ui.add_enabled(self.running.is_none() && !self.proof_exe.trim().is_empty(), egui::Button::new("Run proof")).clicked() {
                self.launch(true);
            }
        });
        if !self.title_note.is_empty() {
            ui.label(egui::RichText::new(&self.title_note).small());
        }
        ui.separator();
        ui.label(format!("Frame line ({})", self.home.frame_log().display()));
        egui::ScrollArea::vertical().id_salt("log").max_height(260.0).stick_to_bottom(true).show(ui, |ui| {
            for l in &self.log {
                ui.monospace(l);
            }
        });
    }

    fn title_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("These are written, at launch, into a copy of the title's settings.xml in the launcher's overlay. The original is never touched.");
        ui.add_space(6.0);
        self.settings(ui, Tab::Title);
        match self.title_source() {
            Some(p) => ui.label(format!("title's file: {}", p.display())),
            None => ui.label("title's file: not found under the game folder (set REBUILD_TITLE_SETTINGS)"),
        };
        if ui.button("Write the copy now").clicked() {
            self.write_title_copy();
        }
        if !self.title_note.is_empty() {
            ui.label(&self.title_note);
        }
    }

    fn mods_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("Mods folder: {}", self.home.mods_dir().display()));
            if ui.button("Refresh").clicked() {
                self.refresh_mods();
            }
        });
        ui.label("Load order: top first; a later mod's file wins.");
        let mut changed = false;
        let mut up = None;
        let mut down = None;
        egui::Grid::new("mods").striped(true).show(ui, |ui| {
            let n = self.mods.len();
            for (i, m) in self.mods.iter_mut().enumerate() {
                ui.label(format!("{}", i + 1));
                changed |= ui.checkbox(&mut m.enabled, "").changed();
                if m.present {
                    ui.label(&m.name);
                } else {
                    ui.label(egui::RichText::new(format!("{} (folder missing)", m.name)).weak());
                }
                if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                    up = Some(i);
                }
                if ui.add_enabled(i + 1 < n, egui::Button::new("↓").small()).clicked() {
                    down = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = up {
            mods::move_up(&mut self.mods, i);
            changed = true;
        }
        if let Some(i) = down {
            mods::move_down(&mut self.mods, i);
            changed = true;
        }
        if changed {
            self.mods_changed();
        }
        ui.separator();
        if self.conflicts.is_empty() {
            ui.label("No conflicts between enabled mods.");
        } else {
            ui.label(format!("{} contested files:", self.conflicts.len()));
            egui::Grid::new("conflicts").striped(true).show(ui, |ui| {
                for c in &self.conflicts {
                    ui.monospace(&c.path);
                    ui.label(c.providers.join(", "));
                    ui.label(format!("wins: {}", c.winner));
                    ui.end_row();
                }
            });
        }
    }

    fn diagnostics_tab(&mut self, ui: &mut egui::Ui) {
        self.settings(ui, Tab::Diagnostics);
        ui.separator();
        if ui.button("Run system check").clicked() {
            self.checks = syscheck::run(&probe(&self.model.config));
        }
        check_table(ui, &self.checks);
    }

    fn load_profile(&mut self, name: &str) {
        match self.home.load_profile(name) {
            Ok(c) => {
                self.import_problems = c.problems.clone();
                self.model = Model::new(c);
                self.refresh_mods();
                self.status = format!("profile {name} loaded (Save to make it active)");
            }
            Err(e) => self.status = e,
        }
    }

    fn profiles_tab(&mut self, ui: &mut egui::Ui) {
        let mut stable = self.model.config.stable_only;
        if ui.checkbox(&mut stable, "Stable only (experimental switches refused)").changed() {
            let experimental_set = rebuild_config::REGISTRY
                .iter()
                .any(|s| s.tier == rebuild_config::Tier::Experimental && self.model.config.get(s.key) != s.default.value());
            if stable && experimental_set {
                self.status = "reset the experimental switches first".into();
            } else {
                self.model.config.stable_only = stable;
            }
        }
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.profile_name);
            if ui.button("Save as profile").clicked() {
                match self.home.save_profile(&self.profile_name, &self.model.config) {
                    Ok(p) => {
                        self.model.config.profile = self.profile_name.trim().to_string();
                        self.status = format!("saved {}", p.display());
                        self.profiles = self.home.profiles();
                    }
                    Err(e) => self.status = e,
                }
            }
        });
        ui.separator();
        let mut load = None;
        for p in &self.profiles {
            ui.horizontal(|ui| {
                ui.label(p);
                if ui.small_button("Load").clicked() {
                    load = Some(p.clone());
                }
            });
        }
        if let Some(p) = load {
            self.load_profile(&p);
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Export…").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("toml", &["toml"]).set_file_name("rebuild.toml").save_file() {
                    self.status = match home::export(&self.model.config, &p) {
                        Ok(()) => format!("exported {}", p.display()),
                        Err(e) => e,
                    };
                }
            }
            if ui.button("Import…").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("toml", &["toml"]).pick_file() {
                    match home::load_file(&p) {
                        Ok(c) => {
                            self.import_problems = c.problems.clone();
                            self.model = Model::new(c);
                            self.refresh_mods();
                            self.status = format!("imported {}", p.display());
                        }
                        Err(e) => self.status = e,
                    }
                }
            }
        });
        for p in &self.import_problems {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), p);
        }
    }
}

fn about_tab(ui: &mut egui::Ui, home: &Home) {
    ui.heading("triumph-rebuild launcher");
    ui.label(format!("version {} · GPL-3.0-only", env!("CARGO_PKG_VERSION")));
    ui.label("No network, no telemetry. The game folder is only read.");
    ui.add_space(8.0);
    ui.label(format!("settings   {}", home.rebuild_toml().display()));
    ui.label(format!("profiles   {}", home.profiles_dir().display()));
    ui.label(format!("mods       {}", home.mods_dir().display()));
    ui.label(format!("overlay    {}", home.overlay_dir().display()));
    ui.label(format!("frame log  {}", home.frame_log().display()));
    ui.add_space(8.0);
    ui.label(format!("{} settings in the registry", rebuild_config::REGISTRY.len()));
}
