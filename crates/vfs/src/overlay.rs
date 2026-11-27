//! The write overlay of one drive: every create, write, rename or delete lands here.
//!
//! On disk: `<overlay>/<LETTER>/files/..` holds the files the title wrote, in the case it first
//! wrote them; `<overlay>/<LETTER>/whiteouts.txt` lists (one folded key a line) the paths deleted
//! from the layers below, which stay hidden. The whole overlay is indexed in memory at start
//! (it holds only what the title wrote) and kept current by every mutation.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::path::{ancestors, child_key, fold};

#[derive(Debug, Clone)]
pub(crate) struct ONode {
    pub host: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

pub(crate) struct Overlay {
    files_root: PathBuf,
    whiteout_file: PathBuf,
    entries: HashMap<String, ONode>,
    /// Parent key → folded child names.
    children: HashMap<String, HashSet<String>>,
    whiteouts: BTreeSet<String>,
}

impl Overlay {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let files_root = dir.join("files");
        fs::create_dir_all(&files_root)?;
        let whiteout_file = dir.join("whiteouts.txt");
        let whiteouts = match fs::read_to_string(&whiteout_file) {
            Ok(text) => text.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(e),
        };
        let mut overlay =
            Overlay { files_root, whiteout_file, entries: HashMap::new(), children: HashMap::new(), whiteouts };
        let root = overlay.files_root.clone();
        overlay.index(&root, "")?;
        Ok(overlay)
    }

    pub(crate) fn index(&mut self, host: &Path, key: &str) -> io::Result<()> {
        for entry in fs::read_dir(host)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            let is_dir = entry.file_type()?.is_dir();
            let child = child_key(key, &fold(&name));
            self.insert(&child, ONode { host: entry.path(), name, is_dir });
            if is_dir {
                self.index(&entry.path(), &child)?;
            }
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&ONode> {
        self.entries.get(key)
    }

    /// Host path of the directory at `key` in the overlay (`""` is the root).
    pub fn dir_host(&self, key: &str) -> Option<PathBuf> {
        if key.is_empty() {
            return Some(self.files_root.clone());
        }
        self.entries.get(key).filter(|n| n.is_dir).map(|n| n.host.clone())
    }

    pub fn insert(&mut self, key: &str, node: ONode) {
        let (parent, name) = split(key);
        self.children.entry(parent.to_owned()).or_default().insert(name.to_owned());
        self.entries.insert(key.to_owned(), node);
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
        self.children.remove(key);
        let (parent, name) = split(key);
        if let Some(set) = self.children.get_mut(parent) {
            set.remove(name);
        }
    }

    /// Removes `key` and everything indexed below it.
    pub fn remove_tree(&mut self, key: &str) {
        let below = format!("{key}/");
        self.entries.retain(|k, _| !k.starts_with(&below));
        self.children.retain(|k, _| k != key && !k.starts_with(&below));
        self.remove(key);
    }

    /// Children of the directory at `key`, as (folded name, node).
    pub fn children(&self, key: &str) -> Vec<(String, ONode)> {
        let Some(set) = self.children.get(key) else { return Vec::new() };
        set.iter().filter_map(|n| Some((n.clone(), self.entries.get(&child_key(key, n))?.clone()))).collect()
    }

    /// Hidden: the path, or a directory above it, was deleted from the layers below.
    pub fn hidden(&self, key: &str) -> bool {
        !self.whiteouts.is_empty()
            && (self.whiteouts.contains(key) || ancestors(key).any(|a| self.whiteouts.contains(a)))
    }

    pub fn whiteout(&mut self, key: &str) -> io::Result<()> {
        if self.whiteouts.insert(key.to_owned()) {
            self.save_whiteouts()?;
        }
        Ok(())
    }

    pub fn unwhiteout(&mut self, key: &str) -> io::Result<()> {
        if self.whiteouts.remove(key) {
            self.save_whiteouts()?;
        }
        Ok(())
    }

    fn save_whiteouts(&self) -> io::Result<()> {
        let mut text = String::new();
        for w in &self.whiteouts {
            text.push_str(w);
            text.push('\n');
        }
        let tmp = self.whiteout_file.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.whiteout_file)
    }
}

fn split(key: &str) -> (&str, &str) {
    match key.rfind('/') {
        Some(i) => (&key[..i], &key[i + 1..]),
        None => ("", key),
    }
}
