//! The title's own sound output: the host's WASAPI, reached through COM as on PC, watched from inside.
//!
//! The title's mixer (and Bink's sound, and anything built on XAudio2) leaves through the host's audio engine:
//! `CoCreateInstance(MMDeviceEnumerator)` → `IMMDevice::Activate(IAudioClient)` → `Initialize` → `GetService
//! (IAudioRenderClient)` → `GetBuffer`/`ReleaseBuffer`. Those calls bind to the host (they are not imports we
//! serve), so [`install`] puts counting hooks in the host classes' method tables before the title runs: every
//! client in the process uses the same tables, so every stream is seen, whoever opened it. The hooks cost a few
//! hundred nanoseconds per call on the title's audio thread and do nothing else unless asked:
//!
//! - `REBUILD_AUDIO_SINK=<file.wav>`: every stream's buffers are also written to `<file>-<n>.wav` (`n` from 1, in
//!   the stream's own format; spatial streams `<file>-spatial-<n>.wav`), on a writer thread: exactly what the
//!   title handed the engine.
//! - `REBUILD_AUDIO_VOLUME=<0..1>|mute`: a gain on the title's buffers after the tap (float and 16-bit; other
//!   formats get the session volume instead), for runs that must not play out loud. The process loopback
//!   recording (`crate::loopback`) is taken after it: its level is the tap's times the gain.
//!
//! XAudio2 (the host's 2.9, which the title and Bink use) renders through spatial audio streams: `crate::spatial`.
//!
//! [`report`] gives the counts for the run report: activations by interface, streams with their formats and
//! results, buffers and frames, frames short of real time (underruns), and which host audio DLLs are loaded.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDeviceEnumerator, ISimpleAudioVolume, ISpatialAudioClient, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};

use crate::lock;
use crate::wav::{Format, Writer};

// ------------------------------------------------------------------------------------------------ the counts

/// Interfaces asked of `IMMDevice::Activate`, by name, with the calls, failures and the first caller's chain.
static ACTIVATIONS: Mutex<Vec<(String, u64, u64, String)>> = Mutex::new(Vec::new());
static STREAMS: Mutex<Vec<Stream>> = Mutex::new(Vec::new());
/// Calls into the hooked tables, all kinds.
pub static CALLS: AtomicU64 = AtomicU64::new(0);
/// Hooked render clients whose table was not the one hooked (a second class): their buffers are not seen.
pub static OTHER_TABLES: AtomicU64 = AtomicU64::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

struct Stream {
    client: usize,
    render: usize,
    format: Option<Format>,
    share: u32,
    flags: u32,
    duration: i64,
    hr: i32,
    buffer_frames: u32,
    started: Option<Instant>,
    /// time spent stopped before the last start, so the real-time check counts playing time only
    played: f64,
    frames: u64,
    buffers: u64,
    silent: u64,
    get_failed: u64,
    /// `GetCurrentPadding` answered 0 while playing, after data had been written: the engine ran dry
    dry: u64,
    was_dry: bool,
    last: usize,
    sink: Option<u32>,
}

impl Stream {
    fn new(client: usize) -> Stream {
        Stream {
            client,
            render: 0,
            format: None,
            share: 0,
            flags: 0,
            duration: 0,
            hr: 0,
            buffer_frames: 0,
            started: None,
            played: 0.0,
            frames: 0,
            buffers: 0,
            silent: 0,
            get_failed: 0,
            dry: 0,
            was_dry: false,
            last: 0,
            sink: None,
        }
    }

    fn playing_seconds(&self) -> f64 {
        self.played + self.started.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }
}

fn with_stream<R>(client: usize, f: impl FnOnce(&mut Stream) -> R) -> R {
    let mut s = lock(&STREAMS);
    let i = match s.iter().rposition(|x| x.client == client) {
        Some(i) => i,
        None => {
            s.push(Stream::new(client));
            s.len() - 1
        }
    };
    f(&mut s[i])
}

fn with_render<R>(render: usize, f: impl FnOnce(&mut Stream) -> R) -> Option<R> {
    let mut s = lock(&STREAMS);
    s.iter_mut().rev().find(|x| x.render == render).map(f)
}

// ------------------------------------------------------------------------------------------------- the sink

pub(crate) enum Msg {
    Open(u32, Format),
    Data(u32, Vec<u8>),
    Silence(u32, u64),
    Flush(std::sync::mpsc::Sender<()>),
}

