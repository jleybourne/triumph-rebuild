//! The inverse of [`Layout::offset`]: a byte offset in a texture back to the
//! element it belongs to (mip, x, y, z and the byte inside the element), or
//! `None` for padding. Together with the forward maths this is X2.
//!
//! Every non-XOR GFX10 block is a permutation of address bits: each element
//! coordinate bit lands on one address bit (`Layout::pattern`), and the
//! bits below log2(element bytes) address the bytes of an element. So the
//! inverse inside a block is the same permutation read the other way, and
//! the block index splits into block row and column by the mip's pitch.

use crate::layout::{Dimension, Error, Layout, Swizzle};

/// One element of a texture, and the byte inside it an offset points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Texel {
    pub mip: u32,
    /// In elements (4x4 blocks for block-compressed formats).
    pub x: u32,
    pub y: u32,
    /// Array slice, or depth slice of a 3D texture.
    pub z: u32,
    /// Byte inside the element.
    pub byte: u32,
}

impl Layout {
    /// Where byte `offset` of the texture belongs: the element, or `None`
    /// for padding (block padding, the unused part of a mip tail, the
    /// space between rows of a linear mip). `Err(Invalid)` past the end,
    /// `Err(Unsupported)` for layouts [`Layout::offset`] cannot address.
    pub fn texel_at(&self, offset: u64) -> Result<Option<Texel>, Error> {
        if offset >= self.size {
            return Err(Error::Invalid);
        }
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        let slice = (offset / self.slice_size) as u32;
        let local = offset % self.slice_size;
        let n = u64::from(self.desc.element_bytes);
        let three_d = self.desc.dimension == Dimension::Tex3d;

        if self.desc.swizzle == Swizzle::LINEAR {
            // Mips are packed smallest first; find the one whose rows hold
            // `local`.
            for (i, m) in self.mips.iter().enumerate() {
                let row_bytes = u64::from(m.pitch) * n;
                let size = row_bytes * u64::from(m.rows);
                if (m.offset..m.offset + size).contains(&local) {
                    let inside = local - m.offset;
                    let (y, column) = ((inside / row_bytes) as u32, inside % row_bytes);
                    let (x, byte) = ((column / n) as u32, (column % n) as u32);
                    let z = slice;
                    let depth = if three_d { m.depth } else { self.slices };
                    let real = x < m.width && y < m.height && z < depth;
                    return Ok(real.then_some(Texel { mip: i as u32, x, y, z, byte }));
                }
            }
            return Ok(None);
        }

        let log2 = self.alignment.trailing_zeros();
        let block_bytes = self.alignment;
        let [bw, bh, bd] = self.block;
        let has_tail = self.first_tail_mip < self.desc.mips;
        let byte = (local % n) as u32;
        let base_z = if self.thick { slice * bd } else { slice };

        // Tail: the first block of the slice holds every tail mip, each at
        // its own element position.
        if has_tail && local < block_bytes {
            let [cx, cy, cz] = self.in_block(local);
            for i in self.first_tail_mip..self.desc.mips {
                let m = &self.mips[i as usize];
                let t = m.tail.expect("tail mips carry a position");
                let (x, y) = (cx.wrapping_sub(t.x), cy.wrapping_sub(t.y));
                if x < m.width && y < m.height {
                    let z = base_z + cz;
                    let depth = if three_d { m.depth } else { self.slices };
                    return Ok((z < depth).then_some(Texel { mip: i, x, y, z, byte }));
                }
            }
            return Ok(None);
        }

        for i in 0..self.first_tail_mip {
            let m = &self.mips[i as usize];
            let size = u64::from(m.pitch) * u64::from(m.rows) * n * u64::from(bd);
            // A single-mip layout keeps its samples in the slice too.
            let size = if self.desc.mips == 1 { self.slice_size } else { size };
            if !(m.offset..m.offset + size).contains(&local) {
                continue;
            }
            let inside = local - m.offset;
            let blocks_per_row = u64::from(m.pitch / bw);
            let index = inside >> log2;
            let (by, bx) = ((index / blocks_per_row) as u32, (index % blocks_per_row) as u32);
            let [cx, cy, cz] = self.in_block(inside & (block_bytes - 1));
            let (x, y, z) = (bx * bw + cx, by * bh + cy, base_z + cz);
            let depth = if three_d { m.depth } else { self.slices };
            let real = x < m.width && y < m.height && z < depth;
            return Ok(real.then_some(Texel { mip: i, x, y, z, byte }));
        }
        Ok(None)
    }

