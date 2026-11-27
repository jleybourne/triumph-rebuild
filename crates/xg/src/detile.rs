//! X4: the CPU detiler, console layout to linear rows and back.
//!
//! An element's address is `row(y, z) + column(x)` in every addressable
//! mode (the block row and the in-block y and z bits never share an address
//! bit with x). So a subresource is two tables, one entry per column and one
//! per row, the same tables a GPU detile kernel would read
//! ([`Tables`]). The column table is folded into contiguous runs once per
//! subresource, and every row copies the same runs from its own base.

use crate::layout::{Dimension, Error, Layout, Swizzle};

/// The per-axis address tables of one subresource (mip `mip`, array slice
/// `slice`; for 3D every depth slice of the mip): the byte offset of
/// element (x, y, z) in the whole texture is `rows[z * height + y] +
/// columns[x]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tables {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub element_bytes: u32,
    pub columns: Vec<u32>,
    pub rows: Vec<u64>,
}

impl Layout {
    /// The address tables of a subresource. 3D textures take `slice` 0 and
    /// cover the mip's depth.
    pub fn tables(&self, mip: u32, slice: u32) -> Result<Tables, Error> {
        let m = *self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let three_d = self.desc.dimension == Dimension::Tex3d;
        let (first, depth) = if three_d {
            if slice != 0 {
                return Err(Error::Invalid);
            }
            (0, m.depth)
        } else {
            if slice >= self.slices {
                return Err(Error::Invalid);
            }
            (slice, 1)
        };
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        // Origin of row 0 / column 0 split out of the forward maths: the
        // column part is `offset(x, 0) - offset(0, 0)`.
        let origin = self.offset(mip, 0, 0, first)?;
        let mut columns = Vec::with_capacity(m.width as usize);
        for x in 0..m.width {
            columns.push((self.offset(mip, x, 0, first)? - origin) as u32);
        }
        let mut rows = Vec::with_capacity((m.height * depth) as usize);
        for z in 0..depth {
            for y in 0..m.height {
                rows.push(self.offset(mip, 0, y, first + z)?);
            }
        }
        Ok(Tables {
            width: m.width,
            height: m.height,
            depth,
            element_bytes: self.desc.element_bytes,
            columns,
            rows,
        })
    }

    /// Copies one subresource out of tiled memory (the whole texture,
    /// [`Layout::size`] bytes) into linear rows `row_pitch` bytes apart,
    /// depth slices `depth_pitch` apart.
    pub fn detile(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        let t = self.tables(mip, slice)?;
        self.check(&t, tiled.len(), linear.len(), row_pitch, depth_pitch)?;
        copy_rows::<true>(&t, tiled, linear, row_pitch, depth_pitch, self.desc.swizzle);
        Ok(())
    }

    /// The reverse of [`Layout::detile`]: linear rows into tiled memory.
    pub fn tile(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        let t = self.tables(mip, slice)?;
        self.check(&t, tiled.len(), linear.len(), row_pitch, depth_pitch)?;
        copy_rows::<false>(&t, linear, tiled, row_pitch, depth_pitch, self.desc.swizzle);
        Ok(())
    }

    fn check(
        &self,
        t: &Tables,
        tiled: usize,
        linear: usize,
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        let bytes = t.width as usize * t.element_bytes as usize;
        let need = (t.depth as usize - 1)
            .checked_mul(depth_pitch)
            .zip((t.height as usize - 1).checked_mul(row_pitch))
            .and_then(|(a, b)| a.checked_add(b))
            .and_then(|o| o.checked_add(bytes))
            .ok_or(Error::TooSmall)?;
        if (tiled as u64) < self.size || linear < need || row_pitch < bytes {
            return Err(Error::TooSmall);
        }
        Ok(())
    }
}

