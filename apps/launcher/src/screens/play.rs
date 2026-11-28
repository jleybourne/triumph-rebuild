//! Play: the header band (Ready to launch, the game, the profile line, Launch, the profile picker, Test
//! matrix) and four cards (Last run, Target, GPU, System check). While the title runs, the Running view:
//! tiles, the frame-time graph, where the frame goes, the frame line, Capture frame and Stop.

use super::{setting_row, well};
use crate::ui::background;
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{mmss, App, BASELINE_FPS, FRAME_BUDGET_MS, TARGET_FPS};
use eframe::egui::{self, Align, CornerRadius, Layout, Pos2, Rect, Sense, Shape, Vec2};
use rebuild_config::profiles::Active;
use rebuild_config::BUILTIN_PROFILES;
use rebuild_launcher::frameline::{self, Frame};
use rebuild_launcher::model::Tab;
use rebuild_launcher::{recommend, session, syscheck};

impl App {
    pub fn play(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        let first_run = self.game_dir().as_os_str().is_empty();
        let mut launch = false;
        let mut to_runs = false;
        let mut to_library = false;
        let mut pick: Option<String> = None;
        // header band
        let art = self.art.clone();
        egui::Frame::new().fill(c.panel).stroke(th::stroke(1.0, c.line)).corner_radius(CornerRadius::same(th::R_LG)).inner_margin(egui::Margin::same(th::S3 as i8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let r = ui.max_rect().expand(th::S3);
            background::band_art(ui.painter(), &art, r);
            ui.horizontal(|ui| {
                if let Some((tex, _)) = &art.logo {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
                    ui.painter().add(egui::epaint::RectShape::filled(r, CornerRadius::same(th::R_MD), egui::Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
                    ui.add_space(th::S1);
                }
                ui.vertical(|ui| {
                    let (status, tone) = if first_run { ("Set the game folder", Tone::Warn) } else { ("Ready to launch", Tone::Ok) };
                    w::text(ui, &status.to_uppercase(), th::caption(), tone.colours().0);
                    let name = self.visuals.name.clone().unwrap_or_else(|| "Your title".into());
                    ui.horizontal(|ui| {
                        w::text(ui, &name, th::display(), c.text);
                        ui.add_space(4.0);
                        if self.library.games.len() > 1 || self.library.selected.is_some() {
                            ui.vertical(|ui| {
                                ui.add_space(12.0);
                                if w::link(ui, "Change game").clicked() {
                                    to_library = true;
                                }
                            });
                        }
                    });
                });
            });
            ui.add_space(2.0);
            ui.label(egui::RichText::new(self.profile_line()).font(th::regular(13.5)).color(c.text_2));
            if !first_run {
                ui.add_space(4.0);
                self.shader_band(ui);
            }
            ui.add_space(th::S2);
            ui.horizontal(|ui| {
                if w::primary_button(ui, icon::PLAY, "Launch", Vec2::new(200.0, 48.0), !first_run && self.running.is_none()).on_hover_text("Ctrl+Enter").clicked() {
                    launch = true;
                }
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.add_space(9.0);
                    ui.horizontal(|ui| {
                        let current = Active::of(&self.store.model.config);
                        let shown = self.profile_label();
                        egui::ComboBox::from_id_salt("play-profile").width(260.0).selected_text(shown).show_ui(ui, |ui| {
                            if let Some(r) = &self.rec {
                                ui.label(egui::RichText::new("FOR THIS PC").font(th::caption().0).color(c.muted));
                                let on = self.store.model.config.profile == recommend::PROFILE && r.matches(&self.store.model.config);
                                if ui.selectable_label(on, format!("Recommended: {}", r.name())).on_hover_text(r.specs_line.as_str()).clicked() {
                                    pick = Some("recommended".into());
                                }
                                ui.separator();
                            }
                            ui.label(egui::RichText::new("BUILT-IN TEST PROFILES").font(th::caption().0).color(c.muted));
                            for b in BUILTIN_PROFILES {
                                let on = matches!(&current, Active::Builtin(a) if a.name == b.name);
                                if ui.selectable_label(on, b.label).clicked() {
                                    pick = Some(format!("builtin:{}", b.name));
                                }
                            }
                            if !self.profiles.is_empty() {
                                ui.separator();
                                ui.label(egui::RichText::new("YOURS").font(th::caption().0).color(c.muted));
                                for p in &self.profiles {
                                    let on = matches!(&current, Active::User(n) if n == p);
                                    if ui.selectable_label(on, p).clicked() {
                                        pick = Some(format!("user:{p}"));
                                    }
                                }
                            }
                        });
                        if w::button(ui, icon::LIST_CHECKS, "Test matrix", true).clicked() {
                            to_runs = true;
                        }
                    });
                });
            });
        });
        ui.add_space(th::S2);
        if self.recommendation_strip(ui) {
            pick = Some("recommended".into());
        }

        // four cards
        let last = self.sessions.iter().find(|e| self.run.as_ref().is_none_or(|r| r.session != e.dir));
        let last_fps = last.and_then(|e| e.summary.as_ref()).and_then(|s| s.avg_fps);
        let last_sub = match last {
            None => "no run yet".to_string(),
            Some(e) if last_fps.is_none() => format!("{} · no frame yet", e.profile()),
            Some(e) => format!("{} · {}", e.profile(), e.info.as_ref().and_then(|i| i.started).map(session::short_time).unwrap_or_default()),
        };
        let gpu = self.adapter_detail("GPU").map(|d| d.split(" (").next().unwrap_or("").to_string());
        let vram = self.adapter_detail("VRAM").map(|d| d.split_whitespace().take(2).collect::<Vec<_>>().join(" "));
        let driver = self.adapter_detail("Driver").map(|d| d.split_whitespace().next().unwrap_or("").to_string());
        let warnings = self.launch_warnings();
        let checking = self.check_rx.is_some();
        let mut open_diag = false;
        ui.columns(4, |cols| {
            w::card(&mut cols[0], None, |ui| {
                let v = last_fps.map(|f| format!("{f:.1}"));
                w::stat(ui, "Last run", v.as_deref(), "fps", &last_sub, None);
            });
            w::card(&mut cols[1], None, |ui| {
                let vs = match last_fps {
                    Some(f) => format!("vs {BASELINE_FPS} baseline · last {}", frameline::vs_target(Some(f), TARGET_FPS)),
                    None => format!("vs {BASELINE_FPS} fps baseline"),
                };
                w::stat(ui, "Target", Some(&format!("{TARGET_FPS:.0}")), "fps", &vs, None);
            });
            w::card(&mut cols[2], None, |ui| {
                w::caption(ui, "GPU");
                ui.add_space(2.0);
                let name = gpu.clone().unwrap_or_else(|| if checking { "Checking…".into() } else { "—".into() });
                ui.add(egui::Label::new(egui::RichText::new(name).font(th::semibold(16.0)).color(c.text)).truncate());
                let sub = match (&vram, &driver) {
                    (Some(v), Some(d)) => format!("{v} · driver {d}"),
                    (Some(v), None) => v.clone(),
                    _ => " ".into(),
                };
                ui.label(egui::RichText::new(sub).font(th::small()).color(c.muted));
            });
            w::card(&mut cols[3], None, |ui| {
                w::caption(ui, "System check");
                ui.add_space(2.0);
                let (head, tone) = if checking && self.checks.is_empty() {
                    ("Checking…".to_string(), Tone::Muted)
                } else if warnings.is_empty() {
                    ("All clear".to_string(), Tone::Ok)
                } else {
                    (format!("{} warning{}", warnings.len(), if warnings.len() == 1 { "" } else { "s" }), Tone::Warn)
                };
                ui.label(egui::RichText::new(head).font(th::semibold(16.0)).color(if tone == Tone::Muted { c.muted } else { tone.colours().0 }));
                for wn in warnings.iter().take(2) {
                    ui.add(egui::Label::new(egui::RichText::new(wn).font(th::small()).color(c.muted)).truncate());
                }
                if warnings.len() > 2 || self.checks.iter().any(|c| c.status == syscheck::Status::Fail) {
                    if w::link(ui, "Open Diagnostics").clicked() {
                        open_diag = true;
                    }
                }
            });
        });

        if first_run {
            ui.add_space(th::S2);
            w::card(ui, Some("First run: where is the game?"), |ui| {
                w::muted(ui, "Pick the folder the title was installed to. The launcher only reads it; everything it writes goes to its own folder.");
                ui.add_space(th::S1);
                let s = rebuild_config::find("game.dir").unwrap();
                setting_row(ui, &mut self.store.model, s, true);
            });
        }
        if !self.title_note.is_empty() {
            ui.add_space(th::S1);
            ui.label(egui::RichText::new(&self.title_note).font(th::small()).color(c.muted));
        }

        if launch {
            self.launch();
        }
        if let Some(p) = pick {
            if p == "recommended" {
                self.apply_recommendation();
            } else if let Some(name) = p.strip_prefix("builtin:") {
                if let Some(b) = rebuild_config::builtin(name) {
                    self.apply_builtin(b);
                }
            } else if let Some(name) = p.strip_prefix("user:") {
                self.load_profile(name);
            }
        }
        if to_runs {
            self.set_tab(Tab::Runs);
        }
        if to_library {
            self.set_tab(Tab::Library);
        }
        if open_diag {
            self.set_tab(Tab::Diagnostics);
        }
    }

