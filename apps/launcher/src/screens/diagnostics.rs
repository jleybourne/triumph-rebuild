//! Diagnostics: the system check (Run again), crash reports (view, copy for a bug report), logs in
//! tabs, frame captures, the D3D12 debug layer switch, and the sessions with their retention. Also the
//! proof-binary runner and "about" (where things live, credits).

use super::{heading, setting_row, split, well};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{open_folder, App, Dialog};
use eframe::egui::{self, Align, Layout, Sense, Vec2};
use rebuild_launcher::{play, recommend, session, syscheck};
use std::path::PathBuf;

const LOG_TABS: [&str; 3] = ["Frame line", "session.toml", "summary.toml"];

impl App {
    pub fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        heading(ui, "Diagnostics", "The system check, what each session left behind, and the switches for finding out why.");
        let mut rerun = false;
        let mut view: Option<Dialog> = None;
        let mut open: Option<PathBuf> = None;
        let checking = self.check_rx.is_some();
        let checks = self.checks.clone();
        let entries = self.sessions.clone();
        let (specs, rec, reading) = (self.specs.clone(), self.rec.clone(), self.specs_rx.is_some());
        split(
            ui,
            0.5,
            |ui| {
                w::card(ui, None, |ui| {
                    ui.horizontal(|ui| {
                        w::text(ui, "System check", th::h2(), c.text);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::button(ui, icon::ARROWS_CLOCKWISE, "Run again", !checking).on_hover_text("F5").clicked() {
                                rerun = true;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if checks.is_empty() {
                        ui.label(egui::RichText::new("Checking…").font(th::small()).color(c.muted));
                    }
                    for ch in &checks {
                        let (ic, tone) = match ch.status {
                            syscheck::Status::Pass => (icon::CHECK_CIRCLE, Tone::Ok),
                            syscheck::Status::Fail => (icon::X_CIRCLE, Tone::Danger),
                            syscheck::Status::Skipped => (icon::MINUS, Tone::Warn),
                        };
                        ui.horizontal_top(|ui| {
                            ui.label(egui::RichText::new(ic).font(th::icon(16.0)).color(tone.colours().0));
                            w::fixed(ui, 118.0, egui::RichText::new(ch.name).font(th::medium(13.0)).color(c.text));
                            ui.vertical(|ui| {
                                ui.add(egui::Label::new(egui::RichText::new(&ch.detail).font(th::regular(13.0)).color(c.text_2)).wrap());
                                if !ch.fix.is_empty() {
                                    ui.add(egui::Label::new(egui::RichText::new(format!("{}  {}", icon::WRENCH, ch.fix)).font(th::small()).color(tone.colours().0)).wrap());
                                }
                            });
                        });
                    }
                });
                ui.add_space(th::S2);
                this_pc_card(ui, specs.as_ref(), rec.as_ref(), reading);
                ui.add_space(th::S2);
                w::card(ui, Some("Crash reports"), |ui| {
                    let crashes: Vec<(&session::Entry, &PathBuf)> = entries.iter().flat_map(|e| e.crashes.iter().map(move |p| (e, p))).collect();
                    if crashes.is_empty() {
                        w::muted(ui, "None. When the layer or the title crashes, its report lands in the session's folder (crash-*.txt) and here.");
                    }
                    for (i, (e, p)) in crashes.iter().enumerate().take(30) {
                        if i > 0 {
                            w::rule(ui);
                        }
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon::BUG).font(th::icon(15.0)).color(c.danger));
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(format!("{} · {}", e.profile(), p.file_name().and_then(|n| n.to_str()).unwrap_or(""))).font(th::medium(13.0)).color(c.text));
                                ui.label(egui::RichText::new(session::short_time(e.started)).font(th::small()).color(c.muted));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if w::button(ui, icon::COPY, "", true).on_hover_text("Copy for a bug report").clicked() {
                                    ui.ctx().copy_text(session::bug_report(e, p));
                                }
                                if w::button(ui, icon::FILE_TEXT, "View", true).clicked() {
                                    view = Some(Dialog::Viewer {
                                        title: p.display().to_string(),
                                        text: std::fs::read_to_string(p).unwrap_or_else(|e| e.to_string()),
                                        copy: Some(session::bug_report(e, p)),
                                    });
                                }
                            });
                        });
                    }
                });
                ui.add_space(th::S2);
                w::card(ui, Some("Frame captures"), |ui| {
                    let caps: Vec<(&session::Entry, &PathBuf)> = entries.iter().flat_map(|e| e.captures.iter().map(move |p| (e, p))).collect();
                    if caps.is_empty() {
                        w::muted(ui, "None yet. Capture frame (while the title runs) asks the layer for one; it lands in the session's captures folder.");
                    }
                    for (e, p) in caps.iter().take(20) {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon::CAMERA).font(th::icon(14.0)).color(c.accent));
                            ui.label(egui::RichText::new(format!("{} · {}", e.profile(), p.file_name().and_then(|n| n.to_str()).unwrap_or(""))).font(th::regular(13.0)).color(c.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if w::button(ui, icon::FOLDER_OPEN, "", true).on_hover_text("Open the captures folder").clicked() {
                                    open = Some(e.dir.join(session::CAPTURES));
                                }
                            });
                        });
                    }
                });
            },
            |ui| {
                w::card(ui, Some("Logs"), |ui| {
                    // which session: the chosen one, else the newest
                    let chosen = self.log_session.clone().filter(|d| entries.iter().any(|e| &e.dir == d)).or_else(|| entries.first().map(|e| e.dir.clone()));
                    let label = chosen.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "no session yet".into());
                    egui::ComboBox::from_id_salt("log-session").width(ui.available_width().min(360.0)).selected_text(label).show_ui(ui, |ui| {
                        for e in entries.iter().take(100) {
                            if ui.selectable_label(chosen.as_ref() == Some(&e.dir), &e.name).clicked() {
                                self.log_session = Some(e.dir.clone());
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        for (k, t) in LOG_TABS.iter().enumerate() {
                            let on = self.log_tab == k;
                            let g = ui.painter().layout_no_wrap(t.to_string(), if on { th::semibold(12.5) } else { th::medium(12.5) }, c.text);
                            let (r, resp) = ui.allocate_exact_size(g.size() + Vec2::new(16.0, 12.0), Sense::click());
                            ui.painter().galley(r.center() - g.size() / 2.0, g, if on { c.text } else { c.muted });
                            if on {
                                ui.painter().hline(r.x_range(), r.bottom() - 1.0, th::stroke(2.0, c.accent));
                            }
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                self.log_tab = k;
                            }
                        }
                    });
                    let file = match self.log_tab {
                        0 => session::FRAME_LOG,
                        1 => session::SESSION_TOML,
                        _ => session::SUMMARY_TOML,
                    };
                    let lines: Vec<String> = chosen.as_ref().map(|d| play::read_last(&d.join(file))).unwrap_or_default();
                    well(ui, "diag-log", &lines, &format!("({file} not written)"), 260.0);
                });
                ui.add_space(th::S2);
                w::card(ui, Some("Debug and retention"), |ui| {
                    for (i, key) in ["diagnostics.d3d12_debug", "diagnostics.retention_days"].iter().enumerate() {
                        setting_row(ui, &mut self.store.model, rebuild_config::find(key).unwrap(), i == 0);
                    }
                });
            },
        );
        ui.add_space(th::S2);
        self.sessions_card(ui, "Sessions", false);
        ui.add_space(th::S2);
        self.storage_card(ui);
        ui.add_space(th::S2);
        self.proof_card(ui);
        ui.add_space(th::S2);
        self.about_card(ui);
        if rerun {
            self.start_check();
        }
        if let Some(d) = view {
            self.dialog = Some(d);
        }
        if let Some(p) = open {
            open_folder(&p);
        }
    }


    fn proof_card(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        let mut run = false;
        w::card(ui, None, |ui| {
            egui::CollapsingHeader::new(egui::RichText::new("Run a proof binary (until the layer runs the title)").font(th::medium(13.5)).color(c.text)).id_salt("proof").show(ui, |ui| {
                w::muted(ui, "Runs any binary as it is, in a session folder, with its output as the frame line.");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.proof_exe).font(th::mono(12.0)).hint_text("path to a proof .exe").desired_width(360.0).margin(egui::Margin::symmetric(8, 6)));
                    if w::button(ui, icon::FOLDER_OPEN, "Browse", true).clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("exe", &["exe"]).pick_file() {
                            self.proof_exe = p.display().to_string();
                        }
                    }
                    ui.add(egui::TextEdit::singleline(&mut self.proof_args).font(th::mono(12.0)).desired_width(110.0).margin(egui::Margin::symmetric(8, 6)));
                    if w::button(ui, icon::PLAY, "Run", self.running.is_none() && !self.proof_exe.trim().is_empty()).clicked() {
                        run = true;
                    }
                });
            });
        });
        if run {
            match self.start_run(None, true) {
                Ok(()) => {
                    self.toast(Tone::Ok, "Proof started");
                    self.set_tab(rebuild_launcher::model::Tab::Play);
                }
                Err(e) => self.toast(Tone::Danger, e),
            }
        }
    }

    fn about_card(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        let paths = [
            ("settings", self.home.rebuild_toml()),
            ("profiles", self.home.profiles_dir()),
            ("mods", self.home.mods_dir()),
            ("overlay", self.home.overlay_dir()),
            ("sessions", self.home.sessions_dir()),
            ("launcher", rebuild_launcher::prefs::file(&self.root)),
            ("caches", self.storage.cache.clone()),
        ];
        w::card(ui, None, |ui| {
            egui::CollapsingHeader::new(egui::RichText::new(format!("About · launcher {} · GPL-3.0-only", env!("CARGO_PKG_VERSION"))).font(th::medium(13.5)).color(c.text)).id_salt("about").show(ui, |ui| {
                for line in [
                    "No network and no telemetry: the launcher makes no connection of any kind.",
                    "Never writes into the game folder: the title's settings are applied to a copy in its own overlay.",
                    "Creates no GPU device while a measurement is running (REBUILD_MEASURE_LOCK).",
                ] {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::CHECK).font(th::icon(14.0)).color(c.ok));
                        w::muted(ui, line);
                    });
                }
                ui.add_space(4.0);
                for (k, p) in paths {
                    ui.horizontal(|ui| {
                        w::fixed(ui, 80.0, egui::RichText::new(k).font(th::medium(12.5)).color(c.text_2));
                        ui.label(egui::RichText::new(p.display().to_string()).font(th::mono(11.5)).color(c.text));
                    });
                }
                ui.add_space(4.0);
                for line in [
                    "Inter 3.019, The Inter Project Authors (rsms/inter), SIL Open Font License 1.1",
                    "Phosphor Icons via egui-phosphor, MIT",
                    "Hack, Source Foundry (bundled with egui), MIT and Bitstream Vera licence",
                    "egui and eframe, Emil Ernerfeldt and contributors, MIT or Apache-2.0",
                ] {
                    w::muted(ui, line);
                }
                ui.label(egui::RichText::new(format!("{} settings in the registry · Ctrl+1…6 sections · Ctrl+S save · Ctrl+Enter launch · F5 check", rebuild_config::REGISTRY.len())).font(th::small()).color(c.muted));
            });
        });
    }
}

