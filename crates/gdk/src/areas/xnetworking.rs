//! XNetworking offline: the network stack is initialized (titles wait for that) but reaches
//! nothing; the connectivity hint never changes after registration's initial notification; what
//! needs the service (a URL's security information) fails at once with
//! `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)` (the console's exact offline code is not known).
//! Slot offsets from the fork's design.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{put, text, token, InterfaceDef};
use crate::taskqueue::{self, COMPLETION};
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

fn current_hint() -> ConnectivityHint {
    if crate::sandbox::online() { SANDBOX_ONLINE } else { OFFLINE }
}

unsafe extern "system" fn connectivity_hint(_: This, out: *mut ConnectivityHint) -> HRESULT {
    unsafe { put(out, current_hint()) }
}

/// `XNetworkingConnectivityHintChangedCallback`.
pub type HintCallback = unsafe extern "system" fn(*mut c_void, *const ConnectivityHint);

/// One change registration: live until unregistered, and how many of its callbacks are running, so
/// an unregister with `wait` can wait them out. A callback still queued when the registration goes
/// is dropped when its queue delivers it, so the wait never depends on anyone dispatching.
#[derive(Default)]
struct Registration {
    state: Mutex<RegState>,
    idle: Condvar,
}

#[derive(Default)]
struct RegState {
    unregistered: bool,
    running: u32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static REGISTRATIONS: LazyLock<Mutex<HashMap<u64, Arc<Registration>>>> = LazyLock::new(Default::default);

thread_local! {
    /// Set while this thread runs a hint callback: an unregister from inside it must not wait for itself.
    static IN_CALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A hint callback on its way through the registration's queue.
struct Notice {
    registration: Arc<Registration>,
    context: usize,
    callback: HintCallback,
    hint: ConnectivityHint,
}

unsafe extern "system" fn notify(context: *mut c_void, canceled: bool) {
    // SAFETY: boxed in `register`, delivered once.
    let n = unsafe { Box::from_raw(context.cast::<Notice>()) };
    let reg = &n.registration;
    {
        let mut s = lock(&reg.state);
        if canceled || s.unregistered {
            return;
        }
        s.running += 1;
    }
    IN_CALLBACK.with(|c| c.set(true));
    // SAFETY: the title's callback with its context; the hint lives across the call.
    unsafe { (n.callback)(n.context as *mut c_void, &n.hint) };
    IN_CALLBACK.with(|c| c.set(false));
    lock(&reg.state).running -= 1;
    reg.idle.notify_all();
}

/// `XNetworkingRegisterConnectivityHintChanged`: registers, and "sends an initial notification
/// callback" (learn.microsoft.com, the function's Remarks) with the current hint, through `queue`'s
/// completion port (null: the process queue). The hint never changes after that. Which port the
/// runtime uses is not documented; the completion port is where the runtime's other user callbacks
/// (XAsync completion routines) go.
unsafe extern "system" fn register(_: This, queue: *const c_void, context: *mut c_void, callback: Option<HintCallback>, out: *mut u64) -> HRESULT {
    let Some(callback) = callback else { return E_INVALIDARG };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let token = token();
    let registration = Arc::new(Registration::default());
    lock(&REGISTRATIONS).insert(token, registration.clone());
    unsafe { out.write_unaligned(token) };
    match taskqueue::queue(queue.cast()) {
        Some(q) => {
            let notice = Notice { registration, context: context as usize, callback, hint: current_hint() };
            let notice = Box::into_raw(Box::new(notice));
            if q.submit(COMPLETION, 0, notice.cast(), notify) != S_OK {
                // A terminated queue: nothing will be delivered.
                drop(unsafe { Box::from_raw(notice) });
            }
        }
        None => crate::discovery::note("XNetworkingRegisterConnectivityHintChanged: no queue (process queue cleared); no initial callback".into()),
    }
    S_OK
}

/// `XNetworkingUnregisterConnectivityHintChanged`: no callback starts after this; with `wait`,
/// returns once a callback already running has returned (not when called from inside one).
unsafe extern "system" fn unregister(_: This, token: u64, wait: u8) -> bool {
    let Some(registration) = lock(&REGISTRATIONS).remove(&token) else { return true };
    let mut s = lock(&registration.state);
    s.unregistered = true;
    if wait != 0 && !IN_CALLBACK.with(|c| c.get()) {
        drop(registration.idle.wait_while(s, |s| s.running > 0).unwrap_or_else(|e| e.into_inner()));
    }
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
    use crate::taskqueue::{Mode, WORK};
    use std::sync::atomic::{AtomicU32, Ordering};

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
            assert_eq!(method::<Register>(&NETWORKING_IFACE, 0x80)(t, std::ptr::null(), std::ptr::null_mut(), None, &mut token), E_INVALIDARG, "the callback is required");
        }
    }