    /// "Recommended for this PC: <name>", the detected specs as its reason (the why and any notes on
    /// hover), and Apply, or "In use" when every value is in force. True when Apply was clicked.
    fn recommendation_strip(&mut self, ui: &mut egui::Ui) -> bool {
        let c = th::p();
        let mut apply = false;
        let Some(r) = self.rec.clone() else {
            if self.specs_rx.is_some() {
                w::card(ui, None, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::SPARKLE).font(th::icon(18.0)).color(c.muted));
                        ui.label(egui::RichText::new("Recommended for this PC: reading the specs…").font(th::medium(13.5)).color(c.muted));
                    });
                });
                ui.add_space(th::S2);
            }
            return false;
        };
        let same = r.matches(&self.store.model.config);
        let mut hover = r.why.clone();
        for n in &r.notes {
            hover += &format!("\n{n}");
        }
        w::card(ui, None, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::SPARKLE).font(th::icon(18.0)).color(c.accent));
                let text_w = (ui.available_width() - 140.0).max(200.0);
                ui.vertical(|ui| {
                    ui.set_max_width(text_w);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Recommended for this PC:").font(th::medium(14.0)).color(c.text_2));
                        ui.label(egui::RichText::new(r.name()).font(th::semibold(14.0)).color(c.text));
                        if !r.notes.is_empty() {
                            w::chip(ui, &format!("{} note{}", r.notes.len(), if r.notes.len() == 1 { "" } else { "s" }), Tone::Warn).on_hover_text(r.notes.join("\n"));
                        }
                    });
                    ui.add(egui::Label::new(egui::RichText::new(&r.specs_line).font(th::small()).color(c.muted)).truncate()).on_hover_text(hover.as_str());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if same {
                        w::chip(ui, "In use", Tone::Ok).on_hover_text("Every recommended value is in force");
                    } else if w::button(ui, icon::CHECK, "Apply", self.running.is_none())
                        .on_hover_text(format!("{}x{}, {} {}, no limits; every other setting kept. {}", r.width, r.height, r.backend.to_uppercase(), recommend::mode_name(r.backend, r.mode), r.why))
                        .clicked()
                    {
                        apply = true;
                    }
                });
            });
        });
        ui.add_space(th::S2);
        apply
    }

    /// The Running view, while the title plays.
    pub fn running_view(&mut self, ui: &mut egui::Ui, now: f64) {
        let c = th::p();
        let _ = now;
        let elapsed = self.running.as_ref().map(|r| r.started.elapsed().as_secs());
        let profile = self.run.as_ref().map(|r| r.profile.clone()).unwrap_or_else(|| self.store.model.config.profile.clone());
        let last: Frame = self.frames.last().cloned().unwrap_or_default();
        let mut stop = false;
        let mut capture = false;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                w::text(ui, "RUNNING", th::caption(), c.accent);
                w::text(ui, &format!("{} · {}", self.visuals.name.clone().unwrap_or_else(|| "Your title".into()), profile), th::h1(), c.text);
                let t = elapsed.map(mmss).unwrap_or_else(|| "—".into());
                let session = self.run.as_ref().and_then(|r| r.session.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| "preview".into());
                ui.label(egui::RichText::new(format!("{t} · session {session}")).font(th::small()).color(c.muted));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::danger_button(ui, icon::STOP, "Stop", self.running.is_some()).clicked() {
                    stop = true;
                }
                if w::button(ui, icon::CAMERA, "Capture frame", self.run.is_some()).on_hover_text("Asks the layer for a capture into the session's captures folder").clicked() {
                    capture = true;
                }
            });
        });
        ui.add_space(th::S2);

        // tiles
        let fps = last.fps.map(|v| format!("{v:.1}"));
        let ms = last.ms.map(|v| format!("{v:.1}"));
        let over = last.ms.is_some_and(|m| m > FRAME_BUDGET_MS);
        let draws = last.draws.map(|d| d.to_string());
        let dropped = match last.dropped {
            Some(d) => format!("+{d} dropped"),
            None => "dropped —".into(),
        };
        let res = match (last.internal, last.output) {
            (Some((a, b)), Some((x, y))) => Some(format!("{a}×{b} → {x}×{y}")),
            (None, Some((x, y))) => Some(format!("{x}×{y}")),
            _ => None,
        };
        let upscaler = last.upscaler.clone().unwrap_or_else(|| "upscaler —".into());
        ui.columns(4, |cols| {
            w::card(&mut cols[0], None, |ui| w::stat(ui, "fps", fps.as_deref(), "", if fps.is_none() { "no frame yet" } else { "this second" }, None));
            w::card(&mut cols[1], None, |ui| w::stat(ui, "Frame time", ms.as_deref(), "ms", &format!("budget {FRAME_BUDGET_MS:.1} ms"), over.then_some(Tone::Warn)));
            w::card(&mut cols[2], None, |ui| w::stat(ui, "Draws", draws.as_deref(), "", &dropped, last.dropped.filter(|d| *d > 0).map(|_| Tone::Warn)));
            w::card(&mut cols[3], None, |ui| {
                w::caption(ui, "Resolution");
                ui.add_space(2.0);
                match &res {
                    Some(r) => ui.add(egui::Label::new(egui::RichText::new(r).font(th::semibold(17.0)).color(c.text)).truncate()),
                    None => ui.label(egui::RichText::new("—").font(th::numerals().0).color(c.muted)),
                };
                ui.label(egui::RichText::new(&upscaler).font(th::small()).color(c.muted));
            });
        });
        ui.add_space(th::S2);

        let frames = self.frames.clone();
        super::split(
            ui,
            0.62,
            |ui| {
                w::card(ui, Some("Frame time"), |ui| frame_graph(ui, &frames));
            },
            |ui| {
                w::card(ui, Some("Where the frame goes"), |ui| breakdown(ui, &last));
            },
        );
        ui.add_space(th::S2);
        let log = self.run.as_ref().map(|r| r.log.display().to_string()).unwrap_or_default();
        w::card(ui, Some("Frame line"), |ui| {
            if !log.is_empty() {
                ui.label(egui::RichText::new(&log).font(th::mono(11.0)).color(c.muted));
            }
            well(ui, "running-log", &self.log, "$ waiting for the first frame line", 220.0);
        });
        if stop {
            self.plan = None;
            self.stop();
        }
        if capture {
            if let Some(r) = &self.run {
                match session::request_capture(&r.session, session::now_unix()) {
                    Ok(_) => self.toast(Tone::Accent, "Capture requested: it lands in the session's captures folder"),
                    Err(e) => self.toast(Tone::Danger, e),
                }
            }
        }
    }
}

