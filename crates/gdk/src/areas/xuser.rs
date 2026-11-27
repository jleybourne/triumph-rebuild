//! XUser: one local user, signed in, not a guest, adult, every privilege, the gamertag from the
//! profile. Adding a user (any options) yields that user at once. Tokens need the service: they fail.
//! Slot offsets from the fork's design; the title asks with the provider's own id (fork's run notes)
//! and for the gamertag interface.

use std::ffi::{c_char, c_void};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put, put_str, text, token, InterfaceDef};
use crate::profile;
use crate::xasync::{self, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("01acd177-91f9-4763-a38e-ccbb55ce32e0");
pub const GAMERTAG_IID: Guid = Guid::parse("cef4fac0-7676-4a94-a119-4c43f9eb5b74");

/// `XUserLocalId` of the one user.
pub const LOCAL_ID: u64 = 1;

/// The one user's handle: a fixed address. Duplicating and closing are free.
static USER: u8 = 0;
pub type UserHandle = *const c_void;
pub fn user() -> UserHandle {
    crate::object::handle_of(&USER)
}

unsafe fn found(yes: bool, out: *mut UserHandle) -> HRESULT {
    if yes { unsafe { put(out, user()) } } else { E_GAMEUSER_USER_NOT_FOUND }
}

unsafe extern "system" fn duplicate(_: This, _: UserHandle, out: *mut UserHandle) -> HRESULT {
    unsafe { put(out, user()) }
}
unsafe extern "system" fn close(_: This, _: UserHandle) {}
/// `XUserCompare`: 0 for the same user.
unsafe extern "system" fn compare(_: This, a: UserHandle, b: UserHandle) -> i32 {
    i32::from(a != b)
}

static ADD: u8 = 0;
/// `XUserAddAsync`: every option set (silent, with UI, guests allowed) adds the one user.
unsafe extern "system" fn add_async(_: This, _options: u32, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &ADD, Ok(xasync::bytes(user()))) }
}
unsafe extern "system" fn add_result(_: This, block: *mut XAsyncBlock, out: *mut UserHandle) -> HRESULT {
    unsafe { xasync::take(block, &ADD, out) }
}
unsafe extern "system" fn get_local_id(_: This, _: UserHandle, out: *mut u64) -> HRESULT {
    unsafe { put(out, LOCAL_ID) }
}
unsafe extern "system" fn find_by_local_id(_: This, id: u64, out: *mut UserHandle) -> HRESULT {
    unsafe { found(id == LOCAL_ID, out) }
}
unsafe extern "system" fn get_id(_: This, _: UserHandle, out: *mut u64) -> HRESULT {
    unsafe { put(out, profile::current().xuid) }
}
unsafe extern "system" fn find_by_id(_: This, xuid: u64, out: *mut UserHandle) -> HRESULT {
    unsafe { found(xuid == profile::current().xuid, out) }
}
unsafe extern "system" fn get_is_guest(_: This, _: UserHandle, out: *mut bool) -> HRESULT {
    unsafe { put(out, false) }
}
/// `XUserState::SignedIn` (0).
unsafe extern "system" fn get_state(_: This, _: UserHandle, out: *mut u32) -> HRESULT {
    unsafe { put(out, 0) }
}

/// `XUserGamertagComponent`: Classic 0, Modern 1, ModernSuffix 2, UniqueModern 3. A configured
/// gamertag has no suffix: the suffix is empty, the other three are the gamertag.
pub const MODERN_SUFFIX: u32 = 2;
unsafe extern "system" fn get_gamertag(_: This, _: UserHandle, component: u32, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    if component > 3 {
        return E_INVALIDARG;
    }
    let gamertag = profile::current().gamertag;
    let text = if component == MODERN_SUFFIX { "" } else { gamertag.as_str() };
    unsafe { put_str(text, size, buffer, used) }
}
/// `XUserAgeGroup::Adult` (3).
unsafe extern "system" fn get_age_group(_: This, _: UserHandle, out: *mut u32) -> HRESULT {
    unsafe { put(out, 3) }
}
/// Every privilege is granted; deny reason `None` (0).
unsafe extern "system" fn check_privilege(_: This, _: UserHandle, _options: u32, _privilege: u32, has: *mut bool, reason: *mut u32) -> HRESULT {
    if !reason.is_null() {
        unsafe { reason.write_unaligned(0) };
    }
    unsafe { put(has, true) }
}

