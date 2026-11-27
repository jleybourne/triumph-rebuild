//! The text atlas: one coverage byte a pixel, glyphs packed in shelves, baked once for the output's size.
//!
//! [`Font::bake`] (Windows) draws each glyph with GDI, grey-scale antialiased, at the theme's face and pixel size,
//! trims it to its ink, and packs it. Text is then drawn 1:1 (a glyph's quad covers exactly its atlas pixels), so it
//! is sharp at any output height: the overlay bakes again when the height changes. [`Font::synthetic`] is a
//! box font with the same metrics shape, for tests that must not depend on the host's fonts.

use std::sync::atomic::{AtomicU64, Ordering};

/// The characters baked: printable ASCII and a few the menu draws.
pub const EXTRA: [char; 8] = ['←', '→', '↑', '↓', '‹', '›', '•', '…'];

/// Atlas rows are this many bytes (a multiple of 4: the shader reads whole words).
pub const PITCH: u32 = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Glyph {
    /// top-left in the atlas
    pub x: u16,
    pub y: u16,
    /// ink size (0 for a space)
    pub w: u16,
    pub h: u16,
    /// ink offset from the pen position (left) and the line's top
    pub dx: i16,
    pub dy: i16,
    pub advance: u16,
}

#[derive(Clone, Debug)]
pub struct Font {
    /// unique per bake: the renderer uploads the atlas again when it changes
    pub id: u64,
    pub px: u32,
    /// line height and ascent, pixels
    pub line: u32,
    pub ascent: u32,
    /// coverage, `PITCH` bytes a row, `rows` rows (a multiple of 4 bytes in all)
    pub atlas: Vec<u8>,
    pub rows: u32,
    ascii: [Glyph; 95],
    extra: [Glyph; EXTRA.len()],
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Every character baked, in atlas order.
pub fn chars() -> impl Iterator<Item = char> {
    (' '..='~').chain(EXTRA)
}

impl Font {
    pub fn glyph(&self, c: char) -> Option<&Glyph> {
        match c {
            ' '..='~' => Some(&self.ascii[c as usize - 32]),
            _ => EXTRA.iter().position(|&e| e == c).map(|i| &self.extra[i]),
        }
    }

    /// The glyph for `c`, or `?`.
    pub fn glyph_or(&self, c: char) -> &Glyph {
        self.glyph(c).unwrap_or(&self.ascii['?' as usize - 32])
    }

    /// Width of `s` in pixels.
    pub fn measure(&self, s: &str) -> u32 {
        s.chars().map(|c| u32::from(self.glyph_or(c).advance)).sum()
    }

    /// `s` cut to fit `width`, with `…` when cut.
    pub fn fit(&self, s: &str, width: u32) -> String {
        if self.measure(s) <= width {
            return s.to_string();
        }
        let ell = self.measure("…");
        let mut out = String::new();
        let mut w = 0;
        for c in s.chars() {
            let a = u32::from(self.glyph_or(c).advance);
            if w + a + ell > width {
                break;
            }
            w += a;
            out.push(c);
        }
        out.push('…');
        out
    }

