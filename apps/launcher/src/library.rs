//! The game library: more than one game, each with its own folder, config, profiles, mods, overlay copy
//! and frame log. Only Xbox Series packages are supported today, and the detector says so from the
//! game's own files at run time; nothing about any title is written in this code.
//!
//! ```text
//! <data>/library.toml                  the list: id, folder, name, detector; which one is selected
//! <data>/games/<id>/settings.toml      that game's settings (game.dir = its folder)
//! <data>/games/<id>/profiles/          its profiles
//! <data>/games/<id>/mods/              its mods
//! <data>/games/<id>/overlay/           its write overlay (the title settings copy)
//! <data>/games/<id>/saves/, storage/   its saves and the title's storage (docs/design/storage.md)
//! ```
//!
//! The id is the game's package identity (`rebuild_storage::identify`), so the same title is in the library
//! once, and a game moved to another drive keeps its settings and saves; a folder without one is a hash
//! of its absolute path. Removing a game removes its library entry and, when asked, its `games/<id>`
//! folder; the game folder itself is only ever read.

use crate::home::Home;
use std::path::{Path, PathBuf};

/// What a detector says about a folder.
#[derive(Debug, Clone, PartialEq)]
pub enum Detection {
    /// supported: the name to show (from the game's own files)
    Supported { name: String, detail: String },
    /// recognised, but the layer cannot run it yet, and why
    Unsupported { name: Option<String>, reason: String },
    /// not this detector's kind of folder
    NotMine,
}

/// One kind of game folder. Future titles or platforms plug in as more detectors.
pub trait Detector {
    fn id(&self) -> &'static str;
    fn detect(&self, dir: &Path) -> Detection;
}

/// A GDK package: `MicrosoftGame.config` in the folder. Supported when it names an executable for
/// Xbox Series (`TargetDeviceFamily="Scarlett"`) that exists in the folder.
pub struct GdkPackage;

fn attr(el: &str, key: &str) -> Option<String> {
    let k = format!(" {key}=\"");
    let i = el.find(&k)? + k.len();
    let j = el[i..].find('"')?;
    Some(el[i..i + j].to_string())
}

impl Detector for GdkPackage {
    fn id(&self) -> &'static str {
        "gdk-package"
    }
    fn detect(&self, dir: &Path) -> Detection {
        let Some(cfg) = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("MicrosoftGame.config"))
        else {
            return Detection::NotMine;
        };
        let Ok(text) = std::fs::read_to_string(cfg.path()) else {
            return Detection::Unsupported { name: None, reason: "MicrosoftGame.config could not be read".into() };
        };
        let name = crate::gamemeta::parse(&text, dir).name;
        let exes: Vec<(String, String)> = text
            .match_indices("<Executable ")
            .filter_map(|(i, _)| {
                let el = &text[i..i + text[i..].find('>')?];
                Some((attr(el, "Name")?, attr(el, "TargetDeviceFamily").unwrap_or_default()))
            })
            .collect();
        if exes.is_empty() {
            return Detection::Unsupported { name, reason: "the package lists no executable".into() };
        }
        let Some((exe, _)) = exes.iter().find(|(_, fam)| fam.eq_ignore_ascii_case("Scarlett")) else {
            let fams: Vec<&str> = exes.iter().map(|(_, f)| if f.is_empty() { "unknown" } else { f.as_str() }).collect();
            return Detection::Unsupported {
                name,
                reason: format!("the package targets {}, not Xbox Series (Scarlett): only Xbox Series titles run today", fams.join(", ")),
            };
        };
        if !exe.contains("..") && dir.join(exe).is_file() {
            Detection::Supported { name: name.unwrap_or_else(|| "Untitled game".into()), detail: "Xbox Series package".into() }
        } else {
            Detection::Unsupported { name, reason: format!("the package's executable {exe} is missing from the folder") }
        }
    }
}

/// Every detector the launcher knows, in order.
pub fn detectors() -> Vec<Box<dyn Detector>> {
    vec![Box::new(GdkPackage)]
}

