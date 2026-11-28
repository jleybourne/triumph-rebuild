//! The six `xgameruntime.dll` entry points the title (and its GDK libraries) take by name.
//!
//! What the arguments mean beyond their shape is not known: the fork's logs show the title's stub
//! calling only `InitializeApiImplEx2`, twice per run (two stubs: `flags` 2 and 3), with a `version`
//! of 0x6336_0000_2712 and `build` values 0x65f4_0000_1edd / 0x65f4_0000_1832, `options` null. They are
//! recorded, not checked.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::sync::Mutex;

use crate::discovery;
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{self, Fp};

/// One initialisation as the stub made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Init {
    pub entry: &'static str,
    pub version: u64,
    pub build: u64,
    pub flags: Option<u64>,
    pub options: usize,
}

static INITS: Mutex<Vec<Init>> = Mutex::new(Vec::new());
static UNINITS: Mutex<u32> = Mutex::new(0);

fn record(init: Init) -> HRESULT {
    INITS.lock().unwrap_or_else(|e| e.into_inner()).push(init);
    S_OK
}

/// Every initialisation so far.
pub fn inits() -> Vec<Init> {
    INITS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn uninits() -> u32 {
    *UNINITS.lock().unwrap_or_else(|e| e.into_inner())
}

pub unsafe extern "system" fn InitializeApiImpl(version: u64, build: u64) -> HRESULT {
    record(Init { entry: "InitializeApiImpl", version, build, flags: None, options: 0 })
}

pub unsafe extern "system" fn InitializeApiImplEx(version: u64, build: u64, flags: u64) -> HRESULT {
    record(Init { entry: "InitializeApiImplEx", version, build, flags: Some(flags), options: 0 })
}

/// The stub prefers this one when present.
pub unsafe extern "system" fn InitializeApiImplEx2(version: u64, build: u64, flags: u64, options: *const c_void) -> HRESULT {
    record(Init { entry: "InitializeApiImplEx2", version, build, flags: Some(flags), options: options as usize })
}

pub unsafe extern "system" fn UninitializeApiImpl() {
    *UNINITS.lock().unwrap_or_else(|e| e.into_inner()) += 1;
}

/// The object for (provider, iid); `out` is left untouched for a pair we do not serve, which is
/// noted as discovery and answered with `E_NOINTERFACE` (the console's code for this case is not
/// known; the title's stubs test for failure only, by the fork's runs).
pub unsafe extern "system" fn QueryApiImpl(provider: *const Guid, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    let (Some(provider), Some(iid)) = (unsafe { provider.as_ref() }, unsafe { iid.as_ref() }) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    match object::find(provider, iid) {
        Some(o) => {
            unsafe { out.write(o.as_ptr()) };
            S_OK
        }
        None => {
            discovery::note(format!("interface not implemented: provider {provider} iid {iid}"));
            E_NOINTERFACE
        }
    }
}

/// Noted (each distinct report once) and otherwise ignored.
pub unsafe extern "system" fn XErrorReport(hr: HRESULT, message: *const c_char) {
    let message = unsafe { object::text(message) }.unwrap_or_default();
    discovery::note(format!("XErrorReport {:#010x} {message}", hr as u32));
}

/// The six names `GetProcAddress` answers for `xgameruntime.dll`.
pub static ENTRY_POINTS: &[(&str, Fp)] = &[
    ("InitializeApiImpl", Fp(InitializeApiImpl as *const ())),
    ("InitializeApiImplEx", Fp(InitializeApiImplEx as *const ())),
    ("InitializeApiImplEx2", Fp(InitializeApiImplEx2 as *const ())),
    ("UninitializeApiImpl", Fp(UninitializeApiImpl as *const ())),
    ("QueryApiImpl", Fp(QueryApiImpl as *const ())),
    ("XErrorReport", Fp(XErrorReport as *const ())),
];

/// `GetProcAddress(xgameruntime, name)`; a miss is noted as discovery.
pub fn get_proc(name: &str) -> Option<*const ()> {
    let found = ENTRY_POINTS.iter().find(|(n, _)| *n == name).map(|(_, f)| f.0);
    if found.is_none() {
        discovery::note(format!("xgameruntime.dll export not implemented: {name}"));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_initialises_and_queries() {
        type Init2 = unsafe extern "system" fn(u64, u64, u64, *const c_void) -> HRESULT;
        type Query = unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> HRESULT;
        let init: Init2 = unsafe { std::mem::transmute(get_proc("InitializeApiImplEx2").unwrap()) };
        let query: Query = unsafe { std::mem::transmute(get_proc("QueryApiImpl").unwrap()) };
        assert_eq!(unsafe { init(0x6336_0000_2712, 0x65f4_0000_1edd, 2, std::ptr::null()) }, S_OK);
        assert!(inits().iter().any(|i| i.flags == Some(2) && i.build == 0x65f4_0000_1edd));
        for name in ["InitializeApiImpl", "InitializeApiImplEx", "UninitializeApiImpl", "XErrorReport"] {
            assert!(get_proc(name).is_some(), "{name}");
        }
        assert!(get_proc("XGameRuntimeInitialize").is_none());
        assert!(discovery::contains("export not implemented: XGameRuntimeInitialize"));

        let q = crate::areas::queue::PROVIDER;
        let mut out = std::ptr::null_mut();
        assert_eq!(unsafe { query(&q, &q, &mut out) }, S_OK);
        assert!(!out.is_null());
        let unknown = Guid::parse("8ca467f7-22e8-4096-8456-bb8aa13f79d8");
        let untouched = 0x1234 as *mut c_void;
        let mut out = untouched;
        assert_eq!(unsafe { query(&unknown, &unknown, &mut out) }, E_NOINTERFACE);
        assert_eq!(out, untouched);
        assert!(discovery::contains("provider 8ca467f7-22e8-4096-8456-bb8aa13f79d8"));
        assert_eq!(unsafe { query(std::ptr::null(), &q, &mut out) }, E_INVALIDARG);

        unsafe { XErrorReport(E_FAIL, c"boom".as_ptr()) };
        assert!(discovery::contains("XErrorReport 0x80004005 boom"));
        let before = uninits();
        unsafe { UninitializeApiImpl() };
        assert_eq!(uninits(), before + 1);
    }

    /// The pairs the title's exe carries in its runtime GUID table (`agents/gdk.md`, G1), with the
    /// pairing the fork's runs served: every one answers.
    #[test]
    fn every_pair_the_title_asks_for_answers() {
        let pairs = [
            ("073b7dcb-1fcf-4030-94be-e3c9eb623428", "073b7dcb-1fcf-4030-94be-e3c9eb623428"),
            ("8836fe87-edb9-4fe3-8dad-05f0d2cd5b40", "8836fe87-edb9-4fe3-8dad-05f0d2cd5b40"),
            ("bbfbdcc7-bfe7-409b-a5ca-edf054960b4d", "bbfbdcc7-bfe7-409b-a5ca-edf054960b4d"),
            ("973a344e-24bf-4d0f-8457-56c534892b29", "2549f142-6419-4a06-97b5-931aab7c2f34"),
            ("b884675d-b738-4a9c-815d-9a9a1e0c6c9b", "b884675d-b738-4a9c-815d-9a9a1e0c6c9b"),
            ("e349bd1a-fc20-4e40-b99c-4178cc6b409f", "1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf"),
            ("01acd177-91f9-4763-a38e-ccbb55ce32e0", "01acd177-91f9-4763-a38e-ccbb55ce32e0"),
            ("01acd177-91f9-4763-a38e-ccbb55ce32e0", "cef4fac0-7676-4a94-a119-4c43f9eb5b74"),
            ("af406016-e850-4aa8-a88d-2f3dcb9dac7e", "0b9d556e-ab70-43f4-baa0-9999ac4cb040"),
            ("37e56907-2f10-41e8-b72f-36edb185331a", "bf2346b2-39af-4658-b5ea-44713c7e83b3"),
            ("0dd112ac-7c24-448c-b92b-3960fb5bd30c", "b09d803c-2414-4a05-82c6-66dfdc9e9a44"),
            ("0dd112ac-7c24-448c-b92b-3960fb5bd30c", "2d42fea5-e71d-4b76-97cd-c50afbb3ae5d"),
            // G5: the two our first run of the title asked for (runs/build3-reached.txt).
            ("f4faf4d4-2d04-4fce-b3e0-474a713a3e84", "41a4e10c-5a7e-41d9-8c37-37bde62a07d6"),
            ("03f0fe74-fdd9-4e5c-b630-f9339c47acc5", "35f07670-706e-4bfb-9476-090798c5ebf3"),
            // The two a card run's report named (game saves; speech synthesis).
            ("704c3f58-e629-4cc2-b197-30511b996fe2", "1bfff3af-f14a-40a3-8e35-9ada906593f9"),
            ("3e241977-6237-41e9-8559-102c2d9983f1", "d722b373-8c4d-4692-9e51-d6ad9b37aa7d"),
        ];
        for (p, i) in pairs {
            let (p, i) = (Guid::parse(p), Guid::parse(i));
            let mut out = std::ptr::null_mut();
            assert_eq!(unsafe { QueryApiImpl(&p, &i, &mut out) }, S_OK, "{p} {i}");
        }
    }
}
