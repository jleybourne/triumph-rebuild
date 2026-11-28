//! WAV files and a plain-numbers look at what is in them: is it silence, noise or something that sounds like
//! audio, and does it drop out or click?
//!
//! Used to check the title's sound without listening to it: `hostout` taps what the title hands the host's audio
//! engine into a WAV, `loopback` records what the engine actually plays for the title's process, and
//! [`analyze`] turns either into numbers (per-second RMS and peak, spectral flatness, dropouts, clicks).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const WAVE_FORMAT_PCM: u16 = 1;
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The parts of a `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE`) a WAV needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// float samples (IEEE float, or the extensible format's float subtype)
    pub float: bool,
    /// the extensible format's channel mask (0: none given)
    pub mask: u32,
}

impl Format {
    pub fn block(&self) -> usize {
        self.channels as usize * self.bits as usize / 8
    }

    /// From a `WAVEFORMATEX` in memory (with its extension when `cbSize` says it has one).
    ///
    /// # Safety
    /// `p` points at a `WAVEFORMATEX`, followed by `cbSize` more bytes.
    pub unsafe fn from_waveformatex(p: *const u8) -> Option<Format> {
        if p.is_null() {
            return None;
        }
        let head = std::slice::from_raw_parts(p, 18);
        let tag = u16::from_le_bytes([head[0], head[1]]);
        let channels = u16::from_le_bytes([head[2], head[3]]);
        let rate = u32::from_le_bytes(head[4..8].try_into().unwrap());
        let bits = u16::from_le_bytes([head[14], head[15]]);
        let cb = u16::from_le_bytes([head[16], head[17]]);
        let (float, mask) = match tag {
            WAVE_FORMAT_IEEE_FLOAT => (true, 0),
            WAVE_FORMAT_PCM => (false, 0),
            WAVE_FORMAT_EXTENSIBLE if cb >= 22 => {
                let ext = std::slice::from_raw_parts(p.add(18), 22);
                let mask = u32::from_le_bytes(ext[2..6].try_into().unwrap());
                // the subtype GUID's first word is the format tag (KSDATAFORMAT_SUBTYPE_PCM = 1, _IEEE_FLOAT = 3)
                let sub = u32::from_le_bytes(ext[6..10].try_into().unwrap());
                match sub {
                    3 => (true, mask),
                    1 => (false, mask),
                    _ => return None,
                }
            }
            _ => return None,
        };
        (channels > 0 && rate > 0 && matches!(bits, 8 | 16 | 24 | 32)).then_some(Format { rate, channels, bits, float, mask })
    }
}

/// Writes a WAV as data arrives; the sizes are fixed up by [`Writer::finish`] (and on drop).
pub struct Writer {
    file: std::fs::File,
    pub format: Format,
    pub bytes: u64,
    done: bool,
}

impl Writer {
    pub fn create(path: &Path, format: Format) -> std::io::Result<Writer> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut file = std::fs::File::create(path)?;
        file.write_all(&header(format, 0))?;
        Ok(Writer { file, format, bytes: 0, done: false })
    }

    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.file.write_all(data)?;
        self.bytes += data.len() as u64;
        Ok(())
    }

    /// Silence: `frames` frames of zeros.
    pub fn silence(&mut self, frames: u64) -> std::io::Result<()> {
        let zero = vec![0u8; 64 * 1024];
        let mut left = frames * self.format.block() as u64;
        while left > 0 {
            let n = left.min(zero.len() as u64) as usize;
            self.write(&zero[..n])?;
            left -= n as u64;
        }
        Ok(())
    }

    pub fn frames(&self) -> u64 {
        self.bytes / self.format.block() as u64
    }

    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(self.format, self.bytes.min(u32::MAX as u64 - 80) as u32))?;
        self.file.flush()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// A 68-byte header: RIFF, a 40-byte extensible `fmt ` (any channel count and mask) and the `data` chunk head.
