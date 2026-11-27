//! A copy of a title image the Windows loader will map but not run.
//!
//! The idea is the fork's (`victory-loader` `prepare.rs`, read for design); this is our own code.
//! The patched copy is handed to `LoadLibraryExW`, so Windows does the mapping, relocation, static TLS
//! allocation and unwind-table registration, and debuggers and profilers see a real module. But it
//! runs none of the title's code and resolves none of its imports. We bind the imports ourselves and
//! call the TLS callbacks and the entry point when the runtime is ready.
//!
//! The changes, all on a copy:
//!
//! - `IMAGE_FILE_DLL` set and the entry point cleared: Windows maps it like a DLL and calls nothing;
//! - the import directory is pointed at its own null terminator (empty, but present: Windows 10 still
//!   assigns static TLS to a module whose import table exists), and bound imports are cleared;
//! - the TLS callback pointer is cleared, and its base relocation becomes a no-op so the zero survives a
//!   rebase. Static TLS is still allocated; the callbacks run only when we call them;
//! - `FORCE_INTEGRITY` is cleared and the checksum zeroed: the copy is unsigned.

use crate::pe::Image;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn put_u16(b: &mut [u8], at: usize, v: u16) -> Option<()> {
    b.get_mut(at..at + 2)?.copy_from_slice(&v.to_le_bytes());
    Some(())
}
fn put_u32(b: &mut [u8], at: usize, v: u32) -> Option<()> {
    b.get_mut(at..at + 4)?.copy_from_slice(&v.to_le_bytes());
    Some(())
}
fn put_u64(b: &mut [u8], at: usize, v: u64) -> Option<()> {
    b.get_mut(at..at + 8)?.copy_from_slice(&v.to_le_bytes());
    Some(())
}

/// File offset of an RVA, through the section table.
fn offset_of(img: &Image, rva: u32) -> Option<usize> {
    img.sections
        .iter()
        .find(|s| rva >= s.rva && rva < s.rva + s.virtual_size.max(s.file_size))
        .map(|s| (rva - s.rva + s.file_offset) as usize)
}

const IMAGE_FILE_DLL: u16 = 0x2000;
const FORCE_INTEGRITY: u16 = 0x0080;
const DIR_IMPORT: usize = 1;
const DIR_BASERELOC: usize = 5;
const DIR_TLS: usize = 9;
const DIR_BOUND_IMPORT: usize = 11;

/// What `prepare` changed, for the record.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Patched {
    pub import_terminator_rva: Option<u32>,
    pub tls_callbacks_cleared: bool,
    pub reloc_neutralised: bool,
}

