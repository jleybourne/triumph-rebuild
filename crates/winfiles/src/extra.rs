//! F5: copies, links, crash-report files, and the changes the host makes through a handle.
//!
//! - `CopyFile2`: the host's copy, from the winning layer into the overlay.
//! - `CreateHardLinkW`: the existing file is copied up first, so a link can never share data with
//!   the base; the link is made in the overlay.
//! - `CreateSymbolicLinkW`: refused with `ERROR_PRIVILEGE_NOT_HELD` (the host's answer to an
//!   unprivileged caller) and logged: a link target is a console path the host file system cannot follow.
//! - `WerRegisterFile`: succeeds and is logged; crash reports are not collected.
//! - `SetFileInformationByHandle`: a rename or a delete disposition through a handle reaches the
//!   host file directly, so the overlay's index is told (renames go to an overlay path; deletes
//!   settle once the host has removed the file). Other classes go straight to the host.

use std::ffi::c_void;
use std::path::PathBuf;

use rebuild_vfs::{ConsolePath, Disposition, Error as VfsError, Layer, OpenOptions};

use crate::host::{self, COPYFILE2_EXTENDED_PARAMETERS, HANDLE};
use crate::{
    discover, installed, set_last_error, with, Files, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_INVALID_PARAMETER,
    ERROR_NOT_READY, ERROR_NOT_SAME_DEVICE, ERROR_PATH_NOT_FOUND, FILE_ATTRIBUTE_READONLY, INVALID_FILE_ATTRIBUTES,
};

pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;
const COPY_FILE_FAIL_IF_EXISTS: u32 = 0x1;

const FILE_RENAME_INFO: i32 = 3;
const FILE_DISPOSITION_INFO: i32 = 4;
const FILE_DISPOSITION_INFO_EX: i32 = 21;
const FILE_RENAME_INFO_EX: i32 = 22;

impl Files {
    /// An existing target in the merged view: the host's error for it, if any.
    fn target_error(&self, function: &'static str, dst: &str, fail_if_exists: bool, exists_code: u32) -> Result<bool, u32> {
        match self.vfs.resolve(dst) {
            Ok(r) if r.is_dir => Err(ERROR_ACCESS_DENIED),
            Ok(_) if fail_if_exists => Err(exists_code),
            Ok(r) => {
                let a = unsafe { host::GetFileAttributesW(host::host_path(&r.host).as_ptr()) };
                if a != INVALID_FILE_ATTRIBUTES && a & FILE_ATTRIBUTE_READONLY != 0 {
                    return Err(ERROR_ACCESS_DENIED);
                }
                Ok(true)
            }
            Err(VfsError::NotFound) => Ok(false),
            Err(e) => Err(self.fail(function, dst, e)),
        }
    }

    fn copy_file2(&self, from: &str, to: &str, params: *const COPYFILE2_EXTENDED_PARAMETERS) -> Result<i32, u32> {
        const F: &str = "CopyFile2";
        let src = self.path(F, from)?;
        let r = self.vfs.resolve(&src).map_err(|e| self.fail(F, &src, e))?;
        if r.is_dir {
            return Err(ERROR_ACCESS_DENIED);
        }
        let dst = self.path(F, to)?;
        let mut p = unsafe { params.as_ref().copied() };
        let fail_if_exists = p.is_some_and(|p| p.dwCopyFlags & COPY_FILE_FAIL_IF_EXISTS != 0);
        let existed = self.target_error(F, &dst, fail_if_exists, ERROR_FILE_EXISTS)?;
        let dst_host = self.vfs.overlay_path_for(&dst).map_err(|e| self.fail(F, &dst, e))?;
        if let Some(p) = p.as_mut() {
            p.dwCopyFlags &= !COPY_FILE_FAIL_IF_EXISTS; // checked above, in the merged view
        }
        let pp = p.as_ref().map_or(std::ptr::null(), |p| p as *const _);
        let hr = unsafe { host::CopyFile2(host::host_path(&r.host).as_ptr(), host::host_path(&dst_host).as_ptr(), pp) };
        if hr >= 0 {
            let _ = self.vfs.note_created(&dst);
        }
        let _ = existed;
        Ok(hr)
    }

