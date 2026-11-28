//! `gameinput.dll`: the title's input, ours (Build 3; `agents/input.md` I3 has the method table).
//!
//! The title imports one function, ordinal 1 (`GameInputCreate`). The host has a `GameInput.dll` with the same
//! export, so without this the title would talk to the host's GameInput service directly: past the in-game
//! menu's isolation and past scripted input. [`bind`] gives it ours instead; `REBUILD_GAMEINPUT=host` gives it the
//! host's again.
//!
//! One static input object, six static devices (`rebuild_input::gameinput::DEVICES`: four pad slots, the
//! keyboard's stand-in pad, the keyboard), and counted readings. A thread of ours polls every 8 ms:
//!
//! - the overlay's title frame once the overlay runs (neutral while its menu is open), else the host directly;
//! - `REBUILD_INPUT_SCRIPT` presses added on top ([`rebuild_input::script`]), with `cap` steps asking the present
//!   for a capture; every step that starts is printed (`input     script ...`);
//! - a new reading per device whose state changed, kept in a history of 256 for next / previous / temporal
//!   lookups; device callbacks when a device comes or goes.
//!
//! Method tables follow the public GameInput v0 interfaces in declaration order (`IGameInput` 22 slots,
//! `IGameInputDevice` 21, `IGameInputReading` 25). What we cannot answer (raw reports, force feedback, mouse,
//! touch, the other controller kinds) returns nothing or `E_NOTIMPL`; every slot's calls are counted for the run
//! report ([`report`]).

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use rebuild_input::gameinput::{kind, Device as Dev, DEVICES, STATUS_CONNECTED, STATUS_NONE};
use rebuild_input::script::{self, Script};
use rebuild_input::{Frame, KeyState, PadState, Source};

type HRESULT = i32;
const S_OK: HRESULT = 0;
const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
/// GameInput's documented errors.
const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;

const N: usize = DEVICES.len();
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);
const ASYNC_ENUMERATION: u32 = 1;

/// `REBUILD_GAMEINPUT=host`: the host's GameInput, not ours.
pub fn enabled() -> bool {
    !std::env::var("REBUILD_GAMEINPUT").is_ok_and(|v| v.trim().eq_ignore_ascii_case("host"))
}

/// The resolver's hook: `gameinput.dll` ordinal 1 (or `GameInputCreate` by name) is ours.
pub fn bind(dll: &str, name: Option<&str>, ordinal: Option<u16>) -> Option<u64> {
    let ours = dll.eq_ignore_ascii_case("gameinput.dll") && (ordinal == Some(1) || name == Some("GameInputCreate"));
    (ours && enabled()).then(|| GameInputCreate as *const () as usize as u64)
}

// ------------------------------------------------------------------------------------------------ objects

#[repr(C)]
struct Table<const K: usize>([*const (); K]);
// SAFETY: function addresses only.
unsafe impl<const K: usize> Sync for Table<K> {}

#[repr(C)]
pub struct InputObject {
    vtable: &'static Table<22>,
}

/// `GameInputDeviceInfo` (v0): the title reads the id at 0x20; the rest is zero where we have nothing to say.
#[repr(C)]
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// counts and optional detail pointers, all zero / null
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20 && std::mem::offset_of!(DeviceInfo, supported_input) == 0x68);

#[repr(C)]
pub struct DeviceObject {
    vtable: &'static Table<21>,
    index: usize,
    info: DeviceInfo,
}

// SAFETY: immutable after construction.
unsafe impl Sync for DeviceObject {}

const fn device(index: usize) -> DeviceObject {
    let mut id = [0u8; 32];
    let tag = *b"rebuild-gameinput";
    let mut i = 0;
    while i < tag.len() {
        id[i] = tag[i];
        i += 1;
    }
    id[31] = index as u8 + 1;
    let keyboard = matches!(DEVICES[index], Dev::Keyboard);
    DeviceObject {
        vtable: &DEVICE,
        index,
        info: DeviceInfo {
            size: std::mem::size_of::<DeviceInfo>() as u32,
            vendor: 0,
            product: 0,
            revision: 0,
            interface: 0,
            collection: 0,
            usage: [0; 2],
            versions: [0; 8],
            id,
            root_id: id,
            family: 0,
            capabilities: 0,
            supported_input: if keyboard { kind::KEYBOARD } else { kind::GAMEPAD },
            // low and high frequency motors on the pads
            rumble_motors: if keyboard { 0 } else { 0x3 },
            rest: [0; 26],
        },
    }
}

