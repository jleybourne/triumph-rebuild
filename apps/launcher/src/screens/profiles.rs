//! Profiles: the built-in hardware tiers as cards (purpose and chips for output, upscaler and limits;
//! VRAM marked "Build 5"), the stress tests (one limit each) in their own row, your profiles, and the
//! "limits in force" panel for the selected one (Use, Save a copy, Export .toml), plus Import .toml.

use super::{grid, heading, split, subheading};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{App, Pick};
use eframe::egui::{self, CornerRadius, Sense, StrokeKind, Vec2};
use rebuild_config::profiles::Active;
use rebuild_config::{Builtin, Config, BUILTIN_PROFILES};
use rebuild_launcher::store::Store;

fn gib(m: i64) -> String {
    if m % 1024 == 0 {
        format!("{} GiB", m / 1024)
    } else {
        format!("{:.1} GiB", m as f64 / 1024.0)
    }
}

/// The chips for a configuration: output, upscaler, then each limit (or "no limits").
pub fn chips(ui: &mut egui::Ui, c: &Config) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        w::chip(ui, &format!("{}×{}", c.int("display.width"), c.int("display.height")), Tone::Muted);
        w::chip(ui, &format!("{} {}", c.choice("upscaler.backend").to_uppercase(), c.choice("upscaler.mode").replace('_', " ")), Tone::Muted);
        let mut any = false;
        let ram = c.int("limits.ram_mb");
        if ram > 0 {
            w::chip(ui, &format!("RAM {}", gib(ram)), Tone::Accent);
            any = true;
        }
        let vram = c.int("limits.vram_mb");
        if vram > 0 {
            w::chip(ui, &format!("VRAM {} · Build 5", gib(vram)), Tone::Warn).on_hover_text("Enforced from Build 5 (the layer's own GPU heaps): recorded, not applied yet");
            any = true;
        }
        let cores = c.int("limits.cpu_cores");
        if cores > 0 {
            w::chip(ui, &format!("{cores} CPUs"), Tone::Accent).on_hover_text("logical processors");
            any = true;
        }
        let rate = c.int("limits.cpu_rate");
        if rate > 0 {
            w::chip(ui, &format!("CPU {rate} %"), Tone::Accent);
            any = true;
        }
        if !any {
            w::chip(ui, "no limits", Tone::Ok);
        }
    });
}

/// A built-in's values on their own, as a configuration (for its chips).
fn builtin_config(b: &Builtin) -> Config {
    let mut c = Config::parse("").unwrap();
    let _ = b.apply(&mut c);
    c
}

