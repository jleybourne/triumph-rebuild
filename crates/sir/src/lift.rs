//! GFX10 → HLSL statements (`docs/design/shader-ir.md`, "Translation from GFX10").
//!
//! Each SGPR and VGPR the program touches becomes a local `uint` (`s12`, `v3`); DXC's `-O3` builds the SSA.
//! Lane masks (VCC, a compare's destination, EXEC) are per-lane booleans: each host lane is one console lane
//! and holds only its own bit. EXEC starts all-on; once an instruction narrows it, every VALU write is
//! guarded by the lane's bit (predication). This step covers scalar and vector ALU; memory, images,
//! interpolation and exports are named in [`Lifted::unsupported`] until they are built.
//!
//! Float semantics follow the console where HLSL differs: `v_mul_legacy_f32` (0 × anything is 0),
//! `v_cvt_pkrtz_f16_f32` (round toward zero), saturating float → int conversions, `v_sin/cos_f32` (input in
//! revolutions), VOP3 `abs`/`neg`/`clamp`/`omod`. Denormal and rounding modes set by `s_setreg` are not
//! modelled yet (named in `notes`). `v_fma_f32` / `v_fmac_f32` become `mad`, which DXIL lowers to a fused
//! multiply-add (HLSL's `fma` is double-only, and doubles are slow on consumer GPUs).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use rebuild_shader::gfx10::enc::{self, Decoded, Src};
use rebuild_shader::gfx10::{Encoding, Instruction};

/// One program's statements and what they use.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Lifted {
    pub body: String,
    pub sgprs: BTreeSet<u8>,
    pub vgprs: BTreeSet<u16>,
    /// SGPR pairs (by low index) that hold a lane mask, as per-lane booleans `m{n}`.
    pub masks: BTreeSet<u8>,
    /// (byte offset, opcode name) of every instruction not translated yet.
    pub unsupported: Vec<(usize, String)>,
    /// Things translated with a known approximation.
    pub notes: BTreeSet<&'static str>,
    /// EXEC was narrowed somewhere (VALU writes after that are predicated).
    pub exec_narrowed: bool,
}

/// Helper functions the statements call, emitted once per program.
pub const PRELUDE: &str = r#"
float sir_mul_legacy(float a, float b) { return (a == 0.0f || b == 0.0f) ? 0.0f : a * b; }
float sir_med3(float a, float b, float c) { return max(min(a, b), min(max(a, b), c)); }
int sir_med3i(int a, int b, int c) { return max(min(a, b), min(max(a, b), c)); }
uint sir_med3u(uint a, uint b, uint c) { return max(min(a, b), min(max(a, b), c)); }
// f32 -> f16 rounding toward zero (the console's v_cvt_pkrtz_f16_f32)
uint sir_f16_rtz(float x) {
    uint h = f32tof16(x);
    float back = f16tof32(h);
    if (!isnan(x) && !isinf(x) && abs(back) > abs(x)) { h -= 1; }
    return h & 0xffff;
}
uint sir_pkrtz(float a, float b) { return sir_f16_rtz(a) | (sir_f16_rtz(b) << 16); }
// float -> int, saturating, NaN -> 0 (as the console)
int sir_f2i(float x) { return isnan(x) ? 0 : (x >= 2147483647.0f ? 2147483647 : (x <= -2147483648.0f ? (int)0x80000000 : (int)x)); }
uint sir_f2u(float x) { return isnan(x) ? 0 : (x >= 4294967295.0f ? 0xffffffff : (x <= 0.0f ? 0 : (uint)x)); }
uint sir_bfe_u(uint v, uint off, uint w) { off &= 31; w &= 31; return w == 0 ? 0 : (v >> off) & ((1u << w) - 1); }
int sir_bfe_i(int v, uint off, uint w) { off &= 31; w &= 31; return w == 0 ? 0 : (v << (32 - off - w)) >> (32 - w); }
"#;

/// The float value of an inline constant code 240..248.
fn inline_float(code: u16) -> Option<&'static str> {
    Some(match code {
        240 => "0.5f",
        241 => "-0.5f",
        242 => "1.0f",
        243 => "-1.0f",
        244 => "2.0f",
        245 => "-2.0f",
        246 => "4.0f",
        247 => "-4.0f",
        248 => "0.15915494f",
        _ => return None,
    })
}

/// The bit pattern of an inline constant (integer 128..208, float 240..248) as a `uint` expression.
fn inline_bits(code: u16) -> String {
    match code {
        128..=192 => format!("{}u", code - 128),
        193..=208 => format!("{}u", (-(i32::from(code) - 192)) as u32),
        c => format!("asuint({})", inline_float(c).unwrap_or("0.0f")),
    }
}

