//! The title's XAudio2 voices (the host's `xaudio2_9.dll`, which the title and Bink load by name), watched: every
//! source voice's format, the buffers submitted to it, and its frequency ratio.
//!
//! What a voice is fed against how long it has played tells a starving feeder apart from an engine that runs
//! late (that one shows in `crate::spatial`'s period timing). [`install`] creates an engine of its own once, to find
//! the engine class's method table, hooks `IXAudio2::CreateSourceVoice` (slot 5), and releases it; the first
//! source voice the title makes then has its class's table hooked: `Start` (19), `SubmitSourceBuffer` (21),
//! `SetFrequencyRatio` (26), `SetSourceSampleRate` (28). XAudio2's voice interfaces are not COM: no `IUnknown`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use windows::core::HRESULT;

use crate::hostout::{patch, table_of, CALLS};
use crate::lock;

struct Real {
    create_source: AtomicUsize,
    start: AtomicUsize,
    submit: AtomicUsize,
    ratio: AtomicUsize,
    rate: AtomicUsize,
    voice_table: AtomicUsize,
}

static REAL: Real = Real {
    create_source: AtomicUsize::new(0),
    start: AtomicUsize::new(0),
    submit: AtomicUsize::new(0),
    ratio: AtomicUsize::new(0),
    rate: AtomicUsize::new(0),
    voice_table: AtomicUsize::new(0),
};

#[derive(Clone)]
struct Voice {
    ptr: usize,
    tag: u16,
    rate: u32,
    channels: u16,
    bits: u16,
    block: u16,
    flags: u32,
    max_ratio: f32,
    hr: i32,
    started: Option<Instant>,
    buffers: u64,
    bytes: u64,
    /// frames submitted (PCM and float only; 0 for compressed formats)
    frames: u64,
    ratio: f32,
    ratio_calls: u64,
    ratio_max: f32,
    rate_changes: u64,
    /// the thread that fed it first: id, priority, affinity mask
    feeder: Option<(u32, i32, u64)>,
    /// submits that found the voice's queue empty (after its first): it had run dry, XAudio2 played silence
    starved: u64,
    /// time between submits: the last, the longest, how many over 20 ms
    last_submit: Option<Instant>,
    max_gap: f64,
    gaps_over_20ms: u64,
    /// frames per buffer: fewest and most (PCM and float)
    min_frames: u32,
    max_frames: u32,
}

/// The calling thread's id, priority and group affinity mask.
pub(crate) fn this_thread() -> (u32, i32, u64) {
    use windows::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, GetThreadGroupAffinity, GetThreadPriority};
    // SAFETY: queries about the calling thread
    unsafe {
        let t = GetCurrentThread();
        let mut a = windows::Win32::System::SystemInformation::GROUP_AFFINITY::default();
        let _ = GetThreadGroupAffinity(t, &mut a);
        (GetCurrentThreadId(), GetThreadPriority(t), a.Mask as u64)
    }
}

static VOICES: Mutex<Vec<Voice>> = Mutex::new(Vec::new());

type CreateSourceFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *const u8, u32, f32, *mut c_void, *const c_void, *const c_void) -> HRESULT;
type StartFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
type SubmitFn = unsafe extern "system" fn(*mut c_void, *const Buffer, *const c_void) -> HRESULT;
type RatioFn = unsafe extern "system" fn(*mut c_void, f32, u32) -> HRESULT;
type RateFn = unsafe extern "system" fn(*mut c_void, u32) -> HRESULT;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type XAudio2CreateFn = unsafe extern "system" fn(*mut *mut c_void, u32, u32) -> HRESULT;

/// `XAUDIO2_BUFFER`
#[repr(C)]
struct Buffer {
    flags: u32,
    audio_bytes: u32,
    data: *const u8,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    context: *mut c_void,
}

unsafe fn real<F: Copy>(slot: &AtomicUsize) -> F {
    let p = slot.load(Relaxed);
    std::mem::transmute_copy(&p)
}