/// Frame times (ms) over the last frames, with a dashed line at the 33.3 ms budget.
fn frame_graph(ui: &mut egui::Ui, frames: &[Frame]) {
    let c = th::p();
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 170.0), Sense::hover());
    let p = ui.painter();
    let plot = Rect::from_min_max(Pos2::new(r.left() + 34.0, r.top() + 6.0), Pos2::new(r.right() - 4.0, r.bottom() - 16.0));
    let ms: Vec<f64> = frames.iter().filter_map(|f| f.ms).collect();
    let top = ms.iter().cloned().fold(50.0f64, f64::max) * 1.1;
    let y = |v: f64| plot.bottom() - (v / top) as f32 * plot.height();
    for g in [0.0, FRAME_BUDGET_MS, top / 1.1] {
        p.text(Pos2::new(plot.left() - 6.0, y(g)), egui::Align2::RIGHT_CENTER, format!("{g:.0}"), th::regular(10.5), c.muted);
    }
    p.hline(plot.x_range(), plot.bottom(), th::stroke(1.0, c.line));
    let budget = y(FRAME_BUDGET_MS);
    p.extend(Shape::dashed_line(&[Pos2::new(plot.left(), budget), Pos2::new(plot.right(), budget)], th::stroke(1.0, c.warn), 5.0, 4.0));
    p.text(Pos2::new(plot.right(), budget - 3.0), egui::Align2::RIGHT_BOTTOM, format!("{FRAME_BUDGET_MS:.1} ms budget"), th::regular(10.5), c.warn);
    if ms.is_empty() {
        p.text(plot.center(), egui::Align2::CENTER_CENTER, "no frame yet", th::regular(12.5), c.muted);
        return;
    }
    let n = ms.len().min(120);
    let shown = &ms[ms.len() - n..];
    let step = plot.width() / (n.max(2) - 1) as f32;
    let pts: Vec<Pos2> = shown.iter().enumerate().map(|(i, v)| Pos2::new(plot.left() + i as f32 * step, y(*v))).collect();
    if pts.len() >= 2 {
        // a soft area under the line: one mesh, a quad per step
        let mut m = egui::Mesh::default();
        let fill = th::lerp(c.panel, c.accent, if th::is_dark() { 0.16 } else { 0.10 });
        for (i, q) in pts.iter().enumerate() {
            m.colored_vertex(*q, fill);
            m.colored_vertex(Pos2::new(q.x, plot.bottom()), fill);
            if i > 0 {
                let k = (i as u32) * 2;
                m.add_triangle(k - 2, k, k - 1);
                m.add_triangle(k, k + 1, k - 1);
            }
        }
        p.add(m);
        p.add(Shape::line(pts, th::stroke(1.5, c.accent)));
    }
    p.text(Pos2::new(plot.left(), r.bottom()), egui::Align2::LEFT_BOTTOM, format!("last {n} s"), th::regular(10.5), c.muted);
}

