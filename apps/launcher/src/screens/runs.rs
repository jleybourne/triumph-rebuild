//! Runs & test matrix: choose profiles, run length and where measuring starts, run them in turn; the
//! last matrix side by side (Export CSV); every run with Compare and Log.

use super::{empty_state, heading, split};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{mmss, open_folder, App, Dialog, TARGET_FPS};
use eframe::egui::{self, Align, Id, Layout, Vec2};
use rebuild_config::BUILTIN_PROFILES;
use rebuild_launcher::frameline::{self, MatrixRow, Summary};
use rebuild_launcher::{runs, session};
use std::path::PathBuf;

/// One table row's cells: avg fps, 1 % low, frame ms, GPU ms, upload ms, vs 30 fps.
fn cells(s: Option<&Summary>) -> [String; 6] {
    let g = |f: fn(&Summary) -> Option<f64>| s.and_then(f);
    [
        frameline::show(g(|s| s.avg_fps), 1),
        frameline::show(g(|s| s.low1_fps), 1),
        frameline::show(g(|s| s.frame_ms), 1),
        frameline::show(g(|s| s.gpu_ms), 1),
        frameline::show(g(|s| s.upload_ms), 1),
        frameline::vs_target(g(|s| s.avg_fps), TARGET_FPS),
    ]
}

const COLS: [&str; 6] = ["avg fps", "1% low", "frame ms", "GPU ms", "upload ms", "vs 30 fps"];

