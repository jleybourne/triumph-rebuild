//! Rectangle lists (console topology 17), which the host has no topology for.
//!
//! A rectangle comes as three vertices, three corners of a screen-aligned rectangle; the console's hardware draws
//! the whole rectangle, the fourth corner continuing every vertex output linearly. The title draws its full-screen
//! passes this way (the loading screen's two rectangles: top left, bottom left, bottom right, the corner between
//! the other two second; one in clip space, one in pixels).
//!
//! Until the pipelines grow the geometry stage that does it on the GPU from the vertex shader's outputs (the
//! fork's `rect_list_gs`), a non-indexed rectangle draw whose vertices come from vertex buffers is expanded here
//! on the CPU, at record time: six vertices a rectangle, `(v0, v1, v2)` and the other half of the parallelogram,
//! the fourth vertex `a + b - corner` for every input component of a format this knows (float, half, unorm8,
//! unorm16), the first vertex's bytes for the rest. That equals the hardware's result for vertex shaders linear
//! in their inputs (blits, 2D quads), which is what these draws are. The corner is the vertex that shares x with
//! one of the others and y with the other, read from the position (the first element, at offset 0, of a float
//! format); without one it is the second vertex.

/// The console's rectangle-list topology.
pub const RECTLIST: i32 = 17;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    F32,
    F16,
    Unorm8,
    Unorm16,
}

impl Kind {
    fn bytes(self) -> usize {
        match self {
            Kind::F32 => 4,
            Kind::F16 | Kind::Unorm16 => 2,
            Kind::Unorm8 => 1,
        }
    }

    fn read(self, b: &[u8], c: usize) -> f32 {
        let at = c * self.bytes();
        match self {
            Kind::F32 => f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]),
            Kind::F16 => half_to_f32(u16::from_le_bytes([b[at], b[at + 1]])),
            Kind::Unorm8 => f32::from(b[at]) / 255.0,
            Kind::Unorm16 => f32::from(u16::from_le_bytes([b[at], b[at + 1]])) / 65535.0,
        }
    }

    fn write(self, b: &mut [u8], c: usize, v: f32) {
        let at = c * self.bytes();
        match self {
            Kind::F32 => b[at..at + 4].copy_from_slice(&v.to_le_bytes()),
            Kind::F16 => b[at..at + 2].copy_from_slice(&f32_to_half(v).to_le_bytes()),
            Kind::Unorm8 => b[at] = (v.clamp(0.0, 1.0) * 255.0).round() as u8,
            Kind::Unorm16 => b[at..at + 2].copy_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes()),
        }
    }
}

/// Components and kind of the vertex formats rectangles use (DXGI numbers).
fn components(format: u32) -> Option<(usize, Kind)> {
    Some(match format {
        2 => (4, Kind::F32),
        6 => (3, Kind::F32),
        16 => (2, Kind::F32),
        41 => (1, Kind::F32),
        10 => (4, Kind::F16),
        34 => (2, Kind::F16),
        54 => (1, Kind::F16),
        28 | 29 | 87 | 88 | 91 => (4, Kind::Unorm8),
        49 => (2, Kind::Unorm8),
        61 => (1, Kind::Unorm8),
        11 => (4, Kind::Unorm16),
        35 => (2, Kind::Unorm16),
        56 => (1, Kind::Unorm16),
        _ => return None,
    })
}

