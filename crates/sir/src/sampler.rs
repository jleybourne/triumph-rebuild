//! Immediate samplers: the console sampler words (`SQ_IMG_SAMP`, 4 dwords, GFX10) a shader builds from
//! constants, decoded into a D3D12 static sampler (`D3D12_STATIC_SAMPLER_DESC` values as plain numbers, so
//! this crate needs no Windows bindings; Build 5 puts them in the root signature at
//! `register(s{k}, space100)`).
//!
//! Field positions (GFX10):
//!
//! ```text
//! word 0   clamp x 2:0, y 5:3, z 8:6 · max aniso ratio 11:9 · depth compare func 14:12 · filter mode 30:29
//! word 1   min lod 11:0, max lod 23:12 (unsigned 4.8)
//! word 2   lod bias 13:0 (signed 5.8) · xy mag filter 21:20, xy min filter 23:22 · z filter 25:24 · mip filter 27:26
//! word 3   border color type 31:30
//! ```
//!
//! Where the host has no exact equal it takes the nearest and says so in [`Static::approx`].

/// A D3D12 static sampler, by value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Static {
    /// `D3D12_FILTER`
    pub filter: u32,
    /// `D3D12_TEXTURE_ADDRESS_MODE` for U, V, W
    pub address: [u32; 3],
    pub mip_lod_bias: f32,
    pub max_anisotropy: u32,
    /// `D3D12_COMPARISON_FUNC` (used only when the shader compares)
    pub comparison: u32,
    /// `D3D12_STATIC_BORDER_COLOR`
    pub border: u32,
    pub min_lod: f32,
    pub max_lod: f32,
    /// What was approximated, if anything.
    pub approx: Option<&'static str>,
}

fn bits(w: u32, lo: u32, n: u32) -> u32 {
    w >> lo & ((1 << n) - 1)
}

/// The host address mode for a console clamp mode, and whether it is exact.
fn address(clamp: u32) -> (u32, bool) {
    // D3D12: wrap 1, mirror 2, clamp 3, border 4, mirror once 5
    match clamp {
        0 => (1, true),  // wrap
        1 => (2, true),  // mirror
        2 => (3, true),  // clamp to the last texel
        3 => (5, true),  // mirror once, then clamp to the last texel
        4 => (4, false), // clamp half-way into the border: border
        5 => (5, false), // mirror once, half border
        6 => (4, true),  // clamp to the border
        _ => (5, false), // mirror once, then border
    }
}

pub fn decode(w: [u32; 4]) -> Static {
    let mut approx = None;
    let mut addr = [0u32; 3];
    for (k, a) in addr.iter_mut().enumerate() {
        let (m, exact) = address(bits(w[0], 3 * k as u32, 3));
        *a = m;
        if !exact {
            approx = Some("half-border or mirror-once-border clamp taken as the nearest host mode");
        }
    }
    let ratio = bits(w[0], 9, 3).min(4);
    let compare = bits(w[0], 12, 3);
    let reduction = match bits(w[0], 29, 2) {
        1 => 2, // minimum
        2 => 3, // maximum
        _ => 0, // standard (comparison is the shader's choice: SampleCmp)
    };
    let min_lod = bits(w[1], 0, 12) as f32 / 256.0;
    let mut max_lod = bits(w[1], 12, 12) as f32 / 256.0;
    let bias = ((bits(w[2], 0, 14) << 18) as i32 >> 18) as f32 / 256.0;
    let (mag, min, mip) = (bits(w[2], 20, 2), bits(w[2], 22, 2), bits(w[2], 26, 2));
    let aniso = mag >= 2 || min >= 2;
    // D3D12_FILTER: min << 4 | mag << 2 | mip (point 0, linear 1), reduction << 7; anisotropic 0x55
    let linear = |f: u32| u32::from(f == 1 || f == 3);
    let filter = if aniso {
        0x55 | reduction << 7
    } else {
        linear(min) << 4 | linear(mag) << 2 | u32::from(mip == 2) | reduction << 7
    };
    if mip == 0 {
        // no mipmapping: only the first level the LOD clamp allows
        max_lod = min_lod;
    }
    let border = match bits(w[3], 30, 2) {
        0 => 0, // transparent black
        1 => 1, // opaque black
        2 => 2, // opaque white
        _ => {
            approx = Some("border color from a register: opaque black until the register is known");
            1
        }
    };
    Static {
        filter,
        address: addr,
        mip_lod_bias: bias,
        max_anisotropy: if aniso { 1 << ratio } else { 1 },
        comparison: compare + 1,
        border,
        min_lod,
        max_lod,
        approx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(clamp: [u32; 3], ratio: u32, mag: u32, min: u32, mip: u32, min_lod: f32, max_lod: f32, bias: f32, border: u32) -> [u32; 4] {
        let w0 = clamp[0] | clamp[1] << 3 | clamp[2] << 6 | ratio << 9;
        let w1 = (min_lod * 256.0) as u32 | ((max_lod * 256.0) as u32) << 12;
        let w2 = ((bias * 256.0) as i32 as u32 & 0x3FFF) | mag << 20 | min << 22 | mip << 26;
        [w0, w1, w2, border << 30]
    }

    #[test]
    fn trilinear_wrap() {
        let s = decode(words([0, 0, 0], 0, 1, 1, 2, 0.0, 15.0, 0.0, 0));
        assert_eq!(s.filter, 0x15, "MIN_MAG_MIP_LINEAR");
        assert_eq!(s.address, [1, 1, 1]);
        assert_eq!((s.min_lod, s.max_lod, s.mip_lod_bias, s.max_anisotropy), (0.0, 15.0, 0.0, 1));
        assert_eq!(s.approx, None);
    }

    #[test]
    fn point_clamp_without_mips_keeps_one_level() {
        let s = decode(words([2, 2, 2], 0, 0, 0, 0, 0.0, 15.0, 0.0, 1));
        assert_eq!(s.filter, 0, "MIN_MAG_MIP_POINT");
        assert_eq!(s.address, [3, 3, 3]);
        assert_eq!(s.max_lod, 0.0);
        assert_eq!(s.border, 1);
    }

    #[test]
    fn anisotropic_16_with_negative_bias_and_mixed_modes() {
        let s = decode(words([0, 6, 1], 4, 3, 3, 2, 0.0, 13.5, -0.5, 2));
        assert_eq!(s.filter, 0x55);
        assert_eq!(s.max_anisotropy, 16);
        assert_eq!(s.address, [1, 4, 2]);
        assert_eq!(s.mip_lod_bias, -0.5);
        assert_eq!(s.max_lod, 13.5);
        assert_eq!(s.border, 2);
    }

    #[test]
    fn min_max_reduction_and_approximations_are_reported() {
        let mut w = words([4, 0, 0], 0, 1, 1, 1, 0.0, 1.0, 0.0, 3);
        w[0] |= 2 << 29;
        let s = decode(w);
        assert_eq!(s.filter, 3 << 7 | 0x14, "MAXIMUM_MIN_MAG_LINEAR_MIP_POINT");
        assert!(s.approx.is_some());
    }
}
