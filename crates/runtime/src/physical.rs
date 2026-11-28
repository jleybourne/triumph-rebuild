//! The console's physical pages: 64 KiB frames the title allocates, then maps (and re-maps, even at
//! two addresses at once) into address ranges it reserved.
//!
//! On the host a frame is 64 KiB of a pagefile section, and a mapping is a view of it placed into a
//! placeholder reservation. Contents follow the frame, not the address, and two views of one frame are
//! the same bytes (console aliasing). The GPU reads these views **in place**: a view opens as a D3D12
//! heap (measured: `gputest --labels`), so physical-page memory needs no write-watch and no upload.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::*;
use windows::Win32::System::Threading::GetCurrentProcess;

pub const FRAME: usize = 64 << 10;

/// The first 64 KiB frame handed to the title (0 reads as "no page"; the fork numbers from here too).
pub const FIRST_PAGE: usize = 0x10_0000;

/// Page numbers as the console gives them: **4 KiB units**, so a 64 KiB page is a multiple of 16. The title keeps
/// them shifted right by 4 in a packed 21-bit table and shifts them back when it builds a scatter list; with
/// consecutive numbers every 16 pages collapsed onto one, and its 420 MB shader arena became one page (the
/// null-program crash on frame 2). Numbers stay below 2^25 so `>> 4` fits 21 bits.
pub const PAGE_SHIFT: u32 = 4;

/// The page number the title sees for frame-table index `i`.
fn number(i: usize) -> usize {
    (i + FIRST_PAGE) << PAGE_SHIFT
}

/// The frame-table index of a page number the title passes (`None`: not one of ours).
fn index(frame: usize) -> Option<usize> {
    if frame & ((1 << PAGE_SHIFT) - 1) != 0 {
        return None;
    }
    (frame >> PAGE_SHIFT).checked_sub(FIRST_PAGE)
}

struct Section {
    handle: HANDLE,
    frames: usize,
    /// frames of it not yet freed; at 0 the section is closed and its bytes go back to the budget
    live: usize,
    /// The whole section mapped once, on first need, for writes that name frames rather than
    /// addresses (DirectStorage page destinations); 0 until then. Same bytes as every placed view.
    view: usize,
    /// a number for this view, unique in the process ([`crate::titlemem`] opens the view once, by it)
    view_id: u64,
}

/// Numbers for section views ([`Section::view_id`]).
static VIEW_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Frames and where they are mapped.
#[derive(Default)]
pub struct Physical {
    sections: Vec<Section>,
    /// frame number -> (section index, frame index in it); `None` once freed
    frames: Vec<Option<(usize, usize)>>,
    /// address -> frame mapped there
    mapped: BTreeMap<u64, usize>,
}

unsafe impl Send for Physical {}

impl Drop for Physical {
    fn drop(&mut self) {
        let addrs: Vec<u64> = self.mapped.keys().copied().collect();
        for a in addrs {
            self.unmap(a);
        }
        for s in &self.sections {
            unsafe {
                if s.view != 0 {
                    crate::titlemem::section_gone(s.view_id);
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: s.view as *mut c_void });
                    crate::titlemem::changed(s.view as u64, (s.view + s.frames * FRAME) as u64);
                }
                if !s.handle.is_invalid() && s.live > 0 {
                    let _ = CloseHandle(s.handle);
                    crate::budget::refund((s.frames * FRAME) as u64);
                }
            }
        }
    }
}

/// Reserve `size` bytes as a placeholder the frames can be mapped into (the title's 64 KiB-page
/// reservation). Returns the base, or 0.
pub fn reserve_placeholder(size: usize) -> u64 {
    unsafe {
        VirtualAlloc2(GetCurrentProcess(), None, size, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS.0, None) as u64
    }
}