impl App {
    pub fn runs_page(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        heading(ui, "Runs & test matrix", "Run the same build as several machines in turn, each in its own session, and read them side by side.");
        let mut start = false;
        let mut stop = false;
        let mut write_files = false;
        let running = self.running.is_some();
        let no_game = self.game_dir().as_os_str().is_empty();
        let plan = self.plan.clone();
        // the last matrix: the newest session with a matrix id, and every session with that id
        let last_matrix = self.sessions.iter().find_map(|e| e.info.as_ref().and_then(|i| i.matrix.clone()));
        let rows: Vec<MatrixRow> = match &last_matrix {
            Some(id) => {
                let mut v: Vec<MatrixRow> = self
                    .sessions
                    .iter()
                    .filter(|e| e.info.as_ref().and_then(|i| i.matrix.as_ref()) == Some(id))
                    .map(|e| MatrixRow { profile: e.profile(), session: e.name.clone(), summary: e.summary.clone() })
                    .collect();
                v.reverse();
                v
            }
            None => Vec::new(),
        };
        let mut export = false;
        split(
            ui,
            0.42,
            |ui| {
                w::card(ui, Some("Test matrix"), |ui| {
                    w::caption(ui, "Profiles");
                    for (i, b) in BUILTIN_PROFILES.iter().enumerate() {
                        let mut on = self.matrix_pick[i];
                        if w::checkbox(ui, &mut on, b.label).on_hover_text(b.summary()).changed() {
                            self.matrix_pick[i] = on;
                        }
                    }
                    ui.add_space(th::S1);
                    w::caption(ui, "Run length");
                    let labels: Vec<&str> = runs::LENGTHS.iter().map(|(_, l)| *l).collect();
                    w::segmented(ui, Id::new("run-len"), &labels, &mut self.run_len);
                    ui.add_space(th::S1);
                    w::caption(ui, "Measure from");
                    let mut k = usize::from(self.measure_from == runs::MeasureFrom::AfterLoading);
                    if w::segmented(ui, Id::new("measure-from"), &["title start", "after loading"], &mut k) {
                        self.measure_from = if k == 1 { runs::MeasureFrom::AfterLoading } else { runs::MeasureFrom::TitleStart };
                    }
                    ui.add_space(th::S2);
                    let n = self.matrix_pick.iter().filter(|x| **x).count();
                    match &plan {
                        Some(p) => {
                            let (done, total) = p.progress();
                            let now = p.current.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
                            ui.label(egui::RichText::new(format!("Running {done} of {total}: {now}")).font(th::medium(13.5)).color(c.accent));
                            if w::danger_button(ui, icon::STOP, "Stop the matrix", true).clicked() {
                                stop = true;
                            }
                        }
                        None => {
                            let label = format!("Run {n} profile{}", if n == 1 { "" } else { "s" });
                            if w::primary_button(ui, icon::PLAY, &label, Vec2::new(ui.available_width().min(240.0), 40.0), n > 0 && !running && !no_game).clicked() {
                                start = true;
                            }
                            let total = n as u64 * runs::LENGTHS[self.run_len.min(2)].0;
                            ui.label(egui::RichText::new(format!("about {} in all; the launcher stays out of the way while each runs", mmss(total))).font(th::small()).color(c.muted));
                            if no_game {
                                ui.label(egui::RichText::new("Set the game folder first.").font(th::small()).color(c.warn));
                            }
                        }
                    }
                    ui.add_space(4.0);
                    if w::link(ui, "Write the configs only (no run)…").clicked() {
                        write_files = true;
                    }
                });
            },
            |ui| {
                w::card(ui, None, |ui| {
                    ui.horizontal(|ui| {
                        w::text(ui, "Last matrix", th::h2(), c.text);
                        if let Some(id) = &last_matrix {
                            ui.label(egui::RichText::new(session::parse_stamp(id).map(session::short_time).unwrap_or_else(|| id.clone())).font(th::small()).color(c.muted));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::button(ui, icon::EXPORT, "Export CSV", !rows.is_empty()).clicked() {
                                export = true;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if rows.is_empty() {
                        empty_state(ui, icon::LIST_CHECKS, "No matrix run yet", "Choose profiles and run them: each one's numbers land here.");
                        return;
                    }
                    egui::Grid::new("matrix-table").num_columns(7).spacing(Vec2::new(14.0, 8.0)).striped(false).show(ui, |ui| {
                        ui.label(egui::RichText::new("profile").font(th::caption().0).color(c.muted));
                        for h in COLS {
                            ui.label(egui::RichText::new(h).font(th::caption().0).color(c.muted));
                        }
                        ui.end_row();
                        for r in &rows {
                            ui.label(egui::RichText::new(&r.profile).font(th::medium(13.0)).color(c.text));
                            for (k, v) in cells(r.summary.as_ref()).iter().enumerate() {
                                let col = if v == "—" {
                                    c.muted
                                } else if k == 5 {
                                    if v.starts_with('+') {
                                        c.ok
                                    } else {
                                        c.warn
                                    }
                                } else {
                                    c.text
                                };
                                ui.label(egui::RichText::new(v).font(th::mono(12.5)).color(col));
                            }
                            ui.end_row();
                        }
                    });
                    if rows.iter().any(|r| r.summary.as_ref().is_some_and(|s| s.from_frame_line)) {
                        ui.add_space(4.0);
                        w::muted(ui, "Rows without a summary.toml are averaged from their frame line; their 1% low stays empty.");
                    }
                });
            },
        );
        ui.add_space(th::S2);
        self.compare_card(ui);
        self.sessions_card(ui, "All runs", true);

        if start {
            self.start_matrix();
            self.toast(Tone::Accent, "Test matrix started");
        }
        if stop {
            self.plan = None;
            self.stop();
        }
        if write_files {
            if let Some(dir) = rfd::FileDialog::new().set_title("Folder for the test matrix").pick_folder() {
                self.write_matrix(&dir);
            }
        }
        if export {
            if let Some(p) = rfd::FileDialog::new().add_filter("csv", &["csv"]).set_file_name("test-matrix.csv").save_file() {
                match rebuild_launcher::write_atomic(&p, &frameline::matrix_csv(&rows, TARGET_FPS)) {
                    Ok(()) => self.toast(Tone::Ok, format!("Exported {}", p.display())),
                    Err(e) => self.toast(Tone::Danger, e.to_string()),
                }
            }
        }
    }

    /// Sessions picked with Compare, side by side.
    fn compare_card(&mut self, ui: &mut egui::Ui) {
        let picked: Vec<&session::Entry> = self.sessions.iter().filter(|e| self.compare.contains(&e.dir)).collect();
        if picked.len() < 2 {
            return;
        }
        let c = th::p();
        let mut clear = false;
        w::card(ui, None, |ui| {
            ui.horizontal(|ui| {
                w::text(ui, &format!("Comparing {} runs", picked.len()), th::h2(), c.text);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button(ui, icon::X, "Clear", true).clicked() {
                        clear = true;
                    }
                });
            });
            egui::Grid::new("compare").num_columns(picked.len() + 1).spacing(Vec2::new(18.0, 6.0)).show(ui, |ui| {
                ui.label("");
                for e in &picked {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(e.profile()).font(th::semibold(13.0)).color(c.text));
                        ui.label(egui::RichText::new(session::short_time(e.started)).font(th::small()).color(c.muted));
                    });
                }
                ui.end_row();
                let all: Vec<[String; 6]> = picked.iter().map(|e| cells(e.summary.as_ref())).collect();
                for (k, h) in COLS.iter().enumerate() {
                    ui.label(egui::RichText::new(*h).font(th::caption().0).color(c.muted));
                    for row in &all {
                        ui.label(egui::RichText::new(&row[k]).font(th::mono(12.5)).color(if row[k] == "—" { c.muted } else { c.text }));
                    }
                    ui.end_row();
                }
                ui.label(egui::RichText::new("limits").font(th::caption().0).color(c.muted));
                for e in &picked {
                    let l = e.info.as_ref().map(|i| if i.limits.is_empty() { "none".to_string() } else { i.limits.join(", ") }).unwrap_or_else(|| "—".into());
                    ui.label(egui::RichText::new(l).font(th::small()).color(c.text_2));
                }
                ui.end_row();
            });
        });
        ui.add_space(th::S2);
        if clear {
            self.compare.clear();
        }
    }

    /// The session list (Runs: with Compare; Diagnostics: with sizes and retention).
    pub fn sessions_card(&mut self, ui: &mut egui::Ui, title: &str, compare: bool) {
        let c = th::p();
        let mut log: Option<PathBuf> = None;
        let mut open: Option<PathBuf> = None;
        let mut delete: Option<(PathBuf, String)> = None;
        let mut toggle: Option<PathBuf> = None;
        let running = self.run.as_ref().map(|r| r.session.clone());
        let entries = self.sessions.clone();
        let picked = self.compare.clone();
        w::card(ui, None, |ui| {
            ui.horizontal(|ui| {
                w::text(ui, title, th::h2(), c.text);
                ui.label(egui::RichText::new(format!("{} session{}", entries.len(), if entries.len() == 1 { "" } else { "s" })).font(th::small()).color(c.muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button(ui, icon::FOLDER_OPEN, "Sessions folder", true).clicked() {
                        open = Some(self.home.sessions_dir());
                    }
                });
            });
            ui.add_space(4.0);
            if entries.is_empty() {
                empty_state(ui, icon::TIMER, "No sessions yet", "Every launch gets a folder here: its frame line, summary, crash reports and captures.");
                return;
            }
            for (i, e) in entries.iter().enumerate().take(200) {
                if i > 0 {
                    w::rule(ui);
                }
                let is_running = running.as_ref() == Some(&e.dir);
                ui.horizontal(|ui| {
                    if compare {
                        let mut on = picked.contains(&e.dir);
                        if w::checkbox(ui, &mut on, "").on_hover_text("Compare").changed() {
                            toggle = Some(e.dir.clone());
                        }
                    }
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(e.profile()).font(th::semibold(13.5)).color(c.text));
                            if e.info.as_ref().is_some_and(|i| i.matrix.is_some()) {
                                w::chip(ui, "matrix", Tone::Accent);
                            }
                            if is_running {
                                w::chip(ui, "running", Tone::Accent);
                            } else if !e.crashes.is_empty() {
                                w::chip(ui, &e.outcome(), Tone::Danger);
                            } else {
                                let o = e.outcome();
                                w::chip(ui, &o, if o == "exit code 0" || o == "stopped" { Tone::Muted } else { Tone::Warn });
                            }
                        });
                        let dur = e.info.as_ref().and_then(|i| i.seconds).map(|s| mmss(s.max(0) as u64)).unwrap_or_else(|| "—".into());
                        let fps = e.summary.as_ref().and_then(|s| s.avg_fps).map(|f| format!("{f:.1} fps")).unwrap_or_else(|| "— fps".into());
                        ui.label(egui::RichText::new(format!("{} · {dur} · {fps} · {}", session::short_time(e.started), crate::bytes(e.bytes))).font(th::small()).color(c.muted));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if w::danger_button(ui, icon::TRASH, "", !is_running).on_hover_text("Delete this session").clicked() {
                            delete = Some((e.dir.clone(), e.name.clone()));
                        }
                        if w::button(ui, icon::FOLDER_OPEN, "", true).on_hover_text("Open its folder").clicked() {
                            open = Some(e.dir.clone());
                        }
                        if w::button(ui, icon::TERMINAL_WINDOW, "Log", true).clicked() {
                            log = Some(e.frame_log());
                        }
                    });
                });
            }
        });
        if let Some(d) = toggle {
            if !self.compare.remove(&d) {
                self.compare.insert(d);
            }
        }
        if let Some(p) = log {
            let text = std::fs::read_to_string(&p).unwrap_or_else(|_| "(no frame line was written)".into());
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(500)..].join("\n");
            self.dialog = Some(Dialog::Viewer { title: p.display().to_string(), text: tail, copy: None });
        }
        if let Some(p) = open {
            open_folder(&p);
        }
        if let Some((dir, name)) = delete {
            self.dialog = Some(Dialog::DeleteSession { dir, name });
        }
    }
}