static INPUT_OBJECT: InputObject = InputObject { vtable: &INPUT };
static DEVICE_OBJECTS: [DeviceObject; N] = [device(0), device(1), device(2), device(3), device(4), device(5)];

#[derive(Clone, PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    index: usize,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => kind::GAMEPAD,
            Value::Keys(_) => kind::KEYBOARD,
        }
    }
}

/// Our reference to a reading (the history's).
struct Ref(*const Reading);
// SAFETY: readings are immutable apart from their atomic count.
unsafe impl Send for Ref {}

impl Ref {
    fn get(&self) -> &Reading {
        // SAFETY: we hold a reference.
        unsafe { &*self.0 }
    }
    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0);
        self.0
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0);
    }
}

// ------------------------------------------------------------------------------------------------ state

type DeviceCallback = unsafe extern "system" fn(u64, *mut c_void, *const DeviceObject, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// one device's index, or any
    device: Option<usize>,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, index: usize) -> bool {
        DEVICES[index].kind() & self.kind != 0 && self.device.is_none_or(|d| d == index)
    }
}

struct Event {
    registration: Registration,
    index: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

#[derive(Default)]
struct State {
    slots: [Slot; N],
    callbacks: Vec<Registration>,
    /// asynchronous enumerations, delivered by the next poll
    pending: Vec<Event>,
    /// device callbacks running now: token, thread
    running: Vec<(u64, ThreadId)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static RETURNED: Condvar = Condvar::new();
static STARTED: AtomicBool = AtomicBool::new(false);
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
static RUMBLE: Mutex<Option<rebuild_input::host::Host>> = Mutex::new(None);

struct Guard(MutexGuard<'static, Option<State>>);
impl std::ops::Deref for Guard {
    type Target = State;
    fn deref(&self) -> &State {
        self.0.as_ref().expect("state made")
    }
}
impl std::ops::DerefMut for Guard {
    fn deref_mut(&mut self) -> &mut State {
        self.0.as_mut().expect("state made")
    }
}

fn state() -> Guard {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(State::default);
    Guard(g)
}

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// Microseconds: the clock of every timestamp we hand out.
fn now() -> u64 {
    epoch().elapsed().as_micros() as u64
}

impl State {
    /// One poll's frame: a reading per changed device, events for devices that came or went.
    fn update(&mut self, frame: &Frame, now: u64) -> Vec<Event> {
        let mut events = std::mem::take(&mut self.pending);
        for (index, d) in DEVICES.iter().enumerate() {
            let value = match d {
                Dev::Keyboard => Some(Value::Keys(frame.keys.clone())),
                _ => d.gamepad(frame).map(Value::Pad),
            };
            let slot = &mut self.slots[index];
            let was = slot.connected;
            slot.connected = value.is_some();
            if let Some(value) = value {
                if !was || slot.history.back().is_none_or(|r| r.get().value != value) {
                    slot.sequence += 1;
                    if slot.history.len() == HISTORY {
                        slot.history.pop_front();
                    }
                    let r = Box::new(Reading { vtable: &READING, refs: AtomicU32::new(1), index, sequence: slot.sequence, timestamp: now, value });
                    slot.history.push_back(Ref(Box::into_raw(r)));
                }
            }
            if was != slot.connected {
                let (current, previous) = if was { (STATUS_NONE, STATUS_CONNECTED) } else { (STATUS_CONNECTED, STATUS_NONE) };
                for r in self.callbacks.iter().filter(|r| r.matches(index) && r.status & STATUS_CONNECTED != 0) {
                    events.push(Event { registration: *r, index, timestamp: now, current, previous });
                }
            }
        }
        events
    }
}

/// Run callbacks outside our lock (they call back into us). One unregistered since its event was taken does not
/// run; `UnregisterCallback` waits for one that is running.
fn deliver(events: Vec<Event>) {
    let me = std::thread::current().id();
    for e in events {
        let r = e.registration;
        {
            let mut s = state();
            if !s.callbacks.iter().any(|c| c.token == r.token) {
                continue;
            }
            s.running.push((r.token, me));
        }
        unsafe { (r.callback)(r.token, r.context as *mut c_void, &DEVICE_OBJECTS[e.index], e.timestamp, e.current, e.previous) };
        let mut s = state();
        if let Some(i) = s.running.iter().position(|&x| x == (r.token, me)) {
            s.running.swap_remove(i);
        }
        RETURNED.notify_all();
    }
}

/// An overlay frame is real once it has a device: with no pad, the stand-in pad is always there. Before the
/// overlay's first poll its frame is empty, and taking that would disconnect every device for a moment.
fn overlay_frame() -> Option<Frame> {
    crate::overlay::title_input().filter(|f| f.stand_in.is_some() || f.pads.iter().any(Option::is_some))
}

struct Poller {
    host: rebuild_input::host::Host,
    script: Option<Script>,
}

impl Poller {
    fn frame(&mut self) -> Frame {
        let mut f = overlay_frame().unwrap_or_else(|| Frame::read(&mut self.host));
        if let Some(s) = self.script.as_mut() {
            let seconds = crate::framelog::elapsed().as_secs_f64();
            let frame = crate::framelog::frames();
            let p = s.poll(seconds, frame);
            for x in &p.fired {
                eprintln!("input     script {:.2} s, frame {}: {}", x.seconds, x.frame, x.text);
            }
            if p.capture {
                crate::present::request_capture(None);
            }
            if let Some(pad) = p.pad {
                script::apply(&mut f, pad);
            }
        }
        f
    }

    fn poll(&mut self) {
        let f = self.frame();
        let events = state().update(&f, now());
        deliver(events);
    }
}

fn start() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let script = Script::from_env();
    if let Some(s) = &script {
        eprintln!("input     REBUILD_INPUT_SCRIPT: {} steps", s.steps().len());
    }
    let mut p = Poller { host: rebuild_input::host::Host::new(), script };
    p.poll();
    if cfg!(test) {
        // tests drive the state themselves
        return;
    }
    let spawned = std::thread::Builder::new().name("gameinput-poll".into()).stack_size(256 << 10).spawn(move || loop {
        std::thread::sleep(POLL);
        p.poll();
    });
    if let Err(e) = spawned {
        eprintln!("gameinput no poll thread: {e}");
    }
}

// ------------------------------------------------------------------------------------------------ call counts

static CALLS: [[AtomicU32; 25]; 3] = [const { [const { AtomicU32::new(0) }; 25] }; 3];
const INPUT_NAMES: [&str; 22] = [
    "QueryInterface", "AddRef", "Release", "GetCurrentTimestamp", "GetCurrentReading", "GetNextReading", "GetPreviousReading",
    "GetTemporalReading", "RegisterReadingCallback", "RegisterDeviceCallback", "RegisterGuideButtonCallback",
    "RegisterKeyboardLayoutCallback", "StopCallback", "UnregisterCallback", "CreateDispatcher", "CreateAggregateDevice",
    "FindDeviceFromId", "FindDeviceFromObject", "FindDeviceFromPlatformHandle", "FindDeviceFromPlatformString",
    "EnableOemDeviceSupport", "SetFocusPolicy",
];
const DEVICE_NAMES: [&str; 21] = [
    "QueryInterface", "AddRef", "Release", "GetDeviceInfo", "GetDeviceStatus", "GetBatteryState", "CreateForceFeedbackEffect",
    "GetForceFeedbackMotorGain", "SetForceFeedbackMotorGain", "SetHapticMotorState", "SetRumbleState",
    "SetInputSynchronizationState", "SendInputSynchronizationHint", "PowerOff", "CreateRawDeviceReport", "GetRawDeviceFeature",
    "SetRawDeviceFeature", "SendRawDeviceOutput", "SendRawDeviceOutputWithResponse", "ExecuteRawDeviceIoControl",
    "AcquireExclusiveRawDeviceAccess",
];
const READING_NAMES: [&str; 25] = [
    "QueryInterface", "AddRef", "Release", "GetInputKind", "GetSequenceNumber", "GetTimestamp", "GetDevice", "GetRawReport",
    "GetControllerAxisCount", "GetControllerAxisState", "GetControllerButtonCount", "GetControllerButtonState",
    "GetControllerSwitchCount", "GetControllerSwitchState", "GetKeyCount", "GetKeyState", "GetMouseState", "GetTouchCount",
    "GetTouchState", "GetMotionState", "GetArcadeStickState", "GetFlightStickState", "GetGamepadState", "GetRacingWheelState",
    "GetUiNavigationState",
];

#[inline]
fn count(table: usize, slot: usize) {
    CALLS[table][slot].fetch_add(1, Ordering::Relaxed);
}

/// The run report: every method called, with its count; `None` when the title never created the object.
pub fn report() -> Option<String> {
    if !STARTED.load(Ordering::Relaxed) {
        return None;
    }
    let mut out = String::from("gameinput ours: methods called");
    for (t, (object, names)) in [("input", &INPUT_NAMES[..]), ("device", &DEVICE_NAMES[..]), ("reading", &READING_NAMES[..])].into_iter().enumerate() {
        for (i, n) in names.iter().enumerate() {
            let c = CALLS[t][i].load(Ordering::Relaxed);
            if c > 0 {
                out.push_str(&format!("\n  {object:<8} {:#06x} {n:<32} {c}×", i * 8));
            }
        }
    }
    let s = state();
    let connected: Vec<String> = DEVICES.iter().zip(&s.slots).filter(|(_, sl)| sl.connected).map(|(d, sl)| format!("{d:?} ({} readings)", sl.sequence)).collect();
    out.push_str(&format!("\n  devices  {}; {} device callbacks registered", connected.join(", "), s.callbacks.len()));
    Some(out)
}

// ------------------------------------------------------------------------------------------------ IGameInput

type Out = *mut *const c_void;

/// `gameinput.dll` ordinal 1: the input object (static, shared), polled from the first call on.
pub unsafe extern "system" fn GameInputCreate(out: *mut *const InputObject) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    start();
    unsafe { out.write(&INPUT_OBJECT) };
    S_OK
}

unsafe fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

unsafe fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(r) => {
            unsafe { out.write(r.share().cast()) };
            S_OK
        }
        None => unsafe { fail(out, READING_NOT_FOUND) },
    }
}

