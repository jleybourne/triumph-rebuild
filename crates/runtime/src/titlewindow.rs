//! The title's own windows never appear: the frame is presented into ours (`present.rs`), so the title's window
//! (class `sgaWindowClass`, titled "Americas") would be a second, empty (white) window on the desktop.
//!
//! **Message-only.** The title's `CreateWindowExW`/`A` make every top-level window it asks for (no `WS_CHILD`, no
//! parent) as a *message-only* window: parent `HWND_MESSAGE`, without `WS_VISIBLE`. Windows never shows a
//! message-only window, whatever is called on it later; it still has its thread's message queue, so the title's
//! message loop, `SendMessage`/`PostMessage` and its window procedure work as before. (A title's child windows
//! are children of that window, so invisible too.) If the system refuses the message-only form, the window is
//! made as asked but hidden, and the overrides below keep it hidden.
//!
//! **Every route to showing one.** The handle is remembered, and on those windows:
//! - `ShowWindow`, `ShowWindowAsync`: any show becomes `SW_HIDE`;
//! - `SetWindowPos`, `DeferWindowPos`: `SWP_SHOWWINDOW` becomes `SWP_HIDEWINDOW`;
//! - `SetWindowLongPtrW`/`A`, `SetWindowLongW`/`A`: a `GWL_STYLE` with `WS_VISIBLE` loses it (a style change is how
//!   a title switches window modes);
//! - `SetParent` to the desktop (which would make it top-level again) keeps it message-only.
//!
//! The title reaches these by import name (any DLL: `user32` or an API set), by `user32` ordinal, through a
//! delay-load, or with `GetProcAddress`: the import resolver and our `GetProcAddress` bind ours by name **and by the
//! host address** a lookup lands on ([`override_host_address`]), so an ordinal or a forwarder gets ours too.
//! Input does not need the title's window: ours is the visible one, and the input layer reads the keyboard while
//! any window of this process has the focus. `REBUILD_TITLE_WINDOW=show` keeps the title's own window (shown, as
//! the title asks) for debugging.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CHILD: u32 = 0x4000_0000;
const SW_HIDE: i32 = 0;
const SWP_SHOWWINDOW: u32 = 0x0040;
const SWP_HIDEWINDOW: u32 = 0x0080;
const GWL_STYLE: i32 = -16;
/// The parent of message-only windows.
pub const HWND_MESSAGE: isize = -3;

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
    #[link_name = "DeferWindowPos"]
    fn host_defer_pos(info: isize, hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> isize;
    #[link_name = "SetWindowLongPtrW"]
    fn host_set_long_ptr_w(hwnd: isize, index: i32, value: isize) -> isize;
    #[link_name = "SetWindowLongPtrA"]
    fn host_set_long_ptr_a(hwnd: isize, index: i32, value: isize) -> isize;
    #[link_name = "SetWindowLongW"]
    fn host_set_long_w(hwnd: isize, index: i32, value: i32) -> i32;
    #[link_name = "SetWindowLongA"]
    fn host_set_long_a(hwnd: isize, index: i32, value: i32) -> i32;
    #[link_name = "SetParent"]
    fn host_set_parent(hwnd: isize, parent: isize) -> isize;
    fn GetDesktopWindow() -> isize;
}

/// The title's windows (handles), kept hidden.
static TITLE: Mutex<Vec<isize>> = Mutex::new(Vec::new());
/// How many of them are message-only.
static MESSAGE_ONLY: Mutex<usize> = Mutex::new(0);

fn enabled() -> bool {
    !std::env::var("REBUILD_TITLE_WINDOW").is_ok_and(|v| v.trim().eq_ignore_ascii_case("show"))
}

fn is_title(hwnd: isize) -> bool {
    hwnd != 0 && TITLE.lock().unwrap_or_else(|e| e.into_inner()).contains(&hwnd)
}

fn remember(hwnd: isize, message_only: bool) {
    if hwnd != 0 {
        TITLE.lock().unwrap_or_else(|e| e.into_inner()).push(hwnd);
        if message_only {
            *MESSAGE_ONLY.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        }
    }
}

