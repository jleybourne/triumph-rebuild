// Derived from VIctory (Marvisak and contributors) via jleybourne/Triumph, GPL-3.0-or-later.
// Used here under GPL-3.0-only. Changes: the console structure layouts, the vtable slot meanings and
// the suggestion rule are taken from `victory-hle/src/modules/xg.rs`; the code is rewritten as plain
// Rust methods (no vtable, no `extern "system"`, no logging) for the lead to bind in `crates/runtime`.

//! X3: `xg_xs.dll` as the title calls it.
//!
//! The title imports two functions, `XGCreateTextureComputer(desc, out)`
//! and `XGSuggestSwizzleMode(desc, flags, out)`, and calls six methods on
//! the computer the first one returns ([`SLOTS`]). Everything here takes
//! and returns the console's own structures ([`ResourceDesc`],
//! [`LayoutOut`], [`PackedMips`], [`SubresourceTiling`]); the lead wraps
//! the methods as `extern "system"` functions in a vtable of 13 slots.

use crate::layout::{Dimension, Error, Layout, Swizzle, Texture, TextureDesc};

pub type HResult = i32;
pub const S_OK: HResult = 0;
pub const E_FAIL: HResult = 0x8000_4005_u32 as i32;
pub const E_NOTIMPL: HResult = 0x8000_4001_u32 as i32;
pub const E_INVALIDARG: HResult = 0x8007_0057_u32 as i32;

/// One computer method: its byte offset in the vtable, what it does, and
/// whether the title calls it (from the fork's runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub offset: u32,
    pub name: &'static str,
    /// `(arguments after this)`, as the title passes them.
    pub args: &'static str,
    pub title_calls: bool,
}

/// The computer's vtable. The title calls Release, the layout, the element
/// offset, both copies and the 64 KiB tiling; the others are named from the
/// library's public shape and answered `E_NOTIMPL`.
pub const SLOTS: [Slot; 13] = [
    Slot { offset: 0x00, name: "AddRef", args: "() -> u32", title_calls: false },
    Slot { offset: 0x08, name: "Release", args: "() -> u32", title_calls: true },
    Slot { offset: 0x10, name: "GetResourceLayout", args: "(LayoutOut*) -> HRESULT", title_calls: true },
    Slot { offset: 0x18, name: "GetResourceSizeBytes", args: "() -> u64", title_calls: false },
    Slot { offset: 0x20, name: "GetResourceBaseAlignmentBytes", args: "() -> u64", title_calls: false },
    Slot { offset: 0x28, name: "GetMipLevelOffsetBytes", args: "(plane, mip) -> u64", title_calls: false },
    Slot {
        offset: 0x30,
        name: "GetTexelElementOffsetBytes",
        args: "(plane, mip, x: u64, y, z or slice, sample, bit*) -> u64",
        title_calls: true,
    },
    Slot { offset: 0x38, name: "GetTexelCoordinate", args: "(plane, offset, ...) -> HRESULT", title_calls: false },
    Slot {
        offset: 0x40,
        name: "CopyIntoSubresource",
        args: "(tiled*, plane, subresource, linear*, row pitch, depth pitch) -> HRESULT",
        title_calls: true,
    },
    Slot {
        offset: 0x48,
        name: "CopyFromSubresource",
        args: "(linear*, plane, subresource, sample, tiled*, row pitch, depth pitch) -> HRESULT",
        title_calls: true,
    },
    Slot {
        offset: 0x50,
        name: "GetResourceTiling",
        args: "(tiles*, PackedMips*, shape[3]*, count* in/out, first, SubresourceTiling*) -> HRESULT",
        title_calls: true,
    },
    Slot { offset: 0x58, name: "GetTextureViewDescriptor", args: "(...) -> HRESULT", title_calls: false },
    Slot { offset: 0x60, name: "(tiled resource query)", args: "(...) -> HRESULT", title_calls: false },
];

