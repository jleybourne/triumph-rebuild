//! Mods: the load order with enable switches, move up and down, conflict warnings; where saves and
//! settings live (the write overlay); and the game folder's read-only check (Verify now).

use super::{empty_state, heading, split};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{open_folder, App};
use eframe::egui::{self, Align, Layout};
use rebuild_launcher::mods;

impl App {
    pub fn mods_page(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        heading(ui, "Mods", "Folders whose files replace the title's by path, through the overlay file system. Later mods win; the game folder is never modified.");
        let mods_dir = self.home.mods_dir();
        let mut refresh = false;
        let mut open_mods = false;
        let mut open_overlay = false;
        let mut verify = false;
        let mut changed = false;
        let mut moved: Option<(usize, usize)> = None;
        let conflicts = self.conflicts.clone();
        let overlay = self.home.overlay_dir();
        let game = self.game_dir();
        let verifying = self.verify_rx.is_some();
        let verified = self.verified.clone();
        let entries = &mut self.mods;
        split(
            ui,
            0.64,
            |ui| {
                w::card(ui, None, |ui| {
                    ui.horizontal(|ui| {
                        w::text(ui, "Load order", th::h2(), c.text);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::button(ui, icon::ARROWS_CLOCKWISE, "Refresh", true).clicked() {
                                refresh = true;
                            }
                            if w::button(ui, icon::FOLDER_OPEN, "Mods folder", true).on_hover_text(mods_dir.display().to_string()).clicked() {
                                open_mods = true;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if entries.is_empty() {
                        empty_state(ui, icon::PUZZLE_PIECE, "No mods yet", "Make a folder in the mods folder: each folder is one mod, and its files replace the title's by path.");
                        return;
                    }
                    let n = entries.len();
                    for i in 0..n {
                        if i > 0 {
                            w::rule(ui);
                        }
                        let name = entries[i].name.clone();
                        let count = conflicts.iter().filter(|k| k.providers.iter().any(|p| *p == name)).count();
                        let wins = conflicts.iter().filter(|k| k.winner == name).count();
                        let m = &mut entries[i];
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{:02}", i + 1)).font(th::mono(12.5)).color(c.muted));
                            changed |= w::toggle(ui, &mut m.enabled).changed();
                            ui.label(egui::RichText::new(&m.name).font(th::semibold(14.0)).color(if m.present { c.text } else { c.muted }));
                            if !m.present {
                                w::chip(ui, "folder missing", Tone::Danger);
                            }
                            if m.enabled && count > 0 {
                                w::chip(ui, &format!("{} {count} conflict{}", icon::WARNING, if count == 1 { "" } else { "s" }), Tone::Warn);
                                if wins > 0 {
                                    w::chip(ui, &format!("wins {wins}"), Tone::Ok);
                                }
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if w::button(ui, icon::CARET_DOWN, "", i + 1 < n).on_hover_text("Later (wins more)").clicked() {
                                    moved = Some((i, i + 1));
                                }
                                if w::button(ui, icon::CARET_UP, "", i > 0).on_hover_text("Earlier").clicked() {
                                    moved = Some((i, i - 1));
                                }
                            });
                        });
                    }
                });
                ui.add_space(th::S2);
                w::card(ui, Some("Conflicts"), |ui| {
                    if conflicts.is_empty() {
                        w::muted(ui, "No two enabled mods replace the same file.");
                    }
                    for (k, cf) in conflicts.iter().enumerate() {
                        if k > 0 {
                            w::rule(ui);
                        }
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&cf.path).font(th::mono(12.0)).color(c.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                w::chip(ui, &format!("{} wins", cf.winner), Tone::Ok);
                                ui.label(egui::RichText::new(cf.providers.join("  →  ")).font(th::small()).color(c.muted));
                            });
                        });
                    }
                });
            },
            |ui| {
                w::card(ui, Some("Your saves and settings"), |ui| {
                    w::muted(ui, "Everything the title writes (saves, its settings copy) lands in the launcher's write overlay, never in the game folder.");
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(egui::RichText::new(overlay.display().to_string()).font(th::mono(11.5)).color(c.text_2)).wrap());
                    ui.add_space(4.0);
                    if w::button(ui, icon::FOLDER_OPEN, "Open the overlay", true).clicked() {
                        open_overlay = true;
                    }
                });
                ui.add_space(th::S2);
                w::card(ui, Some("Game folder"), |ui| {
                    if game.as_os_str().is_empty() {
                        w::muted(ui, "Not set yet (Settings › Files & paths).");
                        return;
                    }
                    ui.add(egui::Label::new(egui::RichText::new(game.display().to_string()).font(th::mono(11.5)).color(c.text_2)).wrap());
                    ui.add_space(4.0);
                    match (&verified, verifying) {
                        (_, true) => {
                            ui.label(egui::RichText::new("Checking every file's read-only flag…").font(th::small()).color(c.muted));
                        }
                        (None, false) => {
                            w::muted(ui, "Not checked in this session. Verify reads metadata only: no file is opened.");
                        }
                        (Some(Ok(r)), false) if r.ok() => {
                            w::chip(ui, &format!("{} read-only: all {} files{}", icon::SHIELD_CHECK, r.files, if r.truncated { " checked so far" } else { "" }), Tone::Ok);
                        }
                        (Some(Ok(r)), false) if r.files == 0 => {
                            w::chip(ui, "no files found", Tone::Warn);
                        }
                        (Some(Ok(r)), false) => {
                            w::chip(ui, &format!("{} {} of {} files are writable", icon::WARNING, r.writable, r.files), Tone::Warn);
                            for e in &r.examples {
                                ui.label(egui::RichText::new(e.display().to_string()).font(th::mono(11.0)).color(c.muted));
                            }
                            w::muted(ui, "Make the folder read-only (file Properties › Read-only, applied to all files) so nothing can overwrite a game file.");
                        }
                        (Some(Err(e)), false) => {
                            ui.label(egui::RichText::new(e).font(th::small()).color(c.danger));
                        }
                    }
                    ui.add_space(4.0);
                    if w::button(ui, icon::SHIELD_CHECK, "Verify now", !verifying).clicked() {
                        verify = true;
                    }
                });
            },
        );
        if let Some((from, to)) = moved {
            if from < self.mods.len() && to < self.mods.len() {
                if to > from {
                    mods::move_down(&mut self.mods, from);
                } else {
                    mods::move_up(&mut self.mods, from);
                }
                changed = true;
            }
        }
        if changed {
            self.mods_changed();
        }
        if refresh {
            self.refresh_mods();
            self.toast(Tone::Accent, format!("{} mods found", self.mods.iter().filter(|m| m.present).count()));
        }
        if open_mods {
            open_folder(&mods_dir);
        }
        if open_overlay {
            open_folder(&overlay);
        }
        if verify {
            self.start_verify();
        }
    }
}
