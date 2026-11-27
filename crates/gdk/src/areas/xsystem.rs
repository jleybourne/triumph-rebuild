//! XSystem: the console's identity and how analytics describe it. One offline retail console of the
//! profile's model. The device type is not a runtime call (code linked into the title reads a
//! registry value; the fork's note), so it is not here.

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put_str, InterfaceDef};
use crate::profile;

pub const PROVIDER: Guid = Guid::parse("e349bd1a-fc20-4e40-b99c-4178cc6b409f");
/// The interface id this title asks for (its table; the fork's run notes).
pub const IID: Guid = Guid::parse("1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf");
/// Analytics information: the interface id is the provider's.
pub const ANALYTICS: Guid = Guid::parse("b884675d-b738-4a9c-815d-9a9a1e0c6c9b");

/// `XSystemConsoleIdBytes` is 39: 38 characters and a NUL.
pub const CONSOLE_ID: &str = "00000000000000000000000000000000000001";
/// `XSystemAppSpecificDeviceIdBytes` is 45: base64 of 32 bytes and a NUL. Ours is constant.
pub const DEVICE_ID: &str = "dHJpdW1waC1yZWJ1aWxkIG9mZmxpbmUgZGV2aWNlISE=";

unsafe extern "system" fn console_id(_: This, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    unsafe { put_str(CONSOLE_ID, size, buffer, used) }
}
unsafe extern "system" fn sandbox_id(_: This, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    unsafe { put_str(&profile::current().sandbox, size, buffer, used) }
}
unsafe extern "system" fn device_id(_: This, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    unsafe { put_str(DEVICE_ID, size, buffer, used) }
}

/// `XVersion`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

/// `XSystemAnalyticsInfo`, 0x90 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AnalyticsInfo {
    pub os_version: Version,
    pub hosting_os_version: Version,
    pub family: [u8; 64],
    pub form: [u8; 64],
}

/// What our console reports as its OS (the console's real numbers are not known here).
pub const OS: Version = Version { major: 10, minor: 0, build: 22000, revision: 0 };

fn text64(s: &str) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..s.len().min(63)].copy_from_slice(&s.as_bytes()[..s.len().min(63)]);
    out
}

/// Returns a 0x90-byte struct: by the x64 ABI for a method, through a hidden pointer after `this`,
/// which is also the return value.
unsafe extern "system" fn analytics_info(_: This, out: *mut AnalyticsInfo) -> *mut AnalyticsInfo {
    let info = AnalyticsInfo {
        os_version: OS,
        hosting_os_version: OS,
        family: text64("Xbox"),
        form: text64(profile::current().console.form()),
    };
    if !out.is_null() {
        unsafe { out.write_unaligned(info) };
    }
    out
}

pub static SYSTEM_IFACE: InterfaceDef = InterfaceDef {
    area: "system",
    provider: PROVIDER,
    iids: &[IID],
    len: 6,
    slots: &[
        slot!(0x18, "XSystemGetConsoleId", console_id, "size, *buffer, *used -> HRESULT"),
        slot!(0x20, "XSystemGetXboxLiveSandboxId", sandbox_id, "size, *buffer, *used -> HRESULT"),
        slot!(0x28, "XSystemGetAppSpecificDeviceId", device_id, "size, *buffer, *used -> HRESULT"),
    ],
};

pub static ANALYTICS_IFACE: InterfaceDef = InterfaceDef {
    area: "analytics",
    provider: ANALYTICS,
    iids: &[ANALYTICS],
    len: 4,
    slots: &[slot!(0x18, "XSystemGetAnalyticsInfo", analytics_info, "*result (hidden) -> *result")],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};

    #[test]
    fn identity_strings_fit_the_documented_sizes() {
        type Get = unsafe extern "system" fn(This, usize, *mut u8, *mut usize) -> HRESULT;
        let t = this(&SYSTEM_IFACE);
        for (slot, size) in [(0x18, 39), (0x20, 16), (0x28, 45)] {
            let mut buf = [0xffu8; 64];
            let mut used = 0;
            assert_eq!(unsafe { method::<Get>(&SYSTEM_IFACE, slot)(t, size, buf.as_mut_ptr(), &mut used) }, S_OK);
            assert!(used <= size && buf[used - 1] == 0, "slot {slot:#x}");
            assert_eq!(unsafe { method::<Get>(&SYSTEM_IFACE, slot)(t, used - 1, buf.as_mut_ptr(), &mut used) }, E_NOT_SUFFICIENT_BUFFER);
        }
        assert_eq!(CONSOLE_ID.len() + 1, 39);
        assert_eq!(DEVICE_ID.len() + 1, 45);
    }

    #[test]
    fn analytics_come_back_through_the_hidden_pointer() {
        assert_eq!(size_of::<AnalyticsInfo>(), 0x90);
        let mut info = std::mem::MaybeUninit::<AnalyticsInfo>::zeroed();
        type Get = unsafe extern "system" fn(This, *mut AnalyticsInfo) -> *mut AnalyticsInfo;
        let back = unsafe { method::<Get>(&ANALYTICS_IFACE, 0x18)(this(&ANALYTICS_IFACE), info.as_mut_ptr()) };
        assert_eq!(back, info.as_mut_ptr());
        let info = unsafe { info.assume_init() };
        assert_eq!(&info.family[..5], b"Xbox\0");
        assert_eq!(&info.form[..13], b"Xbox Series X");
        assert_eq!(info.os_version, OS);
    }
}
