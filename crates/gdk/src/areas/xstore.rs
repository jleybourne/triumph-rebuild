//! XStore offline: the game's own licence is local, active and full (owned, not a trial); the user
//! owns no add-ons or durables; everything that needs the store service (catalog, consumables,
//! purchase and product pages, collections and purchase ids, update checks, downloads) fails with
//! `E_GAMESTORE_NETWORK_ERROR` at once. Slot offsets from the fork's design.

use std::ffi::{c_char, c_void};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{handle_of, put, put_str, text, token, InterfaceDef};
use crate::profile;
use crate::xasync::{self, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("0dd112ac-7c24-448c-b92b-3960fb5bd30c");
pub const IID: Guid = Guid::parse("b09d803c-2414-4a05-82c6-66dfdc9e9a44");
/// The same table extended by durables licensing (0x238, 0x240).
pub const DURABLES_IID: Guid = Guid::parse("2d42fea5-e71d-4b76-97cd-c50afbb3ae5d");

type Handle = *const c_void;

/// Contexts and product queries hold nothing (queries are empty): fixed handles. A licence handle
/// says whether it is valid by which one it is.
static CONTEXT: u8 = 0;
static QUERY: u8 = 0;
static VALID: u8 = 0;
static INVALID: u8 = 0;

/// Result identities, one per result shape.
static FAILED: u8 = 0;
static QUERY_CALL: u8 = 0;
static LICENSE_CALL: u8 = 0;
static GAME_LICENSE: u8 = 0;

unsafe fn offline(block: *mut XAsyncBlock, what: &str) -> HRESULT {
    crate::discovery::note(format!("offline: store {what}"));
    unsafe { xasync::finish(block, &FAILED, Err(E_GAMESTORE_NETWORK_ERROR)) }
}

/// The result functions of calls that can only fail: the call's status.
unsafe extern "system" fn failed_result(_: This, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::get_result(block, (&FAILED as *const u8).cast(), 0, std::ptr::null_mut(), std::ptr::null_mut()) }
}
unsafe extern "system" fn failed_result_out(_: This, block: *mut XAsyncBlock, _out: *mut c_void) -> HRESULT {
    unsafe { failed_result(std::ptr::null(), block) }
}
unsafe extern "system" fn failed_result_buffer(_: This, block: *mut XAsyncBlock, _size: usize, _buffer: *mut c_void) -> HRESULT {
    unsafe { failed_result(std::ptr::null(), block) }
}

unsafe extern "system" fn create_context(_: This, _user: *const c_void, out: *mut Handle) -> HRESULT {
    unsafe { put(out, handle_of(&CONTEXT)) }
}
unsafe extern "system" fn close(_: This, _: Handle) {}

unsafe extern "system" fn associated_products_async(_: This, _: Handle, _kinds: u32, _per_page: u32, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "associated products") }
}
/// The user owns no add-ons: an empty query, from local licences (no service needed).
unsafe extern "system" fn entitled_products_async(_: This, _: Handle, _kinds: u32, _per_page: u32, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &QUERY_CALL, Ok(xasync::bytes(handle_of(&QUERY)))) }
}
/// The result of a product query call: a query handle, or the call's failure.
unsafe extern "system" fn query_result(_: This, block: *mut XAsyncBlock, out: *mut Handle) -> HRESULT {
    let hr = unsafe { xasync::get_status(block, false) };
    if failed(hr) {
        return unsafe { failed_result(std::ptr::null(), block) };
    }
    unsafe { xasync::take(block, &QUERY_CALL, out) }
}
/// Empty: the callback is never called.
unsafe extern "system" fn enumerate_products(_: This, _: Handle, _context: *mut c_void, _callback: *const c_void) -> HRESULT {
    S_OK
}
unsafe extern "system" fn has_more_pages(_: This, _: Handle) -> bool {
    false
}
/// There is no next page.
unsafe extern "system" fn next_page_async(_: This, _: Handle, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &QUERY_CALL, Err(E_ILLEGAL_METHOD_CALL)) }
}

