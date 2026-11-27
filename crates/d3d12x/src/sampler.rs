//! Samplers: the PC description (0xb0) is the host's; the console's own (`CreateSamplerX`,
//! 0x1e8, 27 words) names the hardware's sampler fields and is turned into the host's here.
//!
//! Word positions of the console description are the fork's reading (`victory-gpu`
//! `sampler.rs`, facts only): 1 magnification, 2 minification (point 0, bilinear 1, anisotropic
//! point 2, anisotropic bilinear 3), 3 mip (none 0, point 1, linear 2), 5 reduction (plain 0,
//! minimum 1, maximum 2), 6-8 address modes (host numbering), 9 min LOD, 10 max LOD, 11 LOD bias
//! (floats), 13 anisotropy, 16 comparison (host numbering), 17-20 border colour (floats).

use windows::Win32::Graphics::Direct3D12::*;

/// Point sampling, clamp: what every host sampler slot starts as.
pub fn default_desc() -> D3D12_SAMPLER_DESC {
    D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    }
}

/// The console's 27-word description as the host's.
pub fn console(w: &[u32; 27]) -> D3D12_SAMPLER_DESC {
    let f = |i: usize| f32::from_bits(w[i]);
    let (mag, min, mip, mode) = (w[1], w[2], w[3], w[5]);
    let linear = |x: u32| u32::from(x == 1 || x == 3);
    let aniso = mag >= 2 || min >= 2;
    // a real test (less .. greater-equal) makes a comparison sampler; never / always do not
    let compare = w[16];
    let compares = (2..=7).contains(&compare);
    let reduction = match mode {
        1 => 2,
        2 => 3,
        _ => u32::from(compares),
    };
    let filter = if aniso { 0x55 } else { linear(min) << 4 | linear(mag) << 2 | u32::from(mip == 2) };
    let address = |x: u32| D3D12_TEXTURE_ADDRESS_MODE(x.clamp(1, 5) as i32);
    D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER((reduction << 7 | filter) as i32),
        AddressU: address(w[6]),
        AddressV: address(w[7]),
        AddressW: address(w[8]),
        MipLODBias: f(11),
        MaxAnisotropy: if aniso { w[13].clamp(1, 16) } else { 1 },
        ComparisonFunc: D3D12_COMPARISON_FUNC(compare.clamp(1, 8) as i32),
        BorderColor: [f(17), f(18), f(19), f(20)],
        MinLOD: f(9),
        // without a mip filter the console samples the first level only
        MaxLOD: if mip == 0 { f(9) } else { f(10) },
    }
}

/// A PC description with the fields the host checks made valid (a zeroed description from the
/// title would be refused: address mode 0, comparison 0).
pub fn sanitize(d: &D3D12_SAMPLER_DESC) -> D3D12_SAMPLER_DESC {
    let mut s = *d;
    let fix = |m: D3D12_TEXTURE_ADDRESS_MODE| D3D12_TEXTURE_ADDRESS_MODE(m.0.clamp(1, 5));
    s.AddressU = fix(s.AddressU);
    s.AddressV = fix(s.AddressV);
    s.AddressW = fix(s.AddressW);
    s.ComparisonFunc = D3D12_COMPARISON_FUNC(s.ComparisonFunc.0.clamp(1, 8));
    s.MaxAnisotropy = s.MaxAnisotropy.clamp(1, 16);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_descriptions_decode() {
        let mut w = [0u32; 27];
        w[1] = 1; // bilinear magnification
        w[3] = 2; // linear mips
        w[6..9].copy_from_slice(&[3, 3, 3]);
        w[10] = 1000.0f32.to_bits();
        let d = console(&w);
        assert_eq!(d.Filter.0, 0x5, "min point, mag and mip linear");
        assert_eq!(d.AddressW, D3D12_TEXTURE_ADDRESS_MODE_CLAMP);
        assert_eq!(d.MaxLOD, 1000.0);
        w[16] = 4;
        assert_eq!(console(&w).Filter.0, 0x85, "a less-equal test makes a comparison sampler");
        assert_eq!(console(&w).ComparisonFunc, D3D12_COMPARISON_FUNC_LESS_EQUAL);
        w[16] = 1;
        assert_eq!(console(&w).Filter.0, 0x5);
        w[1] = 3;
        w[13] = 8;
        assert_eq!((console(&w).Filter.0, console(&w).MaxAnisotropy), (0x55, 8));
    }

    #[test]
    fn zeroed_descriptions_become_valid() {
        let s = sanitize(&D3D12_SAMPLER_DESC::default());
        assert_eq!((s.AddressU.0, s.ComparisonFunc.0, s.MaxAnisotropy), (1, 1, 1));
    }
}
