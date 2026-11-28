//! XNetworking offline: the network stack is initialized (titles wait for that) but reaches
//! nothing; the connectivity hint never changes; what needs the service (a URL's security
//! information) fails at once with `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)` (the console's
//! exact offline code is not known). Slot offsets from the fork's design.

use std::ffi::{c_char, c_void};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put, text, token, InterfaceDef};
use crate::xasync::{self, XAsyncBlock};

pub const PROVIDER: Guid = Guid::parse("37e56907-2f10-41e8-b72f-36edb185331a");
pub const IID: Guid = Guid::parse("bf2346b2-39af-4658-b5ea-44713c7e83b3");

/// The documented default preferred local UDP multiplayer port.
pub const UDP_PORT: u16 = 3074;

/// `XNetworkingConnectivityHint`, 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectivityHint {
    pub level: u32,
    pub cost: u32,
    pub iana_interface_type: u32,
    pub network_initialized: bool,
    pub approaching_data_limit: bool,
    pub over_data_limit: bool,
    pub roaming: bool,
}

/// Initialized; level `None` (1): no connectivity; cost `Unknown` (0).
pub const OFFLINE: ConnectivityHint = ConnectivityHint {
    level: 1,
    cost: 0,
    iana_interface_type: 0,
    network_initialized: true,
    approaching_data_limit: false,
    over_data_limit: false,
    roaming: false,
};

/// The offline online-mode sandbox hint (debug): level `InternetAccess` (3), cost `Unrestricted` (1),
/// IANA interface type Ethernet (6). Reported only when the sandbox is on, so the title walks past its
/// connectivity gate; the runtime layer still blocks every real connection.
pub const SANDBOX_ONLINE: ConnectivityHint = ConnectivityHint {
    level: 3,
    cost: 1,
    iana_interface_type: 6,
    network_initialized: true,
    approaching_data_limit: false,
    over_data_limit: false,
    roaming: false,
};

unsafe extern "system" fn udp_port(_: This, out: *mut u16) -> HRESULT {
    unsafe { put(out, UDP_PORT) }
}
static PORT: u8 = 0;
unsafe extern "system" fn udp_port_async(_: This, block: *mut XAsyncBlock) -> HRESULT {
    unsafe { xasync::finish(block, &PORT, Ok(xasync::bytes(UDP_PORT))) }
}
unsafe extern "system" fn udp_port_result(_: This, block: *mut XAsyncBlock, out: *mut u16) -> HRESULT {
    unsafe { xasync::take(block, &PORT, out) }
}

static SECURITY: u8 = 0;
unsafe extern "system" fn security_info_async(_: This, url: *const c_char, block: *mut XAsyncBlock) -> HRESULT {
    let url = unsafe { text(url) }.unwrap_or_default();
    crate::discovery::note(format!("offline: XNetworkingQuerySecurityInformationForUrlAsync {url}"));
    unsafe { xasync::finish(block, &SECURITY, Err(E_NETWORK_UNREACHABLE)) }
}
unsafe extern "system" fn security_info_size(_: This, block: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    unsafe { xasync::get_result_size(block, out) }
}
unsafe extern "system" fn security_info_result(_: This, block: *mut XAsyncBlock, size: usize, buffer: *mut c_void, _info: *mut *const c_void, used: *mut usize) -> HRESULT {
    unsafe { xasync::get_result(block, (&SECURITY as *const u8).cast(), size, buffer, used) }
}

unsafe extern "system" fn connectivity_hint(_: This, out: *mut ConnectivityHint) -> HRESULT {
    let hint = if crate::sandbox::online() { SANDBOX_ONLINE } else { OFFLINE };
    unsafe { put(out, hint) }
}
/// The hint never changes: the callback is never called.
unsafe extern "system" fn register(_: This, _queue: *const c_void, _context: *mut c_void, _callback: *const c_void, out: *mut u64) -> HRESULT {
    unsafe { put(out, token()) }
}
unsafe extern "system" fn unregister(_: This, _token: u64, _wait: u8) -> bool {
    true
}

