//! The screens under the top bar (Direction B), one module each, and what they share: the generated
//! setting row, card grids, the library dialogs.

mod diagnostics;
mod library;
mod mods;
mod play;
mod profiles;
mod runs;
mod settings;

use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{App, Dialog};
use eframe::egui::{self, Align, Color32, Id, Layout, Vec2};
use rebuild_config::{Apply, Setting, Tier};
use rebuild_launcher::model::{self, Edit, Model, Widget};
use rebuild_launcher::store::Store;
use std::path::Path;

/// One registry entry: label, badges (Live / Restart / Experimental) and the one-line help on the left;
/// the generated control and "reset to default" on the right.
pub fn setting_row(ui: &mut egui::Ui, m: &mut Model, s: &'static Setting, first: bool) {
    let c = th::p();
    if !first {
        w::rule(ui);
    }
    let b = m.badges(s);
    let full = ui.available_width();
    ui.horizontal_top(|ui| {
        let left = (full * 0.5).clamp(240.0, 480.0);
        ui.vertical(|ui| {
            ui.set_width(left);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(s.label).font(th::medium(13.5)).color(c.text));
                match s.apply {
                    Apply::Live => w::chip(ui, "Live", Tone::Ok),
                    Apply::Restart => w::chip(ui, "Restart", Tone::Muted),
                }
                .on_hover_text(match s.apply {
                    Apply::Live => "Takes effect at once, from the in-game menu too",
                    Apply::Restart => "Takes effect on the next launch",
                });
                if b.experimental {
                    w::chip(ui, "Experimental", Tone::Warn).on_hover_text("Unmeasured or not yet enforced; refused in a stable-only profile");
                }
            });
            if !s.help.is_empty() {
                w::muted(ui, s.help);
            }
            ui.label(egui::RichText::new(s.key).font(th::mono(11.0)).color(c.muted));
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
                    if max - min > 100_000 {
                        // memory sizes: a number field, not a slider a pixel of which is a gigabyte
                        ui.add(egui::DragValue::new(v).range(min..=max).speed(64.0)).changed()
                    } else {
                        // right to left: the presets sit right of the slider
                        let mut ch = false;
                        if !s.presets.is_empty() {
                            let before = *v;
                            egui::ComboBox::from_id_salt((s.key, "presets")).width(64.0).selected_text("Presets").show_ui(ui, |ui| {
                                for p in s.presets {
                                    ui.selectable_value(v, *p, p.to_string());
                                }
                            });
                            ch |= *v != before;
                        }
                        ch | ui.add(egui::Slider::new(v, min..=max).trailing_fill(true)).changed()
                    }
                }
                (Edit::Pick(i), Widget::Dropdown { options }) => {
                    let short = options.len() <= 4 && options.iter().map(|o| o.len()).sum::<usize>() <= 34;
                    if short {
                        w::segmented(ui, Id::new(s.key), options, i)
                    } else {
                        let before = *i;
                        egui::ComboBox::from_id_salt(s.key).width(190.0).selected_text(options[*i]).show_ui(ui, |ui| {
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
                        let picked = if s.key.ends_with("_dll") || s.key.ends_with("tables") || s.key.ends_with("settings") {
                            rfd::FileDialog::new().pick_file()
                        } else {
                            rfd::FileDialog::new().pick_folder()
                        };
                        if let Some(p) = picked {
                            *t = p.display().to_string();
                            ch = true;
                        }
                    }
                    let te = egui::TextEdit::singleline(t)
                        .font(th::mono(12.0))
                        .hint_text(egui::RichText::new("not set").color(c.muted))
                        .margin(egui::Margin::symmetric(8, 6))
                        .desired_width((ui.available_width() - 8.0).clamp(160.0, 360.0));
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
            ui.label(egui::RichText::new(format!("{}  {why}", icon::X_CIRCLE)).font(th::small()).color(c.danger));
        }
    }
    ui.add_space(4.0);
}

/// A section heading with an optional line under it.
pub fn heading(ui: &mut egui::Ui, title: &str, sub: &str) {
    let c = th::p();
    w::text(ui, title, th::h1(), c.text);
    if !sub.is_empty() {
        ui.label(egui::RichText::new(sub).font(th::regular(13.5)).color(c.muted));
    }
    ui.add_space(th::S2);
}

/// A sub-heading inside a screen.
pub fn subheading(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    w::text(ui, title, th::h2(), th::p().text);
    ui.add_space(2.0);
}

/// Cards in rows of as many as fit at `min_w` (at most `max_cols`), each drawn by `f(ui, index)`.
pub fn grid(ui: &mut egui::Ui, n: usize, min_w: f32, max_cols: usize, mut f: impl FnMut(&mut egui::Ui, usize)) {
    if n == 0 {
        return;
    }
    let per_row = ((ui.available_width() + th::S2) / (min_w + th::S2)).floor().clamp(1.0, max_cols as f32) as usize;
    let mut i = 0;
    while i < n {
        ui.columns(per_row, |cols| {
            for (k, col) in cols.iter_mut().enumerate() {
                if i + k < n {
                    f(col, i + k);
                }
            }
        });
        i += per_row;
        ui.add_space(th::S1);
    }
}

/// Two columns: `left` takes `frac` of the width.
pub fn split(ui: &mut egui::Ui, frac: f32, left: impl FnOnce(&mut egui::Ui), right: impl FnOnce(&mut egui::Ui)) {
    let total = ui.available_width();
    let lw = ((total - th::S2) * frac).floor();
    let rw = total - th::S2 - lw;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = th::S2;
        ui.allocate_ui_with_layout(Vec2::new(lw, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(lw);
            left(ui);
        });
        ui.allocate_ui_with_layout(Vec2::new(rw, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(rw);
            right(ui);
        });
    });
}

