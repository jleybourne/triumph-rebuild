//! Saves: every user's save containers for the selected game, a container's blobs, the title's save
//! header and ASAV header decoded with their checksums judged as the title's load path judges them, a
//! hex view, the edits the format supports, backups (undo), and import / export of one container, one
//! decoded save (JSON), or every save at once (`docs/design/save-editor.md`).
//!
//! Every write goes through `rebuild_saves::store::write_container`: refused while the title runs,
//! the replaced file backed up first, written all or nothing the runtime's way.

use super::{empty_state, heading, split};
use crate::ui::icons as icon;
use crate::ui::theme as th;
use crate::ui::widgets::{self as w, Tone};
use crate::{open_folder, App};
use eframe::egui::{self, Align, Layout};
use rebuild_gdk::areas::xgamesave::Stored;
use rebuild_saves::format::{Editable, Save};
use rebuild_saves::store::{self, Entry};
use rebuild_saves::{json, section};
use std::path::{Path, PathBuf};

const ROWS: usize = 24;

/// The Saves screen's state.
#[derive(Default)]
pub struct SavesUi {
    /// The folder listed; `None` until the screen first opens (then the game's saves folder).
    pub root: Option<PathBuf>,
    pub entries: Vec<Entry>,
    pub open: Option<Opened>,
    pub note: Option<(Tone, String)>,
}

/// One opened container.
pub struct Opened {
    pub path: PathBuf,
    pub stored: Stored,
    /// The save decoded from it: `None` if it holds no save blobs.
    pub save: Option<Result<Save, String>>,
    /// The edited save, until it is written or dropped.
    pub edited: Option<Save>,
    pub name: String,
    pub patch_at: String,
    pub patch_hex: String,
    /// Which blob the hex view shows, and its page.
    pub blob: usize,
    pub page: usize,
    pub backups: Vec<PathBuf>,
}

enum Act {
    Refresh,
    OpenFolder,
    Select(PathBuf),
    Rename(String),
    Patch(String, String),
    Write,
    Discard,
    Restore(PathBuf),
    ExportContainer,
    ImportContainer,
    ExportSection,
    ImportSection,
    ExportAll,
    ImportAll,
}

fn when(t: i64) -> String {
    if t == 0 {
        "-".into()
    } else {
        let s = store::stamp(t);
        format!("{}-{}-{} {}:{} UTC", &s[0..4], &s[4..6], &s[6..8], &s[9..11], &s[11..13])
    }
}

fn size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn open(path: &Path, root: &Path) -> Result<Opened, String> {
    let stored = store::read(path)?;
    let save = store::save_of(&stored);
    let name = match &save {
        Some(Ok(s)) => s.header.name.clone(),
        _ => String::new(),
    };
    let blob = stored.blobs.iter().position(|b| b.0 == rebuild_saves::format::DATA_BLOB).unwrap_or(0);
    Ok(Opened { path: path.into(), backups: store::backups(root, path), stored, save, edited: None, name, patch_at: "0".into(), patch_hex: String::new(), blob, page: 0 })
}

impl App {
    fn saves_root(&self) -> PathBuf {
        let game = self.library.selected_game().map(|g| g.id.clone()).unwrap_or_default();
        store::saves_root(&game)
    }

