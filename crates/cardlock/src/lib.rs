//! The lanes' exclusive hold on the card: `agents\CARD`.
//!
//! The file names its owner (`pid <n>`, `start <creation time>`, `tool <exe>`). A lock whose owner is no longer
//! running (or whose pid now belongs to a later process) is stale and is taken over, so a tool that ended without
//! running its destructors (`std::process::exit`, `panic = "abort"`, a crash, a kill) never blocks later card jobs.
//! Besides the drop, [`exit`] releases every lock this process holds before it exits, and under `panic = "abort"`
//! a panic hook does the same before the abort.
//!
//! The file is written whole and then linked into place (`hard_link` fails when the name exists), so a reader never
//! sees a half-written owner. A file with no owner in it (an older tool that wrote after creating) counts as held
//! for [`UNNAMED_GRACE`] after its last write and as stale after that.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::Duration;

/// The agents folder of the lead's tree, where `MEASURING` and `CARD` live.
pub const AGENTS: &str = r"D:\Developer\triumph-rebuild\agents";

/// The agents folder in use: `REBUILD_LEAD_DIR\agents` when that variable is set (as `execcheck` reads it), else
/// [`AGENTS`].
pub fn agents_dir() -> PathBuf {
    match std::env::var_os("REBUILD_LEAD_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("agents"),
        None => PathBuf::from(AGENTS),
    }
}

/// How long a lock file with no owner in it is taken as being written.
pub const UNNAMED_GRACE: Duration = Duration::from_secs(10);

/// The locks this process holds, for [`exit`] and the panic hook.
static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static HOOK: Once = Once::new();

/// Who holds a lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    /// The process's creation time (Windows FILETIME), when the file records it; tells a reused pid apart.
    pub start: Option<u64>,
}

impl Owner {
    /// This process.
    pub fn me() -> Owner {
        Owner { pid: std::process::id(), start: sys::start_time(std::process::id()) }
    }

    /// The owner a lock file names: `pid <n>` anywhere (also the older `rebuild-pipelines pid <n>`), `start <t>`.
    pub fn parse(text: &str) -> Option<Owner> {
        let w: Vec<&str> = text.split_whitespace().collect();
        let after = |key: &str| w.iter().position(|t| *t == key).and_then(|i| w.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
        let pid = u32::try_from(after("pid")?).ok()?;
        Some(Owner { pid, start: after("start") })
    }

    fn text(&self, tool: &str) -> String {
        let mut s = format!("pid {}\n", self.pid);
        if let Some(t) = self.start {
            s += &format!("start {t}\n");
        }
        s + &format!("tool {tool}\n")
    }

    /// Whether this owner is still running (a reused pid, told apart by its start time, is not).
    pub fn alive(&self) -> bool {
        sys::alive(self.pid, self.start)
    }
}

/// The exclusive hold on the card (`agents\CARD`). Removed on drop, by [`exit`], and by a panic under
/// `panic = "abort"`; left behind only by a crash or a kill, and then taken over as stale.
#[derive(Debug)]
pub struct CardLock {
    path: PathBuf,
    owner: Owner,
}

impl CardLock {
    /// The lock in the lead's agents folder, or why not (the measurement lock is present, or a running lane
    /// holds the card).
    pub fn take() -> Result<CardLock, String> {
        Self::take_in(&agents_dir())
    }