/// Contiguous runs of one row: (tiled offset from the row base, linear
/// offset, length), all in bytes.
fn runs(t: &Tables, swizzle: Swizzle) -> Vec<(usize, usize, usize)> {
    let n = t.element_bytes as usize;
    let bytes = t.width as usize * n;
    if swizzle == Swizzle::LINEAR {
        return vec![(0, 0, bytes)];
    }
    let mut out = vec![];
    let (mut tiled, mut linear) = (t.columns[0] as usize, 0);
    for x in 1..t.width as usize {
        let c = t.columns[x] as usize;
        let at = x * n;
        if c != tiled + (at - linear) {
            out.push((tiled, linear, at - linear));
            tiled = c;
            linear = at;
        }
    }
    out.push((tiled, linear, bytes - linear));
    out
}

/// Runs of one fixed length `N`, copied as arrays so the compiler emits one
/// move each; the general path copies slices.
fn copy_row<const N: usize, const DETILE: bool>(
    runs: &[(usize, usize, usize)],
    src: &[u8],
    dst: &mut [u8],
    tiled_row: usize,
    linear_row: usize,
) {
    for &(t, l, len) in runs {
        let (from, to) = if DETILE {
            (tiled_row + t, linear_row + l)
        } else {
            (linear_row + l, tiled_row + t)
        };
        if N == 0 {
            dst[to..to + len].copy_from_slice(&src[from..from + len]);
        } else {
            let s: &[u8; N] = src[from..from + N].try_into().unwrap();
            let d: &mut [u8; N] = (&mut dst[to..to + N]).try_into().unwrap();
            *d = *s;
        }
    }
}