fn device_index(d: *const DeviceObject) -> Option<usize> {
    DEVICE_OBJECTS.iter().position(|x| std::ptr::eq(x, d))
}

unsafe extern "system" fn no_interface<const T: usize>(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    count(T, 0);
    unsafe { fail(out, E_NOINTERFACE) }
}
/// The input object and devices live as long as the process.
unsafe extern "system" fn static_ref<const T: usize, const S: usize>(_: *const c_void) -> u32 {
    count(T, S);
    1
}

unsafe extern "system" fn timestamp(_: *const InputObject) -> u64 {
    count(0, 3);
    now()
}

unsafe extern "system" fn current_reading(_: *const InputObject, kind: u32, device: *const DeviceObject, out: Out) -> HRESULT {
    count(0, 4);
    let s = state();
    let newest = |i: usize| s.slots[i].history.back().filter(|r| r.get().kind() & kind != 0);
    let found = if device.is_null() {
        (0..N).filter(|&i| s.slots[i].connected).filter_map(newest).max_by_key(|r| r.get().timestamp)
    } else {
        match device_index(device) {
            None => return unsafe { fail(out, E_INVALIDARG) },
            Some(i) if !s.slots[i].connected => return unsafe { fail(out, DEVICE_DISCONNECTED) },
            Some(i) => newest(i),
        }
    };
    unsafe { hand(out, found) }
}