/// `D3D12_RESOURCE_DESC` as the console fills it: `layout` is `0x100 |
/// swizzle mode`, 1 for row major (buffers), 0 for "the library picks".
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceDesc {
    pub dimension: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array: u16,
    pub mips: u16,
    pub format: u32,
    pub samples: u32,
    pub quality: u32,
    pub layout: u32,
    pub flags: u32,
}
const _: () = assert!(std::mem::size_of::<ResourceDesc>() == 0x38);

pub const SWIZZLED: u32 = 0x100;
pub const ROW_MAJOR: u32 = 1;
pub const ALLOW_RENDER_TARGET: u32 = 1;
pub const ALLOW_DEPTH_STENCIL: u32 = 2;
pub const TEXTURE_1D: u32 = 2;
pub const TEXTURE_2D: u32 = 3;
pub const TEXTURE_3D: u32 = 4;

/// The mode the library picks: linear for 1D, the XOR depth and render
/// modes for targets, else 4 KiB standard.
///
/// shortcut (the fork's): the rule of the library's older optimal-mode
/// function. `XGSuggestSwizzleMode` is newer than any copy checked; a
/// texture whose data arrives tiled in another mode carries that mode in
/// its own `layout`, so this only decides "layout 0" textures.
pub fn suggested(desc: &ResourceDesc) -> Swizzle {
    if desc.dimension == TEXTURE_1D {
        Swizzle::LINEAR
    } else if desc.flags & ALLOW_DEPTH_STENCIL != 0 {
        Swizzle::SW_64KB_Z_X
    } else if desc.flags & ALLOW_RENDER_TARGET != 0 {
        Swizzle::SW_64KB_R_X
    } else {
        Swizzle::SW_4KB_S
    }
}

/// `XGSuggestSwizzleMode(desc, flags, out)`: the value for the description's
/// `layout` field.
pub fn suggest_swizzle_mode(desc: &ResourceDesc) -> u32 {
    SWIZZLED | suggested(desc).0
}

/// Mips in one plane of [`LayoutOut`].
pub const MIPS: usize = 15;

/// One mip of [`PlaneOut`]. The title reads `size` (0x00), `offset` (0x08),
/// `slice_size` (0x10) and `pitch_bytes` (0x1c).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MipOut {
    pub size: u64,
    pub offset: u64,
    pub slice_size: u64,
    pub pitch_texels: u32,
    pub pitch_bytes: u32,
    pub alignment: u32,
    /// Padded width, height in elements, and depth or array size.
    pub padded: [u32; 3],
    pub elements: [u32; 2],
    pub texels: [u32; 2],
    pub depth_or_array: u32,
    pub samples: u32,
    pub swizzle: u32,
    pub bank_rotation: [u64; 2],
    pub slice_depth: u32,
    pub in_tail: u32,
    /// A 3D tail mip's position in elements: x bits 0-10, y 11-21.
    pub tail_position: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneOut {
    /// 1 data, 7 depth, 8 stencil.
    pub usage: u32,
    pub size: u64,
    pub slice_size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub element_bytes: u32,
    pub standard_mips: u32,
    pub tail_mips: u32,
    pub mips: [MipOut; MIPS],
}

/// What `GetResourceLayout` (0x10) writes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutOut {
    pub size: u64,
    pub alignment: u64,
    pub mips: u32,
    pub planes: u32,
    pub plane: [PlaneOut; 4],
    pub dimension: u32,
}
const _: () = assert!(std::mem::size_of::<MipOut>() == 0x70);
const _: () = assert!(std::mem::size_of::<PlaneOut>() == 0x6c8);
const _: () = assert!(std::mem::size_of::<LayoutOut>() == 0x1b40);

impl LayoutOut {
    pub fn zeroed() -> Box<LayoutOut> {
        // SAFETY: plain integers only; all zeroes is a valid value.
        Box::new(unsafe { std::mem::zeroed() })
    }
}

/// `D3D12_PACKED_MIP_INFO`-shaped.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PackedMips {
    pub standard: u8,
    pub packed: u8,
    pub tiles: u32,
    pub start: u32,
}

/// `D3D12_SUBRESOURCE_TILING`-shaped.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SubresourceTiling {
    pub width: u32,
    pub height: u16,
    pub depth: u16,
    pub start: u32,
}

