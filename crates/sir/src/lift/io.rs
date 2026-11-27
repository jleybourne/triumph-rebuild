//! Stage inputs and outputs: pixel shaders first.
//!
//! A pixel shader gets the VGPRs `SPI_PS_INPUT_ADDR` enables, in the hardware's fixed order (barycentric
//! pairs per mode, then position, front face, ancillary, coverage). `v_interp_p1` / `v_interp_p2` evaluate
//! one channel of an attribute from a barycentric pair; on the host the rasteriser interpolates, so both
//! read the interpolated input, with the HLSL interpolation modifier of the pair they use.
//! `v_interp_mov_f32` reads one vertex's value: a `nointerpolation` input. Exports carry 32-bit values
//! (the code is export-patched, `NativeProgram::patched_code`): `mrt<n>` is render target n and `mrtz` the
//! depth. A lane whose EXEC bit is clear at the export that ends the program is discarded.
//!
//! Inputs and outputs are named by the container's own signature (`ISG1`, `OSG1`), by register row, so a
//! host pipeline links stages by semantic exactly as the title's did.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rebuild_shader::gfx10::enc::{self, Decoded, Src};
use rebuild_shader::native::{ComputeIo, PsInput, VsInput, VsOutput};
use rebuild_shader::signature::Element;

use super::{Lifted, Lifter};

/// How a pixel input is interpolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Interp {
    PerspSample,
    PerspCenter,
    PerspCentroid,
    LinearSample,
    LinearCenter,
    LinearCentroid,
    Flat,
}

impl Interp {
    pub fn hlsl(self) -> &'static str {
        match self {
            Interp::PerspSample => "sample ",
            Interp::PerspCenter => "",
            Interp::PerspCentroid => "centroid ",
            Interp::LinearSample => "noperspective sample ",
            Interp::LinearCenter => "noperspective ",
            Interp::LinearCentroid => "noperspective centroid ",
            Interp::Flat => "nointerpolation ",
        }
    }
}

/// A pixel input VGPR's meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsVgpr {
    /// barycentric `index` (0 i, 1 j) of a mode
    Bary(Interp, u8),
    PullModel(u8),
    LineStipple,
    /// position channel 0-3
    Pos(u8),
    FrontFace,
    Ancillary,
    Coverage,
    PosFixed,
}

/// The input VGPRs, in order, for an `SPI_PS_INPUT_ADDR` value.
pub fn ps_vgprs(input_addr: u32) -> Vec<PsVgpr> {
    use Interp::*;
    let mut v = Vec::new();
    let on = |bit: u32| input_addr >> bit & 1 == 1;
    for (bit, mode) in [(0, PerspSample), (1, PerspCenter), (2, PerspCentroid)] {
        if on(bit) {
            v.extend([PsVgpr::Bary(mode, 0), PsVgpr::Bary(mode, 1)]);
        }
    }
    if on(3) {
        v.extend([PsVgpr::PullModel(0), PsVgpr::PullModel(1), PsVgpr::PullModel(2)]);
    }
    for (bit, mode) in [(4, LinearSample), (5, LinearCenter), (6, LinearCentroid)] {
        if on(bit) {
            v.extend([PsVgpr::Bary(mode, 0), PsVgpr::Bary(mode, 1)]);
        }
    }
    if on(7) {
        v.push(PsVgpr::LineStipple);
    }
    for c in 0..4 {
        if on(8 + c) {
            v.push(PsVgpr::Pos(c as u8));
        }
    }
    for (bit, x) in [(12, PsVgpr::FrontFace), (13, PsVgpr::Ancillary), (14, PsVgpr::Coverage), (15, PsVgpr::PosFixed)] {
        if on(bit) {
            v.push(x);
        }
    }
    v
}

/// What the lifter needs to know about a pixel shader.
pub struct PsStage<'a> {
    pub inputs: &'a [PsInput],
    pub input_addr: u32,
}

/// What the lifter needs to know about a vertex shader (one that runs as the hardware VS).
pub struct VsStage<'a> {
    /// The attributes the fetch subroutine loads: channel k of each into its VGPR + k.
    pub inputs: &'a [VsInput],
    /// `exp param<n>`: the output row each parameter is.
    pub outputs: &'a [VsOutput],
}

/// Which stage the lifted program is (its inputs and outputs).
#[derive(Default)]
pub enum Stage<'a> {
    #[default]
    None,
    Ps(PsStage<'a>),
    Vs(VsStage<'a>),
    /// A compute program: thread ids in v0-v2, group ids in the SGPRs after the user data, LDS.
    Cs(ComputeIo),
}

/// What the stage's inputs and outputs need, gathered while lifting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StageUse {
    /// pixel input row → (channels read, interpolation)
    pub inputs: BTreeMap<u8, (u8, Interp)>,
    /// export target → channels written (pixel: 0-7 mrt, 8 mrtz; vertex: 12 pos0, 32 + n param n)
    pub exports: BTreeMap<u8, u8>,
    /// vertex: attribute rows the fetch loads (row → channels)
    pub attributes: BTreeMap<u8, u8>,
    /// lanes inactive at the final export are discarded
    pub discard: bool,
    /// the same input row read with two interpolation modes (not translated)
    pub conflicts: usize,
}

