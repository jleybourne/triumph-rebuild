//! The host's own functions and structures, declared exactly as `kernel32` exports them. Calling
//! them raw (not through a wrapper crate) keeps `GetLastError` exactly what the host set.

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type BOOL = i32;

pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WIN32_FIND_DATAW {
    pub dwFileAttributes: u32,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub nFileSizeHigh: u32,
    pub nFileSizeLow: u32,
    pub dwReserved0: u32,
    pub dwReserved1: u32,
    pub cFileName: [u16; 260],
    pub cAlternateFileName: [u16; 14],
}

impl Default for WIN32_FIND_DATAW {
    fn default() -> Self {
        // SAFETY: plain data, all-zero is valid
        unsafe { std::mem::zeroed() }
    }
}

impl WIN32_FIND_DATAW {
    pub fn name(&self) -> String {
        let n = self.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
        String::from_utf16_lossy(&self.cFileName[..n])
    }
    pub fn size(&self) -> u64 {
        (u64::from(self.nFileSizeHigh) << 32) | u64::from(self.nFileSizeLow)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WIN32_FIND_DATAA {
    pub dwFileAttributes: u32,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub nFileSizeHigh: u32,
    pub nFileSizeLow: u32,
    pub dwReserved0: u32,
    pub dwReserved1: u32,
    pub cFileName: [u8; 260],
    pub cAlternateFileName: [u8; 14],
}

impl Default for WIN32_FIND_DATAA {
    fn default() -> Self {
        // SAFETY: plain data, all-zero is valid
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct WIN32_FILE_ATTRIBUTE_DATA {
    pub dwFileAttributes: u32,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub nFileSizeHigh: u32,
    pub nFileSizeLow: u32,
}

#[repr(C)]
pub struct CREATEFILE2_EXTENDED_PARAMETERS {
    pub dwSize: u32,
    pub dwFileAttributes: u32,
    pub dwFileFlags: u32,
    pub dwSecurityQosFlags: u32,
    pub lpSecurityAttributes: *mut c_void,
    pub hTemplateFile: HANDLE,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct COPYFILE2_EXTENDED_PARAMETERS {
    pub dwSize: u32,
    pub dwCopyFlags: u32,
    pub pfCancel: *mut i32,
    pub pProgressRoutine: *mut c_void,
    pub pvCallbackContext: *mut c_void,
}

/// `HRESULT_FROM_WIN32`.
pub fn hresult(code: u32) -> i32 {
    if code == 0 {
        0
    } else {
        ((code & 0xFFFF) | 0x8007_0000) as i32
    }
}

/// A host path in the plain `C:\…` form when it fits in `MAX_PATH`, else the `\\?\` form. Some host
/// functions (DOS wildcards, the CRT) treat the two forms differently.
pub fn host_path_plain(p: &std::path::Path) -> Vec<u16> {
    let mut w = host_path(p);
    if w.len() < 250 && w.starts_with(&[92, 92, 63, 92]) && w.get(5) == Some(&58) {
        w.drain(..4);
    }
    w
}

#[link(name = "kernel32")]
extern "system" {
    pub fn GetLastError() -> u32;
    pub fn SetLastError(code: u32);
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn ReadFile(h: HANDLE, buf: *mut c_void, len: u32, read: *mut u32, ov: *mut c_void) -> BOOL;
    pub fn WriteFile(h: HANDLE, buf: *const c_void, len: u32, written: *mut u32, ov: *mut c_void) -> BOOL;

    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sa: *const c_void,
        disposition: u32,
        flags: u32,
        template: HANDLE,
    ) -> HANDLE;
    pub fn CreateFileA(
        name: *const u8,
        access: u32,
        share: u32,
        sa: *const c_void,
        disposition: u32,
        flags: u32,
        template: HANDLE,
    ) -> HANDLE;
    pub fn CreateFile2(
        name: *const u16,
        access: u32,
        share: u32,
        disposition: u32,
        params: *const CREATEFILE2_EXTENDED_PARAMETERS,
    ) -> HANDLE;
    pub fn DeleteFileW(name: *const u16) -> BOOL;
    pub fn DeleteFileA(name: *const u8) -> BOOL;
    pub fn CreateDirectoryW(name: *const u16, sa: *const c_void) -> BOOL;
    pub fn CreateDirectoryA(name: *const u8, sa: *const c_void) -> BOOL;
    pub fn RemoveDirectoryW(name: *const u16) -> BOOL;
    pub fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> BOOL;
    pub fn GetFileAttributesW(name: *const u16) -> u32;
    pub fn GetFileAttributesA(name: *const u8) -> u32;
    pub fn SetFileAttributesW(name: *const u16, attrs: u32) -> BOOL;
    pub fn GetFileAttributesExW(name: *const u16, level: i32, info: *mut c_void) -> BOOL;
    pub fn GetFileAttributesExA(name: *const u8, level: i32, info: *mut c_void) -> BOOL;
    pub fn FindFirstFileExW(
        name: *const u16,
        level: i32,
        data: *mut c_void,
        search_op: i32,
        filter: *const c_void,
        flags: u32,
    ) -> HANDLE;
    pub fn FindFirstFileW(name: *const u16, data: *mut WIN32_FIND_DATAW) -> HANDLE;
    pub fn FindFirstFileA(name: *const u8, data: *mut WIN32_FIND_DATAA) -> HANDLE;
    pub fn FindNextFileW(h: HANDLE, data: *mut WIN32_FIND_DATAW) -> BOOL;
    pub fn FindNextFileA(h: HANDLE, data: *mut WIN32_FIND_DATAA) -> BOOL;
    pub fn FindClose(h: HANDLE) -> BOOL;
    pub fn GetFullPathNameW(name: *const u16, len: u32, buf: *mut u16, file_part: *mut *mut u16) -> u32;
    pub fn GetFullPathNameA(name: *const u8, len: u32, buf: *mut u8, file_part: *mut *mut u8) -> u32;
    pub fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> BOOL;
    pub fn GetCurrentDirectoryW(len: u32, buf: *mut u16) -> u32;
    pub fn SetCurrentDirectoryW(name: *const u16) -> BOOL;
    pub fn LoadLibraryExW(name: *const u16, file: HANDLE, flags: u32) -> *mut c_void;
    pub fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    pub fn GetFinalPathNameByHandleW(h: HANDLE, buf: *mut u16, len: u32, flags: u32) -> u32;
    pub fn SetFileInformationByHandle(h: HANDLE, class: i32, info: *const c_void, size: u32) -> BOOL;
    pub fn CopyFile2(from: *const u16, to: *const u16, params: *const COPYFILE2_EXTENDED_PARAMETERS) -> i32;
    pub fn CreateHardLinkW(new: *const u16, existing: *const u16, sa: *const c_void) -> BOOL;
    pub fn CreateSymbolicLinkW(link: *const u16, target: *const u16, flags: u32) -> u8;
    pub fn SetFilePointerEx(h: HANDLE, distance: i64, new: *mut i64, method: u32) -> BOOL;
    pub fn SetEndOfFile(h: HANDLE) -> BOOL;
    pub fn MultiByteToWideChar(cp: u32, flags: u32, s: *const u8, n: i32, w: *mut u16, wn: i32) -> i32;
    pub fn WideCharToMultiByte(
        cp: u32,
        flags: u32,
        w: *const u16,
        wn: i32,
        s: *mut u8,
        n: i32,
        default: *const u8,
        used_default: *mut i32,
    ) -> i32;
}

// ---- strings -----------------------------------------------------------------------------------

/// A NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The string at a NUL-terminated UTF-16 pointer (`None` for NULL).
///
/// # Safety
/// `p` is NULL or points at a NUL-terminated UTF-16 string.
pub unsafe fn from_wide(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)))
}

const CP_ACP: u32 = 0;

/// The string at a NUL-terminated ANSI pointer, in the host's ANSI code page, as the `A` functions read it.
///
/// # Safety
/// `p` is NULL or points at a NUL-terminated byte string.
pub unsafe fn from_ansi(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    if n == 0 {
        return Some(String::new());
    }
    let wn = MultiByteToWideChar(CP_ACP, 0, p, n as i32, std::ptr::null_mut(), 0);
    let mut w = vec![0u16; wn.max(0) as usize];
    MultiByteToWideChar(CP_ACP, 0, p, n as i32, w.as_mut_ptr(), wn);
    Some(String::from_utf16_lossy(&w))
}

/// `s` in the host's ANSI code page, without a NUL.
pub fn to_ansi(s: &str) -> Vec<u8> {
    let w: Vec<u16> = s.encode_utf16().collect();
    if w.is_empty() {
        return Vec::new();
    }
    unsafe {
        let n = WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut());
        let mut out = vec![0u8; n.max(0) as usize];
        WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut());
        out
    }
}

/// A host path as the host's `W` functions should get it: absolute and in the `\\?\` form, so the
/// overlay folder's extra length never meets `MAX_PATH`.
pub fn host_path(p: &std::path::Path) -> Vec<u16> {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy();
    let s = if s.starts_with(r"\\?\") {
        s.into_owned()
    } else if let Some(unc) = s.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{s}")
    };
    wide(&s)
}
