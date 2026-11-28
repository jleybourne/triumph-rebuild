//! XAccessibility's speech synthesis (`XSpeechSynthesizer*`), for the title's narration: no voice is
//! installed, and every stream is a valid WAVE file with no samples, so narration is silent and the
//! title goes on. (A host voice would need the platform's speech engine behind these streams.)
//!
//! The slots are the title's: its 10 thunks (`QueryApiImpl(3e241977…, d722b373…)` then one slot each;
//! image `0x1455dce7c`-`0x1455df0e8`) call 0x40-0x80 and 0xa8, with the public functions' argument
//! counts in order: enumerate voices (context, callback), create (*handle), close (handle), default
//! voice (handle), custom voice (handle, id), stream from text (handle, text, *stream), close stream
//! (stream), stream size (stream, *size), stream data (stream, size, buffer, *used), and stream from
//! SSML (handle, ssml, *stream) at 0xa8; one caller chooses between 0x68 and 0xa8. Slots 0x18-0x38 and
//! 0x88-0xa0 (closed captions, high contrast, speech to text by the header's other functions) are not
//! called and not guessed: discovery.

use std::ffi::{c_char, c_void};

use super::{slot, This};
use crate::guid::Guid;
use crate::hr::*;
use crate::object::{handle_of, put, text, InterfaceDef};

pub const PROVIDER: Guid = Guid::parse("3e241977-6237-41e9-8559-102c2d9983f1");
pub const IID: Guid = Guid::parse("d722b373-8c4d-4692-9e51-d6ad9b37aa7d");

type Handle = *const c_void;

/// Synthesizers and streams hold nothing: fixed handles, free to close.
static SYNTHESIZER: u8 = 0;
static STREAM: u8 = 0;

/// A WAVE file of 16-bit mono PCM at 16 kHz and no samples (44 bytes).
pub fn silence() -> Vec<u8> {
    let (rate, channels, bits) = (16_000u32, 1u16, 16u16);
    let block = channels * bits / 8;
    let mut w = Vec::with_capacity(44);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&36u32.to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    w.extend_from_slice(&block.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&0u32.to_le_bytes());
    w
}

/// `XSpeechSynthesizerEnumerateInstalledVoices(context, callback)`: none installed; the callback never runs.
unsafe extern "system" fn enumerate_voices(_: This, _context: *mut c_void, _callback: *const c_void) -> HRESULT {
    S_OK
}
/// `XSpeechSynthesizerCreate(*synthesizer)`.
unsafe extern "system" fn create(_: This, out: *mut Handle) -> HRESULT {
    unsafe { put(out, handle_of(&SYNTHESIZER)) }
}
/// `XSpeechSynthesizerCloseHandle` / `XSpeechSynthesizerCloseStreamHandle`.
unsafe extern "system" fn close(_: This, _: Handle) -> HRESULT {
    S_OK
}
/// `XSpeechSynthesizerSetDefaultVoice(synthesizer)`.
unsafe extern "system" fn set_default_voice(_: This, _: Handle) -> HRESULT {
    S_OK
}
/// `XSpeechSynthesizerSetCustomVoice(synthesizer, voiceId)`: accepted; every voice is silent.
unsafe extern "system" fn set_custom_voice(_: This, _: Handle, _voice: *const c_char) -> HRESULT {
    S_OK
}
/// `XSpeechSynthesizerCreateStreamFromText` / `...FromSsml(synthesizer, text, *stream)`.
unsafe extern "system" fn create_stream(_: This, synthesizer: Handle, words: *const c_char, out: *mut Handle) -> HRESULT {
    if synthesizer.is_null() || unsafe { text(words) }.is_none() {
        return E_INVALIDARG;
    }
    unsafe { put(out, handle_of(&STREAM)) }
}
/// `XSpeechSynthesizerGetStreamDataSize(stream, *size)`.
unsafe extern "system" fn stream_size(_: This, _: Handle, out: *mut usize) -> HRESULT {
    unsafe { put(out, silence().len()) }
}
/// `XSpeechSynthesizerGetStreamData(stream, bufferSize, buffer, *bufferUsed)`.
unsafe extern "system" fn stream_data(_: This, _: Handle, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let wav = silence();
    if buffer.is_null() || size < wav.len() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    unsafe { buffer.copy_from_nonoverlapping(wav.as_ptr(), wav.len()) };
    if !used.is_null() {
        unsafe { used.write_unaligned(wav.len()) };
    }
    S_OK
}