impl Lifter<'_> {
    pub(super) fn interp(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let Stage::Ps(ps) = &self.stage else { return None };
        let (attr, chan, vdst) = (d.get("attr")? as u8, d.get("attrchan")? as u8, d.get("vdst")? as u16);
        let input = ps.inputs.iter().find(|i| i.attr == attr)?;
        // a flat 16-bit attribute, one per dword: the dword holds its f16 bits (the high half 0)
        let flat16 = input.fp16 && !input.pack2 && name == "v_interp_mov_f32";
        if (input.fp16 || input.pack2) && !flat16 {
            return None; // interpolated or packed 16-bit attributes: next step
        }
        let mode = match name {
            "v_interp_mov_f32" => Interp::Flat,
            "v_interp_p1_f32" | "v_interp_p2_f32" => {
                let vsrc = d.get("vsrc")? as usize;
                match ps_vgprs(ps.input_addr).get(vsrc)? {
                    PsVgpr::Bary(m, _) => *m,
                    _ => return None,
                }
            }
            _ => return None,
        };
        let e = self.out.io.inputs.entry(input.row).or_insert((0, mode));
        if e.1 != mode {
            self.out.io.conflicts += 1;
            return None;
        }
        e.0 |= 1 << chan;
        // resolved against the signature when the program is assembled (rows can hold several elements)
        let expr = if flat16 { format!("sir_f16(asfloat(@in{}.{chan}@))", input.row) } else { format!("@in{}.{chan}@", input.row) };
        // p1's partial result only ever feeds p2 on the same register, so both may write the value
        self.vwrite_raw(vdst, expr);
        Some(())
    }

    /// 16-bit interpolation (VOP3): `src0` packs the attribute (bits 0-5), channel (6-7) and the high half of a
    /// packed fp16 pair (8); `src1` is the barycentric VGPR; p2's `src2` is p1's partial result. The host
    /// interpolates the element in float and the result is rounded to fp16 in the low half of `vdst`.
    pub(super) fn interp16(&mut self, d: &Decoded) -> Option<()> {
        let Stage::Ps(ps) = &self.stage else { return None };
        if d.get("opsel")? != 0 {
            self.why = Some("op_sel".into());
            return None;
        }
        let packed = d.get("src0")?;
        let (attr, chan, high) = ((packed & 0x3f) as u8, (packed >> 6 & 3) as u8, packed >> 8 & 1 == 1);
        let input = ps.inputs.iter().find(|i| i.attr == attr)?;
        // components are packed two per dword: channel `chan`'s half `high` is component 2 * chan + high, the
        // first four in `row`, the rest in `row2` (the same row for a single-row input)
        let c = 2 * chan + u8::from(high);
        let (row, chan) = if c < 4 { (input.row, c) } else { (if input.pack2 { input.row2 } else { input.row }, c - 4) };
        let vsrc = match enc::src(d.get("src1")?) {
            Src::Vgpr(v) => v as usize,
            _ => return None,
        };
        let mode = match ps_vgprs(ps.input_addr).get(vsrc)? {
            PsVgpr::Bary(m, _) => *m,
            _ => return None,
        };
        let e = self.out.io.inputs.entry(row).or_insert((0, mode));
        if e.1 != mode {
            self.out.io.conflicts += 1;
            return None;
        }
        e.0 |= 1 << chan;
        let vdst = d.get("vdst")? as u16;
        self.out.notes.insert("fp16 interpolation done in float, rounded to fp16");
        // p1's partial result only feeds p2 on the same register, so both may write the value
        self.vwrite_raw(vdst, format!("(v{vdst} & 0xffff0000u) | f32tof16(asfloat(@in{row}.{chan}@))"));
        Some(())
    }

    /// `s_swappc_b64` in a vertex shader: the call into the fetch subroutine. It loads channel k of each
    /// attribute into its VGPR + k; on the host the input assembler hands them over, so the call becomes
    /// those loads. The return address it writes is never used as data.
    pub(super) fn fetch(&mut self, d: &Decoded) -> Option<()> {
        let Stage::Vs(vs) = &self.stage else { return None };
        let inputs: Vec<VsInput> = vs.inputs.to_vec();
        for i in inputs {
            *self.out.io.attributes.entry(i.row).or_default() |= i.mask;
            for c in 0..4u8 {
                if i.mask >> c & 1 == 1 {
                    self.vwrite_raw(u16::from(i.vgpr + c), format!("@vin{}.{c}@", i.row));
                }
            }
        }
        if let Src::Sgpr(s) = enc::src(d.get("sdst")?) {
            self.swrite(u32::from(s), "0u".into())?;
            self.swrite(u32::from(s) + 1, "0u".into())?;
        }
        Some(())
    }

    pub(super) fn export(&mut self, d: &Decoded) -> Option<()> {
        let target = d.get("target")? as u8;
        let (en, done) = (d.get("en")? as u8, d.get("done")? == 1);
        if d.get("compr")? == 1 {
            return None; // the export patch makes them 32-bit; a compressed one left is not expected
        }
        if let Stage::Vs(vs) = &self.stage {
            // pos0 (12) and the parameters (32 + n); pos1-3 (point size, clip distances) next
            if target != 12 && !(32..64).contains(&target) {
                self.why = Some(format!("export target {target}"));
                return None;
            }
            if target >= 32 && vs.outputs.is_empty() {
                // no output rows at all (a depth-only pass): nothing downstream reads the parameter
                return Some(());
            }
            if target >= 32 {
                let Some(_) = vs.outputs.iter().find(|o| o.param == target - 32).copied() else {
                    self.why = Some("a parameter no output names".into());
                    return None;
                };
                // a 16-bit parameter: its dwords are exported as they are and unpacked into the signature's
                // elements when the program is assembled ([`half_outputs`])
            }
            *self.out.io.exports.entry(target).or_default() |= en;
            for c in 0..4u8 {
                if en >> c & 1 == 1 {
                    let v = d.get(["vsrc0", "vsrc1", "vsrc2", "vsrc3"][c as usize])? as u16;
                    self.out.vgprs.insert(v);
                    let ch = ["x", "y", "z", "w"][c as usize];
                    self.line(format!("o{target}.{ch} = v{v};"));
                }
            }
            return Some(());
        }
        let Stage::Ps(_) = &self.stage else { return None };
        match target {
            0..=8 => {
                if done && self.f.narrowed {
                    self.out.io.discard = true;
                    self.line("if (!ex) discard;".into());
                }
                *self.out.io.exports.entry(target).or_default() |= en;
                for c in 0..4u8 {
                    if en >> c & 1 == 1 {
                        let v = d.get(["vsrc0", "vsrc1", "vsrc2", "vsrc3"][c as usize])? as u16;
                        self.out.vgprs.insert(v);
                        let ch = ["x", "y", "z", "w"][c as usize];
                        self.line(format!("o{target}.{ch} = v{v};"));
                    }
                }
                Some(())
            }
            9 => {
                // null: only the discard, for a program that exports nothing else
                if done && self.f.narrowed {
                    self.out.io.discard = true;
                    self.line("if (!ex) discard;".into());
                }
                Some(())
            }
            _ => None,
        }
    }
}