/// How many windows the title made (none of them shown): the run report.
pub fn count() -> usize {
    TITLE.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// How many of the title's windows are message-only.
pub fn message_only_count() -> usize {
    *MESSAGE_ONLY.lock().unwrap_or_else(|e| e.into_inner())
}

/// The style the title's window is made with: never visible.
pub fn hidden_style(style: u32) -> u32 {
    style & !WS_VISIBLE
}

/// The parent the title's window is made with: a top-level window (no parent, or the desktop, and not a child)
/// becomes message-only; a child keeps its parent.
pub fn message_parent(style: u32, parent: isize, desktop: isize) -> isize {
    if style & WS_CHILD == 0 && (parent == 0 || parent == desktop) {
        HWND_MESSAGE
    } else {
        parent
    }
}

/// `SetWindowPos` flags for a title window: a show request becomes a hide.
pub fn hidden_pos_flags(flags: u32) -> u32 {
    if flags & SWP_SHOWWINDOW != 0 {
        (flags & !SWP_SHOWWINDOW) | SWP_HIDEWINDOW
    } else {
        flags
    }
}

/// A `SetWindowLong(Ptr)` value for a title window: a style keeps everything but `WS_VISIBLE`.
pub fn hidden_long(index: i32, value: isize) -> isize {
    if index == GWL_STYLE {
        value & !(WS_VISIBLE as isize)
    } else {
        value
    }
}

fn desktop() -> isize {
    unsafe { GetDesktopWindow() }
}

/// Make a title window: message-only first, else as asked but hidden.
fn create(style: u32, parent: isize, make: impl Fn(u32, isize) -> isize) -> isize {
    let to = message_parent(style, parent, desktop());
    let hwnd = make(hidden_style(style), to);
    if hwnd != 0 {
        remember(hwnd, to == HWND_MESSAGE);
        return hwnd;
    }
    if to == parent {
        return 0;
    }
    let hwnd = make(hidden_style(style), parent);
    remember(hwnd, false);
    hwnd
}

unsafe extern "system" fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: isize, menu: isize, instance: isize, param: *const c_void) -> isize {
    create(style, parent, |style, parent| unsafe { host_create_w(ex, class, name, style, x, y, w, h, parent, menu, instance, param) })
}