fn sink_base() -> Option<&'static PathBuf> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| std::env::var_os("REBUILD_AUDIO_SINK").filter(|v| !v.is_empty()).map(PathBuf::from)).as_ref()
}

fn sink() -> Option<&'static Mutex<Sender<Msg>>> {
    static S: OnceLock<Option<Mutex<Sender<Msg>>>> = OnceLock::new();
    S.get_or_init(|| {
        let base = sink_base()?.clone();
        let (tx, rx) = channel::<Msg>();
        std::thread::Builder::new()
            .name("rebuild audio sink".into())
            .spawn(move || {
                let mut files: Vec<(u32, Writer)> = Vec::new();
                for m in rx {
                    match m {
                        Msg::Open(n, f) => {
                            let stem = base.file_stem().map_or("audio".into(), |s| s.to_string_lossy().into_owned());
                            // spatial streams are numbered from 1001 (`crate::spatial`)
                            let label = if n > 1000 { format!("spatial-{}", n - 1000) } else { n.to_string() };
                            let path = base.with_file_name(format!("{stem}-{label}.wav"));
                            match Writer::create(&path, f) {
                                Ok(w) => files.push((n, w)),
                                Err(e) => eprintln!("audio     sink {}: {e}", path.display()),
                            }
                        }
                        Msg::Data(n, d) => {
                            if let Some((_, w)) = files.iter_mut().find(|(k, _)| *k == n) {
                                let _ = w.write(&d);
                            }
                        }
                        Msg::Silence(n, frames) => {
                            if let Some((_, w)) = files.iter_mut().find(|(k, _)| *k == n) {
                                let _ = w.silence(frames);
                            }
                        }
                        Msg::Flush(done) => {
                            for (_, w) in &mut files {
                                let _ = w.finish();
                            }
                            let _ = done.send(());
                        }
                    }
                }
            })
            .ok()?;
        Some(Mutex::new(tx))
    })
    .as_ref()
}

pub(crate) fn send_sink(m: Msg) {
    if let Some(s) = sink() {
        let _ = lock(s).send(m);
    }
}

/// Finishes the sink's files (sizes written), waiting up to a second; for the end of a run.
pub fn flush() {
    if let Some(s) = sink() {
        let (tx, rx) = channel();
        let _ = lock(s).send(Msg::Flush(tx));
        let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
    }
}

static SINKS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn sink_wanted() -> bool {
    sink_base().is_some()
}

/// `REBUILD_AUDIO_VOLUME` as a gain on the title's buffers (after the tap): `None` when not set.
pub(crate) fn gain() -> Option<f32> {
    volume().map(|(v, mute)| if mute { 0.0 } else { v })
}

/// `REBUILD_AUDIO_VOLUME`: `Some((volume, mute))`.
fn volume() -> Option<(f32, bool)> {
    static V: OnceLock<Option<(f32, bool)>> = OnceLock::new();
    *V.get_or_init(|| {
        let v = std::env::var("REBUILD_AUDIO_VOLUME").ok()?;
        let v = v.trim();
        if v.eq_ignore_ascii_case("mute") {
            return Some((0.0, true));
        }
        v.parse::<f32>().ok().map(|x| (x.clamp(0.0, 1.0), false))
    })
}

// ----------------------------------------------------------------------------------------------- the hooks

/// The real functions behind the hooked slots.
struct Real {
    activate: AtomicUsize,
    initialize: AtomicUsize,
    padding: AtomicUsize,
    start: AtomicUsize,
    stop: AtomicUsize,
    service: AtomicUsize,
    shared_stream: AtomicUsize,
    get_buffer: AtomicUsize,
    release_buffer: AtomicUsize,
    /// the hooked tables
    client_table: AtomicUsize,
    render_table: AtomicUsize,
}

static REAL: Real = Real {
    activate: AtomicUsize::new(0),
    initialize: AtomicUsize::new(0),
    padding: AtomicUsize::new(0),
    start: AtomicUsize::new(0),
    stop: AtomicUsize::new(0),
    service: AtomicUsize::new(0),
    shared_stream: AtomicUsize::new(0),
    get_buffer: AtomicUsize::new(0),
    release_buffer: AtomicUsize::new(0),
    client_table: AtomicUsize::new(0),
    render_table: AtomicUsize::new(0),
};

