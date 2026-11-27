//! Image helpers for the upscaler tests: half floats, PNG out, and the numbers (PSNR, frame-to-frame
//! difference, NaN and black counts).

use crate::Size;

/// f32 → IEEE half, round to nearest even; NaN stays NaN, overflow goes to infinity.
pub fn f16(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let mut r = m >> shift;
        let rem = m & ((1 << shift) - 1);
        if rem > half || (rem == half && r & 1 == 1) {
            r += 1;
        }
        return sign | r as u16;
    }
    let mut r = ((e as u32) << 10) | (man >> 13);
    let rem = man & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && r & 1 == 1) {
        r += 1;
    }
    sign | r as u16
}

/// IEEE half → f32.
pub fn f32_of(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    match exp {
        0 => sign * man * 2f32.powi(-24),
        0x1f => {
            if man == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An 8-bit RGB PNG (stored deflate blocks: no compression, no dependency). Values clamp to [0, 1].
pub fn png(size: Size, rgba: &[[f32; 4]]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(size.pixels() * 3 + size.height as usize);
    for y in 0..size.height as usize {
        raw.push(0); // filter: none
        for x in 0..size.width as usize {
            let c = rgba[y * size.width as usize + x];
            for k in 0..3 {
                let v = if c[k].is_finite() { c[k].clamp(0.0, 1.0) } else { 0.0 };
                raw.push((v * 255.0 + 0.5) as u8);
            }
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.width.to_be_bytes());
    ihdr.extend_from_slice(&size.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// PSNR in dB over RGB, values in [0, 1] (clamped; NaN counts as the worst error).
pub fn psnr(a: &[[f32; 4]], b: &[[f32; 4]]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mut se = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        for k in 0..3 {
            let d = if x[k].is_finite() && y[k].is_finite() { x[k].clamp(0.0, 1.0) as f64 - y[k].clamp(0.0, 1.0) as f64 } else { 1.0 };
            se += d * d;
        }
    }
    let mse = se / (a.len() * 3) as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (1.0 / mse).log10()
}

/// Mean absolute difference over RGB, on the 0-255 scale.
pub fn mean_abs_diff(a: &[[f32; 4]], b: &[[f32; 4]]) -> f64 {
    let mut s = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        for k in 0..3 {
            s += (x[k].clamp(0.0, 1.0) - y[k].clamp(0.0, 1.0)).abs() as f64;
        }
    }
    s / (a.len() * 3) as f64 * 255.0
}

/// Pixels with a NaN or infinite channel, and pixels that are black (all of RGB below 1/255).
pub fn bad_pixels(a: &[[f32; 4]]) -> (usize, usize) {
    let nan = a.iter().filter(|c| c[..3].iter().any(|v| !v.is_finite())).count();
    let black = a.iter().filter(|c| c[..3].iter().all(|v| *v < 1.0 / 255.0)).count();
    (nan, black)
}

/// Bilinear resize (the baseline an upscaler has to beat), sampling at pixel centres.
pub fn bilinear(src: &[[f32; 4]], from: Size, to: Size) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity(to.pixels());
    let (sx, sy) = (from.width as f32 / to.width as f32, from.height as f32 / to.height as f32);
    let at = |x: i64, y: i64| src[(y.clamp(0, from.height as i64 - 1) * from.width as i64 + x.clamp(0, from.width as i64 - 1)) as usize];
    for y in 0..to.height {
        for x in 0..to.width {
            let fx = (x as f32 + 0.5) * sx - 0.5;
            let fy = (y as f32 + 0.5) * sy - 0.5;
            let (x0, y0) = (fx.floor(), fy.floor());
            let (tx, ty) = (fx - x0, fy - y0);
            let (x0, y0) = (x0 as i64, y0 as i64);
            let mut c = [0.0f32; 4];
            for k in 0..4 {
                let top = at(x0, y0)[k] * (1.0 - tx) + at(x0 + 1, y0)[k] * tx;
                let bot = at(x0, y0 + 1)[k] * (1.0 - tx) + at(x0 + 1, y0 + 1)[k] * tx;
                c[k] = top * (1.0 - ty) + bot * ty;
            }
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_round_trips_every_finite_half() {
        for h in 0u16..=0xffff {
            let v = f32_of(h);
            if v.is_nan() {
                assert!(f32_of(f16(v)).is_nan());
                continue;
            }
            assert_eq!(f16(v), h, "half {h:#06x} -> {v}");
        }
        assert_eq!(f16(1e9), 0x7c00);
        assert_eq!(f16(0.1), 0x2e66);
    }

    #[test]
    fn png_has_valid_crcs_and_adler() {
        let size = Size::new(3, 2);
        let px = vec![[1.0, 0.5, 0.0, 1.0]; 6];
        let p = png(size, &px);
        assert_eq!(&p[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // walk the chunks and check each CRC
        let mut i = 8;
        let mut kinds = Vec::new();
        while i < p.len() {
            let len = u32::from_be_bytes(p[i..i + 4].try_into().unwrap()) as usize;
            let crc = u32::from_be_bytes(p[i + 8 + len..i + 12 + len].try_into().unwrap());
            assert_eq!(crc32(&p[i + 4..i + 8 + len]), crc);
            kinds.push(String::from_utf8_lossy(&p[i + 4..i + 8]).to_string());
            i += 12 + len;
        }
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn metrics() {
        let a = vec![[0.5, 0.5, 0.5, 1.0]; 4];
        let mut b = a.clone();
        assert!(psnr(&a, &b).is_infinite());
        b[0] = [0.6, 0.5, 0.5, 1.0];
        let p = psnr(&a, &b);
        assert!((p - 10.0 * (1.0 / (0.01 / 12.0f64)).log10()).abs() < 1e-3, "{p}");
        b[1] = [f32::NAN, 0.0, 0.0, 1.0];
        b[2] = [0.0, 0.0, 0.0, 1.0];
        assert_eq!(bad_pixels(&b), (1, 1));
        let up = bilinear(&a, Size::new(2, 2), Size::new(5, 3));
        assert!(up.iter().all(|c| (c[0] - 0.5).abs() < 1e-6));
    }
}