fn hlsl_type(comp: u32, n: usize) -> String {
    let base = match comp {
        1 | 4 => "uint",
        2 | 5 => "int",
        _ => "float",
    };
    if n == 1 { base.to_string() } else { format!("{base}{n}") }
}

fn as_type(comp: u32, e: &str) -> String {
    match comp {
        1 | 4 => e.to_string(),
        2 | 5 => format!("asint({e})"),
        _ => format!("asfloat({e})"),
    }
}

/// `static const uint sir_code[]`: the program's words, when it reads data embedded in its code.
fn code_table(l: &Lifted) -> String {
    if l.code.is_empty() {
        return String::new();
    }
    let words: Vec<String> = l.code.iter().map(|w| format!("{w}u")).collect();
    format!("static const uint sir_code[{}] = {{ {} }};\n", l.code.len(), words.join(", "))
}

/// A whole pixel shader: inputs and outputs named by the container's signatures, the system values the
/// input VGPRs carry, the lifted body. `Err` names what is missing.
pub fn ps_program(l: &Lifted, ps: &PsStage, bind: Option<&super::Bindings>, isg1: &[Element], osg1: &[Element]) -> Result<String, String> {
    if !l.unsupported.is_empty() {
        return Err(format!("untranslated: {}", l.unsupported[0].1));
    }
    // user SGPRs read before any write are root constants at b0; anything else read uninitialised is refused
    let mut user_init = String::new();
    for s in &l.live_in {
        // the primitive mask only steers the console's attribute interpolation (via M0); the host interpolates
        if bind.is_some_and(|b| b.user_dword(*s).is_none() && b.after_user() == *s) {
            let _ = writeln!(user_init, "    s{s} = 0u; // PRIM_MASK: interpolation is the host's");
            continue;
        }
        let k = bind.and_then(|b| b.user_dword(*s)).ok_or(format!("reads s{s}, which is no user data"))?;
        if bind.is_some_and(|b| b.is_heap_base(*s)) {
            let _ = writeln!(user_init, "    s{s} = 0u; // heap base: descriptors are indexed from the heap's start");
            continue;
        }
        let _ = writeln!(user_init, "    s{s} = sir_ud[{}].{};", k / 4, ["x", "y", "z", "w"][k as usize % 4]);
    }
    if l.io.conflicts > 0 {
        return Err("an input read with two interpolation modes".into());
    }
    let mut s = String::from(super::PRELUDE);
    s.push_str(&code_table(l));
    if l.wave != super::Wave::Agnostic {
        s.push_str(super::wave::PRELUDE);
    }
    for k in 0..l.immediate_samplers.len() {
        let ty = if l.immediate_compare.contains(&k) { "SamplerComparisonState" } else { "SamplerState" };
        let _ = writeln!(s, "{ty} sir_imm{k} : register(s{k}, space{});", super::IMMEDIATE_SAMPLER_SPACE);
    }
    if l.uses_shadow {
        let _ = writeln!(s, "ByteAddressBuffer sir_shadow : register(t0, space{});", super::SHADOW_SPACE);
    }
    if !user_init.is_empty() {
        s.push_str("cbuffer SirUserData : register(b0, space0) { uint4 sir_ud[16]; };\n");
    }
    // inputs: every element of the input signature, in its order, read or not, so the registers DXC assigns
    // are the console's (the stages link by signature); position and front face also when only a VGPR uses them
    let vg = ps_vgprs(ps.input_addr);
    s.push_str("struct In {\n");
    let mut body = l.body.clone();
    let pos_used = vg.iter().enumerate().any(|(k, v)| matches!(v, PsVgpr::Pos(_)) && l.vgprs.contains(&(k as u16)));
    let ff_used = vg.iter().enumerate().any(|(k, v)| matches!(v, PsVgpr::FrontFace) && l.vgprs.contains(&(k as u16)));
    let (mut pos_done, mut ff_done) = (false, false);
    let mut elements: Vec<&Element> = isg1.iter().collect();
    elements.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    for e in elements {
        match e.system {
            0 => {}
            1 => {
                s.push_str("    float4 pos : SV_Position;\n");
                pos_done = true;
                continue;
            }
            9 => {
                s.push_str("    bool ff : SV_IsFrontFace;\n");
                ff_done = true;
                continue;
            }
            _ => {
                // other system values (clip and cull distances, primitive id, array indices) by their own name
                // and type: declared so the registers match, read by no translated form yet
                let n = e.mask.count_ones() as usize;
                let _ = writeln!(s, "    {} sv{}_{} : {}{};", hlsl_type(e.comp, n), e.register, e.mask.trailing_zeros(), e.name, e.index);
                continue;
            }
        }
        let row = e.register as u8;
        let first = e.mask.trailing_zeros() as u8;
        let n = e.mask.count_ones() as usize;
        let mode = l.io.inputs.get(&row).map(|(_, m)| m.hlsl()).unwrap_or("");
        let _ = writeln!(s, "    {}{} r{row}_{first} : {}{};", mode, hlsl_type(e.comp, n), e.name, e.index);
        for c in 0..4u8 {
            if e.mask >> c & 1 == 1 {
                let field = if n == 1 { String::new() } else { format!(".{}", ["x", "y", "z", "w"][(c - first) as usize]) };
                body = body.replace(&format!("@in{row}.{c}@"), &format!("asuint(i.r{row}_{first}{field})"));
            }
        }
    }
    for (row, (chans, _)) in &l.io.inputs {
        // a channel no element has (the half of a packed fp16 pair that packed math reads alongside the one it
        // uses) reads 0, as a missing component does
        for c in 0..4u8 {
            if chans >> c & 1 == 1 {
                body = body.replace(&format!("@in{row}.{c}@"), "0u");
            }
        }
    }
    if pos_used && !pos_done {
        s.push_str("    float4 pos : SV_Position;\n");
    }
    if ff_used && !ff_done {
        s.push_str("    bool ff : SV_IsFrontFace;\n");
    }
    s.push_str("};\nstruct Out {\n");
    // outputs: every element of the output signature, in its order and with its mask (so registers and widths
    // are the console's); a target the program exports but the signature lacks is a full float4
    let mut returns = String::new();
    let chans = |mask: u8| -> String { (0..4).filter(|c| mask >> c & 1 == 1).map(|c| ["x", "y", "z", "w"][c as usize]).collect() };
    let mut declared = std::collections::BTreeSet::new();
    let mut outs: Vec<&Element> = osg1.iter().collect();
    outs.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    for e in outs {
        let n = e.mask.count_ones() as usize;
        match e.system {
            64 => {
                let t = e.index as u8;
                let _ = writeln!(s, "    {} t{t} : SV_Target{t};", hlsl_type(e.comp, n));
                if l.io.exports.contains_key(&t) {
                    let _ = writeln!(returns, "    r.t{t} = {};", as_type(e.comp, &format!("o{t}.{}", chans(e.mask))));
                }
                declared.insert(t);
            }
            65 => {
                s.push_str("    float depth : SV_Depth;\n");
                if l.io.exports.contains_key(&8) {
                    returns.push_str("    r.depth = asfloat(o8.x);\n");
                }
                declared.insert(8);
            }
            _ => {
                let _ = writeln!(s, "    {} sv{}_{} : {}{};", hlsl_type(e.comp, n), e.register, e.mask.trailing_zeros(), e.name, e.index);
            }
        }
    }
    for target in l.io.exports.keys().filter(|t| !declared.contains(t)) {
        if *target == 8 {
            s.push_str("    float depth : SV_Depth;\n");
            returns.push_str("    r.depth = asfloat(o8.x);\n");
            continue;
        }
        let _ = writeln!(s, "    float4 t{target} : SV_Target{target};");
        let _ = writeln!(returns, "    r.t{target} = asfloat(o{target});");
    }
    // no exports and no output signature (it writes through UAVs only): no output at all, so a bound target 0
    // is never written
    let no_out = l.io.exports.is_empty() && osg1.is_empty();
    if no_out {
        let k = s.rfind("struct Out {\n").expect("pushed above");
        s.truncate(k);
        s.push_str("void main(In i) {\n");
    } else {
        s.push_str("};\nOut main(In i) {\n");
    }
    s.push_str(&super::declarations(l));
    s.push_str(&user_init);
    for t in l.io.exports.keys() {
        let _ = writeln!(s, "    uint4 o{t} = 0;");
    }
    // the system values the input VGPRs carry
    for (k, v) in vg.iter().enumerate() {
        if !l.vgprs.contains(&(k as u16)) {
            continue;
        }
        let e = match v {
            PsVgpr::Pos(c) => format!("asuint(i.pos.{})", ["x", "y", "z", "w"][*c as usize]),
            PsVgpr::FrontFace => "i.ff ? 1u : 0u".to_string(),
            _ => continue, // barycentrics are consumed by v_interp; the rest are not used by v0 programs
        };
        let _ = writeln!(s, "    v{k} = {e};");
    }
    s.push_str(&body);
    if !no_out {
        s.push_str("    Out r = (Out)0;\n");
        s.push_str(&returns);
        s.push_str("    return r;\n");
    }
    s.push_str("}\n");
    Ok(s)
}

