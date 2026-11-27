//! Session diagnostics (L6, owner request 2026-11-27): every launch gets its own folder, kept for
//! `diagnostics.retention_days` days.
//!
//! ```text
//! <home>/sessions/<UTC stamp>-<profile>/     e.g. 20261127T153012Z-series-x
//!   session.toml       written by the launcher: [session] at launch, [end] when the child exits
//!   frame-line.log     the frame line (REBUILD_FRAME_LOG points here)
//!   summary.toml       the run's summary (the layer writes it; see `frameline::Summary`)
//!   crash-*.txt        crash reports (the layer / runtitle)
//!   captures/          frame captures (the layer)
//!   capture.request    the launcher's "Capture frame" button; the layer captures, then deletes it
//! ```
//!
//! The folder goes to the child as `REBUILD_SESSION_DIR`. Pruning only ever deletes folders directly
//! under `sessions/` whose name starts with a session stamp, so nothing else can be removed by it.

use crate::frameline::Summary;
use rebuild_config::{Config, Value, REGISTRY};
use std::path::{Path, PathBuf};

pub const ENV_SESSION_DIR: &str = "REBUILD_SESSION_DIR";
pub const SESSION_TOML: &str = "session.toml";
pub const FRAME_LOG: &str = "frame-line.log";
pub const SUMMARY_TOML: &str = "summary.toml";
pub const CAPTURES: &str = "captures";
pub const CAPTURE_REQUEST: &str = "capture.request";

pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// Seconds since the Unix epoch, now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parts(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let (y, m, d) = civil((secs / 86_400) as i64);
    let s = secs % 86_400;
    (y, m, d, s / 3600, s / 60 % 60, s % 60)
}

/// `20261127T153012Z`: the folder-name stamp (sorts by time, no characters a path refuses).
pub fn stamp(secs: u64) -> String {
    let (y, m, d, h, mi, s) = parts(secs);
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `2026-11-27T15:30:12Z`
pub fn rfc3339(secs: u64) -> String {
    let (y, m, d, h, mi, s) = parts(secs);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `2026-11-27 15:30 UTC`, for lists.
pub fn short_time(secs: u64) -> String {
    let (y, m, d, h, mi, _) = parts(secs);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// The start time from a session folder's name (its first 16 characters), if it is one.
pub fn parse_stamp(name: &str) -> Option<u64> {
    let b = name.as_bytes();
    if b.len() < 16 || b[8] != b'T' || b[15] != b'Z' || (b.len() > 16 && b[16] != b'-') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u64> {
        let s = name.get(r)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok())?
    };
    let (y, m, d, h, mi, s) = (num(0..4)?, num(4..6)?, num(6..8)?, num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let days = days_from_civil(y as i64, m as u32, d as u32);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + h * 3600 + mi * 60 + s)
}

/// A profile name as a folder-name part: letters, digits, `-` and `_`; anything else becomes `-`.
pub fn folder_part(profile: &str) -> String {
    let s: String = profile.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(40).collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "default".into()
    } else {
        s
    }
}

/// What the launcher records at launch.
#[derive(Debug, Clone)]
pub struct Start<'a> {
    pub profile: &'a str,
    pub config: &'a Config,
    pub exe: &'a Path,
    pub config_path: &'a Path,
    /// the test matrix this run belongs to (its id), if any
    pub matrix: Option<&'a str>,
    pub run_seconds: Option<u64>,
    /// "title_start" or "after_loading"
    pub measure_from: Option<&'a str>,
}

fn toml_value(v: Value) -> toml::Value {
    match v {
        Value::Bool(b) => toml::Value::Boolean(b),
        Value::Int(i) => toml::Value::Integer(i),
        Value::Choice(s) | Value::Path(s) => toml::Value::String(s),
    }
}

