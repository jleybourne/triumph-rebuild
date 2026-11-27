//! The title's drives, and the pieces that read them: one overlay file system (`rebuild_vfs`) shared by
//! the file overrides (`rebuild_winfiles`) and DirectStorage (`rebuild_dstorage`), so a file written
//! through one is seen by the other.
//!
//! ```text
//! G:   the game package: the title's folder, read-only, with enabled mods layered on it
//! T:   temporary files: overlay only, emptied at every start (the console's behaviour)
//! S:   persistent local storage: overlay only, kept between runs (the GDK reports this root)
//! ```

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rebuild_vfs::{ModSet, Vfs, VfsBuilder};

use crate::physical::FRAME;

/// The drive the title's persistent local storage lives on (its root is what the GDK hands back).
pub const STORAGE: char = 'S';

/// Where the drives are, for one run.
pub struct Drives {
    pub game: PathBuf,
    pub overlay: PathBuf,
    pub mods: ModSet,
}

impl Drives {
    /// The game folder, the default overlay root, and no mods.
    pub fn new(game: impl Into<PathBuf>) -> Drives {
        Drives { game: game.into(), overlay: rebuild_vfs::default_overlay_root(), mods: ModSet::default() }
    }
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
        .writable(STORAGE)
        .build()
        .map_err(|e| format!("drives: {e}"))?;
    let vfs = Arc::new(vfs);
    rebuild_winfiles::install(rebuild_winfiles::Files::new(vfs.clone(), r"G:\"));
    let config = rebuild_dstorage::Config { pages: Some(Arc::new(PhysicalPages)), ..Default::default() };
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

fn frame_with_retry(frame: usize) -> Option<*mut u8> {
    for i in 0..=RETRIES {
        if let Some(p) = crate::exports::frame_ptr(frame) {
            return Some(p);
        }
        if i < RETRIES {
            std::thread::sleep(RETRY_WAIT);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_dstorage::Pages;

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