/// A whole vertex shader: attributes by the input signature (what the fetch loaded), the vertex and instance
/// ids (v0, v3), the position and the parameters by the output signature.
pub fn vs_program(l: &Lifted, vs: &VsStage, bind: Option<&super::Bindings>, isg1: &[Element], osg1: &[Element]) -> Result<String, String> {
    if !l.unsupported.is_empty() {
        return Err(format!("untranslated: {}", l.unsupported[0].1));
    }
    let mut user_init = String::new();
    for s in &l.live_in {
        let k = bind.and_then(|b| b.user_dword(*s)).ok_or(format!("reads s{s}, which is no user data"))?;
        if bind.is_some_and(|b| b.is_heap_base(*s)) {
            let _ = writeln!(user_init, "    s{s} = 0u; // heap base: descriptors are indexed from the heap's start");
            continue;
        }
        let _ = writeln!(user_init, "    s{s} = sir_ud[{}].{};", k / 4, ["x", "y", "z", "w"][k as usize % 4]);
    }
    let mut s = String::from(super::PRELUDE);
    s.push_str(&code_table(l));
    if l.wave != super::Wave::Agnostic {
        s.push_str(super::wave::PRELUDE);
    }
    for k in 0..l.immediate_samplers.len() {
        let ty = if l.immediate_compare.contains(&k) { "SamplerComparisonState" } else { "SamplerState" };
        let _ = writeln!(s, "{ty} sir_imm{k} : register(s{k}, space{});", super::IMMEDIATE_SAMPLER_SPACE);
    }
    if l.uses_shadow {
        let _ = writeln!(s, "ByteAddressBuffer sir_shadow : register(t0, space{});", super::SHADOW_SPACE);
    }
    if !user_init.is_empty() {
        s.push_str("cbuffer SirUserData : register(b0, space0) { uint4 sir_ud[16]; };\n");
    }
    let mut body = l.body.clone();
    s.push_str("struct In {\n");
    // the attributes the fetch loads, in signature order (the oracle declares no unfetched attribute)
    let mut ins: Vec<&Element> = isg1.iter().filter(|e| e.system == 0 && l.io.attributes.contains_key(&(e.register as u8))).collect();
    ins.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    for e in ins {
        let row = e.register as u8;
        let first = e.mask.trailing_zeros() as u8;
        let n = e.mask.count_ones() as usize;
        let _ = writeln!(s, "    {} a{row}_{first} : {}{};", hlsl_type(e.comp, n), e.name, e.index);
        for c in 0..4u8 {
            if e.mask >> c & 1 == 1 {
                let field = if n == 1 { String::new() } else { format!(".{}", ["x", "y", "z", "w"][(c - first) as usize]) };
                body = body.replace(&format!("@vin{row}.{c}@"), &format!("asuint(i.a{row}_{first}{field})"));
            }
        }
    }
    // a channel the format does not supply: the fetch's default (0, and 1.0 for w)
    for row in l.io.attributes.keys() {
        for c in 0..4u8 {
            body = body.replace(&format!("@vin{row}.{c}@"), if c == 3 { "0x3f800000u" } else { "0u" });
        }
    }
    let (vid, iid) = (l.vgprs.contains(&0), l.vgprs.contains(&3));
    if vid {
        s.push_str("    uint vid : SV_VertexID;\n");
    }
    if iid {
        s.push_str("    uint iid : SV_InstanceID;\n");
    }
    s.push_str("};\nstruct Out {\n");
    // outputs: every element of the output signature, in its order (position where the signature puts it), each
    // written from the parameter whose row it is; one the program does not export stays 0
    let mut returns = String::new();
    let mut rows_done = std::collections::BTreeSet::new();
    let mut pos_done = false;
    let mut outs: Vec<&Element> = osg1.iter().collect();
    outs.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    let halves = half_outputs(vs, osg1);
    for e in outs {
        let first = e.mask.trailing_zeros() as u8;
        let n = e.mask.count_ones() as usize;
        match e.system {
            1 => {
                s.push_str("    float4 pos : SV_Position;\n");
                if l.io.exports.contains_key(&12) {
                    returns.push_str("    r.pos = asfloat(o12);\n");
                }
                pos_done = true;
            }
            // an element carried in 16-bit halves of parameters
            0 if (first..first + n as u8).any(|c| halves.contains_key(&(e.register as u8, c))) => {
                let row = e.register as u8;
                let _ = writeln!(s, "    {} p{row}_{first} : {}{};", hlsl_type(e.comp, n), e.name, e.index);
                let parts: Vec<String> = (first..first + n as u8)
                    .map(|c| match halves.get(&(row, c)) {
                        Some(&(t, dw, high)) if l.io.exports.contains_key(&t) => {
                            rows_done.insert(t);
                            let x = format!("((o{t}.{} >> {}u) & 0xffffu)", ["x", "y", "z", "w"][dw as usize], if high { 16 } else { 0 });
                            match e.comp {
                                1 | 4 => x,
                                2 | 5 => format!("(int)((int){x} << 16) >> 16"),
                                _ => format!("f16tof32({x})"),
                            }
                        }
                        _ => "0".into(),
                    })
                    .collect();
                let ty = hlsl_type(e.comp, n);
                let _ = writeln!(returns, "    r.p{row}_{first} = {ty}({});", parts.join(", "));
            }
            0 => {
                let row = e.register as u8;
                let _ = writeln!(s, "    {} p{row}_{first} : {}{};", hlsl_type(e.comp, n), e.name, e.index);
                let target = vs.outputs.iter().find(|o| o.row == row).map(|o| 32 + o.param);
                if let Some(t) = target.filter(|t| l.io.exports.contains_key(t)) {
                    let chans: String = (0..4u8).filter(|c| e.mask >> c & 1 == 1).map(|c| ["x", "y", "z", "w"][c as usize]).collect();
                    let _ = writeln!(returns, "    r.p{row}_{first} = {};", as_type(e.comp, &format!("o{t}.{chans}")));
                    rows_done.insert(t);
                }
            }
            _ => {
                let _ = writeln!(s, "    {} sv{}_{first} : {}{};", hlsl_type(e.comp, n), e.register, e.name, e.index);
            }
        }
    }
    if l.io.exports.contains_key(&12) && !pos_done {
        s.push_str("    float4 pos : SV_Position;\n");
        returns.push_str("    r.pos = asfloat(o12);\n");
    }
    if let Some(t) = l.io.exports.keys().find(|t| **t >= 32 && !rows_done.contains(t)) {
        return Err(format!("parameter {} is in no output signature element", t - 32));
    }
    s.push_str("};\nOut main(In i) {\n");
    s.push_str(&super::declarations(l));
    s.push_str(&user_init);
    for t in l.io.exports.keys() {
        let _ = writeln!(s, "    uint4 o{t} = 0;");
    }
    if vid {
        s.push_str("    v0 = i.vid;\n");
    }
    if iid {
        s.push_str("    v3 = i.iid;\n");
    }
    s.push_str(&body);
    s.push_str("    Out r = (Out)0;\n");
    s.push_str(&returns);
    s.push_str("    return r;\n}\n");
    Ok(s)
}