/// Puts `ours` in slot `slot` of `table` and returns what was there.
pub(crate) unsafe fn patch(table: *mut usize, slot: usize, ours: usize) -> Result<usize, String> {
    let p = table.add(slot);
    let mut old = PAGE_PROTECTION_FLAGS(0);
    VirtualProtect(p.cast(), 8, PAGE_READWRITE, &mut old).map_err(|e| format!("VirtualProtect: {e}"))?;
    let was = p.read();
    p.write(ours);
    let mut back = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(p.cast(), 8, old, &mut back);
    Ok(was)
}

pub(crate) unsafe fn table_of(obj: *mut c_void) -> *mut usize {
    *(obj as *const *mut usize)
}

fn interface_name(iid: &GUID) -> String {
    let known: [(u128, &str); 9] = [
        (0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2, "IAudioClient"),
        (0x726778cd_f60a_4eda_82de_e47610cd78aa, "IAudioClient2"),
        (0x7ed4ee07_8e67_4cd4_8c1a_2b7a5987ad42, "IAudioClient3"),
        (0xbbf8e066_aaaa_49be_9a4d_fd2a858ea27f, "ISpatialAudioClient"),
        (0x5cdf2c82_841e_4546_9722_0cf74078229a, "IAudioEndpointVolume"),
        (0x77aa99a0_1bd6_484f_8bc7_2c654c9a9b6f, "IAudioSessionManager2"),
        (0xbfa971f1_4d5e_40bb_935e_967039bfbee4, "IAudioSessionManager"),
        (0xc02216f6_8c67_4b5b_9d00_d008e73e0064, "IAudioMeterInformation"),
        (0x2a07407e_6497_4a18_9787_32f79bd0d98f, "IDeviceTopology"),
    ];
    let v = iid.to_u128();
    known.iter().find(|(k, _)| *k == v).map_or_else(|| format!("{iid:?}"), |(_, n)| n.to_string())
}

type ActivateFn = unsafe extern "system" fn(*mut c_void, *const GUID, u32, *const c_void, *mut *mut c_void) -> HRESULT;
type InitializeFn = unsafe extern "system" fn(*mut c_void, i32, u32, i64, i64, *const u8, *const GUID) -> HRESULT;
type PaddingFn = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;
type ThisFn = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type ServiceFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type SharedStreamFn = unsafe extern "system" fn(*mut c_void, u32, u32, *const u8, *const GUID) -> HRESULT;
type GetBufferFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HRESULT;
type ReleaseBufferFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
type BufferSizeFn = unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT;

unsafe fn real<F: Copy>(slot: &AtomicUsize) -> F {
    let p = slot.load(Relaxed);
    debug_assert!(p != 0);
    std::mem::transmute_copy(&p)
}

unsafe extern "system" fn activate(this: *mut c_void, iid: *const GUID, ctx: u32, params: *const c_void, out: *mut *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ActivateFn>(&REAL.activate)(this, iid, ctx, params, out);
    if !iid.is_null() {
        let name = interface_name(&*iid);
        let mut a = lock(&ACTIVATIONS);
        match a.iter_mut().find(|x| x.0 == name) {
            Some(x) => {
                x.1 += 1;
                x.2 += u64::from(hr.is_err());
            }
            None => a.push((name, 1, u64::from(hr.is_err()), caller_chain())),
        }
    }
    hr
}

unsafe extern "system" fn initialize(this: *mut c_void, share: i32, flags: u32, duration: i64, period: i64, format: *const u8, session: *const GUID) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<InitializeFn>(&REAL.initialize)(this, share, flags, duration, period, format, session);
    initialized(this, share as u32, flags, duration, format, hr);
    hr
}

unsafe extern "system" fn shared_stream(this: *mut c_void, flags: u32, period: u32, format: *const u8, session: *const GUID) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<SharedStreamFn>(&REAL.shared_stream)(this, flags, period, format, session);
    initialized(this, 0, flags, i64::from(period), format, hr);
    hr
}

unsafe fn initialized(this: *mut c_void, share: u32, flags: u32, duration: i64, format: *const u8, hr: HRESULT) {
    let f = Format::from_waveformatex(format);
    let mut frames = 0u32;
    if hr.is_ok() {
        // the buffer size, through the client's own (unhooked) slot 4
        let size: BufferSizeFn = std::mem::transmute(*table_of(this).add(4));
        let _ = size(this, &mut frames);
    }
    // a new stream every time (a later client can reuse a released one's address)
    {
        let mut v = lock(&STREAMS);
        let mut s = Stream::new(this as usize);
        s.format = f;
        s.share = share;
        s.flags = flags;
        s.duration = duration;
        s.hr = hr.0;
        s.buffer_frames = frames;
        v.push(s);
    }
    let scalable = f.is_some_and(|f| (f.float && f.bits == 32) || (!f.float && f.bits == 16));
    if hr.is_ok() && !scalable {
        if let Some((v, mute)) = volume() {
            let mut p: *mut c_void = std::ptr::null_mut();
            if real::<ServiceFn>(&REAL.service)(this, &ISimpleAudioVolume::IID, &mut p).is_ok() && !p.is_null() {
                let vol = ISimpleAudioVolume::from_raw(p);
                let _ = vol.SetMasterVolume(v, std::ptr::null());
                let _ = vol.SetMute(windows::Win32::Foundation::BOOL::from(mute), std::ptr::null());
            }
        }
    }
}

