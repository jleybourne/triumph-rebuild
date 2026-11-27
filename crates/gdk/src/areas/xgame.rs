//! XGame: the title id, which runtime areas exist, invites (none arrive offline) and game events
//! (accepted, dropped). `XLaunchNewGame` (title slot 0x20) ends the process: the lead's lifecycle,
//! left to discovery here.

use std::ffi::{c_char, c_void};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put, text, token, InterfaceDef};
use crate::profile;

pub const TITLE_PROVIDER: Guid = Guid::parse("973a344e-24bf-4d0f-8457-56c534892b29");
pub const TITLE_IID: Guid = Guid::parse("2549f142-6419-4a06-97b5-931aab7c2f34");
/// Feature checks, invites and events: the interface id is the provider's.
pub const FEATURES: Guid = Guid::parse("8836fe87-edb9-4fe3-8dad-05f0d2cd5b40");
pub const INVITES: Guid = Guid::parse("0651aae2-4012-4077-bf84-8b9097090e2c");
pub const EVENTS: Guid = Guid::parse("bbfbdcc7-bfe7-409b-a5ca-edf054960b4d");

unsafe extern "system" fn title_id(_: This, out: *mut u32) -> HRESULT {
    unsafe { put(out, profile::current().title_id) }
}

/// `XGameRuntimeFeature` values (the public enum's order) whose area this crate serves: XAsync (2),
/// XAsyncProvider (3), XGame (5), XGameInvite (6), XNetworking (10), XPackage (11), XStore (14),
/// XSystem (15), XTaskQueue (16), XUser (18), XError (19), XGameEvent (20). Not yet: accessibility
/// (0), app capture (1), display (4), game saves (7), game UI (8), launcher (9), local storage (12),
/// speech (13), threads (17), streaming (21). The test keeps this list and the served areas in step.
pub const AVAILABLE: [u32; 12] = [2, 3, 5, 6, 10, 11, 14, 15, 16, 18, 19, 20];

unsafe extern "system" fn is_feature_available(_: This, feature: u32) -> bool {
    AVAILABLE.contains(&feature)
}

/// Nothing raises invites offline; the callback is never called.
unsafe extern "system" fn register(_: This, _queue: *const c_void, _context: *mut c_void, _callback: *const c_void, out: *mut u64) -> HRESULT {
    unsafe { put(out, token()) }
}
unsafe extern "system" fn unregister(_: This, _token: u64, _wait: u8) -> bool {
    true
}

/// `XGameEventWrite`: accepted; the name goes to discovery once (events go nowhere offline).
unsafe extern "system" fn write_event(
    _: This,
    _user: *const c_void,
    _service_config_id: *const c_char,
    _session: *const c_char,
    name: *const c_char,
    _dimensions: *const c_char,
    _measurements: *const c_char,
) -> HRESULT {
    if let Some(name) = unsafe { text(name) } {
        crate::discovery::note(format!("event dropped: {name}"));
    }
    S_OK
}

pub static TITLE_IFACE: InterfaceDef = InterfaceDef {
    area: "title",
    provider: TITLE_PROVIDER,
    iids: &[TITLE_IID],
    len: 5,
    slots: &[slot!(0x18, "XGameGetXboxTitleId", title_id, "*titleId: u32 -> HRESULT")],
};

pub static FEATURES_IFACE: InterfaceDef = InterfaceDef {
    area: "features",
    provider: FEATURES,
    iids: &[FEATURES],
    len: 4,
    slots: &[slot!(0x18, "XGameRuntimeIsFeatureAvailable", is_feature_available, "feature: u32 -> bool")],
};

pub static INVITES_IFACE: InterfaceDef = InterfaceDef {
    area: "invites",
    provider: INVITES,
    iids: &[INVITES],
    len: 5,
    slots: &[
        slot!(0x18, "XGameInviteRegisterForEvent", register, "queue, context, callback, *token: u64 -> HRESULT"),
        slot!(0x20, "XGameInviteUnregisterForEvent", unregister, "token: u64, wait: bool -> bool"),
    ],
};

pub static EVENTS_IFACE: InterfaceDef = InterfaceDef {
    area: "events",
    provider: EVENTS,
    iids: &[EVENTS],
    len: 4,
    slots: &[slot!(0x18, "XGameEventWrite", write_event, "user, serviceConfigId, playSessionId, name, dimensionsJson, measurementsJson -> HRESULT")],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};
    use crate::profile::{Profile, TEST_LOCK};

    #[test]
    fn title_id_from_the_profile() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        profile::configure(Profile { title_id: 0x1234_5678, ..Profile::default() }).unwrap();
        let mut id = 0;
        let f = method::<unsafe extern "system" fn(This, *mut u32) -> HRESULT>(&TITLE_IFACE, 0x18);
        assert_eq!(unsafe { f(this(&TITLE_IFACE), &mut id) }, S_OK);
        assert_eq!(id, 0x1234_5678);
        assert_eq!(unsafe { f(this(&TITLE_IFACE), std::ptr::null_mut()) }, E_INVALIDARG);
        profile::configure(Profile::default()).unwrap();
    }

    /// The available features are exactly the areas whose provider `QueryApiImpl` answers.
    #[test]
    fn features_follow_the_served_areas() {
        let f = method::<unsafe extern "system" fn(This, u32) -> bool>(&FEATURES_IFACE, 0x18);
        let t = this(&FEATURES_IFACE);
        // feature -> provider, for features that are runtime areas of their own.
        let areas = [
            (2, crate::areas::queue::PROVIDER),
            (5, TITLE_PROVIDER),
            (6, INVITES),
            (10, crate::areas::xnetworking::PROVIDER),
            (11, crate::areas::xpackage::PROVIDER),
            (14, crate::areas::xstore::PROVIDER),
            (15, crate::areas::xsystem::PROVIDER),
            (16, crate::areas::queue::PROVIDER),
            (18, crate::areas::xuser::PROVIDER),
            (20, EVENTS),
        ];
        for (feature, provider) in areas {
            assert!(unsafe { f(t, feature) }, "feature {feature}");
            assert!(crate::table::ALL.iter().any(|d| d.provider == provider), "feature {feature} served");
        }
        for unserved in [0, 1, 4, 7, 8, 9, 12, 13, 17, 21, 99] {
            assert!(!unsafe { f(t, unserved) }, "feature {unserved}");
        }
    }

    #[test]
    fn invites_and_events() {
        type Reg = unsafe extern "system" fn(This, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
        let (t, null) = (this(&INVITES_IFACE), std::ptr::null());
        let mut token = 0;
        assert_eq!(unsafe { method::<Reg>(&INVITES_IFACE, 0x18)(t, null, null as _, null, &mut token) }, S_OK);
        assert_ne!(token, 0);
        assert!(unsafe { method::<unsafe extern "system" fn(This, u64, u8) -> bool>(&INVITES_IFACE, 0x20)(t, token, 0) });
        type Write = unsafe extern "system" fn(This, *const c_void, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char) -> HRESULT;
        let n = std::ptr::null();
        assert_eq!(unsafe { method::<Write>(&EVENTS_IFACE, 0x18)(this(&EVENTS_IFACE), null, n, n, c"MenuOpened".as_ptr(), n, n) }, S_OK);
        assert!(crate::discovery::contains("event dropped: MenuOpened"));
    }
}
