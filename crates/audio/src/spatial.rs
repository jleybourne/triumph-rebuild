//! The title's spatial-audio output (Windows Sonic's API, `ISpatialAudioClient`), watched like `hostout`'s
//! WASAPI streams.
//!
//! The host's XAudio2 2.9 (which the title and Bink use) renders its mastering voice through
//! `ISpatialAudioClient::ActivateSpatialAudioStream`: one static audio object per channel, each a mono float
//! buffer filled between `BeginUpdatingAudioObjects` and `EndUpdatingAudioObjects` once per engine period. The
//! hooks here count those streams and periods, tap the static objects into one WAV per stream (channels in object
//! type order: FL FR FC LFE SL SR BL BR, then the height channels), and apply `REBUILD_AUDIO_VOLUME` as a gain on
//! the objects' buffers (spatial streams take no session volume of their own).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{GUID, HRESULT};

use crate::hostout::{gain, patch, send_sink, sink_wanted, table_of, Msg, CALLS};
use crate::lock;
use crate::wav::Format;

struct Real {
    activate_stream: AtomicUsize,
    start: AtomicUsize,
    stop: AtomicUsize,
    begin: AtomicUsize,
    end: AtomicUsize,
    activate_object: AtomicUsize,
    get_buffer: AtomicUsize,
    stream_table: AtomicUsize,
    object_table: AtomicUsize,
}

static REAL: Real = Real {
    activate_stream: AtomicUsize::new(0),
    start: AtomicUsize::new(0),
    stop: AtomicUsize::new(0),
    begin: AtomicUsize::new(0),
    end: AtomicUsize::new(0),
    activate_object: AtomicUsize::new(0),
    get_buffer: AtomicUsize::new(0),
    stream_table: AtomicUsize::new(0),
    object_table: AtomicUsize::new(0),
};

/// Streams or objects of a class other than the hooked one (not watched).
pub static OTHER_TABLES: AtomicU64 = AtomicU64::new(0);

struct Stream {
    ptr: usize,
    hr: i32,
    rate: u32,
    mask: u32,
    max_dynamic: u32,
    started: Option<Instant>,
    played: f64,
    periods: u64,
    frames: u64,
    /// frames per buffer of the last period
    period_frames: u32,
    objects: u64,
    dynamic_objects: u64,
    /// buffers filled this period: (object type, buffer)
    pending: Vec<(u32, usize)>,
    sink: Option<u32>,
    begin_failed: u64,
    /// periods with no object buffer filled (silence out)
    empty: u64,
    /// the last period's end, the longest time between two, and how many came later than two periods' time
    last_end: Option<Instant>,
    max_gap: f64,
    late: u64,
    /// the thread that runs the periods: id, priority, affinity mask
    engine: Option<(u32, i32, u64)>,
}

struct State {
    streams: Vec<Stream>,
    /// object → (stream, type)
    objects: Vec<(usize, usize, u32)>,
    sinks: u32,
}

static STATE: Mutex<State> = Mutex::new(State { streams: Vec::new(), objects: Vec::new(), sinks: 0 });

type ActivateStreamFn = unsafe extern "system" fn(*mut c_void, *const c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type ThisFn = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type BeginFn = unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32) -> HRESULT;
type ActivateObjectFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
type GetBufferFn = unsafe extern "system" fn(*mut c_void, *mut *mut u8, *mut u32) -> HRESULT;

unsafe fn real<F: Copy>(slot: &AtomicUsize) -> F {
    let p = slot.load(Relaxed);
    std::mem::transmute_copy(&p)
}

const DYNAMIC: u32 = 1;

/// Hooks `ISpatialAudioClient::ActivateSpatialAudioStream` (slot 10) on the class of `client`.
///
/// # Safety
/// `client` is a live `ISpatialAudioClient`.
pub unsafe fn hook_client(client: *mut c_void) -> Result<(), String> {
    let t = table_of(client);
    REAL.activate_stream.store(patch(t, 10, activate_stream as *const () as usize)?, Relaxed);
    Ok(())
}

pub fn installed() -> bool {
    REAL.activate_stream.load(Relaxed) != 0
}