unsafe extern "system" fn padding(this: *mut c_void, out: *mut u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<PaddingFn>(&REAL.padding)(this, out);
    if hr.is_ok() && !out.is_null() {
        let p = *out;
        with_stream(this as usize, |s| {
            let dry = p == 0 && s.started.is_some() && s.frames > 0;
            if dry && !s.was_dry {
                s.dry += 1;
            }
            s.was_dry = dry;
        });
    }
    hr
}

unsafe extern "system" fn start(this: *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ThisFn>(&REAL.start)(this);
    if hr.is_ok() {
        with_stream(this as usize, |s| {
            if s.started.is_none() {
                s.started = Some(Instant::now());
            }
        });
    }
    hr
}

unsafe extern "system" fn stop(this: *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ThisFn>(&REAL.stop)(this);
    with_stream(this as usize, |s| {
        if let Some(t) = s.started.take() {
            s.played += t.elapsed().as_secs_f64();
        }
    });
    hr
}

unsafe extern "system" fn service(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ServiceFn>(&REAL.service)(this, iid, out);
    if hr.is_ok() && !iid.is_null() && *iid == IAudioRenderClient::IID && !out.is_null() && !(*out).is_null() {
        let render = *out;
        let table = table_of(render) as usize;
        if REAL.render_table.load(Relaxed) == 0 {
            // the first render client: hook its class's table (every later one shares it)
            let _g = lock(&HOOKING);
            if REAL.render_table.load(Relaxed) == 0 {
                let t = table as *mut usize;
                if let (Ok(g), Ok(r)) = (patch(t, 3, get_buffer as *const () as usize), patch(t, 4, release_buffer as *const () as usize)) {
                    REAL.get_buffer.store(g, Relaxed);
                    REAL.release_buffer.store(r, Relaxed);
                    REAL.render_table.store(table, Relaxed);
                }
            }
        } else if REAL.render_table.load(Relaxed) != table {
            OTHER_TABLES.fetch_add(1, Relaxed);
        }
        let sink_format = with_stream(this as usize, |s| {
            s.render = render as usize;
            if sink_base().is_some() && s.sink.is_none() {
                if let Some(f) = s.format {
                    let n = SINKS.fetch_add(1, Relaxed) as u32 + 1;
                    s.sink = Some(n);
                    return Some((n, f));
                }
            }
            None
        });
        if let Some((n, f)) = sink_format {
            send_sink(Msg::Open(n, f));
        }
    }
    hr
}

static HOOKING: Mutex<()> = Mutex::new(());

// ------------------------------------------------------------------- the client's other methods, counted

/// `IAudioClient` slots watched for calls only (what a caller does before or instead of `Initialize`), by slot:
/// IsFormatSupported 7, GetMixFormat 8, GetDevicePeriod 9, IAudioClient2's IsOffloadCapable 15, SetClientProperties 16,
/// GetBufferSizeLimits 17, IAudioClient3's GetSharedModeEnginePeriod 18, GetCurrentSharedModeEnginePeriod 19.
const COUNTED: [(usize, &str); 8] = [
    (7, "IsFormatSupported"),
    (8, "GetMixFormat"),
    (9, "GetDevicePeriod"),
    (15, "IsOffloadCapable"),
    (16, "SetClientProperties"),
    (17, "GetBufferSizeLimits"),
    (18, "GetSharedModeEnginePeriod"),
    (19, "GetCurrentSharedModeEnginePeriod"),
];
static COUNTED_REAL: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
/// per counted slot: calls, failures, the first failure's HRESULT, and (IsFormatSupported) the formats asked
static COUNTED_CALLS: Mutex<[(u64, u64, i32); 8]> = Mutex::new([(0, 0, 0); 8]);
static FORMATS_ASKED: Mutex<Vec<(String, i32)>> = Mutex::new(Vec::new());