impl Physical {
    /// Allocate `n` frames; returns their numbers.
    pub fn allocate(&mut self, n: usize) -> Option<Vec<usize>> {
        let bytes = n.checked_mul(FRAME)?;
        if n == 0 || !crate::budget::charge(bytes as u64) {
            // past the title's budget: refused as the console refuses it, before the host commits anything
            return None;
        }
        let handle = match unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, (bytes >> 32) as u32, bytes as u32, None)
        } {
            Ok(h) => h,
            Err(_) => {
                crate::budget::refund(bytes as u64);
                return None;
            }
        };
        let si = self.sections.len();
        self.sections.push(Section { handle, frames: n, live: n, view: 0, view_id: 0 });
        let first = self.frames.len();
        self.frames.extend((0..n).map(|k| Some((si, k))));
        Some((first..first + n).map(number).collect())
    }

    /// Free frames (their mappings must be gone first).
    pub fn free(&mut self, frames: &[usize]) -> bool {
        if frames.iter().any(|f| self.mapped.values().any(|m| m == f)) {
            return false;
        }
        if frames.iter().any(|&f| !matches!(index(f).and_then(|i| self.frames.get(i)), Some(Some(_)))) {
            return false;
        }
        for &f in frames {
            let Some(i) = index(f) else { continue };
            let Some((si, _)) = self.frames[i].take() else { continue };
            let s = &mut self.sections[si];
            s.live -= 1;
            if s.live == 0 {
                // the whole section is free: close it, so the host memory really goes back
                unsafe {
                    if s.view != 0 {
                        // the GPU's heap over the view goes first (crate::titlemem)
                        crate::titlemem::section_gone(s.view_id);
                        let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: s.view as *mut c_void });
                        crate::titlemem::changed(s.view as u64, (s.view + s.frames * FRAME) as u64);
                        s.view = 0;
                    }
                    let _ = CloseHandle(s.handle);
                }
                s.handle = HANDLE::default();
                crate::budget::refund((s.frames * FRAME) as u64);
            }
        }
        true
    }

    /// Map frame `frame` at `addr` (64 KiB aligned, inside a placeholder reservation). Whatever was
    /// mapped there is unmapped first.
    pub fn map(&mut self, addr: u64, frame: usize) -> bool {
        let Some(Some((si, k))) = index(frame).and_then(|i| self.frames.get(i)).copied() else { return false };
        if addr % FRAME as u64 != 0 || k >= self.sections[si].frames {
            return false;
        }
        if self.mapped.contains_key(&addr) {
            self.unmap(addr);
        }
        unsafe {
            // split the placeholder so exactly this 64 KiB can be replaced (fails harmlessly when the
            // range is already a 64 KiB placeholder)
            let _ = VirtualFree(addr as *mut c_void, FRAME, VIRTUAL_FREE_TYPE(MEM_RELEASE.0 | MEM_PRESERVE_PLACEHOLDER.0));
            let v = MapViewOfFile3(
                self.sections[si].handle,
                GetCurrentProcess(),
                Some(addr as *const c_void),
                (k * FRAME) as u64,
                FRAME,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE.0,
                None,
            );
            // a new allocation over the 64 KiB (crate::titlemem: nothing opened may cover it from before)
            crate::titlemem::changed(addr, addr + FRAME as u64);
            if v.Value.is_null() {
                return false;
            }
        }
        self.mapped.insert(addr, frame);
        // the GPU reaches the frame through its section's one view, opened once (crate::titlemem), instead of
        // opening every mapping of it
        if crate::titlemem::sections_on() {
            if let Some((view, id)) = self.section_view(si) {
                crate::titlemem::frame_mapped(addr, id, view as u64, (self.sections[si].frames * FRAME) as u64, (k * FRAME) as u64);
            }
        }
        true
    }

    /// Unmap whatever frame is at `addr`, leaving a placeholder for the next map.
    pub fn unmap(&mut self, addr: u64) -> bool {
        if self.mapped.remove(&addr).is_none() {
            return false;
        }
        crate::titlemem::frame_unmapped(addr);
        let ok = unsafe { UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: addr as *mut c_void }, MEM_PRESERVE_PLACEHOLDER).is_ok() };
        // the GPU's allocation opened over this frame (crate::titlemem) is gone
        crate::titlemem::changed(addr, addr + FRAME as u64);
        ok
    }

    /// The 64 KiB of a live frame, wherever (or whether) it is mapped. The pointer stays valid while
    /// this `Physical` lives: sections are only closed when it drops.
    pub fn frame_ptr(&mut self, frame: usize) -> Option<*mut u8> {
        let (si, k) = (*self.frames.get(index(frame)?)?)?;
        let (view, _) = self.section_view(si)?;
        Some((view + k * FRAME) as *mut u8)
    }

    /// Section `si` mapped whole (once), and its view's number.
    fn section_view(&mut self, si: usize) -> Option<(usize, u64)> {
        let s = &mut self.sections[si];
        if s.view == 0 {
            let v = unsafe { MapViewOfFile(s.handle, FILE_MAP_ALL_ACCESS, 0, 0, s.frames * FRAME) };
            if v.Value.is_null() {
                return None;
            }
            s.view = v.Value as usize;
            s.view_id = VIEW_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Some((s.view, s.view_id))
    }

    /// Every address frame `frame` is mapped at now (a diagnostic scan).
    pub fn mapped_at(&self, frame: usize) -> Vec<u64> {
        self.mapped.iter().filter(|(_, &f)| f == frame).map(|(&a, _)| a).collect()
    }

    pub fn frame_at(&self, addr: u64) -> Option<usize> {
        self.mapped.get(&(addr & !(FRAME as u64 - 1))).copied()
    }

    pub fn live_frames(&self) -> usize {
        self.frames.iter().filter(|f| f.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn put(a: u64, v: u32) {
        (a as *mut u32).write_volatile(v)
    }
    unsafe fn get(a: u64) -> u32 {
        (a as *const u32).read_volatile()
    }

    #[test]
    fn frames_keep_their_contents_across_remaps_and_aliases_share_bytes() {
        let mut p = Physical::default();
        let frames = p.allocate(4).unwrap();
        let base = reserve_placeholder(16 * FRAME);
        assert_ne!(base, 0);
        let at = |i: u64| base + i * FRAME as u64;
        for (i, &f) in frames.iter().enumerate() {
            assert!(p.map(at(i as u64), f), "map frame {f}");
        }
        unsafe {
            put(at(0) + 8, 0xF00D);
            put(at(3) + 4096, 0xBEEF);
        }
        // an alias: frame 0 at a second address sees the same bytes, and writes through either show in both
        assert!(p.map(at(8), frames[0]));
        unsafe {
            assert_eq!(get(at(8) + 8), 0xF00D);
            put(at(8) + 12, 0xCAFE);
            assert_eq!(get(at(0) + 12), 0xCAFE);
        }
        // remap frame 3 elsewhere: its contents follow the frame
        assert!(p.unmap(at(3)));
        assert!(p.map(at(12), frames[3]));
        unsafe { assert_eq!(get(at(12) + 4096), 0xBEEF) };
        assert_eq!(p.frame_at(at(12) + 77), Some(frames[3]));
        // writes by frame number (DirectStorage page destinations) land in the placed views, and back
        let w = p.frame_ptr(frames[3]).unwrap();
        unsafe {
            assert_eq!((w.add(4096) as *const u32).read_volatile(), 0xBEEF);
            (w.add(100) as *mut u32).write_volatile(0x1234_5678);
            assert_eq!(get(at(12) + 100), 0x1234_5678);
        }
        assert!(p.frame_ptr(999).is_none(), "an unknown frame has no view");
        // a mapped frame cannot be freed; unmapped, it can
        assert!(!p.free(&frames[..1]));
        for i in [0, 1, 2, 8, 12] {
            p.unmap(at(i));
        }
        assert!(p.free(&frames));
        assert_eq!(p.live_frames(), 0);
    }

    #[test]
    fn page_numbers_are_4k_units_and_survive_the_titles_packing() {
        let mut p = Physical::default();
        let frames = p.allocate(40).unwrap();
        // what the title does: keep `n >> 4` in a 21-bit field, rebuild `(field << 4) & 0x1FFFFF0`
        let rebuilt: Vec<usize> = frames.iter().map(|&n| ((n >> 4) << 4) & 0x1FF_FFF0).collect();
        assert_eq!(rebuilt, frames, "every page survives the round trip");
        assert!(frames.iter().all(|n| n % 16 == 0 && *n != 0));
        let distinct: std::collections::HashSet<_> = rebuilt.iter().collect();
        assert_eq!(distinct.len(), 40, "no two pages collapse onto one");
        assert!(p.free(&frames));
    }
}
