//! Read-write memory reserved and committed at a chosen address, to map an image into. Never made
//! executable here: this half of Build 3 runs no title code.

use std::ffi::c_void;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;

#[link(name = "kernel32")]
extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
}

pub struct Region {
    ptr: *mut u8,
    len: usize,
}

impl Region {
    /// Reserves and commits `len` bytes at exactly `base`; `None` if that range is taken.
    pub fn at(base: u64, len: usize) -> Option<Region> {
        let p = unsafe { VirtualAlloc(base as *mut c_void, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        if p.is_null() {
            return None;
        }
        if p as u64 != base {
            unsafe { VirtualFree(p, 0, MEM_RELEASE) };
            return None;
        }
        Some(Region { ptr: p as *mut u8, len })
    }

    pub fn base(&self) -> u64 {
        self.ptr as u64
    }

    pub fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe { VirtualFree(self.ptr as *mut c_void, 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{map_into, MapOptions, NullResolver};
    use crate::pe::Image;
    use crate::synth;

    #[test]
    fn mapped_at_a_chosen_base_the_pointers_dereference() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        // the first free 64 KiB-aligned address from a fixed start
        let mut base = 0x3_0000_0000u64;
        let mut r = loop {
            if let Some(r) = Region::at(base, img.size_of_image as usize) {
                break r;
            }
            base += 0x100_0000;
        };
        let rep = map_into(&img, &s.file, r.base(), r.bytes(), &mut NullResolver, MapOptions::default()).unwrap();
        assert_eq!(rep.relocs_applied, s.dir64.len());
        unsafe {
            let p_str = *((base + synth::P_STR as u64) as *const u64) as *const u8;
            assert_eq!(std::slice::from_raw_parts(p_str, synth::STR_TEXT.len()), synth::STR_TEXT);
            let p_self = (base + synth::P_SELF as u64) as *const u64;
            assert_eq!(*p_self, p_self as u64);
            let p_f1 = *((base + synth::P_F1 as u64) as *const u64) as *const u8;
            assert_eq!(*p_f1, 0xc3);
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn FreeLibrary(m: *mut c_void) -> i32;
    }
    const DONT_RESOLVE_DLL_REFERENCES: u32 = 0x1;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The Windows loader maps a system DLL (no DllMain, no imports bound); we map the same file at
    /// the same base. Every relocated pointer must equal the Windows loader's.
    #[test]
    fn matches_the_windows_loader_on_a_system_dll() {
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        for name in ["dbghelp.dll", "msi.dll", "d3dcompiler_47.dll", "wininet.dll"] {
            let path = format!(r"{sys}\System32\{name}");
            let Ok(file) = std::fs::read(&path) else { continue };
            if !unsafe { GetModuleHandleW(wide(name).as_ptr()) }.is_null() {
                continue; // already loaded by something else: its memory may have changed since
            }
            let m = unsafe { LoadLibraryExW(wide(&path).as_ptr(), std::ptr::null_mut(), DONT_RESOLVE_DLL_REFERENCES) };
            if m.is_null() {
                continue;
            }
            let img = Image::parse(&file).unwrap();
            let base = m as u64;
            let mut ours = vec![0u8; img.size_of_image as usize];
            let rep = map_into(&img, &file, base, &mut ours, &mut NullResolver, MapOptions { bind_delay: false }).unwrap();
            let live = unsafe { std::slice::from_raw_parts(m as *const u8, img.size_of_image as usize) };
            let mut reloc_wrong = 0;
            for r in &img.relocs {
                let a = r.rva as usize;
                if ours[a..a + 8] != live[a..a + 8] {
                    reloc_wrong += 1;
                }
            }
            let mut diff = Vec::new();
            let mut compared = 0usize;
            for s in &img.sections {
                let (a, n) = (s.rva as usize, s.mapped_file_size() as usize);
                compared += n;
                diff.extend((a..a + n).filter(|&i| ours[i] != live[i]).map(|i| (s.name.clone(), i)));
            }
            let diff_at: Vec<String> = diff.iter().take(16).map(|(s, i)| format!("{s}+0x{i:x}")).collect();
            let diff = diff.len();
            println!("{name}: differing bytes at {diff_at:?}");
            println!(
                "{name}: base 0x{base:x} (preferred 0x{:x}), {} relocations applied, {} wrong; {compared} section bytes compared, {diff} differ",
                img.image_base, rep.relocs_applied, reloc_wrong
            );
            unsafe { FreeLibrary(m) };
            assert!(rep.relocs_applied > 0, "{name} did not move");
            assert_eq!(reloc_wrong, 0);
            return;
        }
        panic!("no candidate system DLL could be loaded");
    }
}
