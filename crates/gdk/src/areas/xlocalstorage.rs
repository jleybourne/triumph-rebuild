//! XPersistentLocalStorage: the title's own storage, a drive of its own (`S:`, overlay only, kept
//! between runs; `crates/runtime/src/drives.rs`) with the quota its manifest declares. The path the
//! title gets is that drive's root, never a host folder: titles keep what precedes the path's last
//! `:\` and open files under it (the fork's note), so the drive is what they must see.
//!
//! Slots 0x18-0x38 are the public header's five functions in order. The fork served the same layout
//! for both interface ids and noted the title (2026) calling 0x20 (path) and 0x28 (space); its run
//! logs show the storage drive mounted shortly after `InitializeApiImplEx2` in 239 of 242 runs (the
//! fork mounts it only inside the path slots), so 0x18 or 0x20 is called at start.

use std::path::Path;

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put, InterfaceDef};
use crate::profile;
use crate::xasync::{self, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("f4faf4d4-2d04-4fce-b3e0-474a713a3e84");
/// The interface id this title asks for (its table; our first run, `runs/build3-reached.txt`).
pub const IID: Guid = Guid::parse("41a4e10c-5a7e-41d9-8c37-37bde62a07d6");
/// The interface id older titles ask for (the fork's; same slots).
pub const IID_OLDER: Guid = Guid::parse("d29411df-0794-4553-8b27-95fc02d0f75d");

/// `S:\`.
fn root() -> String {
    format!("{}:\\", profile::current().storage_drive)
}

/// `XPersistentLocalStorageGetPathSize`: the root and its NUL.
unsafe extern "system" fn path_size(_: This, out: *mut usize) -> HRESULT {
    unsafe { put(out, root().len() + 1) }
}

/// `XPersistentLocalStorageGetPath`. A buffer too small fails with `E_FAIL`: the public reference page
/// gives `E_FAIL` "for Xbox devices" (PC: `ERROR_INSUFFICIENT_BUFFER`); nothing is written then.
unsafe extern "system" fn path(_: This, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let root = root();
    if buffer.is_null() {
        return E_INVALIDARG;
    }
    if size <= root.len() {
        return E_FAIL;
    }
    unsafe {
        buffer.copy_from_nonoverlapping(root.as_ptr(), root.len());
        buffer.add(root.len()).write(0);
        if !used.is_null() {
            used.write_unaligned(root.len() + 1);
        }
    }
    S_OK
}

/// `XPersistentLocalStorageSpaceInfo`, 32 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceInfo {
    /// Writable now: the quota's rest, as far as the host disk allows.
    pub available_free_bytes: u64,
    /// Left in the quota.
    pub total_free_bytes: u64,
    pub used_bytes: u64,
    /// The quota.
    pub total_bytes: u64,
}

fn used_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => used_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// What the host volume holding `dir` has free for us; unlimited if unknown.
fn disk_free(dir: &Path) -> u64 {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(dir: *const u16, free: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
        }
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        let null = std::ptr::null_mut();
        // SAFETY: a NUL-terminated path and one valid output.
        if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, null, null) } != 0 {
            return free;
        }
    }
    let _ = dir;
    u64::MAX
}

pub fn space() -> SpaceInfo {
    let p = profile::current();
    let (used, disk) = match &p.storage_host {
        Some(dir) => (used_bytes(dir), disk_free(dir)),
        None => (0, u64::MAX),
    };
    let free = p.storage_bytes.saturating_sub(used);
    SpaceInfo { available_free_bytes: free.min(disk), total_free_bytes: free, used_bytes: used, total_bytes: p.storage_bytes }
}

/// `XPersistentLocalStorageGetSpaceInfo`.
unsafe extern "system" fn space_info(_: This, out: *mut SpaceInfo) -> HRESULT {
    unsafe { put(out, space()) }
}

static PROMPT: u8 = 0;

/// `XPersistentLocalStoragePromptUserForSpaceAsync`: there is nobody to ask, and no space to free
/// beyond the quota. Granted (`S_OK`) when the quota's rest covers `bytes`, else declined as a user
/// who cancels (`E_ABORT`), at once.
unsafe extern "system" fn prompt_for_space(_: This, bytes: u64, block: *mut XAsyncBlock) -> HRESULT {
    let outcome = if bytes <= space().total_free_bytes { Ok(Vec::new()) } else { Err(E_ABORT) };
    unsafe { xasync::finish(block, &PROMPT, outcome) }
}

/// `XPersistentLocalStoragePromptUserForSpaceResult`: the call's status.
unsafe extern "system" fn prompt_result(_: This, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::get_result(block, (&PROMPT as *const u8).cast(), 0, std::ptr::null_mut(), std::ptr::null_mut()) }
}