/// `ActivateSpatialAudioStream(this, const PROPVARIANT* params, REFIID, void** stream)`; the params are a blob of
/// `SpatialAudioObjectRenderStreamActivationParams` (format*, static mask, min and max dynamic objects, category,
/// event, notify).
unsafe extern "system" fn activate_stream(this: *mut c_void, params: *const c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ActivateStreamFn>(&REAL.activate_stream)(this, params, iid, out);
    let (mut rate, mut mask, mut max_dynamic) = (0, 0, 0);
    if !params.is_null() && params.cast::<u16>().read() == 65 {
        let blob = params.cast::<u8>().add(16).cast::<*const u8>().read();
        if !blob.is_null() {
            if let Some(f) = Format::from_waveformatex(blob.cast::<*const u8>().read()) {
                rate = f.rate;
            }
            mask = blob.add(8).cast::<u32>().read();
            max_dynamic = blob.add(16).cast::<u32>().read();
        }
    }
    let stream = if hr.is_ok() && !out.is_null() { *out } else { std::ptr::null_mut() };
    if !stream.is_null() {
        let table = table_of(stream) as usize;
        if REAL.stream_table.load(Relaxed) == 0 {
            let t = table as *mut usize;
            // ISpatialAudioObjectRenderStreamBase: Start 5, Stop 6, Begin 8, End 9; ISpatialAudioObjectRenderStream:
            // ActivateSpatialAudioObject 10
            let r = (|| -> Result<(), String> {
                REAL.start.store(patch(t, 5, start as *const () as usize)?, Relaxed);
                REAL.stop.store(patch(t, 6, stop as *const () as usize)?, Relaxed);
                REAL.begin.store(patch(t, 8, begin as *const () as usize)?, Relaxed);
                REAL.end.store(patch(t, 9, end as *const () as usize)?, Relaxed);
                REAL.activate_object.store(patch(t, 10, activate_object as *const () as usize)?, Relaxed);
                Ok(())
            })();
            if r.is_ok() {
                REAL.stream_table.store(table, Relaxed);
            }
        } else if REAL.stream_table.load(Relaxed) != table {
            OTHER_TABLES.fetch_add(1, Relaxed);
        }
    }
    let mut s = lock(&STATE);
    let sink = (sink_wanted() && !stream.is_null() && rate > 0).then(|| {
        s.sinks += 1;
        s.sinks
    });
    s.streams.push(Stream {
        ptr: stream as usize,
        hr: hr.0,
        rate,
        mask,
        max_dynamic,
        started: None,
        played: 0.0,
        periods: 0,
        frames: 0,
        period_frames: 0,
        objects: 0,
        dynamic_objects: 0,
        pending: Vec::new(),
        sink,
        begin_failed: 0,
        empty: 0,
        last_end: None,
        max_gap: 0.0,
        late: 0,
        engine: None,
    });
    drop(s);
    if let Some(n) = sink {
        send_sink(Msg::Open(1000 + n, Format { rate, channels: mask.count_ones() as u16 - (mask & DYNAMIC) as u16, bits: 32, float: true, mask: 0 }));
    }
    hr
}

fn with_stream<R>(ptr: usize, f: impl FnOnce(&mut Stream) -> R) -> Option<R> {
    lock(&STATE).streams.iter_mut().rev().find(|s| s.ptr == ptr).map(f)
}