/// The reading after (`forward`) or before `reference` on its device.
unsafe fn relative(reference: *const Reading, kind: u32, device: *const DeviceObject, out: Out, forward: bool) -> HRESULT {
    if reference.is_null() {
        return unsafe { fail(out, E_INVALIDARG) };
    }
    let reference = unsafe { &*reference };
    let index = reference.index;
    if !device.is_null() && device_index(device) != Some(index) {
        return unsafe { fail(out, E_INVALIDARG) };
    }
    let s = state();
    let h = &s.slots[index].history;
    if h.front().is_none_or(|oldest| reference.sequence < oldest.get().sequence) {
        return unsafe { fail(out, REFERENCE_TOO_OLD) };
    }
    let wanted = |r: &&Ref| r.get().kind() & kind != 0;
    let found = if forward {
        h.iter().filter(|r| r.get().sequence > reference.sequence).find(wanted)
    } else {
        h.iter().rev().filter(|r| r.get().sequence < reference.sequence).find(wanted)
    };
    unsafe { hand(out, found) }
}

unsafe extern "system" fn next_reading(_: *const InputObject, reference: *const Reading, kind: u32, device: *const DeviceObject, out: Out) -> HRESULT {
    count(0, 5);
    unsafe { relative(reference, kind, device, out, true) }
}
unsafe extern "system" fn previous_reading(_: *const InputObject, reference: *const Reading, kind: u32, device: *const DeviceObject, out: Out) -> HRESULT {
    count(0, 6);
    unsafe { relative(reference, kind, device, out, false) }
}
unsafe extern "system" fn temporal_reading(_: *const InputObject, time: u64, device: *const DeviceObject, out: Out) -> HRESULT {
    count(0, 7);
    let Some(i) = device_index(device) else { return unsafe { fail(out, E_INVALIDARG) } };
    let s = state();
    unsafe { hand(out, s.slots[i].history.iter().rev().find(|r| r.get().timestamp <= time)) }
}

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

