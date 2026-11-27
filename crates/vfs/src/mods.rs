//! Mods: ordered read-only layers between the write overlay and the game folder.
//!
//! A mod is a folder `mods/<name>/` laid out like the game folder; each file in it replaces the
//! title's file at the same path (case-insensitive). Enabled mods are layered in list order, so a
//! later mod wins. [`ModSet::conflicts`] names every path two or more enabled mods provide, and the
//! one that wins. The launcher's mods tab and the in-game mod list read this.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::path::{child_key, fold};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mod {
    pub name: String,
    pub root: PathBuf,
    pub enabled: bool,
}

/// Mods in load order: later wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModSet {
    pub mods: Vec<Mod>,
}

/// A path more than one enabled mod provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// `/`-separated, in the winning mod's case.
    pub path: String,
    /// Every enabled mod that provides it, in load order (the last one wins).
    pub providers: Vec<String>,
    pub winner: String,
}

impl ModSet {
    /// Every folder under `mods_dir`, enabled, ordered by name. An absent folder is no mods.
    pub fn discover(mods_dir: &Path) -> io::Result<ModSet> {
        let mut mods = Vec::new();
        match fs::read_dir(mods_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        if let Ok(name) = entry.file_name().into_string() {
                            mods.push(Mod { name, root: entry.path(), enabled: true });
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        mods.sort_by(|a, b| fold(&a.name).cmp(&fold(&b.name)));
        Ok(ModSet { mods })
    }

    pub fn position(&self, name: &str) -> Option<usize> {
        self.mods.iter().position(|m| m.name == name)
    }

    /// Returns false if no mod has that name.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        match self.position(name) {
            Some(i) => {
                self.mods[i].enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Moves the named mod to `index` in the load order (clamped). Returns false if absent.
    pub fn move_to(&mut self, name: &str, index: usize) -> bool {
        let Some(i) = self.position(name) else { return false };
        let m = self.mods.remove(i);
        self.mods.insert(index.min(self.mods.len()), m);
        true
    }

    pub fn enabled(&self) -> impl Iterator<Item = &Mod> {
        self.mods.iter().filter(|m| m.enabled)
    }

    /// Every path provided by more than one enabled mod, sorted by folded path.
    pub fn conflicts(&self) -> io::Result<Vec<Conflict>> {
        // folded path → (path in the latest provider's case, providers in load order)
        let mut seen: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
        for m in self.enabled() {
            let mut files = Vec::new();
            walk_files(&m.root, "", "", &mut files)?;
            for (key, path) in files {
                let slot = seen.entry(key).or_insert_with(|| (String::new(), Vec::new()));
                slot.0 = path;
                slot.1.push(m.name.clone());
            }
        }
        Ok(seen
            .into_values()
            .filter(|(_, providers)| providers.len() > 1)
            .map(|(path, providers)| Conflict { path, winner: providers.last().unwrap().clone(), providers })
            .collect())
    }
}

/// Every file under `host`, as (folded key, path as on disk), `/`-separated.
fn walk_files(host: &Path, key: &str, path: &str, out: &mut Vec<(String, String)>) -> io::Result<()> {
    for entry in fs::read_dir(host)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else { continue };
        let k = child_key(key, &fold(&name));
        let p = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
        if entry.file_type()?.is_dir() {
            walk_files(&entry.path(), &k, &p, out)?;
        } else {
            out.push((k, p));
        }
    }
    Ok(())
}
