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

struct Section {
    handle: HANDLE,
    frames: usize,
}

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
                let _ = CloseHandle(s.handle);
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
        let handle = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, (bytes >> 32) as u32, bytes as u32, None)
        }
        .ok()?;
        let si = self.sections.len();
        self.sections.push(Section { handle, frames: n });
        let first = self.frames.len();
        self.frames.extend((0..n).map(|k| Some((si, k))));
        Some((first..first + n).collect())
    }

    /// Free frames (their mappings must be gone first).
    pub fn free(&mut self, frames: &[usize]) -> bool {
        if frames.iter().any(|f| self.mapped.values().any(|m| m == f)) {
            return false;
        }
        for &f in frames {
            match self.frames.get_mut(f) {
                Some(slot @ Some(_)) => *slot = None,
                _ => return false,
            }
        }
        true
    }

    /// Map frame `frame` at `addr` (64 KiB aligned, inside a placeholder reservation). Whatever was
    /// mapped there is unmapped first.
    pub fn map(&mut self, addr: u64, frame: usize) -> bool {
        let Some(Some((si, k))) = self.frames.get(frame).copied() else { return false };
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
            if v.Value.is_null() {
                return false;
            }
        }
        self.mapped.insert(addr, frame);
        true
    }

    /// Unmap whatever frame is at `addr`, leaving a placeholder for the next map.
    pub fn unmap(&mut self, addr: u64) -> bool {
        if self.mapped.remove(&addr).is_none() {
            return false;
        }
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: addr as *mut c_void }, MEM_PRESERVE_PLACEHOLDER).is_ok()
        }
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
        // a mapped frame cannot be freed; unmapped, it can
        assert!(!p.free(&frames[..1]));
        for i in [0, 1, 2, 8, 12] {
            p.unmap(at(i));
        }
        assert!(p.free(&frames));
        assert_eq!(p.live_frames(), 0);
    }
}