pub static ACCESSIBILITY_IFACE: InterfaceDef = InterfaceDef {
    area: "accessibility",
    provider: PROVIDER,
    iids: &[IID],
    len: 22,
    slots: &[
        slot!(0x40, "XSpeechSynthesizerEnumerateInstalledVoices", enumerate_voices, "context, callback -> HRESULT"),
        slot!(0x48, "XSpeechSynthesizerCreate", create, "*synthesizer -> HRESULT"),
        slot!(0x50, "XSpeechSynthesizerCloseHandle", close, "synthesizer -> HRESULT"),
        slot!(0x58, "XSpeechSynthesizerSetDefaultVoice", set_default_voice, "synthesizer -> HRESULT"),
        slot!(0x60, "XSpeechSynthesizerSetCustomVoice", set_custom_voice, "synthesizer, voiceId: char* -> HRESULT"),
        slot!(0x68, "XSpeechSynthesizerCreateStreamFromText", create_stream, "synthesizer, text: char*, *stream -> HRESULT"),
        slot!(0x70, "XSpeechSynthesizerCloseStreamHandle", close, "stream -> HRESULT"),
        slot!(0x78, "XSpeechSynthesizerGetStreamDataSize", stream_size, "stream, *size -> HRESULT"),
        slot!(0x80, "XSpeechSynthesizerGetStreamData", stream_data, "stream, bufferSize, buffer, *bufferUsed -> HRESULT"),
        slot!(0xa8, "XSpeechSynthesizerCreateStreamFromSsml", create_stream, "synthesizer, ssml: char*, *stream -> HRESULT"),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::areas::test::{method, this};

    type Create = unsafe extern "system" fn(This, *mut Handle) -> HRESULT;
    type One = unsafe extern "system" fn(This, Handle) -> HRESULT;
    type Voices = unsafe extern "system" fn(This, *mut c_void, *const c_void) -> HRESULT;
    type Custom = unsafe extern "system" fn(This, Handle, *const c_char) -> HRESULT;
    type Stream = unsafe extern "system" fn(This, Handle, *const c_char, *mut Handle) -> HRESULT;
    type Size = unsafe extern "system" fn(This, Handle, *mut usize) -> HRESULT;
    type Data = unsafe extern "system" fn(This, Handle, usize, *mut u8, *mut usize) -> HRESULT;

    fn m<F: Copy>(offset: usize) -> F {
        method(&ACCESSIBILITY_IFACE, offset)
    }

    #[test]
    fn narration_is_silent_wave_audio() {
        let t = this(&ACCESSIBILITY_IFACE);
        assert_eq!(unsafe { m::<Voices>(0x40)(t, std::ptr::null_mut(), std::ptr::null()) }, S_OK);
        let mut synth = std::ptr::null();
        assert_eq!(unsafe { m::<Create>(0x48)(t, &mut synth) }, S_OK);
        assert!(!synth.is_null());
        assert_eq!(unsafe { m::<Create>(0x48)(t, std::ptr::null_mut()) }, E_INVALIDARG);
        assert_eq!(unsafe { m::<One>(0x58)(t, synth) }, S_OK);
        assert_eq!(unsafe { m::<Custom>(0x60)(t, synth, c"voice".as_ptr()) }, S_OK);
        for offset in [0x68, 0xa8] {
            let mut stream = std::ptr::null();
            assert_eq!(unsafe { m::<Stream>(offset)(t, synth, c"Hello".as_ptr(), &mut stream) }, S_OK);
            assert_eq!(unsafe { m::<Stream>(offset)(t, synth, std::ptr::null(), &mut stream) }, E_INVALIDARG);
            let mut size = 0;
            assert_eq!(unsafe { m::<Size>(0x78)(t, stream, &mut size) }, S_OK);
            assert_eq!(size, 44);
            let (mut wav, mut used) = (vec![0u8; size], 0);
            assert_eq!(unsafe { m::<Data>(0x80)(t, stream, size - 1, wav.as_mut_ptr(), &mut used) }, E_NOT_SUFFICIENT_BUFFER);
            assert_eq!(unsafe { m::<Data>(0x80)(t, stream, size, wav.as_mut_ptr(), &mut used) }, S_OK);
            assert_eq!(used, 44);
            assert_eq!((&wav[..4], &wav[8..16], &wav[36..40]), (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..]));
            assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
            assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 0, "no samples");
            assert_eq!(unsafe { m::<One>(0x70)(t, stream) }, S_OK);
        }
        assert_eq!(unsafe { m::<One>(0x50)(t, synth) }, S_OK);
    }

    /// Closed captions, high contrast and speech to text are not ours to guess.
    #[test]
    fn other_slots_are_discovered() {
        let t = this(&ACCESSIBILITY_IFACE);
        let f: unsafe extern "system" fn(This) -> HRESULT = method(&ACCESSIBILITY_IFACE, 0x18);
        assert_eq!(unsafe { f(t) }, E_NOTIMPL);
        assert!(crate::discovery::contains("slot not implemented: accessibility 0x18"));
    }
}