/// Where each 16-bit output channel comes from: (row, column) of the output signature → (export target,
/// dword, high half). A 16-bit parameter without `pack2` carries one f16 per dword, in the low half. With
/// `pack2` (the oracle's rule, `victory-shader` `packed_layout`): its partner row is the next 16-bit row of
/// the signature that no output entry names, taken in row order; the parameter carries the leading element of
/// its row, then of its partner row, each in whole dwords (two halves a dword); an element sharing a register
/// behind another follows in the first parameter, in row order, that has whole dwords left.
fn half_outputs(vs: &VsStage, osg1: &[Element]) -> BTreeMap<(u8, u8), (u8, u8, bool)> {
    let mut out = BTreeMap::new();
    let half = |e: &Element| e.system == 0 && (e.min_precision != 0 || e.comp >= 4);
    let mut free: Vec<u8> = osg1.iter().filter(|e| half(e)).map(|e| e.register as u8).filter(|r| !vs.outputs.iter().any(|o| o.row == *r)).collect();
    free.sort_unstable();
    free.dedup();
    let mut fp16: Vec<_> = vs.outputs.iter().filter(|o| o.fp16).collect();
    fp16.sort_by_key(|o| o.row);
    let of_row = |r: u8| {
        let mut v: Vec<(u8, u8)> = osg1
            .iter()
            .filter(|e| e.system == 0 && e.register as u8 == r)
            .map(|e| (e.mask.trailing_zeros() as u8, e.mask.count_ones() as u8))
            .collect();
        v.sort_unstable();
        v
    };
    // per packed parameter: its target, the dwords used, (dword, row, column, columns) of its elements
    let mut layout: Vec<(u8, u8, Vec<(u8, u8, u8, u8)>)> = Vec::new();
    let mut behind = Vec::new();
    for o in fp16 {
        let t = 32 + o.param;
        if !o.pack2 {
            for c in 0..4 {
                out.insert((o.row, c), (t, c, false));
            }
            continue;
        }
        let b = free.iter().position(|&r| r > o.row).map(|i| free.remove(i));
        let (mut placed, mut at) = (Vec::new(), 0u8);
        if !of_row(o.row).is_empty() {
            for r in [Some(o.row), b].into_iter().flatten() {
                let of = of_row(r);
                let Some((lead, more)) = of.split_first() else { continue };
                placed.push((at, r, lead.0, lead.1));
                at += lead.1.div_ceil(2);
                behind.extend(more.iter().map(|e| (r, e.0, e.1)));
            }
        }
        layout.push((t, at, placed));
    }
    behind.sort_unstable();
    for (r, col, cols) in behind {
        let need = cols.div_ceil(2);
        if let Some(p) = layout.iter_mut().find(|p| p.1 + need <= 4) {
            p.2.push((p.1, r, col, cols));
            p.1 += need;
        }
    }
    for (t, _, placed) in layout {
        for (dword, r, col, cols) in placed {
            for k in 0..cols {
                let h = 2 * dword + k;
                out.insert((r, col + k), (t, h / 2, h % 2 == 1));
            }
        }
    }
    out
}

