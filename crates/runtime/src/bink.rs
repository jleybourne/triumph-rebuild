//! The title's calls into its Bink 2 video library (`Bink2Scarlett.dll`, one of its own DLLs): bound through ours.
//!
//! Bink decodes on the CPU into frame buffers the title gave it (`BinkRegisterFrameBuffers`), memory the GPU then
//! samples as textures. Those textures are kept current by `crate::cputex`, which needs to know when a frame has
//! landed: the frame calls here tell it. Every call is also counted, and `REBUILD_TRACE_BINK=1` prints the first
//! calls of each with their arguments and result (which video opened, whether frames decode).
//!
//! The wrappers forward four integer registers and the integer result: every function bound here takes at most four
//! integer or pointer arguments and returns an integer, a pointer or nothing (Bink 2's documented signatures).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One bound function: its real address and its calls.
pub struct Bound {
    pub name: &'static str,
    real: AtomicU64,
    pub calls: AtomicU64,
}

impl Bound {
    const fn new(name: &'static str) -> Bound {
        Bound { name, real: AtomicU64::new(0), calls: AtomicU64::new(0) }
    }
}

fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_TRACE_BINK").is_some())
}

/// What a call's result means for the frame buffers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// nothing written
    No,
    /// a frame is in the buffers once it returns
    Always,
    /// a frame is in the buffers when it returns non-zero (the async wait)
    IfNonZero,
}

macro_rules! bink {
    ($( $f:ident $name:literal $frame:ident ),* $(,)?) => {
        /// Every function bound through here.
        pub static BOUND: &[&Bound] = &[$( &$f::B ),*];

        $(
            #[allow(non_snake_case)]
            mod $f {
                pub static B: super::Bound = super::Bound::new($name);
                pub unsafe extern "system" fn call(a: u64, b: u64, c: u64, d: u64) -> u64 {
                    // SAFETY: the title DLL's own export, bound in place of the import; same arguments, same result
                    let f: unsafe extern "system" fn(u64, u64, u64, u64) -> u64 =
                        unsafe { std::mem::transmute(B.real.load(super::Relaxed) as usize) };
                    let r = unsafe { f(a, b, c, d) };
                    super::after(&B, super::Frame::$frame, [a, b, c, d], r);
                    r
                }
            }
        )*

        fn ours(name: &str) -> Option<(&'static Bound, usize)> {
            match name {
                $( $name => Some((&$f::B, $f::call as *const () as usize)), )*
                _ => None,
            }
        }
    };
}

bink! {
    open "BinkOpenWithOptions" No,
    close "BinkClose" No,
    do_frame "BinkDoFrame" Always,
    do_frame_async "BinkDoFrameAsyncMulti" No,
    do_frame_wait "BinkDoFrameAsyncWait" IfNonZero,
    buffers_info "BinkGetFrameBuffersInfo" No,
    register "BinkRegisterFrameBuffers" No,
    next "BinkNextFrame" No,
    wait "BinkWait" No,
    skip "BinkShouldSkip" No,
    goto "BinkGoto" Always,
    pause "BinkPause" No,
    will_loop "BinkSetWillLoop" No,
}

fn after(b: &Bound, frame: Frame, args: [u64; 4], r: u64) {
    let n = b.calls.fetch_add(1, Relaxed);
    let landed = match frame {
        Frame::No => false,
        Frame::Always => true,
        Frame::IfNonZero => r as u32 != 0,
    };
    if landed {
        crate::cputex::frame_decoded();
    }
    if trace() && (n < 8 || n.is_power_of_two()) {
        eprintln!("bink      {} #{} ({:#x}, {:#x}, {:#x}, {:#x}) -> {:#x}{}", b.name, n + 1, args[0], args[1], args[2], args[3], r, if landed { " (a frame)" } else { "" });
        // the frame buffer description the title hands over (BINKFRAMEBUFFERS: sizes, then per buffer set the Y, cR,
        // cB and A planes as (allocate, buffer, pitch)): where the decoder will write
        if b.name == "BinkRegisterFrameBuffers" && args[1] != 0 {
            // SAFETY: the title's own structure, read while the call it was passed to has just returned
            let w = unsafe { std::slice::from_raw_parts(args[1] as *const u64, 48) };
            eprintln!("bink        frame buffers {:x?}", w);
        }
    }
}

/// The binder found `dll!name` among the title's own exports at `real`: ours to bind instead, for Bink's calls.
/// `REBUILD_BINK=host` binds the title's own (no frame marks, no counts).
pub fn wrap_title_export(dll: &str, name: &str, real: u64) -> Option<u64> {
    if !dll.eq_ignore_ascii_case("bink2scarlett.dll") || std::env::var("REBUILD_BINK").is_ok_and(|v| v.trim() == "host") {
        return None;
    }
    let (b, ours) = ours(name)?;
    b.real.store(real, Relaxed);
    Some(ours as u64)
}

/// Calls per bound function (those called), for the run report.
pub fn report() -> String {
    let called: Vec<String> = BOUND.iter().filter(|b| b.calls.load(Relaxed) > 0).map(|b| format!("{} {}", b.name, b.calls.load(Relaxed))).collect();
    if called.is_empty() {
        "no Bink calls".into()
    } else {
        format!("Bink calls: {}", called.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SEEN: AtomicU64 = AtomicU64::new(0);
    unsafe extern "system" fn fake_wait(a: u64, b: u64) -> u64 {
        SEEN.store(a * 10 + b, Relaxed);
        u64::from(b == 1)
    }

    #[test]
    fn a_bound_call_reaches_the_real_function_with_its_arguments_and_counts_frames() {
        assert!(wrap_title_export("kernel32.dll", "BinkDoFrameAsyncWait", 1).is_none());
        assert!(wrap_title_export("Bink2Scarlett.dll", "BinkSetVolume", 1).is_none(), "not bound through here");
        let ours = wrap_title_export("Bink2Scarlett.dll", "BinkDoFrameAsyncWait", fake_wait as *const () as u64).unwrap();
        let f: unsafe extern "system" fn(u64, u64) -> u64 = unsafe { std::mem::transmute(ours as usize) };
        let before = crate::cputex::FRAMES.load(Relaxed);
        assert_eq!(unsafe { f(7, 0) }, 0);
        assert_eq!(SEEN.load(Relaxed), 70);
        assert_eq!(crate::cputex::FRAMES.load(Relaxed), before, "a wait that timed out: no frame");
        assert_eq!(unsafe { f(7, 1) }, 1);
        assert_eq!(SEEN.load(Relaxed), 71);
        assert!(crate::cputex::FRAMES.load(Relaxed) > before, "a finished wait: a frame");
        assert!(report().contains("BinkDoFrameAsyncWait 2"));
    }
}
