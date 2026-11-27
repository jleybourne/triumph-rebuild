//! What the title asked for that we do not answer: an unknown (provider, interface) pair, a slot with
//! no implementation, a call we refuse by design. Each distinct line once, in order. The lead's
//! discovery loop drains this alongside import and `GetProcAddress` misses.

use std::sync::Mutex;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log() -> std::sync::MutexGuard<'static, Vec<String>> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// Records `line` unless it is already there. Never blocks on anything but this list's lock.
pub fn note(line: String) {
    let mut log = log();
    if !log.contains(&line) {
        log.push(line);
    }
}

/// Everything noted so far.
pub fn lines() -> Vec<String> {
    log().clone()
}

pub fn contains(needle: &str) -> bool {
    log().iter().any(|l| l.contains(needle))
}