fn header(f: Format, data: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(68);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(60 + data).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&40u32.to_le_bytes());
    h.extend_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
    h.extend_from_slice(&f.channels.to_le_bytes());
    h.extend_from_slice(&f.rate.to_le_bytes());
    h.extend_from_slice(&(f.rate * f.block() as u32).to_le_bytes());
    h.extend_from_slice(&(f.block() as u16).to_le_bytes());
    h.extend_from_slice(&f.bits.to_le_bytes());
    h.extend_from_slice(&22u16.to_le_bytes());
    h.extend_from_slice(&f.bits.to_le_bytes()); // valid bits
    h.extend_from_slice(&f.mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT: {0000000x-0000-0010-8000-00aa00389b71}
    h.extend_from_slice(&(if f.float { 3u32 } else { 1u32 }).to_le_bytes());
    h.extend_from_slice(&[0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    h
}

/// A WAV read whole, as f32 frames (interleaved).
pub struct Audio {
    pub format: Format,
    pub samples: Vec<f32>,
}

impl Audio {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.format.channels as usize
    }
}

/// Reads a PCM (8/16/24/32-bit) or float WAV, plain or extensible. A `data` size of 0 or past the end (a writer
/// that never finished) reads to the end of the file.
pub fn read(path: &Path) -> std::io::Result<Audio> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    parse(&bytes).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "not a WAV this reads"))
}

pub fn parse(b: &[u8]) -> Option<Audio> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut format = None;
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let size = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        let body = at + 8;
        if id == b"fmt " {
            let mut fmt = b.get(body..body + size.max(18))?.to_vec();
            if size < 18 {
                fmt.resize(18, 0);
                fmt[16] = 0;
                fmt[17] = 0;
            }
            // SAFETY: `fmt` holds the header and the `cbSize` bytes it names (checked by `get` above)
            format = unsafe { Format::from_waveformatex(fmt.as_ptr()) };
        } else if id == b"data" {
            let f = format?;
            let end = if size == 0 || body + size > b.len() { b.len() } else { body + size };
            let data = &b[body..end];
            let n = data.len() / (f.bits as usize / 8);
            let mut samples = Vec::with_capacity(n);
            for i in 0..n {
                let s = &data[i * f.bits as usize / 8..];
                samples.push(match (f.float, f.bits) {
                    (true, 32) => f32::from_le_bytes(s[..4].try_into().unwrap()),
                    (false, 8) => (s[0] as f32 - 128.0) / 128.0,
                    (false, 16) => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                    (false, 24) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
                    (false, 32) => i32::from_le_bytes(s[..4].try_into().unwrap()) as f32 / 2_147_483_648.0,
                    _ => return None,
                });
            }
            samples.truncate(samples.len() / f.channels as usize * f.channels as usize);
            return Some(Audio { format: f, samples });
        }
        at = body + size + (size & 1);
    }
    None
}

// ------------------------------------------------------------------------------------------------- analysis

/// One second (or the last part) of a recording.
#[derive(Clone, Debug)]
pub struct Second {
    pub start: f64,
    /// RMS over every channel, dBFS (-inf for digital silence)
    pub rms_db: f64,
    pub peak_db: f64,
    /// median spectral flatness of the 2048-frame windows that are not silent (0 = tonal, ~0.56 = white noise)
    pub flatness: Option<f64>,
    pub class: Class,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// nothing above -80 dBFS
    Silent,
    /// broadband with no structure (flatness above 0.4)
    Noise,
    /// structured sound: music, speech, effects
    Audio,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub format: Format,
    pub seconds: Vec<Second>,
    /// stretches of all-channel exact zeros of at least 1 ms inside sound (the 50 ms around them above -50 dBFS):
    /// what an output that ran dry leaves
    pub dropouts: usize,
    /// the dropouts: (first frame, frames)
    pub dropout_runs: Vec<(usize, usize)>,
    /// sample-to-sample jumps above 0.25 and 30x the local step size, inside sound (not out of digital silence):
    /// what a torn buffer leaves
    pub clicks: usize,
    pub peak_db: f64,
    pub rms_db: f64,
}

fn db(x: f64) -> f64 {
    if x > 0.0 { 20.0 * x.log10() } else { f64::NEG_INFINITY }
}

impl Report {
    pub fn audio_seconds(&self) -> usize {
        self.seconds.iter().filter(|s| s.class == Class::Audio).count()
    }