    /// `s` broken at spaces into lines no wider than `width` (at most `max` lines; the last cut with `…`).
    pub fn wrap(&self, s: &str, width: u32, max: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut cur = String::new();
        for word in s.split_whitespace() {
            let next = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if self.measure(&next) <= width || cur.is_empty() {
                cur = next;
            } else {
                lines.push(std::mem::take(&mut cur));
                cur = word.to_string();
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        if lines.len() > max {
            let rest = lines[max - 1..].join(" ");
            lines.truncate(max - 1);
            lines.push(rest);
        }
        lines.into_iter().map(|l| self.fit(&l, width)).collect()
    }

    /// Pack trimmed glyph bitmaps `(char, w, h, dx, dy, advance, coverage)` into shelves.
    fn pack(px: u32, line: u32, ascent: u32, glyphs: Vec<(char, u32, u32, i32, i32, u32, Vec<u8>)>) -> Font {
        let mut order: Vec<usize> = (0..glyphs.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(glyphs[i].2));
        let mut placed = vec![Glyph::default(); glyphs.len()];
        let (mut x, mut y, mut shelf) = (0u32, 0u32, 0u32);
        for &i in &order {
            let (_, w, h, dx, dy, adv, _) = &glyphs[i];
            if x + w + 1 > PITCH {
                x = 0;
                y += shelf + 1;
                shelf = 0;
            }
            placed[i] = Glyph { x: x as u16, y: y as u16, w: *w as u16, h: *h as u16, dx: *dx as i16, dy: *dy as i16, advance: *adv as u16 };
            if *w > 0 {
                x += w + 1;
                shelf = shelf.max(*h);
            }
        }
        let rows = (y + shelf + 1).max(1);
        let mut atlas = vec![0u8; (PITCH * rows) as usize];
        for (i, (_, w, h, _, _, _, cov)) in glyphs.iter().enumerate() {
            let g = placed[i];
            for r in 0..*h {
                let dst = ((u32::from(g.y) + r) * PITCH + u32::from(g.x)) as usize;
                atlas[dst..dst + *w as usize].copy_from_slice(&cov[(r * w) as usize..((r + 1) * w) as usize]);
            }
        }
        let mut ascii = [Glyph::default(); 95];
        let mut extra = [Glyph::default(); EXTRA.len()];
        for (i, g) in glyphs.iter().enumerate() {
            match g.0 {
                c @ ' '..='~' => ascii[c as usize - 32] = placed[i],
                c => extra[EXTRA.iter().position(|&e| e == c).expect("extra")] = placed[i],
            }
        }
        Font { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), px, line, ascent, atlas, rows, ascii, extra }
    }

    /// A box font: every visible glyph a solid block of its ink box. Deterministic, no host fonts.
    pub fn synthetic(px: u32) -> Font {
        let px = px.max(4);
        let line = px * 5 / 4;
        let ascent = px;
        let glyphs = chars()
            .map(|c| {
                let adv = px * 3 / 5;
                if c == ' ' {
                    return (c, 0, 0, 0, 0, adv, Vec::new());
                }
                let (w, h) = (adv.saturating_sub(2).max(1), px * 3 / 4);
                (c, w, h, 1, (line - h) as i32 / 2, adv, vec![255u8; (w * h) as usize])
            })
            .collect();
        Font::pack(px, line, ascent, glyphs)
    }

    /// Bake the theme's face at `px` with GDI (grey-scale antialiasing). `None` when GDI fails.
    #[cfg(windows)]
    pub fn bake(face: &str, weight: i32, px: u32) -> Option<Font> {
        gdi::bake(face, weight, px).map(|(line, ascent, glyphs)| Font::pack(px, line, ascent, glyphs))
    }
}

#[cfg(windows)]
mod gdi {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, SIZE};
    use windows::Win32::Graphics::Gdi::*;

    type Baked = (u32, u32, Vec<(char, u32, u32, i32, i32, u32, Vec<u8>)>);