    /// The lock in `dir` (`dir\CARD`), refused while `dir\MEASURING` exists.
    pub fn take_in(dir: &Path) -> Result<CardLock, String> {
        if dir.join("MEASURING").exists() {
            return Err("the measurement lock is present".into());
        }
        let path = dir.join("CARD");
        let me = Owner::me();
        let tool = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        let text = me.text(&tool);
        for _ in 0..8 {
            match place(&path, &text) {
                Ok(()) => {
                    hold(&path);
                    return Ok(CardLock { path, owner: me });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
            let Ok(found) = std::fs::read_to_string(&path) else { continue }; // released meanwhile: again
            match Owner::parse(&found) {
                Some(o) if o.pid == me.pid && (o.start.is_none() || o.start == me.start) => {
                    return Err("this process already holds the card (agents\\CARD)".into())
                }
                Some(o) if o.alive() => return Err(format!("another lane holds the card (agents\\CARD, pid {})", o.pid)),
                Some(_) => take_over(&path, &found),
                None => {
                    let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                    if age.is_some_and(|a| a < UNNAMED_GRACE) {
                        return Err("another lane holds the card (agents\\CARD, being written)".into());
                    }
                    take_over(&path, &found);
                }
            }
        }
        Err("the card lock kept changing hands (agents\\CARD)".into())
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the lock in `dir` when it names `pid` and that process has ended: a supervisor's clean-up after a
    /// worker it waited for. Whether it removed one.
    pub fn release_dead(dir: &Path, pid: u32) -> bool {
        let path = dir.join("CARD");
        match std::fs::read_to_string(&path).ok().as_deref().and_then(Owner::parse) {
            Some(o) if o.pid == pid && !o.alive() => std::fs::remove_file(&path).is_ok(),
            _ => false,
        }
    }
}

impl Drop for CardLock {
    fn drop(&mut self) {
        unhold(&self.path);
        release_if_mine(&self.path, self.owner);
    }
}

/// Writes `text` to a private file and links it in as `path`; `AlreadyExists` when `path` is there.
fn place(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!("CARD.{}.new", std::process::id()));
    std::fs::write(&tmp, text)?;
    let r = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    r
}

/// Moves a stale lock aside and deletes it, unless what was moved is no longer the stale lock that was read
/// (another lane placed its own in between): that one is put back.
fn take_over(path: &Path, stale: &str) {
    let aside = path.with_file_name(format!("CARD.{}.stale", std::process::id()));
    if std::fs::rename(path, &aside).is_err() {
        return;
    }
    if std::fs::read_to_string(&aside).is_ok_and(|t| t != stale) {
        let _ = std::fs::hard_link(&aside, path);
    }
    let _ = std::fs::remove_file(&aside);
}

fn release_if_mine(path: &Path, me: Owner) {
    if std::fs::read_to_string(path).ok().as_deref().and_then(Owner::parse).is_some_and(|o| o.pid == me.pid) {
        let _ = std::fs::remove_file(path);
    }
}

fn hold(path: &Path) {
    HOOK.call_once(|| {
        // with unwinding the drops run; with abort nothing does, so the hook releases before the abort
        if cfg!(panic = "abort") {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                prev(info);
                release_all();
            }));
        }
    });
    HELD.lock().unwrap_or_else(|e| e.into_inner()).push(path.to_path_buf());
}

fn unhold(path: &Path) {
    let mut h = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = h.iter().position(|p| p == path) {
        h.swap_remove(i);
    }
}

/// Releases every card lock this process holds (the drops then find nothing to remove).
pub fn release_all() {
    let held: Vec<PathBuf> = std::mem::take(&mut *HELD.lock().unwrap_or_else(|e| e.into_inner()));
    let me = Owner::me();
    for p in held {
        release_if_mine(&p, me);
    }
}

/// `std::process::exit` for a tool that may hold the card: releases its locks first (exit skips destructors).
pub fn exit(code: i32) -> ! {
    release_all();
    std::process::exit(code)
}

#[cfg(windows)]
mod sys {
    type Handle = isize;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_INVALID_PARAMETER: u32 = 87;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
        fn GetProcessTimes(process: Handle, creation: *mut [u32; 2], exit: *mut [u32; 2], kernel: *mut [u32; 2], user: *mut [u32; 2]) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    fn creation(h: Handle) -> Option<u64> {
        let (mut c, mut e, mut k, mut u) = ([0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2]);
        // SAFETY: a process handle with query rights and four FILETIME-sized outputs
        (unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0).then(|| (c[1] as u64) << 32 | c[0] as u64)
    }

    pub fn start_time(pid: u32) -> Option<u64> {
        // SAFETY: plain Win32 calls; the handle is closed before returning
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h == 0 {
                return None;
            }
            let t = creation(h);
            CloseHandle(h);
            t
        }
    }

    pub fn alive(pid: u32, start: Option<u64>) -> bool {
        // SAFETY: plain Win32 calls; the handle is closed before returning
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h == 0 {
                // no such process; any other refusal (access denied) means it exists
                return GetLastError() != ERROR_INVALID_PARAMETER;
            }
            let mut code = 0u32;
            let running = GetExitCodeProcess(h, &mut code) == 0 || code == STILL_ACTIVE;
            let same = match (start, creation(h)) {
                (Some(want), Some(got)) => want == got,
                _ => true,
            };
            CloseHandle(h);
            running && same
        }
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn start_time(_pid: u32) -> Option<u64> {
        None
    }

    pub fn alive(pid: u32, _start: Option<u64>) -> bool {
        // where /proc is missing, every owner counts as running (the lock then behaves as before)
        !std::path::Path::new("/proc/self").exists() || std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

#[cfg(test)]
mod tests;
