//! Build 5: the console's frame pacing calls on the device (docs/research/d3d12x-device.md, 0x270-0x288), on a
//! simple clock until Build 8 paces for real, and the user's frame-rate setting over it.
//!
//! - 0x270 `SetFrameIntervalX(output, interval µs (0 = 1/60 s), period, flags)`: the title sets 33.333 ms.
//! - 0x278 `ScheduleFrameEventX(kind, offset, *list, flags)`: counted only (no event is scheduled).
//! - 0x280 `WaitFrameEventX(kind, timeout ms, *list, flags, *token)`: waits for the next frame origin on the
//!   clock; `S_FALSE` on timeout, with the token written when flag 2 asks for it.
//! - 0x288 `GetFrameStatisticsX(token, kinds, *count, *out)`: no statistics yet (count 0).
//!
//! **The frame limit** ([`FrameLimit`], `display.frame_rate` / `display.frame_cap`; `REBUILD_FRAME_RATE` at
//! start, [`set_frame_limit`] live):
//!
//! - `title`: origins every interval the title asked for (30 fps for this title).
//! - `Cap(n)`: origins every 1/n s instead; the title's interval is ignored.
//! - `unlimited`: no clock. A wait returns at once with a new token; it only blocks (up to its timeout) while
//!   the present's frame slot is still on the GPU ([`crate::present::wait_slot`]), so the title's thread is never
//!   more than the present's three slots ahead of the GPU.
//!
//! The clock is anchored: a change of interval or mode re-anchors it at the last origin passed, so tokens never go
//! back. Within one clock frame the title's origin poll and its flip wait get the same token, as before.
//!
//! **Vsync** (`display.vsync`; `REBUILD_VSYNC=0|1`, default 0): read by the present on every frame.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

const S_FALSE: i32 = 1;

// ------------------------------------------------------------------------------------- the setting

/// How the title's frames are paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLimit {
    /// the title's own interval (`SetFrameIntervalX`)
    Title,
    /// at most this many frames a second
    Cap(u32),
    /// as fast as the PC can go
    Unlimited,
}

/// The highest cap accepted; above it a cap is the same as unlimited for any display.
pub const MAX_CAP: u32 = 1000;

impl FrameLimit {
    /// `title`, `unlimited` / `uncapped` / `0`, or a number = a cap in fps (clamped to 1..=[`MAX_CAP`]).
    pub fn parse(s: &str) -> Option<FrameLimit> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "title" | "" => Some(FrameLimit::Title),
            "unlimited" | "uncapped" | "0" => Some(FrameLimit::Unlimited),
            n => n.trim_end_matches("fps").trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0).map(|v| FrameLimit::Cap((v.round() as u32).clamp(1, MAX_CAP))),
        }
    }

    fn encode(self) -> u32 {
        match self {
            FrameLimit::Title => 0,
            FrameLimit::Cap(n) => n.clamp(1, MAX_CAP),
            FrameLimit::Unlimited => u32::MAX,
        }
    }

    fn decode(v: u32) -> FrameLimit {
        match v {
            0 => FrameLimit::Title,
            u32::MAX => FrameLimit::Unlimited,
            n => FrameLimit::Cap(n),
        }
    }
}

impl std::fmt::Display for FrameLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameLimit::Title => write!(f, "title"),
            FrameLimit::Cap(n) => write!(f, "cap {n}"),
            FrameLimit::Unlimited => write!(f, "unlimited"),
        }
    }
}

static LIMIT: AtomicU32 = AtomicU32::new(0);
static VSYNC: AtomicBool = AtomicBool::new(false);
static FROM_ENV: Once = Once::new();

