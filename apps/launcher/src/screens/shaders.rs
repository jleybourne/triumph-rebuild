//! "Preparing shaders" (owner decision 2026-11-28): on first Play, or when a game is added, the launcher builds
//! the game's shader library from the user's own copy of the game (`rebuild_launcher::shaders`). Play waits
//! on this screen and launches when it is done; "Continue later" stops it (resumable), "Play now" launches at
//! once (the layer translates what is missing as it is needed), "In the background" goes back to Play while
//! it runs. Its state also shows on Diagnostics → Storage.

use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::App;
use eframe::egui::{self, Vec2};
use rebuild_launcher::shaders::{self, Job, Need, Phase, Progress};

impl App {
    /// The selected game's need, read once per game (and again after a build or a cache clear).
    pub fn shader_need(&mut self) -> Option<Need> {
        let id = self.library.selected.clone()?;
        if let Some((g, n)) = &self.shader_need {
            if *g == id {
                return Some(n.clone());
            }
        }
        let store = self.game_store()?;
        let n = shaders::need(&self.game_dir(), &store, &self.shader_dxc);
        self.shader_need = Some((id, n.clone()));
        Some(n)
    }

    /// The selected game's build, if one runs.
    pub fn shader_progress(&self) -> Option<Progress> {
        let j = self.shader_job.as_ref()?;
        (Some(&j.game) == self.library.selected.as_ref()).then(|| j.progress())
    }

    /// Starts (or resumes) the selected game's build, unless one already runs for it. A build of another
    /// game is stopped first (it resumes when that game is next played).
    pub fn start_shaders(&mut self) -> bool {
        let Some(id) = self.library.selected.clone() else { return false };
        if self.shader_job.as_ref().is_some_and(|j| j.game == id && !j.finished() && !j.cancelled()) {
            return true;
        }
        if let Some(j) = self.shader_job.take() {
            j.stop();
        }
        let Some(store) = self.game_store() else { return false };
        let Ok(dxc) = self.shader_dxc.clone() else { return false };
        self.shader_job = Some(Job::start(&id, &self.game_dir(), &store, dxc, self.shader_workers));
        self.shader_need = None;
        true
    }

    /// Stops a running build and waits for it (Play now, Clear cache, Remove).
    pub fn stop_shaders(&mut self) {
        if let Some(j) = self.shader_job.take() {
            j.stop();
        }
        self.shader_need = None;
    }

    /// Each frame: a finished build reports, and a waiting Play launches.
    pub fn poll_shaders(&mut self) {
        let Some(j) = self.shader_job.as_mut() else { return };
        let Some(r) = j.take_result() else { return };
        self.shader_job = None;
        self.shader_need = None;
        match r {
            Ok(r) if r.complete => {
                self.toast(Tone::Ok, format!("Shaders ready: {} programs in {}", r.in_library, crate::mmss(r.total.as_secs())));
                if self.launch_when_ready {
                    self.launch_when_ready = false;
                    self.preparing = false;
                    self.launch_now();
                }
            }
            Ok(r) => {
                self.toast(Tone::Accent, format!("Shaders paused at {} of {}: they continue next time", r.reused + r.compiled + r.refused, r.programs));
                self.preparing = false;
                self.launch_when_ready = false;
            }
            Err(e) => {
                self.toast(Tone::Danger, format!("Shader library: {e}. The game translates shaders while it runs instead."));
                self.preparing = false;
                self.launch_when_ready = false;
            }
        }
    }