/// A whole compute program: `numthreads` and the LDS size from the dispatch settings, thread ids (v0-v2) and
/// group ids (the system SGPRs) as system values. It writes only through UAVs, so it returns nothing.
pub fn cs_program(l: &Lifted, cs: &ComputeIo, bind: Option<&super::Bindings>) -> Result<String, String> {
    if !l.unsupported.is_empty() {
        return Err(format!("untranslated: {}", l.unsupported[0].1));
    }
    let (ids, info) = cs.system_sgprs();
    let mut init = String::new();
    for s in &l.live_in {
        if let Some(k) = ids.iter().position(|x| *x == Some(*s)) {
            let _ = writeln!(init, "    s{s} = gid.{};", ["x", "y", "z"][k]);
            continue;
        }
        if info == Some(*s) {
            return Err("reads the thread-group info SGPR".into());
        }
        let k = bind.and_then(|b| b.user_dword(*s)).ok_or(format!("reads s{s}, which is no user data"))?;
        if bind.is_some_and(|b| b.is_heap_base(*s)) {
            let _ = writeln!(init, "    s{s} = 0u; // heap base: descriptors are indexed from the heap's start");
            continue;
        }
        let _ = writeln!(init, "    s{s} = sir_ud[{}].{};", k / 4, ["x", "y", "z", "w"][k as usize % 4]);
    }
    let mut s = String::from(super::PRELUDE);
    s.push_str(&code_table(l));
    if l.wave != super::Wave::Agnostic {
        s.push_str(super::wave::PRELUDE);
    }
    for k in 0..l.immediate_samplers.len() {
        let ty = if l.immediate_compare.contains(&k) { "SamplerComparisonState" } else { "SamplerState" };
        let _ = writeln!(s, "{ty} sir_imm{k} : register(s{k}, space{});", super::IMMEDIATE_SAMPLER_SPACE);
    }
    if l.uses_shadow {
        let _ = writeln!(s, "ByteAddressBuffer sir_shadow : register(t0, space{});", super::SHADOW_SPACE);
    }
    if init.contains("sir_ud") {
        s.push_str("cbuffer SirUserData : register(b0, space0) { uint4 sir_ud[16]; };\n");
    }
    if l.uses_lds {
        let _ = writeln!(s, "groupshared uint sir_lds[{}];", (cs.lds_bytes() / 4).max(1));
    }
    let entry = super::wave::cs_entry(l, cs);
    if let Some((attr, _)) = &entry {
        s.push_str(attr);
    } else {
        let [x, y, z] = cs.threads;
        let _ = writeln!(s, "[numthreads({}, {}, {})]", x.max(1), y.max(1), z.max(1));
    }
    s.push_str("void main(uint3 tid : SV_GroupThreadID, uint3 gid : SV_GroupID) {\n");
    s.push_str(&super::declarations(l));
    s.push_str(&init);
    if let Some((_, ids)) = &entry {
        s.push_str(ids);
    } else {
        for k in 0..cs.thread_id_vgprs().min(3) {
            if l.vgprs.contains(&u16::from(k)) {
                let _ = writeln!(s, "    v{k} = tid.{};", ["x", "y", "z"][k as usize]);
            }
        }
    }
    s.push_str(&l.body);
    s.push_str("}\n");
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_vgprs_follow_the_enable_bits_in_hardware_order() {
        // persp center (bit 1), linear centroid (bit 6), pos x and y (8, 9), front face (12)
        let v = ps_vgprs(1 << 1 | 1 << 6 | 1 << 8 | 1 << 9 | 1 << 12);
        assert_eq!(
            v,
            vec![
                PsVgpr::Bary(Interp::PerspCenter, 0),
                PsVgpr::Bary(Interp::PerspCenter, 1),
                PsVgpr::Bary(Interp::LinearCentroid, 0),
                PsVgpr::Bary(Interp::LinearCentroid, 1),
                PsVgpr::Pos(0),
                PsVgpr::Pos(1),
                PsVgpr::FrontFace,
            ]
        );
        assert_eq!(Interp::LinearCentroid.hlsl(), "noperspective centroid ");
    }

    #[test]
    fn a_program_reading_user_data_is_refused_not_zeroed() {
        let mut l = Lifted::default();
        l.live_in.insert(2);
        let ps = PsStage { inputs: &[], input_addr: 0 };
        let e = ps_program(&l, &ps, None, &[], &[]).unwrap_err();
        assert!(e.contains("s2, which is no user data"), "{e}");
    }

    #[test]
    fn sixteen_bit_outputs_follow_the_packed_layout() {
        use rebuild_shader::native::{Usage, VsOutput};
        let el = |register: u32, mask: u8, half: bool| Element {
            stream: 0,
            name: "TEXCOORD".into(),
            index: register,
            system: 0,
            comp: 3,
            register,
            mask,
            rw: 0,
            min_precision: u32::from(half),
        };
        // the oracle's pedestrian case: TEXCOORD0 (register 1, columns 0-1) with TEXCOORD7 behind it (columns
        // 2-3), TEXCOORD1 (register 2, the partner row no entry names); register 3 a plain 16-bit output
        let osg1 = [el(1, 0b0011, true), el(1, 0b1100, true), el(2, 0b1111, true), el(3, 0b0111, true), el(4, 0b1111, false)];
        let outputs = [
            VsOutput { usage: Usage::Generic, row: 1, param: 0, fp16: true, pack2: true },
            VsOutput { usage: Usage::Generic, row: 3, param: 1, fp16: true, pack2: false },
            VsOutput { usage: Usage::Generic, row: 4, param: 2, fp16: false, pack2: false },
        ];
        let vs = VsStage { inputs: &[], outputs: &outputs };
        let h = half_outputs(&vs, &osg1);
        // parameter 0: TEXCOORD0 in dword 0 (two halves), TEXCOORD1 in dwords 1-2, TEXCOORD7 behind in dword 3
        assert_eq!(h[&(1, 0)], (32, 0, false));
        assert_eq!(h[&(1, 1)], (32, 0, true));
        assert_eq!(h[&(2, 0)], (32, 1, false));
        assert_eq!(h[&(2, 3)], (32, 2, true));
        assert_eq!(h[&(1, 2)], (32, 3, false));
        assert_eq!(h[&(1, 3)], (32, 3, true));
        // one f16 per dword, in the low half
        assert_eq!(h[&(3, 2)], (33, 2, false));
        assert!(!h.contains_key(&(4, 0)));
    }
}
