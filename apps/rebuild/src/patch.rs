//! `REBUILD_PATCH=exe+0x4fd1ddb=9090909090;exe+0x...=c3`: diagnostic byte patches in the title's code, applied
//! after the images are mapped and before its entry point runs. Each prints the bytes it replaced. For
//! experiments only (skip a call to see what happens next); never a fix.

#![cfg(windows)]

use std::ffi::c_void;

use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::Threading::GetCurrentProcess;

/// One patch: (image name prefix, offset, bytes).
pub fn parse(spec: &str) -> Result<Vec<(String, u64, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for item in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (at, bytes) = item.split_once('=').ok_or(format!("{item}: want image+offset=hexbytes"))?;
        let (image, off) = at.split_once('+').ok_or(format!("{item}: want image+offset"))?;
        let off = u64::from_str_radix(off.trim_start_matches("0x"), 16).map_err(|e| format!("{item}: {e}"))?;
        let hex: String = bytes.chars().filter(|c| !c.is_whitespace()).collect();
        if hex.is_empty() || hex.len() % 2 != 0 {
            return Err(format!("{item}: an even number of hex digits"));
        }
        let b = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<Vec<u8>, _>>().map_err(|e| format!("{item}: {e}"))?;
        out.push((image.to_ascii_lowercase(), off, b));
    }
    Ok(out)
}

/// Apply `REBUILD_PATCH` to the mapped images `(name, base, size)`.
pub fn apply(images: &[(String, u64, u64)]) {
    let Ok(spec) = std::env::var("REBUILD_PATCH") else { return };
    let patches = match parse(&spec) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("patch     refused: {e}");
            return;
        }
    };
    for (image, off, bytes) in patches {
        let target = images.iter().find(|(n, _, _)| {
            let n = n.to_ascii_lowercase();
            if image == "exe" { n.ends_with(".exe") } else { n.starts_with(&image) }
        });
        let Some((name, base, size)) = target else {
            eprintln!("patch     no image {image}");
            continue;
        };
        if off + bytes.len() as u64 > *size {
            eprintln!("patch     {name}+{off:#x} is outside the image");
            continue;
        }
        let at = (base + off) as *mut u8;
        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            if VirtualProtect(at as *const c_void, bytes.len(), PAGE_EXECUTE_READWRITE, &mut old).is_err() {
                eprintln!("patch     {name}+{off:#x}: cannot unprotect");
                continue;
            }
            let before: Vec<String> = std::slice::from_raw_parts(at, bytes.len()).iter().map(|b| format!("{b:02x}")).collect();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
            let mut ignored = PAGE_PROTECTION_FLAGS(0);
            let _ = VirtualProtect(at as *const c_void, bytes.len(), old, &mut ignored);
            let _ = FlushInstructionCache(GetCurrentProcess(), Some(at as *const c_void), bytes.len());
            eprintln!("patch     {name}+{off:#x}: {} -> {}", before.join(""), bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_parse_and_bad_ones_are_named() {
        let p = parse("exe+0x4fd1ddb=90 90 90 90 90; xboxbvh+10=c3").unwrap();
        assert_eq!(p[0], ("exe".into(), 0x4fd1ddb, vec![0x90; 5]));
        assert_eq!(p[1], ("xboxbvh".into(), 0x10, vec![0xc3]));
        assert!(parse("exe+10=9").is_err());
        assert!(parse("exe10=90").is_err());
    }
}