struct Lifter {
    out: Lifted,
}

/// What an operand reads, as a `uint` expression; `None` if not translated yet.
impl Lifter {
    fn src(&mut self, code: u32, literal: Option<u32>) -> Option<String> {
        Some(match enc::src(code) {
            Src::Sgpr(n) => {
                self.out.sgprs.insert(n);
                format!("s{n}")
            }
            Src::Vgpr(n) => {
                self.out.vgprs.insert(u16::from(n));
                format!("v{n}")
            }
            Src::Inline(c) => inline_bits(c),
            Src::Literal => format!("{}u", literal?),
            Src::M0 => {
                self.out.sgprs.insert(124);
                "s124".into()
            }
            Src::Null => "0u".into(),
            _ => return None,
        })
    }

    /// A float operand, with VOP3 abs/neg for operand `i`.
    fn fsrc(&mut self, d: &Decoded, field: &str, i: u32) -> Option<String> {
        let code = d.get(field)?;
        let mut e = match enc::src(code) {
            Src::Inline(c) if (240..=248).contains(&c) => inline_float(c)?.to_string(),
            _ => format!("asfloat({})", self.src(code, d.literal)?),
        };
        if d.encoding == Encoding::Vop3 {
            if d.get("abs").is_some_and(|a| a >> i & 1 == 1) {
                e = format!("abs({e})");
            }
            if d.get("neg").is_some_and(|n| n >> i & 1 == 1) {
                e = format!("(-{e})");
            }
        }
        Some(e)
    }

    fn usrc(&mut self, d: &Decoded, field: &str) -> Option<String> {
        let code = d.get(field)?;
        self.src(code, d.literal)
    }

    /// The per-lane mask a VALU op reads: VCC for VOP2, an SGPR pair for VOP3 `src2`.
    fn mask_src(&mut self, d: &Decoded) -> Option<String> {
        let code = if d.encoding == Encoding::Vop3 { d.get("src2")? } else { 106 };
        match enc::src(code) {
            Src::VccLo => Some(self.mask_name(106)),
            Src::Sgpr(n) => Some(self.mask_name(n)),
            Src::ExecLo => Some("ex".into()),
            _ => None,
        }
    }

    fn mask_name(&mut self, n: u8) -> String {
        self.out.masks.insert(n);
        if n == 106 { "vcc".into() } else { format!("m{n}") }
    }

    /// Writes VGPR `vdst` with a float expression, applying VOP3 clamp and omod.
    fn vwrite_f(&mut self, d: &Decoded, expr: String) {
        let mut e = expr;
        if d.encoding == Encoding::Vop3 {
            match d.get("omod") {
                Some(1) => e = format!("({e}) * 2.0f"),
                Some(2) => e = format!("({e}) * 4.0f"),
                Some(3) => e = format!("({e}) * 0.5f"),
                _ => {}
            }
            if d.get("clamp") == Some(1) {
                e = format!("saturate({e})");
            }
        }
        self.vwrite(d, format!("asuint({e})"));
    }

    fn vwrite(&mut self, d: &Decoded, expr: String) {
        let v = d.get("vdst").unwrap_or(0) as u16;
        self.out.vgprs.insert(v);
        if self.out.exec_narrowed {
            let _ = writeln!(self.out.body, "    if (ex) v{v} = {expr};");
        } else {
            let _ = writeln!(self.out.body, "    v{v} = {expr};");
        }
    }

    fn swrite(&mut self, n: u32, expr: String) -> Option<()> {
        match enc::src(n) {
            Src::Sgpr(s) => {
                self.out.sgprs.insert(s);
                let _ = writeln!(self.out.body, "    s{s} = {expr};");
            }
            Src::M0 => {
                self.out.sgprs.insert(124);
                let _ = writeln!(self.out.body, "    s124 = {expr};");
            }
            Src::Null => {}
            _ => return None,
        }
        Some(())
    }

    fn line(&mut self, s: String) {
        self.out.body.push_str("    ");
        self.out.body.push_str(&s);
        self.out.body.push('\n');
    }

    fn inst(&mut self, i: &Instruction) -> Option<()> {
        let name = i.name()?;
        let d = enc::decode(i);
        match i.encoding {
            Encoding::Sopp => self.sopp(name),
            Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc | Encoding::Sopk => self.salu(name, &d),
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vop3 | Encoding::Vopc => {
                if d.extra.is_some() {
                    return None; // SDWA / DPP: not yet
                }
                self.valu(name, &d)
            }
            _ => None,
        }
    }

