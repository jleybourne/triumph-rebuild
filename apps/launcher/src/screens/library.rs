//! The game library (reached from Play's "Change game"): every game the launcher knows, each with its
//! own settings, profiles, mods and sessions; add and remove.

use super::{grid, heading};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{App, Dialog};
use eframe::egui::{self, Align, CornerRadius, Layout, Pos2, Rect, Sense, Vec2};
use rebuild_launcher::library::Detection;
use rebuild_launcher::model::Tab;

impl App {
    pub fn library_page(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        let mut add = false;
        ui.horizontal(|ui| {
            ui.vertical(|ui| heading(ui, "Library", "Every game the launcher knows. Each keeps its own settings, profiles, mods and sessions."));
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if w::primary_button(ui, icon::PLUS, "Add a game", Vec2::new(150.0, 36.0), true).clicked() {
                    add = true;
                }
            });
        });
        let games = self.library.games.clone();
        let ctx = ui.ctx().clone();
        let arts: Vec<_> = games.iter().map(|g| self.game_art(&ctx, &g.id, &g.dir)).collect();
        let mut select = None;
        let mut play = None;
        let mut remove = None;
        let selected = self.library.selected.clone();
        let detections = self.detections.clone();
        grid(ui, games.len() + 1, 260.0, 4, |ui, i| {
            let Some(g) = games.get(i) else {
                // the add tile
                let resp = egui::Frame::new().fill(c.panel).stroke(th::stroke(1.0, c.line)).corner_radius(CornerRadius::same(th::R_LG)).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical_centered(|ui| {
                        ui.add_space(th::S3);
                        ui.label(egui::RichText::new(icon::PLUS).font(th::icon(26.0)).color(c.accent));
                        ui.label(egui::RichText::new("Add a game").font(th::semibold(14.0)).color(c.text));
                        ui.add(egui::Label::new(egui::RichText::new("Pick its folder; the launcher checks what it is").font(th::small()).color(c.muted)).wrap());
                        ui.add_space(th::S3);
                    });
                });
                if ui.interact(resp.response.rect, egui::Id::new("add-tile"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    add = true;
                }
                return;
            };
            let is_sel = selected.as_deref() == Some(g.id.as_str());
            egui::Frame::new()
                .fill(c.panel)
                .stroke(th::stroke(if is_sel { 1.5 } else { 1.0 }, if is_sel { c.accent } else { c.line }))
                .corner_radius(CornerRadius::same(th::R_LG))
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ui.available_width() * 9.0 / 16.0), Sense::hover());
                    match &arts[i].key_art {
                        Some((tex, size)) => {
                            let scale = (r.width() / size.x).max(r.height() / size.y);
                            let shown = *size * scale;
                            let uv = Rect::from_center_size(Pos2::new(0.5, 0.5), Vec2::new(r.width() / shown.x, r.height() / shown.y));
                            ui.painter().add(egui::epaint::RectShape::filled(r, CornerRadius::same(th::R_MD), egui::Color32::WHITE).with_texture(tex.id(), uv));
                        }
                        None => {
                            ui.painter().rect_filled(r, CornerRadius::same(th::R_MD), c.control);
                            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, icon::GAME_CONTROLLER, th::icon(36.0), c.muted);
                        }
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(egui::RichText::new(&g.name).font(th::semibold(15.0)).color(c.text)).truncate());
                        if is_sel {
                            w::chip(ui, "selected", Tone::Accent);
                        }
                    });
                    match detections.get(&g.id) {
                        Some(Detection::Supported { detail, .. }) => {
                            ui.horizontal(|ui| {
                                w::chip(ui, "Supported", Tone::Ok);
                                ui.label(egui::RichText::new(detail).font(th::small()).color(c.muted));
                            });
                        }
                        Some(Detection::Unsupported { reason, .. }) => {
                            w::chip(ui, "Not supported yet", Tone::Warn).on_hover_text(reason);
                        }
                        _ => {
                            w::chip(ui, "Folder missing", Tone::Danger);
                        }
                    }
                    ui.add(egui::Label::new(egui::RichText::new(g.dir.display().to_string()).font(th::mono(11.0)).color(c.muted)).truncate());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if w::primary_button(ui, icon::PLAY, "Play", Vec2::new(90.0, 30.0), true).clicked() {
                            play = Some(g.id.clone());
                        }
                        if !is_sel && w::button(ui, icon::CHECK, "Select", true).clicked() {
                            select = Some(g.id.clone());
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::danger_button(ui, icon::TRASH, "", true).on_hover_text("Remove from the library").clicked() {
                                remove = Some((g.id.clone(), g.name.clone()));
                            }
                        });
                    });
                });
        });
        if add {
            self.pick_game_folder();
        }
        if let Some(id) = select {
            self.switch_game(&ctx, &id);
        }
        if let Some(id) = play {
            if self.library.selected.as_deref() != Some(id.as_str()) {
                self.switch_game(&ctx, &id);
            }
            self.set_tab(Tab::Play);
        }
        if let Some((id, name)) = remove {
            self.dialog = Some(Dialog::Remove { id, name, delete_data: false });
        }
    }
}