/// The frame's parts, as the frame line reports them, each with its share of the frame.
fn breakdown(ui: &mut egui::Ui, f: &Frame) {
    let c = th::p();
    let parts = [("Upload", f.upload_ms), ("List translation", f.xlate_ms), ("GPU", f.gpu_ms), ("Upscaler", f.upscale_ms), ("Waiting", f.wait_ms)];
    let total = f.ms.unwrap_or(FRAME_BUDGET_MS).max(1.0);
    for (name, v) in parts {
        ui.horizontal(|ui| {
            w::fixed(ui, 120.0, egui::RichText::new(name).font(th::regular(13.0)).color(c.text_2));
            let (r, _) = ui.allocate_exact_size(Vec2::new((ui.available_width() - 70.0).max(40.0), 8.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(4), c.control);
            if let Some(v) = v {
                let k = (v / total).clamp(0.0, 1.0) as f32;
                ui.painter().rect_filled(Rect::from_min_size(r.min, Vec2::new(r.width() * k, r.height())), CornerRadius::same(4), c.accent);
            }
            ui.label(egui::RichText::new(match v {
                Some(v) => format!("{v:.1} ms"),
                None => "—".into(),
            })
            .font(th::medium(12.5))
            .color(if v.is_some() { c.text } else { c.muted }));
        });
        ui.add_space(2.0);
    }
    ui.add_space(4.0);
    w::muted(ui, match f.ms {
        Some(_) => "Shares of this second's frame time, from the frame line.",
        None => "Filled in from the frame line once the layer reports these parts.",
    });
}
