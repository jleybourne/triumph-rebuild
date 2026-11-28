//! The pipelines a session used, kept for the next one's prewarm.
//!
//! ```text
//! file     "RBPSOSES" · version u32 · sessions u32 (how many were saved into it) · entries u32
//!          · BLAKE3 of the rest (32 bytes) · entries
//! entry    age u8 (sessions since the title last asked for it) · length u32 · Desc::encode
//! ```
//!
//! Saving merges: this session's pipelines get age 0, the rest age by one, and a pipeline unused for
//! [`MAX_AGE`] sessions is dropped, so a short session (the menu only) does not forget the world's pipelines.
//! A file that fails any check is ignored (and replaced at the next save), never trusted in part.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rebuild_shader::blake3;

use crate::state::{Desc, Key};

const MAGIC: &[u8; 8] = b"RBPSOSES";
const VERSION: u32 = 1;
const HEADER: usize = 8 + 4 + 4 + 4 + 32;

/// Sessions a pipeline is kept without the title asking for it.
pub const MAX_AGE: u8 = 8;

/// The default folder for the pipeline cache and the session record: the game's per-build `pipelines` cache
/// (`rebuild_storage`, `docs/design/storage.md`; the shared one without a game), or `REBUILD_PSO_CACHE_DIR` when set.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("REBUILD_PSO_CACHE_DIR") {
        return d.into();
    }
    rebuild_storage::cache_dir(rebuild_storage::CacheKind::Pipelines)
}

/// One remembered pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: Key,
    pub desc: Desc,
    pub age: u8,
}

/// The record of earlier sessions.
#[derive(Debug, Default)]
pub struct Record {
    pub entries: Vec<Entry>,
    pub sessions: u32,
}

impl Record {
    /// Reads a record; a missing file is an empty record, a bad one is an empty record and a reason.
    pub fn load(path: &Path) -> (Record, Option<String>) {
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Record::default(), None),
            Err(e) => (Record::default(), Some(format!("{}: {e}", path.display()))),
            Ok(b) => match Record::parse(&b) {
                Ok(r) => (r, None),
                Err(e) => (Record::default(), Some(format!("{}: {e}; ignored", path.display()))),
            },
        }
    }

    fn parse(b: &[u8]) -> Result<Record, String> {
        if b.len() < HEADER || &b[..8] != MAGIC {
            return Err("not a session record".into());
        }
        let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        if u(8) != VERSION {
            return Err(format!("version {} (this build reads {VERSION})", u(8)));
        }
        if blake3::hash(&b[HEADER..]).as_bytes() != &b[20..52] {
            return Err("content hash mismatch".into());
        }
        let (sessions, n) = (u(12), u(16) as usize);
        let mut entries = Vec::with_capacity(n.min(1 << 20));
        let mut at = HEADER;
        for i in 0..n {
            let age = *b.get(at).ok_or("short")?;
            let len = u32::from_le_bytes(b.get(at + 1..at + 5).ok_or("short")?.try_into().unwrap()) as usize;
            let desc = Desc::decode(b.get(at + 5..at + 5 + len).ok_or("short")?).ok_or_else(|| format!("entry {i} unreadable"))?;
            entries.push(Entry { key: Key::of(&desc), desc, age });
            at += 5 + len;
        }
        if at != b.len() {
            return Err("bytes after the last entry".into());
        }
        Ok(Record { entries, sessions })
    }

    /// The record after this session: `used` at age 0, the rest one older, the oldest dropped.
    pub fn merge(&self, used: &[(Key, Desc)]) -> Record {
        let mut out: Vec<Entry> = Vec::with_capacity(self.entries.len() + used.len());
        let mut seen: HashMap<Key, ()> = HashMap::with_capacity(used.len());
        for (k, d) in used {
            if seen.insert(*k, ()).is_none() {
                out.push(Entry { key: *k, desc: d.clone(), age: 0 });
            }
        }
        for e in &self.entries {
            if !seen.contains_key(&e.key) && e.age < MAX_AGE {
                out.push(Entry { age: e.age + 1, ..e.clone() });
            }
        }
        Record { entries: out, sessions: self.sessions + 1 }
    }

    /// Writes the record beside, then renames it over the old one.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut b = vec![0u8; HEADER];
        let mut d = Vec::new();
        for e in &self.entries {
            d.clear();
            e.desc.encode(&mut d);
            b.push(e.age);
            b.extend((d.len() as u32).to_le_bytes());
            b.extend(&d);
        }
        b[..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.sessions.to_le_bytes());
        b[16..20].copy_from_slice(&(self.entries.len() as u32).to_le_bytes());
        let h = blake3::hash(&b[HEADER..]);
        b[20..52].copy_from_slice(h.as_bytes());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &b).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(h: u64) -> (Key, Desc) {
        let d = Desc::Compute { cs: h };
        (Key::of(&d), d)
    }

    #[test]
    fn sessions_merge_age_and_round_trip() {
        let dir = std::env::temp_dir().join(format!("rebuild-pso-session-{}", std::process::id()));
        let path = dir.join("session.bin");
        let _ = std::fs::remove_file(&path);
        let (r, e) = Record::load(&path);
        assert!(r.entries.is_empty() && e.is_none(), "missing file: empty, no complaint");

        let g = crate::state::tests::sample();
        let used = vec![cs(1), (Key::of(&g), g.clone()), cs(1)];
        let r1 = Record::default().merge(&used);
        assert_eq!(r1.entries.len(), 2);
        r1.save(&path).unwrap();
        let (back, e) = Record::load(&path);
        assert!(e.is_none());
        assert_eq!(back.entries, r1.entries);
        assert_eq!(back.sessions, 1);

        // later sessions only use cs 2: the others age, then go
        let mut r = back;
        for s in 1..=MAX_AGE {
            r = r.merge(&[cs(2)]);
            assert_eq!(r.entries.len(), 3, "session {s}");
        }
        let gone = r.merge(&[cs(2)]);
        assert_eq!(gone.entries.iter().map(|e| e.key).collect::<Vec<_>>(), [cs(2).0]);

        // a damaged file is ignored with a reason
        let mut b = std::fs::read(&path).unwrap();
        let n = b.len();
        b[n - 1] ^= 0x40;
        std::fs::write(&path, &b).unwrap();
        let (r, e) = Record::load(&path);
        assert!(r.entries.is_empty());
        assert!(e.unwrap().contains("content hash"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