/// What `GetResourceTiling` (0x50) answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiling {
    pub tiles: u32,
    pub packed: PackedMips,
    /// Tile shape in texels.
    pub shape: [u32; 3],
    /// Entries from `first`, at most the caller's room.
    pub subresources: Vec<SubresourceTiling>,
}

const TILE: u64 = 64 << 10;

/// A texture computer: one description, its layout.
#[derive(Debug, Clone)]
pub struct Computer {
    pub desc: ResourceDesc,
    pub texture: Texture,
    pub mips: u32,
}

fn hresult(r: Result<(), Error>) -> HResult {
    match r {
        Ok(()) => S_OK,
        Err(Error::Unsupported) => E_NOTIMPL,
        Err(_) => E_INVALIDARG,
    }
}

impl Computer {
    /// `XGCreateTextureComputer(desc, out)`. `Err(E_INVALIDARG)` for a
    /// description with no layout (unknown format, mode or dimension, more
    /// than 15 mips).
    pub fn new(desc: &ResourceDesc) -> Result<Computer, HResult> {
        let dimension = match desc.dimension {
            TEXTURE_1D => Dimension::Tex1d,
            TEXTURE_2D => Dimension::Tex2d,
            TEXTURE_3D => Dimension::Tex3d,
            _ => return Err(E_INVALIDARG),
        };
        let swizzle = match desc.layout {
            0 => suggested(desc),
            ROW_MAJOR => Swizzle::LINEAR,
            l if l & !0xff == SWIZZLED => Swizzle(l & 0xff),
            _ => return Err(E_INVALIDARG),
        };
        let texture = Texture::new(&TextureDesc {
            dimension,
            width: u32::try_from(desc.width).map_err(|_| E_INVALIDARG)?,
            height: desc.height,
            depth_or_array: u32::from(desc.depth_or_array),
            mips: u32::from(desc.mips),
            format: desc.format,
            samples: desc.samples.max(1),
            swizzle,
        })
        .map_err(|_| E_INVALIDARG)?;
        let mips = texture.planes[0].layout.mips.len() as u32;
        if mips as usize > MIPS {
            return Err(E_INVALIDARG);
        }
        Ok(Computer { desc: *desc, texture, mips })
    }

    /// 0x10 `GetResourceLayout`.
    pub fn layout(&self, out: &mut LayoutOut) {
        // SAFETY: plain integers only.
        *out = unsafe { std::mem::zeroed() };
        out.size = self.texture.size;
        out.alignment = self.texture.alignment;
        out.mips = self.mips;
        out.planes = self.texture.planes.len() as u32;
        out.dimension = self.desc.dimension;
        // The library calls a depth plane "depth" only beside stencil or
        // without mips.
        let depth = self.desc.flags & ALLOW_DEPTH_STENCIL != 0
            && (self.texture.planes.len() > 1 || self.mips == 1);
        for (plane, p) in out.plane.iter_mut().zip(&self.texture.planes) {
            let l = &p.layout;
            plane.usage = match (p.stencil, depth) {
                (true, _) => 8,
                (false, true) => 7,
                (false, false) => 1,
            };
            plane.size = l.size;
            plane.slice_size = l.slice_size;
            plane.offset = p.offset;
            plane.alignment = l.alignment;
            plane.element_bytes = l.desc.element_bytes;
            plane.standard_mips = l.first_tail_mip;
            plane.tail_mips = self.mips - l.first_tail_mip;
            for (i, mip) in plane.mips.iter_mut().take(l.mips.len()).enumerate() {
                *mip = self.mip_out(l, i);
            }
        }
    }