static RESOLVE: u8 = 0;
/// Nothing to resolve: succeeds at once.
unsafe extern "system" fn resolve_privilege_async(_: This, _: UserHandle, _options: u32, _privilege: u32, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &RESOLVE, Ok(Vec::new())) }
}
unsafe extern "system" fn resolve_privilege_result(_: This, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::get_result(block, (&RESOLVE as *const u8).cast(), 0, std::ptr::null_mut(), std::ptr::null_mut()) }
}

static TOKEN: u8 = 0;
/// `XUserGetTokenAndSignatureAsync`: tokens come from the service, so offline this fails with
/// `E_GAMEUSER_FAILED_TO_GET_TOKEN` (the console's exact offline code is not known).
unsafe extern "system" fn token_async(
    _: This,
    _: UserHandle,
    _options: u32,
    _method: *const c_char,
    url: *const c_char,
    _header_count: usize,
    _headers: *const c_void,
    _body_size: usize,
    _body: *const c_void,
    block: *mut XAsyncBlock,
) -> HRESULT {
    let url = unsafe { text(url) }.unwrap_or_default();
    crate::discovery::note(format!("offline: XUserGetTokenAndSignatureAsync {url}"));
    unsafe { xasync::finish(block, &TOKEN, Err(E_GAMEUSER_FAILED_TO_GET_TOKEN)) }
}
unsafe extern "system" fn token_size(_: This, block: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    unsafe { xasync::get_result_size(block, out) }
}
unsafe extern "system" fn token_result(_: This, block: *mut XAsyncBlock, size: usize, buffer: *mut c_void, _data: *mut *const c_void, used: *mut usize) -> HRESULT {
    unsafe { xasync::get_result(block, (&TOKEN as *const u8).cast(), size, buffer, used) }
}

/// The user never changes: the callback is never called.
unsafe extern "system" fn register(_: This, _queue: *const c_void, _context: *mut c_void, _callback: *const c_void, out: *mut u64) -> HRESULT {
    unsafe { put(out, token()) }
}
unsafe extern "system" fn unregister(_: This, _token: u64, _wait: u8) -> bool {
    true
}

pub static USER_IFACE: InterfaceDef = InterfaceDef {
    area: "user",
    provider: PROVIDER,
    iids: &[PROVIDER],
    len: 35,
    slots: &[
        slot!(0x18, "XUserDuplicateHandle", duplicate, "user, *user -> HRESULT"),
        slot!(0x20, "XUserCloseHandle", close, "user"),
        slot!(0x28, "XUserCompare", compare, "user, user -> i32"),
        slot!(0x38, "XUserAddAsync", add_async, "options: u32, block -> HRESULT"),
        slot!(0x40, "XUserAddResult", add_result, "block, *user -> HRESULT"),
        slot!(0x48, "XUserGetLocalId", get_local_id, "user, *localId: u64 -> HRESULT"),
        slot!(0x50, "XUserFindUserByLocalId", find_by_local_id, "localId: u64, *user -> HRESULT"),
        slot!(0x58, "XUserGetId", get_id, "user, *xuid: u64 -> HRESULT"),
        slot!(0x60, "XUserFindUserById", find_by_id, "xuid: u64, *user -> HRESULT"),
        slot!(0x68, "XUserGetIsGuest", get_is_guest, "user, *bool -> HRESULT"),
        slot!(0x70, "XUserGetState", get_state, "user, *state: u32 -> HRESULT"),
        slot!(0x78, "XUserGetGamertag", get_gamertag, "user, component: u32, size, *buffer, *used -> HRESULT"),
        slot!(0x98, "XUserGetAgeGroup", get_age_group, "user, *ageGroup: u32 -> HRESULT"),
        slot!(0xa0, "XUserCheckPrivilege", check_privilege, "user, options: u32, privilege: u32, *hasPrivilege: bool, *denyReasons: u32 -> HRESULT"),
        slot!(0xa8, "XUserResolvePrivilegeWithUiAsync", resolve_privilege_async, "user, options: u32, privilege: u32, block -> HRESULT"),
        slot!(0xb0, "XUserResolvePrivilegeWithUiResult", resolve_privilege_result, "block -> HRESULT"),
        slot!(0xb8, "XUserGetTokenAndSignatureAsync", token_async, "user, options: u32, method, url, headerCount, *headers, bodySize, *body, block -> HRESULT"),
        slot!(0xc0, "XUserGetTokenAndSignatureResultSize", token_size, "block, *size -> HRESULT"),
        slot!(0xc8, "XUserGetTokenAndSignatureResult", token_result, "block, size, *buffer, **result, *used -> HRESULT"),
        slot!(0x108, "XUserRegisterForChangeEvent", register, "queue, context, callback, *token: u64 -> HRESULT"),
        slot!(0x110, "XUserUnregisterForChangeEvent", unregister, "token: u64, wait: bool -> bool"),
    ],
};