/// The specs the recommendation was made from (DXGI, no device), and the recommendation with its reason.
fn this_pc_card(ui: &mut egui::Ui, specs: Option<&recommend::Specs>, rec: Option<&recommend::Recommendation>, reading: bool) {
    let c = th::p();
    w::card(ui, Some("This PC"), |ui| {
        let Some(s) = specs else {
            w::muted(ui, if reading { "Reading the specs…" } else { "The specs could not be read." });
            return;
        };
        let row = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.horizontal_top(|ui| {
                w::fixed(ui, 118.0, egui::RichText::new(k).font(th::medium(13.0)).color(c.text));
                ui.add(egui::Label::new(egui::RichText::new(v).font(th::regular(13.0)).color(c.text_2)).wrap());
            });
        };
        match &s.gpu {
            Some(g) => {
                row(ui, "GPU", format!("{} ({})", g.name, syscheck::vendor(g.vendor_id)));
                row(ui, "VRAM", format!("{} dedicated", recommend::gb(g.vram_bytes)));
            }
            None => row(ui, "GPU", "no hardware adapter (software only)".into()),
        }
        row(ui, "RAM", if s.ram_bytes > 0 { format!("{} installed", recommend::gb(s.ram_bytes)) } else { "—".into() });
        row(ui, "CPU", match (s.physical, s.logical) {
            (0, 0) => "—".into(),
            (0, l) => format!("{l} logical processors"),
            (p, l) => format!("{p} cores, {l} logical processors"),
        });
        row(ui, "Display", s.display.map(|(w, h)| format!("{w}×{h} (primary)")).unwrap_or_else(|| "—".into()));
        if let Some(r) = rec {
            w::rule(ui);
            row(ui, "Recommended", format!("{} · {}×{} internal · no limits", r.name(), r.internal.0, r.internal.1));
            ui.add(egui::Label::new(egui::RichText::new(&r.why).font(th::small()).color(c.muted)).wrap());
            for n in &r.notes {
                ui.add(egui::Label::new(egui::RichText::new(format!("{}  {n}", icon::WARNING)).font(th::small()).color(Tone::Warn.colours().0)).wrap());
            }
        }
    });
}