    fn sopp(&mut self, name: &str) -> Option<()> {
        match name {
            "s_endpgm" | "s_nop" | "s_clause" | "s_code_end" | "s_dcache_inv" | "s_ttracedata" => Some(()),
            n if n.starts_with("s_waitcnt") || n.starts_with("s_inst_prefetch") || n.starts_with("s_sethalt") => Some(()),
            _ => None,
        }
    }

    fn salu(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let sdst = d.get("sdst");
        let a = |l: &mut Self| l.usrc(d, "ssrc0");
        let b = |l: &mut Self| l.usrc(d, "ssrc1");
        match name {
            "s_mov_b32" => {
                let x = a(self)?;
                self.swrite(sdst?, x)
            }
            "s_movk_i32" => self.swrite(sdst?, format!("{}u", d.get("simm16")? as u16 as i16 as i32 as u32)),
            "s_mov_b64" => {
                // a lane mask copy (EXEC or a mask pair) or a plain 64-bit copy
                let (src, dst) = (d.get("ssrc0")?, sdst?);
                match (enc::src(src), enc::src(dst)) {
                    (Src::ExecLo, Src::Sgpr(n)) => {
                        let m = self.mask_name(n);
                        self.line(format!("{m} = ex;"));
                        Some(())
                    }
                    (Src::Sgpr(s), Src::ExecLo) if self.out.masks.contains(&s) => {
                        let m = self.mask_name(s);
                        self.out.exec_narrowed = true;
                        self.line(format!("ex = {m};"));
                        Some(())
                    }
                    (Src::Sgpr(s), Src::Sgpr(t)) => {
                        self.out.sgprs.extend([s, s + 1, t, t + 1]);
                        self.line(format!("s{t} = s{s}; s{} = s{};", t + 1, s + 1));
                        Some(())
                    }
                    _ => None,
                }
            }
            "s_add_u32" | "s_add_i32" | "s_sub_u32" | "s_sub_i32" | "s_mul_i32" | "s_and_b32" | "s_or_b32" | "s_xor_b32"
            | "s_andn2_b32" | "s_orn2_b32" | "s_lshl_b32" | "s_lshr_b32" | "s_ashr_i32" | "s_min_u32" | "s_max_u32"
            | "s_min_i32" | "s_max_i32" | "s_cselect_b32" | "s_bfe_u32" | "s_bfe_i32" | "s_lshl1_add_u32"
            | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32" | "s_pack_ll_b32_b16" => {
                let (x, y) = (a(self)?, b(self)?);
                let (e, scc) = match name {
                    "s_add_u32" => (format!("{x} + {y}"), Some(format!("({x} + {y}) < {x}"))),
                    "s_add_i32" => (format!("{x} + {y}"), None),
                    "s_sub_u32" => (format!("{x} - {y}"), Some(format!("{y} > {x}"))),
                    "s_sub_i32" => (format!("{x} - {y}"), None),
                    "s_mul_i32" => (format!("{x} * {y}"), None),
                    "s_and_b32" => (format!("{x} & {y}"), Some("@ != 0".into())),
                    "s_or_b32" => (format!("{x} | {y}"), Some("@ != 0".into())),
                    "s_xor_b32" => (format!("{x} ^ {y}"), Some("@ != 0".into())),
                    "s_andn2_b32" => (format!("{x} & ~{y}"), Some("@ != 0".into())),
                    "s_orn2_b32" => (format!("{x} | ~{y}"), Some("@ != 0".into())),
                    "s_lshl_b32" => (format!("{x} << ({y} & 31)"), Some("@ != 0".into())),
                    "s_lshr_b32" => (format!("{x} >> ({y} & 31)"), Some("@ != 0".into())),
                    "s_ashr_i32" => (format!("(uint)((int){x} >> ({y} & 31))"), Some("@ != 0".into())),
                    "s_min_u32" => (format!("min({x}, {y})"), Some(format!("{x} < {y}"))),
                    "s_max_u32" => (format!("max({x}, {y})"), Some(format!("{x} > {y}"))),
                    "s_min_i32" => (format!("(uint)min((int){x}, (int){y})"), Some(format!("(int){x} < (int){y}"))),
                    "s_max_i32" => (format!("(uint)max((int){x}, (int){y})"), Some(format!("(int){x} > (int){y}"))),
                    "s_cselect_b32" => (format!("scc ? {x} : {y}"), None),
                    "s_bfe_u32" => (format!("sir_bfe_u({x}, {y} & 31, ({y} >> 16) & 0x7f)"), Some("@ != 0".into())),
                    "s_bfe_i32" => (format!("(uint)sir_bfe_i((int){x}, {y} & 31, ({y} >> 16) & 0x7f)"), Some("@ != 0".into())),
                    "s_lshl1_add_u32" => (format!("({x} << 1) + {y}"), None),
                    "s_lshl2_add_u32" => (format!("({x} << 2) + {y}"), None),
                    "s_lshl3_add_u32" => (format!("({x} << 3) + {y}"), None),
                    "s_lshl4_add_u32" => (format!("({x} << 4) + {y}"), None),
                    "s_pack_ll_b32_b16" => (format!("({x} & 0xffff) | ({y} << 16)"), None),
                    _ => unreachable!(),
                };
                // SCC first (it reads the sources before the destination may overwrite one)
                self.line(format!("{{ uint r = {e};"));
                if let Some(s) = scc {
                    self.line(format!("  scc = {};", s.replace('@', "r")));
                }
                let dst = sdst?;
                self.swrite(dst, "r".into())?;
                self.line("}".into());
                Some(())
            }
            n if n.starts_with("s_cmp_") => {
                let (x, y) = (a(self)?, b(self)?);
                let (op, signed) = cmp_op(n.strip_prefix("s_cmp_")?)?;
                let e = if signed { format!("(int){x} {op} (int){y}") } else { format!("{x} {op} {y}") };
                self.line(format!("scc = {e};"));
                Some(())
            }
            "s_setreg_imm32_b32" | "s_setreg_b32" => {
                self.out.notes.insert("s_setreg (float mode) not modelled");
                Some(())
            }
            "s_and_b64" | "s_or_b64" | "s_andn2_b64" | "s_xor_b64" => self.mask_op(name, d),
            "s_and_saveexec_b64" => {
                let (src, dst) = (d.get("ssrc0")?, sdst?);
                let m = match enc::src(src) {
                    Src::VccLo => self.mask_name(106),
                    Src::Sgpr(n) if self.out.masks.contains(&n) => self.mask_name(n),
                    _ => return None,
                };
                let Src::Sgpr(t) = enc::src(dst) else { return None };
                let saved = self.mask_name(t);
                self.out.exec_narrowed = true;
                self.line(format!("{saved} = ex; ex = ex && {m};"));
                Some(())
            }
            _ => None,
        }
    }

    /// 64-bit SALU logic on lane masks: per-lane boolean logic.
    fn mask_op(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let m = |l: &mut Self, code: u32| -> Option<String> {
            match enc::src(code) {
                Src::VccLo => Some(l.mask_name(106)),
                Src::ExecLo => Some("ex".into()),
                Src::Sgpr(n) if l.out.masks.contains(&n) => Some(l.mask_name(n)),
                _ => None,
            }
        };
        let (x, y) = (m(self, d.get("ssrc0")?)?, m(self, d.get("ssrc1")?)?);
        let e = match name {
            "s_and_b64" => format!("{x} && {y}"),
            "s_or_b64" => format!("{x} || {y}"),
            "s_andn2_b64" => format!("{x} && !{y}"),
            "s_xor_b64" => format!("{x} != {y}"),
            _ => return None,
        };
        match enc::src(d.get("sdst")?) {
            Src::ExecLo => {
                self.out.exec_narrowed = true;
                self.line(format!("ex = {e};"));
            }
            Src::VccLo => self.line(format!("vcc = {e};")),
            Src::Sgpr(n) => {
                let t = self.mask_name(n);
                self.line(format!("{t} = {e};"));
            }
            _ => return None,
        }
        // SCC = the result is non-zero over the wave: a wave-wide test, not modelled in a straight line
        Some(())
    }

    fn valu(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let f0 = |l: &mut Self| l.fsrc(d, "src0", 0);
        let u0 = |l: &mut Self| l.usrc(d, "src0");
        // VOP2's second operand is `vsrc1` (a VGPR); VOP3's is `src1`
        let s1 = if d.encoding == Encoding::Vop3 { "src1" } else { "vsrc1" };
        let f1 = |l: &mut Self| -> Option<String> {
            if d.encoding == Encoding::Vop3 {
                l.fsrc(d, "src1", 1)
            } else {
                let v = d.get("vsrc1")? as u16;
                l.out.vgprs.insert(v);
                Some(format!("asfloat(v{v})"))
            }
        };
        let u1 = |l: &mut Self| -> Option<String> {
            if d.encoding == Encoding::Vop3 {
                l.usrc(d, "src1")
            } else {
                let v = d.get(s1)? as u16;
                l.out.vgprs.insert(v);
                Some(format!("v{v}"))
            }
        };
        let f2 = |l: &mut Self| l.fsrc(d, "src2", 2);
        let u2 = |l: &mut Self| l.usrc(d, "src2");
        let vd = |l: &mut Self| -> String {
            let v = d.get("vdst").unwrap_or(0) as u16;
            l.out.vgprs.insert(v);
            format!("asfloat(v{v})")
        };
        let k = || -> Option<String> { Some(format!("asfloat({}u)", d.literal?)) };

        if let Some(rest) = name.strip_prefix("v_cmp_").or_else(|| name.strip_prefix("v_cmpx_")) {
            return self.vcmp(name.starts_with("v_cmpx_"), rest, d);
        }
        match name {
            "v_nop" => Some(()),
            "v_mov_b32" => {
                let x = u0(self)?;
                self.vwrite(d, x);
                Some(())
            }
            // float, one operand
            "v_floor_f32" | "v_ceil_f32" | "v_trunc_f32" | "v_rndne_f32" | "v_fract_f32" | "v_sqrt_f32" | "v_rsq_f32"
            | "v_rcp_f32" | "v_exp_f32" | "v_log_f32" | "v_sin_f32" | "v_cos_f32" => {
                let x = f0(self)?;
                let e = match name {
                    "v_floor_f32" => format!("floor({x})"),
                    "v_ceil_f32" => format!("ceil({x})"),
                    "v_trunc_f32" => format!("trunc({x})"),
                    "v_rndne_f32" => format!("round({x})"),
                    "v_fract_f32" => format!("frac({x})"),
                    "v_sqrt_f32" => format!("sqrt({x})"),
                    "v_rsq_f32" => format!("rsqrt({x})"),
                    "v_rcp_f32" => format!("rcp({x})"),
                    "v_exp_f32" => format!("exp2({x})"),
                    "v_log_f32" => format!("log2({x})"),
                    "v_sin_f32" => format!("sin(({x}) * 6.2831853f)"),
                    "v_cos_f32" => format!("cos(({x}) * 6.2831853f)"),
                    _ => unreachable!(),
                };
                self.vwrite_f(d, e);
                Some(())
            }
            // float, two operands
            "v_add_f32" | "v_sub_f32" | "v_subrev_f32" | "v_mul_f32" | "v_mul_legacy_f32" | "v_max_f32" | "v_min_f32"
            | "v_ldexp_f32" => {
                let (x, y) = (f0(self)?, f1(self)?);
                let e = match name {
                    "v_add_f32" => format!("{x} + {y}"),
                    "v_sub_f32" => format!("{x} - {y}"),
                    "v_subrev_f32" => format!("{y} - {x}"),
                    "v_mul_f32" => format!("{x} * {y}"),
                    "v_mul_legacy_f32" => format!("sir_mul_legacy({x}, {y})"),
                    "v_max_f32" => format!("max({x}, {y})"),
                    "v_min_f32" => format!("min({x}, {y})"),
                    "v_ldexp_f32" => format!("ldexp({x}, (float)(int)asuint({y}))"),
                    _ => unreachable!(),
                };
                self.vwrite_f(d, e);
                Some(())
            }
            "v_fmac_f32" | "v_mac_f32" | "v_mac_legacy_f32" | "v_fmac_legacy_f32" => {
                let (x, y) = (f0(self)?, f1(self)?);
                let acc = vd(self);
                let e = match name {
                    "v_fmac_f32" => format!("mad({x}, {y}, {acc})"),
                    "v_mac_f32" => format!("mad({x}, {y}, {acc})"),
                    _ => format!("sir_mul_legacy({x}, {y}) + {acc}"),
                };
                self.vwrite_f(d, e);
                Some(())
            }
            "v_madmk_f32" | "v_fmamk_f32" => {
                let (x, y, kk) = (f0(self)?, f1(self)?, k()?);
                self.vwrite_f(d, if name == "v_fmamk_f32" { format!("mad({x}, {kk}, {y})") } else { format!("mad({x}, {kk}, {y})") });
                Some(())
            }
            "v_madak_f32" | "v_fmaak_f32" => {
                let (x, y, kk) = (f0(self)?, f1(self)?, k()?);
                self.vwrite_f(d, if name == "v_fmaak_f32" { format!("mad({x}, {y}, {kk})") } else { format!("mad({x}, {y}, {kk})") });
                Some(())
            }
            "v_fma_f32" | "v_mad_f32" | "v_mad_legacy_f32" | "v_fma_legacy_f32" | "v_med3_f32" | "v_min3_f32" | "v_max3_f32" => {
                let (x, y, z) = (f0(self)?, f1(self)?, f2(self)?);
                let e = match name {
                    "v_fma_f32" => format!("mad({x}, {y}, {z})"),
                    "v_mad_f32" => format!("mad({x}, {y}, {z})"),
                    "v_mad_legacy_f32" | "v_fma_legacy_f32" => format!("sir_mul_legacy({x}, {y}) + {z}"),
                    "v_med3_f32" => format!("sir_med3({x}, {y}, {z})"),
                    "v_min3_f32" => format!("min(min({x}, {y}), {z})"),
                    "v_max3_f32" => format!("max(max({x}, {y}), {z})"),
                    _ => unreachable!(),
                };
                self.vwrite_f(d, e);
                Some(())
            }
            "v_cvt_pkrtz_f16_f32" => {
                let (x, y) = (f0(self)?, f1(self)?);
                self.vwrite(d, format!("sir_pkrtz({x}, {y})"));
                Some(())
            }
            "v_cvt_f32_u32" | "v_cvt_f32_i32" | "v_cvt_f32_ubyte0" | "v_cvt_f32_ubyte1" | "v_cvt_f32_ubyte2" | "v_cvt_f32_ubyte3"
            | "v_cvt_f32_f16" => {
                let x = u0(self)?;
                let e = match name {
                    "v_cvt_f32_u32" => format!("(float){x}"),
                    "v_cvt_f32_i32" => format!("(float)(int){x}"),
                    "v_cvt_f32_ubyte0" => format!("(float)({x} & 0xff)"),
                    "v_cvt_f32_ubyte1" => format!("(float)(({x} >> 8) & 0xff)"),
                    "v_cvt_f32_ubyte2" => format!("(float)(({x} >> 16) & 0xff)"),
                    "v_cvt_f32_ubyte3" => format!("(float)({x} >> 24)"),
                    "v_cvt_f32_f16" => format!("f16tof32({x})"),
                    _ => unreachable!(),
                };
                self.vwrite_f(d, e);
                Some(())
            }
            "v_cvt_i32_f32" | "v_cvt_u32_f32" | "v_cvt_f16_f32" | "v_cvt_flr_i32_f32" | "v_cvt_rpi_i32_f32" => {
                let x = f0(self)?;
                let e = match name {
                    "v_cvt_i32_f32" => format!("(uint)sir_f2i({x})"),
                    "v_cvt_u32_f32" => format!("sir_f2u({x})"),
                    "v_cvt_f16_f32" => format!("f32tof16({x})"),
                    "v_cvt_flr_i32_f32" => format!("(uint)sir_f2i(floor({x}))"),
                    "v_cvt_rpi_i32_f32" => format!("(uint)sir_f2i(floor({x} + 0.5f))"),
                    _ => unreachable!(),
                };
                self.vwrite(d, e);
                Some(())
            }
            // integer, two operands
            "v_add_nc_u32" | "v_sub_nc_u32" | "v_subrev_nc_u32" | "v_and_b32" | "v_or_b32" | "v_xor_b32" | "v_lshlrev_b32"
            | "v_lshrrev_b32" | "v_ashrrev_i32" | "v_mul_lo_u32" | "v_mul_hi_u32" | "v_mul_u32_u24" | "v_mul_i32_i24"
            | "v_min_u32" | "v_max_u32" | "v_min_i32" | "v_max_i32" | "v_bfm_b32" | "v_bcnt_u32_b32" | "v_cvt_pk_u16_u32"
            | "v_pack_b32_f16" | "v_cvt_pknorm_u16_f32" => {
                let (x, y) = (u0(self)?, u1(self)?);
                let e = match name {
                    "v_add_nc_u32" => format!("{x} + {y}"),
                    "v_sub_nc_u32" => format!("{x} - {y}"),
                    "v_subrev_nc_u32" => format!("{y} - {x}"),
                    "v_and_b32" => format!("{x} & {y}"),
                    "v_or_b32" => format!("{x} | {y}"),
                    "v_xor_b32" => format!("{x} ^ {y}"),
                    "v_lshlrev_b32" => format!("{y} << ({x} & 31)"),
                    "v_lshrrev_b32" => format!("{y} >> ({x} & 31)"),
                    "v_ashrrev_i32" => format!("(uint)((int){y} >> ({x} & 31))"),
                    "v_mul_lo_u32" => format!("{x} * {y}"),
                    "v_mul_hi_u32" => format!("(uint)(((uint64_t){x} * (uint64_t){y}) >> 32)"),
                    "v_mul_u32_u24" => format!("({x} & 0xffffff) * ({y} & 0xffffff)"),
                    "v_mul_i32_i24" => format!("(uint)((((int){x} << 8) >> 8) * (((int){y} << 8) >> 8))"),
                    "v_min_u32" => format!("min({x}, {y})"),
                    "v_max_u32" => format!("max({x}, {y})"),
                    "v_min_i32" => format!("(uint)min((int){x}, (int){y})"),
                    "v_max_i32" => format!("(uint)max((int){x}, (int){y})"),
                    "v_bfm_b32" => format!("((1u << ({x} & 31)) - 1) << ({y} & 31)"),
                    "v_bcnt_u32_b32" => format!("countbits({x}) + {y}"),
                    "v_cvt_pk_u16_u32" => format!("({x} & 0xffff) | ({y} << 16)"),
                    "v_pack_b32_f16" => format!("({x} & 0xffff) | ({y} << 16)"),
                    "v_cvt_pknorm_u16_f32" => format!(
                        "((uint)round(saturate(asfloat({x})) * 65535.0f)) | ((uint)round(saturate(asfloat({y})) * 65535.0f) << 16)"
                    ),
                    _ => unreachable!(),
                };
                self.vwrite(d, e);
                Some(())
            }
            // integer, three operands (VOP3)
            "v_lshl_add_u32" | "v_add_lshl_u32" | "v_add3_u32" | "v_lshl_or_b32" | "v_and_or_b32" | "v_or3_b32" | "v_xad_u32"
            | "v_bfe_u32" | "v_bfe_i32" | "v_bfi_b32" | "v_mad_u32_u24" | "v_mad_i32_i24" | "v_med3_u32" | "v_med3_i32"
            | "v_min3_u32" | "v_max3_u32" | "v_alignbit_b32" | "v_perm_b32" => {
                let (x, y, z) = (u0(self)?, u1(self)?, u2(self)?);
                let e = match name {
                    "v_lshl_add_u32" => format!("({x} << ({y} & 31)) + {z}"),
                    "v_add_lshl_u32" => format!("({x} + {y}) << ({z} & 31)"),
                    "v_add3_u32" => format!("{x} + {y} + {z}"),
                    "v_lshl_or_b32" => format!("({x} << ({y} & 31)) | {z}"),
                    "v_and_or_b32" => format!("({x} & {y}) | {z}"),
                    "v_or3_b32" => format!("{x} | {y} | {z}"),
                    "v_xad_u32" => format!("({x} ^ {y}) + {z}"),
                    "v_bfe_u32" => format!("sir_bfe_u({x}, {y}, {z})"),
                    "v_bfe_i32" => format!("(uint)sir_bfe_i((int){x}, {y}, {z})"),
                    "v_bfi_b32" => format!("({x} & {y}) | (~{x} & {z})"),
                    "v_mad_u32_u24" => format!("({x} & 0xffffff) * ({y} & 0xffffff) + {z}"),
                    "v_mad_i32_i24" => format!("(uint)((((int){x} << 8) >> 8) * (((int){y} << 8) >> 8) + (int){z})"),
                    "v_med3_u32" => format!("sir_med3u({x}, {y}, {z})"),
                    "v_med3_i32" => format!("(uint)sir_med3i((int){x}, (int){y}, (int){z})"),
                    "v_min3_u32" => format!("min(min({x}, {y}), {z})"),
                    "v_max3_u32" => format!("max(max({x}, {y}), {z})"),
                    "v_alignbit_b32" => format!("(uint)((((uint64_t){x} << 32) | (uint64_t){y}) >> ({z} & 31))"),
                    _ => return None, // v_perm_b32: byte select, next step
                };
                self.vwrite(d, e);
                Some(())
            }
            "v_cndmask_b32" => {
                let (x, y) = (u0(self)?, u1(self)?);
                let m = self.mask_src(d)?;
                self.vwrite(d, format!("{m} ? {y} : {x}"));
                Some(())
            }
            "v_readfirstlane_b32" => {
                // agnostic programs read a uniform value here (the classifier checked it)
                let x = u0(self)?;
                let dst = d.get("vdst")?;
                self.swrite(dst, format!("WaveReadLaneFirst({x})"))
            }
            _ => None,
        }
    }

    /// VOPC (to VCC) or VOP3 compare (to an SGPR pair): a per-lane boolean. `v_cmpx` also narrows EXEC.
    fn vcmp(&mut self, x_form: bool, rest: &str, d: &Decoded) -> Option<()> {
        let (op, ty) = rest.rsplit_once('_')?;
        let e = match ty {
            "f32" => {
                let a = self.fsrc(d, "src0", 0)?;
                let b = if d.encoding == Encoding::Vop3 {
                    self.fsrc(d, "src1", 1)?
                } else {
                    let v = d.get("vsrc1")? as u16;
                    self.out.vgprs.insert(v);
                    format!("asfloat(v{v})")
                };
                fcmp(op, &a, &b)?
            }
            "u32" | "i32" => {
                let a = self.usrc(d, "src0")?;
                let b = if d.encoding == Encoding::Vop3 {
                    self.usrc(d, "src1")?
                } else {
                    let v = d.get("vsrc1")? as u16;
                    self.out.vgprs.insert(v);
                    format!("v{v}")
                };
                let (o, _) = cmp_op(&format!("{op}_u32"))?;
                match (op, ty) {
                    ("t", _) => "true".into(),
                    ("f", _) => "false".into(),
                    (_, "i32") => format!("(int){a} {o} (int){b}"),
                    _ => format!("{a} {o} {b}"),
                }
            }
            _ => return None,
        };
        // the result is the lane's bit, and 0 in inactive lanes
        let e = if self.out.exec_narrowed { format!("ex && ({e})") } else { format!("({e})") };
        // GFX10: v_cmpx writes EXEC only; v_cmp writes VCC (VOPC) or an SGPR pair (VOP3)
        if x_form {
            self.out.exec_narrowed = true;
            self.line(format!("ex = {e};"));
            return Some(());
        }
        let dst = if d.encoding == Encoding::Vop3 { d.get("vdst")? } else { 106 };
        let target = match enc::src(dst) {
            Src::VccLo => self.mask_name(106),
            Src::Sgpr(n) => self.mask_name(n),
            _ => return None,
        };
        self.line(format!("{target} = {e};"));
        Some(())
    }
}