/// `REBUILD_FRAME_RATE` and `REBUILD_VSYNC`, once, before the first read or write of either.
fn from_env() {
    FROM_ENV.call_once(|| {
        if let Ok(v) = std::env::var("REBUILD_FRAME_RATE") {
            match FrameLimit::parse(&v) {
                Some(l) => LIMIT.store(l.encode(), Ordering::Relaxed),
                None => eprintln!("frame     REBUILD_FRAME_RATE={v:?} is not title, unlimited or a number: title pacing"),
            }
        }
        if let Ok(v) = std::env::var("REBUILD_VSYNC") {
            VSYNC.store(matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"), Ordering::Relaxed);
        }
    });
}

pub fn frame_limit() -> FrameLimit {
    from_env();
    FrameLimit::decode(LIMIT.load(Ordering::Relaxed))
}

/// Change the pacing at once (the in-game menu).
pub fn set_frame_limit(l: FrameLimit) {
    from_env();
    LIMIT.store(l.encode(), Ordering::Relaxed);
}

pub fn vsync() -> bool {
    from_env();
    VSYNC.load(Ordering::Relaxed)
}

/// Vsync on or off from the next present (the in-game menu).
pub fn set_vsync(on: bool) {
    from_env();
    VSYNC.store(on, Ordering::Relaxed);
}

/// One line for the run report and the frame line's header.
pub fn describe() -> String {
    let limit = match frame_limit() {
        FrameLimit::Title => format!("title ({:.1} fps, the title's interval)", 1e6 / INTERVAL_US.load(Ordering::Relaxed) as f64),
        FrameLimit::Cap(n) => format!("cap {n} fps"),
        FrameLimit::Unlimited => "unlimited (bounded by the present's 3 frame slots on the GPU)".into(),
    };
    let vsync = if vsync() {
        "on".to_string()
    } else {
        match crate::present::tearing() {
            Some(true) => "off (tearing allowed)".into(),
            Some(false) => "off (no tearing support: the compositor may still wait)".into(),
            None => "off".into(),
        }
    };
    format!("frame rate {limit} | vsync {vsync}")
}

/// The pacing state is process-wide: the tests that change it (here and in `overlay`) run one at a time.
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

// ------------------------------------------------------------------------------------- the clock

/// The interval in µs the title asked for (default 1/60 s until it sets one).
static INTERVAL_US: AtomicU64 = AtomicU64::new(16_667);

/// The interval the title asked for.
pub fn interval() -> Duration {
    Duration::from_micros(INTERVAL_US.load(Ordering::Relaxed))
}

/// The step the clock runs at under `l`: `None` when unlimited.
fn step(l: FrameLimit) -> Option<Duration> {
    match l {
        FrameLimit::Title => Some(interval()),
        FrameLimit::Cap(n) => Some(Duration::from_nanos(1_000_000_000 / u64::from(n.max(1)))),
        FrameLimit::Unlimited => None,
    }
}

/// The clock: origins every `step` after `anchor`; the origin at `anchor` has token `token`.
struct Clock {
    anchor: Instant,
    token: u64,
    step: Option<Duration>,
}

/// Started by the first wait.
static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);

/// The next origin after `now` at `step` (`None` = unlimited): its token and when it falls. Re-anchors the clock
/// when the step changed, at the last origin passed under the old step, so tokens never go back.
fn next_origin(now: Instant, step: Option<Duration>) -> (u64, Instant) {
    let mut g = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert(Clock { anchor: now, token: 0, step });
    if c.step != step {
        if let Some(old) = c.step {
            let passed = now.saturating_duration_since(c.anchor).as_nanos() / old.as_nanos().max(1);
            c.anchor += Duration::from_nanos((passed * old.as_nanos()) as u64);
            c.token += passed as u64;
        }
        c.step = step;
    }
    match step {
        None => {
            // unlimited: every call is a new origin, now
            c.token += 1;
            c.anchor = now;
            (c.token, now)
        }
        Some(s) => {
            let s = s.as_nanos().max(1);
            let k = now.saturating_duration_since(c.anchor).as_nanos() / s + 1;
            (c.token + k as u64, c.anchor + Duration::from_nanos((k * s) as u64))
        }
    }
}

/// Sleep until `at`: the OS sleep to within half a millisecond, then yield to the mark.
fn sleep_until(at: Instant) {
    const SPIN: Duration = Duration::from_micros(500);
    let now = Instant::now();
    if at <= now {
        return;
    }
    if at - now > SPIN {
        std::thread::sleep(at - now - SPIN);
    }
    while Instant::now() < at {
        std::thread::yield_now();
    }
}

pub static CALLS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub unsafe extern "system" fn SetFrameIntervalX(_this: *mut c_void, _output: *mut c_void, interval_us: u32, _period: u32, _flags: u32) -> i32 {
    CALLS[0].fetch_add(1, Ordering::Relaxed);
    let us = if interval_us == 0 { 16_667 } else { u64::from(interval_us) };
    INTERVAL_US.store(us, Ordering::Relaxed);
    0
}

pub unsafe extern "system" fn ScheduleFrameEventX(_this: *mut c_void, _kind: u32, _offset: u32, _list: *const c_void, _flags: u32) -> i32 {
    CALLS[1].fetch_add(1, Ordering::Relaxed);
    0
}

pub unsafe extern "system" fn WaitFrameEventX(
    _this: *mut c_void,
    _kind: u32,
    timeout_ms: u32,
    _list: *const c_void,
    flags: u32,
    token: *mut u64,
) -> i32 {
    CALLS[2].fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let _waiting = Waiting(now);
    let mode = frame_limit();
    let (n, at) = next_origin(now, step(mode));
    let limit = now + Duration::from_millis(u64::from(timeout_ms));
    let write = |always: bool| {
        if (always || flags & 2 != 0) && !token.is_null() {
            unsafe { *token = n };
        }
    };
    if at > limit {
        sleep_until(limit);
        write(false);
        return S_FALSE;
    }
    sleep_until(at);
    // past the title's own pacing: keep the title within the present's slots of the GPU
    if mode != FrameLimit::Title && !crate::present::wait_slot(limit) {
        write(false);
        return S_FALSE;
    }
    write(true);
    0
}

/// Time the title spends in [`WaitFrameEventX`], for the frame line's `wait`.
struct Waiting(Instant);
impl Drop for Waiting {
    fn drop(&mut self) {
        crate::framelog::waited(self.0.elapsed());
    }
}

