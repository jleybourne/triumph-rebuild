//! Settings: a search box over every registry setting, the group pills, and each setting as a row with
//! its one-line help and a Live / Restart / Experimental badge. The save bar (in `main.rs`) counts the
//! changes and how many take effect on the next launch.

use super::{empty_state, heading, setting_row};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets as w;
use crate::App;
use eframe::egui::{self, CornerRadius, Sense, Vec2};
use rebuild_config::REGISTRY;
use rebuild_launcher::model::{self, Group};
use rebuild_launcher::title;

impl App {
    pub fn settings_page(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        heading(ui, "Settings", "Every setting the layer has, from one registry. Each default is the measured best.");
        // search
        egui::Frame::new().fill(c.control).stroke(th::stroke(1.0, c.line)).corner_radius(CornerRadius::same(th::R_MD)).inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::MAGNIFYING_GLASS).font(th::icon(15.0)).color(c.muted));
                ui.add(egui::TextEdit::singleline(&mut self.query).frame(false).hint_text(format!("Search {} settings", REGISTRY.len())).desired_width(f32::INFINITY));
            });
        });
        ui.add_space(th::S1);
        // pills
        let searching = !self.query.trim().is_empty();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            for g in Group::ALL {
                let on = !searching && self.group == g;
                let n = model::settings_in(g).count();
                let text = format!("{}  {n}", g.label());
                let galley = ui.painter().layout_no_wrap(text.clone(), th::medium(12.5), c.text);
                let (r, resp) = ui.allocate_exact_size(galley.size() + Vec2::new(22.0, 12.0), Sense::click());
                let hov = resp.hovered() || resp.has_focus();
                let (fill, stroke, col) = if on { (c.accent, c.accent, egui::Color32::WHITE) } else if hov { (c.control_hi, c.line_hi, c.text) } else { (c.panel, c.line, c.text_2) };
                ui.painter().rect_filled(r, CornerRadius::same(th::R_PILL), fill);
                ui.painter().rect_stroke(r, CornerRadius::same(th::R_PILL), th::stroke(1.0, stroke), egui::StrokeKind::Inside);
                ui.painter().galley(r.center() - galley.size() / 2.0, galley, col);
                w::focus_ring(ui, &resp, r, th::R_PILL);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.group = g;
                    self.query.clear();
                }
            }
        });
        ui.add_space(th::S2);
        if searching {
            let q = self.query.clone();
            let hits: Vec<_> = REGISTRY.iter().filter(|s| model::matches(s, &q)).collect();
            if hits.is_empty() {
                w::card(ui, None, |ui| empty_state(ui, icon::MAGNIFYING_GLASS, "No setting matches", "Try a word from its name, its key or its help."));
                return;
            }
            for g in Group::ALL {
                let in_g: Vec<_> = hits.iter().filter(|s| model::group_of(s) == g).collect();
                if in_g.is_empty() {
                    continue;
                }
                w::card(ui, Some(g.label()), |ui| {
                    for (i, s) in in_g.iter().enumerate() {
                        setting_row(ui, &mut self.store.model, s, i == 0);
                    }
                });
                ui.add_space(th::S1);
            }
            return;
        }
        let g = self.group;
        w::card(ui, Some(g.label()), |ui| {
            w::muted(ui, group_help(g));
            ui.add_space(th::S1);
            for (i, s) in model::settings_in(g).enumerate() {
                setting_row(ui, &mut self.store.model, s, i == 0);
            }
        });
        if g == Group::Title {
            ui.add_space(th::S2);
            let src = self.title_source();
            let mut write = false;
            w::card(ui, Some("Where it is written"), |ui| {
                let row = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.horizontal(|ui| {
                        w::fixed(ui, 110.0, egui::RichText::new(k).font(th::medium(12.5)).color(c.text_2));
                        ui.label(egui::RichText::new(v).font(th::mono(12.0)).color(c.text));
                    });
                };
                match &src {
                    Some(p) => row(ui, "title's file", p.display().to_string()),
                    None => row(ui, "title's file", "not found: set Title settings file (Files & paths) or REBUILD_TITLE_SETTINGS".into()),
                }
                row(ui, "copy", title::overlay_copy(&self.home.overlay_dir()).display().to_string());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    w::chip(ui, "original never written", w::Tone::Ok);
                    if w::button(ui, icon::FLOPPY_DISK, "Write the copy now", src.is_some()).clicked() {
                        write = true;
                    }
                });
                if !self.title_note.is_empty() {
                    w::muted(ui, &self.title_note);
                }
            });
            if write {
                let ok = self.write_title_copy();
                self.toast(if ok { w::Tone::Ok } else { w::Tone::Danger }, if ok { "Copy written; the original is unchanged" } else { "Copy not written" });
            }
        }
    }
}

fn group_help(g: Group) -> &'static str {
    match g {
        Group::Display => "Output size, window, pacing, and how the picture is upscaled.",
        Group::Limits => "Make this machine behave like a smaller one: the profiles set these. 0 = no limit.",
        Group::Title => "Written into a copy of the title's settings.xml at every launch. The original is never touched.",
        Group::MemoryShaders => "The layer's own switches for memory, uploads, shaders and streaming.",
        Group::Menu => "The in-game menu (after Build 5).",
        Group::Diagnostics => "The frame line, captures, the debug layer and how long session folders are kept.",
        Group::Files => "The game folder and the files the layer loads from outside the repository. Nothing is ever written to them.",
        Group::Launcher => "The launcher itself. Saved for the whole launcher, not per game.",
    }
}
