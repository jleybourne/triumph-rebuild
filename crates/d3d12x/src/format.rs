//! Console descriptions as the title passes them, and what they become on the host.
//!
//! The console's `D3D12_RESOURCE_DESC` is the PC's 56 bytes with two differences: `Layout` is
//! `0x100 | swizzle mode` (1 row major, 0 "the library picks"), and the console adds formats the
//! PC lacks (116-120, 189-191). Sizes the title sees come from the console's own layout (lane
//! `xg`), because the title keeps them in its texture tables and asserts on disagreement; host
//! resources follow host rules.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub const KIB_64: u64 = 64 << 10;
pub const MIB_4: u64 = 4 << 20;

pub const DIMENSION_BUFFER: i32 = 1;
pub const DIMENSION_TEXTURE1D: i32 = 2;
pub const DIMENSION_TEXTURE2D: i32 = 3;
pub const DIMENSION_TEXTURE3D: i32 = 4;

/// `D3D12_HEAP_PROPERTIES` (20 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapProperties {
    pub kind: u32,
    pub cpu_pages: u32,
    pub pool: u32,
    pub creation_nodes: u32,
    pub visible_nodes: u32,
}

/// `D3D12_HEAP_DESC` (48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapDesc {
    pub size: u64,
    pub properties: HeapProperties,
    pub alignment: u64,
    pub flags: u32,
}

/// `D3D12_RESOURCE_ALLOCATION_INFO`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocationInfo {
    pub size: u64,
    pub alignment: u64,
}

/// `D3D12_RESOURCE_ALLOCATION_INFO1`: where one resource of a set goes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocationInfo1 {
    pub offset: u64,
    pub alignment: u64,
    pub size: u64,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);
const _: () = assert!(size_of::<HeapDesc>() == 48);
const _: () = assert!(size_of::<D3D12_RESOURCE_DESC>() == 56);
// the sampler feedback region sits at +52, in the plain description's tail padding: 64 bytes
// (the fork's 72-byte struct would misread the second of an array)
const _: () = assert!(size_of::<D3D12_RESOURCE_DESC1>() == 64);

/// Bytes per element and block width in texels, by DXGI number, with the console's additions
/// (116/117 10-bit float colour, 118-120 16-bit depth + 8-bit stencil, 189 signed 10-bit, 190
/// two 4-bit channels, 191 24/8 colour). `None` for formats with no fixed element.
pub fn element(format: u32) -> Option<(u64, u64)> {
    Some(match format {
        1..=4 => (16, 1),
        5..=8 => (12, 1),
        9..=22 => (8, 1),
        23..=47 | 67 | 87..=93 | 116 | 117 | 189 | 191 => (4, 1),
        118..=120 => (3, 1),
        48..=59 | 68 | 69 | 85 | 86 | 115 => (2, 1),
        60..=65 | 190 => (1, 1),
        70..=72 | 79..=81 => (8, 4),
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        _ => return None,
    })
}

/// A console format as the host has it: the 16-bit depth with stencil (no PC format) becomes
/// the 24-bit one; everything else keeps its number.
pub fn host_format(format: u32) -> u32 {
    match format {
        118 => 45,
        119 => 46,
        120 => 47,
        other => other,
    }
}

/// The typeless member of a format's family, so the host texture takes every view format the
/// console allows (SRGB views of UNORM targets, depth read as colour). `None`: keep the format.
pub fn typeless(format: u32) -> Option<u32> {
    Some(match format {
        1..=4 => 1,
        5..=8 => 5,
        9..=14 => 9,
        15..=18 => 15,
        19..=22 => 19,
        23..=25 => 23,
        27..=32 => 27,
        33..=38 => 33,
        39..=43 => 39,
        44..=47 => 44,
        48..=52 => 48,
        53..=59 => 53,
        60..=64 => 60,
        70..=72 => 70,
        73..=75 => 73,
        76..=78 => 76,
        79..=81 => 79,
        82..=84 => 82,
        87 | 90 | 91 => 90,
        88 | 92 | 93 => 92,
        94..=96 => 94,
        97..=99 => 97,
        _ => return None,
    })
}