fn format_text(f: Option<Format>) -> String {
    f.map_or("format not read".to_string(), |f| {
        format!("{} Hz {} ch {}-bit {}{}", f.rate, f.channels, f.bits, if f.float { "float" } else { "PCM" }, if f.mask != 0 { format!(" mask {:#x}", f.mask) } else { String::new() })
    })
}

/// Six integer arguments forwarded (the slots take at most six; unused ones are the caller's stack, ignored).
unsafe fn counted_call<const I: usize>(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let real: unsafe extern "system" fn(u64, u64, u64, u64, u64, u64) -> HRESULT = std::mem::transmute(COUNTED_REAL[I].load(Relaxed));
    let hr = real(a, b, c, d, e, f);
    {
        let mut k = lock(&COUNTED_CALLS);
        k[I].0 += 1;
        if hr.is_err() {
            if k[I].1 == 0 {
                k[I].2 = hr.0;
            }
            k[I].1 += 1;
        }
    }
    // IsFormatSupported(this, share, format, closest): the format asked, with the answer (S_FALSE 1: a closest one)
    if COUNTED[I].0 == 7 && c != 0 {
        let asked = format!("{} {}", if b == 1 { "exclusive" } else { "shared" }, format_text(Format::from_waveformatex(c as *const u8)));
        let mut v = lock(&FORMATS_ASKED);
        if v.len() < 8 && !v.iter().any(|x| x.0 == asked && x.1 == hr.0) {
            v.push((asked, hr.0));
        }
    }
    hr
}

macro_rules! counted {
    ($($i:literal)*) => {
        [$({
            unsafe extern "system" fn hook(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> HRESULT {
                counted_call::<$i>(a, b, c, d, e, f)
            }
            hook as usize
        }),*]
    };
}

fn counted_hooks() -> [usize; 8] {
    counted!(0 1 2 3 4 5 6 7)
}

unsafe extern "system" fn get_buffer(this: *mut c_void, frames: u32, out: *mut *mut u8) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<GetBufferFn>(&REAL.get_buffer)(this, frames, out);
    let p = if hr.is_ok() && !out.is_null() { *out as usize } else { 0 };
    with_render(this as usize, |s| {
        s.last = p;
        s.get_failed += u64::from(hr.is_err());
    });
    hr
}

const SILENT: u32 = 2; // AUDCLNT_BUFFERFLAGS_SILENT

unsafe extern "system" fn release_buffer(this: *mut c_void, frames: u32, flags: u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let tap = with_render(this as usize, |s| {
        s.frames += u64::from(frames);
        s.buffers += 1;
        let silent = flags & SILENT != 0;
        s.silent += u64::from(silent);
        let last = std::mem::take(&mut s.last);
        match s.format {
            Some(f) if frames > 0 => Some((s.sink, f, last, silent)),
            _ => None,
        }
    })
    .flatten();
    if let Some((n, f, p, silent)) = tap {
        if let Some(n) = n {
            if silent || p == 0 {
                send_sink(Msg::Silence(n, u64::from(frames)));
            } else {
                // the buffer is the title's until the real ReleaseBuffer below
                let bytes = std::slice::from_raw_parts(p as *const u8, frames as usize * f.block());
                send_sink(Msg::Data(n, bytes.to_vec()));
            }
        }
        // REBUILD_AUDIO_VOLUME, after the tap: float and 16-bit buffers scaled here (others took a session volume)
        if let (Some(g), false, true) = (gain(), silent, p != 0) {
            let samples = frames as usize * f.channels as usize;
            if f.float && f.bits == 32 {
                for v in std::slice::from_raw_parts_mut(p as *mut f32, samples) {
                    *v *= g;
                }
            } else if !f.float && f.bits == 16 {
                for v in std::slice::from_raw_parts_mut(p as *mut i16, samples) {
                    *v = (f32::from(*v) * g) as i16;
                }
            }
        }
    }
    real::<ReleaseBufferFn>(&REAL.release_buffer)(this, frames, flags)
}

