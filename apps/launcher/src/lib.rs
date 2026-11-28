//! The desktop launcher's logic, kept apart from the UI so every piece is tested without a window
//! (`docs/design/launcher.md`, lane `launcher`, tasks U1-U5).
//!
//! - [`model`]: the settings widgets, generated from `rebuild_config::REGISTRY` (U1)
//! - [`home`]: where `rebuild.toml`, profiles, mods and the overlay live; profiles (U1)
//! - [`syscheck`]: first-run system check, behind a probe trait so it is tested with fakes (U2)
//! - [`title`]: `title.*` written into a copy of the title's `settings.xml` (U3)
//! - [`mods`]: mod folders, order, enable, conflicts (U4)
//! - [`play`]: launching the layer and tailing its frame-line log (U5)
//! - [`library`]: more than one game, each with its own data folder; detectors
//! - [`store`]: per-game settings saved safely: explicit values kept, unknown keys kept, `.bak` recovery
//! - [`gamemeta`]: the title's name and key art from its package, read-only, for the Play hero
//! - [`session`]: a folder per launch, `session.toml`, retention (L6)
//! - [`frameline`]: the frame line and `summary.toml`, read; the matrix CSV (L6)
//! - [`runs`]: the test matrix run plan (L6)
//! - [`verify`]: the game folder read-only check, metadata only (L6)
//! - [`prefs`]: launcher-wide preferences (the theme) (L6)
//! - [`shaders`]: "Preparing shaders": each game's shader library, built from its own files on first Play
//! - [`recommend`]: this PC's specs (DXGI, RAM, cores, display) and the settings recommended for it

pub mod frameline;
pub mod gamemeta;
pub mod home;
pub mod library;
pub mod model;
pub mod mods;
pub mod play;
pub mod prefs;
pub mod recommend;
pub mod runs;
pub mod session;
pub mod shaders;
pub mod store;
pub mod syscheck;
pub mod title;
pub mod verify;

/// FNV-1a 64 over a file's bytes: enough to prove a file was not touched.
pub fn hash_file(path: &std::path::Path) -> std::io::Result<u64> {
    let bytes = std::fs::read(path)?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    Ok(h)
}

/// Write a file through a temporary beside it, so a crash never leaves half a file.
pub fn write_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp-write");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
pub(crate) fn temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "rebuild-launcher-test-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
