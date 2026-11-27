//! XPackage: the title's own installation, complete and launchable, every chunk ready; no other
//! package (content, add-ons) installed. The installation's identifier is the profile's
//! `package_id`; any other identifier is `E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER`.
//! Slot offsets and the meaning of 0x110-0x138 from the fork's design.

use std::ffi::{c_char, c_void};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{handle_of, put, put_str, text, token, InterfaceDef};
use crate::profile;
use crate::xasync::{self, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("af406016-e850-4aa8-a88d-2f3dcb9dac7e");
pub const IID: Guid = Guid::parse("0b9d556e-ab70-43f4-baa0-9999ac4cb040");

unsafe fn is_ours(id: *const c_char) -> bool {
    unsafe { text(id) }.is_some_and(|id| id == profile::current().package_id)
}

/// Every installation monitor watches the same finished installation: one fixed handle.
static MONITOR: u8 = 0;
pub fn monitor() -> *const c_void {
    handle_of(&MONITOR)
}

/// `XPACKAGE_IDENTIFIER_MAX_LENGTH` is 33 bytes.
unsafe extern "system" fn current_identifier(_: This, size: usize, buffer: *mut u8) -> HRESULT {
    unsafe { put_str(&profile::current().package_id, size, buffer, std::ptr::null_mut()) }
}
unsafe extern "system" fn is_packaged(_: This) -> bool {
    true
}
unsafe extern "system" fn create_monitor(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _queue: *const c_void,
    out: *mut *const c_void,
) -> HRESULT {
    if !unsafe { is_ours(id) } {
        return E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER;
    }
    unsafe { put(out, monitor()) }
}
unsafe extern "system" fn close_monitor(_: This, _: *const c_void) {}

/// `XPackageInstallationProgress`, 32 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub total_bytes: u64,
    pub installed_bytes: u64,
    pub launch_bytes: u64,
    pub launchable: bool,
    pub completed: bool,
}

unsafe extern "system" fn progress(_: This, _monitor: *const c_void, out: *mut Progress) {
    let bytes = profile::current().package_bytes;
    let p = Progress { total_bytes: bytes, installed_bytes: bytes, launch_bytes: bytes, launchable: true, completed: true };
    unsafe { put(out, p) };
}
/// Complete installations never change: the callback is never called.
unsafe extern "system" fn register_progress(_: This, _monitor: *const c_void, _context: *mut c_void, _callback: *const c_void, out: *mut u64) -> HRESULT {
    unsafe { put(out, token()) }
}
/// `XPackageChunkAvailability::Ready` (0) for any selector of ours: a full install has every chunk.
unsafe extern "system" fn find_availability(_: This, id: *const c_char, _count: u32, _selectors: *const c_void, out: *mut u32) -> HRESULT {
    if !unsafe { is_ours(id) } {
        return E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER;
    }
    unsafe { put(out, 0) }
}
unsafe extern "system" fn change_order(_: This, id: *const c_char, _count: u32, _selectors: *const c_void) -> HRESULT {
    if unsafe { is_ours(id) } { S_OK } else { E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER }
}

static INSTALL: u8 = 0;
/// Everything is installed: done at once; the result is a monitor.
unsafe extern "system" fn install_chunks_async(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _suppress_confirmation: u8,
    block: *mut XAsyncBlock,
) -> HRESULT {
    let outcome = if unsafe { is_ours(id) } { Ok(xasync::bytes(monitor())) } else { Err(E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER) };
    unsafe { xasync::finish(block, &INSTALL, outcome) }
}
unsafe extern "system" fn install_chunks_result(_: This, block: *mut XAsyncBlock, out: *mut *const c_void) -> HRESULT {
    unsafe { xasync::take(block, &INSTALL, out) }
}
/// Nothing to download, nothing to confirm.
unsafe extern "system" fn estimate_download(_: This, id: *const c_char, _count: u32, _selectors: *const c_void, bytes: *mut u64, confirm: *mut bool) -> HRESULT {
    if !unsafe { is_ours(id) } {
        return E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER;
    }
    if !confirm.is_null() {
        unsafe { confirm.write(false) };
    }
    unsafe { put(bytes, 0) }
}
/// Accepted; nothing is removed (the game folder is read-only to us), so the chunks stay ready.
unsafe extern "system" fn uninstall_chunks(_: This, id: *const c_char, _count: u32, _selectors: *const c_void) -> HRESULT {
    if unsafe { is_ours(id) } { S_OK } else { E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER }
}
/// Mount handles come only from mounting another package; there is none.
unsafe extern "system" fn mount_path(_: This, _mount: *const c_void, _size: usize, _buffer: *mut u8) -> HRESULT {
    E_INVALIDARG
}
unsafe extern "system" fn close_mount(_: This, _mount: *const c_void) {}
/// Only other packages are uninstalled this way, and none is installed.
unsafe extern "system" fn uninstall_package(_: This, id: *const c_char) -> bool {
    let id = unsafe { text(id) }.unwrap_or_default();
    crate::discovery::note(format!("package uninstall refused: {id}"));
    false
}
static MOUNT: u8 = 0;
unsafe extern "system" fn mount_async(_: This, _id: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &MOUNT, Err(E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER)) }
}
unsafe extern "system" fn mount_result(_: This, block: *mut XAsyncBlock, out: *mut *const c_void) -> HRESULT {
    unsafe { xasync::take(block, &MOUNT, out) }
}
/// `XPackageEnumeratePackages`: no content packages, so the callback is never called. (Kind 0, the
/// game itself, also lists nothing: the fork saw the title enumerate content (kind 1) only.)
unsafe extern "system" fn enumerate(_: This, _kind: u32, _scope: u32, _context: *mut c_void, _callback: *const c_void) -> HRESULT {
    S_OK
}

