//! The launcher's save editor, without its window (`docs/design/save-editor.md`; the format is in
//! `docs/research/save-format.md`).
//!
//! - [`format`]: the title's "Save Game Header" / "Save Game Data" blobs: strict parse, serialize, the
//!   title's own load checks, reseal after an edit
//! - [`section`]: one decoded save as JSON and back (schema-checked, checksums recomputed)
//! - [`store`]: containers on disk; every write backed up first, refused while the title runs, made
//!   through the runtime's all-or-nothing path; container import and export with a manifest; every save
//!   of a game at once
//! - [`body`]: the bodies of the two kinds real saves use: the player profile (id, value pairs) and
//!   `SettingsProfileSettings` (JSON with hashed member names), edited in place
//! - [`triumph`]: import from the owner's older build's save folders (read only)
//! - [`json`]: the small strict JSON the exports use
//!
//! Offline single-player saves only: nothing here talks to a network, and nothing named like online
//! state is ever written.

pub mod body;
pub mod format;
pub mod json;
pub mod section;
pub mod store;
pub mod triumph;

/// Writes a new file (refusing to replace one), flushed.
pub(crate) fn write_new(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(bytes).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", path.display()))
}