    type Register = unsafe extern "system" fn(This, *const c_void, *mut c_void, Option<HintCallback>, *mut u64) -> HRESULT;
    type Unregister = unsafe extern "system" fn(This, u64, u8) -> bool;

    /// Counts calls into the `AtomicU32` its context points at; checks the hint it gets.
    unsafe extern "system" fn count(context: *mut c_void, hint: *const ConnectivityHint) {
        assert!(unsafe { (*hint).network_initialized });
        unsafe { &*context.cast::<AtomicU32>() }.fetch_add(1, Ordering::SeqCst);
    }

    fn make(work: Mode, completion: Mode) -> taskqueue::Handle {
        let mut q = std::ptr::null();
        assert_eq!(unsafe { taskqueue::create(work as u32, completion as u32, &mut q) }, S_OK);
        q
    }

    /// "registers ... and sends an initial notification callback": once, through the queue's
    /// completion port, with the current hint.
    #[test]
    fn registering_sends_one_initial_notification_through_the_queue() {
        let t = this(&NETWORKING_IFACE);
        let q = make(Mode::Manual, Mode::Manual);
        let calls = AtomicU32::new(0);
        let ctx = (&calls as *const AtomicU32).cast_mut().cast();
        let mut token = 0;
        unsafe {
            assert_eq!(method::<Register>(&NETWORKING_IFACE, 0x80)(t, q.cast(), ctx, Some(count), &mut token), S_OK);
            assert_ne!(token, 0);
            assert_eq!(calls.load(Ordering::SeqCst), 0, "queued, not called inline on a manual port");
            assert!(!taskqueue::dispatch(q, WORK, 0), "not on the work port");
            assert!(taskqueue::dispatch(q, COMPLETION, 0));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(!taskqueue::dispatch(q, COMPLETION, 0), "only one");
            assert!(method::<Unregister>(&NETWORKING_IFACE, 0x88)(t, token, 1));
            taskqueue::close(q);
        }
    }

    /// Unregistered before its manual port delivered the notification: an unregister with `wait`
    /// returns at once, and the callback is never called. On an immediate port it runs inside the
    /// register call.
    #[test]
    fn unregistered_registrations_get_no_callback() {
        let t = this(&NETWORKING_IFACE);
        let q = make(Mode::Manual, Mode::Manual);
        let calls = AtomicU32::new(0);
        let ctx = (&calls as *const AtomicU32).cast_mut().cast();
        let mut token = 0;
        unsafe {
            assert_eq!(method::<Register>(&NETWORKING_IFACE, 0x80)(t, q.cast(), ctx, Some(count), &mut token), S_OK);
            assert!(method::<Unregister>(&NETWORKING_IFACE, 0x88)(t, token, 1), "a queued notice does not hold up the wait");
            assert!(taskqueue::dispatch(q, COMPLETION, 0), "the notice is still delivered");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "but not to an unregistered callback");
            taskqueue::close(q);
        }
        let q = make(Mode::Immediate, Mode::Immediate);
        unsafe {
            assert_eq!(method::<Register>(&NETWORKING_IFACE, 0x80)(t, q.cast(), ctx, Some(count), &mut token), S_OK);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(method::<Unregister>(&NETWORKING_IFACE, 0x88)(t, token, 1));
            taskqueue::close(q);
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
