//! The title's drives, and the pieces that read them: one overlay file system (`rebuild_vfs`) shared by
//! the file overrides (`rebuild_winfiles`) and DirectStorage (`rebuild_dstorage`), so a file written
//! through one is seen by the other.
//!
//! ```text
//! G:   the game package: the title's folder, read-only, with enabled mods layered on it
//! T:   temporary files: overlay only, emptied at every start (the console's behaviour)
//! S:   persistent local storage: overlay only, kept between runs (the GDK reports this root)
//! ```
//!
//! Where they live is `rebuild_storage`'s (`docs/design/storage.md`): the overlay (G: writes, T:) in the game's
//! `overlay\`, S: in its `storage\`, and the user's saves (XGameSave) in its `saves\`.
//!
//! **D: is not mounted, on purpose.** On the console it is the devkit's developer scratch partition ("for use
//! during development only", the GDK's "Local storage on XBOX console" page); a retail console has none. The title
//! only probes `D:\decorations\presets`, `\presetgroups` and `/compressed_textures` for optional designer files
//! (decoration manager, exe 0x145bb7090: each find that fails skips to cleanup, nothing is ever written there) and
//! goes on with the package's own content, as on retail. A refused find is that behaviour.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rebuild_vfs::{ModSet, Vfs, VfsBuilder};

use crate::physical::FRAME;

/// The drive the title's persistent local storage lives on (its root is what the GDK hands back).
pub const STORAGE: char = 'S';

/// Where the drives are, for one run.
pub struct Drives {
    pub game: PathBuf,
    /// the write overlay: G: writes and T:
    pub overlay: PathBuf,
    /// S:, the title's persistent local storage
    pub storage: PathBuf,
    /// the user's game saves (XGameSave): `<saves>\<user>\<scid>\`
    pub saves: PathBuf,
    pub mods: ModSet,
    /// the BCPack format tables (`dstorage.bcpack_tables`); `None` = `REBUILD_BCPACK_TABLES` or the default place
    pub bcpack_tables: Option<PathBuf>,
    /// the package's title id (`MicrosoftGame.config` `<TitleId>`): `XGameGetXboxTitleId`, and through it the scid
    /// the title opens its saves with; `None` = 0
    pub title_id: Option<u32>,
}

impl Drives {
    /// The game folder, its folders in the data root (`rebuild_storage::game_for`), and no mods.
    pub fn new(game: impl Into<PathBuf>) -> Drives {
        let game = game.into();
        let store = rebuild_storage::game_for(&game);
        Drives::in_store(game, &store)
    }

    /// The game folder over a given game's folders.
    pub fn in_store(game: PathBuf, store: &rebuild_storage::GameStore) -> Drives {
        let title_id = rebuild_storage::identify(&game).title_id;
        Drives { game, overlay: store.overlay_dir(), storage: store.storage_dir(), saves: store.saves_root(), mods: ModSet::default(), bcpack_tables: None, title_id }
    }
}

/// The service configuration id a title opens its saves with when it has no configured one: its title id in the
/// last 8 hex digits (the title builds it from `XGameGetXboxTitleId`; with a title id of 0 it asked for
/// [`ZERO_SCID`], and the older Triumph build's saves sit under `…-00007952987a` for title id 0x7952987a).
pub fn scid_for(title_id: u32) -> String {
    format!("00000000-0000-0000-0000-0000{title_id:08x}")
}

/// The scid the title asked for while the title id was unset (0).
pub const ZERO_SCID: &str = "00000000-0000-0000-0000-000000000000";

