use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

use super::*;
use crate::awc;
use crate::xma::{Stream, PACKET_BYTES};

/// Every frame of a stream, walked from its first packet, decoded to 16-bit.
fn decode_stream(data: &[u8], channels: usize, rate: u32) -> (Vec<i16>, usize, usize) {
    let mut d = Xma2::new(channels, rate);
    let s = Stream::new(data);
    let (mut pcm, mut frames, mut bad) = (Vec::new(), 0, 0);
    let mut at = Some(0);
    while let Some(f) = at.and_then(|p| s.frame(p)) {
        frames += 1;
        bad += usize::from(!d.decode(&f.data, f.bits, &mut pcm));
        at = f.next;
    }
    (pcm, frames, bad)
}

#[test]
fn a5_garbage_never_panics_and_keeps_timing() {
    let mut d = Decoder::new(2, 48_000);
    let mut seed = 0x1234_5678u32;
    let mut out = Vec::new();
    for round in 0..2000 {
        let len = 4 + (round * 37) % 600;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        out.clear();
        let _ = d.decode_f32(&bytes, len * 8, &mut out);
        assert_eq!(out.len(), FRAME * 2);
        assert!(out.iter().all(|v| v.is_finite()));
    }
}

#[test]
fn a5_band_layout_covers_every_block() {
    for rate in [24_000, 32_000, 44_100, 48_000] {
        let d = Decoder::new(1, rate);
        for i in 0..SIZES {
            let n = d.num_sfb[i];
            assert!(n > 0 && n < MAX_BANDS);
            assert_eq!(d.sfb[i][0], 0);
            assert_eq!(d.sfb[i][n], FRAME >> i, "rate {rate} size {i}");
            assert!(d.sfb[i][..=n].windows(2).all(|w| w[0] < w[1]));
        }
    }
}

#[test]
fn a5_container_finds_streams() {
    // Two streams, the title's layout: flags 1, ids, n + 1 words, then per stream
    // (other, data, format, other) chunks.
    let n = 2usize;
    let header = 16 + 2 * n + 4 * n + 4 * (n + 1) + 8 * 4 * n;
    let mut b = vec![0u8; header + 4096];
    b[..4].copy_from_slice(b"ADAT");
    b[4..8].copy_from_slice(&0xFF01_0001u32.to_le_bytes());
    b[8..12].copy_from_slice(&(n as u32).to_le_bytes());
    b[12..16].copy_from_slice(&(header as u32).to_le_bytes());
    let mut p = 16 + 2 * n + 4 * n + 4 * (n + 1);
    let mut put = |tag: u8, size: usize, at: usize, p: &mut usize| {
        let v = (u64::from(tag) << 56) | ((size as u64) << 28) | at as u64;
        b[*p..*p + 8].copy_from_slice(&v.to_le_bytes());
        *p += 8;
    };
    for s in 0..n {
        let base = header + s * 2048;
        put(0x36, 4, base, &mut p);
        put(awc::TAG_DATA, 1024, base + 64, &mut p);
        put(awc::TAG_FORMAT, 32, base + 8, &mut p);
        put(0xA3, 8, base + 40, &mut p);
    }
    for s in 0..n {
        let f = header + s * 2048 + 8;
        b[f..f + 4].copy_from_slice(&(1000u32 + s as u32).to_le_bytes());
        b[f + 8..f + 10].copy_from_slice(&32_000u16.to_le_bytes());
        b[f + 28] = awc::CODEC_XMA2;
        b[f + 29] = 1;
    }
    let got = awc::streams(&b);
    assert_eq!(got.len(), 2);
    assert_eq!(got[1], awc::Stream { samples: 1001, rate: 32_000, codec: 5, channels: 1, data: header + 2048 + 64..header + 2048 + 64 + 1024 });
}

// Against the title's own streams (hand-run; nothing from them is kept).

struct Found {
    at: u64,
    channels: usize,
    rate: u32,
    samples: u32,
    data: Vec<u8>,
}

