//! Host root signatures, built from what the translated programs bind.
//!
//! Every root signature has the **same parameters**, so Build 5's per-draw code never branches on the
//! pipeline and parameter indices are constants:
//!
//! ```text
//! compute    0  root constants  b0 space0     user data (the console's user SGPRs)    ALL
//!            1  root SRV        t0 space101   the descriptor shadow table (decision 9) ALL
//! graphics   0  root constants  b0 space0     the vertex shader's user data            VERTEX
//!            1  root constants  b0 space0     the pixel shader's user data             PIXEL
//!            2  root SRV        t0 space101   the descriptor shadow table              ALL
//! flags      CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED, always
//!            (+ input layout, and no hull/domain/geometry/amplification/mesh root access, for graphics)
//! samplers   each stage's immediate samplers as static samplers s{k} space100, visible to that stage
//! ```
//!
//! Only the static samplers differ between root signatures. The fork left `SAMPLER_HEAP_DIRECTLY_INDEXED`
//! off beside static samplers and 311 pipelines were refused (`docs/research/d3d12x-device.md`, pitfall 9).

use rebuild_sir::lift::{IMMEDIATE_SAMPLER_SPACE, SHADOW_SPACE};
use rebuild_sir::sampler::{self, Static};

use crate::bindings::{res, Binding};
use crate::Stage;

/// The user-data dwords each stage's root constants carry. The translated programs read at most 15 (pixel
/// dword 14; `agents/pipelines.md`), and the runtime accepts root constants shorter than the 256-byte
/// `SirUserData` cbuffer the HLSL declares (measured by `pipelines check`).
pub const USER_DATA_DWORDS: u32 = 16;

/// `D3D12_ROOT_SIGNATURE_FLAGS`.
pub mod flags {
    pub const ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT: u32 = 0x1;
    pub const DENY_HULL_SHADER_ROOT_ACCESS: u32 = 0x4;
    pub const DENY_DOMAIN_SHADER_ROOT_ACCESS: u32 = 0x8;
    pub const DENY_GEOMETRY_SHADER_ROOT_ACCESS: u32 = 0x10;
    pub const DENY_AMPLIFICATION_SHADER_ROOT_ACCESS: u32 = 0x100;
    pub const DENY_MESH_SHADER_ROOT_ACCESS: u32 = 0x200;
    pub const CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED: u32 = 0x400;
    pub const SAMPLER_HEAP_DIRECTLY_INDEXED: u32 = 0x800;
    /// Always set (heap indexing, decision 5).
    pub const HEAP: u32 = CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED;
    pub const GRAPHICS: u32 = HEAP
        | ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
        | DENY_HULL_SHADER_ROOT_ACCESS
        | DENY_DOMAIN_SHADER_ROOT_ACCESS
        | DENY_GEOMETRY_SHADER_ROOT_ACCESS
        | DENY_AMPLIFICATION_SHADER_ROOT_ACCESS
        | DENY_MESH_SHADER_ROOT_ACCESS;
}

/// Root parameter indices, for the code that sets them.
pub mod param {
    pub const CS_USER_DATA: u32 = 0;
    pub const CS_SHADOW: u32 = 1;
    pub const VS_USER_DATA: u32 = 0;
    pub const PS_USER_DATA: u32 = 1;
    pub const GFX_SHADOW: u32 = 2;
}

/// `D3D12_SHADER_VISIBILITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    All = 0,
    Vertex = 1,
    Pixel = 5,
}

/// What one translated program needs from its root signature.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StageLayout {
    /// Reads user data (`b0, space0`).
    pub user_data: bool,
    /// Reads the descriptor shadow table (`t0, space101`).
    pub shadow: bool,
    /// Immediate sampler words, register `s{k}` at index k (up to the highest the program binds).
    pub samplers: Vec<[u32; 4]>,
    /// Bit k: sampler k is read by a comparison (`SampleCmp*`), so its static sampler takes a comparison
    /// filter (the host refuses a `SamplerComparisonState` bound to a plain sampler).
    pub compare: u32,
}