    fn hard_link(&self, new: &str, existing: &str) -> Result<i32, u32> {
        const F: &str = "CreateHardLinkW";
        let ex = self.path(F, existing)?;
        let r = self.vfs.resolve(&ex).map_err(|e| self.fail(F, &ex, e))?;
        if r.is_dir {
            return Err(ERROR_ACCESS_DENIED);
        }
        let nw = self.path(F, new)?;
        match self.vfs.resolve(&nw) {
            Ok(_) => return Err(ERROR_ALREADY_EXISTS),
            Err(VfsError::NotFound) => {}
            Err(e) => return Err(self.fail(F, &nw, e)),
        }
        if crate::drive_of(&ex) != crate::drive_of(&nw) {
            return Err(ERROR_NOT_SAME_DEVICE);
        }
        let mut target = r.host;
        if r.layer != Layer::Overlay {
            // never link to the base: copy up, link the copy
            let h = self.vfs.open(&ex, OpenOptions::write(Disposition::OpenExisting)).map_err(|e| self.fail(F, &ex, e))?;
            let _ = self.vfs.close(h);
            target = self.vfs.resolve(&ex).map_err(|e| self.fail(F, &ex, e))?.host;
        }
        let link = self.vfs.overlay_path_for(&nw).map_err(|e| self.fail(F, &nw, e))?;
        if unsafe { host::CreateHardLinkW(host::host_path(&link).as_ptr(), host::host_path(&target).as_ptr(), std::ptr::null()) } == 0 {
            return Err(unsafe { host::GetLastError() });
        }
        self.vfs.note_created(&nw).map_err(|e| self.fail(F, &nw, e))?;
        Ok(1)
    }

    /// The console path of the overlay file a host handle has open.
    fn console_of(&self, h: HANDLE) -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let n = unsafe { host::GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        self.vfs.console_path_of(&PathBuf::from(String::from_utf16_lossy(&buf[..n])))
    }

    unsafe fn set_info(&self, h: HANDLE, class: i32, info: *const c_void, size: u32) -> Result<i32, u32> {
        const F: &str = "SetFileInformationByHandle";
        match class {
            FILE_DISPOSITION_INFO | FILE_DISPOSITION_INFO_EX => {
                let r = host::SetFileInformationByHandle(h, class, info, size);
                if r == 0 {
                    return Err(host::GetLastError());
                }
                let deleting = if class == FILE_DISPOSITION_INFO { *(info as *const u8) != 0 } else { *(info as *const u32) & 1 != 0 };
                if deleting {
                    if let Some(p) = self.console_of(h) {
                        let _ = self.vfs.delete_pending(&p);
                    }
                }
                Ok(r)
            }
            FILE_RENAME_INFO | FILE_RENAME_INFO_EX => {
                if info.is_null() || size < 22 {
                    return Err(ERROR_INVALID_PARAMETER);
                }
                let b = info as *const u8;
                let flags = *(b as *const u32);
                let replace = if class == FILE_RENAME_INFO { flags & 0xFF != 0 } else { flags & 1 != 0 };
                let root = *(b.add(8) as *const HANDLE);
                let len = *(b.add(16) as *const u32) as usize / 2;
                if 20 + len * 2 > size as usize {
                    return Err(ERROR_INVALID_PARAMETER);
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(b.add(20) as *const u16, len));
                if !root.is_null() {
                    discover(F, &name, "a rename relative to a directory handle");
                    return Err(ERROR_INVALID_PARAMETER);
                }
                // a handle below the overlay was opened without write access: the host refuses too
                let src = self.console_of(h).ok_or(ERROR_ACCESS_DENIED)?;
                // as the host: a relative name is relative to the current directory (ours)
                let dst = self.path(F, &name)?;
                if crate::drive_of(&src) != crate::drive_of(&dst) {
                    return Err(ERROR_NOT_SAME_DEVICE);
                }
                let same = ConsolePath::parse(&src).ok().map(|p| p.key()) == ConsolePath::parse(&dst).ok().map(|p| p.key());
                if !same {
                    match self.vfs.resolve(&dst) {
                        Ok(r) if r.is_dir => return Err(ERROR_ACCESS_DENIED),
                        Ok(_) if !replace => return Err(ERROR_ALREADY_EXISTS),
                        Ok(_) | Err(VfsError::NotFound) => {}
                        Err(VfsError::PathNotFound) => return Err(ERROR_PATH_NOT_FOUND),
                        Err(e) => return Err(self.fail(F, &dst, e)),
                    }
                }
                let dst_host = self.vfs.overlay_path_for(&dst).map_err(|e| self.fail(F, &dst, e))?;
                let mut target = host::host_path(&dst_host);
                target.pop();
                let mut buf = vec![0u8; 20 + target.len() * 2 + 2];
                buf[..4].copy_from_slice(&flags.to_ne_bytes());
                buf[16..20].copy_from_slice(&((target.len() * 2) as u32).to_ne_bytes());
                for (i, c) in target.iter().enumerate() {
                    buf[20 + i * 2..22 + i * 2].copy_from_slice(&c.to_ne_bytes());
                }
                if host::SetFileInformationByHandle(h, class, buf.as_ptr() as *const c_void, buf.len() as u32) == 0 {
                    return Err(host::GetLastError());
                }
                self.vfs.note_renamed(&src, &dst).map_err(|e| self.fail(F, &dst, e))?;
                Ok(1)
            }
            _ => match host::SetFileInformationByHandle(h, class, info, size) {
                0 => Err(host::GetLastError()),
                r => Ok(r),
            },
        }
    }
}

