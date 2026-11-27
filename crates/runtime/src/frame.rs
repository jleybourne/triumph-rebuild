//! Build 5: the console's frame pacing calls on the device (docs/research/d3d12x-device.md, 0x270-0x288), on a
//! simple clock until Build 8 paces for real.
//!
//! - 0x270 `SetFrameIntervalX(output, interval µs (0 = 1/60 s), period, flags)`: the title sets 33.333 ms.
//! - 0x278 `ScheduleFrameEventX(kind, offset, *list, flags)`: recorded.
//! - 0x280 `WaitFrameEventX(kind, timeout ms, *list, flags, *token)`: waits for the next frame origin on the
//!   clock (origins every interval from the first wait); `S_FALSE` on timeout, with the token written when flag 2
//!   asks for it.
//! - 0x288 `GetFrameStatisticsX(token, kinds, *count, *out)`: no statistics yet (count 0).

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const S_FALSE: i32 = 1;

/// The interval in µs (default 1/60 s until the title sets one).
static INTERVAL_US: AtomicU64 = AtomicU64::new(16_667);
static ORIGIN: OnceLock<Instant> = OnceLock::new();

pub fn interval() -> Duration {
    Duration::from_micros(INTERVAL_US.load(Ordering::Relaxed))
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

/// The frame number (token) of the next origin after `now`, and when it falls.
fn next_origin(now: Instant) -> (u64, Instant) {
    let origin = *ORIGIN.get_or_init(Instant::now);
    let step = interval().as_nanos().max(1);
    let since = now.saturating_duration_since(origin).as_nanos();
    let n = since / step + 1;
    (n as u64, origin + Duration::from_nanos((n * step) as u64))
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
    let (n, at) = next_origin(now);
    let limit = now + Duration::from_millis(u64::from(timeout_ms));
    if at > limit {
        if timeout_ms > 0 {
            std::thread::sleep(limit - now);
        }
        if flags & 2 != 0 && !token.is_null() {
            unsafe { *token = n };
        }
        return S_FALSE;
    }
    std::thread::sleep(at.saturating_duration_since(Instant::now()));
    if !token.is_null() {
        unsafe { *token = n };
    }
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

    #[test]
    fn origins_fall_on_the_interval_and_a_short_timeout_reports_false() {
        unsafe { SetFrameIntervalX(std::ptr::null_mut(), std::ptr::null_mut(), 20_000, 0, 0) };
        let mut t = 0u64;
        assert_eq!(unsafe { WaitFrameEventX(std::ptr::null_mut(), 0, 1000, std::ptr::null(), 0, &mut t) }, 0);
        let first = t;
        let start = Instant::now();
        assert_eq!(unsafe { WaitFrameEventX(std::ptr::null_mut(), 0, 1000, std::ptr::null(), 0, &mut t) }, 0);
        assert_eq!(t, first + 1, "the next origin");
        assert!(start.elapsed() <= Duration::from_millis(30));
        // 0 ms timeout: not reached yet, token only when asked
        let mut t2 = 0u64;
        assert_eq!(unsafe { WaitFrameEventX(std::ptr::null_mut(), 0, 0, std::ptr::null(), 2, &mut t2) }, S_FALSE);
        assert!(t2 > first);
    }
}