/// Make the session folder (`<sessions>/<stamp>-<profile>`, with `-2`, `-3`… if taken) and write its
/// `session.toml`: the profile, every registry setting's effective value, the limits in force, the start
/// time and the launcher's version.
pub fn create(sessions: &Path, s: &Start, now: u64) -> Result<PathBuf, String> {
    std::fs::create_dir_all(sessions).map_err(|e| format!("{}: {e}", sessions.display()))?;
    let base = format!("{}-{}", stamp(now), folder_part(s.profile));
    let mut dir = sessions.join(&base);
    let mut n = 2;
    while dir.exists() {
        dir = sessions.join(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut sess = toml::Table::new();
    let mut put = |k: &str, v: toml::Value| {
        sess.insert(k.into(), v);
    };
    put("profile", toml::Value::String(s.profile.into()));
    put("profile_label", toml::Value::String(rebuild_config::profiles::Active::of(s.config).describe()));
    put("started", toml::Value::String(rfc3339(now)));
    put("started_unix", toml::Value::Integer(now as i64));
    put("launcher_version", toml::Value::String(env!("CARGO_PKG_VERSION").into()));
    put("exe", toml::Value::String(s.exe.display().to_string()));
    put("config", toml::Value::String(s.config_path.display().to_string()));
    let limits = rebuild_config::profiles::limits_in_force(s.config);
    put("limits", toml::Value::Array(limits.into_iter().map(toml::Value::String).collect()));
    if let Some(m) = s.matrix {
        put("matrix", toml::Value::String(m.into()));
    }
    if let Some(r) = s.run_seconds {
        put("run_seconds", toml::Value::Integer(r as i64));
    }
    if let Some(m) = s.measure_from {
        put("measure_from", toml::Value::String(m.into()));
    }
    let mut settings = toml::Table::new();
    for r in REGISTRY {
        settings.insert(r.key.into(), toml_value(s.config.get(r.key)));
    }
    let mut doc = toml::Table::new();
    doc.insert("session".into(), toml::Value::Table(sess));
    doc.insert("settings".into(), toml::Value::Table(settings));
    let text = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    crate::write_atomic(&dir.join(SESSION_TOML), &text).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// How the child ended.
#[derive(Debug, Clone, PartialEq)]
pub struct End {
    /// `None` when the process gave no code (killed)
    pub code: Option<i32>,
    /// the status as the OS described it
    pub status: String,
    pub stopped_by_launcher: bool,
}

/// Add `[end]` to `session.toml`: end time, how long it ran, the exit status.
pub fn finish(dir: &Path, end: &End, now: u64) -> Result<(), String> {
    let path = dir.join(SESSION_TOML);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut doc: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    let started = doc.get("session").and_then(|s| s.get("started_unix")).and_then(|v| v.as_integer()).unwrap_or(now as i64);
    let mut t = toml::Table::new();
    t.insert("ended".into(), toml::Value::String(rfc3339(now)));
    t.insert("ended_unix".into(), toml::Value::Integer(now as i64));
    t.insert("seconds".into(), toml::Value::Integer((now as i64 - started).max(0)));
    if let Some(c) = end.code {
        t.insert("exit_code".into(), toml::Value::Integer(c as i64));
    }
    t.insert("exit".into(), toml::Value::String(end.status.clone()));
    t.insert("stopped_by_launcher".into(), toml::Value::Boolean(end.stopped_by_launcher));
    doc.insert("end".into(), toml::Value::Table(t));
    let text = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    crate::write_atomic(&path, &text).map_err(|e| format!("{}: {e}", path.display()))
}

/// `session.toml`, read back.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Info {
    pub profile: String,
    pub profile_label: String,
    pub started: Option<u64>,
    pub ended: Option<u64>,
    pub seconds: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit: Option<String>,
    pub stopped_by_launcher: bool,
    pub launcher_version: String,
    pub limits: Vec<String>,
    pub matrix: Option<String>,
    pub run_seconds: Option<i64>,
    pub measure_from: Option<String>,
    /// every setting as recorded (key → value as text)
    pub settings: Vec<(String, String)>,
}

impl Info {
    pub fn parse(text: &str) -> Result<Info, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("session.toml: {e}"))?;
        let empty = toml::Table::new();
        let s = doc.get("session").and_then(|v| v.as_table()).unwrap_or(&empty);
        let e = doc.get("end").and_then(|v| v.as_table()).unwrap_or(&empty);
        let st = |t: &toml::Table, k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
        let int = |t: &toml::Table, k: &str| t.get(k).and_then(|v| v.as_integer());
        let settings = doc
            .get("settings")
            .and_then(|v| v.as_table())
            .map(|t| t.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
            .unwrap_or_default();
        Ok(Info {
            profile: st(s, "profile").unwrap_or_default(),
            profile_label: st(s, "profile_label").unwrap_or_default(),
            started: int(s, "started_unix").map(|v| v.max(0) as u64),
            ended: int(e, "ended_unix").map(|v| v.max(0) as u64),
            seconds: int(e, "seconds"),
            exit_code: int(e, "exit_code"),
            exit: st(e, "exit"),
            stopped_by_launcher: e.get("stopped_by_launcher").and_then(|v| v.as_bool()).unwrap_or(false),
            launcher_version: st(s, "launcher_version").unwrap_or_default(),
            limits: s.get("limits").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            matrix: st(s, "matrix"),
            run_seconds: int(s, "run_seconds"),
            measure_from: st(s, "measure_from"),
            settings,
        })
    }

    pub fn read(dir: &Path) -> Option<Info> {
        Info::parse(&std::fs::read_to_string(dir.join(SESSION_TOML)).ok()?).ok()
    }

    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// One session folder, as the Runs and Diagnostics lists show it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub dir: PathBuf,
    pub name: String,
    pub started: u64,
    pub info: Option<Info>,
    pub summary: Option<Summary>,
    pub crashes: Vec<PathBuf>,
    pub captures: Vec<PathBuf>,
    pub bytes: u64,
}

impl Entry {
    pub fn profile(&self) -> String {
        match &self.info {
            Some(i) if !i.profile.is_empty() => i.profile.clone(),
            _ => self.name.get(17..).unwrap_or("").to_string(),
        }
    }
    pub fn frame_log(&self) -> PathBuf {
        self.dir.join(FRAME_LOG)
    }
    /// "exit code 0", "stopped", "crashed (2 reports)", "no end recorded"
    pub fn outcome(&self) -> String {
        if !self.crashes.is_empty() {
            return format!("crash report{} ({})", if self.crashes.len() == 1 { "" } else { "s" }, self.crashes.len());
        }
        match &self.info {
            Some(Info { ended: Some(_), stopped_by_launcher: true, .. }) => "stopped".into(),
            Some(Info { ended: Some(_), exit_code: Some(c), .. }) => format!("exit code {c}"),
            Some(Info { ended: Some(_), exit: Some(e), .. }) => e.clone(),
            _ => "no end recorded".into(),
        }
    }
}

fn dir_size(d: &Path, depth: u32) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        match e.metadata() {
            Ok(m) if m.is_dir() && depth < 3 => n += dir_size(&e.path(), depth + 1),
            Ok(m) => n += m.len(),
            Err(_) => {}
        }
    }
    n
}

