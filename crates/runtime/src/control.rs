//! `REBUILD_INPUT_PIPE=<name>`: a live control channel for a run (a dev tool; off without the variable).
//!
//! A named pipe `\\.\pipe\<name>` (local clients only, one at a time) that takes one command a line and answers
//! one line, `ok ...` or `err <why>`:
//!
//! | command | does | answer |
//! |:--|:--|:--|
//! | `ping`, `frame` | nothing | `ok <frame> <seconds>`: frames presented, seconds since the process started |
//! | `press <what>` | a pad press, the input script's `<what>` (`A`, `Down+LT/500ms`, `RDown/3000ms`, `A/4f`), due at once | `ok <frame> <seconds>` when queued |
//! | `cap` | capture the next shown frame | `ok <png path>` once the PNG is written (15 s at most) |
//! | `exit` | write the session summary (`pipe exit`) and end the run | `ok`, then the process ends |
//!
//! Presses go the input script's way ([`rebuild_input::script`]): added to what the title reads, the real devices
//! still working; every press that starts is printed (`input     pipe ...`). `titlectl` (apps/rebuild) is the
//! client and the setup driver.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rebuild_input::script::{self, At, Poll, Script};

/// Presses from the pipe, due at once.
static LIVE: Mutex<Option<Script>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);

