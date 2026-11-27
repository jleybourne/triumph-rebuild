//! Directory searches over the merged view.
//!
//! A console directory can be several host directories (overlay, mods, base), so a search is ours:
//! for each layer that contributes, the host's own `FindFirstFileExW` runs on that layer's host
//! directory with the title's pattern, info level and flags (so wildcard rules, 8.3 matching and
//! every byte of `WIN32_FIND_DATA` are the host's), and an entry is kept only from the layer that
//! wins it in the merged view (so whiteouts and shadowed files never show). One layer: the host's
//! order. Several: sorted by upper-cased name, as NTFS lists. `.` and `..` come first, as the host
//! gives them, except at a drive root (a root has none, as on the host).
//!
//! Search handles are ours (above the user address space, never a host handle); `FindNextFile*`
//! and `FindClose` pass any other handle to the host.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;

use rebuild_vfs::{fold, ConsolePath, Layer};

use crate::host::{self, HANDLE, INVALID_HANDLE_VALUE, WIN32_FIND_DATAA, WIN32_FIND_DATAW};
use crate::{discover, set_last_error, Files, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND};

const SEARCH_BASE: usize = 0xFFFF_9000_0000_0000;

#[derive(Default)]
struct Searches {
    next: usize,
    open: HashMap<usize, VecDeque<WIN32_FIND_DATAW>>,
}

static SEARCHES: Mutex<Option<Searches>> = Mutex::new(None);

fn searches<R>(f: impl FnOnce(&mut Searches) -> R) -> R {
    let mut g = SEARCHES.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Searches::default))
}

/// Searches open now (for tests and the frame line).
pub fn open_searches() -> usize {
    searches(|s| s.open.len())
}

fn is_ours(h: HANDLE) -> bool {
    (h as usize) >= SEARCH_BASE && h != INVALID_HANDLE_VALUE
}