/// Saves written while the title id was unset move to the real scid, per user folder (`<saves>\<user>\<scid>`):
/// when only `<user>\`[`ZERO_SCID`] exists it is renamed to `<user>\<scid_for(title_id)>`; when both exist both
/// are kept and the conflict is reported. Returns one line per user folder it acted on or skipped.
pub fn adopt_zero_scid(saves: &Path, title_id: u32) -> Vec<String> {
    let mut out = Vec::new();
    if title_id == 0 {
        return out;
    }
    let real = scid_for(title_id);
    let Ok(users) = std::fs::read_dir(saves) else { return out };
    for user in users.flatten().filter(|e| e.path().is_dir()) {
        let zero = user.path().join(ZERO_SCID);
        if !zero.is_dir() {
            continue;
        }
        let to = user.path().join(&real);
        let name = user.file_name().to_string_lossy().into_owned();
        if to.exists() {
            out.push(format!("user {name}: both {ZERO_SCID} and {real} hold saves; both kept, the title reads {real}"));
            continue;
        }
        match std::fs::rename(&zero, &to) {
            Ok(()) => out.push(format!("user {name}: saves moved from {ZERO_SCID} to {real} (title id {title_id:#010x})")),
            Err(e) => out.push(format!("user {name}: could not move {ZERO_SCID} to {real}: {e}; both kept")),
        }
    }
    out
}