fn live() -> std::sync::MutexGuard<'static, Option<Script>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pipe's name from `REBUILD_INPUT_PIPE` (a bare name, or a full `\\.\pipe\` path).
pub fn pipe_name() -> Option<String> {
    let v = std::env::var("REBUILD_INPUT_PIPE").ok()?;
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    Some(full_name(v))
}

pub fn full_name(name: &str) -> String {
    if name.starts_with(r"\\.\pipe\") {
        name.to_string()
    } else {
        format!(r"\\.\pipe\{name}")
    }
}

/// The server runs (the input poller asks the pipe only then).
pub fn active() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// What the pipe adds to this poll (the input poller calls it every 8 ms). One lock when nothing is queued.
pub fn poll(seconds: f64, frame: u64) -> Option<Poll> {
    let mut g = live();
    let s = g.as_mut()?;
    if s.is_empty() {
        return None;
    }
    let p = s.poll(seconds, frame);
    s.prune(seconds, frame);
    Some(p)
}

/// Queue a press (`<what>` as in the input script).
pub fn press(what: &str) -> Result<(), String> {
    let action = script::parse_action(what)?;
    live().get_or_insert_with(Script::default).push(At::Seconds(0.0), action, what);
    Ok(())
}

fn now() -> (u64, f64) {
    (crate::framelog::frames(), crate::framelog::elapsed().as_secs_f64())
}

/// One command, one answer (`exit` is answered by the server, which then ends the process).
pub fn handle(line: &str) -> String {
    let line = line.trim();
    let (cmd, arg) = line.split_once(char::is_whitespace).map_or((line, ""), |(c, a)| (c, a.trim()));
    match cmd.to_ascii_lowercase().as_str() {
        "ping" | "frame" => {
            let (f, s) = now();
            format!("ok {f} {s:.2}")
        }
        "press" => match press(arg) {
            Ok(()) => {
                let (f, s) = now();
                format!("ok {f} {s:.2}")
            }
            Err(e) => format!("err {e}"),
        },
        "cap" | "capture" => capture(),
        "help" => "ok ping | frame | press <what> | cap | exit".into(),
        "" => "err empty command".into(),
        other => format!("err no command '{other}' (help)"),
    }
}

#[cfg(windows)]
fn capture() -> String {
    let before = crate::present::captures().len();
    crate::present::request_capture(None);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        let c = crate::present::captures();
        if c.len() > before {
            return format!("ok {}", c.last().expect("grew").display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    "err no frame was captured within 15 s".into()
}

#[cfg(not(windows))]
fn capture() -> String {
    let _ = (Instant::now(), Duration::ZERO);
    "err captures need the presenter (Windows)".into()
}

/// Start the server thread when `REBUILD_INPUT_PIPE` is set (later calls do nothing).
pub fn start() {
    let Some(name) = pipe_name() else { return };
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    eprintln!("control   REBUILD_INPUT_PIPE: {name} (ping | frame | press <what> | cap | exit)");
    let spawned = std::thread::Builder::new().name("control-pipe".into()).stack_size(256 << 10).spawn(move || serve(&name));
    if let Err(e) = spawned {
        eprintln!("control   no pipe thread: {e}");
    }
}

/// Serve clients one after another, for ever. `exit` writes the summary and ends the process.
#[cfg(windows)]
pub fn serve(name: &str) {
    use std::os::windows::io::FromRawHandle;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::*;
    let wide = HSTRING::from(name);
    loop {
        let h = unsafe {
            CreateNamedPipeW(&wide, PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, 1, 1 << 16, 1 << 16, 0, None)
        };
        if h == INVALID_HANDLE_VALUE || h.is_invalid() {
            eprintln!("control   could not create {name}: {}", std::io::Error::last_os_error());
            return;
        }
        if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        // SAFETY: a pipe handle we own; the File closes it (which disconnects the client).
        let file = unsafe { std::fs::File::from_raw_handle(h.0 as _) };
        let Ok(mut out) = file.try_clone() else { continue };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().eq_ignore_ascii_case("exit") {
                let _ = out.write_all(b"ok\n");
                let _ = out.flush();
                eprintln!("control   exit asked on {name}: summary written, ending the run");
                crate::framelog::write_summary("pipe exit");
                crate::present::shutdown();
                std::process::exit(0);
            }
            let answer = handle(&line);
            if out.write_all(answer.as_bytes()).and_then(|_| out.write_all(b"\n")).and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

#[cfg(not(windows))]
pub fn serve(_name: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_input::pad::buttons as b;

    #[test]
    fn commands_answer_one_line() {
        assert!(handle("ping").starts_with("ok "));
        assert!(handle("  frame ").starts_with("ok "));
        assert!(handle("press Nope").starts_with("err no button named 'Nope'"));
        assert!(handle("jump").starts_with("err no command 'jump'"));
        assert!(handle("").starts_with("err"));
        assert_eq!(full_name("setup1"), r"\\.\pipe\setup1");
        assert_eq!(full_name(r"\\.\pipe\x"), r"\\.\pipe\x");
    }

    /// Over a real pipe: the press reaches the poller, held for its time, then gone.
    #[cfg(windows)]
    #[test]
    fn a_press_over_the_pipe_reaches_the_poller() {
        let name = full_name(&format!("rebuild-control-test-{}", std::process::id()));
        let n = name.clone();
        std::thread::spawn(move || serve(&n));
        let t = Instant::now();
        let file = loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                Ok(f) => break f,
                Err(_) if t.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("{name}: {e}"),
            }
        };
        let mut w = file.try_clone().unwrap();
        let mut r = BufReader::new(file);
        let mut ask = |cmd: &str| {
            w.write_all(format!("{cmd}\n").as_bytes()).unwrap();
            let mut s = String::new();
            r.read_line(&mut s).unwrap();
            s.trim().to_string()
        };
        assert!(ask("ping").starts_with("ok "));
        assert!(ask("press X/3f").starts_with("ok "));
        assert!(ask("press X/").starts_with("err"));
        let p = poll(10.0, 50).expect("queued");
        assert_eq!(p.pad.unwrap().buttons, b::X);
        assert_eq!(poll(10.0, 52).unwrap().pad.unwrap().buttons, b::X, "held 3 frames");
        assert_eq!(poll(10.0, 53).and_then(|p| p.pad), None, "released");
        assert!(poll(10.0, 54).is_none(), "nothing left");
    }
}