fn read_entry(dir: &Path, name: &str, started: u64) -> Entry {
    let mut crashes: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
            n.starts_with("crash-") && n.ends_with(".txt")
        })
        .collect();
    crashes.sort();
    let mut captures: Vec<PathBuf> = std::fs::read_dir(dir.join(CAPTURES)).into_iter().flatten().flatten().map(|e| e.path()).collect();
    captures.sort();
    Entry {
        dir: dir.to_path_buf(),
        name: name.to_string(),
        started,
        info: Info::read(dir),
        summary: Summary::read(&dir.join(SUMMARY_TOML)),
        crashes,
        captures,
        bytes: dir_size(dir, 0),
    }
}

/// Every session folder, newest first. Anything under `sessions/` that isn't one is ignored.
pub fn list(sessions: &Path) -> Vec<Entry> {
    let mut v: Vec<Entry> = std::fs::read_dir(sessions)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let started = parse_stamp(&name)?;
            Some(read_entry(&e.path(), &name, started))
        })
        .collect();
    v.sort_by(|a, b| b.started.cmp(&a.started).then(b.name.cmp(&a.name)));
    v
}

fn is_session_child(sessions: &Path, dir: &Path) -> bool {
    let name_ok = dir.file_name().and_then(|n| n.to_str()).and_then(parse_stamp).is_some();
    let parent_ok = match (dir.parent().map(std::path::absolute), std::path::absolute(sessions)) {
        (Some(Ok(p)), Ok(s)) => p == s,
        _ => false,
    };
    name_ok && parent_ok
}

/// Delete one session folder. Refused unless it is a session folder directly under `sessions`.
pub fn delete(sessions: &Path, dir: &Path) -> Result<(), String> {
    if !is_session_child(sessions, dir) {
        return Err(format!("{} is not a session folder under {}", dir.display(), sessions.display()));
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Delete session folders that started more than `retention_days` days before `now`. 0 (or less) keeps
/// everything. Folders in `keep` (a running session) are never deleted. Returns what was deleted.
pub fn prune(sessions: &Path, retention_days: i64, now: u64, keep: &[PathBuf]) -> Vec<PathBuf> {
    if retention_days <= 0 {
        return Vec::new();
    }
    let cutoff = now.saturating_sub(retention_days as u64 * 86_400);
    let mut gone = Vec::new();
    for e in std::fs::read_dir(sessions).into_iter().flatten().flatten() {
        let path = e.path();
        let Some(started) = e.file_name().to_str().and_then(parse_stamp) else { continue };
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) || started >= cutoff || keep.iter().any(|k| k == &path) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            gone.push(path);
        }
    }
    gone.sort();
    gone
}