unsafe extern "system" fn CreateWindowExA(ex: u32, class: *const u8, name: *const u8, style: u32, x: i32, y: i32, w: i32, h: i32, parent: isize, menu: isize, instance: isize, param: *const c_void) -> isize {
    create(style, parent, |style, parent| unsafe { host_create_a(ex, class, name, style, x, y, w, h, parent, menu, instance, param) })
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

unsafe extern "system" fn DeferWindowPos(info: isize, hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> isize {
    let flags = if is_title(hwnd) { hidden_pos_flags(flags) } else { flags };
    unsafe { host_defer_pos(info, hwnd, after, x, y, cx, cy, flags) }
}

unsafe extern "system" fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize {
    let value = if is_title(hwnd) { hidden_long(index, value) } else { value };
    unsafe { host_set_long_ptr_w(hwnd, index, value) }
}

unsafe extern "system" fn SetWindowLongPtrA(hwnd: isize, index: i32, value: isize) -> isize {
    let value = if is_title(hwnd) { hidden_long(index, value) } else { value };
    unsafe { host_set_long_ptr_a(hwnd, index, value) }
}

unsafe extern "system" fn SetWindowLongW(hwnd: isize, index: i32, value: i32) -> i32 {
    let value = if is_title(hwnd) { hidden_long(index, value as isize) as i32 } else { value };
    unsafe { host_set_long_w(hwnd, index, value) }
}

unsafe extern "system" fn SetWindowLongA(hwnd: isize, index: i32, value: i32) -> i32 {
    let value = if is_title(hwnd) { hidden_long(index, value as isize) as i32 } else { value };
    unsafe { host_set_long_a(hwnd, index, value) }
}

unsafe extern "system" fn SetParent(hwnd: isize, parent: isize) -> isize {
    let parent = if is_title(hwnd) && (parent == 0 || parent == desktop()) { HWND_MESSAGE } else { parent };
    unsafe { host_set_parent(hwnd, parent) }
}

/// The functions we replace, by name.
const NAMES: &[&str] = &[
    "CreateWindowExW",
    "CreateWindowExA",
    "ShowWindow",
    "ShowWindowAsync",
    "SetWindowPos",
    "DeferWindowPos",
    "SetWindowLongPtrW",
    "SetWindowLongPtrA",
    "SetWindowLongW",
    "SetWindowLongA",
    "SetParent",
];

fn ours(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "CreateWindowExW" => CreateWindowExW as *const (),
        "CreateWindowExA" => CreateWindowExA as *const (),
        "ShowWindow" => ShowWindow as *const (),
        "ShowWindowAsync" => ShowWindowAsync as *const (),
        "SetWindowPos" => SetWindowPos as *const (),
        "DeferWindowPos" => DeferWindowPos as *const (),
        "SetWindowLongPtrW" => SetWindowLongPtrW as *const (),
        "SetWindowLongPtrA" => SetWindowLongPtrA as *const (),
        "SetWindowLongW" => SetWindowLongW as *const (),
        "SetWindowLongA" => SetWindowLongA as *const (),
        "SetParent" => SetParent as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

/// Ours instead of the host's, for the title's imports by name (`REBUILD_TITLE_WINDOW=show`: none).
pub fn override_export(name: &str) -> Option<u64> {
    if !enabled() {
        return None;
    }
    ours(name)
}

/// The host's own addresses of the functions we replace (`user32`'s exports; every API set and ordinal that
/// reaches one of them lands on the same address).
fn host_addresses() -> &'static [(u64, u64)] {
    static A: OnceLock<Vec<(u64, u64)>> = OnceLock::new();
    A.get_or_init(|| {
        use windows::core::{PCSTR, PCWSTR};
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
        let name: Vec<u16> = "user32.dll".encode_utf16().chain(std::iter::once(0)).collect();
        let Ok(user32) = (unsafe { LoadLibraryExW(PCWSTR(name.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }) else { return Vec::new() };
        NAMES
            .iter()
            .filter_map(|n| {
                let c = std::ffi::CString::new(*n).ok()?;
                let host = unsafe { GetProcAddress(user32, PCSTR(c.as_ptr().cast())) }? as usize as u64;
                Some((host, ours(n)?))
            })
            .collect()
    })
}

/// Ours for a host function the title reached by any other route (a `user32` ordinal, an API-set forwarder,
/// `GetProcAddress`): `host` is where the host's lookup landed. `None` for every other function.
pub fn override_host_address(host: u64) -> Option<u64> {
    if !enabled() || host == 0 {
        return None;
    }
    host_addresses().iter().find(|(h, _)| *h == host).map(|(_, o)| *o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_titles_window_is_never_made_or_shown_visible() {
        assert_eq!(hidden_style(0x10cf_0000), 0x00cf_0000, "WS_VISIBLE dropped, the rest kept");
        assert_eq!(hidden_pos_flags(0x0040 | 0x0004), 0x0080 | 0x0004, "show becomes hide");
        assert_eq!(hidden_pos_flags(0x0004), 0x0004, "no show: unchanged");
        assert_eq!(hidden_long(GWL_STYLE, 0x10cf_0000), 0x00cf_0000, "a style change cannot show it");
        assert_eq!(hidden_long(-21, 0x1000_0000), 0x1000_0000, "other indices (user data) unchanged");
        // top-level windows become message-only; children keep their parent
        assert_eq!(message_parent(0x00cf_0000, 0, 0x10010), HWND_MESSAGE);
        assert_eq!(message_parent(0x00cf_0000, 0x10010, 0x10010), HWND_MESSAGE, "the desktop as parent: top-level");
        assert_eq!(message_parent(WS_CHILD, 0x2222, 0x10010), 0x2222);
        for n in NAMES {
            assert!(override_export(n).is_some(), "{n} is ours");
            assert!(crate::override_reason(n).is_some(), "{n} is listed in OVERRIDES");
        }
        assert!(override_export("DestroyWindow").is_none());
    }

    /// The real thing: a window made through our `CreateWindowExW` with `WS_VISIBLE`, then shown every way the
    /// title could, is message-only and never visible; and the user32 ordinal / address route finds ours.
    #[test]
    fn a_title_window_stays_invisible_whatever_is_called_on_it() {
        use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetAncestor, IsWindowVisible, GA_PARENT};
        use windows::Win32::Foundation::HWND;
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "Americas\0".encode_utf16().collect();
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0x10cf_0000, 0, 0, 640, 360, 0, 0, 0, std::ptr::null()) };
        assert_ne!(hwnd, 0, "made");
        let h = HWND(hwnd as *mut c_void);
        let visible = || unsafe { IsWindowVisible(h) }.as_bool();
        assert!(!visible(), "made hidden");
        // a message-only window's parent is the message window, not the desktop
        let parent = unsafe { GetAncestor(h, GA_PARENT) }.0 as isize;
        assert_ne!(parent, unsafe { GetDesktopWindow() }, "message-only, not top-level");
        unsafe {
            ShowWindow(hwnd, 5);
            assert!(!visible(), "ShowWindow(SW_SHOW)");
            SetWindowPos(hwnd, 0, 0, 0, 800, 600, SWP_SHOWWINDOW);
            assert!(!visible(), "SetWindowPos(SWP_SHOWWINDOW)");
            SetWindowLongPtrW(hwnd, GWL_STYLE, 0x10cf_0000);
            assert!(!visible(), "SetWindowLongPtr(GWL_STYLE, WS_VISIBLE)");
            SetParent(hwnd, 0);
            assert!(!visible(), "SetParent(desktop)");
            // even the host's own ShowWindow cannot show a message-only window
            host_show(hwnd, 5);
            host_set_long_ptr_w(hwnd, GWL_STYLE, 0x10cf_0000);
            assert!(!visible(), "the host's ShowWindow on a message-only window");
            let _ = DestroyWindow(h);
        }
        assert!(message_only_count() >= 1);
        // routes other than the name: ours for the host's own address of each function
        for n in NAMES {
            let host = host_addresses().iter().find(|(_, o)| Some(*o) == ours(n)).map(|(h, _)| *h);
            let host = host.unwrap_or_else(|| panic!("{n}: the host's address"));
            assert_eq!(override_host_address(host), ours(n), "{n}");
        }
        assert_eq!(override_host_address(0), None);
    }
}