/// Who called: up to six return addresses as `module+offset`, after the frames of the host's audio and COM DLLs.
pub(crate) fn caller_chain() -> String {
    use windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    let mut frames = [std::ptr::null_mut::<c_void>(); 24];
    // SAFETY: room for 24 frames
    // from 2: past this function and the hook that called it
    let n = unsafe { RtlCaptureStackBackTrace(2, &mut frames, None) } as usize;
    let skip = ["mmdevapi.dll", "audioses.dll", "combase.dll"];
    frames[..n]
        .iter()
        .map(|&f| module_offset(f as usize))
        .skip_while(|s| skip.iter().any(|m| s.to_ascii_lowercase().starts_with(m)))
        .take(6)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `module+offset` for a code address (the title's images are loaded by the Windows loader, so they are modules).
fn module_offset(addr: usize) -> String {
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT};
    let mut m = windows::Win32::Foundation::HMODULE::default();
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    // SAFETY: lookups by address; the name buffer is ours
    unsafe {
        if GetModuleHandleExW(flags, windows::core::PCWSTR(addr as *const u16), &mut m).is_err() {
            return format!("{addr:#x}");
        }
        let mut path = [0u16; 260];
        let n = GetModuleFileNameW(m, &mut path) as usize;
        let name = String::from_utf16_lossy(&path[..n]);
        format!("{}+{:#x}", name.rsplit(['\\', '/']).next().unwrap_or("?"), addr - m.0 as usize)
    }
}

// ---------------------------------------------------------------------------------------------- installing

/// Hooks the host's device and audio client classes. Done once, before the title runs, on a thread of its own
/// (its COM apartment is its own). `Err` when the host has no render device (nothing to watch then).
pub fn install() -> Result<(), String> {
    if INSTALLED.swap(true, Relaxed) {
        return Ok(());
    }
    // keep the classes' DLLs loaded for the life of the process: their tables are patched
    for dll in ["mmdevapi.dll", "audioses.dll"] {
        let w: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a System32 DLL, loaded and never freed
        let _ = unsafe {
            windows::Win32::System::LibraryLoader::LoadLibraryExW(
                windows::core::PCWSTR(w.as_ptr()),
                None,
                windows::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
    }
    std::thread::Builder::new()
        .name("rebuild audio hooks".into())
        .spawn(|| {
            // SAFETY: this thread's apartment, undone at the end; the objects are released before it
            unsafe {
                let com = CoInitializeEx(None, COINIT_MULTITHREADED);
                let r = hook();
                if com.is_ok() {
                    CoUninitialize();
                }
                r
            }
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "audio hook thread panicked".to_string())??;
    // the title's XAudio2 voices (what feeds the spatial streams)
    if let Err(e) = crate::xaudio::install() {
        eprintln!("audio     XAudio2 voices not watched: {e}");
    }
    // REBUILD_AUDIO_REPORT=<s>: the report every s seconds too (a run that ends in a crash still leaves one)
    if let Some(every) = std::env::var("REBUILD_AUDIO_REPORT").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v >= 1.0) {
        let _ = std::thread::Builder::new().name("rebuild audio report".into()).spawn(move || {
            let t = Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_secs_f64(every));
                let at = t.elapsed().as_secs_f64();
                println!("audio @{at:.0}s {}", crate::stats::report());
                for l in report() {
                    println!("audio @{at:.0}s {l}");
                }
            }
        });
    }
    Ok(())
}

/// The console's timer granularity for this process: 1 ms timer resolution (`timeBeginPeriod(1)`, kept for the life of
/// the process), honoured even while no window of the process is visible (Windows 11 otherwise ignores the request
/// then). Without it `Sleep(1)` takes ~15.6 ms here, and threads written for the console's 1 ms sleeps run late: Bink's
/// and XAudio2's feeders let their voices run dry between wake-ups (10-30 ms holes in the title's sound, 26 a second).
/// `REBUILD_TIMER_RESOLUTION=default` leaves the host's default. Returns `Sleep(1)`'s time afterwards (ms).
pub fn fine_timer() -> Option<f64> {
    if std::env::var("REBUILD_TIMER_RESOLUTION").is_ok_and(|v| v.trim() == "default") {
        return None;
    }
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    };
    // SAFETY: this process's own settings; the structure lives across the call
    unsafe {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        windows::Win32::Media::timeBeginPeriod(1);
    }
    Some(sleep_one_ms())
}