pub static PACKAGE_IFACE: InterfaceDef = InterfaceDef {
    area: "package",
    provider: PROVIDER,
    iids: &[IID],
    len: 40,
    slots: &[
        slot!(0x18, "XPackageGetCurrentProcessPackageIdentifier", current_identifier, "size, *buffer -> HRESULT"),
        slot!(0x20, "XPackageIsPackagedProcess", is_packaged, "-> bool"),
        slot!(0x28, "XPackageCreateInstallationMonitor", create_monitor, "id, selectorCount: u32, *selectors, intervalMs: u32, queue, *monitor -> HRESULT"),
        slot!(0x30, "XPackageCloseInstallationMonitorHandle", close_monitor, "monitor"),
        slot!(0x38, "XPackageGetInstallationProgress", progress, "monitor, *progress"),
        slot!(0x48, "XPackageRegisterInstallationProgressChanged", register_progress, "monitor, context, callback, *token: u64 -> HRESULT"),
        slot!(0x60, "XPackageFindChunkAvailability", find_availability, "id, selectorCount: u32, *selectors, *availability: u32 -> HRESULT"),
        slot!(0x70, "XPackageChangeChunkInstallOrder", change_order, "id, selectorCount: u32, *selectors -> HRESULT"),
        slot!(0x80, "XPackageInstallChunksAsync", install_chunks_async, "id, selectorCount: u32, *selectors, intervalMs: u32, suppressConfirmation: bool, block -> HRESULT"),
        slot!(0x88, "XPackageInstallChunksResult", install_chunks_result, "block, *monitor -> HRESULT"),
        slot!(0x90, "XPackageEstimateDownloadSize", estimate_download, "id, selectorCount: u32, *selectors, *bytes: u64, *shouldPresentConfirmation: bool -> HRESULT"),
        slot!(0x98, "XPackageUninstallChunks", uninstall_chunks, "id, selectorCount: u32, *selectors -> HRESULT"),
        slot!(0xc8, "XPackageGetMountPath", mount_path, "mount, size, *buffer -> HRESULT"),
        slot!(0xd0, "XPackageCloseMountHandle", close_mount, "mount"),
        slot!(0x110, "XPackageUninstallPackage", uninstall_package, "id -> bool"),
        slot!(0x128, "XPackageMountWithUiAsync", mount_async, "id, block -> HRESULT"),
        slot!(0x130, "XPackageMountWithUiResult", mount_result, "block, *mount -> HRESULT"),
        slot!(0x138, "XPackageEnumeratePackages", enumerate, "kind: u32, scope: u32, context, callback -> HRESULT"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};
    use crate::profile::{Profile, TEST_LOCK};

    type Find = unsafe extern "system" fn(This, *const c_char, u32, *const c_void, *mut u32) -> HRESULT;
    type Ids = unsafe extern "system" fn(This, *const c_char, u32, *const c_void) -> HRESULT;

    #[test]
    fn the_installation_is_complete_and_alone() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        profile::configure(Profile { package_id: "RB_TEST_PKG".into(), package_bytes: 123_456, ..Profile::default() }).unwrap();
        let t = this(&PACKAGE_IFACE);
        let ours = c"RB_TEST_PKG".as_ptr();
        let other = c"dlc".as_ptr();
        let none = std::ptr::null();
        unsafe {
            let mut id = [0u8; 33];
            assert_eq!(method::<unsafe extern "system" fn(This, usize, *mut u8) -> HRESULT>(&PACKAGE_IFACE, 0x18)(t, 33, id.as_mut_ptr()), S_OK);
            assert_eq!(&id[..12], b"RB_TEST_PKG\0");
            assert!(method::<unsafe extern "system" fn(This) -> bool>(&PACKAGE_IFACE, 0x20)(t));

            type Create = unsafe extern "system" fn(This, *const c_char, u32, *const c_void, u32, *const c_void, *mut *const c_void) -> HRESULT;
            let mut m = std::ptr::null();
            assert_eq!(method::<Create>(&PACKAGE_IFACE, 0x28)(t, ours, 0, none, 1000, none, &mut m), S_OK);
            assert_eq!(method::<Create>(&PACKAGE_IFACE, 0x28)(t, other, 0, none, 1000, none, &mut m), E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER);
            let mut p = Progress { total_bytes: 0, installed_bytes: 0, launch_bytes: 0, launchable: false, completed: false };
            method::<unsafe extern "system" fn(This, *const c_void, *mut Progress)>(&PACKAGE_IFACE, 0x38)(t, m, &mut p);
            assert_eq!(p, Progress { total_bytes: 123_456, installed_bytes: 123_456, launch_bytes: 123_456, launchable: true, completed: true });
            method::<unsafe extern "system" fn(This, *const c_void)>(&PACKAGE_IFACE, 0x30)(t, m);
            let mut token = 0u64;
            type Reg = unsafe extern "system" fn(This, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
            assert_eq!(method::<Reg>(&PACKAGE_IFACE, 0x48)(t, m, std::ptr::null_mut(), none, &mut token), S_OK);

            let mut a = 9u32;
            assert_eq!(method::<Find>(&PACKAGE_IFACE, 0x60)(t, ours, 0, none, &mut a), S_OK);
            assert_eq!(a, 0, "ready");
            assert_eq!(method::<Find>(&PACKAGE_IFACE, 0x60)(t, other, 0, none, &mut a), E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER);
            assert_eq!(method::<Ids>(&PACKAGE_IFACE, 0x70)(t, ours, 0, none), S_OK);
            assert_eq!(method::<Ids>(&PACKAGE_IFACE, 0x98)(t, ours, 0, none), S_OK);
            assert_eq!(method::<Ids>(&PACKAGE_IFACE, 0x98)(t, other, 0, none), E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER);

            type Est = unsafe extern "system" fn(This, *const c_char, u32, *const c_void, *mut u64, *mut bool) -> HRESULT;
            let (mut bytes, mut confirm) = (1u64, true);
            assert_eq!(method::<Est>(&PACKAGE_IFACE, 0x90)(t, ours, 0, none, &mut bytes, &mut confirm), S_OK);
            assert_eq!((bytes, confirm), (0, false));

            type Install = unsafe extern "system" fn(This, *const c_char, u32, *const c_void, u32, u8, *mut XAsyncBlock) -> HRESULT;
            type Res = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut *const c_void) -> HRESULT;
            let mut b = XAsyncBlock::new(std::ptr::null(), None);
            assert_eq!(method::<Install>(&PACKAGE_IFACE, 0x80)(t, ours, 0, none, 1000, 1, &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), S_OK);
            let mut got = std::ptr::null();
            assert_eq!(method::<Res>(&PACKAGE_IFACE, 0x88)(t, &mut b, &mut got), S_OK);
            assert_eq!(got, monitor());

            let mut path = [0u8; 8];
            assert_eq!(method::<unsafe extern "system" fn(This, *const c_void, usize, *mut u8) -> HRESULT>(&PACKAGE_IFACE, 0xc8)(t, none, 8, path.as_mut_ptr()), E_INVALIDARG);
            method::<unsafe extern "system" fn(This, *const c_void)>(&PACKAGE_IFACE, 0xd0)(t, none);
            assert!(!method::<unsafe extern "system" fn(This, *const c_char) -> bool>(&PACKAGE_IFACE, 0x110)(t, other));
            type Mount = unsafe extern "system" fn(This, *const c_char, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Mount>(&PACKAGE_IFACE, 0x128)(t, other, &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER);
            assert_eq!(method::<Res>(&PACKAGE_IFACE, 0x130)(t, &mut b, &mut got), E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER);
            type Enum = unsafe extern "system" fn(This, u32, u32, *mut c_void, *const c_void) -> HRESULT;
            assert_eq!(method::<Enum>(&PACKAGE_IFACE, 0x138)(t, 1, 0, std::ptr::null_mut(), none), S_OK);
        }
        profile::configure(Profile::default()).unwrap();
    }

    #[test]
    fn progress_layout() {
        assert_eq!(size_of::<Progress>(), 32);
    }
}
