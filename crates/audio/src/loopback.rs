//! Process loopback: what the host's audio engine plays for one process tree, recorded into a WAV.
//!
//! Windows' process loopback (`ActivateAudioInterfaceAsync` on `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` with
//! `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10 2004 and later) captures only the target's streams,
//! never the rest of the host's mix. The recorder belongs in a process of its own (`audiocap record`): an audio
//! client in the title's process would change that process's timer resolution, and the title's waits with it.
//!
//! The engine hands over packets only while the target plays; stretches with nothing are written as silence from
//! the packets' timestamps, so the file keeps real time.

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceCompletionHandler, IAudioCaptureClient, IAudioClient,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::System::Threading::{CreateEventW, OpenProcess, SetEvent, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use crate::wav::{Format, Writer, WAVE_FORMAT_IEEE_FLOAT};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// What a recording saw.
#[derive(Debug, Default, Clone)]
pub struct Recorded {
    pub frames: u64,
    pub packets: u64,
    /// packets the engine flagged silent
    pub silent_packets: u64,
    /// packets flagged as following a gap in the capture (the recorder fell behind)
    pub discontinuities: u64,
    /// frames written as silence because no packet covered them
    pub filled_frames: u64,
    pub seconds: f64,
}

// --------------------------------------------------------------- the activation's completion handler, by hand

#[repr(C)]
struct Handler {
    vtbl: *const HandlerVtbl,
    event: HANDLE,
}

#[repr(C)]
struct HandlerVtbl {
    query: unsafe extern "system" fn(*mut Handler, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Handler) -> u32,
    release: unsafe extern "system" fn(*mut Handler) -> u32,
    completed: unsafe extern "system" fn(*mut Handler, *mut c_void) -> HRESULT,
}

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
/// `IAgileObject`: the activation calls the handler on a thread of its own choosing.
const IID_IAGILE: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);

unsafe extern "system" fn query(this: *mut Handler, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    let iid = *iid;
    if iid == IID_IUNKNOWN || iid == IID_IAGILE || iid == IActivateAudioInterfaceCompletionHandler::IID {
        *out = this.cast();
        HRESULT(0)
    } else {
        *out = std::ptr::null_mut();
        windows::Win32::Foundation::E_NOINTERFACE
    }
}

/// The handler lives as long as the recording (it is leaked: one per recording), so counting is not needed.
unsafe extern "system" fn add_ref(_: *mut Handler) -> u32 {
    1
}
unsafe extern "system" fn release(_: *mut Handler) -> u32 {
    1
}
unsafe extern "system" fn completed(this: *mut Handler, _op: *mut c_void) -> HRESULT {
    let _ = SetEvent((*this).event);
    HRESULT(0)
}

static HANDLER_VTBL: HandlerVtbl = HandlerVtbl { query, add_ref, release, completed };

/// `AUDIOCLIENT_ACTIVATION_PARAMS` with process-loopback parameters.
#[repr(C)]
struct ActivationParams {
    kind: u32,
    pid: u32,
    mode: u32,
}

/// A `PROPVARIANT` holding a `VT_BLOB` (the layout the activation reads: vt, three reserved words, then the blob).
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const c_void,
}

const VT_BLOB: u16 = 65;
const ACTIVATION_TYPE_PROCESS_LOOPBACK: u32 = 1;
const INCLUDE_TARGET_PROCESS_TREE: u32 = 0;

/// Opens a loopback capture client on `pid`'s process tree, initialized for 48 kHz stereo float.
unsafe fn open(pid: u32) -> Result<(IAudioClient, IAudioCaptureClient, HANDLE), String> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
    let handler = Box::leak(Box::new(Handler { vtbl: &HANDLER_VTBL, event }));
    let params = ActivationParams { kind: ACTIVATION_TYPE_PROCESS_LOOPBACK, pid, mode: INCLUDE_TARGET_PROCESS_TREE };
    let blob = BlobVariant { vt: VT_BLOB, reserved: [0; 3], size: std::mem::size_of::<ActivationParams>() as u32, data: (&params as *const ActivationParams).cast() };
    let h = std::mem::ManuallyDrop::new(IActivateAudioInterfaceCompletionHandler::from_raw((handler as *mut Handler).cast()));
    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some((&blob as *const BlobVariant).cast()),
        &*h,
    )
    .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;
    if WaitForSingleObject(event, 5_000) != WAIT_OBJECT_0 {
        return Err("process loopback activation did not complete in 5 s".into());
    }
    let mut hr = HRESULT(0);
    let mut unk = None;
    op.GetActivateResult(&mut hr, &mut unk).map_err(|e| format!("GetActivateResult: {e}"))?;
    hr.ok().map_err(|e| format!("activation: {e}"))?;
    let client: IAudioClient = unk.ok_or("no audio client")?.cast().map_err(|e| e.to_string())?;
    let block = CHANNELS * 4;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            200_000,
            0,
            &format,
            None,
        )
        .map_err(|e| format!("Initialize: {e}"))?;
    let cap_event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
    client.SetEventHandle(cap_event).map_err(|e| e.to_string())?;
    let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.to_string())?;
    let _ = CloseHandle(event);
    Ok((client, capture, cap_event))
}

