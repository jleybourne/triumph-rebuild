//! Diagnostics → Storage: where every file lives (`rebuild_storage`, `docs/design/storage.md`), portable or
//! per-user, the selected game's sizes, Open folder, Clear cache (caches only, never saves), and Export /
//! Import of saves and settings.

use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{bytes, open_folder, App};
use eframe::egui::{self, Align, Layout};
use rebuild_storage::{GameStore, ImportOptions, Mode, Users, What};
use std::path::PathBuf;

impl App {
    /// The selected game's folders, with its build.
    pub fn game_store(&self) -> Option<GameStore> {
        let g = self.library.selected_game()?;
        Some(self.storage.game(&g.id).with_build(&rebuild_storage::identify(&g.dir).build))
    }

    fn import_refused(&self) -> Option<&'static str> {
        if self.running.is_some() || rebuild_storage::title_running() {
            return Some("the title is running: close it before importing saves");
        }
        None
    }

    pub fn storage_card(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        // measured once per game (and on refresh), not every frame: the folders can hold many files
        let Some(id) = self.library.selected.clone() else { return };
        if self.storage_sizes.as_ref().is_none_or(|(g, _)| g.id != id) {
            let Some(g) = self.game_store() else { return };
            let s = g.sizes();
            self.storage_sizes = Some((g, s));
        }
        let Some((store, sizes)) = self.storage_sizes.clone() else { return };
        let mut open: Option<PathBuf> = None;
        let mut refresh = false;
        w::card(ui, None, |ui| {
            ui.horizontal(|ui| {
                w::text(ui, "Storage", th::h2(), c.text);
                let (label, tone, why) = match self.storage.mode {
                    Mode::Portable => ("Portable", Tone::Ok, "Everything is in the data folder beside the launcher: move or delete it as one folder."),
                    Mode::PerUser => ("Per-user", Tone::Accent, "The launcher's folder is not writable, so settings and saves are in %APPDATA% and caches in %LOCALAPPDATA%."),
                    Mode::Explicit => ("REBUILD_DATA_DIR", Tone::Warn, "The data folder was given by REBUILD_DATA_DIR."),
                };
                w::chip(ui, label, tone).on_hover_text(why);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button(ui, icon::ARROWS_CLOCKWISE, "", true).on_hover_text("Measure again").clicked() {
                        refresh = true;
                    }
                });
            });
            ui.add_space(4.0);
            let row = |ui: &mut egui::Ui, k: &str, v: String, p: Option<PathBuf>, open: &mut Option<PathBuf>| {
                ui.horizontal(|ui| {
                    w::fixed(ui, 118.0, egui::RichText::new(k).font(th::medium(13.0)).color(c.text));
                    ui.label(egui::RichText::new(v).font(th::regular(13.0)).color(c.text_2));
                    if let Some(p) = p {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::button(ui, icon::FOLDER_OPEN, "", true).on_hover_text(format!("Open {}", p.display())).clicked() {
                                *open = Some(p.clone());
                            }
                        });
                    }
                });
            };
            row(ui, "Data", self.storage.user.display().to_string(), Some(self.storage.user.clone()), &mut open);
            if self.storage.cache != self.storage.user {
                row(ui, "Caches", self.storage.cache.display().to_string(), Some(self.storage.cache.clone()), &mut open);
            }
            if self.storage.local != self.storage.user && self.storage.local != self.storage.cache {
                row(ui, "Logs", self.storage.logs_dir().display().to_string(), Some(self.storage.logs_dir()), &mut open);
            }
            for n in &self.storage.notes {
                w::muted(ui, n);
            }
            w::rule(ui);
            row(ui, "Game id", format!("{} · build {}", store.id, if store.build.is_empty() { "unknown" } else { &store.build }), Some(store.dir.clone()), &mut open);
            let users = store.save_users();
            row(ui, "Saves", format!("{} · {} user{}", bytes(sizes.saves), users.len(), if users.len() == 1 { "" } else { "s" }), Some(store.saves_root()), &mut open);
            row(ui, "Title storage", bytes(sizes.storage), Some(store.storage_dir()), &mut open);
            row(ui, "Settings", format!("{} · overlay {} · mods {}", bytes(sizes.settings), bytes(sizes.overlay), bytes(sizes.mods)), None, &mut open);
            if sizes.backups > 0 {
                row(ui, "Backups", bytes(sizes.backups), Some(store.backups_dir()), &mut open);
            }
            let builds: Vec<String> = sizes.cache.iter().map(|(b, n)| format!("{b} {}", bytes(*n))).collect();
            row(ui, "Cache", format!("{}{}", bytes(sizes.cache_total), if builds.is_empty() { String::new() } else { format!(" · {}", builds.join(" · ")) }), Some(store.cache.clone()), &mut open);
            row(ui, "Logs", bytes(sizes.logs), Some(store.sessions_dir()), &mut open);
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let old_builds = sizes.cache.iter().any(|(b, _)| b != &store.build && b != "bcpack");
                if w::button(ui, icon::TRASH, "Clear old builds", old_builds && self.running.is_none()).on_hover_text("Caches of other game builds only. Never saves or settings.").clicked() {
                    self.clear_cache(&store, true);
                    refresh = true;
                }
                if w::danger_button(ui, icon::TRASH, "Clear cache", sizes.cache_total > 0 && self.running.is_none())
                    .on_hover_text("Every regenerable cache of this game (shaders, pipelines, images, decoded textures). Never saves or settings: they are rebuilt on the next launch.")
                    .clicked()
                {
                    self.clear_cache(&store, false);
                    refresh = true;
                }
                if w::button(ui, icon::EXPORT, "Export saves…", !users.is_empty()).on_hover_text("Every user's saves, in one .zip with a manifest").clicked() {
                    self.export(&store, What { saves: Some(Users::All), settings: false });
                }
                if w::button(ui, icon::EXPORT, "Export settings…", store.settings_toml().is_file()).on_hover_text("settings.toml and the active profile").clicked() {
                    self.export(&store, What { saves: None, settings: true });
                }
                if w::button(ui, icon::EXPORT, "Export both…", !users.is_empty() || store.settings_toml().is_file()).clicked() {
                    self.export(&store, What::EVERYTHING);
                }
                if w::button(ui, icon::DOWNLOAD_SIMPLE, "Import…", true).on_hover_text("Checked first; the current saves are backed up before anything is replaced").clicked() {
                    self.pick_import(&store);
                }
            });
            if let Some((path, plan)) = self.import_plan.clone() {
                ui.add_space(6.0);
                w::rule(ui);
                let m = &plan.manifest;
                ui.label(egui::RichText::new(format!("Import {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())).font(th::medium(13.5)).color(c.text));
                w::muted(ui, &format!("Exported {} · build {} · {} user(s) · {} file(s)", m.exported, m.build, m.users.len(), m.files.len()));
                for (from, to) in &plan.users {
                    if from != to {
                        w::muted(ui, &format!("User {} is imported as this machine's user {}", rebuild_storage::user_dir_name(*from), rebuild_storage::user_dir_name(*to)));
                    }
                }
                for (label, list, tone) in [("replaced", &plan.replaced, Tone::Warn), ("removed", &plan.removed, Tone::Danger), ("added", &plan.added, Tone::Ok)] {
                    if list.is_empty() {
                        continue;
                    }
                    ui.horizontal_wrapped(|ui| {
                        w::chip(ui, &format!("{} {label}", list.len()), tone);
                        let shown: Vec<&str> = list.iter().take(6).map(String::as_str).collect();
                        w::muted(ui, &format!("{}{}", shown.join(", "), if list.len() > 6 { ", …" } else { "" }));
                    });
                }
                if plan.changes() == 0 {
                    w::muted(ui, "Nothing would change: these files are already here.");
                }
                let refused = self.import_refused();
                if let Some(r) = refused {
                    ui.label(egui::RichText::new(r).font(th::small()).color(c.danger));
                }
                ui.horizontal(|ui| {
                    if w::button(ui, icon::CHECK, "Back up and import", plan.changes() > 0 && refused.is_none()).clicked() {
                        let ctx = ui.ctx().clone();
                        self.run_import(&ctx, &store, &path, &plan);
                        refresh = true;
                    }
                    if w::button(ui, icon::X, "Cancel", true).clicked() {
                        self.import_plan = None;
                    }
                });
            }
        });
        if refresh {
            self.storage_sizes = None;
        }
        if let Some(p) = open {
            open_folder(&p);
        }
    }

    fn clear_cache(&mut self, store: &GameStore, old_only: bool) {
        let keep = old_only.then_some(store.build.as_str());
        match store.clear_cache(keep) {
            Ok(n) => self.toast(Tone::Accent, format!("Cache cleared: {} freed (saves and settings untouched)", bytes(n))),
            Err(e) => self.toast(Tone::Danger, e),
        }
    }

    fn export(&mut self, store: &GameStore, what: What) {
        let kind = match (what.saves.is_some(), what.settings) {
            (true, true) => "saves-and-settings",
            (true, false) => "saves",
            _ => "settings",
        };
        let name = format!("{}-{kind}-{}.zip", store.id, rebuild_storage::stamp(rebuild_storage::now_unix()));
        let Some(p) = rfd::FileDialog::new().add_filter("zip", &["zip"]).set_file_name(name).save_file() else { return };
        match rebuild_storage::export(store, &what, &p) {
            Ok(m) => self.toast(Tone::Ok, format!("Exported {} file(s) to {}", m.files.len(), p.display())),
            Err(e) => self.toast(Tone::Danger, e),
        }
    }

    fn pick_import(&mut self, store: &GameStore) {
        let Some(p) = rfd::FileDialog::new().add_filter("zip", &["zip"]).pick_file() else { return };
        let o = match rebuild_storage::transfer::read_export(&p) {
            Ok((m, _)) => ImportOptions::for_this_machine(&m),
            Err(e) => return self.toast(Tone::Danger, e),
        };
        match rebuild_storage::inspect(store, &p, &o) {
            Ok(plan) => self.import_plan = Some((p, plan)),
            Err(e) => self.toast(Tone::Danger, e),
        }
    }

    fn run_import(&mut self, ctx: &egui::Context, store: &GameStore, path: &std::path::Path, plan: &rebuild_storage::Plan) {
        if let Some(r) = self.import_refused() {
            return self.toast(Tone::Danger, r);
        }
        let o = ImportOptions::for_this_machine(&plan.manifest);
        match rebuild_storage::import(store, path, &o) {
            Ok(done) => {
                self.import_plan = None;
                let backup = done.backup.map(|b| format!("; the old ones are in {}", b.display())).unwrap_or_default();
                self.toast(Tone::Ok, format!("Imported {} change(s){backup}", done.plan.changes()));
                // the settings may have changed under the launcher: read them again
                if plan.manifest.settings {
                    self.load_game(ctx);
                }
            }
            Err(e) => self.toast(Tone::Danger, e),
        }
    }
}