unsafe fn license(owned: bool, block: *mut XAsyncBlock) -> HRESULT {
    let h = handle_of(if owned { &VALID } else { &INVALID });
    unsafe { xasync::finish(block, &LICENSE_CALL, Ok(xasync::bytes(h))) }
}
/// Only the game's own package is installed, so only its licence is valid.
unsafe extern "system" fn license_for_package_async(_: This, _: Handle, package: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    let ours = unsafe { text(package) }.is_some_and(|p| p == profile::current().package_id);
    unsafe { license(ours, block) }
}
/// The user owns no durables.
unsafe extern "system" fn license_for_durables_async(_: This, _: Handle, _store_id: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { license(false, block) }
}
unsafe extern "system" fn license_result(_: This, block: *mut XAsyncBlock, out: *mut Handle) -> HRESULT {
    unsafe { xasync::take(block, &LICENSE_CALL, out) }
}
unsafe extern "system" fn is_license_valid(_: This, license: Handle) -> bool {
    license == handle_of(&VALID)
}

/// `XStoreGameLicense`, 104 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GameLicense {
    pub sku_store_id: [u8; 18],
    pub is_active: bool,
    pub is_trial_owned_by_this_user: bool,
    pub is_disc_license: bool,
    pub is_trial: bool,
    pub trial_time_remaining_s: u32,
    pub trial_unique_id: [u8; 64],
    pub expiration_date: i64,
}

/// A full, active licence for `<store id>/0010` (the store's first SKU), no trial, no expiry.
pub fn game_license() -> GameLicense {
    let mut sku = [0u8; 18];
    let store_id = profile::current().store_id;
    if !store_id.is_empty() {
        let id = format!("{store_id}/0010");
        let n = id.len().min(17);
        sku[..n].copy_from_slice(&id.as_bytes()[..n]);
    }
    GameLicense {
        sku_store_id: sku,
        is_active: true,
        is_trial_owned_by_this_user: false,
        is_disc_license: false,
        is_trial: false,
        trial_time_remaining_s: 0,
        trial_unique_id: [0; 64],
        expiration_date: 0,
    }
}

unsafe extern "system" fn game_license_async(_: This, _: Handle, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &GAME_LICENSE, Ok(xasync::bytes(game_license()))) }
}
unsafe extern "system" fn game_license_result(_: This, block: *mut XAsyncBlock, out: *mut GameLicense) -> HRESULT {
    unsafe { xasync::take(block, &GAME_LICENSE, out) }
}

unsafe extern "system" fn consumable_balance_async(_: This, _: Handle, _store_id: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "consumable balance") }
}
unsafe extern "system" fn report_fulfillment_async(_: This, _: Handle, _store_id: *const c_char, _quantity: u32, _tracking_id: *const c_void, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "consumable fulfillment") }
}
unsafe extern "system" fn user_id_async(_: This, _: Handle, _ticket: *const c_char, _publisher_user_id: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "user store id") }
}
unsafe extern "system" fn purchase_ui_async(_: This, _: Handle, _store_id: *const c_char, _name: *const c_char, _json: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "purchase page") }
}
unsafe extern "system" fn redeem_ui_async(_: This, _: Handle, _token: *const c_char, _allowed: *const *const c_char, _count: usize, _disallow_csv: u8, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "redeem page") }
}
unsafe extern "system" fn updates_async(_: This, _: Handle, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "update check") }
}
unsafe extern "system" fn install_async(_: This, _: Handle, _ids: *const *const c_char, _count: usize, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "download") }
}
unsafe extern "system" fn product_page_ui_async(_: This, _: Handle, _store_id: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { offline(block, "product page") }
}