/// Up to `want` XMA2 streams from the title's audio archive, read from its end backwards (that is
/// where they sit). Returns them and the archive's (size, modified) before the read.
fn title_streams(want: usize) -> Option<(PathBuf, Vec<Found>)> {
    let dir = std::env::var_os("REBUILD_GAME_DIR")?;
    let path = PathBuf::from(dir).join("audio.rpf");
    let mut f = std::fs::File::open(&path).ok()?;
    let size = f.metadata().ok()?.len();
    let window = 64u64 << 20;
    let mut found = Vec::new();
    let mut end = size;
    let mut buf = vec![0u8; window as usize + 16];
    while end > 0 && found.len() < want {
        let start = end.saturating_sub(window);
        let len = (end - start) as usize;
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_exact(&mut buf[..len]).ok()?;
        let mut hits = Vec::new();
        let mut i = 0;
        while let Some(k) = buf[i..len].windows(4).position(|w| w == b"ADAT") {
            hits.push(start + (i + k) as u64);
            i += k + 4;
        }
        for at in hits.into_iter().rev() {
            let mut head = vec![0u8; 1 << 20];
            f.seek(SeekFrom::Start(at)).ok()?;
            let n = f.read(&mut head).ok()?;
            head.truncate(n);
            for s in awc::streams(&head) {
                if s.codec != awc::CODEC_XMA2 || s.data.len() < PACKET_BYTES || found.len() >= want {
                    continue;
                }
                let mut data = vec![0u8; s.data.len()];
                f.seek(SeekFrom::Start(at + s.data.start as u64)).ok()?;
                f.read_exact(&mut data).ok()?;
                found.push(Found { at, channels: usize::from(s.channels), rate: s.rate, samples: s.samples, data });
            }
        }
        end = start;
    }
    Some((path, found))
}