    /// The Preparing shaders screen.
    pub fn preparing_view(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        let p = self.shader_progress().unwrap_or_default();
        let running = self.shader_job.is_some();
        let (mut later, mut now, mut background) = (false, false, false);
        let mut workers = self.shader_workers;
        let name = self.visuals.name.clone().unwrap_or_else(|| "this game".into());
        w::card(ui, None, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::CPU).font(th::icon(26.0)).color(c.accent));
                w::text(ui, "Preparing shaders", th::h1(), c.text);
            });
            ui.add_space(th::S1);
            w::muted(ui, &format!(
                "The first launch of {name} builds its shader library from your own copy of the game: every console shader is translated and compiled for this PC. It happens once, and again only after a game or launcher update. Later launches start at once."
            ));
            ui.add_space(th::S2);
            let (what, tone) = match p.phase {
                Phase::Starting | Phase::Reading => ("Reading the game's shaders…".to_string(), Tone::Accent),
                Phase::Translating => (format!("{} of {} programs", p.done, p.total), Tone::Accent),
                Phase::Writing => ("Writing the library…".to_string(), Tone::Accent),
                Phase::Done => ("Done".to_string(), Tone::Ok),
                Phase::Paused => (format!("Paused at {} of {}", p.done, p.total), Tone::Warn),
                Phase::Failed => (p.error.clone().unwrap_or_else(|| "failed".into()), Tone::Danger),
            };
            ui.horizontal(|ui| {
                w::text(ui, &what, th::h2(), c.text);
                if p.phase == Phase::Translating {
                    w::chip(ui, &shaders::eta_text(p.eta), tone);
                }
            });
            ui.add_space(4.0);
            ui.add(egui::ProgressBar::new(p.fraction()).desired_height(10.0).fill(c.accent));
            ui.add_space(4.0);
            w::muted(ui, &format!(
                "compiled {} · kept from before {} · not translated yet {} · DXC errors {} · {} elapsed · {} worker{}",
                p.compiled,
                p.reused,
                p.refused,
                p.failed,
                crate::mmss(p.elapsed.as_secs()),
                self.shader_job.as_ref().map_or(workers, |j| j.workers),
                if workers == 1 { "" } else { "s" }
            ));
            ui.add_space(th::S2);
            ui.horizontal(|ui| {
                if w::primary_button(ui, icon::PLAY, "Play now", Vec2::new(160.0, 40.0), true)
                    .on_hover_text("Launch now. Shaders not built yet are translated as the game first uses them, so the first minutes can stutter. The build continues next time.")
                    .clicked()
                {
                    now = true;
                }
                if w::button(ui, icon::STACK, "In the background", running).on_hover_text("Back to Play while the build runs; Launch waits for it").clicked() {
                    background = true;
                }
                if w::button(ui, icon::X, "Continue later", true).on_hover_text("Stop for now: everything built so far is kept, and the next Play continues from here").clicked() {
                    later = true;
                }
            });
            ui.add_space(th::S1);
            ui.horizontal(|ui| {
                w::muted(ui, "Workers");
                if w::button(ui, icon::MINUS, "", workers > 1).clicked() {
                    workers -= 1;
                }
                w::text(ui, &workers.to_string(), th::h2(), c.text);
                if w::button(ui, icon::PLUS, "", workers < shaders::max_workers()).clicked() {
                    workers += 1;
                }
                w::muted(ui, "one DXC each; fewer keeps the PC more responsive (applies when the build next starts)");
            });
        });
        self.shader_workers = workers;
        if later {
            self.stop_shaders();
            self.preparing = false;
            self.launch_when_ready = false;
        } else if now {
            self.stop_shaders();
            self.preparing = false;
            self.launch_when_ready = false;
            self.launch_now();
        } else if background {
            self.preparing = false;
            self.launch_when_ready = false;
        }
    }

    /// The Play band's shader line, and its action (prepare now / show progress).
    pub fn shader_band(&mut self, ui: &mut egui::Ui) {
        if self.fixture {
            return;
        }
        let Some(need) = self.shader_need() else { return };
        let p = self.shader_progress();
        let line = shaders::status_line(&need, p.as_ref());
        let tone = match (&need, &p) {
            (_, Some(p)) if p.phase == Phase::Failed => Tone::Danger,
            (_, Some(_)) => Tone::Accent,
            (Need::Ready(_), None) => Tone::Ok,
            (Need::Build(_), None) => Tone::Warn,
            (Need::OnDemand(_), None) => Tone::Muted,
        };
        let mut open = false;
        let mut start = false;
        ui.horizontal(|ui| {
            w::chip(ui, &line, tone);
            if p.is_some() {
                if w::link(ui, "Show").clicked() {
                    open = true;
                }
            } else if matches!(need, Need::Build(_)) && w::link(ui, "Prepare now").clicked() {
                start = true;
            }
        });
        if start && self.start_shaders() {
            open = true;
        }
        if open {
            self.preparing = true;
        }
    }

    /// Diagnostics → Storage: the shader library row.
    pub fn shader_storage_row(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        let Some(need) = self.shader_need() else { return };
        let p = self.shader_progress();
        let mut start = false;
        let mut pause = false;
        ui.horizontal(|ui| {
            w::fixed(ui, 118.0, egui::RichText::new("Shader library").font(th::medium(13.0)).color(c.text));
            ui.label(egui::RichText::new(shaders::status_line(&need, p.as_ref())).font(th::regular(13.0)).color(c.text_2));
            if p.is_some() {
                if w::button(ui, icon::STOP, "Pause", true).on_hover_text("Stop for now; it continues next time").clicked() {
                    pause = true;
                }
            } else if matches!(need, Need::Build(_)) && w::button(ui, icon::CPU, "Prepare now", self.running.is_none()).clicked() {
                start = true;
            }
        });
        if let Some(p) = &p {
            ui.add(egui::ProgressBar::new(p.fraction()).desired_height(6.0).fill(c.accent));
        }
        if pause {
            self.stop_shaders();
        }
        if start {
            self.start_shaders();
        }
    }
}
