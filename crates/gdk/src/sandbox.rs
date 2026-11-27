//! The offline online-mode sandbox switch, as the GDK runtime sees it.
//!
//! Default (off): the runtime behaves as a genuinely offline retail console — the connectivity hint is
//! `OFFLINE`, and `XUserGetTokenAndSignatureAsync` fails, because a token can only come from the live
//! service. That is the shipped behaviour.
//!
//! On (`REBUILD_OFFLINE_SANDBOX`, a debug flag): the runtime reports an online-capable console and hands
//! the sign-in code a locally generated placeholder token, so the title proceeds past its connectivity /
//! sign-in gate into its online init and content-loading paths. **This is only safe because the runtime
//! layer hard-blocks every non-loopback network connection in code** (`rebuild_runtime::sandbox`): the
//! placeholder token is never presented to any real service, and nothing leaves the machine. It does not
//! produce a working multiplayer session — there is nothing real to connect to — it exercises the online
//! code offline for debugging.
//!
//! A thread-local override lets tests drive both paths deterministically without touching the process
//! environment; the title's own threads never set it, so at run time the flag alone decides.

use std::cell::Cell;

thread_local! {
    static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Whether the offline online-mode sandbox is on: the thread-local test override if set, else the
/// `REBUILD_OFFLINE_SANDBOX` environment flag.
pub fn online() -> bool {
    if let Some(v) = OVERRIDE.with(Cell::get) {
        return v;
    }
    std::env::var_os("REBUILD_OFFLINE_SANDBOX").is_some()
}

/// Run `f` with the sandbox forced on or off on this thread (tests only); the previous state is restored.
#[cfg(test)]
pub fn with_override<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let prev = OVERRIDE.with(|o| o.replace(Some(on)));
    let r = f();
    OVERRIDE.with(|o| o.set(prev));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_overridable_on_this_thread() {
        // No override and (in the test harness) no env var: off.
        assert!(!online() || std::env::var_os("REBUILD_OFFLINE_SANDBOX").is_some());
        with_override(true, || assert!(online()));
        with_override(false, || assert!(!online()));
        // the override is gone again
        assert_eq!(OVERRIDE.with(Cell::get), None);
    }
}