unsafe extern "system" fn start(this: *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ThisFn>(&REAL.start)(this);
    if hr.is_ok() {
        with_stream(this as usize, |s| {
            s.started.get_or_insert_with(Instant::now);
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

unsafe extern "system" fn begin(this: *mut c_void, available: *mut u32, frames: *mut u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<BeginFn>(&REAL.begin)(this, available, frames);
    let n = if hr.is_ok() && !frames.is_null() { *frames } else { 0 };
    with_stream(this as usize, |s| {
        s.pending.clear();
        s.period_frames = n;
        if s.engine.is_none() {
            s.engine = Some(crate::xaudio::this_thread());
        }
        s.begin_failed += u64::from(hr.is_err());
    });
    hr
}

unsafe extern "system" fn end(this: *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let g = gain();
    let work = with_stream(this as usize, |s| {
        s.periods += 1;
        s.frames += u64::from(s.period_frames);
        s.empty += u64::from(s.pending.is_empty());
        let now = Instant::now();
        if let (Some(t), true) = (s.last_end, s.rate > 0) {
            let gap = now.duration_since(t).as_secs_f64();
            s.max_gap = s.max_gap.max(gap);
            if gap > 2.0 * f64::from(s.period_frames) / f64::from(s.rate) {
                s.late += 1;
            }
        }
        s.last_end = Some(now);
        (std::mem::take(&mut s.pending), s.period_frames, s.mask & !DYNAMIC, s.sink)
    });
    if let Some((pending, frames, mask, sink)) = work {
        let n = frames as usize;
        if let Some(k) = sink {
            // interleave the static objects in type order; an object not filled this period is silence
            let types: Vec<u32> = (0..32).map(|b| 1u32 << b).filter(|t| mask & t != 0).collect();
            let ch = types.len();
            let mut out = vec![0f32; n * ch];
            for (c, t) in types.iter().enumerate() {
                if let Some(&(_, p)) = pending.iter().find(|(ty, _)| ty == t) {
                    let b = std::slice::from_raw_parts(p as *const f32, n);
                    for i in 0..n {
                        out[i * ch + c] = b[i];
                    }
                }
            }
            send_sink(Msg::Data(1000 + k, out.iter().flat_map(|v| v.to_le_bytes()).collect()));
        }
        if let Some(g) = g {
            for &(_, p) in &pending {
                for v in std::slice::from_raw_parts_mut(p as *mut f32, n) {
                    *v *= g;
                }
            }
        }
    }
    real::<ThisFn>(&REAL.end)(this)
}

unsafe extern "system" fn activate_object(this: *mut c_void, kind: u32, out: *mut *mut c_void) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<ActivateObjectFn>(&REAL.activate_object)(this, kind, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() {
        let obj = *out;
        let table = table_of(obj) as usize;
        if REAL.object_table.load(Relaxed) == 0 {
            // ISpatialAudioObjectBase::GetBuffer, slot 3
            if let Ok(r) = patch(table as *mut usize, 3, get_buffer as *const () as usize) {
                REAL.get_buffer.store(r, Relaxed);
                REAL.object_table.store(table, Relaxed);
            }
        } else if REAL.object_table.load(Relaxed) != table {
            OTHER_TABLES.fetch_add(1, Relaxed);
        }
        let mut s = lock(&STATE);
        s.objects.retain(|o| o.0 != obj as usize);
        s.objects.push((obj as usize, this as usize, kind));
        if let Some(st) = s.streams.iter_mut().rev().find(|x| x.ptr == this as usize) {
            st.objects += 1;
            st.dynamic_objects += u64::from(kind == DYNAMIC);
        }
    }
    hr
}

unsafe extern "system" fn get_buffer(this: *mut c_void, buffer: *mut *mut u8, length: *mut u32) -> HRESULT {
    CALLS.fetch_add(1, Relaxed);
    let hr = real::<GetBufferFn>(&REAL.get_buffer)(this, buffer, length);
    if hr.is_ok() && !buffer.is_null() && !(*buffer).is_null() {
        let p = *buffer as usize;
        let mut s = lock(&STATE);
        if let Some(&(_, stream, kind)) = s.objects.iter().rev().find(|o| o.0 == this as usize) {
            if let Some(st) = s.streams.iter_mut().rev().find(|x| x.ptr == stream) {
                st.pending.push((kind, p));
            }
        }
    }
    hr
}

/// The run report's lines.
pub fn report() -> Vec<String> {
    let s = lock(&STATE);
    let mut out = vec![format!("{} spatial audio streams activated", s.streams.len())];
    for (i, x) in s.streams.iter().enumerate() {
        let playing = x.played + x.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
        let want = (playing * f64::from(x.rate)) as u64;
        out.push(format!(
            "  spatial {}: ActivateSpatialAudioStream {:#x}, {} Hz, static mask {:#x} ({} channels), up to {} dynamic; {:.1} s playing, {} periods of {} frames, {} frames ({} short of real time), {} objects ({} dynamic), {} Begin failed; {} periods with no buffer filled, {} late (over two periods after the last), longest gap {:.1} ms; periods run by thread {}",
            i + 1,
            x.hr as u32,
            x.rate,
            x.mask,
            (x.mask & !DYNAMIC).count_ones(),
            x.max_dynamic,
            playing,
            x.periods,
            x.period_frames,
            x.frames,
            want.saturating_sub(x.frames + u64::from(x.period_frames) * 2),
            x.objects,
            x.dynamic_objects,
            x.begin_failed,
            x.empty,
            x.late,
            x.max_gap * 1e3,
            x.engine.map_or("?".into(), |(id, pr, mask)| format!("{id} (priority {pr}, affinity {mask:#x})"))
        ));
    }
    if OTHER_TABLES.load(Relaxed) > 0 {
        out.push(format!("{} spatial streams or objects of another class (not watched)", OTHER_TABLES.load(Relaxed)));
    }
    out
}