fn ffmpeg() -> PathBuf {
    std::env::var_os("REBUILD_FFMPEG").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// FFmpeg's decode of the packets, wrapped in a RIFF WAVE with an XMA2 format (0x166), as 16-bit.
fn reference(s: &Found, tmp: &std::path::Path) -> Option<Vec<i16>> {
    let mut fmt = Vec::new();
    let ch = s.channels as u16;
    for v in [0x166u16, ch] {
        fmt.extend(v.to_le_bytes());
    }
    for v in [s.rate, s.rate * u32::from(ch) * 2] {
        fmt.extend(v.to_le_bytes());
    }
    for v in [2048u16, 16, 34, 1] {
        fmt.extend(v.to_le_bytes());
    }
    for v in [if ch == 1 { 4 } else { 3 }, s.samples, s.data.len() as u32, 0, s.samples, 0, 0] {
        fmt.extend(v.to_le_bytes());
    }
    fmt.extend([0u8, 4]);
    fmt.extend(1u16.to_le_bytes());
    let mut riff = b"WAVEfmt ".to_vec();
    riff.extend((fmt.len() as u32).to_le_bytes());
    riff.extend(&fmt);
    riff.extend(b"data");
    riff.extend((s.data.len() as u32).to_le_bytes());
    riff.extend(&s.data);
    let mut file = b"RIFF".to_vec();
    file.extend((riff.len() as u32).to_le_bytes());
    file.extend(riff);
    let wav = tmp.join("stream.wav");
    std::fs::write(&wav, file).ok()?;
    let out = std::process::Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&wav)
        .args(["-f", "s16le", "-"])
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&wav);
    Some(out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

/// A5's closing test: our decode equals FFmpeg's within one step of 16-bit, on the title's streams.
/// `REBUILD_GAME_DIR=<title> cargo test --release -p rebuild-audio -- --ignored a5_matches --nocapture`
/// (FFmpeg from `REBUILD_FFMPEG` or the path; `REBUILD_A5_STREAMS=<n>` streams, 64 by default).
#[test]
#[ignore]
fn a5_matches_ffmpeg_within_1_lsb() {
    let want = std::env::var("REBUILD_A5_STREAMS").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
    let Some((path, streams)) = title_streams(want) else {
        eprintln!("REBUILD_GAME_DIR not set: skipped");
        return;
    };
    let meta = |p: &PathBuf| std::fs::metadata(p).map(|m| (m.len(), m.modified().ok())).unwrap();
    let before = meta(&path);
    assert!(!streams.is_empty(), "no XMA2 streams found");
    let tmp = std::env::temp_dir().join("rebuild-a5");
    std::fs::create_dir_all(&tmp).unwrap();
    let (mut compared, mut off_by_one, mut worst, mut frames, mut bad, mut delays) = (0usize, 0usize, 0i32, 0, 0, Vec::new());
    for s in &streams {
        let (ours, f, b) = decode_stream(&s.data, s.channels, s.rate);
        frames += f;
        bad += b;
        let theirs = reference(s, &tmp).expect("ffmpeg ran");
        assert!(!theirs.is_empty(), "ffmpeg decoded the stream at {:#x}", s.at);
        // FFmpeg drops the first frame; find the offset rather than assume it.
        let ch = s.channels;
        let (delay, diff) = (0..=4 * FRAME)
            .filter(|d| d * ch < ours.len())
            .map(|d| {
                let n = theirs.len().min(ours.len() - d * ch);
                let m = (0..n).map(|i| (i32::from(ours[d * ch + i]) - i32::from(theirs[i])).abs()).max().unwrap_or(0);
                (d, m)
            })
            .min_by_key(|&(_, m)| m)
            .unwrap();
        let n = theirs.len().min(ours.len() - delay * ch);
        compared += n;
        off_by_one += (0..n).filter(|&i| ours[delay * ch + i] != theirs[i]).count();
        worst = worst.max(diff);
        delays.push(delay);
        if diff > 1 {
            eprintln!("stream at {:#x}: {} ch, {} Hz, max diff {diff} at delay {delay}", s.at, ch, s.rate);
        }
    }
    let _ = std::fs::remove_dir(&tmp);
    delays.sort();
    delays.dedup();
    eprintln!(
        "A5: {} streams, {frames} frames ({bad} refused), {compared} samples compared, {off_by_one} differ by 1, worst {worst}; delays {delays:?}",
        streams.len()
    );
    assert_eq!(meta(&path), before, "archive untouched");
    assert_eq!(bad, 0);
    assert!(worst <= 1);
}

/// A5's speed: real-time factor on one thread. The title's XMA2 streams are mono, so a stereo stream
/// is measured as its two channels' worth of the same frames (two decoders, one thread).
#[test]
#[ignore]
fn a5_speed() {
    let Some((_, streams)) = title_streams(64) else {
        eprintln!("REBUILD_GAME_DIR not set: skipped");
        return;
    };
    let frames: Vec<(Vec<u8>, usize, u32)> = streams
        .iter()
        .flat_map(|s| {
            let st = Stream::new(&s.data);
            let mut v = Vec::new();
            let mut at = Some(0);
            while let Some(f) = at.and_then(|p| st.frame(p)) {
                v.push((f.data, f.bits, s.rate));
                at = f.next;
            }
            v
        })
        .collect();
    let rate = streams[0].rate;
    for _ in 0..2 {
        let mut a = Decoder::new(1, rate);
        let mut b = Decoder::new(1, rate);
        let mut out = Vec::with_capacity(FRAME * 2);
        let t = Instant::now();
        let rounds = 5;
        for _ in 0..rounds {
            for (data, bits, _) in &frames {
                out.clear();
                let _ = a.decode_f32(data, *bits, &mut out);
                let _ = b.decode_f32(data, *bits, &mut out);
            }
        }
        let secs = t.elapsed().as_secs_f64();
        let audio = (rounds * frames.len() * FRAME) as f64 / f64::from(rate);
        eprintln!(
            "A5 speed: {} frames x {rounds}, stereo-equivalent {:.1} s of {rate} Hz audio in {:.3} s: {:.0}x real time ({:.1} us per stereo frame)",
            frames.len(),
            audio,
            secs,
            audio / secs,
            secs * 1e6 / (rounds * frames.len()) as f64
        );
    }
}