    pub fn bake(face: &str, weight: i32, px: u32) -> Option<Baked> {
        let wide: Vec<u16> = face.encode_utf16().chain([0]).collect();
        // SAFETY: GDI objects created, used and deleted here; the DIB's bits are read after GdiFlush.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let font = CreateFontW(
                -(px as i32),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_TT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
                PCWSTR(wide.as_ptr()),
            );
            let result = (|| {
                if font.is_invalid() {
                    return None;
                }
                SelectObject(dc, font);
                let mut tm = TEXTMETRICW::default();
                if !GetTextMetricsW(dc, &mut tm).as_bool() {
                    return None;
                }
                let line = tm.tmHeight.max(1) as u32;
                let ascent = tm.tmAscent.max(0) as u32;
                // one cell, big enough for any glyph and its overhangs
                let pad = px as i32 / 2 + 2;
                let cw = (tm.tmMaxCharWidth.max(px as i32) + 2 * pad) as u32;
                let ch = (tm.tmHeight + 2 * pad) as u32;
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: cw as i32,
                        biHeight: -(ch as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
                let old = SelectObject(dc, dib);
                SetTextColor(dc, COLORREF(0x00FF_FFFF));
                SetBkColor(dc, COLORREF(0));
                SetBkMode(dc, OPAQUE);
                let mut out = Vec::new();
                let mut units = [0u16; 2];
                for c in super::chars() {
                    let s = c.encode_utf16(&mut units);
                    let mut size = SIZE::default();
                    let _ = GetTextExtentPoint32W(dc, s, &mut size);
                    // clear the cell, draw at (pad, pad)
                    std::ptr::write_bytes(bits as *mut u8, 0, (cw * ch * 4) as usize);
                    let _ = TextOutW(dc, pad, pad, s);
                    let _ = GdiFlush();
                    let px32 = std::slice::from_raw_parts(bits as *const u32, (cw * ch) as usize);
                    let cov = |x: u32, y: u32| ((px32[(y * cw + x) as usize] >> 8) & 0xFF) as u8;
                    let (mut x0, mut y0, mut x1, mut y1) = (cw, ch, 0, 0);
                    for y in 0..ch {
                        for x in 0..cw {
                            if cov(x, y) != 0 {
                                x0 = x0.min(x);
                                y0 = y0.min(y);
                                x1 = x1.max(x + 1);
                                y1 = y1.max(y + 1);
                            }
                        }
                    }
                    let adv = size.cx.max(0) as u32;
                    if x1 <= x0 {
                        out.push((c, 0, 0, 0, 0, adv, Vec::new()));
                        continue;
                    }
                    let (w, h) = (x1 - x0, y1 - y0);
                    let mut bitmap = Vec::with_capacity((w * h) as usize);
                    for y in y0..y1 {
                        for x in x0..x1 {
                            bitmap.push(cov(x, y));
                        }
                    }
                    out.push((c, w, h, x0 as i32 - pad, y0 as i32 - pad, adv, bitmap));
                }
                SelectObject(dc, old);
                let _ = DeleteObject(dib);
                Some((line, ascent, out))
            })();
            if !font.is_invalid() {
                let _ = DeleteObject(font);
            }
            let _ = DeleteDC(dc);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_font_measures_fits_and_wraps() {
        let f = Font::synthetic(20);
        assert_eq!(f.atlas.len() as u32, PITCH * f.rows);
        assert_eq!(f.measure("abc"), 3 * 12);
        assert_eq!(f.glyph(' ').unwrap().w, 0);
        assert!(f.glyph('→').is_some());
        assert_eq!(f.glyph_or('\u{4e00}'), f.glyph('?').unwrap());
        let cut = f.fit("abcdefghij", 60);
        assert!(f.measure(&cut) <= 60 && cut.ends_with('…'));
        assert_eq!(f.fit("ab", 60), "ab");
        let lines = f.wrap("one two three four five six seven", 100, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| f.measure(l) <= 100));
        assert!(lines[1].ends_with('…'));
        // each glyph's atlas pixels are its own
        let g = f.glyph('A').unwrap();
        assert_eq!(f.atlas[(u32::from(g.y) * PITCH + u32::from(g.x)) as usize], 255);
    }

    #[cfg(windows)]
    #[test]
    fn gdi_bakes_every_character_with_ink_and_scales_with_size() {
        let small = Font::bake(crate::theme::FONT_FACE, crate::theme::FONT_WEIGHT, 21).expect("bake 21 px");
        let big = Font::bake(crate::theme::FONT_FACE, crate::theme::FONT_WEIGHT, 42).expect("bake 42 px");
        assert_ne!(small.id, big.id);
        for c in chars().filter(|c| *c != ' ') {
            let g = small.glyph(c).unwrap();
            assert!(g.w > 0 && g.h > 0, "{c:?} has no ink");
            assert!(u32::from(g.h) <= small.line + 4, "{c:?} {g:?}");
        }
        assert!(small.line >= 21 && small.line <= 32, "line {}", small.line);
        let (a, b) = (small.measure("Frame rate 60 fps"), big.measure("Frame rate 60 fps"));
        assert!(b > a * 17 / 10 && b < a * 23 / 10, "{a} {b}");
        // tabular digits: the stats columns stay still as numbers change
        assert!(('0'..='9').all(|d| small.glyph(d).unwrap().advance == small.glyph('0').unwrap().advance));
        // the atlas stays small: one upload of a few hundred KB at most, even at 4K sizes
        assert!(big.atlas.len() < 512 << 10, "{} bytes", big.atlas.len());
    }
}
