//! The console's device type, as the title reads it from the registry.
//!
//! The GDK's device-type lookup, compiled into the title (exe+0x55deb88), opens
//! `HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM` and reads the DWORD `ConsoleMode`: 0x60 is Series S
//! (type 6), 0x64 Series X (type 7), 0x68 a devkit (8). On a PC the key is absent, so the title took itself for
//! a PC (type 1) and went down paths a console build does not fill (no program for an immediate draw:
//! `exe+0xb5a5f8`). We answer that one key and value; every other registry call goes to the host unchanged.
//!
//! `REBUILD_CONSOLE=series-s` answers Series S; otherwise Series X (the target).

// the host's own names, so the override table and the code read the same
#![allow(non_snake_case)]

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey as HostRegCloseKey, RegGetValueW as HostRegGetValueW, RegOpenKeyExW as HostRegOpenKeyExW, RegOpenKeyW as HostRegOpenKeyW,
    RegQueryValueExW as HostRegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, REG_SAM_FLAGS, REG_DWORD, REG_ROUTINE_FLAGS, REG_VALUE_TYPE,
};

/// Our handle for the OEM key (never a real handle: kernel handles are multiples of 4 and far smaller).
const OEM: isize = 0x7E57_0E11;
const OEM_KEY: &str = r"software\microsoft\windows nt\currentversion\oem";
const REG_DWORD_FLAG: u32 = 0x10; // RRF_RT_REG_DWORD

/// The `ConsoleMode` value we answer.
pub fn console_mode() -> u32 {
    match std::env::var("REBUILD_CONSOLE").as_deref() {
        Ok("series-s") | Ok("s") => 0x60,
        _ => 0x64,
    }
}

fn wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: a NUL-terminated string from the title
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

fn is_oem(root: HKEY, subkey: *const u16) -> bool {
    let yes = root == HKEY_LOCAL_MACHINE && wide(subkey).trim_matches('\\').eq_ignore_ascii_case(OEM_KEY);
    static ONCE: std::sync::Once = std::sync::Once::new();
    if yes {
        ONCE.call_once(|| eprintln!("registry  the title reads its device type: ConsoleMode {:#x}", console_mode()));
    }
    yes
}

fn ours(h: HKEY) -> bool {
    h.0 as isize == OEM
}

fn code(e: WIN32_ERROR) -> i32 {
    e.0 as i32
}

pub unsafe extern "system" fn RegOpenKeyW(root: HKEY, subkey: *const u16, out: *mut HKEY) -> i32 {
    if is_oem(root, subkey) && !out.is_null() {
        unsafe { out.write(HKEY(OEM as *mut c_void)) };
        return code(ERROR_SUCCESS);
    }
    code(unsafe { HostRegOpenKeyW(root, PCWSTR(subkey), out) })
}

pub unsafe extern "system" fn RegOpenKeyExW(root: HKEY, subkey: *const u16, options: u32, sam: u32, out: *mut HKEY) -> i32 {
    if is_oem(root, subkey) && !out.is_null() {
        unsafe { out.write(HKEY(OEM as *mut c_void)) };
        return code(ERROR_SUCCESS);
    }
    code(unsafe { HostRegOpenKeyExW(root, PCWSTR(subkey), options, REG_SAM_FLAGS(sam), out) })
}

/// The DWORD answer for `ConsoleMode` into the caller's buffer, as RegQueryValueExW would.
unsafe fn answer(name: *const u16, kind: *mut u32, data: *mut u8, size: *mut u32) -> i32 {
    if !wide(name).eq_ignore_ascii_case("ConsoleMode") {
        return code(ERROR_FILE_NOT_FOUND);
    }
    if !kind.is_null() {
        unsafe { kind.write(REG_DWORD.0) };
    }
    if size.is_null() {
        return if data.is_null() { code(ERROR_SUCCESS) } else { 87 }; // ERROR_INVALID_PARAMETER
    }
    let room = unsafe { size.read() };
    unsafe { size.write(4) };
    if data.is_null() {
        return code(ERROR_SUCCESS);
    }
    if room < 4 {
        return code(ERROR_MORE_DATA);
    }
    unsafe { std::ptr::copy_nonoverlapping(console_mode().to_le_bytes().as_ptr(), data, 4) };
    code(ERROR_SUCCESS)
}

pub unsafe extern "system" fn RegQueryValueExW(key: HKEY, name: *const u16, reserved: *const u32, kind: *mut u32, data: *mut u8, size: *mut u32) -> i32 {
    if ours(key) {
        return unsafe { answer(name, kind, data, size) };
    }
    code(unsafe {
        HostRegQueryValueExW(key, PCWSTR(name), Some(reserved), Some(kind.cast::<REG_VALUE_TYPE>()), Some(data), Some(size))
    })
}

pub unsafe extern "system" fn RegGetValueW(key: HKEY, subkey: *const u16, name: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, size: *mut u32) -> i32 {
    let direct = ours(key) && wide(subkey).is_empty();
    if direct || is_oem(key, subkey) {
        if flags & REG_DWORD_FLAG == 0 {
            return 1630; // ERROR_UNSUPPORTED_TYPE
        }
        return unsafe { answer(name, kind, data.cast(), size) };
    }
    code(unsafe {
        HostRegGetValueW(key, PCWSTR(subkey), PCWSTR(name), REG_ROUTINE_FLAGS(flags), Some(kind.cast::<REG_VALUE_TYPE>()), Some(data), Some(size))
    })
}

pub unsafe extern "system" fn RegCloseKey(key: HKEY) -> i32 {
    if ours(key) {
        return code(ERROR_SUCCESS);
    }
    code(unsafe { HostRegCloseKey(key) })
}

/// The overrides, by name.
pub fn export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "RegOpenKeyW" => RegOpenKeyW as *const (),
        "RegOpenKeyExW" => RegOpenKeyExW as *const (),
        "RegQueryValueExW" => RegQueryValueExW as *const (),
        "RegGetValueW" => RegGetValueW as *const (),
        "RegCloseKey" => RegCloseKey as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn the_title_reads_a_series_x_and_other_keys_reach_the_host() {
        let mut h = HKEY::default();
        assert_eq!(unsafe { RegOpenKeyW(HKEY_LOCAL_MACHINE, w(r"Software\Microsoft\Windows NT\CurrentVersion\OEM").as_ptr(), &mut h) }, 0);
        let (mut kind, mut v, mut n) = (0u32, 0u32, 4u32);
        assert_eq!(unsafe { RegQueryValueExW(h, w("ConsoleMode").as_ptr(), std::ptr::null(), &mut kind, (&mut v as *mut u32).cast(), &mut n) }, 0);
        assert_eq!((kind, v, n), (REG_DWORD.0, console_mode(), 4));
        assert_eq!(unsafe { RegQueryValueExW(h, w("Other").as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut n) }, 2);
        assert_eq!(unsafe { RegCloseKey(h) }, 0);
        // a real key still opens through the host
        let mut real = HKEY::default();
        assert_eq!(unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, w(r"Software\Microsoft\Windows NT\CurrentVersion").as_ptr(), 0, 0x20019, &mut real) }, 0);
        assert!(!ours(real));
        assert_eq!(unsafe { RegCloseKey(real) }, 0);
    }
}