/// Builds the drives and installs the file overrides and DirectStorage over them. Returns the vfs.
pub fn install(d: &Drives) -> Result<Arc<Vfs>, String> {
    let temp = d.overlay.join("T");
    if temp.exists() {
        std::fs::remove_dir_all(&temp).map_err(|e| format!("emptying {}: {e}", temp.display()))?;
    }
    let vfs = VfsBuilder::new(&d.overlay)
        .package('G', &d.game, &d.mods)
        .writable('T')
        .writable_at(STORAGE, &d.storage)
        .build()
        .map_err(|e| format!("drives: {e}"))?;
    let vfs = Arc::new(vfs);
    // the GDK's persistent local storage is the S: drive; its used space is the host folder behind it
    let profile = rebuild_gdk::profile::Profile {
        storage_drive: STORAGE,
        storage_host: Some(d.storage.clone()),
        // the user's game saves (XGameSave): beside the drive, not on it, so they never count against the
        // storage quota or show on S:
        saves_host: Some(d.saves.clone()),
        title_id: d.title_id.unwrap_or(0),
        ..rebuild_gdk::profile::Profile::default()
    };
    // saves written before the title id was set (scid all zeros) follow it to the scid the title now asks for
    for line in adopt_zero_scid(&d.saves, profile.title_id) {
        eprintln!("saves     {line}");
    }
    rebuild_gdk::profile::configure(profile)?;
    rebuild_winfiles::install(rebuild_winfiles::Files::new(vfs.clone(), r"G:\"));
    // BCPack: our decoder (tables from the setting / REBUILD_BCPACK_TABLES, outside git), with the title's own
    // requests captured when REBUILD_BCPACK_CAPTURE names a folder (D5b: checking the decoder on real data)
    let bcpack = rebuild_dstorage::Decoder::from_config(d.bcpack_tables.as_deref())
        .map(|d| rebuild_dstorage::Capture::from_env(d as Arc<dyn rebuild_dstorage::BcPack>));
    let config = rebuild_dstorage::Config { pages: Some(Arc::new(PhysicalPages)), bcpack, ..Default::default() };
    // a second install in one process keeps the first engine (tests install more than once)
    let _ = rebuild_dstorage::install(rebuild_dstorage::Engine::new(vfs.clone(), config));
    Ok(vfs)
}

/// DirectStorage page destinations: the title's 64 KiB physical frames, by number.
pub struct PhysicalPages;

/// The title sometimes names a frame it makes a moment later (the fork lost 12-446 reads a launch without
/// a retry): wait up to about 0.9 s for it, then fail the read.
const RETRIES: u32 = 12;
const RETRY_WAIT: Duration = Duration::from_millis(75);

impl rebuild_dstorage::Pages for PhysicalPages {
    fn write(&self, pages: &[u64], offset: usize, data: &[u8]) -> bool {
        let mut at = offset;
        let mut done = 0;
        while done < data.len() {
            let (page, within) = (at / FRAME, at % FRAME);
            let Some(&frame) = pages.get(page) else { return false };
            let Some(dst) = frame_with_retry(frame as usize) else { return false };
            let n = (FRAME - within).min(data.len() - done);
            // SAFETY: `dst` is the start of a live 64 KiB frame view, and `within + n <= FRAME`.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), dst.add(within), n) };
            done += n;
            at += n;
        }
        true
    }
}

/// Page writes that waited for a frame (one per sleep), and frames never found.
pub static FRAME_WAITS: AtomicU64 = AtomicU64::new(0);
pub static FRAME_MISSES: AtomicU64 = AtomicU64::new(0);

fn frame_with_retry(frame: usize) -> Option<*mut u8> {
    for i in 0..=RETRIES {
        if let Some(p) = crate::exports::frame_ptr(frame) {
            return Some(p);
        }
        if i < RETRIES {
            FRAME_WAITS.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(RETRY_WAIT);
        }
    }
    FRAME_MISSES.fetch_add(1, Ordering::Relaxed);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_dstorage::Pages;

    #[test]
    fn zero_scid_saves_follow_the_title_id_and_a_conflict_keeps_both() {
        assert_eq!(scid_for(0x7952_987a), "00000000-0000-0000-0000-00007952987a");
        assert_eq!(scid_for(0), ZERO_SCID);
        let root = std::env::temp_dir().join(format!("rebuild-scid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = scid_for(0x7952_987a);
        let put = |p: PathBuf, bytes: &[u8]| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        };
        // a: only the zero folder; b: both; c: only the real one
        put(root.join("a").join(ZERO_SCID).join("%50layer.gsc"), b"a-zero");
        put(root.join("b").join(ZERO_SCID).join("%50layer.gsc"), b"b-zero");
        put(root.join("b").join(&real).join("%50layer.gsc"), b"b-real");
        put(root.join("c").join(&real).join("%50layer.gsc"), b"c-real");
        // title id unset: nothing moves
        assert!(adopt_zero_scid(&root, 0).is_empty());
        assert!(root.join("a").join(ZERO_SCID).is_dir());
        let lines = adopt_zero_scid(&root, 0x7952_987a);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("user a: saves moved")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("user b: both")), "{lines:?}");
        assert!(!root.join("a").join(ZERO_SCID).exists());
        assert_eq!(std::fs::read(root.join("a").join(&real).join("%50layer.gsc")).unwrap(), b"a-zero");
        assert_eq!(std::fs::read(root.join("b").join(ZERO_SCID).join("%50layer.gsc")).unwrap(), b"b-zero", "kept");
        assert_eq!(std::fs::read(root.join("b").join(&real).join("%50layer.gsc")).unwrap(), b"b-real", "kept");
        assert_eq!(std::fs::read(root.join("c").join(&real).join("%50layer.gsc")).unwrap(), b"c-real");
        // a second start: nothing left to move, the conflict reported again
        assert_eq!(adopt_zero_scid(&root, 0x7952_987a).len(), 1);
        // no saves folder at all
        assert!(adopt_zero_scid(&root.join("missing"), 0x7952_987a).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn page_writes_span_frames_and_land_in_the_titles_pages() {
        let mut n = 2usize;
        let mut frames = [0usize; 2];
        assert_eq!(crate::exports::XMemAllocatePhysicalPages(0, &mut n, frames.as_mut_ptr()).0, 1);
        let pages: Vec<u64> = frames.iter().map(|&f| f as u64).collect();
        let data: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        // 100 bytes before the end of the first frame: the write crosses into the second
        assert!(PhysicalPages.write(&pages, FRAME - 100, &data));
        let a = crate::exports::frame_ptr(frames[0]).unwrap();
        let b = crate::exports::frame_ptr(frames[1]).unwrap();
        unsafe {
            assert_eq!(std::slice::from_raw_parts(a.add(FRAME - 100), 100), &data[..100]);
            assert_eq!(std::slice::from_raw_parts(b, 100), &data[100..]);
        }
        // beyond the listed pages: refused, not written elsewhere
        assert!(!PhysicalPages.write(&pages, 2 * FRAME - 10, &data));
        assert_eq!(crate::exports::XMemFreePhysicalPages(2, frames.as_ptr()).0, 1);
    }
}