/// The "Capture frame" request: the layer captures into `captures/` and deletes the file.
pub fn request_capture(dir: &Path, now: u64) -> Result<PathBuf, String> {
    let p = dir.join(CAPTURE_REQUEST);
    crate::write_atomic(&p, &format!("requested = \"{}\"\n", rfc3339(now))).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

/// A crash report with a header a bug report needs (launcher version, profile, limits, settings that
/// differ from the default), for the clipboard.
pub fn bug_report(entry: &Entry, crash: &Path) -> String {
    let body = std::fs::read_to_string(crash).unwrap_or_else(|e| format!("({}: {e})", crash.display()));
    let mut s = format!("Triumph Rebuild crash report\nsession  {}\n", entry.name);
    if let Some(i) = &entry.info {
        s += &format!("launcher {}\nprofile  {}\nlimits   {}\nexit     {}\n", i.launcher_version, i.profile_label, if i.limits.is_empty() { "none".into() } else { i.limits.join(", ") }, entry.outcome());
        let changed: Vec<String> = i
            .settings
            .iter()
            .filter(|(k, v)| rebuild_config::find(k).is_some_and(|r| r.default.value().to_string() != *v) && !k.ends_with("dir") && !k.contains("path") && !k.ends_with("dll"))
            .map(|(k, v)| format!("{k} = {v}"))
            .collect();
        s += &format!("settings {}\n", if changed.is_empty() { "all defaults".into() } else { changed.join(", ") });
    }
    s += &format!("file     {}\n\n{body}", crash.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn stamps_are_utc_and_round_trip() {
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z", "a leap day");
        // 2026-11-27 15:30:12 UTC
        let t = days_from_civil(2026, 11, 27) as u64 * DAY + 15 * 3600 + 30 * 60 + 12;
        assert_eq!(stamp(t), "20261127T153012Z");
        assert_eq!(rfc3339(t), "2026-11-27T15:30:12Z");
        assert_eq!(parse_stamp("20261127T153012Z-series-x"), Some(t));
        assert_eq!(parse_stamp("20261127T153012Z"), Some(t));
        for s in [0, 1, 59, 3600, DAY - 1, DAY, 4_102_444_800, t] {
            assert_eq!(parse_stamp(&stamp(s)), Some(s), "{s}");
        }
        for bad in ["", "mods", "20261127T153012", "20261327T153012Z-x", "2026112XT153012Z-x", "20261127T153012Zx"] {
            assert_eq!(parse_stamp(bad), None, "{bad}");
        }
        assert_eq!(folder_part("series-x"), "series-x");
        assert_eq!(folder_part("1440p quality"), "1440p-quality");
        assert_eq!(folder_part("../../x"), "x");
        assert_eq!(folder_part("   "), "default");
    }

    #[test]
    fn session_toml_is_written_at_launch_and_on_exit_and_parses() {
        let home = crate::temp_dir("session");
        let sessions = sessions_dir(&home);
        let mut c = Config::parse("").unwrap();
        rebuild_config::builtin("stress-ram").unwrap().apply(&mut c).unwrap();
        c.set("display.vsync", Value::Bool(true)).unwrap();
        let exe = home.join("rebuild.exe");
        let cfg = home.join("rebuild.toml");
        let start = Start { profile: &c.profile, config: &c, exe: &exe, config_path: &cfg, matrix: Some("m1"), run_seconds: Some(60), measure_from: Some("after_loading") };
        let t0 = 1_795_000_000;
        let dir = create(&sessions, &start, t0).unwrap();
        assert_eq!(dir.file_name().unwrap().to_str().unwrap(), format!("{}-stress-ram", stamp(t0)));
        let second = create(&sessions, &start, t0).unwrap();
        assert!(second.to_str().unwrap().ends_with("-stress-ram-2"), "a second launch in the same second gets its own folder");

        let i = Info::read(&dir).unwrap();
        assert_eq!(i.profile, "stress-ram");
        assert!(i.profile_label.contains("memory-starved"));
        assert_eq!(i.started, Some(t0));
        assert_eq!(i.ended, None);
        assert_eq!(i.launcher_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(i.limits, vec!["RAM 8 GiB".to_string()]);
        assert_eq!(i.matrix.as_deref(), Some("m1"));
        assert_eq!(i.run_seconds, Some(60));
        assert_eq!(i.measure_from.as_deref(), Some("after_loading"));
        assert_eq!(i.settings.len(), REGISTRY.len(), "every effective setting is recorded");
        assert_eq!(i.setting("limits.ram_mb"), Some("8192"));
        assert_eq!(i.setting("display.vsync"), Some("true"));
        assert_eq!(i.setting("upscaler.backend"), Some("auto"));

        finish(&dir, &End { code: Some(3), status: "exit code: 3".into(), stopped_by_launcher: false }, t0 + 75).unwrap();
        let i = Info::read(&dir).unwrap();
        assert_eq!(i.ended, Some(t0 + 75));
        assert_eq!(i.seconds, Some(75));
        assert_eq!(i.exit_code, Some(3));
        assert_eq!(i.settings.len(), REGISTRY.len(), "the start record is kept");
        let e = list(&sessions);
        assert_eq!(e.len(), 2);
        let ours = e.iter().find(|x| x.dir == dir).unwrap();
        assert_eq!(ours.outcome(), "exit code 3");
        assert_eq!(ours.profile(), "stress-ram");
    }

    #[test]
    fn retention_deletes_old_sessions_keeps_recent_ones_and_0_keeps_all() {
        let home = crate::temp_dir("prune");
        let sessions = sessions_dir(&home);
        let now = 1_795_000_000;
        let mk = |age_days: u64, tag: &str| {
            let d = sessions.join(format!("{}-{tag}", stamp(now - age_days * DAY)));
            std::fs::create_dir_all(d.join(CAPTURES)).unwrap();
            std::fs::write(d.join(FRAME_LOG), b"fps 30.0\n").unwrap();
            d
        };
        let old = mk(30, "old");
        let edge = mk(15, "edge");
        let recent = mk(2, "recent");
        let today = mk(0, "today");
        let running = mk(40, "running");
        // not session folders: never touched, however old they look
        let other = sessions.join("keep-me");
        std::fs::create_dir_all(&other).unwrap();
        let file = sessions.join("19700101T000000Z-a-file");
        std::fs::write(&file, b"x").unwrap();

        assert!(prune(&sessions, 0, now, &[]).is_empty(), "0 = keep forever");
        assert_eq!(list(&sessions).len(), 5);

        let gone = prune(&sessions, 14, now, &[running.clone()]);
        assert_eq!(gone, {
            let mut g = vec![old.clone(), edge.clone()];
            g.sort();
            g
        });
        assert!(!old.exists() && !edge.exists());
        assert!(recent.exists() && today.exists());
        assert!(running.exists(), "a running session is never pruned");
        assert!(other.exists() && file.exists(), "only session folders are pruned");

        let names: Vec<String> = list(&sessions).iter().map(|e| e.name.clone()).collect();
        assert_eq!(names.len(), 3);
        assert!(names[0].ends_with("today") && names[1].ends_with("recent") && names[2].ends_with("running"), "newest first: {names:?}");
    }

    #[test]
    fn delete_only_removes_session_folders() {
        let home = crate::temp_dir("delete");
        let sessions = sessions_dir(&home);
        let d = sessions.join("20261127T000000Z-x");
        std::fs::create_dir_all(&d).unwrap();
        assert!(delete(&sessions, &home).is_err());
        assert!(delete(&sessions, &sessions.join("mods")).is_err());
        assert!(delete(&home, &d).is_err(), "not directly under the sessions folder given");
        delete(&sessions, &d).unwrap();
        assert!(!d.exists());
    }

    #[test]
    fn crashes_captures_and_the_capture_request() {
        let home = crate::temp_dir("crash");
        let sessions = sessions_dir(&home);
        let c = Config::parse("").unwrap();
        let p = home.join("x");
        let dir = create(&sessions, &Start { profile: "default", config: &c, exe: &p, config_path: &p, matrix: None, run_seconds: None, measure_from: None }, 1_795_000_000).unwrap();
        std::fs::write(dir.join("crash-0001.txt"), "access violation at 0x1234\n").unwrap();
        std::fs::create_dir_all(dir.join(CAPTURES)).unwrap();
        std::fs::write(dir.join(CAPTURES).join("frame-1.png"), b"png").unwrap();
        let req = request_capture(&dir, 1_795_000_010).unwrap();
        assert!(req.exists());
        let e = &list(&sessions)[0];
        assert_eq!(e.crashes.len(), 1);
        assert_eq!(e.captures.len(), 1);
        assert!(e.outcome().starts_with("crash report"));
        let report = bug_report(e, &e.crashes[0]);
        assert!(report.contains("access violation") && report.contains("all defaults") && report.contains(env!("CARGO_PKG_VERSION")));
    }
}