    /// One line per second plus a summary, as `audiocap analyze` prints it.
    pub fn text(&self) -> String {
        let f = self.format;
        let mut s = format!(
            "format    {} Hz, {} channels, {}-bit {}{}\n",
            f.rate,
            f.channels,
            f.bits,
            if f.float { "float" } else { "PCM" },
            if f.mask != 0 { format!(", mask {:#x}", f.mask) } else { String::new() }
        );
        s += "  second    rms dBFS  peak dBFS  flatness  class\n";
        for x in &self.seconds {
            s += &format!(
                "  {:>6.0}  {:>9.1}  {:>9.1}  {:>8}  {:?}\n",
                x.start,
                x.rms_db.max(-199.0),
                x.peak_db.max(-199.0),
                x.flatness.map_or("-".to_string(), |v| format!("{v:.3}")),
                x.class
            );
        }
        let n = |c| self.seconds.iter().filter(|s| s.class == c).count();
        s += &format!(
            "summary   {:.1} s: {} s audio, {} s noise, {} s silent; peak {:.1} dBFS, rms {:.1} dBFS; {} dropouts, {} clicks\n",
            self.seconds.last().map_or(0.0, |x| x.start + 1.0).min(self.seconds.len() as f64),
            n(Class::Audio),
            n(Class::Noise),
            n(Class::Silent),
            self.peak_db.max(-199.0),
            self.rms_db.max(-199.0),
            self.dropouts,
            self.clicks
        );
        if !self.dropout_runs.is_empty() {
            let mut lens: Vec<usize> = self.dropout_runs.iter().map(|r| r.1).collect();
            lens.sort_unstable();
            let ms = |f: usize| f as f64 * 1000.0 / f64::from(self.format.rate);
            s += &format!(
                "dropouts  lengths: shortest {:.1} ms, median {:.1} ms, longest {:.1} ms; first at {}\n",
                ms(lens[0]),
                ms(lens[lens.len() / 2]),
                ms(lens[lens.len() - 1]),
                self.dropout_runs.iter().take(8).map(|r| format!("{:.3} s ({:.1} ms)", r.0 as f64 / f64::from(self.format.rate), ms(r.1))).collect::<Vec<_>>().join(", ")
            );
        }
        s
    }
}

pub fn analyze(a: &Audio) -> Report {
    let ch = a.format.channels as usize;
    let rate = a.format.rate as usize;
    let frames = a.frames();
    let mono: Vec<f32> = (0..frames).map(|i| a.samples[i * ch..i * ch + ch].iter().sum::<f32>() / ch as f32).collect();
    let mut seconds = Vec::new();
    let (mut total_sq, mut total_peak) = (0f64, 0f64);
    let mut fft = Fft::new(2048);
    for (k, start) in (0..frames).step_by(rate).enumerate() {
        let end = (start + rate).min(frames);
        let s = &a.samples[start * ch..end * ch];
        let sq: f64 = s.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        let peak = s.iter().fold(0f64, |m, &v| m.max(f64::from(v.abs())));
        total_sq += sq;
        total_peak = total_peak.max(peak);
        let rms = (sq / s.len().max(1) as f64).sqrt();
        let mut flat = Vec::new();
        let mut w = start;
        while w + 2048 <= end {
            if let Some(v) = fft.flatness(&mono[w..w + 2048]) {
                flat.push(v);
            }
            w += 2048;
        }
        flat.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let flatness = (!flat.is_empty()).then(|| flat[flat.len() / 2]);
        let class = if db(peak) < -80.0 {
            Class::Silent
        } else if flatness.is_some_and(|f| f > 0.4) {
            Class::Noise
        } else {
            Class::Audio
        };
        seconds.push(Second { start: k as f64, rms_db: db(rms), peak_db: db(peak), flatness, class });
    }
    let (dropout_runs, clicks) = glitches(&a.samples, ch, rate);
    let dropouts = dropout_runs.len();
    Report {
        format: a.format,
        seconds,
        dropouts,
        dropout_runs,
        clicks,
        peak_db: db(total_peak),
        rms_db: db((total_sq / a.samples.len().max(1) as f64).sqrt()),
    }
}