/// The game's own Store id maps to its installation; nothing else is installed.
unsafe extern "system" fn package_identifier(_: This, store_id: *const c_char, size: usize, buffer: *mut u8) -> HRESULT {
    let p = profile::current();
    match unsafe { text(store_id) } {
        Some(id) if !p.store_id.is_empty() && id == p.store_id => unsafe { put_str(&p.package_id, size, buffer, std::ptr::null_mut()) },
        _ => E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER,
    }
}

/// Licences never change offline: the callbacks are never called.
unsafe extern "system" fn register(_: This, _: Handle, _queue: *const c_void, _context: *mut c_void, _callback: *const c_void, out: *mut u64) -> HRESULT {
    unsafe { put(out, token()) }
}
unsafe extern "system" fn unregister(_: This, _: Handle, _token: u64, _wait: u8) -> bool {
    true
}

pub static STORE_IFACE: InterfaceDef = InterfaceDef {
    area: "store",
    provider: PROVIDER,
    iids: &[IID, DURABLES_IID],
    len: 76,
    slots: &[
        slot!(0x18, "XStoreCreateContext", create_context, "user, *context -> HRESULT"),
        slot!(0x20, "XStoreCloseContextHandle", close, "context"),
        slot!(0x28, "XStoreQueryAssociatedProductsAsync", associated_products_async, "context, kinds: u32, perPage: u32, block -> HRESULT"),
        slot!(0x30, "XStoreQueryAssociatedProductsResult", query_result, "block, *query -> HRESULT"),
        slot!(0x48, "XStoreQueryEntitledProductsAsync", entitled_products_async, "context, kinds: u32, perPage: u32, block -> HRESULT"),
        slot!(0x50, "XStoreQueryEntitledProductsResult", query_result, "block, *query -> HRESULT"),
        slot!(0x78, "XStoreEnumerateProductsQuery", enumerate_products, "query, context, callback -> HRESULT"),
        slot!(0x80, "XStoreProductsQueryHasMorePages", has_more_pages, "query -> bool"),
        slot!(0x88, "XStoreProductsQueryNextPageAsync", next_page_async, "query, block -> HRESULT"),
        slot!(0x90, "XStoreProductsQueryNextPageResult", query_result, "block, *query -> HRESULT"),
        slot!(0x98, "XStoreCloseProductsQueryHandle", close, "query"),
        slot!(0xa0, "XStoreAcquireLicenseForPackageAsync", license_for_package_async, "context, packageId, block -> HRESULT"),
        slot!(0xa8, "XStoreAcquireLicenseForPackageResult", license_result, "block, *license -> HRESULT"),
        slot!(0xb0, "XStoreIsLicenseValid", is_license_valid, "license -> bool"),
        slot!(0xb8, "XStoreCloseLicenseHandle", close, "license"),
        slot!(0xe0, "XStoreQueryGameLicenseAsync", game_license_async, "context, block -> HRESULT"),
        slot!(0xe8, "XStoreQueryGameLicenseResult", game_license_result, "block, *XStoreGameLicense -> HRESULT"),
        slot!(0x108, "XStoreQueryConsumableBalanceRemainingAsync", consumable_balance_async, "context, storeId, block -> HRESULT"),
        slot!(0x110, "XStoreQueryConsumableBalanceRemainingResult", failed_result_out, "block, *result -> HRESULT"),
        slot!(0x118, "XStoreReportConsumableFulfillmentAsync", report_fulfillment_async, "context, storeId, quantity: u32, trackingId: GUID, block -> HRESULT"),
        slot!(0x128, "XStoreGetUserCollectionsIdAsync", user_id_async, "context, serviceTicket, publisherUserId, block -> HRESULT"),
        slot!(0x130, "XStoreGetUserCollectionsIdResultSize", failed_result_out, "block, *size -> HRESULT"),
        slot!(0x138, "XStoreGetUserCollectionsIdResult", failed_result_buffer, "block, size, *buffer -> HRESULT"),
        slot!(0x140, "XStoreGetUserPurchaseIdAsync", user_id_async, "context, serviceTicket, publisherUserId, block -> HRESULT"),
        slot!(0x148, "XStoreGetUserPurchaseIdResultSize", failed_result_out, "block, *size -> HRESULT"),
        slot!(0x150, "XStoreGetUserPurchaseIdResult", failed_result_buffer, "block, size, *buffer -> HRESULT"),
        slot!(0x188, "XStoreShowPurchaseUIAsync", purchase_ui_async, "context, storeId, name, extendedJson, block -> HRESULT"),
        slot!(0x1a8, "XStoreShowRedeemTokenUIAsync", redeem_ui_async, "context, token, *allowedStoreIds, count, disallowCsvRedemption: bool, block -> HRESULT"),
        slot!(0x1b8, "XStoreQueryGameAndDlcPackageUpdatesAsync", updates_async, "context, block -> HRESULT"),
        slot!(0x1c0, "XStoreQueryGameAndDlcPackageUpdatesResultCount", failed_result_out, "block, *count: u32 -> HRESULT"),
        slot!(0x1c8, "XStoreQueryGameAndDlcPackageUpdatesResult", failed_result_buffer, "block, count: u32, *updates -> HRESULT"),
        slot!(0x1e0, "XStoreDownloadAndInstallPackageUpdatesAsync", install_async, "context, *ids, count, block -> HRESULT"),
        slot!(0x1e8, "XStoreDownloadAndInstallPackageUpdatesResult", failed_result, "block -> HRESULT"),
        slot!(0x1f0, "XStoreDownloadAndInstallPackagesAsync", install_async, "context, *storeIds, count, block -> HRESULT"),
        slot!(0x1f8, "XStoreDownloadAndInstallPackagesResultCount", failed_result_out, "block, *count: u32 -> HRESULT"),
        slot!(0x200, "XStoreDownloadAndInstallPackagesResult", failed_result_buffer, "block, count: u32, *packageIds -> HRESULT"),
        slot!(0x208, "XStoreQueryPackageIdentifier", package_identifier, "storeId, size, *packageId -> HRESULT"),
        slot!(0x210, "XStoreRegisterGameLicenseChanged", register, "context, queue, context, callback, *token: u64 -> HRESULT"),
        slot!(0x218, "XStoreUnregisterGameLicenseChanged", unregister, "context, token: u64, wait: bool -> bool"),
        slot!(0x220, "XStoreRegisterPackageLicenseLost", register, "license, queue, context, callback, *token: u64 -> HRESULT"),
        slot!(0x228, "XStoreUnregisterPackageLicenseLost", unregister, "license, token: u64, wait: bool -> bool"),
        slot!(0x238, "XStoreAcquireLicenseForDurablesAsync", license_for_durables_async, "context, storeId, block -> HRESULT"),
        slot!(0x240, "XStoreAcquireLicenseForDurablesResult", license_result, "block, *license -> HRESULT"),
        slot!(0x258, "XStoreShowProductPageUIAsync", product_page_ui_async, "context, storeId, block -> HRESULT"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};
    use crate::profile::{Profile, TEST_LOCK};

    type Begin = unsafe extern "system" fn(This, Handle, *mut XAsyncBlock) -> HRESULT;
    type HandleResult = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut Handle) -> HRESULT;
    type Licensed = unsafe extern "system" fn(This, Handle, *const c_char, *mut XAsyncBlock) -> HRESULT;
    type Valid = unsafe extern "system" fn(This, Handle) -> bool;

    fn block() -> XAsyncBlock {
        XAsyncBlock::new(std::ptr::null(), None)
    }

    #[test]
    fn the_game_is_owned() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        profile::configure(Profile { store_id: "9NTESTSTORE1".into(), package_id: "RB_PKG".into(), ..Profile::default() }).unwrap();
        assert_eq!(size_of::<GameLicense>(), 104);
        let t = this(&STORE_IFACE);
        unsafe {
            let mut ctx = std::ptr::null();
            type Create = unsafe extern "system" fn(This, *const c_void, *mut Handle) -> HRESULT;
            assert_eq!(method::<Create>(&STORE_IFACE, 0x18)(t, std::ptr::null(), &mut ctx), S_OK);
            let mut b = block();
            assert_eq!(method::<Begin>(&STORE_IFACE, 0xe0)(t, ctx, &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), S_OK);
            let mut lic = std::mem::MaybeUninit::<GameLicense>::zeroed();
            type Lic = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut GameLicense) -> HRESULT;
            assert_eq!(method::<Lic>(&STORE_IFACE, 0xe8)(t, &mut b, lic.as_mut_ptr()), S_OK);
            let lic = lic.assume_init();
            assert!(lic.is_active && !lic.is_trial && !lic.is_trial_owned_by_this_user);
            assert_eq!(&lic.sku_store_id[..17], b"9NTESTSTORE1/0010");

            // Our package's licence is valid; a durable's is not.
            let mut l = std::ptr::null();
            assert_eq!(method::<Licensed>(&STORE_IFACE, 0xa0)(t, ctx, c"RB_PKG".as_ptr(), &mut b), S_OK);
            assert_eq!(method::<HandleResult>(&STORE_IFACE, 0xa8)(t, &mut b, &mut l), S_OK);
            assert!(method::<Valid>(&STORE_IFACE, 0xb0)(t, l));
            method::<unsafe extern "system" fn(This, Handle)>(&STORE_IFACE, 0xb8)(t, l);
            assert_eq!(method::<Licensed>(&STORE_IFACE, 0x238)(t, ctx, c"9DURABLE".as_ptr(), &mut b), S_OK);
            assert_eq!(method::<HandleResult>(&STORE_IFACE, 0x240)(t, &mut b, &mut l), S_OK);
            assert!(!method::<Valid>(&STORE_IFACE, 0xb0)(t, l));

            // No add-ons: an empty query with no more pages.
            type Query = unsafe extern "system" fn(This, Handle, u32, u32, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Query>(&STORE_IFACE, 0x48)(t, ctx, 0xff, 25, &mut b), S_OK);
            let mut q = std::ptr::null();
            assert_eq!(method::<HandleResult>(&STORE_IFACE, 0x50)(t, &mut b, &mut q), S_OK);
            assert!(!q.is_null());
            type Enum = unsafe extern "system" fn(This, Handle, *mut c_void, *const c_void) -> HRESULT;
            assert_eq!(method::<Enum>(&STORE_IFACE, 0x78)(t, q, std::ptr::null_mut(), std::ptr::null()), S_OK);
            assert!(!method::<unsafe extern "system" fn(This, Handle) -> bool>(&STORE_IFACE, 0x80)(t, q));
            type Next = unsafe extern "system" fn(This, Handle, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Next>(&STORE_IFACE, 0x88)(t, q, &mut b), S_OK);
            assert_eq!(method::<HandleResult>(&STORE_IFACE, 0x90)(t, &mut b, &mut q), E_ILLEGAL_METHOD_CALL);

            // Store id -> our package identifier.
            type Pid = unsafe extern "system" fn(This, *const c_char, usize, *mut u8) -> HRESULT;
            let mut id = [0u8; 33];
            assert_eq!(method::<Pid>(&STORE_IFACE, 0x208)(t, c"9NTESTSTORE1".as_ptr(), 33, id.as_mut_ptr()), S_OK);
            assert_eq!(&id[..7], b"RB_PKG\0");
            assert_eq!(method::<Pid>(&STORE_IFACE, 0x208)(t, c"9OTHER".as_ptr(), 33, id.as_mut_ptr()), E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER);

            let mut token = 0;
            type Reg = unsafe extern "system" fn(This, Handle, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
            assert_eq!(method::<Reg>(&STORE_IFACE, 0x210)(t, ctx, std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), &mut token), S_OK);
            assert!(method::<unsafe extern "system" fn(This, Handle, u64, u8) -> bool>(&STORE_IFACE, 0x218)(t, ctx, token, 1));
        }
        profile::configure(Profile::default()).unwrap();
    }

    /// Every call that needs the service fails cleanly, through its result function too.
    #[test]
    fn the_service_is_offline() {
        let t = this(&STORE_IFACE);
        let ctx = handle_of(&CONTEXT);
        let s = c"9X".as_ptr();
        let n: *const c_char = std::ptr::null();
        type Status = unsafe extern "system" fn(This, *mut XAsyncBlock, *mut c_void) -> HRESULT;
        let check = |b: &mut XAsyncBlock, result_slot: usize| unsafe {
            assert_eq!(xasync::get_status(b, true), E_GAMESTORE_NETWORK_ERROR);
            let mut out = [0u8; 64];
            assert_eq!(method::<Status>(&STORE_IFACE, result_slot)(t, b, out.as_mut_ptr().cast()), E_GAMESTORE_NETWORK_ERROR);
        };
        unsafe {
            let mut b = block();
            type Query = unsafe extern "system" fn(This, Handle, u32, u32, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Query>(&STORE_IFACE, 0x28)(t, ctx, 0xff, 25, &mut b), S_OK);
            check(&mut b, 0x30);
            assert_eq!(method::<Licensed>(&STORE_IFACE, 0x108)(t, ctx, s, &mut b), S_OK);
            check(&mut b, 0x110);
            type Fulfil = unsafe extern "system" fn(This, Handle, *const c_char, u32, *const c_void, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Fulfil>(&STORE_IFACE, 0x118)(t, ctx, s, 1, std::ptr::null(), &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), E_GAMESTORE_NETWORK_ERROR);
            type Ids = unsafe extern "system" fn(This, Handle, *const c_char, *const c_char, *mut XAsyncBlock) -> HRESULT;
            for (begin, size, result) in [(0x128, 0x130, 0x138), (0x140, 0x148, 0x150)] {
                assert_eq!(method::<Ids>(&STORE_IFACE, begin)(t, ctx, s, s, &mut b), S_OK);
                check(&mut b, size);
                type Buf = unsafe extern "system" fn(This, *mut XAsyncBlock, usize, *mut c_void) -> HRESULT;
                assert_eq!(method::<Buf>(&STORE_IFACE, result)(t, &mut b, 0, std::ptr::null_mut()), E_GAMESTORE_NETWORK_ERROR);
            }
            type Purchase = unsafe extern "system" fn(This, Handle, *const c_char, *const c_char, *const c_char, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Purchase>(&STORE_IFACE, 0x188)(t, ctx, s, n, n, &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), E_GAMESTORE_NETWORK_ERROR);
            type Redeem = unsafe extern "system" fn(This, Handle, *const c_char, *const *const c_char, usize, u8, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Redeem>(&STORE_IFACE, 0x1a8)(t, ctx, s, std::ptr::null(), 0, 0, &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), E_GAMESTORE_NETWORK_ERROR);
            assert_eq!(method::<Begin>(&STORE_IFACE, 0x1b8)(t, ctx, &mut b), S_OK);
            check(&mut b, 0x1c0);
            type Install = unsafe extern "system" fn(This, Handle, *const *const c_char, usize, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Install>(&STORE_IFACE, 0x1e0)(t, ctx, std::ptr::null(), 0, &mut b), S_OK);
            assert_eq!(method::<unsafe extern "system" fn(This, *mut XAsyncBlock) -> HRESULT>(&STORE_IFACE, 0x1e8)(t, &mut b), E_GAMESTORE_NETWORK_ERROR);
            assert_eq!(method::<Install>(&STORE_IFACE, 0x1f0)(t, ctx, std::ptr::null(), 0, &mut b), S_OK);
            check(&mut b, 0x1f8);
            assert_eq!(method::<Licensed>(&STORE_IFACE, 0x258)(t, ctx, s, &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), E_GAMESTORE_NETWORK_ERROR);
        }
        assert!(crate::discovery::contains("offline: store update check"));
    }
}
