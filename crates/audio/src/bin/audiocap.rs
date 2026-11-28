//! Checks a process's sound without listening to it.
//!
//! audiocap record (--pid N | --wait runtitle.exe) --out x.wav [--seconds S]   process loopback of N's tree into a WAV
//! audiocap analyze x.wav [--gain g]                                         per-second RMS / peak / flatness, dropouts, clicks
//! audiocap selftest --out <dir> [--volume mute|0..1]                        a child plays a quiet tone with its session volume
//!                                                                           set (default mute) while this records it: shows
//!                                                                           whether loopback is taken before the session volume,
//!                                                                           and checks the hooks' file tap against it
//! audiocap tone [--seconds S]                                               (the selftest's child) hooks, then a quiet tone
//! audiocap spatialtone [--seconds S]                                        the same through a spatial audio stream (selftest --spatial)
//!
//! Recording happens in this process, never in the one recorded: an audio client changes a process's timer resolution.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rebuild_audio::{hostout, loopback, wasapi, wav};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("record") => record(&args),
        Some("analyze") => analyze(&args),
        Some("selftest") => selftest(&args),
        Some("tone") => tone(&args),
        Some("spatialtone") => spatial_tone(&args),
        _ => {
            eprintln!("audiocap record (--pid N | --wait <exe>) --out x.wav [--seconds S] | analyze x.wav | selftest --out <dir> [--volume mute|v] | tone [--seconds S]");
            2
        }
    };
    std::process::exit(code);
}

fn print_analysis(path: &Path) -> Option<wav::Report> {
    print_analysis_gain(path, 1.0)
}

/// `gain`: what the recording was played at (`REBUILD_AUDIO_VOLUME`); levels are shown as the title made them.
fn print_analysis_gain(path: &Path, gain: f32) -> Option<wav::Report> {
    match wav::read(path) {
        Ok(mut a) => {
            if gain > 0.0 && gain != 1.0 {
                a.samples.iter_mut().for_each(|v| *v /= gain);
                println!("gain      levels divided by {gain} (the run's REBUILD_AUDIO_VOLUME)");
            }
            let r = wav::analyze(&a);
            print!("{}", r.text());
            Some(r)
        }
        Err(e) => {
            println!("FAILED    {}: {e}", path.display());
            None
        }
    }
}

fn record(args: &[String]) -> i32 {
    let Some(out) = arg(args, "--out").map(PathBuf::from) else {
        eprintln!("record needs --out");
        return 2;
    };
    let seconds = arg(args, "--seconds").and_then(|s| s.parse::<f64>().ok());
    let pid = match (arg(args, "--pid"), arg(args, "--wait")) {
        (Some(p), _) => p.parse().ok(),
        (None, Some(exe)) => loopback::wait_for_process(&exe, Duration::from_secs(120)),
        _ => None,
    };
    let Some(pid) = pid else {
        println!("FAILED    no process to record");
        return 1;
    };
    println!("record    pid {pid} -> {}", out.display());
    match loopback::record(pid, &out, seconds, || {}) {
        Ok(r) => {
            println!(
                "recorded  {:.1} s, {} frames at {} Hz: {} packets ({} flagged silent, {} discontinuities), {} frames filled as silence",
                r.seconds, r.frames, loopback::RATE, r.packets, r.silent_packets, r.discontinuities, r.filled_frames
            );
            print_analysis(&out);
            0
        }
        Err(e) => {
            println!("FAILED    {e}");
            1
        }
    }
}

fn analyze(args: &[String]) -> i32 {
    let Some(p) = args.get(1) else {
        eprintln!("analyze needs a file");
        return 2;
    };
    let gain = arg(args, "--gain").and_then(|g| g.parse::<f32>().ok()).unwrap_or(1.0);
    if print_analysis_gain(Path::new(p), gain).is_some() { 0 } else { 1 }
}

/// The child: hooks in, a quiet chord through the default device, the hooks' report.
fn tone(args: &[String]) -> i32 {
    let seconds = arg(args, "--seconds").and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.5);
    reset_session();
    if let Err(e) = hostout::install() {
        println!("FAILED    hooks: {e}");
        return 1;
    }
    let (rate, ch) = (48_000u32, 2u16);
    let (out, mut w) = match wasapi::Output::start(rate, ch, 200) {
        Ok(x) => x,
        Err(e) => {
            println!("FAILED    output: {e}");
            return 1;
        }
    };
    let total = (rate as f64 * seconds) as usize;
    let mut i = 0usize;
    let mut piece = Vec::with_capacity(960 * 2);
    while i < total {
        piece.clear();
        for k in i..(i + 960).min(total) {
            let t = k as f32 / rate as f32;
            let v = 0.05 * ((t * 440.0 * std::f32::consts::TAU).sin() + 0.5 * (t * 660.0 * std::f32::consts::TAU).sin()) / 1.5;
            piece.extend_from_slice(&[v, v]);
        }
        let mut at = 0;
        while at < piece.len() {
            at += w.push(&piece[at..]);
            if at < piece.len() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        i += 960;
    }
    while w.held() > 0 {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    drop(out);
    hostout::flush();
    for l in hostout::report() {
        println!("hooks     {l}");
    }
    0
}

fn selftest(args: &[String]) -> i32 {
    let Some(dir) = arg(args, "--out").map(PathBuf::from) else {
        eprintln!("selftest needs --out <dir>");
        return 2;
    };
    let volume = arg(args, "--volume").unwrap_or_else(|| "mute".into());
    let _ = std::fs::create_dir_all(&dir);
    let tap = dir.join("selftest-tap.wav");
    let spatial = args.iter().any(|a| a == "--spatial");
    let cap = dir.join("selftest-loopback.wav");
    let me = std::env::current_exe().unwrap();
    let child = match std::process::Command::new(me)
        .args([if args.iter().any(|a| a == "--spatial") { "spatialtone" } else { "tone" }, "--seconds", "1.5"])
        .env("REBUILD_AUDIO_SINK", &tap)
        .env("REBUILD_AUDIO_VOLUME", &volume)
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            println!("FAILED    child: {e}");
            return 1;
        }
    };
    let pid = child.id();
    let rec = loopback::record(pid, &cap, Some(10.0), || {});
    let out = child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    print!("{out}");
    match rec {
        Ok(r) => println!(
            "loopback  {:.1} s, {} packets ({} flagged silent, {} discontinuities), {} frames filled",
            r.seconds, r.packets, r.silent_packets, r.discontinuities, r.filled_frames
        ),
        Err(e) => {
            println!("FAILED    loopback: {e}");
            return 1;
        }
    }
    println!("--- the child's own buffers (hooks' file tap), session volume {volume}");
    let t = print_analysis(&dir.join(if spatial { "selftest-tap-spatial-1.wav" } else { "selftest-tap-1.wav" }));
    println!("--- what the engine played for the child (process loopback)");
    let l = print_analysis(&cap);
    match (t, l) {
        (Some(t), Some(l)) => {
            println!(
                "selftest  tap peak {:.1} dBFS, loopback peak {:.1} dBFS: loopback is taken {} the session volume",
                t.peak_db,
                l.peak_db,
                if l.peak_db > t.peak_db - 3.0 { "before" } else { "after" }
            );
            0
        }
        _ => 1,
    }
}