    pub fn saves_page(&mut self, ui: &mut egui::Ui) {
        let c = th::p();
        heading(
            ui,
            "Saves",
            "Offline single-player saves for this game. Every write keeps a backup first, is all or nothing, and waits until the title is closed.",
        );
        if self.saves.root.is_none() {
            self.saves.root = Some(self.saves_root());
            self.saves.entries = store::list(self.saves.root.as_ref().unwrap());
        }
        let root = self.saves.root.clone().unwrap();
        let running = self.is_running();
        let mut act: Option<Act> = None;
        if running {
            w::chip(ui, &format!("{} The title is running: saves are read-only until it closes", icon::WARNING), Tone::Warn);
            ui.add_space(th::S2);
        }
        if let Some((tone, text)) = &self.saves.note {
            w::chip(ui, text, *tone);
            ui.add_space(th::S2);
        }
        let entries = &self.saves.entries;
        let open_path = self.saves.open.as_ref().map(|o| o.path.clone());
        let opened = &mut self.saves.open;
        let mut act_r: Option<Act> = None;
        split(
            ui,
            0.38,
            |ui| {
                w::card(ui, None, |ui| {
                    ui.horizontal(|ui| {
                        w::text(ui, "Containers", th::h2(), c.text);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if w::button(ui, icon::ARROWS_CLOCKWISE, "", true).on_hover_text("Refresh").clicked() {
                                act = Some(Act::Refresh);
                            }
                            if w::button(ui, icon::FOLDER_OPEN, "", true).on_hover_text(root.display().to_string()).clicked() {
                                act = Some(Act::OpenFolder);
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if entries.is_empty() {
                        empty_state(ui, icon::FLOPPY_DISK, "No saves yet", "The title writes its saves here once it has run far enough to save (first-run setup finished).");
                    }
                    let mut last = (String::new(), String::new());
                    for e in entries {
                        if (e.user.clone(), e.scid.clone()) != last {
                            last = (e.user.clone(), e.scid.clone());
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new(format!("{} user {} · {}", icon::USER_CIRCLE, e.user, e.scid)).font(th::small()).color(c.muted));
                        }
                        let selected = open_path.as_ref() == Some(&e.path);
                        let title = match &e.container {
                            Some((n, _)) => n.clone(),
                            None => format!("{} (unreadable)", e.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()),
                        };
                        let r = ui.selectable_label(selected, egui::RichText::new(title).font(th::semibold(13.5)));
                        ui.label(egui::RichText::new(format!("{} · {} · {} blobs", when(e.modified), size(e.bytes), e.blobs)).font(th::small()).color(c.muted));
                        if r.clicked() && e.container.is_some() {
                            act = Some(Act::Select(e.path.clone()));
                        }
                    }
                });
                ui.add_space(th::S2);
                w::card(ui, Some("Every save at once"), |ui| {
                    w::muted(ui, "A folder with every container and its manifest; importing checks all of them before writing any.");
                    ui.horizontal(|ui| {
                        if w::button(ui, icon::EXPORT, "Export all", !entries.is_empty()).clicked() {
                            act = Some(Act::ExportAll);
                        }
                        if w::button(ui, icon::DOWNLOAD_SIMPLE, "Import all", !running).clicked() {
                            act = Some(Act::ImportAll);
                        }
                    });
                });
            },
            |ui| {
                let Some(o) = opened.as_mut() else {
                    w::card(ui, None, |ui| empty_state(ui, icon::FILE_TEXT, "No container open", "Pick one on the left."));
                    return;
                };
                container_view(ui, o, running, &mut act_r);
            },
        );
        act = act.or(act_r);
        if let Some(a) = act {
            self.saves_act(a, &root);
        }
    }

    fn saves_act(&mut self, a: Act, root: &Path) {
        let running = self.is_running();
        let guard = move || running;
        let st = &mut self.saves;
        match run(st, a, root, &guard) {
            Ok(Some(m)) => st.note = Some((Tone::Ok, m)),
            Ok(None) => {}
            Err(e) => st.note = Some((Tone::Danger, e)),
        }
        st.entries = store::list(root);
        if let Some(o) = st.open.as_mut() {
            o.backups = store::backups(root, &o.path);
        }
    }
}

/// One action; a message for the user, or why it was refused.
fn run(st: &mut SavesUi, a: Act, root: &Path, guard: &dyn Fn() -> bool) -> Result<Option<String>, String> {
    {
        match a {
            Act::Refresh => Ok(None),
            Act::OpenFolder => {
                open_folder(root);
                Ok(None)
            }
            Act::Select(p) => {
                st.open = Some(open(&p, root)?);
                Ok(None)
            }
            Act::Rename(n) => {
                let o = st.open.as_mut().ok_or("nothing open")?;
                let mut s = o.edited.clone().or_else(|| o.save.clone()?.ok()).ok_or("no save to edit")?;
                s.set_name(&n).map_err(|e| e.0)?;
                o.edited = Some(s);
                Ok(Some("Renamed; not written yet".into()))
            }
            Act::Patch(at, hex) => {
                let o = st.open.as_mut().ok_or("nothing open")?;
                let at = parse_offset(&at)?;
                let bytes = json::from_hex(&hex)?;
                if bytes.is_empty() {
                    return Err("no bytes to write".into());
                }
                let mut s = o.edited.clone().or_else(|| o.save.clone()?.ok()).ok_or("no save to edit")?;
                s.patch_body(at, &bytes).map_err(|e| e.0)?;
                o.edited = Some(s);
                Ok(Some(format!("{} bytes changed at body +{at:#x}; checksums recomputed; not written yet", bytes.len())))
            }
            Act::Discard => {
                if let Some(o) = st.open.as_mut() {
                    o.edited = None;
                    o.name = o.save.as_ref().and_then(|s| s.as_ref().ok()).map(|s| s.header.name.clone()).unwrap_or_default();
                }
                Ok(Some("Edits dropped".into()))
            }
            Act::Write => {
                let o = st.open.as_ref().ok_or("nothing open")?;
                let s = o.edited.as_ref().ok_or("nothing edited")?;
                let mut c = store::with_save(&o.stored, s);
                c.modified = store::now();
                let dir = o.path.parent().ok_or("no folder")?.to_path_buf();
                let w = store::write_container(root, &dir, &c, guard)?;
                st.open = Some(open(&w.path, root)?);
                Ok(Some(format!("Written; the previous version is backed up ({})", w.backup.map(|b| b.file_name().unwrap_or_default().to_string_lossy().into_owned()).unwrap_or_default())))
            }
            Act::Restore(b) => {
                let o = st.open.as_ref().ok_or("nothing open")?;
                let w = store::restore(root, &o.path.clone(), &b, guard)?;
                st.open = Some(open(&w.path, root)?);
                Ok(Some("Backup restored; what it replaced is backed up too".into()))
            }
            Act::ExportContainer => {
                let o = st.open.as_ref().ok_or("nothing open")?;
                let Some(dest) = rfd::FileDialog::new().set_title("Export the container to a folder").pick_folder() else { return Ok(None) };
                let (gsc, _) = store::export_container(&o.path, &dest)?;
                Ok(Some(format!("Exported {} and its manifest", gsc.display())))
            }
            Act::ImportContainer => {
                let o = st.open.as_ref().ok_or("open a container first: the import goes into its folder")?;
                let Some(f) = rfd::FileDialog::new().add_filter("container", &["gsc"]).pick_file() else { return Ok(None) };
                let dir = o.path.parent().ok_or("no folder")?.to_path_buf();
                let w = store::import_container(root, &dir, &f, guard)?;
                st.open = Some(open(&w.path, root)?);
                Ok(Some(format!("Imported{}", if w.backup.is_some() { "; the container it replaced is backed up" } else { "" })))
            }
            Act::ExportSection => {
                let o = st.open.as_ref().ok_or("nothing open")?;
                let s = o.edited.clone().or_else(|| o.save.clone()?.ok()).ok_or("this container holds no decoded save")?;
                let file = format!("{}.save.json", o.stored.name.replace('/', "_"));
                let Some(p) = rfd::FileDialog::new().add_filter("json", &["json"]).set_file_name(file).save_file() else { return Ok(None) };
                rebuild_launcher::write_atomic(&p, &section::export(&o.stored.name, &s)).map_err(|e| format!("{}: {e}", p.display()))?;
                Ok(Some(format!("Exported {}", p.display())))
            }
            Act::ImportSection => {
                let o = st.open.as_mut().ok_or("nothing open")?;
                let cur = o.save.clone().ok_or("this container holds no decoded save")??;
                let Some(p) = rfd::FileDialog::new().add_filter("json", &["json"]).pick_file() else { return Ok(None) };
                let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                let s = section::import(&text, &o.stored.name, &cur)?;
                o.name = s.header.name.clone();
                o.edited = if s == cur { None } else { Some(s) };
                Ok(Some(if o.edited.is_some() { "Imported into the editor; checksums recomputed; not written yet" } else { "Imported: nothing differs from the save" }.into()))
            }
            Act::ExportAll => {
                let Some(dest) = rfd::FileDialog::new().set_title("Folder for every save").pick_folder() else { return Ok(None) };
                let dest = dest.join(format!("saves-{}", store::stamp(store::now())));
                let n = store::export_all(root, &dest)?;
                Ok(Some(format!("Exported {n} containers to {}", dest.display())))
            }
            Act::ImportAll => {
                let Some(src) = rfd::FileDialog::new().set_title("A folder made by Export all").pick_folder() else { return Ok(None) };
                let w = store::import_all(root, &src, guard)?;
                Ok(Some(format!("Imported {} containers; each one replaced is backed up", w.len())))
            }
        }
    }
}

fn parse_offset(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let r = match s.strip_prefix("0x") {
        Some(h) => usize::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| format!("offset {s:?}: a number, decimal or 0x hex"))
}

fn kv(ui: &mut egui::Ui, k: &str, v: String) {
    let c = th::p();
    ui.horizontal(|ui| {
        w::fixed(ui, 150.0, egui::RichText::new(k).font(th::small()).color(c.muted));
        ui.label(egui::RichText::new(v).font(th::mono(12.0)).color(c.text));
    });
}

fn check(ui: &mut egui::Ui, ok: bool, what: &str) {
    w::chip(ui, &format!("{} {what}", if ok { icon::CHECK } else { icon::X }), if ok { Tone::Ok } else { Tone::Danger });
}

fn container_view(ui: &mut egui::Ui, o: &mut Opened, running: bool, act: &mut Option<Act>) {
    let c = th::p();
    w::card(ui, None, |ui| {
        ui.horizontal(|ui| {
            w::text(ui, &o.stored.name, th::h2(), c.text);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, icon::DOWNLOAD_SIMPLE, "Import", !running).on_hover_text("Replace it with an exported container (.gsc and its manifest)").clicked() {
                    *act = Some(Act::ImportContainer);
                }
                if w::button(ui, icon::EXPORT, "Export", true).on_hover_text("The container file and a manifest").clicked() {
                    *act = Some(Act::ExportContainer);
                }
            });
        });
        kv(ui, "display name", o.stored.display.clone());
        kv(ui, "modified", when(o.stored.modified));
        kv(ui, "file", o.path.display().to_string());
        for (n, d) in &o.stored.blobs {
            kv(ui, "blob", format!("{n}  ({})", size(d.len() as u64)));
        }
    });
    ui.add_space(th::S2);
    let shown = o.edited.clone().or_else(|| o.save.clone().and_then(|s| s.ok()));
    match (&o.save, &shown) {
        (None, _) => {
            w::card(ui, Some("Contents"), |ui| {
                w::muted(ui, "Not one of the title's save containers (no \"Save Game Header\" / \"Save Game Data\"): shown as hex, read-only.");
            });
        }
        (Some(Err(e)), _) => {
            w::card(ui, Some("Contents"), |ui| {
                w::chip(ui, &format!("{} not understood: {e}", icon::WARNING), Tone::Warn);
                w::muted(ui, "The editor refuses to change a save it cannot parse completely. Shown as hex, read-only.");
            });
        }
        (Some(Ok(_)), Some(s)) => save_view(ui, o, s.clone(), running, act),
        _ => {}
    }
    ui.add_space(th::S2);
    hex_view(ui, o);
    ui.add_space(th::S2);
    w::card(ui, Some("Backups (undo)"), |ui| {
        if o.backups.is_empty() {
            w::muted(ui, "None yet: the editor makes one before every write.");
        }
        for b in o.backups.iter().take(10) {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(b.file_name().unwrap_or_default().to_string_lossy()).font(th::mono(11.5)).color(c.text_2));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if w::button(ui, icon::ARROW_COUNTER_CLOCKWISE, "Restore", !running).clicked() {
                        *act = Some(Act::Restore(b.clone()));
                    }
                });
            });
        }
    });
}

