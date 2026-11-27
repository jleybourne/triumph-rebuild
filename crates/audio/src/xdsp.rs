//! A3: `chad.dll`, the console's audio DSP: `XDspConnectWithMaximumStreamLimit`, `XDspActivate`,
//! `XDspSubmitCommand`, `XDspDeactivate`, `XDspDisconnect`.
//!
//! A stream takes one block of float samples per command and runs a convolution (reverb, with an
//! impulse response given at activation) or another block transform. Activation hands back a status
//! block the title polls: the ticket of the last command completed and its result. The title connects
//! with 384 KiB of DSP memory, 30 s of impulse response, 128 streams, mono (fork run log).
//!
//! Commands complete when submitted. The transform is not computed: the impulse response arrives in
//! the hardware's own frequency-domain layout, which is not known, so the output block is silence
//! (reverb returns are dry). Zeroing one block costs well under a microsecond.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::{lock, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, HRESULT, S_OK};

pub const CONVOLUTION: u32 = 1;
/// Option bits that need a stereo stream; and one a convolution refuses (fork's observed checks).
const STEREO_OPTIONS: u32 = 2 | 8;
const NOT_FOR_CONVOLUTION: u32 = 4;

pub struct Dsp {
    max_streams: u32,
    active: Mutex<u32>,
}

/// What `XDspActivate` reads, 28 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct StreamDesc {
    pub kind: u32,
    pub options: u32,
    /// Samples per channel per block: 512 or 1024 (2048 too for transforms).
    pub frames: u32,
    pub channels: u32,
    pub ir_floats: u32,
    pub ir: *const f32,
}

/// The block the title polls, 16 bytes.
#[repr(C)]
pub struct Status {
    pub submitted: AtomicU32,
    pub completed: AtomicU32,
    pub reserved: AtomicU32,
    pub result: AtomicU32,
}

pub struct Stream {
    dsp: *mut Dsp,
    samples: usize,
    status: Box<Status>,
}

/// One block of work, 32 bytes.
#[repr(C)]
pub struct Command {
    pub gains: [f32; 2],
    pub input: *const f32,
    pub output: *mut f32,
    pub reserved: u64,
}

fn valid(d: &StreamDesc) -> bool {
    let (kind, options, frames, channels) = (d.kind, d.options, d.frames, d.channels);
    let shape = if kind == CONVOLUTION {
        matches!(channels, 1 | 2) && matches!(frames, 512 | 1024) && options & NOT_FOR_CONVOLUTION == 0
    } else {
        channels == 1 && matches!(frames, 512 | 1024 | 2048)
    };
    shape && (options & STEREO_OPTIONS == 0 || channels == 2)
}

/// `HRESULT XDspConnectWithMaximumStreamLimit(void* dspMemory, UINT32 dspMemoryBytes,
/// UINT32 maxImpulseSeconds, UINT32 maxStreams, UINT32 maxChannels, XDspHandle* out)`
///
/// # Safety
/// `out` null or writable.
pub unsafe extern "system" fn XDspConnectWithMaximumStreamLimit(
    base: *mut c_void,
    base_len: u32,
    ir_seconds: u32,
    max_streams: u32,
    max_channels: u32,
    out: *mut *mut Dsp,
) -> HRESULT {
    if base.is_null()
        || !(0x1_0000..=0x1F40_0000).contains(&base_len)
        || ir_seconds > 0x80
        || max_streams > 0x100
        || max_channels > 2
    {
        return E_INVALIDARG;
    }
    if out.is_null() {
        return E_POINTER;
    }
    out.write(Box::into_raw(Box::new(Dsp {
        max_streams,
        active: Mutex::new(0),
    })));
    S_OK
}

/// `HRESULT XDspDisconnect(XDspHandle dsp)`: after its streams.
///
/// # Safety
/// A handle from the connect call, once.
pub unsafe extern "system" fn XDspDisconnect(dsp: *mut Dsp) -> HRESULT {
    if dsp.is_null() {
        return E_INVALIDARG;
    }
    drop(Box::from_raw(dsp));
    S_OK
}