/// How long `Sleep(1)` takes in this process (ms, median of 5): the timer resolution this process's waits get (on
/// Windows 11 a process's own request counts, not another process's).
pub fn sleep_one_ms() -> f64 {
    let mut t: Vec<f64> = (0..5)
        .map(|_| {
            let s = Instant::now();
            // SAFETY: a plain sleep
            unsafe { windows::Win32::System::Threading::Sleep(1) };
            s.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    t[2]
}

/// The host's timer resolution now (ms): what `Sleep` and timed waits round up to.
pub fn timer_resolution_ms() -> Option<f64> {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type QueryFn = unsafe extern "system" fn(*mut u32, *mut u32, *mut u32) -> i32;
    // SAFETY: ntdll's export, three out values in 100 ns units
    unsafe {
        let nt = GetModuleHandleW(windows::core::w!("ntdll.dll")).ok()?;
        let f: QueryFn = std::mem::transmute(GetProcAddress(nt, windows::core::s!("NtQueryTimerResolution"))?);
        let (mut coarsest, mut finest, mut now) = (0u32, 0u32, 0u32);
        (f(&mut coarsest, &mut finest, &mut now) >= 0).then(|| f64::from(now) / 1e4)
    }
}

unsafe fn hook() -> Result<(), String> {
    let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    let device = e.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("no default render device: {e}"))?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient: {e}"))?;
    let _g = lock(&HOOKING);
    let dt = table_of(device.as_raw());
    REAL.activate.store(patch(dt, 3, activate as *const () as usize)?, Relaxed);
    let ct = table_of(client.as_raw());
    REAL.initialize.store(patch(ct, 3, initialize as *const () as usize)?, Relaxed);
    REAL.padding.store(patch(ct, 6, padding as *const () as usize)?, Relaxed);
    REAL.start.store(patch(ct, 10, start as *const () as usize)?, Relaxed);
    REAL.stop.store(patch(ct, 11, stop as *const () as usize)?, Relaxed);
    REAL.service.store(patch(ct, 14, service as *const () as usize)?, Relaxed);
    // slots 15-19 exist only when the object is an IAudioClient3 on the same table (checked below with slot 20)
    let three = client.cast::<IAudioClient3>().is_ok_and(|c3| table_of(c3.as_raw()) == ct);
    for (i, (&(slot, _), hook)) in COUNTED.iter().zip(counted_hooks()).enumerate() {
        if slot < 15 || three {
            COUNTED_REAL[i].store(patch(ct, slot, hook)?, Relaxed);
        }
    }
    // IAudioClient3 continues the same table when the object hands back the same pointer for it
    if let Ok(c3) = client.cast::<IAudioClient3>() {
        if table_of(c3.as_raw()) == ct {
            REAL.shared_stream.store(patch(ct, 20, shared_stream as *const () as usize)?, Relaxed);
        }
    }
    REAL.client_table.store(ct as usize, Relaxed);
    // the host's XAudio2 renders through spatial audio streams
    match device.Activate::<ISpatialAudioClient>(CLSCTX_ALL, None) {
        Ok(sc) => crate::spatial::hook_client(sc.as_raw())?,
        Err(e) => eprintln!("audio     no ISpatialAudioClient on the default device ({e}): spatial streams not watched"),
    }
    Ok(())
}

pub fn installed() -> bool {
    REAL.client_table.load(Relaxed) != 0
}

/// Which host audio DLLs the process has loaded (what the title's sound went through).
pub fn loaded_dlls() -> Vec<&'static str> {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    ["mmdevapi.dll", "audioses.dll", "xaudio2_9.dll", "xaudio2_9redist.dll", "xaudio2_8.dll", "x3daudio1_7.dll", "spatialaudiometadata.dll", "windows.media.audio.dll", "mfplat.dll", "msmpeg2adec.dll"]
        .into_iter()
        .filter(|d| {
            let w: Vec<u16> = d.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: a lookup by name
            unsafe { GetModuleHandleW(windows::core::PCWSTR(w.as_ptr())) }.is_ok()
        })
        .collect()
}

/// One stream's numbers, for tests and the run report.
#[derive(Clone, Debug)]
pub struct StreamReport {
    pub format: Option<Format>,
    pub hr: i32,
    pub share: u32,
    pub flags: u32,
    pub buffer_frames: u32,
    pub frames: u64,
    pub buffers: u64,
    pub silent: u64,
    pub get_failed: u64,
    pub dry: u64,
    /// seconds the stream was started
    pub playing: f64,
    /// frames short of real time over the playing time, less one buffer (> 0: the stream ran dry for that long)
    pub short: u64,
}

pub fn streams() -> Vec<StreamReport> {
    lock(&STREAMS)
        .iter()
        .filter(|s| s.format.is_some() || s.hr != 0)
        .map(|s| {
            let playing = s.playing_seconds();
            let want = s.format.map_or(0.0, |f| playing * f64::from(f.rate)) as u64;
            StreamReport {
                format: s.format,
                hr: s.hr,
                share: s.share,
                flags: s.flags,
                buffer_frames: s.buffer_frames,
                frames: s.frames,
                buffers: s.buffers,
                silent: s.silent,
                get_failed: s.get_failed,
                dry: s.dry,
                playing,
                short: want.saturating_sub(s.frames + u64::from(s.buffer_frames)),
            }
        })
        .collect()
}

