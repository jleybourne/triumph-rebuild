//! U5: Play. Launch the layer with `--config <rebuild.toml>`, and show its frame-line log as it grows
//! (a file the layer appends; no sockets).
//!
//! Every launch runs in a session folder (`session.rs`): the folder goes to the child as
//! `REBUILD_SESSION_DIR`, and the log's path as `REBUILD_FRAME_LOG` (`<session>/frame-line.log`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

/// The layer: `REBUILD_EXE`, else `rebuild.exe` beside the launcher.
pub fn find_layer() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("REBUILD_EXE") {
        return Some(PathBuf::from(p));
    }
    let p = std::env::current_exe().ok()?.parent()?.join("rebuild.exe");
    p.is_file().then_some(p)
}

/// The command the launcher runs. `extra` are further arguments (a proof's own flags).
pub fn command(exe: &Path, config: &Path, frame_log: &Path, extra: &[String]) -> Command {
    command_in(exe, config, frame_log, None, &[], extra)
}

/// The command, in a session: `REBUILD_SESSION_DIR` and any further environment (`env`).
pub fn command_in(exe: &Path, config: &Path, frame_log: &Path, session: Option<&Path>, env: &[(String, String)], extra: &[String]) -> Command {
    let mut c = Command::new(exe);
    c.arg("--config").arg(config).args(extra).env("REBUILD_FRAME_LOG", frame_log);
    if let Some(d) = session {
        c.env(crate::session::ENV_SESSION_DIR, d);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    // every other setting (limits, game folder, vsync, WARP…) the layer reads from the file itself (C2,
    // `rebuild_config::runtime`)
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // in a session, the layer's own output (what it applied, the run, a crash) goes to `layer.log` there
    if let Some(log) = session.and_then(|d| File::create(d.join(crate::session::LAYER_LOG)).ok()) {
        if let Ok(err) = log.try_clone() {
            c.stdout(log).stderr(err);
        }
    }
    if let Some(d) = exe.parent() {
        c.current_dir(d);
    }
    c
}

/// A running layer.
pub struct Running {
    child: Child,
    pub started: std::time::Instant,
    /// stopped by the launcher (Stop, or a matrix run past its length)
    pub stopped: bool,
}

impl Running {
    /// Start it. The frame log is emptied first so the tail shows this run only.
    pub fn start(exe: &Path, config: &Path, frame_log: &Path, extra: &[String]) -> Result<Running, String> {
        Running::start_in(exe, config, frame_log, None, &[], extra)
    }

    /// Start it in a session folder (`REBUILD_SESSION_DIR`), with further environment.
    pub fn start_in(exe: &Path, config: &Path, frame_log: &Path, session: Option<&Path>, env: &[(String, String)], extra: &[String]) -> Result<Running, String> {
        if let Some(d) = frame_log.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(frame_log, b"").map_err(|e| format!("{}: {e}", frame_log.display()))?;
        let child = command_in(exe, config, frame_log, session, env, extra).spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
        Ok(Running { child, started: std::time::Instant::now(), stopped: false })
    }

    /// Start a proof binary (`gputest --bare` and the like) as it is, with its standard output
    /// appended to the frame log, so the Play tab shows its result through the same tail.
    pub fn start_proof(exe: &Path, args: &[String], frame_log: &Path) -> Result<Running, String> {
        Running::start_proof_in(exe, args, frame_log, None)
    }

    pub fn start_proof_in(exe: &Path, args: &[String], frame_log: &Path, session: Option<&Path>) -> Result<Running, String> {
        if let Some(d) = frame_log.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let out = File::create(frame_log).map_err(|e| format!("{}: {e}", frame_log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let mut c = Command::new(exe);
        c.args(args).stdin(Stdio::null()).stdout(out).stderr(err);
        if let Some(d) = session {
            c.env(crate::session::ENV_SESSION_DIR, d);
        }
        if let Some(d) = exe.parent() {
            c.current_dir(d);
        }
        let child = c.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
        Ok(Running { child, started: std::time::Instant::now(), stopped: false })
    }

    /// `Some(status)` once it has exited.
    pub fn poll(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    pub fn wait(mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }

    pub fn stop(&mut self) {
        self.stopped = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the lines a file gains. A partial last line waits for its newline; a file that shrank
/// (a new run) is read again from the start.
#[derive(Debug)]
pub struct LogTail {
    pub path: PathBuf,
    pos: u64,
    partial: Vec<u8>,
}

impl LogTail {
    pub fn new(path: PathBuf) -> LogTail {
        LogTail { path, pos: 0, partial: Vec::new() }
    }

    /// The complete lines added since the last call.
    pub fn poll(&mut self) -> Vec<String> {
        let Ok(mut f) = File::open(&self.path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            self.pos = 0;
            self.partial.clear();
        }
        if len == self.pos || f.seek(SeekFrom::Start(self.pos)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let Ok(n) = f.take(len - self.pos).read_to_end(&mut buf) else { return Vec::new() };
        self.pos += n as u64;
        self.partial.extend_from_slice(&buf);
        let mut lines = Vec::new();
        while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string());
        }
        lines
    }
}

/// The fps of the last frame line that has one (`... fps 29.8 ...` or `fps=29.8`).
pub fn last_fps<S: AsRef<str>>(lines: &[S]) -> Option<f32> {
    lines.iter().rev().find_map(|l| {
        let mut words = l.as_ref().split(|c: char| c.is_whitespace() || c == '|' || c == '=' || c == ':').filter(|w| !w.is_empty());
        while let Some(w) = words.next() {
            if w.eq_ignore_ascii_case("fps") {
                if let Some(v) = words.next().and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite()) {
                    return Some(v);
                }
            }
        }
        None
    })
}

/// The last session's lines from a frame log (the last 200), for the Play tab at start.
pub fn read_last(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(String::from).collect();
    lines[lines.len().saturating_sub(200)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn last_fps_reads_the_newest_frame_line() {
        assert_eq!(last_fps(&["fps 30.0 | ms 33.3", "fps 29.5 | ms 33.9", "exited"]), Some(29.5));
        assert_eq!(last_fps(&["fps=7.6 ms=131.3"]), Some(7.6));
        assert_eq!(last_fps(&["config ok", "fps nan"]), None);
        assert_eq!(last_fps::<&str>(&[]), None);
    }

    #[test]
    fn the_tail_reads_whole_lines_and_restarts_on_a_new_file() {
        let d = crate::temp_dir("tail");
        let p = d.join("frame-line.log");
        let mut t = LogTail::new(p.clone());
        assert!(t.poll().is_empty(), "no file yet");
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).unwrap();
        f.write_all(b"fps 30.0 | ms 33.3\r\nfps 29.").unwrap();
        f.flush().unwrap();
        assert_eq!(t.poll(), vec!["fps 30.0 | ms 33.3"]);
        assert!(t.poll().is_empty(), "half a line waits");
        f.write_all(b"8 | ms 33.6\n").unwrap();
        f.flush().unwrap();
        assert_eq!(t.poll(), vec!["fps 29.8 | ms 33.6"]);
        drop(f);
        std::fs::write(&p, b"new run\n").unwrap();
        assert_eq!(t.poll(), vec!["new run"]);
    }
}