/// Hooks the host XAudio2 engine class. `Err` when the host has no `xaudio2_9.dll`.
pub fn install() -> Result<(), String> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    let w: Vec<u16> = "xaudio2_9.dll".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a System32 DLL, loaded for good (its tables are patched); its documented entry point
    unsafe {
        let m = LoadLibraryExW(windows::core::PCWSTR(w.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32).map_err(|e| format!("xaudio2_9.dll: {e}"))?;
        let f = GetProcAddress(m, windows::core::s!("XAudio2Create")).ok_or("XAudio2Create not exported")?;
        let create: XAudio2CreateFn = std::mem::transmute(f);
        let mut engine: *mut c_void = std::ptr::null_mut();
        // XAUDIO2_DEFAULT_PROCESSOR
        create(&mut engine, 0, 0xffff_ffff).ok().map_err(|e| format!("XAudio2Create: {e}"))?;
        let t = table_of(engine);
        let r = patch(t, 5, create_source as *const () as usize);
        let release: ReleaseFn = std::mem::transmute(*t.add(2));
        release(engine);
        REAL.create_source.store(r?, Relaxed);
    }
    Ok(())
}

pub fn installed() -> bool {
    REAL.create_source.load(Relaxed) != 0
}

unsafe extern "system" fn create_source(
    this: *mut c_void,
    out: *mut *mut c_void,
    format: *const u8,
    flags: u32,
    max_ratio: f32,
    callback: *mut c_void,
    sends: *const c_void,
    effects: *const c_void,
) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<CreateSourceFn>(&REAL.create_source)(this, out, format, flags, max_ratio, callback, sends, effects);
    let voice = if hr.is_ok() && !out.is_null() { *out } else { std::ptr::null_mut() };
    if !voice.is_null() {
        let table = table_of(voice) as usize;
        if REAL.voice_table.load(Relaxed) == 0 {
            let t = table as *mut usize;
            let ok = (|| -> Result<(), String> {
                REAL.start.store(patch(t, 19, start as *const () as usize)?, Relaxed);
                REAL.submit.store(patch(t, 21, submit as *const () as usize)?, Relaxed);
                REAL.ratio.store(patch(t, 26, set_ratio as *const () as usize)?, Relaxed);
                REAL.rate.store(patch(t, 28, set_rate as *const () as usize)?, Relaxed);
                Ok(())
            })();
            if ok.is_ok() {
                REAL.voice_table.store(table, Relaxed);
            }
        } else if REAL.voice_table.load(Relaxed) != table {
            crate::spatial::OTHER_TABLES.fetch_add(1, Relaxed);
        }
    }
    let (mut tag, mut rate, mut channels, mut bits, mut block) = (0, 0, 0, 0, 0);
    if !format.is_null() {
        let h = std::slice::from_raw_parts(format, 16);
        tag = u16::from_le_bytes([h[0], h[1]]);
        channels = u16::from_le_bytes([h[2], h[3]]);
        rate = u32::from_le_bytes(h[4..8].try_into().unwrap());
        block = u16::from_le_bytes([h[12], h[13]]);
        bits = u16::from_le_bytes([h[14], h[15]]);
    }
    lock(&VOICES).push(Voice {
        ptr: voice as usize,
        tag,
        rate,
        channels,
        bits,
        block,
        flags,
        max_ratio,
        hr: hr.0,
        started: None,
        buffers: 0,
        bytes: 0,
        frames: 0,
        ratio: 1.0,
        ratio_calls: 0,
        ratio_max: 1.0,
        rate_changes: 0,
        feeder: None,
        starved: 0,
        last_submit: None,
        max_gap: 0.0,
        gaps_over_20ms: 0,
        min_frames: u32::MAX,
        max_frames: 0,
    });
    hr
}

fn with_voice(ptr: usize, f: impl FnOnce(&mut Voice)) {
    if let Some(v) = lock(&VOICES).iter_mut().rev().find(|v| v.ptr == ptr) {
        f(v);
    }
}

unsafe extern "system" fn start(this: *mut c_void, flags: u32, set: u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    with_voice(this as usize, |v| {
        v.started.get_or_insert_with(Instant::now);
    });
    real::<StartFn>(&REAL.start)(this, flags, set)
}