pub fn prepare(original: &[u8], img: &Image) -> Result<(Vec<u8>, Patched), String> {
    let mut b = original.to_vec();
    let mut p = Patched::default();
    let lfanew = u32_at(&b, 0x3C).ok_or("no e_lfanew")? as usize;
    let fh = lfanew + 4;
    let opt = fh + 20;
    if u16_at(&b, opt) != Some(0x20B) {
        return Err("not PE32+".into());
    }
    let ch = u16_at(&b, fh + 18).ok_or("no characteristics")?;
    put_u16(&mut b, fh + 18, ch | IMAGE_FILE_DLL).ok_or("patch characteristics")?;
    put_u32(&mut b, opt + 16, 0).ok_or("patch entry point")?;
    put_u32(&mut b, opt + 64, 0).ok_or("patch checksum")?;
    let dc = u16_at(&b, opt + 70).ok_or("no dll characteristics")?;
    put_u16(&mut b, opt + 70, dc & !FORCE_INTEGRITY).ok_or("patch dll characteristics")?;
    let dir = |i: usize| opt + 112 + i * 8;

    // imports: point the directory at its own null terminator (20-byte descriptors)
    let import_rva = u32_at(&b, dir(DIR_IMPORT)).ok_or("no import dir")?;
    if import_rva != 0 {
        let terminator = import_rva + img.imports.len() as u32 * 20;
        put_u32(&mut b, dir(DIR_IMPORT), terminator).ok_or("patch import dir")?;
        put_u32(&mut b, dir(DIR_IMPORT) + 4, 20).ok_or("patch import size")?;
        p.import_terminator_rva = Some(terminator);
    }
    put_u32(&mut b, dir(DIR_BOUND_IMPORT), 0).ok_or("patch bound imports")?;
    put_u32(&mut b, dir(DIR_BOUND_IMPORT) + 4, 0).ok_or("patch bound imports")?;

    // TLS: clear AddressOfCallBacks (offset 24 in the PE32+ TLS directory)
    let tls_rva = u32_at(&b, dir(DIR_TLS)).ok_or("no tls dir")?;
    if tls_rva != 0 {
        let at = offset_of(img, tls_rva).ok_or("TLS directory outside every section")? + 24;
        put_u64(&mut b, at, 0).ok_or("patch tls callbacks")?;
        p.tls_callbacks_cleared = true;
        // and make its base relocation a no-op, so the zero is not rebased into an address
        let target = tls_rva + 24;
        let (rel_rva, rel_size) = (u32_at(&b, dir(DIR_BASERELOC)).unwrap_or(0), u32_at(&b, dir(DIR_BASERELOC) + 4).unwrap_or(0));
        if rel_rva != 0 {
            let start = offset_of(img, rel_rva).ok_or("relocations outside every section")?;
            let mut at = start;
            while at + 8 <= start + rel_size as usize {
                let page = u32_at(&b, at).ok_or("reloc page")?;
                let size = u32_at(&b, at + 4).ok_or("reloc size")? as usize;
                if size < 8 {
                    break;
                }
                for e in (at + 8..at + size).step_by(2) {
                    let v = u16_at(&b, e).ok_or("reloc entry")?;
                    if v >> 12 != 0 && page + (v & 0xFFF) as u32 == target {
                        put_u16(&mut b, e, 0).ok_or("patch reloc")?;
                        p.reloc_neutralised = true;
                    }
                }
                at += size;
            }
        }
    }
    Ok((b, p))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn the_windows_loader_maps_a_prepared_image_assigns_static_tls_and_runs_nothing() {
        use windows::core::PCWSTR;
        use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_FLAGS};

        // our own test binary: a PE32+ exe with static TLS and TLS callbacks (Rust's std)
        let exe = std::env::current_exe().unwrap();
        let original = std::fs::read(&exe).unwrap();
        let img = Image::parse(&original).unwrap();
        let tls = img.tls.clone().expect("Rust test binaries use static TLS");
        let (bytes, patched) = prepare(&original, &img).unwrap();
        assert!(patched.tls_callbacks_cleared);

        let dir = std::env::temp_dir().join("rebuild-prepare-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("prepared-{}.dll", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_FLAGS(0)) }.expect("Windows loads the prepared copy");
        let base = module.0 as u64;
        assert_ne!(base, 0);

        // Windows assigned the copy a static TLS index: it wrote it to AddressOfIndex
        let index_slot = base + (tls.index_va - img.image_base);
        let index = unsafe { (index_slot as *const u32).read() };
        // and this thread's TLS array has a block at that index
        let teb: u64;
        unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb) };
        let tls_array = unsafe { ((teb + 0x58) as *const *const u64).read() };
        let block = unsafe { tls_array.add(index as usize).read() };
        assert_ne!(block, 0, "static TLS block for index {index} on this thread");
        // the TLS callbacks pointer in the mapped copy is still zero after relocation
        let tls_dir = base + (u32_at(&bytes, (u32_at(&bytes, 0x3C).unwrap() + 4 + 20 + 112 + 9 * 8) as usize).unwrap() as u64);
        assert_eq!(unsafe { ((tls_dir + 24) as *const u64).read() }, 0);
    }
}