impl StageLayout {
    /// The layout of a program from its `PSV0` bindings and the immediate sampler words the lifter found
    /// (`compare`: bit k when sampler k is a comparison sampler). Any binding outside the layout above is
    /// refused with its name.
    pub fn from_bindings(bindings: &[Binding], immediate: &[[u32; 4]], compare: u32) -> Result<StageLayout, String> {
        let mut l = StageLayout::default();
        let mut samplers = 0usize;
        for b in bindings {
            match (b.kind, b.space, b.lower, b.upper) {
                (res::CBV, 0, 0, 0) => l.user_data = true,
                (_, SHADOW_SPACE, 0, 0) if b.is_srv() => l.shadow = true,
                (res::SAMPLER, IMMEDIATE_SAMPLER_SPACE, _, hi) if (hi as usize) < immediate.len() => samplers = samplers.max(hi as usize + 1),
                (res::SAMPLER, IMMEDIATE_SAMPLER_SPACE, _, _) => {
                    return Err(format!("{} has no immediate sampler words ({} known)", b.describe(), immediate.len()))
                }
                _ => return Err(format!("{} is outside the root layout", b.describe())),
            }
        }
        l.samplers = immediate[..samplers].to_vec();
        l.compare = if samplers >= 32 { compare } else { compare & ((1u32 << samplers) - 1) };
        Ok(l)
    }
}

/// One root parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    Constants { register: u32, space: u32, count: u32, vis: Visibility },
    /// A root SRV whose data may change while set (the shadow table is written as descriptors are).
    Srv { register: u32, space: u32, vis: Visibility },
}

/// One static sampler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sampler {
    pub desc: Static,
    pub register: u32,
    pub space: u32,
    pub vis: Visibility,
}

/// A root signature as plain data (version 1.1), serialized by `d3d::serialize`.
#[derive(Debug, Clone, PartialEq)]
pub struct RootDesc {
    pub params: Vec<Param>,
    pub samplers: Vec<Sampler>,
    pub flags: u32,
}

/// `D3D12_FILTER_REDUCTION_TYPE` bits of a filter: 1 is comparison.
const REDUCTION: u32 = 3 << 7;

fn samplers(l: &StageLayout, vis: Visibility) -> impl Iterator<Item = Sampler> + '_ {
    l.samplers.iter().enumerate().map(move |(k, w)| {
        let mut desc = sampler::decode(*w);
        if k < 32 && l.compare >> k & 1 != 0 {
            desc.filter = desc.filter & !REDUCTION | 1 << 7;
        }
        Sampler { desc, register: k as u32, space: IMMEDIATE_SAMPLER_SPACE, vis }
    })
}

impl RootDesc {
    /// The root signature of a compute program.
    pub fn compute(cs: &StageLayout, user_dwords: u32) -> RootDesc {
        RootDesc {
            params: vec![
                Param::Constants { register: 0, space: 0, count: user_dwords, vis: Visibility::All },
                Param::Srv { register: 0, space: SHADOW_SPACE, vis: Visibility::All },
            ],
            samplers: samplers(cs, Visibility::All).collect(),
            flags: flags::HEAP,
        }
    }

    /// The root signature of a vertex and pixel shader pair.
    pub fn graphics(vs: &StageLayout, ps: &StageLayout, user_dwords: u32) -> RootDesc {
        RootDesc {
            params: vec![
                Param::Constants { register: 0, space: 0, count: user_dwords, vis: Visibility::Vertex },
                Param::Constants { register: 0, space: 0, count: user_dwords, vis: Visibility::Pixel },
                Param::Srv { register: 0, space: SHADOW_SPACE, vis: Visibility::All },
            ],
            samplers: samplers(vs, Visibility::Vertex).chain(samplers(ps, Visibility::Pixel)).collect(),
            flags: flags::GRAPHICS,
        }
    }

    /// The root signature's size in dwords (the host's limit is 64).
    pub fn dwords(&self) -> u32 {
        self.params
            .iter()
            .map(|p| match p {
                Param::Constants { count, .. } => *count,
                Param::Srv { .. } => 2,
            })
            .sum()
    }

    /// What the host would refuse, checked before serializing: size, and sampler values out of range.
    pub fn check(&self) -> Result<(), String> {
        if self.dwords() > 64 {
            return Err(format!("{} dwords (limit 64)", self.dwords()));
        }
        if self.flags & flags::HEAP != flags::HEAP {
            return Err("heap-indexing flags missing".into());
        }
        for s in &self.samplers {
            let d = &s.desc;
            if d.min_lod > d.max_lod || !(1..=16).contains(&d.max_anisotropy) || !(1..=8).contains(&d.comparison) || d.border > 2 {
                return Err(format!("static sampler s{} space{}: {:?}", s.register, s.space, d));
            }
        }
        Ok(())
    }
}