pub fn empty_state(ui: &mut egui::Ui, ic: &str, head: &str, body: &str) {
    let c = th::p();
    ui.vertical_centered(|ui| {
        ui.add_space(th::S2);
        ui.label(egui::RichText::new(ic).font(th::icon(28.0)).color(c.muted));
        ui.add_space(4.0);
        w::text(ui, head, th::h2(), c.text);
        ui.label(egui::RichText::new(body).font(th::regular(13.0)).color(c.muted));
        ui.add_space(th::S2);
    });
}

/// A read-only text well (logs, `session.toml`), scrolled to the end.
pub fn well(ui: &mut egui::Ui, id: &str, lines: &[String], empty: &str, max_h: f32) {
    let c = th::p();
    egui::Frame::new().fill(c.well).stroke(th::stroke(1.0, c.line)).corner_radius(egui::CornerRadius::same(th::R_MD)).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if lines.is_empty() {
            ui.label(egui::RichText::new(empty).font(th::mono(12.0)).color(c.muted));
            return;
        }
        egui::ScrollArea::vertical().id_salt(id).max_height(max_h).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
            for l in lines {
                let col = if l.contains("fps") { c.text } else { c.text_2 };
                ui.add(egui::Label::new(egui::RichText::new(l).font(th::mono(12.0)).color(col)).wrap_mode(egui::TextWrapMode::Truncate));
            }
        });
    });
}

impl App {
    /// Sections that need a game, before there is one.
    pub fn no_game(&mut self, ui: &mut egui::Ui) {
        heading(ui, self.tab.label(), "");
        let mut add = false;
        w::card(ui, None, |ui| {
            empty_state(ui, icon::GAME_CONTROLLER, "Add a game first", "Settings, profiles, runs and mods belong to a game. Add its folder to begin.");
            ui.vertical_centered(|ui| {
                if w::primary_button(ui, icon::PLUS, "Add a game", Vec2::new(170.0, 40.0), true).clicked() {
                    add = true;
                }
                ui.add_space(th::S2);
            });
        });
        if add {
            self.pick_game_folder();
        }
    }

    pub fn load_profile(&mut self, name: &str) {
        match Store::load(&self.home.profiles_dir().join(format!("{name}.toml"))) {
            Ok(s) => self.adopt(s, format!("Profile “{name}” loaded. Save to make it active.")),
            Err(e) => self.toast(Tone::Danger, e),
        }
    }

    /// Apply a built-in test profile: its keys only; every other setting is kept. "Save a copy" is
    /// offered a copy's name, since a built-in can't be saved over.
    pub fn apply_builtin(&mut self, b: &'static rebuild_config::Builtin) {
        match self.store.model.apply_builtin(b) {
            Ok(()) => {
                self.profile_name = format!("{} copy", b.name);
                self.toast(Tone::Accent, format!("{} applied ({} settings; the rest kept). Save to make it active.", b.label, b.settings.len()));
            }
            Err(e) => self.toast(Tone::Danger, format!("{e}. Turn off Stable only to use it.")),
        }
    }