fn save_view(ui: &mut egui::Ui, o: &mut Opened, s: Save, running: bool, act: &mut Option<Act>) {
    let c = th::p();
    let problems = s.problems();
    w::card(ui, Some("Save header"), |ui| {
        ui.horizontal_wrapped(|ui| {
            check(ui, problems.is_empty(), if problems.is_empty() { "the title's load checks pass" } else { "the title would refuse this save" });
            if s.inner.signed() {
                w::chip(ui, &format!("kind {}: encrypted and signed, read-only", s.inner.kind), Tone::Muted);
            }
            if o.edited.is_some() {
                w::chip(ui, "edited, not written", Tone::Accent);
            }
        });
        for p in &problems {
            ui.label(egui::RichText::new(p).font(th::small()).color(c.danger));
        }
        for p in s.warnings() {
            ui.label(egui::RichText::new(p).font(th::small()).color(c.warn));
        }
        let h = &s.header;
        kv(ui, "version", h.version.to_string());
        kv(ui, "data", format!("{} bytes, hash {:#010x}", h.data_size, h.data_hash));
        kv(ui, "header hash", format!("{:#010x}", h.header_hash));
        kv(ui, "stamp", format!("{:#018x}", h.stamp));
        kv(ui, "name", h.name.clone());
        w::rule(ui);
        let i = &s.inner;
        kv(ui, "ASAV kind", format!("{} (word {}, flags {:#x})", i.kind, i.kind_word, i.flags));
        kv(ui, "ASAV text", format!("{:?}", i.tag));
        kv(ui, "ASAV values", format!("+0x154 {} · +0x158 {} · +0x15c {} · +0x164 {}", i.value_154, f32::from_bits(i.value_158), i.value_15c, f32::from_bits(i.value_164)));
        kv(ui, "ASAV hash", format!("{:#010x}", i.hash));
        kv(ui, "body", format!("{} bytes{}", s.body.len(), if i.signed() { " (encrypted, with a 32-byte signature)" } else { " (layout not decoded yet)" }));
    });
    ui.add_space(th::S2);
    let editable = s.editable();
    w::card(ui, Some("Edit"), |ui| {
        let can = matches!(editable, Editable::Yes) && !running;
        if let Editable::No(why) = &editable {
            w::muted(ui, why);
        }
        ui.horizontal(|ui| {
            w::fixed(ui, 150.0, egui::RichText::new("display name").font(th::small()).color(c.muted));
            ui.add_enabled(can, egui::TextEdit::singleline(&mut o.name).desired_width(260.0));
            if w::button(ui, icon::CHECK, "Set", can && o.name != s.header.name).clicked() {
                *act = Some(Act::Rename(o.name.clone()));
            }
        });
        ui.horizontal(|ui| {
            w::fixed(ui, 150.0, egui::RichText::new("body bytes").font(th::small()).color(c.muted));
            ui.add_enabled(can, egui::TextEdit::singleline(&mut o.patch_at).hint_text("offset").desired_width(70.0));
            ui.add_enabled(can, egui::TextEdit::singleline(&mut o.patch_hex).hint_text("hex bytes, e.g. 01ff").desired_width(180.0));
            if w::button(ui, icon::CHECK, "Patch", can && !o.patch_hex.trim().is_empty()).clicked() {
                *act = Some(Act::Patch(o.patch_at.clone(), o.patch_hex.clone()));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if w::primary_button(ui, icon::FLOPPY_DISK, "Write", egui::vec2(110.0, 30.0), o.edited.is_some() && !running).on_hover_text("Backs up the container, then writes the edit").clicked() {
                *act = Some(Act::Write);
            }
            if w::button(ui, icon::X, "Discard", o.edited.is_some()).clicked() {
                *act = Some(Act::Discard);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, icon::UPLOAD_SIMPLE, "Import JSON", !running).on_hover_text("Read an exported save back into the editor (checked; checksums recomputed)").clicked() {
                    *act = Some(Act::ImportSection);
                }
                if w::button(ui, icon::EXPORT, "Export JSON", true).on_hover_text("The decoded save as readable JSON").clicked() {
                    *act = Some(Act::ExportSection);
                }
            });
        });
    });
}