fn half_to_f32(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = i32::from((h >> 10) & 0x1f);
    let m = f32::from(h & 0x3ff);
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => {
            if m == 0.0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

fn f32_to_half(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32;
    let m = b & 0x7f_ffff;
    if e == 255 {
        return sign | 0x7c00 | if m != 0 { 0x200 } else { 0 };
    }
    let e = e - 127 + 15;
    if e >= 31 {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = (m | 0x80_0000) >> (1 - e);
        return sign | ((m + 0x1000) >> 13) as u16;
    }
    let h = (sign as u32) | ((e as u32) << 10) | (m >> 13);
    // round to nearest
    (h + ((m >> 12) & 1)) as u16
}

/// Which of three corners lies between the other two: the one sharing x with one and y with the other. `None`
/// when no vertex does (not a screen-aligned rectangle).
pub fn corner(p: [[f32; 2]; 3]) -> Option<usize> {
    (0..3).find(|&c| {
        let (a, b) = (p[(c + 1) % 3], p[(c + 2) % 3]);
        let q = p[c];
        (q[0] == a[0] && q[1] == b[1] && q[0] != b[0] && q[1] != a[1]) || (q[0] == b[0] && q[1] == a[1] && q[0] != a[0] && q[1] != b[1])
    })
}

/// Expand `src` (rectangles of three `stride`-byte vertices) into `dst` (six vertices each). `elements` are the
/// slot's per-vertex inputs as (byte offset, DXGI format); the position is the one at offset 0 if its format is
/// a float one. Returns the rectangles written.
pub fn expand(src: &[u8], dst: &mut [u8], stride: usize, elements: &[(u32, u32)]) -> usize {
    if stride == 0 {
        return 0;
    }
    let position = elements.iter().find(|e| e.0 == 0).and_then(|e| components(e.1)).filter(|(n, k)| *n >= 2 && matches!(k, Kind::F32 | Kind::F16));
    let mut n = 0;
    for (rect, out) in src.chunks_exact(3 * stride).zip(dst.chunks_exact_mut(6 * stride)) {
        let v = |i: usize| &rect[i * stride..(i + 1) * stride];
        let c = position
            .and_then(|(_, k)| {
                let xy = |i: usize| [k.read(v(i), 0), k.read(v(i), 1)];
                corner([xy(0), xy(1), xy(2)])
            })
            .unwrap_or(1);
        let (a, b) = ((c + 1) % 3, (c + 2) % 3);
        // (v0, v1, v2), then (a, fourth, b): the same turn as (c, a, b), which is a rotation of (v0, v1, v2)
        for (slot, i) in [0, 1, 2, a].into_iter().enumerate() {
            out[slot * stride..(slot + 1) * stride].copy_from_slice(v(i));
        }
        out[5 * stride..6 * stride].copy_from_slice(v(b));
        let fourth = &mut out[4 * stride..5 * stride];
        fourth.copy_from_slice(v(0));
        for &(offset, format) in elements {
            let at = offset as usize;
            let Some((count, kind)) = components(format) else { continue };
            // only the components inside the vertex
            let inside = stride.saturating_sub(at) / kind.bytes();
            for k in 0..count.min(inside) {
                let get = |i: usize| kind.read(&v(i)[at..], k);
                kind.write(&mut fourth[at..], k, get(a) + get(b) - get(c));
            }
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vertex(x: f32, y: f32, colour: [u8; 4], u: f32, v: f32) -> Vec<u8> {
        let mut b = Vec::new();
        for f in [x, y, 0.0] {
            b.extend_from_slice(&f.to_le_bytes());
        }
        b.extend_from_slice(&colour);
        for f in [u, v] {
            b.extend_from_slice(&f.to_le_bytes());
        }
        b
    }

    fn read(out: &[u8], i: usize) -> (f32, f32, [u8; 4], f32, f32) {
        let v = &out[i * 24..(i + 1) * 24];
        let f = |o: usize| f32::from_le_bytes(v[o..o + 4].try_into().unwrap());
        (f(0), f(4), v[12..16].try_into().unwrap(), f(16), f(20))
    }

    /// The loading screen's layout: float3 position, unorm8x4 colour, float2 uv.
    const LAYOUT: [(u32, u32); 3] = [(0, 6), (12, 28), (16, 16)];

    #[test]
    fn the_title_full_screen_rectangle_becomes_two_triangles_covering_it() {
        // top left, bottom left, bottom right, as the title gives it
        let src = [vertex(-1.0, 1.0, [255; 4], 0.0, 0.0), vertex(-1.0, -1.0, [255; 4], 0.0, 1.0), vertex(1.0, -1.0, [255; 4], 1.0, 1.0)].concat();
        let mut dst = vec![0u8; 6 * 24];
        assert_eq!(expand(&src, &mut dst, 24, &LAYOUT), 1);
        let got: Vec<_> = (0..6).map(|i| read(&dst, i)).collect();
        assert_eq!(got[0], (-1.0, 1.0, [255; 4], 0.0, 0.0));
        assert_eq!(got[1], (-1.0, -1.0, [255; 4], 0.0, 1.0));
        assert_eq!(got[2], (1.0, -1.0, [255; 4], 1.0, 1.0));
        // second half: bottom right, top right (the fourth corner), top left
        assert_eq!(got[3], (1.0, -1.0, [255; 4], 1.0, 1.0));
        assert_eq!(got[4], (1.0, 1.0, [255; 4], 1.0, 0.0), "the fourth corner continues position and uv");
        assert_eq!(got[5], (-1.0, 1.0, [255; 4], 0.0, 0.0));
        // both triangles turn the same way
        let turn = |p: [(f32, f32); 3]| (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
        let t = |i: usize| (got[i].0, got[i].1);
        assert!(turn([t(0), t(1), t(2)]) * turn([t(3), t(4), t(5)]) > 0.0);
    }

    #[test]
    fn the_corner_is_found_wherever_it_is() {
        assert_eq!(corner([[-1.0, 1.0], [-1.0, -1.0], [1.0, -1.0]]), Some(1));
        assert_eq!(corner([[0.0, 0.0], [0.0, 2160.0], [3840.0, 2160.0]]), Some(1));
        // tiled: top left, bottom left, top right (the corner first)
        assert_eq!(corner([[0.0, 0.0], [0.0, 8.0], [8.0, 0.0]]), Some(0));
        assert_eq!(corner([[0.0, 0.0], [1.0, 3.0], [5.0, 2.0]]), None);
        // corner first: the fourth is the opposite one
        let src = [vertex(0.0, 0.0, [0; 4], 0.0, 0.0), vertex(0.0, 8.0, [0; 4], 0.0, 1.0), vertex(8.0, 0.0, [0; 4], 1.0, 0.0)].concat();
        let mut dst = vec![0u8; 6 * 24];
        expand(&src, &mut dst, 24, &LAYOUT);
        assert_eq!(read(&dst, 4), (8.0, 8.0, [0; 4], 1.0, 1.0));
    }

    #[test]
    fn colours_continue_and_clamp_and_several_rectangles_expand() {
        let src = [
            vertex(0.0, 0.0, [0, 100, 255, 255], 0.0, 0.0),
            vertex(0.0, 1.0, [100, 100, 255, 255], 0.0, 1.0),
            vertex(1.0, 1.0, [200, 50, 0, 255], 1.0, 1.0),
            vertex(5.0, 5.0, [1; 4], 0.0, 0.0),
            vertex(5.0, 6.0, [1; 4], 0.0, 1.0),
            vertex(6.0, 6.0, [1; 4], 1.0, 1.0),
        ]
        .concat();
        let mut dst = vec![0u8; 12 * 24];
        assert_eq!(expand(&src, &mut dst, 24, &LAYOUT), 2);
        // fourth = v0 + v2 - v1: 0+200-100, 100+50-100, 255+0-255, 255
        assert_eq!(read(&dst, 4).2, [100, 50, 0, 255]);
        assert_eq!((read(&dst, 10).0, read(&dst, 10).1), (6.0, 5.0));
        // an unknown format keeps the first vertex's bytes
        let mut dst2 = vec![0u8; 6 * 24];
        expand(&src[..72], &mut dst2, 24, &[(0, 6), (12, 9999)]);
        assert_eq!(read(&dst2, 4).2, [0, 100, 255, 255]);
    }

    #[test]
    fn halves_round_trip() {
        for v in [0.0f32, 1.0, -2.0, 0.5, 1024.0, 0.000_06] {
            assert!((half_to_f32(f32_to_half(v)) - v).abs() <= v.abs() / 1000.0, "{v}");
        }
    }
}