/// The format a view of a depth texture reads (and a depth view writes) when the title names
/// none: depth formats, as views of their typeless host resource.
pub fn default_view_format(format: u32, depth_view: bool) -> u32 {
    let f = host_format(format);
    if depth_view {
        return f;
    }
    match f {
        40 => 41,  // D32_FLOAT -> R32_FLOAT
        45 => 46,  // D24S8 -> R24_UNORM_X8
        55 => 56,  // D16 -> R16_UNORM
        20 => 21,  // D32S8X24 -> R32_FLOAT_X8X24
        f => f,
    }
}

/// The host does not render to B5G5R5A1 (86); RGBA8 stands in (the fork's rule, timing-run L662).
pub const STAND_IN: (u32, u32) = (86, 28);

/// The console description as `rebuild-xg` takes it.
pub fn xg_desc(d: &D3D12_RESOURCE_DESC) -> rebuild_xg::ResourceDesc {
    rebuild_xg::ResourceDesc {
        dimension: d.Dimension.0 as u32,
        alignment: d.Alignment,
        width: d.Width,
        height: d.Height,
        depth_or_array: d.DepthOrArraySize,
        mips: d.MipLevels,
        format: d.Format.0 as u32,
        samples: d.SampleDesc.Count,
        quality: d.SampleDesc.Quality,
        layout: d.Layout.0 as u32,
        flags: d.Flags.0 as u32,
    }
}

/// Mips in the full chain of a description (0 = all).
pub fn mip_count(d: &D3D12_RESOURCE_DESC) -> u32 {
    if d.MipLevels != 0 {
        return u32::from(d.MipLevels);
    }
    let depth = if d.Dimension.0 == DIMENSION_TEXTURE3D { u64::from(d.DepthOrArraySize) } else { 1 };
    let largest = d.Width.max(u64::from(d.Height)).max(depth).max(1);
    64 - largest.leading_zeros()
}

/// Bytes and alignment a resource takes in console memory. Buffers: the width, 64 KiB aligned.
/// Textures: the console layout (`xg`); a texture `xg` refuses gets every mip of every slice and
/// sample packed (the fork's fallback), 64 KiB aligned (4 MiB with samples).
pub fn footprint(d: &D3D12_RESOURCE_DESC) -> Option<AllocationInfo> {
    if d.Dimension.0 != DIMENSION_BUFFER {
        if let Ok(c) = rebuild_xg::Computer::new(&xg_desc(d)) {
            let alignment = d.Alignment.max(c.texture.alignment);
            if !alignment.is_power_of_two() {
                return None;
            }
            return Some(AllocationInfo { size: c.texture.size.checked_next_multiple_of(alignment)?, alignment });
        }
    }
    let bytes = if d.Dimension.0 == DIMENSION_BUFFER {
        d.Width
    } else {
        let (element, block) = element(d.Format.0 as u32)?;
        let (depth, slices) =
            if d.Dimension.0 == DIMENSION_TEXTURE3D { (u64::from(d.DepthOrArraySize), 1) } else { (1, u64::from(d.DepthOrArraySize.max(1))) };
        let mut chain = 0u64;
        for mip in 0..mip_count(d).min(64) {
            let w = (d.Width >> mip).max(1).div_ceil(block);
            let h = (u64::from(d.Height) >> mip).max(1).div_ceil(block);
            let z = (depth >> mip).max(1);
            chain = chain.checked_add(w.checked_mul(h)?.checked_mul(z)?.checked_mul(element)?)?;
        }
        chain.checked_mul(slices)?.checked_mul(u64::from(d.SampleDesc.Count.max(1)))?
    };
    if bytes == 0 {
        return None;
    }
    let minimum = if d.SampleDesc.Count > 1 { MIB_4 } else { KIB_64 };
    let alignment = d.Alignment.max(minimum);
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo { size: bytes.checked_next_multiple_of(KIB_64)?, alignment })
}

/// Resources placed one after another: the total, and where each goes (`each`). An unknown size
/// gives the documented `u64::MAX`.
pub fn place(descs: &[D3D12_RESOURCE_DESC], mut each: impl FnMut(usize, AllocationInfo1)) -> AllocationInfo {
    // the set's alignment is its most-aligned member's (one resource: its own)
    let mut total = AllocationInfo { size: 0, alignment: if descs.is_empty() { KIB_64 } else { 1 } };
    for (i, d) in descs.iter().enumerate() {
        let Some(one) = footprint(d) else {
            each(i, AllocationInfo1 { offset: u64::MAX, alignment: 0, size: u64::MAX });
            return AllocationInfo { size: u64::MAX, alignment: 0 };
        };
        let offset = total.size.next_multiple_of(one.alignment);
        each(i, AllocationInfo1 { offset, alignment: one.alignment, size: one.size });
        total.size = offset + one.size;
        total.alignment = total.alignment.max(one.alignment);
    }
    total
}