/// Dropouts and clicks (see [`Report`]).
fn glitches(s: &[f32], ch: usize, rate: usize) -> (Vec<(usize, usize)>, usize) {
    let frames = s.len() / ch;
    let ms = (rate / 1000).max(1);
    // the level around each frame: RMS over 50 ms blocks
    let block = 50 * ms;
    let level: Vec<f64> = (0..frames.div_ceil(block))
        .map(|b| {
            let x = &s[b * block * ch..((b + 1) * block).min(frames) * ch];
            (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len().max(1) as f64).sqrt()
        })
        .collect();
    // a zero run's own block is quieter because of the run, so look at its neighbours
    let loud = |b: usize| -> bool {
        let l = |i: usize| level.get(i).copied().unwrap_or(0.0);
        let before = if b == 0 { 0.0 } else { l(b - 1) };
        db(before) > -50.0 && db(l(b + 1)) > -50.0
    };
    let mut dropouts = Vec::new();
    let mut run = 0usize;
    for i in 0..frames {
        if s[i * ch..i * ch + ch].iter().all(|&v| v == 0.0) {
            run += 1;
        } else {
            if run >= ms && i > run && loud((i - run) / block) && loud(i / block) {
                dropouts.push((i - run, run));
            }
            run = 0;
        }
    }
    let mut clicks = 0;
    for c in 0..ch {
        // mean step size over a 10 ms neighbourhood (running), compared with each step
        let w = 10 * ms;
        let step = |i: usize| (s[i * ch + c] - s[(i - 1) * ch + c]).abs();
        // steps of exactly 0 in the window: digital silence. A sound starting out of silence (a video's first
        // frame, a dropout's end) is a step too, but not a torn buffer: only steps inside sound count.
        let mut sum = 0f32;
        let mut still = 0usize;
        let mut win = std::collections::VecDeque::with_capacity(w);
        for i in 1..frames {
            let d = step(i);
            if win.len() == w {
                let mean = sum / w as f32;
                if d > 0.25 && d > 30.0 * mean && still < w / 4 {
                    clicks += 1;
                }
                let gone = win.pop_front().unwrap();
                sum -= gone;
                still -= usize::from(gone == 0.0);
            }
            win.push_back(d);
            sum += d;
            still += usize::from(d == 0.0);
        }
    }
    (dropouts, clicks)
}