/// Reading callbacks never run (titles poll); the token is real.
unsafe extern "system" fn register_reading(_: *const InputObject, _d: *const c_void, _k: u32, _t: f32, _c: *mut c_void, _f: *const c_void, out: *mut u64) -> HRESULT {
    count(0, 8);
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes. Enumeration reports the devices already
/// there: blocking, on the caller's thread before we return; asynchronous, on the next poll.
unsafe extern "system" fn register_device(
    _: *const InputObject,
    device: *const DeviceObject,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    count(0, 9);
    let Some(callback) = callback else { return E_INVALIDARG };
    let device = if device.is_null() {
        None
    } else {
        match device_index(device) {
            Some(i) => Some(i),
            None => return E_INVALIDARG,
        }
    };
    let r = Registration { token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed), device, kind, status, context: context as usize, callback };
    let mut found = Vec::new();
    {
        let mut s = state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let t = now();
            found = (0..N)
                .filter(|&i| s.slots[i].connected && r.matches(i))
                .map(|i| Event { registration: r, index: i, timestamp: t, current: STATUS_CONNECTED, previous: STATUS_NONE })
                .collect();
        }
        s.callbacks.push(r);
        if enumeration == ASYNC_ENUMERATION {
            s.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        unsafe { out.write(r.token) };
    }
    deliver(found);
    S_OK
}

/// Guide button and keyboard layout callbacks: neither ever changes here.
unsafe extern "system" fn register_other<const S: usize>(_: *const InputObject, _d: *const c_void, _c: *mut c_void, _f: *const c_void, out: *mut u64) -> HRESULT {
    count(0, S);
    token(out)
}

fn forget(token: u64) {
    let mut s = state();
    s.callbacks.retain(|r| r.token != token);
    s.pending.retain(|e| e.registration.token != token);
}

unsafe extern "system" fn stop_callback(_: *const InputObject, token: u64) {
    count(0, 12);
    forget(token);
}

/// Stops the callback, then waits up to `timeout` µs for a running call of it; false on time-out or from inside
/// that callback (it cannot wait for itself).
unsafe extern "system" fn unregister_callback(_: *const InputObject, token: u64, timeout: u64) -> bool {
    count(0, 13);
    forget(token);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let running: Vec<ThreadId> = g.as_ref().map(|s| s.running.iter().filter(|r| r.0 == token).map(|r| r.1).collect()).unwrap_or_default();
        if running.contains(&me) {
            return false;
        }
        if running.is_empty() {
            return true;
        }
        g = match deadline {
            None => RETURNED.wait(g).unwrap_or_else(|e| e.into_inner()),
            Some(d) => {
                let Some(left) = d.checked_duration_since(Instant::now()) else { return false };
                RETURNED.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0
            }
        };
    }
}

