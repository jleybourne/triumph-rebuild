//! A4: output to the host through WASAPI, on a thread of its own.
//!
//! `Output::start` opens the default render device in shared mode, event driven, as 32-bit float at
//! the rate and channel count asked for (Windows converts to the device's mix format). Producers hand
//! samples to a lock-free ring (`Writer::push`), which never waits; the output thread drains it on each
//! device period and plays silence when it runs dry (counted as underruns). No audio work happens on
//! the caller's thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::ring::{Reader, Ring, Writer};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// Shared-mode buffer asked for: 20 ms, in 100 ns units.
const BUFFER_100NS: i64 = 200_000;

#[derive(Default)]
pub struct Stats {
    /// Frames handed to the device.
    pub frames: AtomicU64,
    /// Frames of those that were silence because the ring ran dry.
    pub underrun_frames: AtomicU64,
    /// Device periods serviced.
    pub periods: AtomicU64,
}

pub struct Output {
    pub rate: u32,
    pub channels: u16,
    /// The device buffer, in frames.
    pub buffer_frames: u32,
    pub stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    /// Starts the output thread; returns once the device is playing, with the ring's writer. The ring
    /// holds `ring_ms` of audio.
    pub fn start(rate: u32, channels: u16, ring_ms: u32) -> Result<(Output, Writer), String> {
        let (writer, reader) = Ring::new((rate as usize * channels as usize * ring_ms as usize) / 1000);
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let (s, st) = (stop.clone(), stats.clone());
        let thread = std::thread::Builder::new()
            .name("rebuild audio out".into())
            .spawn(move || {
                crate::raise_priority();
                // SAFETY: this thread's COM apartment, undone below.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if let Err(e) = unsafe { render(rate, channels, reader, &s, &st, &tx) } {
                    let _ = tx.send(Err(e.to_string()));
                }
                if com.is_ok() {
                    // SAFETY: paired with the call above.
                    unsafe { CoUninitialize() };
                }
            })
            .map_err(|e| e.to_string())?;
        let buffer_frames = rx.recv().map_err(|e| e.to_string())??;
        Ok((
            Output {
                rate,
                channels,
                buffer_frames,
                stats,
                stop,
                thread: Some(thread),
            },
            writer,
        ))
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The output thread: open, then fill the device each period until stopped.
unsafe fn render(
    rate: u32,
    channels: u16,
    mut ring: Reader,
    stop: &AtomicBool,
    stats: &Stats,
    ready: &std::sync::mpsc::Sender<Result<u32, String>>,
) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let block = channels * 4;
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        BUFFER_100NS,
        0,
        &format,
        None,
    )?;
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let frames = client.GetBufferSize()?;
    let out: IAudioRenderClient = client.GetService()?;
    // Prime with silence so the first period does not start dry.
    let p = out.GetBuffer(frames)?;
    std::ptr::write_bytes(p, 0, frames as usize * block as usize);
    out.ReleaseBuffer(frames, 0)?;
    client.Start()?;
    let _ = ready.send(Ok(frames));
    let ch = channels as usize;
    while !stop.load(Ordering::Acquire) {
        if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
            continue;
        }
        let room = frames - client.GetCurrentPadding()?;
        if room == 0 {
            continue;
        }
        let p = out.GetBuffer(room)?;
        let buf = std::slice::from_raw_parts_mut(p.cast::<f32>(), room as usize * ch);
        let got = ring.pop(buf);
        buf[got..].fill(0.0);
        out.ReleaseBuffer(room, 0)?;
        stats.frames.fetch_add(u64::from(room), Ordering::Relaxed);
        stats.underrun_frames.fetch_add(((buf.len() - got) / ch) as u64, Ordering::Relaxed);
        stats.periods.fetch_add(1, Ordering::Relaxed);
    }
    client.Stop()?;
    let _ = CloseHandle(event);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A4: a short, quiet 440 Hz tone (0.5 s, amplitude 0.05) through the default device, fed in 10 ms
    /// pieces; every `push` timed. Hand-run only, and only while `agents\MEASURING` is absent:
    /// `cargo test --release -p rebuild-audio -- --ignored a4_test_tone --nocapture`.
    #[test]
    #[ignore]
    fn a4_test_tone_plays() {
        let lock = std::path::Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING");
        if lock.exists() {
            eprintln!("measurement lock present: not playing");
            return;
        }
        let (rate, ch) = (48_000u32, 2u16);
        let t0 = Instant::now();
        let (out, mut w) = Output::start(rate, ch, 100).expect("default render device");
        let open = t0.elapsed();
        let total = rate as usize / 2;
        let tone: Vec<f32> = (0..total)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.05;
                [v, v]
            })
            .collect();
        let before = out.stats.frames.load(Ordering::Relaxed);
        let under_before = out.stats.underrun_frames.load(Ordering::Relaxed);
        let mut at = 0;
        let mut worst = Duration::ZERO;
        let mut pushes = 0u32;
        while at < tone.len() {
            let piece = &tone[at..(at + 960).min(tone.len())];
            let t = Instant::now();
            at += w.push(piece);
            worst = worst.max(t.elapsed());
            pushes += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        // Let the ring drain, then a period more.
        let t = Instant::now();
        while w.held() > 0 && t.elapsed().as_secs() < 3 {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        let played = out.stats.frames.load(Ordering::Relaxed) - before;
        let periods = out.stats.periods.load(Ordering::Relaxed);
        let under = out.stats.underrun_frames.load(Ordering::Relaxed) - under_before;
        let buffer = out.buffer_frames;
        drop(out);
        eprintln!(
            "A4: device buffer {} frames; opened in {:.1} ms; {total} tone frames pushed in {pushes} pushes, \
             worst push {:.1} us; {played} frames played over {periods} periods; {under} silent frames (incl. lead-in and tail)",
            buffer, open.as_secs_f64() * 1e3, worst.as_secs_f64() * 1e6
        );
        assert!(played >= total as u64, "the whole tone reached the device");
        assert!(worst < Duration::from_millis(1), "push never blocks 1 ms");
    }
}