/// A selectable profile card; returns true when clicked.
fn profile_card(ui: &mut egui::Ui, id: &str, title: &str, purpose: &str, cfg: &Config, selected: bool, active: Option<&str>) -> bool {
    let c = th::p();
    let (fill, line, width) = if selected { (c.accent_bg, c.accent, 1.5) } else { (c.panel, c.line, 1.0) };
    let (_, rect) = w::panel(ui, fill, line, width, 14.0, |ui| {
        ui.add(egui::Label::new(egui::RichText::new(title).font(th::semibold(14.0)).color(c.text)).wrap());
        if !purpose.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(purpose).font(th::small()).color(c.muted)).wrap());
        }
        ui.add_space(4.0);
        if let Some(a) = active {
            w::chip(ui, a, Tone::Ok);
        }
        chips(ui, cfg);
    });
    let resp = ui.interact(rect, egui::Id::new(("profile-card", id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.hovered() && !selected {
        ui.painter().rect_stroke(rect, CornerRadius::same(th::R_LG), th::stroke(1.0, c.line_hi), StrokeKind::Inside);
    }
    resp.clicked()
}

impl App {
    pub fn profiles_page(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Profiles", "Built-in machine classes and stress tests for testing the same build, and your own sets of settings.");
        let active = Active::of(&self.store.model.config);
        let active_name = |name: &str| match &active {
            Active::Builtin(b) if b.name == name => Some("active"),
            Active::Edited(b) if b.name == name => Some("active, edited"),
            _ => None,
        };
        let tiers: Vec<&'static Builtin> = BUILTIN_PROFILES.iter().filter(|b| !b.name.starts_with("stress-")).collect();
        let stress: Vec<&'static Builtin> = BUILTIN_PROFILES.iter().filter(|b| b.name.starts_with("stress-")).collect();
        let mut clicked: Option<Pick> = None;
        let mut import = false;
        let mut save_new = false;
        let mut use_it = false;
        let mut copy = false;
        let mut export = false;
        let pick = self.pick.clone();
        let user_profiles = self.profiles.clone();
        let current = self.store.model.config.profile.clone();
        let users: Vec<(String, Option<Config>)> = user_profiles.iter().map(|n| (n.clone(), self.home.load_profile(n).ok())).collect();
        split(
            ui,
            0.68,
            |ui| {
                subheading(ui, "Hardware tiers");
                grid(ui, tiers.len(), 230.0, 3, |ui, i| {
                    let b = tiers[i];
                    if profile_card(ui, b.name, b.label, b.purpose, &builtin_config(b), pick == Some(Pick::Builtin(b.name)), active_name(b.name)) {
                        clicked = Some(Pick::Builtin(b.name));
                    }
                });
                subheading(ui, "Stress tests (one limit each)");
                grid(ui, stress.len(), 170.0, 4, |ui, i| {
                    let b = stress[i];
                    let label = b.label.trim_start_matches("Stress: ");
                    if profile_card(ui, b.name, label, "", &builtin_config(b), pick == Some(Pick::Builtin(b.name)), active_name(b.name)) {
                        clicked = Some(Pick::Builtin(b.name));
                    }
                });
                subheading(ui, "Your profiles");
                if users.is_empty() {
                    w::muted(ui, "None yet. Save the current settings under a name on the right, or import a .toml.");
                    ui.add_space(th::S1);
                } else {
                    grid(ui, users.len(), 230.0, 3, |ui, i| {
                        let (n, cfg) = &users[i];
                        let cfg = cfg.clone().unwrap_or_else(|| Config::parse("").unwrap());
                        let act = (matches!(&active, Active::User(_)) && *n == current).then_some("active");
                        if profile_card(ui, n, n, "", &cfg, pick == Some(Pick::User(n.clone())), act) {
                            clicked = Some(Pick::User(n.clone()));
                        }
                    });
                }
            },
            |ui| {
                let c = th::p();
                // the selected profile, or the settings in force now
                let (title, cfg, is_builtin): (String, Config, bool) = match &pick {
                    Some(Pick::Builtin(n)) => {
                        let b = rebuild_config::builtin(n).unwrap();
                        let mut cfg = self.store.model.config.clone();
                        let _ = b.apply(&mut cfg);
                        (b.label.to_string(), cfg, true)
                    }
                    Some(Pick::User(n)) => (n.clone(), users.iter().find(|(u, _)| u == n).and_then(|(_, c)| c.clone()).unwrap_or_else(|| self.store.model.config.clone()), false),
                    None => (format!("In force now: {}", active.describe()), self.store.model.config.clone(), false),
                };
                w::card(ui, Some("Limits in force"), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(&title).font(th::medium(13.5)).color(c.text)).wrap());
                    ui.add_space(4.0);
                    let limits = rebuild_config::profiles::limits_in_force(&cfg);
                    if limits.is_empty() {
                        ui.label(egui::RichText::new("No limits: the whole machine").font(th::small()).color(c.muted));
                    }
                    for l in &limits {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon::GAUGE).font(th::icon(14.0)).color(if l.contains("VRAM") { c.warn } else { c.accent }));
                            ui.label(egui::RichText::new(l).font(th::regular(13.0)).color(c.text));
                        });
                    }
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(format!("{}×{} · {} {}", cfg.int("display.width"), cfg.int("display.height"), cfg.choice("upscaler.backend").to_uppercase(), cfg.choice("upscaler.mode").replace('_', " "))).font(th::small()).color(c.muted));
                    if is_builtin {
                        if let Some(Pick::Builtin(n)) = &pick {
                            let b = rebuild_config::builtin(n).unwrap();
                            if !b.stable() {
                                ui.label(egui::RichText::new(format!("{}  Sets {} (experimental): refused while Stable only is on", icon::WARNING, b.experimental_keys().join(", "))).font(th::small()).color(c.warn));
                            }
                        }
                    }
                    ui.add_space(th::S1);
                    ui.horizontal_wrapped(|ui| {
                        if w::primary_button(ui, icon::CHECK, "Use", Vec2::new(80.0, 30.0), pick.is_some()).on_hover_text("Apply it to this game's settings (save to keep)").clicked() {
                            use_it = true;
                        }
                        if w::button(ui, icon::COPY, "Save a copy", true).clicked() {
                            copy = true;
                        }
                        if w::button(ui, icon::EXPORT, "Export .toml", true).clicked() {
                            export = true;
                        }
                    });
                });
                ui.add_space(th::S2);
                w::card(ui, Some("Save or import"), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.profile_name).hint_text("profile name").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(8, 6)));
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if w::button(ui, icon::FLOPPY_DISK, "Save current as", !self.profile_name.trim().is_empty()).clicked() {
                            save_new = true;
                        }
                        if w::button(ui, icon::DOWNLOAD_SIMPLE, "Import .toml", true).clicked() {
                            import = true;
                        }
                    });
                    ui.add_space(th::S1);
                    w::rule(ui);
                    ui.horizontal(|ui| {
                        let mut stable = self.store.model.config.stable_only;
                        if w::toggle(ui, &mut stable).changed() {
                            if stable && super::experimental_set(&self.store.model.config) {
                                self.toasts.push(crate::Toast { text: "Reset the experimental settings first".into(), tone: Tone::Danger, at: -1.0 });
                            } else {
                                self.store.model.config.stable_only = stable;
                            }
                        }
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("Stable only").font(th::medium(13.5)).color(c.text));
                            w::muted(ui, "Experimental settings are refused in this profile.");
                        });
                    });
                });
                for p in &self.import_problems {
                    ui.label(egui::RichText::new(format!("{}  {p}", icon::WARNING_CIRCLE)).font(th::small()).color(c.danger));
                }
            },
        );
        if let Some(p) = clicked {
            self.pick = if self.pick.as_ref() == Some(&p) { None } else { Some(p) };
        }
        if use_it {
            match self.pick.clone() {
                Some(Pick::Builtin(n)) => {
                    if let Some(b) = rebuild_config::builtin(n) {
                        self.apply_builtin(b);
                    }
                }
                Some(Pick::User(n)) => self.load_profile(&n),
                None => {}
            }
        }
        if copy {
            let (name, cfg) = match self.pick.clone() {
                Some(Pick::Builtin(n)) => {
                    let mut cfg = self.store.model.config.clone();
                    let b = rebuild_config::builtin(n).unwrap();
                    if !b.stable() {
                        cfg.stable_only = false;
                    }
                    let _ = b.apply(&mut cfg);
                    (format!("{n} copy"), Some(cfg))
                }
                Some(Pick::User(n)) => (format!("{n} copy"), self.home.load_profile(&n).ok()),
                None => (if self.profile_name.trim().is_empty() { "my profile".into() } else { self.profile_name.trim().to_string() }, Some(self.store.model.config.clone())),
            };
            if let Some(cfg) = cfg {
                match self.home.save_profile(&name, &cfg) {
                    Ok(_) => {
                        self.profiles = self.home.profiles();
                        self.pick = Some(Pick::User(name.clone()));
                        self.toast(Tone::Ok, format!("Saved “{name}”"));
                    }
                    Err(e) => self.toast(Tone::Danger, e),
                }
            }
        }
        if save_new {
            match self.home.save_profile(&self.profile_name, &self.store.model.config) {
                Ok(_) => {
                    self.store.model.config.profile = self.profile_name.trim().to_string();
                    self.profiles = self.home.profiles();
                    self.toast(Tone::Ok, format!("Profile “{}” saved", self.profile_name.trim()));
                }
                Err(e) => self.toast(Tone::Danger, e),
            }
        }
        if export {
            let cfg = match self.pick.clone() {
                Some(Pick::Builtin(n)) => rebuild_config::profiles::matrix_config(&self.store.model.config, rebuild_config::builtin(n).unwrap()).unwrap_or_else(|_| self.store.model.config.clone()),
                Some(Pick::User(n)) => self.home.load_profile(&n).unwrap_or_else(|_| self.store.model.config.clone()),
                None => self.store.model.config.clone(),
            };
            let file = format!("{}.toml", rebuild_launcher::session::folder_part(&cfg.profile));
            if let Some(p) = rfd::FileDialog::new().add_filter("toml", &["toml"]).set_file_name(file).save_file() {
                match rebuild_launcher::home::export(&cfg, &p) {
                    Ok(()) => self.toast(Tone::Ok, format!("Exported {}", p.display())),
                    Err(e) => self.toast(Tone::Danger, e),
                }
            }
        }
        if import {
            if let Some(p) = rfd::FileDialog::new().add_filter("toml", &["toml"]).pick_file() {
                match Store::load(&p) {
                    Ok(s) => self.adopt(s, format!("Imported {}", p.display())),
                    Err(e) => self.toast(Tone::Danger, e),
                }
            }
        }
    }
}