    /// The test matrix files: one rebuild.toml per built-in, from the settings as they are now.
    pub fn write_matrix(&mut self, dir: &Path) {
        match rebuild_launcher::home::write_matrix(dir, &self.store.model.config, &[]) {
            Ok(files) => self.toast(Tone::Ok, format!("{} test configurations written to {}", files.len(), dir.display())),
            Err(e) => self.toast(Tone::Danger, e),
        }
    }

    /// Take settings from a profile or an imported file, keeping this game's folder and the theme.
    pub fn adopt(&mut self, mut s: Store, what: String) {
        let dir = self.game_dir().display().to_string();
        let _ = s.model.apply(rebuild_config::find("game.dir").unwrap(), Edit::Text(dir));
        let _ = s.model.config.set("launcher.theme", rebuild_config::Value::Choice(if th::is_dark() { "dark" } else { "light" }.into()));
        self.import_problems = s.model.config.problems.iter().filter(|p| !p.contains("unknown setting")).cloned().collect();
        self.store = s;
        self.refresh_mods();
        self.toast(Tone::Accent, what);
    }

    pub fn confirm_add(&mut self, ctx: &egui::Context, dir: &Path, detection: &rebuild_launcher::library::Detection, restore: bool) -> bool {
        match self.library.add(dir, &self.detectors, restore) {
            Ok(id) => {
                self.detections.insert(id, detection.clone());
                self.load_game(ctx);
                self.toast(Tone::Ok, "Added to the library");
                true
            }
            Err(e) => {
                self.toast(Tone::Danger, e);
                false
            }
        }
    }

    pub fn confirm_remove(&mut self, ctx: &egui::Context, id: &str, name: &str, delete_data: bool) -> bool {
        let was_selected = self.library.selected.as_deref() == Some(id);
        if was_selected && self.dirty() {
            self.save();
        }
        match self.library.remove(id, delete_data) {
            Ok(()) => {
                self.arts.remove(id);
                self.detections.remove(id);
                if was_selected {
                    self.load_game(ctx);
                }
                self.toast(Tone::Accent, format!("{name} removed from the library"));
                true
            }
            Err(e) => {
                self.toast(Tone::Danger, e);
                false
            }
        }
    }