/// Run the detectors on a folder: the first that recognises it, and its id.
pub fn detect(dir: &Path, detectors: &[Box<dyn Detector>]) -> (Detection, &'static str) {
    if !dir.is_dir() {
        return (Detection::Unsupported { name: None, reason: format!("{} is not a folder", dir.display()) }, "");
    }
    for d in detectors {
        match d.detect(dir) {
            Detection::NotMine => continue,
            other => return (other, d.id()),
        }
    }
    (
        Detection::Unsupported { name: None, reason: "no supported game was found in this folder (expected an Xbox Series package with MicrosoftGame.config)".into() },
        "",
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Game {
    pub id: String,
    pub dir: PathBuf,
    pub name: String,
    pub detector: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Library {
    /// the launcher's own data folder (the user data root)
    pub root: PathBuf,
    pub games: Vec<Game>,
    pub selected: Option<String>,
    /// where each game's session folders go (`<logs>/<id>/`)
    pub logs: PathBuf,
}

/// A stable id for a game folder: its package identity, else a hash of its path (`rebuild_storage::identify`).
pub fn game_id(dir: &Path) -> String {
    rebuild_storage::identify(dir).id
}

impl Library {
    /// An empty library in a data root.
    pub fn empty(st: &rebuild_storage::Storage) -> Library {
        Library { root: st.user.clone(), games: Vec::new(), selected: None, logs: st.logs_dir() }
    }

    /// The library of a data root, its logs where the storage puts them.
    pub fn load_in(st: &rebuild_storage::Storage) -> Result<Library, String> {
        Library::load(&st.user).map(|l| Library { logs: st.logs_dir(), ..l })
    }

    /// The library in the current layout, once: each game under its package identity (folders merged, never
    /// a file lost), `rebuild.toml` renamed `settings.toml`, sessions moved to the logs. One line per change.
    pub fn migrate(&mut self, st: &rebuild_storage::Storage) -> Vec<String> {
        use rebuild_storage::migrate;
        let mut lines = Vec::new();
        let mut changed = false;
        for i in 0..self.games.len() {
            let (old, dir) = (self.games[i].id.clone(), self.games[i].dir.clone());
            if dir.is_dir() {
                let new = game_id(&dir);
                if new != old && !self.games.iter().any(|g| g.id == new) {
                    let r = migrate::rename_game(st, &old, &new);
                    lines.extend(r.lines);
                    lines.extend(r.kept.iter().map(|(p, why)| format!("kept {}: {why}", p.display())));
                    if self.selected.as_deref() == Some(old.as_str()) {
                        self.selected = Some(new.clone());
                    }
                    self.games[i].id = new;
                    changed = true;
                }
            }
            let r = migrate::game_layout(&st.game(&self.games[i].id));
            lines.extend(r.moved.iter().map(|(a, b)| format!("{} -> {}", a.display(), b.display())));
            lines.extend(r.kept.iter().map(|(p, why)| format!("kept {}: {why}", p.display())));
        }
        if changed {
            if let Err(e) = self.save() {
                lines.push(format!("library: {e}"));
            }
        }
        lines
    }

    pub fn file(root: &Path) -> PathBuf {
        root.join("library.toml")
    }

    /// Load the library (a missing file is an empty library).
    pub fn load(root: &Path) -> Result<Library, String> {
        let mut lib = Library { root: root.to_path_buf(), games: Vec::new(), selected: None, logs: root.join("logs") };
        let path = Library::file(root);
        let Ok(text) = std::fs::read_to_string(&path) else { return Ok(lib) };
        let doc: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
        lib.selected = doc.get("selected").and_then(|v| v.as_str()).map(String::from);
        for g in doc.get("games").and_then(|v| v.as_array()).into_iter().flatten() {
            let s = |k: &str| g.get(k).and_then(|v| v.as_str()).map(String::from);
            if let (Some(id), Some(dir)) = (s("id"), s("dir")) {
                lib.games.push(Game { id, dir: dir.into(), name: s("name").unwrap_or_default(), detector: s("detector").unwrap_or_default() });
            }
        }
        if lib.selected.as_ref().is_some_and(|s| !lib.games.iter().any(|g| &g.id == s)) {
            lib.selected = lib.games.first().map(|g| g.id.clone());
        }
        Ok(lib)
    }

    pub fn save(&self) -> Result<(), String> {
        let mut doc = toml::Table::new();
        if let Some(s) = &self.selected {
            doc.insert("selected".into(), toml::Value::String(s.clone()));
        }
        let games: Vec<toml::Value> = self
            .games
            .iter()
            .map(|g| {
                let mut t = toml::Table::new();
                t.insert("id".into(), toml::Value::String(g.id.clone()));
                t.insert("dir".into(), toml::Value::String(g.dir.display().to_string()));
                t.insert("name".into(), toml::Value::String(g.name.clone()));
                t.insert("detector".into(), toml::Value::String(g.detector.clone()));
                toml::Value::Table(t)
            })
            .collect();
        doc.insert("games".into(), toml::Value::Array(games));
        let text = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        crate::write_atomic(&Library::file(&self.root), &text).map_err(|e| e.to_string())
    }

    /// A game's own data folder, as a `Home`.
    pub fn home(&self, id: &str) -> Home {
        Home { dir: self.root.join("games").join(id), sessions: self.logs.join(id) }
    }

    pub fn selected_game(&self) -> Option<&Game> {
        self.selected.as_ref().and_then(|s| self.games.iter().find(|g| &g.id == s))
    }

    /// Settings left from an earlier time this folder was in the library (kept on remove by default).
    pub fn has_previous(&self, dir: &Path) -> bool {
        self.home(&game_id(dir)).rebuild_toml().is_file()
    }

    /// Add a supported game: its entry, and its `rebuild.toml` with `game.dir` set. Selects it.
    ///
    /// If settings from an earlier time are there, `restore` keeps them (every value, unknown keys too);
    /// otherwise they are moved aside to `games/<id>.previous-<unix seconds>`, never deleted.
    pub fn add(&mut self, dir: &Path, detectors: &[Box<dyn Detector>], restore: bool) -> Result<String, String> {
        let id = game_id(dir);
        if let Some(g) = self.games.iter().find(|g| g.id == id) {
            return Err(if g.dir == dir { "this folder is already in the library".into() } else { format!("this game is already in the library (from {}): remove it first to use this folder", g.dir.display()) });
        }
        let (det, by) = detect(dir, detectors);
        let name = match det {
            Detection::Supported { name, .. } => name,
            Detection::Unsupported { reason, .. } => return Err(format!("not supported yet: {reason}")),
            Detection::NotMine => unreachable!(),
        };
        let home = self.home(&id);
        if home.dir.exists() && !restore {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let aside = self.root.join("games").join(format!("{id}.previous-{secs}"));
            std::fs::rename(&home.dir, &aside).map_err(|e| format!("{}: {e}", home.dir.display()))?;
        }
        let mut st = crate::store::Store::load(&home.rebuild_toml())?;
        let g = rebuild_config::find("game.dir").expect("game.dir is in the registry");
        st.model.apply(g, crate::model::Edit::Text(dir.display().to_string()))?;
        st.save(&home.rebuild_toml())?;
        self.games.push(Game { id: id.clone(), dir: dir.to_path_buf(), name, detector: by.into() });
        self.selected = Some(id.clone());
        self.save()?;
        Ok(id)
    }

    /// Remove a game's entry; with `delete_data`, also the launcher's own `games/<id>` folder. The game
    /// folder is never touched.
    pub fn remove(&mut self, id: &str, delete_data: bool) -> Result<(), String> {
        let i = self.games.iter().position(|g| g.id == id).ok_or("no such game in the library")?;
        let game = self.games.remove(i);
        if self.selected.as_deref() == Some(id) {
            self.selected = self.games.first().map(|g| g.id.clone());
        }
        self.save()?;
        if delete_data {
            let data = self.home(id).dir;
            let games_dir = self.root.join("games");
            // only ever a direct child of <root>/games, and never the game folder or anything above it
            let safe = data.parent() == Some(games_dir.as_path())
                && rebuild_storage::is_game_id(id)
                && !std::path::absolute(&game.dir).unwrap_or(game.dir.clone()).starts_with(std::path::absolute(&data).unwrap_or(data.clone()));
            if !safe {
                return Err("refused: the data folder is not the launcher's own".into());
            }
            if data.exists() {
                std::fs::remove_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
            }
        }
        Ok(())
    }

    pub fn select(&mut self, id: &str) -> Result<(), String> {
        if !self.games.iter().any(|g| g.id == id) {
            return Err("no such game in the library".into());
        }
        self.selected = Some(id.into());
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic package folder (neutral placeholder names only).
    fn package(root: &Path, name: &str, family: &str, with_exe: bool) -> PathBuf {
        let d = root.join(name.replace(' ', "-"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("MicrosoftGame.config"),
            format!(r#"<Game><Executable Name="placeholder.exe" TargetDeviceFamily="{family}" /><ShellVisuals DefaultDisplayName="{name}" /></Game>"#),
        )
        .unwrap();
        if with_exe {
            std::fs::write(d.join("placeholder.exe"), b"MZ").unwrap();
        }
        d
    }

    /// A hash of every file name and byte under a folder.
    fn tree_hash(dir: &Path) -> u64 {
        let mut items = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    // size and mtime from fs::metadata (read_dir entry times are updated lazily on NTFS)
                    let m = std::fs::metadata(&p).unwrap();
                    let t = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
                    items.push((p.display().to_string(), crate::hash_file(&p).unwrap() ^ m.len().rotate_left(17) ^ t.rotate_left(31)));
                }
            }
        }
        items.sort();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for (n, x) in items {
            for b in n.bytes().chain(x.to_le_bytes()) {
                h ^= b as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        h
    }

    #[test]
    fn add_switch_and_persist_across_a_restart() {
        let root = crate::temp_dir("lib");
        let a = package(&root, "Your title", "Scarlett", true);
        let b = package(&root, "Another title", "Scarlett", true);
        let det = detectors();
        let mut lib = Library::load(&root.join("app")).unwrap();
        assert!(lib.games.is_empty());
        let ia = lib.add(&a, &det, true).unwrap();
        let ib = lib.add(&b, &det, true).unwrap();
        assert_ne!(ia, ib);
        assert_eq!(lib.selected.as_deref(), Some(ib.as_str()), "a new game is selected");
        assert!(lib.add(&a, &det, true).is_err(), "the same folder twice");

        // each game has its own config, pointing at its own folder
        assert_eq!(lib.home(&ia).load().unwrap().get("game.dir").to_string(), a.display().to_string());
        assert_eq!(lib.home(&ib).load().unwrap().get("game.dir").to_string(), b.display().to_string());
        lib.select(&ia).unwrap();
        assert_eq!(lib.selected_game().unwrap().name, "Your title");

        // a game update (its files change) keeps its settings: the id is the folder, not its contents
        let mut st = crate::store::Store::load(&lib.home(&ia).rebuild_toml()).unwrap();
        st.model.apply(rebuild_config::find("display.width").unwrap(), crate::model::Edit::Slide(2560)).unwrap();
        st.save(&lib.home(&ia).rebuild_toml()).unwrap();
        let b_before = std::fs::read(lib.home(&ib).rebuild_toml()).unwrap();
        std::fs::write(a.join("placeholder.exe"), b"MZ updated").unwrap();
        assert_eq!(game_id(&a), ia);

        // restart
        let back = Library::load(&root.join("app")).unwrap();
        assert_eq!(crate::store::Store::load(&back.home(&ia).rebuild_toml()).unwrap().model.config.int("display.width"), 2560);
        // per-game isolation: changing game A left game B byte-identical
        assert_eq!(std::fs::read(back.home(&ib).rebuild_toml()).unwrap(), b_before);
        assert_eq!(back, lib);
        assert_eq!(back.selected_game().unwrap().id, ia);
        assert!(lib.select("0000000000000000").is_err());
    }

    #[test]
    fn unsupported_folders_say_why_and_are_not_added() {
        let root = crate::temp_dir("lib-unsupported");
        let det = detectors();
        let mut lib = Library::load(&root.join("app")).unwrap();
        let wrong_family = package(&root, "Older title", "XboxOne", true);
        let no_exe = package(&root, "Broken title", "Scarlett", false);
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        for (dir, why) in [(&wrong_family, "not Xbox Series"), (&no_exe, "missing"), (&empty, "no supported game"), (&root.join("nope"), "not a folder")] {
            let (d, _) = detect(dir, &det);
            match &d {
                Detection::Unsupported { reason, .. } => assert!(reason.contains(why), "{reason}"),
                other => panic!("{other:?}"),
            }
            let e = lib.add(dir, &det, true).unwrap_err();
            assert!(e.starts_with("not supported yet"), "{e}");
        }
        assert!(lib.games.is_empty());
        assert!(!Library::file(&root.join("app")).exists(), "nothing saved");
        assert_eq!(detect(&wrong_family, &det).0, Detection::Unsupported { name: Some("Older title".into()), reason: detect(&wrong_family, &det).0.reason() });
    }

    #[test]
    fn remove_never_touches_the_game_folder() {
        let root = crate::temp_dir("lib-remove");
        let det = detectors();
        let a = package(&root, "Your title", "Scarlett", true);
        std::fs::create_dir_all(a.join("data")).unwrap();
        std::fs::write(a.join("data").join("asset.bin"), [7u8; 4096]).unwrap();
        let before = tree_hash(&a);
        let mut lib = Library::load(&root.join("app")).unwrap();
        let id = lib.add(&a, &det, true).unwrap();
        // per-game data the launcher wrote
        let home = lib.home(&id);
        std::fs::create_dir_all(home.mods_dir().join("m")).unwrap();
        std::fs::write(home.frame_log(), "fps 30\n").unwrap();
        std::fs::create_dir_all(home.overlay_dir().join("title")).unwrap();
        std::fs::write(home.overlay_dir().join("title").join("settings.xml"), "<x/>").unwrap();

        // remove, keeping the data
        lib.remove(&id, false).unwrap();
        assert!(lib.games.is_empty() && lib.selected.is_none());
        assert!(home.rebuild_toml().exists());
        assert_eq!(tree_hash(&a), before);

        // add again: the old settings are offered back, and restored
        assert!(lib.has_previous(&a));
        let mut st = crate::store::Store::load(&home.rebuild_toml()).unwrap();
        st.model.apply(rebuild_config::find("display.width").unwrap(), crate::model::Edit::Slide(1920)).unwrap();
        st.save(&home.rebuild_toml()).unwrap();
        let id2 = lib.add(&a, &det, true).unwrap();
        assert_eq!(id, id2);
        assert_eq!(crate::store::Store::load(&home.rebuild_toml()).unwrap().model.config.int("display.width"), 1920, "restored");
        // or started fresh: the old ones are moved aside, not deleted
        lib.remove(&id, false).unwrap();
        lib.add(&a, &det, false).unwrap();
        assert_eq!(crate::store::Store::load(&home.rebuild_toml()).unwrap().model.config.int("display.width"), 3840, "fresh");
        let aside: Vec<_> = std::fs::read_dir(root.join("app").join("games")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".previous-")).collect();
        assert_eq!(aside.len(), 1);
        assert!(std::fs::read_to_string(aside[0].path().join("settings.toml")).unwrap().contains("1920"));
        assert_eq!(tree_hash(&a), before);
        lib.remove(&id2, true).unwrap();
        assert!(!home.dir.exists(), "the launcher's own data is gone");
        assert!(a.join("MicrosoftGame.config").exists());
        assert_eq!(tree_hash(&a), before, "the game folder is unchanged");
        assert_eq!(Library::load(&root.join("app")).unwrap().games.len(), 0);
        assert!(lib.remove(&id2, true).is_err());
    }
}

impl Detection {
    /// The reason, for an unsupported folder ("" otherwise).
    pub fn reason(&self) -> String {
        match self {
            Detection::Unsupported { reason, .. } => reason.clone(),
            _ => String::new(),
        }
    }
}
