//! A read-only layer (the game folder, or one mod) with case-insensitive lookup.
//!
//! Each directory is listed once, on first use, into a map from folded name to the name on disk,
//! and cached under its folded key with its host path. A lookup is then one cache hit on the
//! parent's key and one on the leaf's name. The layer is never written, so the cache stays valid
//! for the whole run; files in it are only ever opened for reading.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::path::fold;

#[derive(Debug, Clone)]
pub(crate) struct Node {
    /// The name on disk.
    pub name: String,
    pub is_dir: bool,
    pub len: u64,
}

pub(crate) struct Dir {
    pub host: PathBuf,
    /// Folded name → node.
    pub entries: HashMap<String, Node>,
}

pub(crate) struct ReadLayer {
    pub root: PathBuf,
    /// Folded directory key → its listing (`None`: no directory there in this layer).
    dirs: RwLock<HashMap<String, Option<Arc<Dir>>>>,
}

impl ReadLayer {
    pub fn new(root: PathBuf) -> Self {
        Self { root, dirs: RwLock::new(HashMap::new()) }
    }

    fn cached(&self, key: &str) -> Option<Option<Arc<Dir>>> {
        self.dirs.read().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    fn remember(&self, key: &str, dir: Option<Arc<Dir>>) -> Option<Arc<Dir>> {
        self.dirs.write().unwrap_or_else(|e| e.into_inner()).entry(key.to_owned()).or_insert(dir).clone()
    }

    /// The directory at `parts` (folded `key`), walking and caching from the nearest known level.
    fn dir(&self, parts: &[String], key: &str) -> Option<Arc<Dir>> {
        if let Some(hit) = self.cached(key) {
            return hit;
        }
        let dir = match parts.split_last() {
            None => list_host(&self.root).map(Arc::new),
            Some((last, above)) => {
                let parent_key = key.rfind('/').map_or("", |i| &key[..i]);
                let parent = self.dir(above, parent_key)?;
                match parent.entries.get(&fold(last)) {
                    Some(n) if n.is_dir => list_host(&parent.host.join(&n.name)).map(Arc::new),
                    _ => None,
                }
            }
        };
        self.remember(key, dir)
    }

    /// The host path and node for `parts` (folded `key`), if this layer has it.
    pub fn lookup(&self, parts: &[String], key: &str) -> Option<(PathBuf, bool, u64)> {
        let Some((_, above)) = parts.split_last() else {
            return Some((self.root.clone(), true, 0));
        };
        let (parent_key, leaf) = match key.rfind('/') {
            Some(i) => (&key[..i], &key[i + 1..]),
            None => ("", key),
        };
        let dir = self.dir(above, parent_key)?;
        let n = dir.entries.get(leaf)?;
        Some((dir.host.join(&n.name), n.is_dir, n.len))
    }

    /// The listing of the directory at `parts` (folded `key`), if this layer has one there.
    pub fn list(&self, parts: &[String], key: &str) -> Option<Arc<Dir>> {
        self.dir(parts, key)
    }
}

fn list_host(host: &Path) -> Option<Dir> {
    let mut entries = HashMap::new();
    for entry in fs::read_dir(host).ok()?.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        // On a case-sensitive host two names can fold together; the first listed wins.
        entries.entry(fold(&name)).or_insert(Node { name, is_dir: meta.is_dir(), len: meta.len() });
    }
    Some(Dir { host: host.to_owned(), entries })
}