/// `HRESULT XDspActivate(XDspHandle dsp, const StreamDesc* desc, const Status** status,
/// XDspStream* stream)`
///
/// # Safety
/// Pointers null or valid for their types.
pub unsafe extern "system" fn XDspActivate(
    dsp: *mut Dsp,
    desc: *const StreamDesc,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> HRESULT {
    if status.is_null() || stream.is_null() {
        return E_POINTER;
    }
    if dsp.is_null() || desc.is_null() {
        return E_INVALIDARG;
    }
    let d = desc.read_unaligned();
    if !valid(&d) {
        return E_INVALIDARG;
    }
    {
        let mut active = lock(&(*dsp).active);
        if *active >= (*dsp).max_streams {
            return E_OUTOFMEMORY;
        }
        *active += 1;
    }
    let s = Box::new(Stream {
        dsp,
        samples: (d.frames * d.channels) as usize,
        status: Box::new(Status {
            submitted: AtomicU32::new(0),
            completed: AtomicU32::new(0),
            reserved: AtomicU32::new(0),
            result: AtomicU32::new(0),
        }),
    });
    status.write(&*s.status);
    stream.write(Box::into_raw(s));
    S_OK
}

/// `HRESULT XDspDeactivate(XDspStream stream)`
///
/// # Safety
/// A stream from `XDspActivate`, once.
pub unsafe extern "system" fn XDspDeactivate(stream: *mut Stream) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    let s = Box::from_raw(stream);
    *lock(&(*s.dsp).active) -= 1;
    S_OK
}

/// `HRESULT XDspSubmitCommand(XDspStream stream, const Command* command, UINT32* ticket /*optional*/)`:
/// complete at once; the output block is silence.
///
/// # Safety
/// A live stream; `command` valid, its output one block long; `ticket` null or writable.
pub unsafe extern "system" fn XDspSubmitCommand(
    stream: *mut Stream,
    command: *const Command,
    ticket: *mut u32,
) -> HRESULT {
    if stream.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    let s = &*stream;
    let c = &*command;
    if !c.output.is_null() {
        std::ptr::write_bytes(c.output, 0, s.samples);
    }
    let n = s.status.submitted.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    s.status.result.store(S_OK as u32, Ordering::Release);
    s.status.completed.store(n, Ordering::Release);
    if !ticket.is_null() {
        ticket.write(n);
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a3_dsp_blocks_complete_in_ticket_order() {
        assert_eq!(std::mem::size_of::<StreamDesc>(), 28);
        assert_eq!(std::mem::size_of::<Status>(), 16);
        assert_eq!(std::mem::size_of::<Command>(), 32);
        let mut base = vec![0u8; 393_216];
        let mut dsp = std::ptr::null_mut();
        // SAFETY: valid pointers throughout, as the title passes them.
        unsafe {
            assert_eq!(
                XDspConnectWithMaximumStreamLimit(base.as_mut_ptr().cast(), 393_216, 30, 2, 1, &mut dsp),
                S_OK
            );
            let ir = vec![0f32; 1024];
            let desc = StreamDesc { kind: CONVOLUTION, options: 0, frames: 1024, channels: 1, ir_floats: 1024, ir: ir.as_ptr() };
            let (mut status, mut a, mut b, mut c) = (std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut a), S_OK);
            assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut b), S_OK);
            assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut c), E_OUTOFMEMORY, "limit of 2");
            assert_eq!(XDspDeactivate(b), S_OK);
            assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut b), S_OK);
            let input = vec![0.5f32; 1024];
            let mut output = vec![1f32; 1024];
            for expect in 1..=100u32 {
                let cmd = Command { gains: [1.0, 1.0], input: input.as_ptr(), output: output.as_mut_ptr(), reserved: 0 };
                let mut ticket = 0;
                assert_eq!(XDspSubmitCommand(b, &cmd, &mut ticket), S_OK);
                assert_eq!(ticket, expect);
                assert_eq!((*status).completed.load(Ordering::Acquire), expect);
            }
            assert!(output.iter().all(|&s| s == 0.0));
            let stereo_on_mono = StreamDesc { options: 2, ..desc };
            assert_eq!(XDspActivate(dsp, &stereo_on_mono, &mut status, &mut c), E_INVALIDARG);
            assert_eq!(XDspDeactivate(a), S_OK);
            assert_eq!(XDspDeactivate(b), S_OK);
            assert_eq!(XDspDisconnect(dsp), S_OK);
        }
    }
}