/// Copies every row of a subresource; `src` is the tiled memory when
/// detiling, the linear rows when tiling.
fn copy_rows<const DETILE: bool>(
    t: &Tables,
    src: &[u8],
    dst: &mut [u8],
    row_pitch: usize,
    depth_pitch: usize,
    swizzle: Swizzle,
) {
    let runs = runs(t, swizzle);
    // Every run but the last has one length in the common modes; the last
    // can end partway through a run.
    let (body, last) = runs.split_at(runs.len() - 1);
    let uniform = body.first().map(|r| r.2).filter(|&len| body.iter().all(|r| r.2 == len));
    for z in 0..t.depth as usize {
        for y in 0..t.height as usize {
            let tiled_row = t.rows[z * t.height as usize + y] as usize;
            let linear_row = z * depth_pitch + y * row_pitch;
            match uniform {
                Some(4) => copy_row::<4, DETILE>(body, src, dst, tiled_row, linear_row),
                Some(8) => copy_row::<8, DETILE>(body, src, dst, tiled_row, linear_row),
                Some(16) => copy_row::<16, DETILE>(body, src, dst, tiled_row, linear_row),
                Some(32) => copy_row::<32, DETILE>(body, src, dst, tiled_row, linear_row),
                Some(64) => copy_row::<64, DETILE>(body, src, dst, tiled_row, linear_row),
                _ => copy_row::<0, DETILE>(body, src, dst, tiled_row, linear_row),
            }
            copy_row::<0, DETILE>(last, src, dst, tiled_row, linear_row);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::{Desc, Texture, TextureDesc};

    /// Deterministic synthetic bytes (xorshift64*), the same in the fork
    /// harness that produced [`FORK`].
    pub(crate) fn synthetic(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    pub(crate) fn fnv(bytes: &[u8], mut h: u64) -> u64 {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    pub(crate) const FNV: u64 = 0xcbf2_9ce4_8422_2325;

    /// The X4 cases: the title's formats (X1) in every addressable mode,
    /// as `(name, dxgi format, dimension, width, height, depth or array,
    /// mips (0 full chain), swizzle)`.
    pub(crate) fn cases() -> Vec<(String, TextureDesc)> {
        let formats: [(&str, u32); 8] = [
            ("BC1", 71),
            ("BC3", 77),
            ("BC7", 98),
            ("RGBA8", 28),
            ("R16", 56),
            ("R8", 61),
            ("RGBA16F", 10),
            ("RGBA32F", 2),
        ];
        let modes = [
            Swizzle::LINEAR,
            Swizzle::SW_256B_S,
            Swizzle::SW_256B_D,
            Swizzle::SW_4KB_S,
            Swizzle::SW_4KB_D,
            Swizzle::SW_64KB_S,
            Swizzle::SW_64KB_D,
        ];
        let mut out = vec![];
        for (name, format) in formats {
            for swizzle in modes {
                for (w, h, a, mips) in [(256u32, 256u32, 1u32, 0u32), (300, 77, 3, 0), (128, 4, 1, 1)] {
                    out.push((
                        format!("{name} {w}x{h}x{a} mode {}", swizzle.0),
                        TextureDesc {
                            dimension: Dimension::Tex2d,
                            width: w,
                            height: h,
                            depth_or_array: a,
                            mips,
                            format,
                            samples: 1,
                            swizzle,
                        },
                    ));
                }
            }
        }
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_S] {
            for (name, format) in [("RGBA8", 28u32), ("BC7", 98)] {
                out.push((
                    format!("3D {name} 64x48x20 mode {}", swizzle.0),
                    TextureDesc {
                        dimension: Dimension::Tex3d,
                        width: 64,
                        height: 48,
                        depth_or_array: 20,
                        mips: 0,
                        format,
                        samples: 1,
                        swizzle,
                    },
                ));
            }
        }
        out
    }

    /// Detiles every subresource of a case's synthetic tiled bytes, rows
    /// tight plus 3 bytes, and hashes the rows in order. The fork harness
    /// does exactly this with its `Layout::detile`.
    pub(crate) fn detile_hash(t: &TextureDesc, seed: u64) -> u64 {
        let tex = Texture::new(t).unwrap();
        let l = &tex.planes[0].layout;
        let tiled = synthetic(l.size as usize, seed);
        let mut h = FNV;
        let slices = if t.dimension == Dimension::Tex3d { 1 } else { l.slices };
        for slice in 0..slices {
            for mip in 0..l.mips.len() as u32 {
                let m = l.mips[mip as usize];
                let row = (m.width * l.desc.element_bytes) as usize + 3;
                let plane = row * m.height as usize;
                let mut out = vec![0u8; plane * m.depth as usize];
                l.detile(&tiled, mip, slice, &mut out, row, plane).unwrap();
                h = fnv(&out, h);
            }
        }
        h
    }

    /// FNV-1a of the fork's detiled rows (`victory_formats::texture::
    /// Layout::detile`, jleybourne/Triumph `triumph/main`) for each case in
    /// [`cases`], seed = case index. Synthetic bytes only. Made by the
    /// harness described in `agents/xg.md`.
    const FORK: &[u64] = &include!("fork_hashes.in");

    #[test]
    fn x4_detile_matches_the_fork_on_synthetic_data() {
        let cases = cases();
        assert_eq!(cases.len(), FORK.len(), "one fork hash per case");
        for (i, ((name, t), &want)) in cases.iter().zip(FORK).enumerate() {
            assert_eq!(detile_hash(t, i as u64), want, "case {i}: {name}");
        }
    }

    /// Tiling then detiling gives the rows back, and the detiled bytes are
    /// exactly what [`Layout::offset`] names, element by element.
    #[test]
    fn x4_detile_agrees_with_element_offsets_and_round_trips() {
        for (i, (name, t)) in cases().iter().enumerate() {
            let tex = Texture::new(t).unwrap();
            let l = &tex.planes[0].layout;
            let tiled = synthetic(l.size as usize, i as u64 + 1000);
            let n = l.desc.element_bytes as usize;
            let mut retiled = vec![0u8; tiled.len()];
            let slices = if t.dimension == Dimension::Tex3d { 1 } else { l.slices };
            for slice in 0..slices {
                for mip in 0..l.mips.len() as u32 {
                    let m = l.mips[mip as usize];
                    let row = m.width as usize * n + 5;
                    let plane = row * m.height as usize + 7;
                    let mut out = vec![0xcc; plane * m.depth as usize];
                    l.detile(&tiled, mip, slice, &mut out, row, plane).unwrap();
                    for z in 0..m.depth {
                        for y in 0..m.height {
                            for x in 0..m.width {
                                let at = l.offset(mip, x, y, slice + z).unwrap() as usize;
                                let lin = z as usize * plane + y as usize * row + x as usize * n;
                                assert_eq!(out[lin..lin + n], tiled[at..at + n], "{name} mip {mip}");
                            }
                            // Padding between rows untouched.
                            let end = z as usize * plane + y as usize * row + m.width as usize * n;
                            assert!(out[end..end + 5].iter().all(|&b| b == 0xcc), "{name}");
                        }
                    }
                    l.tile(&mut retiled, mip, slice, &out, row, plane).unwrap();
                }
            }
            // Every element byte went back where it came from; padding stays 0.
            for (o, (&a, &b)) in tiled.iter().zip(&retiled).enumerate() {
                match l.texel_at(o as u64).unwrap() {
                    Some(_) => assert_eq!(a, b, "{name} byte {o}"),
                    None => assert_eq!(b, 0, "{name} padding byte {o}"),
                }
            }
        }
    }

    #[test]
    fn copies_refuse_short_buffers_and_overflowing_pitches() {
        let l = Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width: 64,
            height: 64,
            depth: 1,
            mips: 1,
            element_bytes: 4,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        })
        .unwrap();
        let mut out = vec![0; 64 * 64 * 4];
        assert_eq!(l.detile(&[0; 100], 0, 0, &mut out, 256, 0), Err(Error::TooSmall));
        let tiled = vec![0; l.size as usize];
        assert_eq!(l.detile(&tiled, 0, 0, &mut out, usize::MAX, 0), Err(Error::TooSmall));
        assert_eq!(l.detile(&tiled, 0, 1, &mut out, 256, 0), Err(Error::Invalid));
        assert_eq!(l.detile(&tiled, 1, 0, &mut out, 256, 0), Err(Error::Invalid));
    }

    /// The number for X4: detile throughput of a 2048x2048 BC7 chain in
    /// 64 KiB `D` (the shape BCPack streams deliver), one thread.
    #[test]
    #[ignore = "a measurement; run with --ignored --nocapture"]
    fn x4_detile_throughput() {
        for (name, format, swizzle) in [
            ("BC7 64KB_D", 98u32, Swizzle::SW_64KB_D),
            ("BC7 4KB_S", 98, Swizzle::SW_4KB_S),
            ("RGBA8 4KB_S", 28, Swizzle::SW_4KB_S),
            ("R8 64KB_S", 61, Swizzle::SW_64KB_S),
        ] {
            let tex = Texture::new(&TextureDesc {
                dimension: Dimension::Tex2d,
                width: 2048,
                height: 2048,
                depth_or_array: 1,
                mips: 0,
                format,
                samples: 1,
                swizzle,
            })
            .unwrap();
            let l = &tex.planes[0].layout;
            let tiled = synthetic(l.size as usize, 7);
            let outs: Vec<(usize, Vec<u8>)> = l
                .mips
                .iter()
                .map(|m| {
                    let row = (m.width * l.desc.element_bytes) as usize;
                    (row, vec![0u8; row * m.height as usize])
                })
                .collect();
            let mut outs = outs;
            let reps = 20;
            let start = std::time::Instant::now();
            let mut bytes = 0usize;
            for _ in 0..reps {
                for (mip, (row, out)) in outs.iter_mut().enumerate() {
                    let len = out.len();
                    l.detile(&tiled, mip as u32, 0, out, *row, len).unwrap();
                    bytes += out.len();
                }
            }
            let s = start.elapsed().as_secs_f64();
            println!(
                "{name}: {:.1} MB a chain, {:.3} ms a chain, {:.2} GB/s",
                bytes as f64 / reps as f64 / 1e6,
                s * 1e3 / reps as f64,
                bytes as f64 / s / 1e9
            );
        }
    }
}