/// `IXAudio2SourceVoice::GetState` (slot 25)'s output.
#[repr(C)]
#[derive(Default)]
struct VoiceState {
    context: usize,
    queued: u32,
    played: u64,
}

unsafe extern "system" fn submit(this: *mut c_void, buffer: *const Buffer, wma: *const c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    if !buffer.is_null() {
        let b = &*buffer;
        // the queue before this buffer joins it (XAUDIO2_VOICE_NOSAMPLESPLAYED: no position query)
        let mut st = VoiceState::default();
        let get_state: unsafe extern "system" fn(*mut c_void, *mut VoiceState, u32) = std::mem::transmute(*table_of(this).add(25));
        get_state(this, &mut st, 0x100);
        let now = Instant::now();
        with_voice(this as usize, |v| {
            if v.buffers > 0 && st.queued == 0 && v.started.is_some() {
                v.starved += 1;
            }
            if let Some(t) = v.last_submit {
                let gap = now.duration_since(t).as_secs_f64();
                v.max_gap = v.max_gap.max(gap);
                v.gaps_over_20ms += u64::from(gap > 0.020);
            }
            v.last_submit = Some(now);
            if v.feeder.is_none() {
                v.feeder = Some(this_thread());
            }
            v.buffers += 1;
            v.bytes += u64::from(b.audio_bytes);
            // PCM (1), float (3), extensible (0xfffe): frames from bytes, or the play length when one is given
            if matches!(v.tag, 1 | 3 | 0xfffe) && v.block > 0 {
                let whole = u64::from(b.audio_bytes / u32::from(v.block));
                let n = if b.play_length > 0 { u64::from(b.play_length) } else { whole.saturating_sub(u64::from(b.play_begin)) };
                v.frames += n;
                v.min_frames = v.min_frames.min(n as u32);
                v.max_frames = v.max_frames.max(n as u32);
            }
        });
    }
    real::<SubmitFn>(&REAL.submit)(this, buffer, wma)
}

unsafe extern "system" fn set_ratio(this: *mut c_void, ratio: f32, set: u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    with_voice(this as usize, |v| {
        v.ratio = ratio;
        v.ratio_calls += 1;
        v.ratio_max = v.ratio_max.max(ratio);
    });
    real::<RatioFn>(&REAL.ratio)(this, ratio, set)
}

unsafe extern "system" fn set_rate(this: *mut c_void, rate: u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    with_voice(this as usize, |v| {
        v.rate = rate;
        v.rate_changes += 1;
    });
    real::<RateFn>(&REAL.rate)(this, rate)
}

/// The run report's lines: one per voice that was fed, the busiest first (at most 12).
pub fn report() -> Vec<String> {
    let mut v = lock(&VOICES).clone();
    let mut out = vec![format!("XAudio2: {} source voices created ({} fed)", v.len(), v.iter().filter(|x| x.buffers > 0).count())];
    v.retain(|x| x.buffers > 0);
    v.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    for x in v.iter().take(12) {
        let playing = x.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
        let fed = if x.frames > 0 && x.rate > 0 { format!("{:.1} s of audio", x.frames as f64 / f64::from(x.rate)) } else { "compressed".into() };
        out.push(format!(
            "  voice: tag {:#x} {} Hz {} ch {}-bit, flags {:#x}, max ratio {}, create {:#x}; {:.1} s since start, {} buffers ({}-{} frames), {} bytes = {fed}; ran dry {}x; submits apart up to {:.1} ms, {} over 20 ms; ratio {} (set {}x, max {}), rate set {}x; fed by thread {}",
            x.tag, x.rate, x.channels, x.bits, x.flags, x.max_ratio, x.hr as u32, playing, x.buffers, if x.min_frames == u32::MAX { 0 } else { x.min_frames }, x.max_frames, x.bytes, x.starved, x.max_gap * 1e3, x.gaps_over_20ms, x.ratio, x.ratio_calls, x.ratio_max, x.rate_changes,
            x.feeder.map_or("?".into(), |(id, pr, mask)| format!("{id} (priority {pr}, affinity {mask:#x})"))
        ));
    }
    out
}