pub static GAMERTAG_IFACE: InterfaceDef = InterfaceDef {
    area: "user gamertag",
    provider: PROVIDER,
    iids: &[GAMERTAG_IID],
    len: 4,
    slots: &[slot!(0x18, "XUserGetGamertag", get_gamertag, "user, component: u32, size, *buffer, *used -> HRESULT")],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};
    use crate::profile::{Profile, TEST_LOCK};

    type Gamertag = unsafe extern "system" fn(This, UserHandle, u32, usize, *mut u8, *mut usize) -> HRESULT;
    type GetU64 = unsafe extern "system" fn(This, UserHandle, *mut u64) -> HRESULT;
    type GetU32 = unsafe extern "system" fn(This, UserHandle, *mut u32) -> HRESULT;
    type Find = unsafe extern "system" fn(This, u64, *mut UserHandle) -> HRESULT;

    #[test]
    fn the_configured_user_signs_in() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        profile::configure(Profile { gamertag: "Rebuilder".into(), xuid: 0x1234, ..Profile::default() }).unwrap();
        let t = this(&USER_IFACE);
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        unsafe {
            type Add = unsafe extern "system" fn(This, u32, *mut XAsyncBlock) -> HRESULT;
            type AddResult = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut UserHandle) -> HRESULT;
            assert_eq!(method::<Add>(&USER_IFACE, 0x38)(t, 1, &mut block), S_OK);
            assert_eq!(xasync::get_status(&mut block, true), S_OK);
            let mut h = std::ptr::null();
            assert_eq!(method::<AddResult>(&USER_IFACE, 0x40)(t, &mut block, &mut h), S_OK);
            assert_eq!(h, user());

            let mut d = std::ptr::null();
            type Dup = unsafe extern "system" fn(This, UserHandle, *mut UserHandle) -> HRESULT;
            assert_eq!(method::<Dup>(&USER_IFACE, 0x18)(t, h, &mut d), S_OK);
            type Cmp = unsafe extern "system" fn(This, UserHandle, UserHandle) -> i32;
            assert_eq!(method::<Cmp>(&USER_IFACE, 0x28)(t, h, d), 0);
            method::<unsafe extern "system" fn(This, UserHandle)>(&USER_IFACE, 0x20)(t, d);

            let mut v = 0u64;
            assert_eq!(method::<GetU64>(&USER_IFACE, 0x58)(t, h, &mut v), S_OK);
            assert_eq!(v, 0x1234);
            assert_eq!(method::<GetU64>(&USER_IFACE, 0x48)(t, h, &mut v), S_OK);
            assert_eq!(v, LOCAL_ID);
            let mut f = std::ptr::null();
            assert_eq!(method::<Find>(&USER_IFACE, 0x60)(t, 0x1234, &mut f), S_OK);
            assert_eq!(method::<Find>(&USER_IFACE, 0x60)(t, 7, &mut f), E_GAMEUSER_USER_NOT_FOUND);
            assert_eq!(method::<Find>(&USER_IFACE, 0x50)(t, LOCAL_ID, &mut f), S_OK);
            assert_eq!(method::<Find>(&USER_IFACE, 0x50)(t, 2, &mut f), E_GAMEUSER_USER_NOT_FOUND);

            let mut guest = true;
            type Guest = unsafe extern "system" fn(This, UserHandle, *mut bool) -> HRESULT;
            assert_eq!(method::<Guest>(&USER_IFACE, 0x68)(t, h, &mut guest), S_OK);
            assert!(!guest);
            let mut s = 9u32;
            assert_eq!(method::<GetU32>(&USER_IFACE, 0x70)(t, h, &mut s), S_OK);
            assert_eq!(s, 0, "signed in");
            assert_eq!(method::<GetU32>(&USER_IFACE, 0x98)(t, h, &mut s), S_OK);
            assert_eq!(s, 3, "adult");

            // Gamertag through both interfaces, every component.
            let expected: [&[u8]; 4] = [b"Rebuilder\0", b"Rebuilder\0", b"\0", b"Rebuilder\0"];
            for (def, at) in [(&GAMERTAG_IFACE, 0x18), (&USER_IFACE, 0x78)] {
                for (c, want) in (0u32..).zip(expected) {
                    let (mut buf, mut used) = ([0xffu8; 16], 0usize);
                    assert_eq!(method::<Gamertag>(def, at)(this(def), h, c, 16, buf.as_mut_ptr(), &mut used), S_OK);
                    assert_eq!(&buf[..used], want, "component {c}");
                }
                let mut buf = [0u8; 4];
                assert_eq!(method::<Gamertag>(def, at)(this(def), h, 0, 4, buf.as_mut_ptr(), std::ptr::null_mut()), E_NOT_SUFFICIENT_BUFFER);
            }

            type Check = unsafe extern "system" fn(This, UserHandle, u32, u32, *mut bool, *mut u32) -> HRESULT;
            let (mut has, mut reason) = (false, 9u32);
            assert_eq!(method::<Check>(&USER_IFACE, 0xa0)(t, h, 0, 254, &mut has, &mut reason), S_OK);
            assert!(has && reason == 0);
            type Resolve = unsafe extern "system" fn(This, UserHandle, u32, u32, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Resolve>(&USER_IFACE, 0xa8)(t, h, 0, 254, &mut block), S_OK);
            assert_eq!(method::<unsafe extern "system" fn(This, *mut XAsyncBlock) -> HRESULT>(&USER_IFACE, 0xb0)(t, &mut block), S_OK);

            let mut token = 0u64;
            type Reg = unsafe extern "system" fn(This, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
            assert_eq!(method::<Reg>(&USER_IFACE, 0x108)(t, std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), &mut token), S_OK);
            assert_ne!(token, 0);
            assert!(method::<unsafe extern "system" fn(This, u64, u8) -> bool>(&USER_IFACE, 0x110)(t, token, 1));
        }
        profile::configure(Profile::default()).unwrap();
    }

    #[test]
    fn tokens_fail_offline() {
        type Token = unsafe extern "system" fn(This, UserHandle, u32, *const c_char, *const c_char, usize, *const c_void, usize, *const c_void, *mut XAsyncBlock) -> HRESULT;
        let t = this(&USER_IFACE);
        let mut block = XAsyncBlock::new(std::ptr::null(), None);
        let null = std::ptr::null();
        unsafe {
            let hr = method::<Token>(&USER_IFACE, 0xb8)(t, user(), 0, c"GET".as_ptr(), c"https://example.invalid/".as_ptr(), 0, null, 0, null, &mut block);
            assert_eq!(hr, S_OK);
            assert_eq!(xasync::get_status(&mut block, true), E_GAMEUSER_FAILED_TO_GET_TOKEN);
            let mut size = 0;
            type Size = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut usize) -> HRESULT;
            assert_eq!(method::<Size>(&USER_IFACE, 0xc0)(t, &mut block, &mut size), E_GAMEUSER_FAILED_TO_GET_TOKEN);
            type Res = unsafe extern "system" fn(This, *mut XAsyncBlock, usize, *mut c_void, *mut *const c_void, *mut usize) -> HRESULT;
            assert_eq!(method::<Res>(&USER_IFACE, 0xc8)(t, &mut block, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()), E_GAMEUSER_FAILED_TO_GET_TOKEN);
        }
        assert!(crate::discovery::contains("offline: XUserGetTokenAndSignatureAsync https://example.invalid/"));
    }
}
