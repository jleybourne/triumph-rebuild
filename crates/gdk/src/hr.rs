//! Result codes. Names are the GDK's (`winerror.h`, `XGameErr.h`); where the console's exact choice
//! for a case is not known, the call site says so.

pub type HRESULT = i32;

const fn hr(v: u32) -> HRESULT {
    v as i32
}

pub const S_OK: HRESULT = 0;
pub const S_FALSE: HRESULT = 1;
pub const E_NOTIMPL: HRESULT = hr(0x8000_4001);
pub const E_NOINTERFACE: HRESULT = hr(0x8000_4002);
pub const E_ABORT: HRESULT = hr(0x8000_4004);
pub const E_FAIL: HRESULT = hr(0x8000_4005);
pub const E_PENDING: HRESULT = hr(0x8000_000A);
pub const E_ILLEGAL_METHOD_CALL: HRESULT = hr(0x8000_000E);
pub const E_INVALIDARG: HRESULT = hr(0x8007_0057);
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = hr(0x8007_007A);
/// `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)`.
pub const E_NETWORK_UNREACHABLE: HRESULT = hr(0x8007_04CF);

/// `XGameErr.h`, users.
pub const E_GAMEUSER_USER_NOT_FOUND: HRESULT = hr(0x8924_5104);
pub const E_GAMEUSER_FAILED_TO_GET_TOKEN: HRESULT = hr(0x8924_5111);
/// `XGameErr.h`, packages.
pub const E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER: HRESULT = hr(0x8924_5208);
/// `XGameErr.h`, store.
pub const E_GAMESTORE_NETWORK_ERROR: HRESULT = hr(0x8924_5301);

pub fn failed(h: HRESULT) -> bool {
    h < 0
}