    fn mip_out(&self, l: &Layout, i: usize) -> MipOut {
        let m = &l.mips[i];
        let d = &self.desc;
        let bpe = u64::from(l.desc.element_bytes);
        let three_d = d.dimension == TEXTURE_3D;
        let array = u32::from(d.depth_or_array).max(1);
        let mut out = MipOut {
            pitch_texels: m.pitch * l.desc.block,
            pitch_bytes: m.pitch * l.desc.element_bytes,
            padded: [m.pitch, m.rows, if three_d { m.depth_pitch } else { array }],
            elements: [m.width, m.height],
            texels: [((d.width >> i) as u32).max(1), (d.height >> i).max(1)],
            depth_or_array: if three_d { m.depth } else { array },
            samples: l.desc.samples,
            slice_depth: l.block[2],
            ..MipOut::default()
        };
        let slice = u64::from(m.pitch) * u64::from(m.rows) * bpe;
        let depth = u64::from(if three_d { m.depth_pitch } else { 1 });
        match m.tail {
            None => {
                out.offset = m.offset;
                out.slice_size = slice * u64::from(l.desc.samples);
                out.size = out.slice_size * depth;
                out.alignment = l.alignment as u32;
                out.swizzle = l.desc.swizzle.0;
            }
            Some(t) => {
                out.in_tail = 1;
                out.alignment = 256;
                // The 256-byte mode of the same kind (3 for the render
                // kind, which GFX10 lacks); depth keeps its own.
                out.swizzle = match l.desc.swizzle.0 {
                    2 | 6 | 10 | 18 | 22 | 26 => Swizzle::SW_256B_D.0,
                    23 | 27 => 3,
                    24 => 24,
                    _ => Swizzle::SW_256B_S.0,
                };
                if l.thick {
                    out.slice_size = l.alignment / u64::from(l.block[2]);
                    out.tail_position = t.x | t.y << 11;
                } else {
                    out.offset = u64::from(t.offset);
                    out.slice_size = slice;
                }
                out.size = out.slice_size * depth;
            }
        }
        out
    }

    /// 0x30 `GetTexelElementOffsetBytes`: an element's byte offset from the
    /// texture's start, 0 outside (the title reads through the answer, so
    /// an error code would send it into wild memory). The bit position is
    /// always 0.
    pub fn element_offset(&self, plane: u32, mip: u32, x: u64, y: u32, z: u32) -> u64 {
        let Some(p) = self.texture.planes.get(plane as usize) else {
            return 0;
        };
        let x = u32::try_from(x).unwrap_or(u32::MAX);
        p.layout.offset(mip, x, y, z).map_or(0, |at| p.offset + at)
    }

    /// The layout, mip and slice of a subresource index, and the bytes of
    /// tiled memory (from the plane's start) and of linear rows it touches.
    fn subresource(&self, plane: u32, subresource: u32, row: u32, depth: u32) -> Option<(&Layout, u32, u32, usize, usize)> {
        let p = self.texture.planes.get(plane as usize)?;
        let l = &p.layout;
        let (mip, slice) = (subresource % self.mips, subresource / self.mips);
        let m = l.mips.get(mip as usize)?;
        let bytes = m.width as usize * l.desc.element_bytes as usize;
        let linear = (m.depth as usize - 1) * depth as usize + (m.height as usize - 1) * row as usize + bytes;
        Some((l, mip, slice, l.size as usize, linear))
    }

    /// The plane's offset from the texture's start.
    pub fn plane_offset(&self, plane: u32) -> Option<u64> {
        self.texture.planes.get(plane as usize).map(|p| p.offset)
    }

    /// Bytes `CopyIntoSubresource` / `CopyFromSubresource` touch: tiled
    /// (from the texture's start) and linear, or `None` for a bad index.
    pub fn copy_extent(&self, plane: u32, subresource: u32, row: u32, depth: u32) -> Option<(usize, usize)> {
        let (_, _, _, tiled, linear) = self.subresource(plane, subresource, row, depth)?;
        Some((self.plane_offset(plane)? as usize + tiled, linear))
    }

    /// 0x40 `CopyIntoSubresource`: linear rows into tiled memory. `tiled`
    /// starts at the texture's start.
    pub fn copy_into(&self, tiled: &mut [u8], plane: u32, subresource: u32, linear: &[u8], row: u32, depth: u32) -> HResult {
        let Some((l, mip, slice, _, _)) = self.subresource(plane, subresource, row, depth) else {
            return E_INVALIDARG;
        };
        let offset = self.texture.planes[plane as usize].offset as usize;
        let Some(tiled) = tiled.get_mut(offset..) else {
            return E_INVALIDARG;
        };
        hresult(l.tile(tiled, mip, slice, linear, row as usize, depth as usize))
    }