    pub fn dialogs(&mut self, ctx: &egui::Context) {
        use rebuild_launcher::library::Detection;
        let Some(dialog) = self.dialog.clone() else { return };
        let c = th::p();
        let frame = egui::Frame::new()
            .fill(c.panel)
            .stroke(th::stroke(1.0, c.line_hi))
            .corner_radius(egui::CornerRadius::same(th::R_LG))
            .shadow(th::popup_shadow())
            .inner_margin(egui::Margin::same(th::S3 as i8));
        let mut close = false;
        let mut repick = false;
        let mut next: Option<Dialog> = None;
        let resp = egui::Modal::new(Id::new("dialog")).backdrop_color(Color32::from_black_alpha(if th::is_dark() { 150 } else { 70 })).frame(frame).show(ctx, |ui| {
            match dialog.clone() {
                Dialog::Add { dir, detection, previous, mut restore } => {
                    ui.set_width(520.0);
                    let (ic, col, head) = match &detection {
                        Detection::Supported { .. } => (icon::CHECK_CIRCLE, c.ok, "Ready to add"),
                        _ => (icon::WARNING_CIRCLE, c.warn, "Not supported yet"),
                    };
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(ic).font(th::icon(22.0)).color(col));
                        w::text(ui, head, th::h2(), c.text);
                    });
                    ui.label(egui::RichText::new(dir.display().to_string()).font(th::mono(12.0)).color(c.muted));
                    ui.add_space(th::S1);
                    match &detection {
                        Detection::Supported { name, detail } => {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(name).font(th::semibold(15.0)).color(c.text));
                                w::chip(ui, "Supported", Tone::Ok);
                            });
                            w::muted(ui, &format!("{detail}. It gets its own settings, profiles, mods and sessions. The folder is only ever read."));
                            if previous {
                                ui.add_space(th::S1);
                                ui.horizontal(|ui| {
                                    if w::toggle(ui, &mut restore).changed() {
                                        next = Some(Dialog::Add { dir: dir.clone(), detection: detection.clone(), previous, restore });
                                    }
                                    ui.vertical(|ui| {
                                        ui.label(egui::RichText::new("Restore previous settings").font(th::medium(13.5)).color(c.text));
                                        w::muted(ui, "This folder was in the library before; its settings were kept. Off: they are set aside, not deleted.");
                                    });
                                });
                            }
                        }
                        Detection::Unsupported { name, reason } => {
                            if let Some(n) = name {
                                ui.label(egui::RichText::new(n).font(th::semibold(15.0)).color(c.text));
                            }
                            ui.label(egui::RichText::new(reason).font(th::regular(13.0)).color(c.warn));
                            w::muted(ui, "Nothing was added. The launcher runs Xbox Series packages today; more can plug in later.");
                        }
                        Detection::NotMine => {}
                    }
                    ui.add_space(th::S2);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if matches!(detection, Detection::Supported { .. }) {
                            if w::primary_button(ui, icon::PLUS, "Add to library", Vec2::new(160.0, 34.0), true).clicked() {
                                self.confirm_add(ctx, &dir, &detection, restore);
                                close = true;
                            }
                        } else if w::button(ui, icon::FOLDER_OPEN, "Choose another folder", true).clicked() {
                            repick = true;
                        }
                        if w::button(ui, "", "Cancel", true).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Remove { id, name, mut delete_data } => {
                    ui.set_width(520.0);
                    w::text(ui, &format!("Remove {name}?"), th::h2(), c.text);
                    w::muted(ui, "Removes it from the library. The game folder is never touched.");
                    ui.add_space(th::S1);
                    ui.horizontal(|ui| {
                        if w::toggle(ui, &mut delete_data).changed() {
                            next = Some(Dialog::Remove { id: id.clone(), name: name.clone(), delete_data });
                        }
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("Also delete the launcher's data for it").font(th::medium(13.5)).color(c.text));
                            w::muted(ui, "Its settings, profiles, mods list, sessions and overlay copy. Off (the default): kept, and offered back if you add it again.");
                        });
                    });
                    ui.add_space(th::S2);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if w::danger_button(ui, icon::TRASH, if delete_data { "Remove and delete data" } else { "Remove" }, true).clicked() {
                            self.confirm_remove(ctx, &id, &name, delete_data);
                            close = true;
                        }
                        if w::button(ui, "", "Cancel", true).clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Viewer { title, text, copy } => {
                    let wd = (ctx.screen_rect().width() - 160.0).clamp(480.0, 900.0);
                    ui.set_width(wd);
                    w::text(ui, &title, th::h2(), c.text);
                    ui.add_space(th::S1);
                    let lines: Vec<String> = text.lines().map(String::from).collect();
                    well(ui, "viewer", &lines, "(empty)", (ctx.screen_rect().height() - 260.0).max(200.0));
                    ui.add_space(th::S1);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if w::button(ui, "", "Close", true).clicked() {
                            close = true;
                        }
                        if let Some(report) = &copy {
                            if w::button(ui, icon::COPY, "Copy for a bug report", true).clicked() {
                                ctx.copy_text(report.clone());
                                self.toast(Tone::Ok, "Copied: launcher version, profile, settings and the report");
                            }
                        }
                    });
                }
                Dialog::DeleteSession { dir, name } => {
                    ui.set_width(460.0);
                    w::text(ui, "Delete this session?", th::h2(), c.text);
                    ui.label(egui::RichText::new(&name).font(th::mono(12.0)).color(c.muted));
                    w::muted(ui, "Its frame line, summary, crash reports and captures are deleted.");
                    ui.add_space(th::S2);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if w::danger_button(ui, icon::TRASH, "Delete", true).clicked() {
                            match rebuild_launcher::session::delete(&self.home.sessions_dir(), &dir) {
                                Ok(()) => self.toast(Tone::Accent, "Session deleted"),
                                Err(e) => self.toast(Tone::Danger, e),
                            }
                            self.refresh_sessions();
                            close = true;
                        }
                        if w::button(ui, "", "Cancel", true).clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if let Some(n) = next {
            self.dialog = Some(n);
        }
        if close || repick || resp.should_close() {
            self.dialog = None;
        }
        if repick {
            self.pick_game_folder();
        }
    }
}

/// "Experimental" settings set away from their defaults (Stable only refuses them).
pub fn experimental_set(c: &rebuild_config::Config) -> bool {
    rebuild_config::REGISTRY.iter().any(|s| s.tier == Tier::Experimental && c.get(s.key) != s.default.value())
}