unsafe extern "system" fn not_made<const S: usize>(_: *const InputObject, _a: usize, out: Out) -> HRESULT {
    count(0, S);
    // CreateDispatcher(out) has `out` first; CreateAggregateDevice(kind, out) second
    let out = if S == 14 { _a as Out } else { out };
    unsafe { fail(out, E_NOTIMPL) }
}

unsafe extern "system" fn find_by_id(_: *const InputObject, id: *const [u8; 32], out: Out) -> HRESULT {
    count(0, 16);
    if id.is_null() || out.is_null() {
        return unsafe { fail(out, E_INVALIDARG) };
    }
    let id = unsafe { &*id };
    match DEVICE_OBJECTS.iter().find(|d| d.info.id == *id) {
        Some(d) => {
            unsafe { out.write((d as *const DeviceObject).cast()) };
            S_OK
        }
        None => unsafe { fail(out, DEVICE_NOT_FOUND) },
    }
}

/// By object, platform handle or platform string: our devices have none.
unsafe extern "system" fn find_device<const S: usize>(_: *const InputObject, _v: *const c_void, out: Out) -> HRESULT {
    count(0, S);
    unsafe { fail(out, DEVICE_NOT_FOUND) }
}

unsafe extern "system" fn enable_oem(_: *const InputObject, _v: u16, _p: u16, _i: u8, _c: u8) -> HRESULT {
    count(0, 20);
    S_OK
}

unsafe extern "system" fn set_focus_policy(_: *const InputObject, _policy: u32) {
    count(0, 21);
}

static INPUT: Table<22> = Table([
    no_interface::<0> as *const (),
    static_ref::<0, 1> as *const (),
    static_ref::<0, 2> as *const (),
    timestamp as *const (),
    current_reading as *const (),
    next_reading as *const (),
    previous_reading as *const (),
    temporal_reading as *const (),
    register_reading as *const (),
    register_device as *const (),
    register_other::<10> as *const (),
    register_other::<11> as *const (),
    stop_callback as *const (),
    unregister_callback as *const (),
    not_made::<14> as *const (),
    not_made::<15> as *const (),
    find_by_id as *const (),
    find_device::<17> as *const (),
    find_device::<18> as *const (),
    find_device::<19> as *const (),
    enable_oem as *const (),
    set_focus_policy as *const (),
]);

// ------------------------------------------------------------------------------------------------ IGameInputDevice

unsafe extern "system" fn device_info(this: *const DeviceObject) -> *const DeviceInfo {
    count(1, 3);
    unsafe { &(*this).info }
}

unsafe extern "system" fn device_status(this: *const DeviceObject) -> u32 {
    count(1, 4);
    let i = unsafe { (*this).index };
    if state().slots[i].connected {
        STATUS_CONNECTED
    } else {
        STATUS_NONE
    }
}

/// `GameInputBatteryState`: charge rate, max charge rate, remaining, full capacity, status. All zero, status
/// "not present" (a wired pad).
unsafe extern "system" fn battery(_: *const DeviceObject, out: *mut [u32; 5]) {
    count(1, 5);
    if !out.is_null() {
        unsafe { out.write([0; 5]) };
    }
}

/// `GameInputRumbleParams` (low, high, left trigger, right trigger), 0 to 1. Trigger motors are dropped (XInput
/// has none).
unsafe extern "system" fn set_rumble(this: *const DeviceObject, params: *const [f32; 4]) {
    count(1, 10);
    let Dev::Pad(slot) = DEVICES[unsafe { (*this).index }] else { return };
    let [low, high, ..] = if params.is_null() { [0.0; 4] } else { unsafe { *params } };
    let speed = |v: f32| (v.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut g = RUMBLE.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(rebuild_input::host::Host::new).rumble(slot, speed(low), speed(high));
}

unsafe extern "system" fn device_unsupported<const S: usize>(_: *const c_void) -> HRESULT {
    count(1, S);
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface::<1> as *const (),
    static_ref::<1, 1> as *const (),
    static_ref::<1, 2> as *const (),
    device_info as *const (),
    device_status as *const (),
    battery as *const (),
    device_unsupported::<6> as *const (),
    device_unsupported::<7> as *const (),
    device_unsupported::<8> as *const (),
    device_unsupported::<9> as *const (),
    set_rumble as *const (),
    device_unsupported::<11> as *const (),
    device_unsupported::<12> as *const (),
    device_unsupported::<13> as *const (),
    device_unsupported::<14> as *const (),
    device_unsupported::<15> as *const (),
    device_unsupported::<16> as *const (),
    device_unsupported::<17> as *const (),
    device_unsupported::<18> as *const (),
    device_unsupported::<19> as *const (),
    device_unsupported::<20> as *const (),
]);