pub static LOCAL_STORAGE_IFACE: InterfaceDef = InterfaceDef {
    area: "local storage",
    provider: PROVIDER,
    iids: &[IID, IID_OLDER],
    len: 8,
    slots: &[
        slot!(0x18, "XPersistentLocalStorageGetPathSize", path_size, "*pathSize: usize -> HRESULT"),
        slot!(0x20, "XPersistentLocalStorageGetPath", path, "pathSize, *path: char, *pathUsed -> HRESULT"),
        slot!(0x28, "XPersistentLocalStorageGetSpaceInfo", space_info, "*info: XPersistentLocalStorageSpaceInfo (32 bytes) -> HRESULT"),
        slot!(0x30, "XPersistentLocalStoragePromptUserForSpaceAsync", prompt_for_space, "requestedBytes: u64, block -> HRESULT"),
        slot!(0x38, "XPersistentLocalStoragePromptUserForSpaceResult", prompt_result, "block -> HRESULT"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Object;
    use crate::profile::{Profile, TEST_LOCK};
    use crate::runtime::QueryApiImpl;
    use std::ffi::c_void;

    type Size = unsafe extern "system" fn(This, *mut usize) -> HRESULT;
    type Get = unsafe extern "system" fn(This, usize, *mut u8, *mut usize) -> HRESULT;
    type Space = unsafe extern "system" fn(This, *mut SpaceInfo) -> HRESULT;
    type Prompt = unsafe extern "system" fn(This, u64, *mut XAsyncBlock) -> HRESULT;
    type PromptResult = unsafe extern "system" fn(This, *mut XAsyncBlock) -> HRESULT;

    /// The object the title gets for the pair it asks with, and its slot at `offset`, read the way
    /// title code reads it (through the object's first word).
    fn queried<F: Copy>(iid: &Guid, offset: usize) -> (This, F) {
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(unsafe { QueryApiImpl(&PROVIDER, iid, &mut out) }, S_OK);
        let f = unsafe { *(*(out as *const *const *const ())).add(offset / 8) };
        assert_eq!(size_of::<F>(), size_of::<*const ()>());
        (out as *const Object, unsafe { std::mem::transmute_copy(&f) })
    }

    #[test]
    fn the_storage_drive_and_its_quota() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rebuild-gdk-pls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("settings")).unwrap();
        std::fs::write(dir.join("save.dat"), [0u8; 100]).unwrap();
        std::fs::write(dir.join("settings").join("a.xml"), [0u8; 50]).unwrap();
        profile::configure(Profile { storage_bytes: 1000, storage_host: Some(dir.clone()), ..Profile::default() }).unwrap();

        for iid in [IID, IID_OLDER] {
            // The path is the drive's root, never the host folder.
            let (t, size) = queried::<Size>(&iid, 0x18);
            let mut n = 0;
            assert_eq!(unsafe { size(t, &mut n) }, S_OK);
            assert_eq!(n, 4, "S:\\ and its NUL");
            assert_eq!(unsafe { size(t, std::ptr::null_mut()) }, E_INVALIDARG);
            let (t, get) = queried::<Get>(&iid, 0x20);
            let (mut buf, mut used) = ([0xffu8; 8], 0);
            assert_eq!(unsafe { get(t, 8, buf.as_mut_ptr(), &mut used) }, S_OK);
            assert_eq!((&buf[..4], used), (&b"S:\\\0"[..], 4));
            let mut small = [0xffu8; 3];
            assert_eq!(unsafe { get(t, 3, small.as_mut_ptr(), &mut used) }, E_FAIL);
            assert_eq!(small, [0xff; 3], "nothing written");

            // Used bytes are the folder's files, nested ones too; the rest of the quota is free.
            let (t, space) = queried::<Space>(&iid, 0x28);
            let mut info = SpaceInfo { available_free_bytes: 0, total_free_bytes: 0, used_bytes: 0, total_bytes: 0 };
            assert_eq!(unsafe { space(t, &mut info) }, S_OK);
            assert_eq!((info.used_bytes, info.total_free_bytes, info.total_bytes, info.available_free_bytes), (150, 850, 1000, 850));

            // Nobody to ask: granted within the quota, declined past it, at once.
            let (t, prompt) = queried::<Prompt>(&iid, 0x30);
            let (_, result) = queried::<PromptResult>(&iid, 0x38);
            for (bytes, expect) in [(0, S_OK), (850, S_OK), (851, E_ABORT)] {
                let mut b = XAsyncBlock::new(std::ptr::null(), None);
                assert_eq!(unsafe { prompt(t, bytes, &mut b) }, S_OK);
                assert_eq!(unsafe { xasync::get_status(&mut b, true) }, expect, "{bytes} bytes");
                assert_eq!(unsafe { result(t, &mut b) }, expect, "{bytes} bytes");
            }
        }
        assert_eq!(size_of::<SpaceInfo>(), 32);

        // Another drive letter is reported as configured; no host folder means nothing used.
        profile::configure(Profile { storage_drive: 'Z', storage_host: None, ..Profile::default() }).unwrap();
        let (t, get) = queried::<Get>(&IID, 0x20);
        let mut buf = [0u8; 4];
        assert_eq!(unsafe { get(t, 4, buf.as_mut_ptr(), std::ptr::null_mut()) }, S_OK);
        assert_eq!(&buf, b"Z:\\\0");
        assert_eq!(space().used_bytes, 0);
        assert_eq!(space().total_free_bytes, 2048 << 20);
        profile::configure(Profile::default()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A slot past the five is not ours to guess.
    #[test]
    fn unknown_slots_are_discovered() {
        let (t, f) = queried::<unsafe extern "system" fn(This) -> HRESULT>(&IID, 0x40);
        assert_eq!(unsafe { f(t) }, E_NOTIMPL);
        assert!(crate::discovery::contains("slot not implemented: local storage 0x40"));
    }
}