/// The run report's lines.
pub fn report() -> Vec<String> {
    let mut out = Vec::new();
    if !installed() {
        out.push("host WASAPI not watched (no render device, or hooks not installed)".into());
    }
    let a = lock(&ACTIVATIONS).clone();
    out.push(format!(
        "device activations: {}",
        if a.is_empty() { "none".into() } else { a.iter().map(|(n, c, f, w)| format!("{n} {c}{} [first from {w}]", if *f > 0 { format!(" ({f} failed)") } else { String::new() })).collect::<Vec<_>>().join(", ") }
    ));
    let k = *lock(&COUNTED_CALLS);
    let called: Vec<String> = COUNTED
        .iter()
        .zip(k.iter())
        .filter(|(_, c)| c.0 > 0)
        .map(|(&(_, n), c)| format!("{n} {}{}", c.0, if c.1 > 0 { format!(" ({} failed, first {:#x})", c.1, c.2 as u32) } else { String::new() }))
        .collect();
    out.push(format!("other client calls: {}", if called.is_empty() { "none".into() } else { called.join(", ") }));
    for (f, hr) in lock(&FORMATS_ASKED).iter() {
        out.push(format!("  IsFormatSupported {f} -> {:#x}", *hr as u32));
    }
    let s = streams();
    out.push(format!("{} audio clients initialized", s.len()));
    for (i, r) in s.iter().enumerate() {
        let f = r.format.map_or("format not read".to_string(), |f| {
            format!("{} Hz {} ch {}-bit {}{}", f.rate, f.channels, f.bits, if f.float { "float" } else { "PCM" }, if f.mask != 0 { format!(" mask {:#x}", f.mask) } else { String::new() })
        });
        out.push(format!(
            "  stream {}: {f}, {} mode, flags {:#x}, Initialize {:#x}, buffer {} frames; {:.1} s playing, {} buffers, {} frames ({} silent buffers, {} GetBuffer failed), ran dry {}x, {} frames short of real time",
            i + 1,
            if r.share == 1 { "exclusive" } else { "shared" },
            r.flags,
            r.hr as u32,
            r.buffer_frames,
            r.playing,
            r.buffers,
            r.frames,
            r.silent,
            r.get_failed,
            r.dry,
            r.short
        ));
    }
    if OTHER_TABLES.load(Relaxed) > 0 {
        out.push(format!("{} render clients of another class (not watched)", OTHER_TABLES.load(Relaxed)));
    }
    out.extend(crate::spatial::report());
    out.extend(crate::xaudio::report());
    out.push(format!(
        "host audio DLLs loaded: {}; host timer resolution {}, Sleep(1) here takes {:.2} ms (median of 5)",
        loaded_dlls().join(", "),
        timer_resolution_ms().map_or("?".into(), |v| format!("{v:.3} ms")),
        sleep_one_ms()
    ));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_fine_timer_makes_a_one_ms_sleep_take_about_one_ms() {
        let ms = super::fine_timer().expect("not switched off in tests");
        assert!(ms < 4.0, "Sleep(1) took {ms:.2} ms after timeBeginPeriod(1)");
    }

    /// A client's calls before `Initialize` are counted through the hooked table (needs a render device: skipped
    /// without one).
    #[test]
    fn a_clients_mix_format_and_format_questions_are_counted() {
        use windows::Win32::Media::Audio::{eConsole, eRender, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED};
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
        if super::install().is_err() {
            return;
        }
        // SAFETY: COM on this thread; the mix format is freed as documented
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let c: IAudioClient = e.GetDefaultAudioEndpoint(eRender, eConsole).unwrap().Activate(CLSCTX_ALL, None).unwrap();
            let f = c.GetMixFormat().unwrap();
            let _ = c.IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, f, None);
            CoTaskMemFree(Some(f.cast()));
        }
        let k = *super::lock(&super::COUNTED_CALLS);
        assert!(k[0].0 >= 1 && k[1].0 >= 1, "IsFormatSupported and GetMixFormat counted: {k:?}");
        assert!(super::report().iter().any(|l| l.contains("IsFormatSupported shared")), "{:?}", super::report());
    }
}