/// A radix-2 FFT, enough for spectral flatness.
struct Fft {
    n: usize,
    re: Vec<f64>,
    im: Vec<f64>,
    window: Vec<f64>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        let window = (0..n).map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()).collect();
        Fft { n, re: vec![0.0; n], im: vec![0.0; n], window }
    }

    /// Geometric over arithmetic mean of the power spectrum (bins 1..n/2), of a Hann-windowed block;
    /// `None` for a block with no energy.
    fn flatness(&mut self, x: &[f32]) -> Option<f64> {
        let n = self.n;
        for i in 0..n {
            self.re[i] = f64::from(x[i]) * self.window[i];
            self.im[i] = 0.0;
        }
        // bit reversal
        let mut j = 0;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                self.re.swap(i, j);
                self.im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let ang = -std::f64::consts::TAU / len as f64;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                    let (a, b) = (start + k, start + k + len / 2);
                    let (xr, xi) = (self.re[b] * wr - self.im[b] * wi, self.re[b] * wi + self.im[b] * wr);
                    self.re[b] = self.re[a] - xr;
                    self.im[b] = self.im[a] - xi;
                    self.re[a] += xr;
                    self.im[a] += xi;
                }
            }
            len <<= 1;
        }
        let p: Vec<f64> = (1..n / 2).map(|k| self.re[k] * self.re[k] + self.im[k] * self.im[k]).collect();
        let mean = p.iter().sum::<f64>() / p.len() as f64;
        if mean < 1e-14 {
            return None;
        }
        let geo = (p.iter().map(|&v| (v + 1e-20).ln()).sum::<f64>() / p.len() as f64).exp();
        Some(geo / mean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, secs: f64, amp: f32) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let t = i as f32 / rate as f32;
                // a chord, as music would have
                let v = amp * ((t * 440.0 * std::f32::consts::TAU).sin() + 0.5 * (t * 660.0 * std::f32::consts::TAU).sin()) / 1.5;
                [v, v]
            })
            .collect()
    }

    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut x = 0x1234_5678u32;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                amp * (x as f32 / u32::MAX as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    #[test]
    fn a_written_wav_reads_back_and_silence_noise_and_tone_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("rebuild-audio-wav-{}", std::process::id()));
        let path = dir.join("t.wav");
        let f = Format { rate: 48_000, channels: 2, bits: 32, float: true, mask: 3 };
        let mut w = Writer::create(&path, f).unwrap();
        w.silence(48_000).unwrap();
        let n = noise(96_000, 0.3);
        w.write(&n.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        let t = tone(48_000, 1.0, 0.3);
        w.write(&t.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        assert_eq!(w.frames(), 48_000 * 3);
        w.finish().unwrap();
        drop(w);
        let a = read(&path).unwrap();
        assert_eq!(a.format, f);
        assert_eq!(a.frames(), 48_000 * 3);
        let r = analyze(&a);
        let classes: Vec<Class> = r.seconds.iter().map(|s| s.class).collect();
        assert_eq!(classes, [Class::Silent, Class::Noise, Class::Audio], "{}", r.text());
        // the chord's peak is below its amplitude (the two sines never peak together): compare with the samples'
        let peak = f64::from(t.iter().fold(0f32, |m, v| m.max(v.abs())));
        assert!((r.seconds[2].peak_db - db(peak)).abs() < 0.1, "{} vs {}", r.seconds[2].peak_db, db(peak));
        assert_eq!((r.dropouts, r.clicks), (0, 0), "{}", r.text());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_title_format_is_read_from_its_waveformatex_plain_or_extensible() {
        // 7.1 float, extensible (what a console mixer asks for), as the header this writes it
        let f = Format { rate: 48_000, channels: 8, bits: 32, float: true, mask: 0x63f };
        let h = header(f, 0);
        // SAFETY: the 40-byte fmt chunk body starts at byte 20
        assert_eq!(unsafe { Format::from_waveformatex(h[20..].as_ptr()) }, Some(f));
        let pcm = Format { rate: 44_100, channels: 2, bits: 16, float: false, mask: 3 };
        assert_eq!(unsafe { Format::from_waveformatex(header(pcm, 0)[20..].as_ptr()) }, Some(pcm));
        // a plain WAVEFORMATEX (cbSize 0)
        let mut plain = [0u8; 18];
        plain[0] = 3;
        plain[2] = 2;
        plain[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        plain[14] = 32;
        assert_eq!(unsafe { Format::from_waveformatex(plain.as_ptr()) }, Some(Format { rate: 48_000, channels: 2, bits: 32, float: true, mask: 0 }));
        plain[0] = 0x66; // not PCM or float
        assert_eq!(unsafe { Format::from_waveformatex(plain.as_ptr()) }, None);
    }

    #[test]
    fn dropouts_and_clicks_in_sound_are_counted_and_plain_pcm16_reads() {
        let mut t = tone(48_000, 2.0, 0.4);
        // three 5 ms holes in the middle of the tone (an output running dry), one click (a torn buffer)
        for at in [30_000usize, 50_000, 70_000] {
            t[at * 2..(at + 240) * 2].fill(0.0);
        }
        t[2 * 60_000] = 0.95;
        t[2 * 60_000 + 1] = 0.95;
        let pcm: Vec<u8> = t.iter().flat_map(|v| ((v * 32767.0) as i16).to_le_bytes()).collect();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 2] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        for v in [4u16, 16] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        b.extend_from_slice(&pcm);
        let a = parse(&b).unwrap();
        assert_eq!(a.format, Format { rate: 48_000, channels: 2, bits: 16, float: false, mask: 0 });
        let r = analyze(&a);
        assert_eq!(r.dropouts, 3, "{}", r.text());
        assert!(r.clicks >= 2, "both channels' click: {}", r.text());
        assert_eq!(r.audio_seconds(), 2);
    }
}
