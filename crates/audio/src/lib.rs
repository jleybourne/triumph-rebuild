//! The console's audio hardware, on the host (Build 3 piece, lane `audio`).
//!
//! The title imports 19 functions from three console-only DLLs. Each is here as a function with the
//! console's argument layout (documented on the function); `crates/runtime` binds them.
//!
//! - `heap` (A2, `acphal.dll` `Apu*`): the APU heap. One reserved host range; a device address is the
//!   offset into it plus a fixed base, so both address maps are arithmetic and exact.
//! - `acp` (`acphal.dll` `AcpHal*`): the SHAPE context arrays and the audio coprocessor object, whose
//!   commands complete when submitted and are acknowledged by message.
//! - `xma` (A3): XMA contexts. The packet and frame walk, the input buffers, loops, and the output ring
//!   are ours, on a decoder thread of their own; the frame decoder itself is a trait (see `agents/audio.md`).
//! - `xapu` (A3, `xapu.dll`): the Opus decoder's command queue. Commands are decoded on a worker
//!   thread; results come back in command order.
//! - `xdsp` (A3, `chad.dll`): the DSP's block streams.
//! - `wasapi` (A4): host output on a thread of its own, fed through a lock-free ring.
//!
//! No GPU. No title bytes: every test builds its own input.
#![cfg(windows)]
#![allow(non_snake_case)]

pub mod acp;
pub mod awc;
pub mod heap;
#[cfg(test)]
mod measure;
pub mod ring;
pub mod wasapi;
pub mod wma;
pub mod xapu;
pub mod xdsp;
pub mod xma;

/// The console's result type.
pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
pub const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
pub const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;

/// Windows `BOOL`. Returned where the console returns a truth value: a caller reading only `al`
/// (a C++ `bool`) and one reading `eax` (a `BOOL`) both see 0 or 1.
pub type BOOL = i32;

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Audio threads (Opus worker, XMA thread, output) run above normal priority, so a title thread
/// waiting on one of their short locks is not left waiting behind a preempted holder.
pub(crate) fn raise_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    // SAFETY: the calling thread's own pseudo-handle.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
}