/// What a console description becomes on the host: host format (typeless family for textures,
/// so every console view format works), host layout and alignment, flags the host knows.
/// Returns the host description and the typed format views default to.
pub fn host_desc(d: &D3D12_RESOURCE_DESC) -> (D3D12_RESOURCE_DESC, u32) {
    let mut h = *d;
    h.Alignment = 0;
    // render target, depth, unordered access, deny shader resource, cross adapter, simultaneous access
    h.Flags = D3D12_RESOURCE_FLAGS(d.Flags.0 & 0x3f);
    // combinations the console takes and the host refuses (the resource would not exist): depth with render
    // target, unordered access or simultaneous access; render target or depth on a buffer
    const RT: i32 = 0x1;
    const DS: i32 = 0x2;
    const UAV: i32 = 0x4;
    const SIMULTANEOUS: i32 = 0x20;
    if d.Dimension.0 == DIMENSION_BUFFER {
        h.Flags.0 &= !(RT | DS);
    } else if h.Flags.0 & DS != 0 {
        h.Flags.0 &= !(RT | UAV | SIMULTANEOUS);
    }
    if d.Dimension.0 == DIMENSION_BUFFER {
        h.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
        h.Format = DXGI_FORMAT_UNKNOWN;
        h.Height = 1;
        h.DepthOrArraySize = 1;
        h.MipLevels = 1;
        h.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
        return (h, 0);
    }
    h.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    let mut f = host_format(d.Format.0 as u32);
    if f == STAND_IN.0 && d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 != 0 {
        f = STAND_IN.1;
    }
    let view = f;
    h.Format = DXGI_FORMAT(typeless(f).unwrap_or(f) as i32);
    (h, view)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tex(w: u64, h: u32, format: u32, mips: u16, flags: i32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION(DIMENSION_TEXTURE2D),
            Width: w,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: DXGI_FORMAT(format as i32),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAGS(flags),
            ..Default::default()
        }
    }

    #[test]
    fn buffers_take_their_width_64k_aligned() {
        let d = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: 100, ..Default::default() };
        assert_eq!(footprint(&d), Some(AllocationInfo { size: KIB_64, alignment: KIB_64 }));
    }

    #[test]
    fn textures_take_the_console_layout() {
        let d = tex(256, 256, 28, 1, 0);
        let f = footprint(&d).unwrap();
        let c = rebuild_xg::Computer::new(&xg_desc(&d)).unwrap();
        assert_eq!(f.size, c.texture.size.next_multiple_of(f.alignment));
        assert!(f.size >= 256 * 256 * 4);
    }

    #[test]
    fn placing_several_aligns_each() {
        let b = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: 10, ..Default::default() };
        let mut at = Vec::new();
        let total = place(&[b, b], |i, a| at.push((i, a.offset)));
        assert_eq!(at, vec![(0, 0), (1, KIB_64)]);
        assert_eq!(total.size, 2 * KIB_64);
        let bad = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: 0, ..Default::default() };
        assert_eq!(place(&[b, bad], |_, _| ()).size, u64::MAX);
    }

    #[test]
    fn host_textures_are_typeless_and_keep_the_view_format() {
        let (h, view) = host_desc(&tex(64, 64, 29, 1, 1));
        assert_eq!((h.Format.0, view), (27, 29));
        let (h, view) = host_desc(&tex(64, 64, 86, 1, 1));
        assert_eq!((h.Format.0, view), (27, 28), "B5G5R5A1 render target: RGBA8 stands in");
        let (h, view) = host_desc(&tex(64, 64, 118, 1, 2));
        assert_eq!((h.Format.0, view), (44, 45), "console 16/8 depth: the 24/8 one");
        assert_eq!(default_view_format(40, false), 41);
        assert_eq!(default_view_format(40, true), 40);
        assert_eq!(h.Layout, D3D12_TEXTURE_LAYOUT_UNKNOWN);
    }
}