    /// Element coordinates inside a block for a byte offset inside it.
    fn in_block(&self, offset: u64) -> [u32; 3] {
        let offset = offset as u32;
        let mut out = [0u32; 3];
        for (axis, table) in self.pattern.iter().enumerate() {
            // The table spreads coordinate c; its entry at 1 << i is the
            // address bit coordinate bit i lands on.
            let bits = table.len().trailing_zeros();
            for i in 0..bits {
                if offset & table[1 << i] != 0 {
                    out[axis] |= 1 << i;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Desc;

    pub(crate) fn every_mode() -> Vec<Swizzle> {
        vec![
            Swizzle::LINEAR,
            Swizzle::SW_256B_S,
            Swizzle::SW_256B_D,
            Swizzle::SW_4KB_S,
            Swizzle::SW_4KB_D,
            Swizzle::SW_64KB_S,
            Swizzle::SW_64KB_D,
        ]
    }

    /// Small textures in every addressable mode, every element size, with
    /// odd sizes, mip tails, arrays, block compression and thick 3D.
    pub(crate) fn small_textures() -> Vec<Desc> {
        let mut out = vec![];
        for swizzle in every_mode() {
            for element_bytes in [1u32, 2, 4, 8, 16] {
                for (w, h, depth, mips) in [(1, 1, 1, 1), (37, 19, 1, 6), (64, 64, 2, 7), (5, 130, 1, 8)] {
                    out.push(Desc {
                        dimension: Dimension::Tex2d,
                        width: w,
                        height: h,
                        depth,
                        mips,
                        element_bytes,
                        block: 1,
                        samples: 1,
                        swizzle,
                    });
                }
                // Block compressed: 4x4 texels an element.
                if matches!(element_bytes, 8 | 16) {
                    out.push(Desc {
                        dimension: Dimension::Tex2d,
                        width: 101,
                        height: 67,
                        depth: 1,
                        mips: 7,
                        element_bytes,
                        block: 4,
                        samples: 1,
                        swizzle,
                    });
                }
            }
            // 1D (linear only on the console's suggestion, but every 2D mode
            // takes height 1).
            out.push(Desc {
                dimension: Dimension::Tex1d,
                width: 300,
                height: 1,
                depth: 1,
                mips: 9,
                element_bytes: 4,
                block: 1,
                samples: 1,
                swizzle,
            });
        }
        // 3D: linear, and the thick S modes (D 3D is refused by `Layout::new`
        // as on GFX10).
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_S] {
            for element_bytes in [1u32, 4, 16] {
                out.push(Desc {
                    dimension: Dimension::Tex3d,
                    width: 33,
                    height: 17,
                    depth: 9,
                    mips: 6,
                    element_bytes,
                    block: 1,
                    samples: 1,
                    swizzle,
                });
            }
        }
        out
    }

    /// X2: every element of every mip and slice goes to an offset and back,
    /// and every byte of the texture is either exactly one element's byte or
    /// padding.
    #[test]
    fn x2_offset_and_inverse_round_trip_over_every_texel() {
        let mut checked = 0u64;
        for d in small_textures() {
            let l = Layout::new(&d).unwrap_or_else(|e| panic!("{d:?}: {e:?}"));
            let n = d.element_bytes;
            let mut owner = vec![false; l.size as usize];
            for mip in 0..d.mips {
                let m = l.mips[mip as usize];
                let depth = if d.dimension == Dimension::Tex3d { m.depth } else { l.slices };
                for z in 0..depth {
                    for y in 0..m.height {
                        for x in 0..m.width {
                            let at = l.offset(mip, x, y, z).unwrap();
                            for byte in 0..n {
                                let o = at + u64::from(byte);
                                assert_eq!(
                                    l.texel_at(o),
                                    Ok(Some(Texel { mip, x, y, z, byte })),
                                    "{d:?} mip {mip} ({x},{y},{z})"
                                );
                                assert!(!owner[o as usize], "{d:?} byte {o} twice");
                                owner[o as usize] = true;
                            }
                            checked += 1;
                        }
                    }
                }
            }
            // The other direction: every byte the inverse names maps back.
            for o in 0..l.size {
                match l.texel_at(o).unwrap() {
                    Some(t) => {
                        assert!(owner[o as usize], "{d:?} byte {o} named but unowned");
                        assert_eq!(l.offset(t.mip, t.x, t.y, t.z).unwrap() + u64::from(t.byte), o);
                    }
                    None => assert!(!owner[o as usize], "{d:?} byte {o} owned but called padding"),
                }
            }
            assert_eq!(l.texel_at(l.size), Err(Error::Invalid));
        }
        assert!(checked > 100_000, "{checked}");
    }

    #[test]
    fn xor_modes_are_refused_not_guessed() {
        let d = Desc {
            dimension: Dimension::Tex2d,
            width: 64,
            height: 64,
            depth: 1,
            mips: 1,
            element_bytes: 4,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_64KB_R_X,
        };
        let l = Layout::new(&d).unwrap();
        assert_eq!(l.texel_at(0), Err(Error::Unsupported));
        assert_eq!(l.offset(0, 0, 0, 0), Err(Error::Unsupported));
    }
}
