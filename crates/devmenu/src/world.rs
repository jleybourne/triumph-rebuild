//! The packed world position the title keeps in an entity's matrix (`entity + 0x70`, 16 bytes).
//!
//! Lanes: `f32 x_local, f32 y_local, f32 z, (i16 cell_x, i16 cell_y)`, with `world = local + cell * 32` on x and y.
//! Unpacking is `exe+0x4dc3790` (`vpunpckhwd`/`vpsrad`/`vcvtdq2ps`, then an FMA with `(32, 32, 0, 0)` from
//! `exe+0x6159c90`). Packing is inlined where the title rebuilds a matrix (`exe+0x3c25e0a..0x3c25e3b`):
//! `cell = trunc(pos / 32)` (`vroundps` mode 3), `local = pos - cell * 32`, cells saturated to `i16`
//! (`vpackssdw`) and inserted as lane 3.

/// The cell size in world units.
pub const CELL: f32 = 32.0;

/// A packed position as the title stores it (16 bytes, 16-aligned in entities).
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Packed {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub cell_x: i16,
    pub cell_y: i16,
}

/// A world position in the title's units (metres).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct World {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Packed {
    /// What `exe+0x4dc3790` computes (lane 3 of its output, which the title ignores, is dropped).
    pub fn unpack(self) -> World {
        World { x: self.cell_x as f32 * CELL + self.x, y: self.cell_y as f32 * CELL + self.y, z: self.z }
    }

    /// The inverse, as the title packs a matrix it rebuilds: truncation toward zero, `i16` saturation.
    pub fn pack(w: World) -> Packed {
        let cell = |v: f32| (v / CELL).trunc().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        let (cx, cy) = (cell(w.x), cell(w.y));
        Packed { x: w.x - cx as f32 * CELL, y: w.y - cy as f32 * CELL, z: w.z, cell_x: cx, cell_y: cy }
    }

    /// From the 16 bytes at `entity + 0x70`.
    pub fn from_bytes(b: [u8; 16]) -> Packed {
        let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let s = |i: usize| i16::from_le_bytes([b[i], b[i + 1]]);
        Packed { x: f(0), y: f(4), z: f(8), cell_x: s(12), cell_y: s(14) }
    }

    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.x.to_le_bytes());
        b[4..8].copy_from_slice(&self.y.to_le_bytes());
        b[8..12].copy_from_slice(&self.z.to_le_bytes());
        b[12..14].copy_from_slice(&self.cell_x.to_le_bytes());
        b[14..16].copy_from_slice(&self.cell_y.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for &(x, y, z) in &[(0.0, 0.0, 0.0), (123.5, -987.25, 40.0), (-31.9, 31.9, -2.0), (5000.0, -6000.0, 300.0)] {
            let p = Packed::pack(World { x, y, z });
            assert!(p.x.abs() < CELL && p.y.abs() < CELL, "local part stays inside one cell: {p:?}");
            let w = p.unpack();
            assert!((w.x - x).abs() < 1e-3 && (w.y - y).abs() < 1e-3 && w.z == z, "{x},{y} -> {w:?}");
        }
    }

    #[test]
    fn truncates_toward_zero_like_vroundps_mode_3() {
        let p = Packed::pack(World { x: -33.0, y: 33.0, z: 0.0 });
        assert_eq!((p.cell_x, p.cell_y), (-1, 1));
        assert_eq!((p.x, p.y), (-1.0, 1.0));
    }

    #[test]
    fn bytes_layout() {
        let p = Packed { x: 1.5, y: -2.0, z: 3.0, cell_x: -7, cell_y: 9 };
        let b = p.to_bytes();
        assert_eq!(&b[12..16], &[0xf9, 0xff, 0x09, 0x00]);
        assert_eq!(Packed::from_bytes(b), p);
        assert_eq!(p.unpack(), World { x: -7.0 * 32.0 + 1.5, y: 9.0 * 32.0 - 2.0, z: 3.0 });
    }
}