pub unsafe extern "system" fn GetFrameStatisticsX(_this: *mut c_void, _token: u64, _kinds: u32, count: *mut u32, _out: *mut c_void) -> i32 {
    CALLS[3].fetch_add(1, Ordering::Relaxed);
    if !count.is_null() {
        unsafe { *count = 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::TEST_SERIAL as SERIAL;

    fn wait(timeout_ms: u32, flags: u32, t: &mut u64) -> i32 {
        unsafe { WaitFrameEventX(std::ptr::null_mut(), 0, timeout_ms, std::ptr::null(), flags, t) }
    }

    #[test]
    fn the_setting_parses() {
        assert_eq!(FrameLimit::parse("title"), Some(FrameLimit::Title));
        assert_eq!(FrameLimit::parse(" Unlimited "), Some(FrameLimit::Unlimited));
        assert_eq!(FrameLimit::parse("uncapped"), Some(FrameLimit::Unlimited));
        assert_eq!(FrameLimit::parse("0"), Some(FrameLimit::Unlimited));
        assert_eq!(FrameLimit::parse("144"), Some(FrameLimit::Cap(144)));
        assert_eq!(FrameLimit::parse("59.94"), Some(FrameLimit::Cap(60)));
        assert_eq!(FrameLimit::parse("60fps"), Some(FrameLimit::Cap(60)));
        assert_eq!(FrameLimit::parse("100000"), Some(FrameLimit::Cap(MAX_CAP)));
        assert_eq!(FrameLimit::parse("fast"), None);
        assert_eq!(FrameLimit::parse("-30"), None);
        for l in [FrameLimit::Title, FrameLimit::Cap(1), FrameLimit::Cap(240), FrameLimit::Cap(MAX_CAP), FrameLimit::Unlimited] {
            assert_eq!(FrameLimit::decode(l.encode()), l);
        }
    }

    #[test]
    fn origins_fall_on_the_interval_and_a_short_timeout_reports_false() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_frame_limit(FrameLimit::Title);
        unsafe { SetFrameIntervalX(std::ptr::null_mut(), std::ptr::null_mut(), 20_000, 0, 0) };
        let mut t = 0u64;
        assert_eq!(wait(1000, 0, &mut t), 0);
        let first = t;
        let start = Instant::now();
        assert_eq!(wait(1000, 0, &mut t), 0);
        assert_eq!(t, first + 1, "the next origin");
        // one interval (20 ms) plus scheduler slack: the machine may be busy (helper lanes compiling)
        assert!(start.elapsed() <= Duration::from_millis(100));
        // 0 ms timeout: not reached yet, token only when asked
        let mut t2 = 0u64;
        assert_eq!(wait(0, 2, &mut t2), S_FALSE);
        assert!(t2 > first);
    }

    #[test]
    fn a_cap_paces_at_its_rate_and_unlimited_does_not_wait_and_tokens_only_go_up() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { SetFrameIntervalX(std::ptr::null_mut(), std::ptr::null_mut(), 33_333, 0, 0) };
        let mut last = 0u64;
        let mut t = 0u64;
        let mut seen = |t: u64, what: &str| {
            assert!(t >= last, "{what}: token {t} after {last}");
            last = t;
        };

        // cap 100: 10 ms frames, not the title's 33 ms
        set_frame_limit(FrameLimit::Cap(100));
        assert_eq!(wait(1000, 0, &mut t), 0);
        seen(t, "cap first");
        let start = Instant::now();
        for _ in 0..10 {
            assert_eq!(wait(1000, 0, &mut t), 0);
            seen(t, "cap");
        }
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(95), "10 frames at 100 fps took {took:?}");
        // 10 frames of the title's interval would be 333 ms; allow slack for a busy machine
        assert!(took <= Duration::from_millis(250), "10 frames at 100 fps took {took:?}");

        // unlimited: no sleep, a new token each call
        set_frame_limit(FrameLimit::Unlimited);
        let start = Instant::now();
        let mut prev = None;
        for _ in 0..1000 {
            assert_eq!(wait(1000, 0, &mut t), 0);
            seen(t, "unlimited");
            if let Some(p) = prev {
                assert!(t > p, "unlimited tokens are new each call");
            }
            prev = Some(t);
        }
        assert!(start.elapsed() < Duration::from_millis(100), "1000 unlimited waits took {:?}", start.elapsed());
        // a 0 ms poll succeeds at once too
        assert_eq!(wait(0, 2, &mut t), 0);
        seen(t, "unlimited poll");

        // back to a clock: tokens continue upward
        set_frame_limit(FrameLimit::Cap(200));
        assert_eq!(wait(1000, 0, &mut t), 0);
        seen(t, "cap after unlimited");
        set_frame_limit(FrameLimit::Title);
        let mut p = 0u64;
        assert_eq!(wait(0, 2, &mut p), S_FALSE, "a 0 ms poll on the title's clock times out");
        seen(p, "title poll");
        assert_eq!(wait(1000, 0, &mut t), 0);
        seen(t, "title");
        assert_eq!(t, p, "the poll and the wait name the same origin");
        set_frame_limit(FrameLimit::Title);
    }
}