/// Puts this process's audio session on the default device back to full volume, unmuted (the default). An earlier
/// version set the session volume, which Windows keeps per program; the tones' gain is now `REBUILD_AUDIO_VOLUME`.
fn reset_session() {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    // SAFETY: COM on this thread, objects released here
    let _ = unsafe {
        (|| -> windows::core::Result<()> {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let m: IAudioSessionManager = e.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
            let v = m.GetSimpleAudioVolume(None, 0)?;
            v.SetMasterVolume(1.0, std::ptr::null())?;
            v.SetMute(windows::Win32::Foundation::BOOL::from(false), std::ptr::null())
        })()
    };
}

fn chord(k: usize, rate: u32) -> f32 {
    let t = k as f32 / rate as f32;
    0.05 * ((t * 440.0 * std::f32::consts::TAU).sin() + 0.5 * (t * 660.0 * std::f32::consts::TAU).sin()) / 1.5
}

/// The child for `selftest --spatial`: hooks, then the quiet chord through a spatial audio stream with two static
/// objects (front left and right), the way the host's XAudio2 renders its mastering voice.
fn spatial_tone(args: &[String]) -> i32 {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    let seconds = arg(args, "--seconds").and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.5);
    reset_session();
    if let Err(e) = hostout::install() {
        println!("FAILED    hooks: {e}");
        return 1;
    }
    #[repr(C)]
    struct Params {
        format: *const WAVEFORMATEX,
        mask: u32,
        min: u32,
        max: u32,
        category: i32,
        event: windows::Win32::Foundation::HANDLE,
        notify: *const std::ffi::c_void,
    }
    #[repr(C)]
    struct Blob {
        vt: u16,
        reserved: [u16; 3],
        size: u32,
        data: *const std::ffi::c_void,
    }
    // SAFETY: COM on this thread; every object lives in this block
    let r = unsafe {
        (|| -> windows::core::Result<u64> {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = e.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: ISpatialAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let rate = 48_000u32;
            let format = WAVEFORMATEX { wFormatTag: 3, nChannels: 1, nSamplesPerSec: rate, nAvgBytesPerSec: rate * 4, nBlockAlign: 4, wBitsPerSample: 32, cbSize: 0 };
            let event = CreateEventW(None, false, false, None)?;
            let params = Params { format: &format, mask: 2 | 4, min: 0, max: 0, category: 6, event, notify: std::ptr::null() };
            let blob = Blob { vt: 65, reserved: [0; 3], size: std::mem::size_of::<Params>() as u32, data: (&params as *const Params).cast() };
            let stream: ISpatialAudioObjectRenderStream = client.ActivateSpatialAudioStream((&blob as *const Blob).cast())?;
            let left = stream.ActivateSpatialAudioObject(AudioObjectType_FrontLeft)?;
            let right = stream.ActivateSpatialAudioObject(AudioObjectType_FrontRight)?;
            stream.Start()?;
            let total = (rate as f64 * seconds) as usize;
            let mut at = 0usize;
            let mut periods = 0u64;
            while at < total {
                if WaitForSingleObject(event, 200) != windows::Win32::Foundation::WAIT_OBJECT_0 {
                    continue;
                }
                let (mut avail, mut frames) = (0u32, 0u32);
                stream.BeginUpdatingAudioObjects(&mut avail, &mut frames)?;
                for o in [&left, &right] {
                    let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
                    o.GetBuffer(&mut p, &mut len)?;
                    let b = std::slice::from_raw_parts_mut(p.cast::<f32>(), frames as usize);
                    for (i, v) in b.iter_mut().enumerate() {
                        *v = chord(at + i, rate);
                    }
                }
                stream.EndUpdatingAudioObjects()?;
                at += frames as usize;
                periods += 1;
            }
            std::thread::sleep(Duration::from_millis(100));
            stream.Stop()?;
            Ok(periods)
        })()
    };
    match r {
        Ok(p) => println!("spatial   {p} periods rendered"),
        Err(e) => {
            println!("FAILED    spatial stream: {e}");
            return 1;
        }
    }
    hostout::flush();
    for l in hostout::report() {
        println!("hooks     {l}");
    }
    0
}