/// Integer compare suffix → (operator, signed).
fn cmp_op(s: &str) -> Option<(&'static str, bool)> {
    let (op, ty) = s.rsplit_once('_')?;
    let signed = ty.starts_with('i');
    Some((
        match op {
            "eq" => "==",
            "lg" | "ne" => "!=",
            "gt" => ">",
            "ge" => ">=",
            "lt" => "<",
            "le" => "<=",
            "t" | "f" => "==",
            _ => return None,
        },
        signed,
    ))
}

/// Float compares: ordered (false on NaN) unless the name says unordered (`n*`, `u`).
fn fcmp(op: &str, a: &str, b: &str) -> Option<String> {
    Some(match op {
        "f" => "false".into(),
        "t" | "tru" => "true".into(),
        "lt" => format!("{a} < {b}"),
        "eq" => format!("{a} == {b}"),
        "le" => format!("{a} <= {b}"),
        "gt" => format!("{a} > {b}"),
        "lg" => format!("({a} < {b} || {a} > {b})"),
        "ge" => format!("{a} >= {b}"),
        "o" => format!("(!isnan({a}) && !isnan({b}))"),
        "u" => format!("(isnan({a}) || isnan({b}))"),
        "nge" => format!("!({a} >= {b})"),
        "nlg" => format!("!({a} < {b} || {a} > {b})"),
        "ngt" => format!("!({a} > {b})"),
        "nle" => format!("!({a} <= {b})"),
        "neq" => format!("!({a} == {b})"),
        "nlt" => format!("!({a} < {b})"),
        _ => return None,
    })
}

/// Translates a straight-line program's instructions.
pub fn lift(insts: &[Instruction]) -> Lifted {
    let mut l = Lifter { out: Lifted::default() };
    for i in insts {
        if l.inst(i).is_none() {
            l.out.unsupported.push((i.offset, i.name().unwrap_or("?").to_string()));
            let _ = writeln!(l.out.body, "    // not translated: {}", i.name().unwrap_or("?"));
        }
    }
    l.out
}

/// The register declarations a lifted body needs (all registers start at 0; inputs are assigned after).
pub fn declarations(l: &Lifted) -> String {
    let mut s = String::new();
    for r in &l.sgprs {
        let _ = writeln!(s, "    uint s{r} = 0;");
    }
    for r in &l.vgprs {
        let _ = writeln!(s, "    uint v{r} = 0;");
    }
    for m in &l.masks {
        if *m == 106 {
            s.push_str("    bool vcc = false;\n");
        } else {
            let _ = writeln!(s, "    bool m{m} = false;");
        }
    }
    s.push_str("    bool ex = true;\n    bool scc = false;\n");
    s
}

#[cfg(test)]
mod tests;