impl Files {
    /// Every entry of the search `raw` (a directory and a pattern), first to last.
    fn search(&self, function: &'static str, raw: &str, level: i32, op: i32, flags: u32) -> Result<Vec<WIN32_FIND_DATAW>, u32> {
        let abs = self.path(function, raw)?;
        let cut = abs.rfind(['\\', '/']).ok_or(ERROR_FILE_NOT_FOUND)?;
        let (dir, pattern) = (&abs[..cut], &abs[cut + 1..]);
        let dir = if dir.ends_with(':') { format!("{dir}\\") } else { dir.to_owned() };
        if pattern.is_empty() {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        let top = self.vfs.resolve(&dir).map_err(|e| match self.fail(function, &dir, e) {
            ERROR_FILE_NOT_FOUND => ERROR_PATH_NOT_FOUND,
            c => c,
        })?;
        if !top.is_dir {
            return Err(crate::ERROR_DIRECTORY);
        }
        let is_root = ConsolePath::parse(&dir).map(|p| p.is_root()).unwrap_or(false);
        let merged = self.vfs.enumerate(&dir).map_err(|e| self.fail(function, &dir, e))?;
        let winner: HashMap<String, Layer> = merged.iter().map(|e| (fold(&e.name), e.layer.clone())).collect();

        // one host directory per contributing layer, the top layer's first
        let mut layers = vec![(top.layer.clone(), top.host.clone())];
        for e in &merged {
            if layers.iter().any(|(l, _)| *l == e.layer) {
                continue;
            }
            if let Ok(r) = self.vfs.resolve(&crate::join(&dir, &e.name)) {
                if let Some(parent) = r.host.parent() {
                    layers.push((e.layer.clone(), parent.to_path_buf()));
                }
            }
        }

        let mut dots: Vec<WIN32_FIND_DATAW> = Vec::new();
        let mut items: Vec<WIN32_FIND_DATAW> = Vec::new();
        let mut contributors = 0;
        let mut first_error = None;
        for (layer, host_dir) in &layers {
            // built by hand: a pattern such as `*.` must reach the host unnormalised, and outside the
            // `\\?\` form when it fits (the host applies its DOS wildcard rules only there)
            let mut query = host::host_path(host_dir);
            query.pop();
            if query.len() + pattern.len() < 250 && query.starts_with(&[92, 92, 63, 92]) && query.get(5) == Some(&58) {
                query.drain(..4);
            }
            if query.last() != Some(&u16::from(b'\\')) {
                query.push(u16::from(b'\\'));
            }
            query.extend(pattern.encode_utf16());
            query.push(0);
            let mut d = WIN32_FIND_DATAW::default();
            let h = unsafe {
                host::FindFirstFileExW(query.as_ptr(), level, &mut d as *mut _ as *mut c_void, op, std::ptr::null(), flags)
            };
            if h == INVALID_HANDLE_VALUE {
                let e = unsafe { host::GetLastError() };
                if e != ERROR_FILE_NOT_FOUND && e != ERROR_NO_MORE_FILES {
                    first_error.get_or_insert(e);
                }
                continue;
            }
            let mut gave = false;
            loop {
                let name = d.name();
                if name == "." || name == ".." {
                    if !is_root && !dots.iter().any(|x| x.name() == name) {
                        dots.push(d);
                    }
                } else if winner.get(&fold(&name)) == Some(layer) {
                    items.push(d);
                    gave = true;
                }
                if unsafe { host::FindNextFileW(h, &mut d) } == 0 {
                    break;
                }
            }
            unsafe { host::FindClose(h) };
            contributors += gave as usize;
        }
        if contributors > 1 {
            items.sort_by_cached_key(|d| d.name().to_uppercase());
        }
        dots.extend(items);
        if dots.is_empty() {
            return Err(first_error.unwrap_or(ERROR_FILE_NOT_FOUND));
        }
        Ok(dots)
    }
}

fn to_ansi(d: &WIN32_FIND_DATAW) -> WIN32_FIND_DATAA {
    let mut a = WIN32_FIND_DATAA {
        dwFileAttributes: d.dwFileAttributes,
        ftCreationTime: d.ftCreationTime,
        ftLastAccessTime: d.ftLastAccessTime,
        ftLastWriteTime: d.ftLastWriteTime,
        nFileSizeHigh: d.nFileSizeHigh,
        nFileSizeLow: d.nFileSizeLow,
        dwReserved0: d.dwReserved0,
        dwReserved1: d.dwReserved1,
        ..Default::default()
    };
    let name = host::to_ansi(&d.name());
    let n = name.len().min(259);
    a.cFileName[..n].copy_from_slice(&name[..n]);
    let alt_end = d.cAlternateFileName.iter().position(|&c| c == 0).unwrap_or(14);
    let alt = host::to_ansi(&String::from_utf16_lossy(&d.cAlternateFileName[..alt_end]));
    let n = alt.len().min(13);
    a.cAlternateFileName[..n].copy_from_slice(&alt[..n]);
    a
}

/// Runs the search; returns our handle with the first entry, the rest kept for `FindNextFile*`.
fn first(function: &'static str, raw: &str, level: i32, op: i32, flags: u32) -> Result<(HANDLE, WIN32_FIND_DATAW), u32> {
    if !(0..=1).contains(&level) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let Some(files) = crate::installed() else {
        discover(function, raw, "no file system installed");
        return Err(crate::ERROR_NOT_READY);
    };
    let mut all: VecDeque<_> = files.search(function, raw, level, op, flags)?.into();
    let d = all.pop_front().expect("search is never empty");
    let h = searches(|s| {
        let h = SEARCH_BASE + s.next * 0x10;
        s.next += 1;
        s.open.insert(h, all);
        h
    });
    Ok((h as HANDLE, d))
}

fn first_w(function: &'static str, raw: &str, level: i32, data: *mut c_void, op: i32, flags: u32) -> HANDLE {
    if data.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    }
    match first(function, raw, level, op, flags) {
        Ok((h, d)) => {
            unsafe { *(data as *mut WIN32_FIND_DATAW) = d };
            h
        }
        Err(e) => {
            set_last_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

fn first_a(function: &'static str, raw: &str, level: i32, data: *mut c_void, op: i32, flags: u32) -> HANDLE {
    if data.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    }
    match first(function, raw, level, op, flags) {
        Ok((h, d)) => {
            unsafe { *(data as *mut WIN32_FIND_DATAA) = to_ansi(&d) };
            h
        }
        Err(e) => {
            set_last_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "system" fn FindFirstFileExW(
    name: *const u16,
    level: i32,
    data: *mut c_void,
    op: i32,
    _filter: *const c_void,
    flags: u32,
) -> HANDLE {
    let raw = host::from_wide(name).unwrap_or_default();
    first_w("FindFirstFileExW", &raw, level, data, op, flags)
}

pub unsafe extern "system" fn FindFirstFileW(name: *const u16, data: *mut WIN32_FIND_DATAW) -> HANDLE {
    let raw = host::from_wide(name).unwrap_or_default();
    first_w("FindFirstFileW", &raw, 0, data as *mut c_void, 0, 0)
}

pub unsafe extern "system" fn FindFirstFileExA(
    name: *const u8,
    level: i32,
    data: *mut c_void,
    op: i32,
    _filter: *const c_void,
    flags: u32,
) -> HANDLE {
    let raw = host::from_ansi(name).unwrap_or_default();
    first_a("FindFirstFileExA", &raw, level, data, op, flags)
}

pub unsafe extern "system" fn FindFirstFileA(name: *const u8, data: *mut WIN32_FIND_DATAA) -> HANDLE {
    let raw = host::from_ansi(name).unwrap_or_default();
    first_a("FindFirstFileA", &raw, 0, data as *mut c_void, 0, 0)
}

fn next(h: HANDLE) -> Option<Result<WIN32_FIND_DATAW, u32>> {
    if !is_ours(h) {
        return None;
    }
    Some(searches(|s| match s.open.get_mut(&(h as usize)) {
        Some(q) => q.pop_front().ok_or(ERROR_NO_MORE_FILES),
        None => Err(crate::ERROR_INVALID_HANDLE),
    }))
}

pub unsafe extern "system" fn FindNextFileW(h: HANDLE, data: *mut WIN32_FIND_DATAW) -> i32 {
    match next(h) {
        None => host::FindNextFileW(h, data),
        Some(Ok(d)) => {
            *data = d;
            1
        }
        Some(Err(e)) => {
            set_last_error(e);
            0
        }
    }
}

pub unsafe extern "system" fn FindNextFileA(h: HANDLE, data: *mut WIN32_FIND_DATAA) -> i32 {
    match next(h) {
        None => host::FindNextFileA(h, data),
        Some(Ok(d)) => {
            *data = to_ansi(&d);
            1
        }
        Some(Err(e)) => {
            set_last_error(e);
            0
        }
    }
}

pub unsafe extern "system" fn FindClose(h: HANDLE) -> i32 {
    if !is_ours(h) {
        return host::FindClose(h);
    }
    if searches(|s| s.open.remove(&(h as usize))).is_some() {
        1
    } else {
        set_last_error(crate::ERROR_INVALID_HANDLE);
        0
    }
}
