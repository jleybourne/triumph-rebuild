//! XDisplay, HDR mode. Our presentation is SDR until the swap chain can output HDR (Build 5), so
//! `XDisplayTryEnableHdrMode` answers disabled and the title renders SDR; once the lead sets
//! [`profile::Profile::hdr`] (the host display's luminance), it answers enabled with those numbers, as
//! a console on an HDR display would.
//!
//! Evidence: the fork's 242 run logs reaching `InitializeApiImplEx2` all call slot 0x20 (1,402 calls,
//! 1-6 per run, 232 runs with 6), always with preference 0 (prefer HDR). Slot 0x18 is not known (the
//! fork left it unimplemented and never logged a call): discovery.

use super::{slot, This};
use crate::guid::Guid;
use crate::object::InterfaceDef;
use crate::profile::{self, HdrModeInfo};

pub const PROVIDER: Guid = Guid::parse("03f0fe74-fdd9-4e5c-b630-f9339c47acc5");
/// The interface id this title asks for (its table; our first run, `runs/build3-reached.txt`).
pub const IID: Guid = Guid::parse("35f07670-706e-4bfb-9476-090798c5ebf3");

/// `XDisplayHdrModePreference`.
pub const PREFER_HDR: u32 = 0;
pub const PREFER_REFRESH_RATE: u32 = 1;
/// `XDisplayHdrModeResult`.
pub const HDR_UNKNOWN: u32 = 0;
pub const HDR_ENABLED: u32 = 1;
pub const HDR_DISABLED: u32 = 2;

/// `XDisplayTryEnableHdrMode`: enabled with the display's luminance when the presentation outputs HDR,
/// else disabled with `info` untouched. The preference does not change the answer: our refresh rate
/// does not depend on HDR.
unsafe extern "system" fn try_enable_hdr_mode(_: This, _preference: u32, info: *mut HdrModeInfo) -> u32 {
    match profile::current().hdr {
        Some(hdr) => {
            if !info.is_null() {
                unsafe { info.write_unaligned(hdr) };
            }
            HDR_ENABLED
        }
        None => HDR_DISABLED,
    }
}

pub static DISPLAY_IFACE: InterfaceDef = InterfaceDef {
    area: "display",
    provider: PROVIDER,
    iids: &[IID],
    len: 5,
    slots: &[slot!(
        0x20,
        "XDisplayTryEnableHdrMode",
        try_enable_hdr_mode,
        "preference: XDisplayHdrModePreference (u32), *info: XDisplayHdrModeInfo (3 x f32, optional) -> XDisplayHdrModeResult (u32)"
    )],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hr::*;
    use crate::object::Object;
    use crate::profile::{Profile, TEST_LOCK};
    use crate::runtime::QueryApiImpl;
    use std::ffi::c_void;

    fn queried<F: Copy>(offset: usize) -> (This, F) {
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(unsafe { QueryApiImpl(&PROVIDER, &IID, &mut out) }, S_OK);
        let f = unsafe { *(*(out as *const *const *const ())).add(offset / 8) };
        assert_eq!(size_of::<F>(), size_of::<*const ()>());
        (out as *const Object, unsafe { std::mem::transmute_copy(&f) })
    }

    type Try = unsafe extern "system" fn(This, u32, *mut HdrModeInfo) -> u32;

    #[test]
    fn sdr_until_the_presentation_outputs_hdr() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(size_of::<HdrModeInfo>(), 12);
        let (t, try_hdr) = queried::<Try>(0x20);
        let untouched = HdrModeInfo { min_tone_map_luminance: -1.0, max_tone_map_luminance: -1.0, max_full_frame_tone_map_luminance: -1.0 };

        profile::configure(Profile::default()).unwrap();
        for preference in [PREFER_HDR, PREFER_REFRESH_RATE] {
            let mut info = untouched;
            assert_eq!(unsafe { try_hdr(t, preference, &mut info) }, HDR_DISABLED);
            assert_eq!(info, untouched);
            assert_eq!(unsafe { try_hdr(t, preference, std::ptr::null_mut()) }, HDR_DISABLED);
        }

        let hdr = HdrModeInfo { min_tone_map_luminance: 0.01, max_tone_map_luminance: 1000.0, max_full_frame_tone_map_luminance: 600.0 };
        profile::configure(Profile { hdr: Some(hdr), ..Profile::default() }).unwrap();
        let mut info = untouched;
        assert_eq!(unsafe { try_hdr(t, PREFER_HDR, &mut info) }, HDR_ENABLED);
        assert_eq!(info, hdr);
        assert_eq!(unsafe { try_hdr(t, PREFER_HDR, std::ptr::null_mut()) }, HDR_ENABLED);
        profile::configure(Profile::default()).unwrap();
    }

    /// Slot 0x18 is not known: discovery, as is anything past 0x20.
    #[test]
    fn unknown_slots_are_discovered() {
        for offset in [0x18, 0x28] {
            let (t, f) = queried::<unsafe extern "system" fn(This) -> HRESULT>(offset);
            assert_eq!(unsafe { f(t) }, E_NOTIMPL);
            assert!(crate::discovery::contains(&format!("slot not implemented: display {offset:#x}")));
        }
    }
}