// ------------------------------------------------------------------------------------------------ IGameInputReading

fn reading<'a>(this: *const Reading) -> &'a Reading {
    unsafe { &*this }
}

unsafe extern "system" fn reading_add_ref_slot(this: *const Reading) -> u32 {
    count(2, 1);
    reading_add_ref(this)
}
unsafe extern "system" fn reading_release_slot(this: *const Reading) -> u32 {
    count(2, 2);
    reading_release(this)
}
fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
unsafe extern "system" fn input_kind(this: *const Reading) -> u32 {
    count(2, 3);
    reading(this).kind()
}
unsafe extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    count(2, 4);
    let r = reading(this);
    if r.kind() & kind != 0 {
        r.sequence
    } else {
        0
    }
}
unsafe extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    count(2, 5);
    reading(this).timestamp
}
unsafe extern "system" fn reading_device(this: *const Reading, out: *mut *const DeviceObject) {
    count(2, 6);
    if !out.is_null() {
        unsafe { out.write(&DEVICE_OBJECTS[reading(this).index]) };
    }
}
unsafe extern "system" fn key_count(this: *const Reading) -> u32 {
    count(2, 14);
    match &reading(this).value {
        Value::Keys(k) => k.len() as u32,
        Value::Pad(_) => 0,
    }
}
unsafe extern "system" fn key_state(this: *const Reading, n: u32, out: *mut KeyState) -> u32 {
    count(2, 15);
    let Value::Keys(keys) = &reading(this).value else { return 0 };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(n as usize);
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
unsafe extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    count(2, 22);
    match &reading(this).value {
        Value::Pad(p) if !out.is_null() => {
            unsafe { out.write(*p) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state (false / 0).
unsafe extern "system" fn nothing<const S: usize>(_: *const c_void) -> usize {
    count(2, S);
    0
}

static READING: Table<25> = Table([
    no_interface::<2> as *const (),
    reading_add_ref_slot as *const (),
    reading_release_slot as *const (),
    input_kind as *const (),
    sequence_number as *const (),
    reading_timestamp as *const (),
    reading_device as *const (),
    nothing::<7> as *const (),
    nothing::<8> as *const (),
    nothing::<9> as *const (),
    nothing::<10> as *const (),
    nothing::<11> as *const (),
    nothing::<12> as *const (),
    nothing::<13> as *const (),
    key_count as *const (),
    key_state as *const (),
    nothing::<16> as *const (),
    nothing::<17> as *const (),
    nothing::<18> as *const (),
    nothing::<19> as *const (),
    nothing::<20> as *const (),
    nothing::<21> as *const (),
    gamepad_state as *const (),
    nothing::<23> as *const (),
    nothing::<24> as *const (),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_input::pad::{buttons, NEUTRAL};

    fn slot<F: Copy>(table: *const *const (), i: usize) -> F {
        let p = unsafe { *table.add(i) };
        unsafe { std::mem::transmute_copy(&p) }
    }

    #[test]
    fn binds_ordinal_one_of_gameinput_only() {
        assert!(bind("GameInput.dll", None, Some(1)).is_some() == enabled());
        assert!(bind("gameinput.dll", Some("GameInputCreate"), None).is_some() == enabled());
        assert!(bind("gameinput.dll", None, Some(2)).is_none());
        assert!(bind("xinput1_4.dll", None, Some(1)).is_none());
    }

    /// Through the method tables, as the title calls them: create, enumerate (blocking), read the pad, see a
    /// change as a new reading with the old one still reachable, release.
    #[test]
    fn readings_and_device_callbacks_through_the_tables() {
        let mut obj: *const InputObject = std::ptr::null();
        assert_eq!(unsafe { GameInputCreate(&mut obj) }, S_OK);
        let vt = unsafe { (*obj).vtable.0.as_ptr() };
        // drive the state directly (the poll thread also runs; it reads the host, so pin a frame per update)
        let mut f = Frame { stand_in: Some(NEUTRAL), ..Frame::default() };
        f.pads = [None; 4];
        let events = state().update(&f, now());
        deliver(events);

        static SEEN: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn cb(_: u64, _: *mut c_void, d: *const DeviceObject, _: u64, cur: u32, _: u32) {
            if cur == STATUS_CONNECTED && device_index(d).is_some() {
                SEEN.fetch_add(1, Ordering::Relaxed);
            }
        }
        let register: unsafe extern "system" fn(*const InputObject, *const DeviceObject, u32, u32, u32, *mut c_void, Option<DeviceCallback>, *mut u64) -> HRESULT = slot(vt, 9);
        let mut token = 0u64;
        assert_eq!(unsafe { register(obj, std::ptr::null(), kind::GAMEPAD, STATUS_CONNECTED, 2, std::ptr::null_mut(), Some(cb), &mut token) }, S_OK);
        assert!(token != 0);
        assert!(SEEN.load(Ordering::Relaxed) >= 1, "blocking enumeration reports the connected pad");

        let current: unsafe extern "system" fn(*const InputObject, u32, *const DeviceObject, Out) -> HRESULT = slot(vt, 4);
        let mut r: *const c_void = std::ptr::null();
        assert_eq!(unsafe { current(obj, kind::GAMEPAD, std::ptr::null(), &mut r) }, S_OK);
        let rvt = unsafe { (*(r as *const Reading)).vtable.0.as_ptr() };
        let pad: unsafe extern "system" fn(*const c_void, *mut PadState) -> bool = slot(rvt, 22);
        let mut p = PadState { buttons: 0xFFFF, ..NEUTRAL };
        assert!(unsafe { pad(r, &mut p) });
        assert_eq!(p.buttons & buttons::A, 0);
        let get_dev: unsafe extern "system" fn(*const c_void, *mut *const DeviceObject) = slot(rvt, 6);
        let mut d: *const DeviceObject = std::ptr::null();
        unsafe { get_dev(r, &mut d) };
        let dvt = unsafe { (*d).vtable.0.as_ptr() };
        let status: unsafe extern "system" fn(*const DeviceObject) -> u32 = slot(dvt, 4);
        assert_eq!(unsafe { status(d) }, STATUS_CONNECTED);
        let info: unsafe extern "system" fn(*const DeviceObject) -> *const DeviceInfo = slot(dvt, 3);
        assert_eq!(unsafe { (*info(d)).supported_input }, kind::GAMEPAD);

        // a press on the same device: a newer reading, and next-from-the-old finds it
        let i = device_index(d).unwrap();
        let mut f2 = f.clone();
        match DEVICES[i] {
            Dev::Pad(s) => f2.pads[s] = Some(PadState { buttons: buttons::A, ..NEUTRAL }),
            _ => f2.stand_in = Some(PadState { buttons: buttons::A, ..NEUTRAL }),
        }
        let events = state().update(&f2, now() + 1);
        deliver(events);
        let next: unsafe extern "system" fn(*const InputObject, *const c_void, u32, *const DeviceObject, Out) -> HRESULT = slot(vt, 5);
        let mut r2: *const c_void = std::ptr::null();
        assert_eq!(unsafe { next(obj, r, kind::GAMEPAD, d, &mut r2) }, S_OK);
        let seq: unsafe extern "system" fn(*const c_void, u32) -> u64 = slot(rvt, 4);
        assert!(unsafe { seq(r2, kind::GAMEPAD) } > unsafe { seq(r, kind::GAMEPAD) });
        assert!(unsafe { pad(r2, &mut p) });
        assert_eq!(p.buttons & buttons::A, buttons::A);
        let release: unsafe extern "system" fn(*const c_void) -> u32 = slot(rvt, 2);
        unsafe {
            release(r);
            release(r2);
        }
        let unregister: unsafe extern "system" fn(*const InputObject, u64, u64) -> bool = slot(vt, 13);
        assert!(unsafe { unregister(obj, token, 1000) });
        assert!(report().is_some_and(|r| r.contains("GetCurrentReading")));
    }
}