pub static NETWORKING_IFACE: InterfaceDef = InterfaceDef {
    area: "networking",
    provider: PROVIDER,
    // The provider's own id: older titles; the fork answers both with the same slots.
    iids: &[IID, PROVIDER],
    len: 18,
    slots: &[
        slot!(0x18, "XNetworkingGetPreferredLocalUdpMultiplayerPort", udp_port, "*port: u16 -> HRESULT"),
        slot!(0x20, "XNetworkingGetPreferredLocalUdpMultiplayerPortAsync", udp_port_async, "block -> HRESULT"),
        slot!(0x28, "XNetworkingGetPreferredLocalUdpMultiplayerPortAsyncResult", udp_port_result, "block, *port: u16 -> HRESULT"),
        slot!(0x40, "XNetworkingQuerySecurityInformationForUrlAsync", security_info_async, "url, block -> HRESULT"),
        slot!(0x48, "XNetworkingQuerySecurityInformationForUrlAsyncResultSize", security_info_size, "block, *size -> HRESULT"),
        slot!(0x50, "XNetworkingQuerySecurityInformationForUrlAsyncResult", security_info_result, "block, size, *buffer, **info, *used -> HRESULT"),
        slot!(0x78, "XNetworkingGetConnectivityHint", connectivity_hint, "*hint -> HRESULT"),
        slot!(0x80, "XNetworkingRegisterConnectivityHintChanged", register, "queue, context, callback, *token: u64 -> HRESULT"),
        slot!(0x88, "XNetworkingUnregisterConnectivityHintChanged", unregister, "token: u64, wait: bool -> bool"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};

    #[test]
    fn up_but_offline() {
        assert_eq!(size_of::<ConnectivityHint>(), 16);
        let t = this(&NETWORKING_IFACE);
        unsafe {
            let mut hint = std::mem::MaybeUninit::<ConnectivityHint>::zeroed();
            assert_eq!(method::<unsafe extern "system" fn(This, *mut ConnectivityHint) -> HRESULT>(&NETWORKING_IFACE, 0x78)(t, hint.as_mut_ptr()), S_OK);
            let hint = hint.assume_init();
            assert!(hint.network_initialized && hint.level == 1);

            let mut port = 0u16;
            assert_eq!(method::<unsafe extern "system" fn(This, *mut u16) -> HRESULT>(&NETWORKING_IFACE, 0x18)(t, &mut port), S_OK);
            assert_eq!(port, 3074);
            let mut b = XAsyncBlock::new(std::ptr::null(), None);
            assert_eq!(method::<unsafe extern "system" fn(This, *mut XAsyncBlock) -> HRESULT>(&NETWORKING_IFACE, 0x20)(t, &mut b), S_OK);
            port = 0;
            assert_eq!(method::<unsafe extern "system" fn(This, *mut XAsyncBlock, *mut u16) -> HRESULT>(&NETWORKING_IFACE, 0x28)(t, &mut b, &mut port), S_OK);
            assert_eq!(port, 3074);

            type Sec = unsafe extern "system" fn(This, *const c_char, *mut XAsyncBlock) -> HRESULT;
            assert_eq!(method::<Sec>(&NETWORKING_IFACE, 0x40)(t, c"https://example.invalid".as_ptr(), &mut b), S_OK);
            assert_eq!(xasync::get_status(&mut b, true), E_NETWORK_UNREACHABLE);
            let mut size = 0;
            assert_eq!(method::<unsafe extern "system" fn(This, *mut XAsyncBlock, *mut usize) -> HRESULT>(&NETWORKING_IFACE, 0x48)(t, &mut b, &mut size), E_NETWORK_UNREACHABLE);
            type Res = unsafe extern "system" fn(This, *mut XAsyncBlock, usize, *mut c_void, *mut *const c_void, *mut usize) -> HRESULT;
            assert_eq!(method::<Res>(&NETWORKING_IFACE, 0x50)(t, &mut b, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()), E_NETWORK_UNREACHABLE);

            let mut token = 0;
            type Reg = unsafe extern "system" fn(This, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
            assert_eq!(method::<Reg>(&NETWORKING_IFACE, 0x80)(t, std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), &mut token), S_OK);
            assert!(method::<unsafe extern "system" fn(This, u64, u8) -> bool>(&NETWORKING_IFACE, 0x88)(t, token, 1));
        }
    }

    #[test]
    fn the_sandbox_reports_online_but_default_stays_offline() {
        type Hint = unsafe extern "system" fn(This, *mut ConnectivityHint) -> HRESULT;
        let t = this(&NETWORKING_IFACE);
        let read = || unsafe {
            let mut h = std::mem::MaybeUninit::<ConnectivityHint>::zeroed();
            assert_eq!(method::<Hint>(&NETWORKING_IFACE, 0x78)(t, h.as_mut_ptr()), S_OK);
            h.assume_init()
        };
        // default: offline (level None = 1)
        crate::sandbox::with_override(false, || assert_eq!(read(), OFFLINE));
        // sandbox on: online-capable (level InternetAccess = 3)
        crate::sandbox::with_override(true, || {
            let h = read();
            assert_eq!(h, SANDBOX_ONLINE);
            assert_eq!(h.level, 3);
            assert!(h.network_initialized);
        });
    }
}