pub unsafe extern "system" fn CopyFile2(from: *const u16, to: *const u16, params: *const COPYFILE2_EXTENDED_PARAMETERS) -> i32 {
    let (src, dst) = (host::from_wide(from).unwrap_or_default(), host::from_wide(to).unwrap_or_default());
    let Some(files) = installed() else {
        discover("CopyFile2", "", "no file system installed");
        return host::hresult(ERROR_NOT_READY);
    };
    match files.copy_file2(&src, &dst, params) {
        Ok(hr) => hr,
        Err(code) => {
            set_last_error(code);
            host::hresult(code)
        }
    }
}

pub unsafe extern "system" fn CreateHardLinkW(new: *const u16, existing: *const u16, _sa: *const c_void) -> i32 {
    let (n, e) = (host::from_wide(new).unwrap_or_default(), host::from_wide(existing).unwrap_or_default());
    with("CreateHardLinkW", 0, |f| f.hard_link(&n, &e))
}

pub unsafe extern "system" fn CreateSymbolicLinkW(link: *const u16, target: *const u16, _flags: u32) -> u8 {
    let (l, t) = (host::from_wide(link).unwrap_or_default(), host::from_wide(target).unwrap_or_default());
    discover("CreateSymbolicLinkW", &format!("{l} -> {t}"), "symbolic links are refused: a console target cannot be followed");
    set_last_error(ERROR_PRIVILEGE_NOT_HELD);
    0
}

/// `HRESULT WerRegisterFile(PCWSTR, WER_REGISTER_FILE_TYPE, DWORD)`.
pub unsafe extern "system" fn WerRegisterFile(path: *const u16, _kind: i32, _flags: u32) -> i32 {
    let p = host::from_wide(path).unwrap_or_default();
    discover("WerRegisterFile", &p, "registered for crash reports; reports are not collected");
    0
}

pub unsafe extern "system" fn SetFileInformationByHandle(h: HANDLE, class: i32, info: *const c_void, size: u32) -> i32 {
    let Some(files) = installed() else {
        // nothing to keep in step: the host's own
        return host::SetFileInformationByHandle(h, class, info, size);
    };
    match files.set_info(h, class, info, size) {
        Ok(r) => r,
        Err(code) => {
            set_last_error(code);
            0
        }
    }
}