/// The root signature a single program would have on its own (graphics programs paired with an empty other
/// stage): what "every program's root signature" is checked as.
pub fn single(stage: Stage, l: &StageLayout, user_dwords: u32) -> RootDesc {
    let none = StageLayout::default();
    match stage {
        Stage::Cs => RootDesc::compute(l, user_dwords),
        Stage::Vs => RootDesc::graphics(l, &none, user_dwords),
        Stage::Ps => RootDesc::graphics(&none, l, user_dwords),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRILINEAR_WRAP: [u32; 4] = [0, 15 << 20, 1 << 20 | 1 << 22 | 2 << 26, 0];

    fn b(kind: u32, space: u32, lower: u32, upper: u32) -> Binding {
        Binding { kind, space, lower, upper }
    }

    #[test]
    fn layout_from_bindings() {
        let l = StageLayout::from_bindings(
            &[b(res::CBV, 0, 0, 0), b(res::SRV_RAW, SHADOW_SPACE, 0, 0), b(res::SAMPLER, IMMEDIATE_SAMPLER_SPACE, 1, 1)],
            &[TRILINEAR_WRAP, TRILINEAR_WRAP, TRILINEAR_WRAP],
            0b110,
        )
        .unwrap();
        assert!(l.user_data && l.shadow);
        assert_eq!(l.samplers.len(), 2, "registers up to the highest bound, unbound third dropped");
        assert_eq!(l.compare, 0b10, "compare bits past the bound samplers dropped");
        assert_eq!(StageLayout::from_bindings(&[], &[TRILINEAR_WRAP], 1).unwrap(), StageLayout::default());
    }

    #[test]
    fn bindings_outside_the_layout_are_refused() {
        let e = StageLayout::from_bindings(&[b(res::UAV_RAW, 0, 0, 0)], &[], 0).unwrap_err();
        assert!(e.contains("uav u0 space0"), "{e}");
        assert!(StageLayout::from_bindings(&[b(res::CBV, 0, 1, 1)], &[], 0).is_err());
        assert!(StageLayout::from_bindings(&[b(res::SAMPLER, IMMEDIATE_SAMPLER_SPACE, 0, 1)], &[TRILINEAR_WRAP], 0).is_err());
    }

    #[test]
    fn compute_root_shape() {
        let l = StageLayout { user_data: true, shadow: false, samplers: vec![TRILINEAR_WRAP], compare: 0 };
        let r = RootDesc::compute(&l, USER_DATA_DWORDS);
        assert_eq!(r.params[param::CS_USER_DATA as usize], Param::Constants { register: 0, space: 0, count: 16, vis: Visibility::All });
        assert_eq!(r.params[param::CS_SHADOW as usize], Param::Srv { register: 0, space: 101, vis: Visibility::All });
        assert_eq!(r.flags & flags::HEAP, flags::HEAP);
        assert_eq!((r.samplers[0].register, r.samplers[0].space, r.samplers[0].desc.filter), (0, 100, 0x15));
        assert_eq!(r.dwords(), 18);
        r.check().unwrap();
    }

    #[test]
    fn graphics_root_shape_and_sampler_visibility() {
        let vs = StageLayout { user_data: true, shadow: true, samplers: vec![TRILINEAR_WRAP], compare: 0 };
        let ps = StageLayout { user_data: true, shadow: false, samplers: vec![TRILINEAR_WRAP, [2 | 2 << 3 | 2 << 6, 0, 0, 1 << 30]], compare: 0 };
        let r = RootDesc::graphics(&vs, &ps, USER_DATA_DWORDS);
        assert_eq!(r.dwords(), 34);
        assert!(r.flags & flags::ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT != 0 && r.flags & flags::HEAP == flags::HEAP);
        let s: Vec<_> = r.samplers.iter().map(|s| (s.register, s.vis)).collect();
        assert_eq!(s, [(0, Visibility::Vertex), (0, Visibility::Pixel), (1, Visibility::Pixel)]);
        assert_eq!(r.samplers[2].desc.address, [3, 3, 3]);
        r.check().unwrap();
        // the same parameters whatever the programs use
        assert_eq!(RootDesc::graphics(&StageLayout::default(), &StageLayout::default(), 16).params, r.params);
    }

    #[test]
    fn comparison_samplers_take_a_comparison_filter() {
        let aniso = [0, 0, 2 << 20 | 2 << 22, 0];
        let l = StageLayout { user_data: false, shadow: false, samplers: vec![TRILINEAR_WRAP, TRILINEAR_WRAP, aniso], compare: 0b101 };
        let r = RootDesc::compute(&l, USER_DATA_DWORDS);
        let f: Vec<u32> = r.samplers.iter().map(|s| s.desc.filter).collect();
        assert_eq!(f, [0x95, 0x15, 0xd5], "COMPARISON_MIN_MAG_MIP_LINEAR, untouched, COMPARISON_ANISOTROPIC");
        r.check().unwrap();
    }

    #[test]
    fn too_many_dwords_is_caught() {
        assert!(RootDesc::graphics(&StageLayout::default(), &StageLayout::default(), 32).check().is_err());
        assert!(single(Stage::Cs, &StageLayout::default(), 62).check().is_ok());
    }
}
