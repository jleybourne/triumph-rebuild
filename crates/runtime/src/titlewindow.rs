//! The title's own windows stay hidden: the frame is presented into ours (`present.rs`), so the title's window
//! (class `sgaWindowClass`, titled "Americas") would be a second, empty window on the desktop.
//!
//! The title's `CreateWindowExW`/`A` make the window as asked but without `WS_VISIBLE`, and its handle is
//! remembered; `ShowWindow`, `ShowWindowAsync` and `SetWindowPos` asking to show one of those windows hide it
//! instead. Messages, focus queries and everything else are the host's. `REBUILD_TITLE_WINDOW=show` keeps the
//! old behaviour (the title's window shown) for debugging.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::Mutex;

const WS_VISIBLE: u32 = 0x1000_0000;
const SW_HIDE: i32 = 0;
const SWP_SHOWWINDOW: u32 = 0x0040;
const SWP_HIDEWINDOW: u32 = 0x0080;

#[link(name = "user32")]
unsafe extern "system" {
    #[link_name = "CreateWindowExW"]
    fn host_create_w(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: isize, menu: isize, instance: isize, param: *const c_void) -> isize;
    #[link_name = "CreateWindowExA"]
    fn host_create_a(ex: u32, class: *const u8, name: *const u8, style: u32, x: i32, y: i32, w: i32, h: i32, parent: isize, menu: isize, instance: isize, param: *const c_void) -> isize;
    #[link_name = "ShowWindow"]
    fn host_show(hwnd: isize, cmd: i32) -> i32;
    #[link_name = "ShowWindowAsync"]
    fn host_show_async(hwnd: isize, cmd: i32) -> i32;
    #[link_name = "SetWindowPos"]
    fn host_set_pos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
}

/// The title's windows (handles), kept hidden.
static TITLE: Mutex<Vec<isize>> = Mutex::new(Vec::new());

fn enabled() -> bool {
    !std::env::var("REBUILD_TITLE_WINDOW").is_ok_and(|v| v.trim().eq_ignore_ascii_case("show"))
}

fn is_title(hwnd: isize) -> bool {
    TITLE.lock().unwrap_or_else(|e| e.into_inner()).contains(&hwnd)
}

fn remember(hwnd: isize) {
    if hwnd != 0 {
        TITLE.lock().unwrap_or_else(|e| e.into_inner()).push(hwnd);
    }
}

/// How many windows the title made (hidden): the run report.
pub fn count() -> usize {
    TITLE.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// The style the title's window is made with: never visible.
pub fn hidden_style(style: u32) -> u32 {
    style & !WS_VISIBLE
}

/// `SetWindowPos` flags for a title window: a show request becomes a hide.
pub fn hidden_pos_flags(flags: u32) -> u32 {
    if flags & SWP_SHOWWINDOW != 0 {
        (flags & !SWP_SHOWWINDOW) | SWP_HIDEWINDOW
    } else {
        flags
    }
}

unsafe extern "system" fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: isize, menu: isize, instance: isize, param: *const c_void) -> isize {
    let hwnd = unsafe { host_create_w(ex, class, name, hidden_style(style), x, y, w, h, parent, menu, instance, param) };
    remember(hwnd);
    hwnd
}

unsafe extern "system" fn CreateWindowExA(ex: u32, class: *const u8, name: *const u8, style: u32, x: i32, y: i32, w: i32, h: i32, parent: isize, menu: isize, instance: isize, param: *const c_void) -> isize {
    let hwnd = unsafe { host_create_a(ex, class, name, hidden_style(style), x, y, w, h, parent, menu, instance, param) };
    remember(hwnd);
    hwnd
}

unsafe extern "system" fn ShowWindow(hwnd: isize, cmd: i32) -> i32 {
    unsafe { host_show(hwnd, if is_title(hwnd) { SW_HIDE } else { cmd }) }
}

unsafe extern "system" fn ShowWindowAsync(hwnd: isize, cmd: i32) -> i32 {
    unsafe { host_show_async(hwnd, if is_title(hwnd) { SW_HIDE } else { cmd }) }
}

unsafe extern "system" fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32 {
    let flags = if is_title(hwnd) { hidden_pos_flags(flags) } else { flags };
    unsafe { host_set_pos(hwnd, after, x, y, cx, cy, flags) }
}

/// Ours instead of the host's, for the title's imports (`REBUILD_TITLE_WINDOW=show`: none).
pub fn override_export(name: &str) -> Option<u64> {
    if !enabled() {
        return None;
    }
    let f: *const () = match name {
        "CreateWindowExW" => CreateWindowExW as *const (),
        "CreateWindowExA" => CreateWindowExA as *const (),
        "ShowWindow" => ShowWindow as *const (),
        "ShowWindowAsync" => ShowWindowAsync as *const (),
        "SetWindowPos" => SetWindowPos as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_titles_window_is_never_made_or_shown_visible() {
        assert_eq!(hidden_style(0x10cf_0000), 0x00cf_0000, "WS_VISIBLE dropped, the rest kept");
        assert_eq!(hidden_pos_flags(0x0040 | 0x0004), 0x0080 | 0x0004, "show becomes hide");
        assert_eq!(hidden_pos_flags(0x0004), 0x0004, "no show: unchanged");
        for n in ["CreateWindowExW", "CreateWindowExA", "ShowWindow", "ShowWindowAsync", "SetWindowPos"] {
            assert!(override_export(n).is_some(), "{n} is ours");
        }
        assert!(override_export("DestroyWindow").is_none());
    }
}