fn qpc_100ns() -> u64 {
    let mut c = 0i64;
    let mut f = 0i64;
    // SAFETY: out-parameters on the stack
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut f);
    }
    (c as u128 * 10_000_000 / f.max(1) as u128) as u64
}

/// Set from another thread (or a console handler) to end a recording early.
pub static STOP: AtomicU32 = AtomicU32::new(0);

/// Records `pid`'s process tree into `out` until the process ends, `seconds` pass, or [`STOP`] is set.
/// `ready` is called once capture has started (a test starts its sound then).
pub fn record(pid: u32, out: &Path, seconds: Option<f64>, ready: impl FnOnce()) -> Result<Recorded, String> {
    // SAFETY: COM calls on this thread, with objects that live in this function
    unsafe {
        let (client, capture, event) = open(pid)?;
        let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok();
        let mut w = Writer::create(out, Format { rate: RATE, channels: CHANNELS, bits: 32, float: true, mask: 3 }).map_err(|e| e.to_string())?;
        client.Start().map_err(|e| e.to_string())?;
        let t0 = qpc_100ns();
        let started = Instant::now();
        ready();
        let mut r = Recorded::default();
        let block = w.format.block();
        let frame_at = |qpc: u64| (qpc.saturating_sub(t0) as u128 * RATE as u128 / 10_000_000) as u64;
        loop {
            if STOP.load(Ordering::Relaxed) != 0 || seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s) {
                break;
            }
            if let Some(p) = process {
                if WaitForSingleObject(p, 0) == WAIT_OBJECT_0 {
                    break;
                }
            }
            let _ = WaitForSingleObject(event, 100);
            loop {
                let n = capture.GetNextPacketSize().map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)).map_err(|e| e.to_string())?;
                // the gap before this packet, if any (more than 10 ms), as silence
                let at = frame_at(qpc);
                if at > w.frames() + u64::from(RATE / 100) {
                    let gap = at - w.frames();
                    w.silence(gap).map_err(|e| e.to_string())?;
                    r.filled_frames += gap;
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    w.silence(u64::from(frames)).map_err(|e| e.to_string())?;
                    r.silent_packets += 1;
                } else {
                    w.write(std::slice::from_raw_parts(data, frames as usize * block)).map_err(|e| e.to_string())?;
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    r.discontinuities += 1;
                }
                r.packets += 1;
                capture.ReleaseBuffer(frames).map_err(|e| e.to_string())?;
            }
        }
        let _ = client.Stop();
        // the tail with nothing played, as silence, so the file is as long as the recording
        let end = frame_at(qpc_100ns());
        if end > w.frames() {
            let gap = end - w.frames();
            w.silence(gap).map_err(|e| e.to_string())?;
            r.filled_frames += gap;
        }
        r.frames = w.frames();
        r.seconds = started.elapsed().as_secs_f64();
        w.finish().map_err(|e| e.to_string())?;
        if let Some(p) = process {
            let _ = CloseHandle(p);
        }
        let _ = CloseHandle(event);
        Ok(r)
    }
}

/// Waits up to `timeout` for a process named `exe` (case-insensitive, e.g. `runtitle.exe`) and returns its id.
pub fn wait_for_process(exe: &str, timeout: Duration) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    let t = Instant::now();
    while t.elapsed() < timeout {
        // SAFETY: a snapshot walked and closed here
        unsafe {
            if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
                let mut ok = Process32FirstW(snap, &mut e).is_ok();
                while ok {
                    let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(exe) {
                        let _ = CloseHandle(snap);
                        return Some(e.th32ProcessID);
                    }
                    ok = Process32NextW(snap, &mut e).is_ok();
                }
                let _ = CloseHandle(snap);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}