    /// 0x48 `CopyFromSubresource`: tiled memory into linear rows (the
    /// sample argument is ignored: MSAA is not addressable).
    pub fn copy_from(&self, linear: &mut [u8], plane: u32, subresource: u32, tiled: &[u8], row: u32, depth: u32) -> HResult {
        let Some((l, mip, slice, _, _)) = self.subresource(plane, subresource, row, depth) else {
            return E_INVALIDARG;
        };
        let offset = self.texture.planes[plane as usize].offset as usize;
        let Some(tiled) = tiled.get(offset..) else {
            return E_INVALIDARG;
        };
        hresult(l.detile(tiled, mip, slice, linear, row as usize, depth as usize))
    }

    /// 0x50 `GetResourceTiling`, in 64 KiB tiles of plane 0, for the 2D
    /// 64 KiB modes (`E_FAIL` for smaller blocks, as on the console;
    /// `E_NOTIMPL` for thick 3D). The tail is the packed part, tile 0 of
    /// each slice. With a tail the library lists two slices at most and
    /// zeroes all but the first.
    pub fn tiling(&self, first: u32, room: u32) -> Result<Tiling, HResult> {
        let l = &self.texture.planes[0].layout;
        if l.alignment != TILE {
            return Err(E_FAIL);
        }
        if l.thick {
            return Err(E_NOTIMPL);
        }
        let standard = l.first_tail_mip;
        let tail = standard < self.mips;
        let tail_tiles = match standard {
            0 => l.slice_size / TILE,
            s if s < self.mips => l.mips[s as usize - 1].offset / TILE,
            _ => 0,
        };
        let slices = if tail { l.slices.min(2) } else { l.slices };
        let wanted = (standard * slices).saturating_sub(first).min(room);
        let subresources = (0..wanted)
            .map(|i| {
                let (slice, mip) = ((first + i) / standard, (first + i) % standard);
                let m = &l.mips[mip as usize];
                if tail && slice > 0 {
                    SubresourceTiling::default()
                } else {
                    SubresourceTiling {
                        width: m.pitch / l.block[0],
                        height: (m.rows / l.block[1]) as u16,
                        depth: 1,
                        start: ((u64::from(slice) * l.slice_size + m.offset) / TILE) as u32,
                    }
                }
            })
            .collect();
        Ok(Tiling {
            tiles: (l.size / TILE) as u32,
            packed: PackedMips {
                standard: standard as u8,
                packed: (self.mips - standard) as u8,
                tiles: tail_tiles as u32,
                start: 0,
            },
            shape: [l.block[0] * l.desc.block, l.block[1] * l.desc.block, l.block[2]],
            subresources,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(width: u64, height: u32, mips: u16, format: u32, layout: u32) -> ResourceDesc {
        ResourceDesc {
            dimension: TEXTURE_2D,
            alignment: 0,
            width,
            height,
            depth_or_array: 1,
            mips,
            format,
            samples: 1,
            quality: 0,
            layout,
            flags: 0,
        }
    }

    #[test]
    fn slots_the_title_calls() {
        let called: Vec<u32> = SLOTS.iter().filter(|s| s.title_calls).map(|s| s.offset).collect();
        assert_eq!(called, [0x08, 0x10, 0x30, 0x40, 0x48, 0x50]);
        assert!(SLOTS.iter().enumerate().all(|(i, s)| s.offset == 8 * i as u32));
    }

    #[test]
    fn suggestions_follow_the_use() {
        let mut d = desc(256, 256, 1, 98, 0);
        assert_eq!(suggest_swizzle_mode(&d), 0x105);
        d.flags = ALLOW_RENDER_TARGET;
        assert_eq!(suggest_swizzle_mode(&d), 0x11b);
        d.flags = ALLOW_DEPTH_STENCIL;
        assert_eq!(suggest_swizzle_mode(&d), 0x118);
        d.dimension = TEXTURE_1D;
        assert_eq!(suggest_swizzle_mode(&d), 0x100);
    }

    /// The fork's numbers for a 256x256 BC7 chain in 64 KiB `D` (layout
    /// 0x10a): two 64 KiB blocks, mip 0 after the tail, mip 1 32 KiB into
    /// it; copies round-trip through the subresource index; one tail tile.
    #[test]
    fn layout_copies_and_tiling_as_the_title_sees_them() {
        let c = Computer::new(&desc(256, 256, 0, 98, 0x10a)).unwrap();
        let mut l = LayoutOut::zeroed();
        c.layout(&mut l);
        assert_eq!((l.size, l.alignment, l.mips, l.planes), (131072, 65536, 9, 1));
        let p = &l.plane[0];
        assert_eq!((p.standard_mips, p.tail_mips, p.element_bytes), (1, 8, 16));
        assert_eq!((p.mips[0].offset, p.mips[0].pitch_bytes), (65536, 1024));
        assert_eq!((p.mips[1].offset, p.mips[1].in_tail), (32768, 1));

        let mut tiled = vec![0u8; l.size as usize];
        for mip in 0..9u32 {
            let m = &p.mips[mip as usize];
            let row = m.elements[0] * 16;
            let rows: Vec<u8> = (0..row * m.elements[1]).map(|i| (i ^ mip) as u8).collect();
            assert_eq!(c.copy_into(&mut tiled, 0, mip, &rows, row, 0), S_OK);
            let mut back = vec![0u8; rows.len()];
            assert_eq!(c.copy_from(&mut back, 0, mip, &tiled, row, 0), S_OK);
            assert_eq!(back, rows, "mip {mip}");
        }
        let t = c.tiling(0, 4).unwrap();
        assert_eq!((t.tiles, t.packed.standard, t.packed.packed, t.packed.tiles), (2, 1, 8, 1));
        assert_eq!((t.shape, t.subresources.len(), t.subresources[0].start), ([256, 256, 1], 1, 1));

        let c = Computer::new(&desc(256, 256, 0, 98, 0x105)).unwrap();
        assert_eq!(c.tiling(0, 4), Err(E_FAIL));
    }

    /// The title's CPU read through 0x30: a 128x4 `4KB_S` texture of 8-byte
    /// elements (title+0x5151c90 in the fork's notes).
    #[test]
    fn element_offsets_follow_the_layout() {
        let c = Computer::new(&desc(128, 4, 1, 11, 0x100)).unwrap();
        assert_eq!(c.element_offset(0, 0, 3, 1, 0), (128 + 3) * 8);
        assert_eq!(c.element_offset(0, 0, 128, 0, 0), 0);
        let c = Computer::new(&desc(128, 4, 1, 11, 0x105)).unwrap();
        let l = &c.texture.planes[0].layout;
        for y in 0..4 {
            for x in 0..128 {
                assert_eq!(c.element_offset(0, 0, u64::from(x), y, 0), l.offset(0, x, y, 0).unwrap());
            }
        }
    }

    #[test]
    fn bad_descriptions_fail() {
        assert_eq!(Computer::new(&desc(256, 256, 1, 0, 0x10a)).unwrap_err(), E_INVALIDARG);
        assert_eq!(Computer::new(&desc(256, 256, 1, 28, 0x103)).unwrap_err(), E_INVALIDARG);
        assert_eq!(Computer::new(&desc(1 << 40, 4, 1, 28, 0x105)).unwrap_err(), E_INVALIDARG);
        let c = Computer::new(&desc(64, 64, 1, 28, 0x105)).unwrap();
        assert_eq!(c.copy_from(&mut [0; 16], 0, 0, &[0; 16], 256, 0), E_INVALIDARG);
        assert_eq!(c.copy_from(&mut [0; 16], 1, 0, &[0; 16], 256, 0), E_INVALIDARG);
    }
}