fn hex_view(ui: &mut egui::Ui, o: &mut Opened) {
    let c = th::p();
    let edited_data;
    let bytes: &[u8] = match (&o.edited, o.stored.blobs.get(o.blob)) {
        (Some(s), Some((n, _))) if n == rebuild_saves::format::DATA_BLOB => {
            edited_data = s.data();
            &edited_data
        }
        (Some(s), Some((n, _))) if n == rebuild_saves::format::HEADER_BLOB => {
            edited_data = s.header.to_bytes();
            &edited_data
        }
        (_, Some((_, d))) => d,
        (_, None) => &[],
    };
    let pages = bytes.len().div_ceil(16 * ROWS).max(1);
    o.page = o.page.min(pages - 1);
    w::card(ui, Some("Hex"), |ui| {
        ui.horizontal(|ui| {
            let names: Vec<String> = o.stored.blobs.iter().map(|b| b.0.clone()).collect();
            egui::ComboBox::from_id_salt("saves-blob").selected_text(names.get(o.blob).cloned().unwrap_or_default()).show_ui(ui, |ui| {
                for (k, n) in names.iter().enumerate() {
                    if ui.selectable_value(&mut o.blob, k, n).changed() {
                        o.page = 0;
                    }
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, icon::CARET_DOUBLE_RIGHT, "", o.page + 1 < pages).clicked() {
                    o.page += 1;
                }
                ui.label(egui::RichText::new(format!("{} / {pages}", o.page + 1)).font(th::small()).color(c.muted));
                if w::button(ui, icon::CARET_DOUBLE_LEFT, "", o.page > 0).clicked() {
                    o.page -= 1;
                }
            });
        });
        let start = o.page * 16 * ROWS;
        let mut text = String::new();
        for (r, row) in bytes[start.min(bytes.len())..].chunks(16).take(ROWS).enumerate() {
            let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            let asc: String = row.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
            text.push_str(&format!("{:08x}  {:<48} {asc}\n", start + r * 16, hex.join(" ")));
        }
        ui.label(egui::RichText::new(text).font(th::mono(11.5)).color(c.text_2));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_gdk::areas::xgamesave::file;
    use rebuild_saves::format::{build, DATA_BLOB, HEADER_BLOB};

    /// The screen's actions end to end on a temporary saves folder: open, rename, patch, refused while the
    /// title runs, write (backed up), restore.
    #[test]
    fn edit_write_and_undo_through_the_screen() {
        let base = std::env::temp_dir().join(format!("rebuild-launcher-saves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("GameSave");
        let dir = root.join("0009000000000001").join("default");
        std::fs::create_dir_all(&dir).unwrap();
        let s = build(5, "Profile Settings", 7, "", &[0; 32]).unwrap();
        let c = Stored { name: "SettingsProfileSettings".into(), display: "Profile Settings".into(), modified: 1, blobs: vec![(HEADER_BLOB.into(), s.header.to_bytes()), (DATA_BLOB.into(), s.data())] };
        let path = file::path(&dir, &c.name);
        file::write(&path, &c).unwrap();
        let before = std::fs::read(&path).unwrap();
        let idle = || false;
        let busy = || true;
        let mut st = SavesUi { root: Some(root.clone()), ..SavesUi::default() };
        run(&mut st, Act::Select(path.clone()), &root, &idle).unwrap();
        run(&mut st, Act::Rename("Mine".into()), &root, &idle).unwrap();
        run(&mut st, Act::Patch("0x4".into(), "ab cd".into()), &root, &idle).unwrap();
        assert!(run(&mut st, Act::Patch("31".into(), "abcd".into()), &root, &idle).is_err());
        assert!(run(&mut st, Act::Write, &root, &busy).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        run(&mut st, Act::Write, &root, &idle).unwrap();
        let o = st.open.as_ref().unwrap();
        let saved = o.save.clone().unwrap().unwrap();
        assert_eq!(saved.header.name, "Mine");
        assert_eq!(saved.body[4..6], [0xab, 0xcd]);
        assert!(saved.problems().is_empty());
        let backups = store::backups(&root, &path);
        assert_eq!(backups.len(), 1);
        run(&mut st, Act::Restore(backups[0].clone()), &root, &idle).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&base);
    }
}
