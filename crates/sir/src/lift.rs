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

use std::collections::{BTreeMap, BTreeSet};
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
    /// What the stage's inputs and outputs need.
    pub io: io::StageUse,
    /// SGPRs read before any write: user data (root constants, descriptor addresses) the program needs as
    /// inputs. A program with any is not complete until root parameters are built.
    pub live_in: BTreeSet<u8>,
    /// A constant buffer is read through the heap (`SirCb`).
    pub uses_cb: bool,
    /// Immediate samplers: the console sampler words the code builds from constants, in register order
    /// (`sir_imm{k} : register(s{k}, space100)`); [`crate::sampler::decode`] gives the host's static sampler.
    pub immediate_samplers: Vec<[u32; 4]>,
    /// Immediate samplers a comparison reads (`SamplerComparisonState`; the static sampler's comparison
    /// function is in its words).
    pub immediate_compare: BTreeSet<usize>,
    /// The descriptor shadow table is read (`sir_shadow`, see [`SHADOW_SPACE`]).
    pub uses_shadow: bool,
    /// LDS is read or written (`groupshared uint sir_lds[]`).
    pub uses_lds: bool,
    /// The program's own words, when it reads data embedded in its code (`s_getpc_b64` and a load from
    /// there): `static const uint sir_code[]`, indexed by byte offset / 4.
    pub code: Vec<u32>,
}

/// The register space of the descriptor shadow table: per heap index, 16 bytes the runtime writes from the
/// console descriptor (stride, number of records, 0, 0), for what the host view does not carry.
pub const SHADOW_SPACE: u32 = 101;

/// Window addressing (`docs/design/shader-ir.md`, "Memory"): the console's 4 GiB window `k` (addresses
/// `k << 32 ..`) is a raw buffer view at host view-heap index `WINDOW_HEAP_BASE + k`, for `k <
/// WINDOW_COUNT` (47-bit addresses). The range is the top of a 1,000,000-view heap (the host limit on tiers
/// 1-2); the runtime keeps it for window views (a null view where no window is mapped).
pub const WINDOW_HEAP_BASE: u32 = 1_000_000 - WINDOW_COUNT;
/// Windows the heap range holds: every 4 GiB window of a 47-bit address space.
pub const WINDOW_COUNT: u32 = 1 << 15;

/// The register space of immediate samplers (static samplers in the host root signature).
pub const IMMEDIATE_SAMPLER_SPACE: u32 = 100;

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
// v_cmp_class_f32: is the float (as bits) in one of the classes the mask names
bool sir_class(uint x, uint m) {
    uint e = (x >> 23) & 0xff, f = x & 0x7fffff, neg = x >> 31;
    uint bit;
    if (e == 0xff) { bit = f != 0 ? ((f & 0x400000) != 0 ? 1u : 0u) : (neg != 0 ? 2u : 9u); }
    else if (e == 0) { bit = f == 0 ? (neg != 0 ? 5u : 6u) : (neg != 0 ? 4u : 7u); }
    else { bit = neg != 0 ? 3u : 8u; }
    return ((m >> bit) & 1) != 0;
}
// v_cubeid/sc/tc/ma_f32 as the ISA defines them: (face, sc, tc, 2 * major axis)
float4 sir_cube(float3 d) {
    float3 a = abs(d);
    if (a.z >= a.x && a.z >= a.y) { return float4(d.z < 0.0f ? 5.0f : 4.0f, d.z < 0.0f ? -d.x : d.x, -d.y, 2.0f * d.z); }
    if (a.y >= a.x) { return float4(d.y < 0.0f ? 3.0f : 2.0f, d.x, d.y < 0.0f ? -d.z : d.z, 2.0f * d.y); }
    return float4(d.x < 0.0f ? 1.0f : 0.0f, d.x < 0.0f ? d.z : -d.z, -d.y, 2.0f * d.x);
}
// the console's cube coordinates (s, t in [1, 2] after the +1.5 bias, face) back to a direction for TextureCube
float3 sir_cube_dir(float s, float t, float f) {
    float u = (s - 1.5f) * 2.0f, v = (t - 1.5f) * 2.0f;
    uint k = (uint)clamp(round(f), 0.0f, 5.0f);
    float3 d[6] = { float3(1, -v, -u), float3(-1, -v, u), float3(u, 1, v), float3(u, -1, -v), float3(u, -v, 1), float3(-u, -v, -1) };
    return d[k];
}
// float -> int, saturating, NaN -> 0 (as the console)
int sir_f2i(float x) { return isnan(x) ? 0 : (x >= 2147483647.0f ? 2147483647 : (x <= -2147483648.0f ? (int)0x80000000 : (int)x)); }
uint sir_f2u(float x) { return isnan(x) ? 0 : (x >= 4294967295.0f ? 0xffffffff : (x <= 0.0f ? 0 : (uint)x)); }
struct SirCb { uint4 d[4096]; };
uint sir_bfe_u(uint v, uint off, uint w) { off &= 31; w &= 31; return w == 0 ? 0 : (v >> off) & ((1u << w) - 1); }
int sir_bfe_i(int v, uint off, uint w) { off &= 31; w &= 31; return w == 0 ? 0 : (v << (32 - off - w)) >> (32 - w); }
// f32 to f16 bits, rounded to nearest even as the console's 16-bit ops round (a native conversion; HLSL's
// f32tof16 rounding is the device's: WARP rounds toward zero and clamps overflow)
uint sir_f16(float x) { return (uint)asuint16((float16_t)x); }
// the dword at byte address a (any alignment) from the aligned dwords lo and hi that hold it
uint sir_unaligned(uint lo, uint hi, uint a) { uint sh = (a & 3u) * 8u; return sh == 0u ? lo : (lo >> sh) | (hi << (32u - sh)); }
// a V#'s dst_sel: 0 zero, 1 one, 4-7 channel x-w
uint sir_dst_sel(uint4 c, uint sel, uint one) { return sel >= 4u ? c[sel - 4u] : (sel == 1u ? one : 0u); }
// v_perm_b32: each result byte picks a byte of {src0, src1} (0-7), a sign fill (8-11: bit 15, 31, 47, 63), 0 (12)
// or 0xff (13 and up)
uint sir_perm(uint a, uint b, uint sel) {
    uint r = 0;
    for (uint i = 0; i < 4; i++) {
        uint s = (sel >> (8 * i)) & 0xff, x;
        if (s < 4) { x = (b >> (8 * s)) & 0xff; }
        else if (s < 8) { x = (a >> (8 * (s - 4))) & 0xff; }
        else if (s < 12) { uint w = s < 10 ? b : a; x = ((w >> ((s & 1) != 0 ? 31 : 15)) & 1) != 0 ? 0xff : 0; }
        else { x = s == 12 ? 0 : 0xff; }
        r |= x << (8 * i);
    }
    return r;
}
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

/// The f16 bit pattern of an inline float constant 240..248 (what 16-bit operations read).
fn inline_f16(code: u16) -> u32 {
    match code {
        240 => 0x3800,
        241 => 0xB800,
        242 => 0x3C00,
        243 => 0xBC00,
        244 => 0x4000,
        245 => 0xC000,
        246 => 0x4400,
        247 => 0xC400,
        _ => 0x3118, // 1 / (2 pi)
    }
}

pub mod cfg;
pub mod check;
pub mod fmt;
pub mod io;
pub mod res;
pub use io::{cs_program, ps_program, vs_program, PsStage, Stage, VsStage};
pub use res::Bindings;

/// What the lifter knows at one point of the program. In a straight line it only grows; with control flow
/// ([`cfg`]) each block starts from the meet of its predecessors' states.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FlowState {
    /// SGPRs holding a loaded descriptor's heap index (its first dword)
    descs: BTreeMap<u8, res::Desc>,
    /// the other dwords of loaded descriptors (not data on the host)
    desc_words: BTreeSet<u8>,
    /// SGPRs whose value is derived from a descriptor table's offset (table's user-data dword)
    table_of: BTreeMap<u8, u8>,
    /// SGPRs holding a known constant (set by `s_mov_b32`/`s_movk_i32` from a literal or inline integer)
    consts: BTreeMap<u8, u32>,
    /// SGPRs written on every path here (a read of any other may read user data)
    written: BTreeSet<u8>,
    /// SGPRs written on some path here (any other still holds its user data)
    may_written: BTreeSet<u8>,
    /// SGPRs (by low index) whose lane mask `m{n}` is current
    mask_cur: BTreeSet<u8>,
    /// SGPRs whose data variable `s{n}` is stale: a lane mask was written there since
    stale: BTreeSet<u8>,
    /// EXEC may be narrower than at entry (VALU writes are predicated)
    narrowed: bool,
    /// SGPRs (the low half of a pair) holding a byte offset into the program's code: `s_getpc_b64` and
    /// values added to it
    code_of: BTreeSet<u8>,
}

impl FlowState {
    /// The state where paths join: only what holds on every incoming path.
    pub(crate) fn meet(&self, o: &FlowState) -> FlowState {
        fn same<K: Ord + Copy, V: PartialEq + Copy>(a: &BTreeMap<K, V>, b: &BTreeMap<K, V>) -> BTreeMap<K, V> {
            a.iter().filter(|(k, v)| b.get(k) == Some(v)).map(|(k, v)| (*k, *v)).collect()
        }
        FlowState {
            descs: same(&self.descs, &o.descs),
            desc_words: self.desc_words.union(&o.desc_words).copied().collect(),
            table_of: same(&self.table_of, &o.table_of),
            consts: same(&self.consts, &o.consts),
            written: self.written.intersection(&o.written).copied().collect(),
            may_written: self.may_written.union(&o.may_written).copied().collect(),
            mask_cur: self.mask_cur.intersection(&o.mask_cur).copied().collect(),
            stale: self.stale.union(&o.stale).copied().collect(),
            narrowed: self.narrowed || o.narrowed,
            code_of: self.code_of.intersection(&o.code_of).copied().collect(),
        }
    }
}

struct Lifter<'a> {
    out: Lifted,
    stage: Stage<'a>,
    bind: Option<Bindings<'a>>,
    f: FlowState,
    /// why the last instruction was refused, when a reason is known
    why: Option<String>,
    /// the program runs as wave64 on the console (lane masks are SGPR pairs and `_b64` ops)
    wave64: bool,
    /// SCC is read after the current instruction before anything writes it again
    scc_live: bool,
    /// inside a DPP instruction: src0 reads `sir_dpp`; VALU writes also need `sir_dv` when set
    dpp: Option<bool>,
    /// the current instruction keeps only bits 16 and up of its first operand (a shift right or field
    /// extract from bit 16): a descriptor's word 1 may be read as its stride
    hi16: bool,
    /// inside an SDWA instruction: the operands are `sir_a` / `sir_b` (selected and extended), and the op's
    /// result goes to `sir_r` (merged into the destination's selected bits afterwards)
    sdwa: bool,
    /// the program's words by byte offset / 4 (empty unless given: [`lift_program_code`]), for data embedded in
    /// the code
    code: Vec<u32>,
}

/// What an operand reads, as a `uint` expression; `None` if not translated yet.
impl Lifter<'_> {
    fn src(&mut self, code: u32, literal: Option<u32>) -> Option<String> {
        Some(match enc::src(code) {
            Src::Sgpr(n) => {
                if self.f.descs.contains_key(&n) || self.f.desc_words.contains(&n) {
                    // a buffer descriptor's stride (word 1, bits 16-29) and record count (word 2) are in the shadow
                    // table; word 1 only when the instruction keeps no bit below 16 (the address is not there)
                    if let Some((b, k)) = self.desc_word_of(n) {
                        if self.f.descs.get(&b).is_some_and(|d| d.heap == res::Heap::Resource) && (k == 2 || (k == 1 && self.hi16)) {
                            self.out.uses_shadow = true;
                            self.out.sgprs.insert(b);
                            return Some(if k == 2 {
                                format!("sir_shadow.Load(s{b} * 16u + 4u)")
                            } else {
                                format!("(sir_shadow.Load(s{b} * 16u) << 16)")
                            });
                        }
                    }
                    // a descriptor word read as data
                    let w = self.desc_word_of(n).map_or("?".to_string(), |(_, k)| k.to_string());
                    self.why.get_or_insert(format!("descriptor word {w} read as data"));
                    return None;
                }
                if self.f.stale.contains(&n) {
                    self.why = Some("a lane mask read as data".into());
                    return None;
                }
                self.out.sgprs.insert(n);
                if !self.f.written.contains(&n) {
                    self.out.live_in.insert(n);
                }
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
            // VCC as data: only after data was written to it on every path here
            Src::VccLo | Src::VccHi => {
                let n = if enc::src(code) == Src::VccLo { 106 } else { 107 };
                if !self.f.written.contains(&n) {
                    self.why = Some("a lane mask read as data".into());
                    return None;
                }
                self.out.sgprs.insert(n);
                format!("s{n}")
            }
            Src::Null => "0u".into(),
            _ => return None,
        })
    }

    /// The loaded descriptor SGPR `n` belongs to: (its first SGPR, the word's index).
    fn desc_word_of(&self, n: u8) -> Option<(u8, u8)> {
        self.f.descs.iter().filter(|(b, d)| **b <= n && n < **b + d.dwords).map(|(b, _)| (*b, n - *b)).next()
    }

    /// A float operand, with VOP3 abs/neg for operand `i`.
    fn fsrc(&mut self, d: &Decoded, field: &str, i: u32) -> Option<String> {
        if self.sdwa && (field == "src0" || field == "vsrc1") {
            let (x, k) = if field == "src0" { ("sir_a", "src0") } else { ("sir_b", "src1") };
            let mut e = format!("asfloat({x})");
            if d.get(&format!("{k}_abs")) == Some(1) {
                e = format!("abs({e})");
            }
            if d.get(&format!("{k}_neg")) == Some(1) {
                e = format!("(-{e})");
            }
            return Some(e);
        }
        if self.dpp.is_some() && field == "src0" {
            let mut e = "asfloat(sir_dpp)".to_string();
            if d.get("src0_abs") == Some(1) {
                e = format!("abs({e})");
            }
            if d.get("src0_neg") == Some(1) {
                e = format!("(-{e})");
            }
            return Some(e);
        }
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

    /// VOP3P. The mix forms: each source is f16 when its `opsel_hi` bit is set (the half `opsel` names) and
    /// f32 otherwise; `neg` negates, `neg_hi` takes the absolute value; the result is f32 (`v_fma_mix_f32`) or
    /// f16 into the low or high half (`v_fma_mixlo_f16`, `v_fma_mixhi_f16`).
    fn vop3p(&mut self, name: &str, d: &Decoded) -> Option<()> {
        if name.starts_with("v_pk_") {
            return self.packed(name, d);
        }
        if !matches!(name, "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16") {
            return None;
        }
        let (opsel, hi) = (d.get("opsel")?, d.get("opsel_hi")? | d.get("opsel_hi2")? << 2);
        let (neg, abs) = (d.get("neg")?, d.get("neg_hi")?);
        let mut srcs = Vec::new();
        for (i, field) in ["src0", "src1", "src2"].into_iter().enumerate() {
            let code = d.get(field)?;
            let mut e = match enc::src(code) {
                // an inline float is the same number in either width
                Src::Inline(c) if (240..=248).contains(&c) => inline_float(c)?.to_string(),
                _ => {
                    let u = self.src(code, d.literal)?;
                    if hi >> i & 1 == 1 {
                        format!("f16tof32(({u}) >> {}u)", 16 * (opsel >> i & 1))
                    } else {
                        format!("asfloat({u})")
                    }
                }
            };
            if abs >> i & 1 == 1 {
                e = format!("abs({e})");
            }
            if neg >> i & 1 == 1 {
                e = format!("(-{e})");
            }
            srcs.push(e);
        }
        let mut r = format!("mad({}, {}, {})", srcs[0], srcs[1], srcs[2]);
        if d.get("clamp")? == 1 {
            r = format!("saturate({r})");
        }
        let v = d.get("vdst")? as u16;
        let e = match name {
            "v_fma_mix_f32" => format!("asuint({r})"),
            "v_fma_mixlo_f16" => format!("(v{v} & 0xffff0000u) | f32tof16({r})"),
            _ => format!("(v{v} & 0xffffu) | (f32tof16({r}) << 16)"),
        };
        self.vwrite_raw(v, e);
        Some(())
    }

    /// Packed 16-bit ops (`v_pk_*`): each half computed on its own. Operand `i` of the low half is the half
    /// `op_sel` bit `i` names, of the high half the one `op_sel_hi` names; `neg` / `neg_hi` negate a float
    /// operand of the low / high half. f16 goes through f32 (exact operands, one rounding back to f16), as the
    /// 16-bit ops do. Integer halves wrap; integer clamp is refused.
    fn packed(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let op = name.strip_prefix("v_pk_")?;
        let float = op.ends_with("_f16");
        let n = match op {
            "fma_f16" | "mad_u16" | "mad_i16" => 3,
            "add_f16" | "mul_f16" | "max_f16" | "min_f16" | "add_u16" | "sub_u16" | "add_i16" | "sub_i16" | "mul_lo_u16" | "lshlrev_b16"
            | "lshrrev_b16" | "ashrrev_i16" | "max_u16" | "min_u16" | "max_i16" | "min_i16" => 2,
            _ => {
                self.why = Some(name.into());
                return None;
            }
        };
        if !float && d.get("clamp")? == 1 {
            self.why = Some("integer clamp".into());
            return None;
        }
        let (opsel, hi) = (d.get("opsel")?, d.get("opsel_hi")? | d.get("opsel_hi2")? << 2);
        let (neg, neg_hi) = (d.get("neg")?, d.get("neg_hi")?);
        let mut raw = Vec::new();
        for field in ["src0", "src1", "src2"].iter().take(n) {
            let code = d.get(field)?;
            raw.push(match enc::src(code) {
                Src::Inline(c @ 240..=248) => format!("{}u", inline_f16(c)),
                _ => self.src(code, d.literal)?,
            });
        }
        let mut halves = Vec::new();
        for h in 0..2u32 {
            let (sel, ng) = if h == 0 { (opsel, neg) } else { (hi, neg_hi) };
            let a: Vec<String> = (0..n)
                .map(|i| {
                    let x = if sel >> i & 1 == 1 { format!("({} >> 16)", raw[i]) } else { raw[i].clone() };
                    if float {
                        let f = format!("f16tof32({x})");
                        if ng >> i & 1 == 1 { format!("(-{f})") } else { f }
                    } else {
                        format!("({x} & 0xffffu)")
                    }
                })
                .collect();
            let s = |x: &str| format!("(((int){x} << 16) >> 16)");
            let e = match op {
                "add_f16" => format!("{} + {}", a[0], a[1]),
                "mul_f16" => format!("{} * {}", a[0], a[1]),
                "fma_f16" => format!("mad({}, {}, {})", a[0], a[1], a[2]),
                "max_f16" => format!("max({}, {})", a[0], a[1]),
                "min_f16" => format!("min({}, {})", a[0], a[1]),
                "add_u16" | "add_i16" => format!("{} + {}", a[0], a[1]),
                "sub_u16" | "sub_i16" => format!("{} - {}", a[0], a[1]),
                "mul_lo_u16" => format!("{} * {}", a[0], a[1]),
                "mad_u16" => format!("{} * {} + {}", a[0], a[1], a[2]),
                "mad_i16" => format!("(uint)({} * {} + {})", s(&a[0]), s(&a[1]), s(&a[2])),
                "lshlrev_b16" => format!("{} << ({} & 15u)", a[1], a[0]),
                "lshrrev_b16" => format!("{} >> ({} & 15u)", a[1], a[0]),
                "ashrrev_i16" => format!("(uint)({} >> ({} & 15u))", s(&a[1]), a[0]),
                "max_u16" => format!("max({}, {})", a[0], a[1]),
                "min_u16" => format!("min({}, {})", a[0], a[1]),
                "max_i16" => format!("(uint)max({}, {})", s(&a[0]), s(&a[1])),
                "min_i16" => format!("(uint)min({}, {})", s(&a[0]), s(&a[1])),
                _ => return None,
            };
            halves.push(if float {
                let e = if d.get("clamp")? == 1 { format!("saturate({e})") } else { e };
                format!("sir_f16({e})")
            } else {
                format!("(({e}) & 0xffffu)")
            });
        }
        let v = d.get("vdst")? as u16;
        self.vwrite_raw(v, format!("{} | ({} << 16)", halves[0], halves[1]));
        Some(())
    }

    /// `ds_swizzle_b32`: a lane reads another lane's value. Quad mode (offset bit 15): lane `i` of each quad
    /// reads the lane the 2-bit field `i` names, as four `QuadReadLaneAt` with constant lanes. Bitmask mode:
    /// lane `((id & and) | or) ^ xor` within each 32 lanes, as `WaveReadLaneAt`.
    fn swizzle(&mut self, d: &Decoded) -> Option<()> {
        let offset = d.get("offset0")? | d.get("offset1")? << 8;
        let (src, dst) = (d.get("addr")? as u16, d.get("vdst")? as u16);
        self.out.vgprs.insert(src);
        let e = if offset & 0x8000 != 0 {
            let q: Vec<String> = (0..4).map(|k| format!("QuadReadLaneAt(v{src}, {})", offset >> (2 * k) & 3)).collect();
            format!("{{ uint q[4] = {{ {} }}; r = q[WaveGetLaneIndex() & 3]; }}", q.join(", "))
        } else {
            let (and, or, xor) = (offset & 0x1f, offset >> 5 & 0x1f, offset >> 10 & 0x1f);
            self.out.notes.insert("ds_swizzle bitmask mode: WaveReadLaneAt with a per-lane index");
            format!("{{ uint id = WaveGetLaneIndex(); r = WaveReadLaneAt(v{src}, (id & ~31u) | ((((id & 31u) & {and}u) | {or}u) ^ {xor}u)); }}")
        };
        // every lane takes part in the read; only active lanes write
        self.line(format!("{{ uint r; {e}"));
        self.vwrite_raw(dst, "r".into());
        self.line("}".into());
        Some(())
    }

    /// DPP16 in its quad form (`quad_perm`, the only one an agnostic program has): lane `i` of each quad reads
    /// src0 from the lane the 2-bit field `i` names, as `QuadReadLaneAt` with constant lanes (host control flow
    /// is uniform, so every lane takes part). A source lane EXEC has off is read only with `fi`; otherwise it
    /// gives 0 with `bound_ctrl`, or leaves the destination unwritten.
    fn dpp16(&mut self, name: &str, d: &Decoded, src: u16) -> Option<()> {
        let ctrl = d.get("dpp_ctrl")?;
        if ctrl > 0xFF || d.get("row_mask")? != 0xF || d.get("bank_mask")? != 0xF {
            self.why = Some("dpp16 beyond a full-mask quad_perm".into());
            return None;
        }
        if d.get("src1_abs")? == 1 || d.get("src1_neg")? == 1 || name.starts_with("v_cmp") {
            self.why = Some("dpp16 form".into());
            return None;
        }
        self.out.vgprs.insert(src);
        let sel: Vec<u32> = (0..4).map(|k| ctrl >> (2 * k) & 3).collect();
        let q: Vec<String> = sel.iter().map(|l| format!("QuadReadLaneAt(v{src}, {l})")).collect();
        self.line(format!("{{ uint sir_q[4] = {{ {} }}; uint sir_dpp = sir_q[WaveGetLaneIndex() & 3];", q.join(", ")));
        let mut guard = false;
        if self.f.narrowed && d.get("fi")? == 0 {
            let v: Vec<String> = sel.iter().map(|l| format!("QuadReadLaneAt(ex, {l})")).collect();
            self.line(format!("  bool sir_qv[4] = {{ {} }}; bool sir_dv = sir_qv[WaveGetLaneIndex() & 3];", v.join(", ")));
            if d.get("bound_ctrl")? == 1 {
                self.line("  if (!sir_dv) sir_dpp = 0u;".into());
            } else {
                guard = true;
            }
        }
        self.dpp = Some(guard);
        let r = self.valu(name, d);
        self.dpp = None;
        self.line("}".into());
        r
    }

    /// SDWA (VOP1, VOP2, VOPC with a sub-dword word): each operand is a byte, a word or the dword of its
    /// register, zero- or sign-extended (`sext`); the op runs on those as on whole registers; a VOP1/VOP2
    /// result goes into the destination's selected byte or word, the other bits zeroed, sign-extended, or kept
    /// (`dst_unused`). `s0`/`s1` say an operand is an SGPR or a constant rather than a VGPR.
    fn sdwa(&mut self, name: &str, d: &Decoded, w: &[u32]) -> Option<()> {
        let (layout, _) = d.extra.as_ref()?;
        let field = |f: &str| layout.fields.iter().position(|x| x.name == f).map(|i| w[i]);
        let operand = |l: &mut Self, code: u32, scalar: bool, sel: u32, sext: bool| -> Option<String> {
            let x = l.src(if scalar { code } else { 256 + code }, None)?;
            Some(match (sel, sext) {
                (0..=3, false) => format!("(({x} >> {}u) & 0xffu)", 8 * sel),
                (0..=3, true) => format!("(uint)(((int){x} << {}) >> 24)", 24 - 8 * sel),
                (4 | 5, false) => format!("(({x} >> {}u) & 0xffffu)", 16 * (sel - 4)),
                (4 | 5, true) => format!("(uint)(((int){x} << {}) >> 16)", 16 - 16 * (sel - 4)),
                (6, _) => x,
                _ => return None,
            })
        };
        let a = operand(self, field("src0")?, field("s0")? == 1, field("src0_sel")?, field("src0_sext")? == 1)?;
        let b = if d.encoding == Encoding::Vop1 {
            "0u".to_string()
        } else {
            operand(self, d.get("vsrc1")?, field("s1")? == 1, field("src1_sel")?, field("src1_sext")? == 1)?
        };
        self.line(format!("{{ uint sir_a = {a}, sir_b = {b}, sir_r = 0u;"));
        self.sdwa = true;
        let r = self.valu(name, d);
        self.sdwa = false;
        if r.is_none() {
            self.line("}".into());
            return None;
        }
        if d.encoding != Encoding::Vopc && name != "v_nop" {
            let v = d.get("vdst")? as u16;
            let (sel, unused) = (field("dst_sel")?, field("dst_unused")?);
            let e = match sel {
                6 => "sir_r".to_string(),
                0..=5 => {
                    let (bits, off) = if sel < 4 { (8u32, 8 * sel) } else { (16u32, 16 * (sel - 4)) };
                    let m = ((1u64 << bits) - 1) as u32;
                    let f = format!("((sir_r & {m}u) << {off}u)");
                    match unused {
                        0 => f,
                        // the bits above the field copy its top bit; those below are 0
                        1 => {
                            let above = !(((1u64 << (bits + off)) - 1) as u32);
                            format!("({f} | (((sir_r >> {}u) & 1u) != 0u ? {above}u : 0u))", bits - 1)
                        }
                        2 => format!("((v{v} & {}u) | {f})", !(m << off)),
                        _ => {
                            self.line("}".into());
                            return None;
                        }
                    }
                }
                _ => {
                    self.line("}".into());
                    return None;
                }
            };
            self.vwrite_raw(v, e);
        }
        self.line("}".into());
        Some(())
    }

    /// LDS as `groupshared uint sir_lds[]`, by dword: reads and writes of 1, 2 or 4 dwords, the two-address
    /// forms (offsets in dwords, or 64 dwords with `st64`), and `ds_add_u32`. Only active lanes write.
    fn lds(&mut self, name: &str, d: &Decoded) -> Option<()> {
        if !matches!(self.stage, Stage::Cs(_)) || d.get("gds")? == 1 {
            return None;
        }
        self.out.uses_lds = true;
        let a = d.get("addr")? as u16;
        self.out.vgprs.insert(a);
        let (o0, o1) = (d.get("offset0")?, d.get("offset1")?);
        let off = o0 | o1 << 8;
        let guard = if self.f.narrowed { "if (ex) " } else { "" };
        let data = |l: &mut Self, f: &str, k: u16| -> Option<String> {
            let v = d.get(f)? as u16 + k;
            l.out.vgprs.insert(v);
            Some(format!("v{v}"))
        };
        let width = |n: &str| -> Option<u16> {
            Some(match n.rsplit('_').next()? {
                "b32" | "u32" => 1,
                "b64" => 2,
                "b96" => 3,
                "b128" => 4,
                _ => return None,
            })
        };
        match name {
            "ds_read_b32" | "ds_read_b64" | "ds_read_b96" | "ds_read_b128" => {
                let vdst = d.get("vdst")? as u16;
                for k in 0..width(name)? {
                    self.vwrite_raw(vdst + k, format!("sir_lds[((v{a} + {off}u) >> 2) + {k}u]"));
                }
            }
            "ds_write_b32" | "ds_write_b64" | "ds_write_b96" | "ds_write_b128" => {
                for k in 0..width(name)? {
                    let v = data(self, "data0", k)?;
                    self.line(format!("{guard}sir_lds[((v{a} + {off}u) >> 2) + {k}u] = {v};"));
                }
            }
            "ds_read2_b32" | "ds_read2st64_b32" => {
                let (vdst, m) = (d.get("vdst")? as u16, if name.contains("st64") { 64 } else { 1 });
                self.vwrite_raw(vdst, format!("sir_lds[(v{a} >> 2) + {}u]", o0 * m));
                self.vwrite_raw(vdst + 1, format!("sir_lds[(v{a} >> 2) + {}u]", o1 * m));
            }
            "ds_write2_b32" | "ds_write2st64_b32" => {
                let m = if name.contains("st64") { 64 } else { 1 };
                let (x, y) = (data(self, "data0", 0)?, data(self, "data1", 0)?);
                self.line(format!("{guard}sir_lds[(v{a} >> 2) + {}u] = {x};", o0 * m));
                self.line(format!("{guard}sir_lds[(v{a} >> 2) + {}u] = {y};", o1 * m));
            }
            // 8- and 16-bit reads (zero- or sign-extended) and writes; a write clears and sets its bits with two
            // atomics, so writes to the other bytes of the dword by other lanes are kept
            "ds_read_u8" | "ds_read_i8" | "ds_read_u16" | "ds_read_i16" => {
                let vdst = d.get("vdst")? as u16;
                let bits = if name.ends_with("8") { 8 } else { 16 };
                let x = format!("(sir_lds[(v{a} + {off}u) >> 2] >> (((v{a} + {off}u) & 3u) * 8u))");
                let e = if name.contains("_i") { format!("(uint)(((int){x} << {}) >> {})", 32 - bits, 32 - bits) } else { format!("({x} & {}u)", (1u32 << bits) - 1) };
                self.vwrite_raw(vdst, e);
            }
            "ds_write_b8" | "ds_write_b16" => {
                let x = data(self, "data0", 0)?;
                let m = if name.ends_with("8") { 0xffu32 } else { 0xffff };
                self.line(format!("{{ uint sir_b = v{a} + {off}u, sir_sh = (sir_b & 3u) * 8u;"));
                self.line(format!("  {guard}{{ InterlockedAnd(sir_lds[sir_b >> 2], ~({m}u << sir_sh)); InterlockedOr(sir_lds[sir_b >> 2], ({x} & {m}u) << sir_sh); }}"));
                self.line("}".into());
            }
            // atomics, with the old value returned by the `_rtn` forms
            n if lds_atomic(n).is_some() => {
                let (f, rtn) = lds_atomic(n)?;
                let x = data(self, "data0", 0)?;
                let args = match n.trim_start_matches("ds_").split('_').next()? {
                    "sub" => format!("0u - {x}"),
                    "cmpst" => format!("{x}, {}", data(self, "data1", 0)?),
                    _ => x,
                };
                self.line("{ uint o = 0;".into());
                self.line(format!("  {guard}{f}(sir_lds[(v{a} + {off}u) >> 2], {args}, o);"));
                if rtn {
                    let vdst = d.get("vdst")? as u16;
                    self.vwrite_raw(vdst, "(uint)o".into());
                }
                self.line("}".into());
            }
            _ => return None,
        }
        Some(())
    }

    /// `s_not_b64` / `s_not_b32`: of a lane mask, the per-lane complement (wave64 `_b64`, wave32 `_b32`);
    /// of data, the bitwise one. SCC: any bit (lane) set.
    fn s_not(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let (src, dst) = (d.get("ssrc0")?, d.get("sdst")?);
        let wide = name.ends_with("_b64");
        let mask = match enc::src(src) {
            Src::ExecLo | Src::VccLo => true,
            Src::Sgpr(n) => self.f.mask_cur.contains(&n),
            _ => false,
        };
        if mask && wide == self.wave64 {
            let m = self.mask_read(src)?;
            self.line(format!("{{ bool r = !{m};"));
            self.mask_write(dst, "r")?;
            self.mask_scc("r");
            self.line("}".into());
            return Some(());
        }
        let n = if wide { 2 } else { 1 };
        let xs: Option<Vec<String>> = (0..n).map(|k| self.src(src + k, None)).collect();
        let xs = xs?;
        self.line(format!("{{ uint r0 = ~{}, r1 = ~{};", xs[0], xs.get(1).map_or("0u", |s| s.as_str())));
        self.line("  scc = (r0 | r1) != 0;".into());
        self.swrite(dst, "r0".into())?;
        if wide {
            self.swrite(dst + 1, "r1".into())?;
        }
        self.line("}".into());
        Some(())
    }

    /// A 16-bit operand in the low half of a `uint` expression (the high half is not cleared): a register's low
    /// half, or its high half with VOP3 `op_sel` bit `i` (SDWA selects are already in `sir_a`/`sir_b`). Inline
    /// float constants are their f16 bit patterns (the high half 0).
    fn src16(&mut self, d: &Decoded, field: &str, i: u32) -> Option<String> {
        if self.sdwa && (field == "src0" || field == "vsrc1") {
            return Some(if field == "src0" { "sir_a" } else { "sir_b" }.into());
        }
        if self.dpp.is_some() && field == "src0" {
            return Some("sir_dpp".into());
        }
        if field == "vsrc1" {
            let v = d.get("vsrc1")? as u16;
            self.out.vgprs.insert(v);
            return Some(format!("v{v}"));
        }
        let code = d.get(field)?;
        let x = match enc::src(code) {
            Src::Inline(c @ 240..=248) => format!("{}u", inline_f16(c)),
            _ => self.src(code, d.literal)?,
        };
        Some(if d.encoding == Encoding::Vop3 && d.get("opsel").is_some_and(|o| o >> i & 1 == 1) { format!("({x} >> 16)") } else { x })
    }

    /// An f16 operand as a float (exact), with abs/neg (VOP3, SDWA or DPP).
    fn fsrc16(&mut self, d: &Decoded, field: &str, i: u32) -> Option<String> {
        let mut e = format!("f16tof32({})", self.src16(d, field, i)?);
        let modifiers = if self.sdwa && (field == "src0" || field == "vsrc1") {
            Some(if field == "src0" { "src0" } else { "src1" })
        } else if self.dpp.is_some() && field == "src0" {
            Some("src0")
        } else {
            None
        };
        let (abs, neg) = match modifiers {
            Some(k) => (d.get(&format!("{k}_abs")) == Some(1), d.get(&format!("{k}_neg")) == Some(1)),
            None if d.encoding == Encoding::Vop3 => (d.get("abs")? >> i & 1 == 1, d.get("neg")? >> i & 1 == 1),
            None => (false, false),
        };
        if abs {
            e = format!("abs({e})");
        }
        if neg {
            e = format!("(-{e})");
        }
        Some(e)
    }

    /// A 64-bit integer operand (a register pair, or an inline integer sign-extended) as a `uint64_t`.
    fn src64(&mut self, code: u32) -> Option<String> {
        let (lo, hi) = match enc::src(code) {
            Src::Vgpr(_) | Src::Sgpr(_) => (self.src(code, None)?, self.src(code + 1, None)?),
            Src::Inline(c @ 128..=192) => (format!("{}u", c - 128), "0u".to_string()),
            Src::Inline(c @ 193..=208) => (format!("{}u", (-(i32::from(c) - 192)) as u32), "0xffffffffu".to_string()),
            _ => return None,
        };
        Some(format!("(((uint64_t){hi} << 32) | (uint64_t){lo})"))
    }

    fn usrc(&mut self, d: &Decoded, field: &str) -> Option<String> {
        if self.sdwa && (field == "src0" || field == "vsrc1") {
            let k = if field == "src0" { "src0" } else { "src1" };
            if d.get(&format!("{k}_abs")) == Some(1) || d.get(&format!("{k}_neg")) == Some(1) {
                self.why = Some("SDWA abs/neg on an integer operand".into());
                return None;
            }
            return Some(if field == "src0" { "sir_a" } else { "sir_b" }.into());
        }
        if self.dpp.is_some() && field == "src0" {
            return Some("sir_dpp".into());
        }
        let code = d.get(field)?;
        self.src(code, d.literal)
    }

    /// The per-lane mask a VALU op reads: VCC for VOP2, an SGPR pair for VOP3 `src2`.
    fn mask_src(&mut self, d: &Decoded) -> Option<String> {
        let code = if d.encoding == Encoding::Vop3 { d.get("src2")? } else { 106 };
        self.mask_read(code)
    }

    /// A lane mask operand as a per-lane boolean: VCC, EXEC, an SGPR pair whose mask is current, or the
    /// constants 0 and -1 (no lane, every lane).
    fn mask_read(&mut self, code: u32) -> Option<String> {
        match enc::src(code) {
            Src::VccLo if self.f.may_written.contains(&106) => {
                self.why = Some("VCC read as a lane mask after data was written to it".into());
                None
            }
            Src::VccLo => Some(self.mask_name(106)),
            Src::ExecLo => Some("ex".into()),
            Src::Sgpr(n) if self.f.mask_cur.contains(&n) => Some(self.mask_name(n)),
            Src::Inline(128) => Some("false".into()),
            Src::Inline(193) => Some("true".into()),
            // a pair set to a constant as data (a loop's exit mask starts at 0)
            Src::Sgpr(n) if self.const_mask(n).is_some() => Some(self.const_mask(n)?.into()),
            Src::Sgpr(_) => {
                self.why = Some("lane mask not traced".into());
                None
            }
            _ => None,
        }
    }

    /// Writes a lane mask (a per-lane boolean expression) to EXEC, VCC or an SGPR pair.
    fn mask_write(&mut self, dst: u32, e: &str) -> Option<()> {
        match enc::src(dst) {
            Src::ExecLo => {
                self.out.exec_narrowed = true;
                self.f.narrowed = true;
                self.line(format!("ex = {e};"));
            }
            Src::VccLo => {
                for n in [106, 107] {
                    self.f.written.remove(&n);
                    self.f.may_written.remove(&n);
                }
                let v = self.mask_name(106);
                self.line(format!("{v} = {e};"));
            }
            Src::Sgpr(n) => {
                let m = self.mask_name(n);
                self.mask_owns(n);
                self.line(format!("{m} = {e};"));
            }
            Src::Null => {}
            _ => return None,
        }
        Some(())
    }

    /// SGPR `n` (and `n + 1` in wave64) now holds a lane mask: its data variables are stale.
    fn mask_owns(&mut self, n: u8) {
        let hi = if self.wave64 { n.saturating_add(1) } else { n };
        for s in [n, hi] {
            self.wrote(s);
            self.f.stale.insert(s);
        }
        self.f.mask_cur.insert(n);
    }

    /// An SGPR (pair in wave64) holding the constant 0 or -1 as data, as a lane mask.
    fn const_mask(&self, n: u8) -> Option<&'static str> {
        let lo = *self.f.consts.get(&n)?;
        if self.wave64 && self.f.consts.get(&(n + 1)) != Some(&lo) {
            return None;
        }
        match lo {
            0 => Some("false"),
            u32::MAX => Some("true"),
            _ => None,
        }
    }

    /// SGPR `s` was written with data.
    fn wrote(&mut self, s: u8) {
        self.f.written.insert(s);
        self.f.may_written.insert(s);
        self.f.descs.remove(&s);
        self.f.desc_words.remove(&s);
        self.f.consts.remove(&s);
        self.f.table_of.remove(&s);
        self.f.code_of.remove(&s);
        self.f.stale.remove(&s);
        self.f.mask_cur.remove(&s);
        // in wave64 a mask is a pair: writing its high SGPR ends it too
        if self.wave64 {
            self.f.mask_cur.remove(&s.wrapping_sub(1));
        }
    }

    /// SCC after a lane-mask op: is any lane's bit set (only computed when something reads it).
    fn mask_scc(&mut self, e: &str) {
        if self.scc_live {
            self.line(format!("scc = WaveActiveAnyTrue({e});"));
        }
    }

    fn mask_name(&mut self, n: u8) -> String {
        self.out.masks.insert(n);
        // the pair holds a lane mask from here on (or is read as one): not a known constant any more
        self.f.consts.remove(&n);
        self.f.consts.remove(&(n + 1));
        if n == 106 { "vcc".into() } else { format!("m{n}") }
    }

    /// Writes VGPR `vdst` with a float expression, applying VOP3 clamp and omod.
    fn vwrite_f(&mut self, d: &Decoded, expr: String) {
        let mut e = expr;
        if d.encoding == Encoding::Vop3 || self.sdwa {
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

    /// Writes a 16-bit result (an f32 expression rounded to f16 with VOP3 clamp/omod when `float`, else the
    /// low 16 bits of a `uint`) into the destination's low half, or its high half with VOP3 `op_sel` bit 3; the
    /// other half is kept (GFX10). Under SDWA the 16 bits go to `sir_r` for the destination select.
    fn vwrite16(&mut self, d: &Decoded, expr: String, float: bool) {
        let mut e = expr;
        if float {
            if d.encoding == Encoding::Vop3 || self.sdwa {
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
            e = format!("sir_f16({e})");
        } else {
            e = format!("(({e}) & 0xffffu)");
        }
        let v = d.get("vdst").unwrap_or(0) as u16;
        if self.sdwa {
            self.vwrite_raw(v, e);
        } else if d.encoding == Encoding::Vop3 && d.get("opsel").is_some_and(|o| o >> 3 & 1 == 1) {
            self.vwrite_raw(v, format!("(v{v} & 0xffffu) | ({e} << 16)"));
        } else {
            self.vwrite_raw(v, format!("(v{v} & 0xffff0000u) | {e}"));
        }
    }

    fn vwrite(&mut self, d: &Decoded, expr: String) {
        let v = d.get("vdst").unwrap_or(0) as u16;
        self.vwrite_raw(v, expr);
    }

    fn vwrite_raw(&mut self, v: u16, expr: String) {
        self.out.vgprs.insert(v);
        if self.sdwa {
            let _ = writeln!(self.out.body, "    sir_r = {expr};");
        } else if self.dpp == Some(true) {
            let g = if self.f.narrowed { "ex && sir_dv" } else { "sir_dv" };
            let _ = writeln!(self.out.body, "    if ({g}) v{v} = {expr};");
        } else if self.f.narrowed {
            let _ = writeln!(self.out.body, "    if (ex) v{v} = {expr};");
        } else {
            let _ = writeln!(self.out.body, "    v{v} = {expr};");
        }
    }

    fn swrite(&mut self, n: u32, expr: String) -> Option<()> {
        match enc::src(n) {
            Src::Sgpr(s) => {
                self.out.sgprs.insert(s);
                self.wrote(s);
                let _ = writeln!(self.out.body, "    s{s} = {expr};");
            }
            Src::M0 => {
                self.out.sgprs.insert(124);
                let _ = writeln!(self.out.body, "    s124 = {expr};");
            }
            // VCC as a spare SGPR pair (its lane mask is not current any more)
            Src::VccLo | Src::VccHi => {
                let s = if enc::src(n) == Src::VccLo { 106 } else { 107 };
                self.out.sgprs.insert(s);
                self.f.written.insert(s);
                self.f.may_written.insert(s);
                self.f.consts.remove(&s);
                let _ = writeln!(self.out.body, "    s{s} = {expr};");
            }
            Src::Null => {}
            _ => return None,
        }
        Some(())
    }

    /// An SALU operand's value when it is known: a literal, an inline integer, or an SGPR holding a constant.
    fn cval(&self, d: &Decoded, field: &str) -> Option<u32> {
        match enc::src(d.get(field)?) {
            Src::Literal => d.literal,
            Src::Inline(c @ 128..=192) => Some(u32::from(c - 128)),
            Src::Inline(c @ 193..=208) => Some((-(i32::from(c) - 192)) as u32),
            Src::Sgpr(s) => self.f.consts.get(&s).copied(),
            _ => None,
        }
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
            Encoding::Sop1 if name == "s_swappc_b64" => self.fetch(&d),
            Encoding::Sop1 if name == "s_getpc_b64" && matches!(self.stage, Stage::Vs(_)) => {
                // only ever the base of the fetch subroutine's address
                let s = d.get("sdst")?;
                self.swrite(s, "0u".into())?;
                self.swrite(s + 1, "0u".into())
            }
            // the address of the next instruction, as a byte offset into the code (data embedded after the
            // program is read from there: [`Lifted::code`])
            Encoding::Sop1 if name == "s_getpc_b64" => {
                let s = d.get("sdst")?;
                let pc = (i.offset + 4 * i.words.len()) as u32;
                self.swrite(s, format!("{pc}u"))?;
                self.swrite(s + 1, "0u".into())?;
                if let Src::Sgpr(n) = enc::src(s) {
                    self.f.code_of.insert(n);
                    self.f.consts.insert(n, pc);
                    self.f.consts.insert(n + 1, 0);
                }
                Some(())
            }
            Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc | Encoding::Sopk => self.salu(name, &d),
            Encoding::Vintrp => self.interp(name, &d),
            Encoding::Smem => self.smem(name, &d),
            Encoding::Mimg => self.mimg(name, &d),
            Encoding::Mubuf => self.mubuf(name, &d),
            Encoding::Mtbuf => self.tbuffer(name, &d),
            Encoding::Exp => self.export(&d),
            Encoding::Vop3p => self.vop3p(name, &d),
            Encoding::Ds if name == "ds_swizzle_b32" => self.swizzle(&d),
            Encoding::Ds => self.lds(name, &d),
            Encoding::Global => self.global(name, &d),
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vop3 | Encoding::Vopc => {
                if let Some((x, v)) = &d.extra {
                    if x.name == "dpp16" {
                        return self.dpp16(name, &d, v[0] as u16);
                    }
                    if x.name.starts_with("sdwa") {
                        let w = v.clone();
                        return self.sdwa(name, &d, &w);
                    }
                    // DPP8: not yet
                    self.why = Some(if x.name.starts_with("sdwa") { "SDWA".into() } else { x.name.to_string() });
                    return None;
                }
                self.valu(name, &d)
            }
            _ => None,
        }
    }

    fn sopp(&mut self, name: &str) -> Option<()> {
        match name {
            "s_endpgm" | "s_nop" | "s_clause" | "s_code_end" | "s_dcache_inv" | "s_ttracedata" => Some(()),
            "s_barrier" if matches!(self.stage, Stage::Cs(_)) => {
                self.line("GroupMemoryBarrierWithGroupSync();".into());
                Some(())
            }
            n if n.starts_with("s_waitcnt") || n.starts_with("s_inst_prefetch") || n.starts_with("s_sethalt") => Some(()),
            _ => None,
        }
    }

    fn salu(&mut self, name: &str, d: &Decoded) -> Option<()> {
        // which table (if any) the sources derive from: the destination derives from it too
        let table = [d.get("ssrc0"), d.get("ssrc1")].into_iter().flatten().find_map(|c| match enc::src(c) {
            Src::Sgpr(n) => self.table_origin(n),
            _ => None,
        });
        let code = matches!(name, "s_add_u32" | "s_add_i32" | "s_sub_u32" | "s_sub_i32" | "s_mov_b32" | "s_mov_b64")
            && [d.get("ssrc0"), d.get("ssrc1")].into_iter().flatten().any(|c| matches!(enc::src(c), Src::Sgpr(n) if self.f.code_of.contains(&n)));
        let r = self.salu_inner(name, d);
        if r.is_some() {
            if let (true, Some(Src::Sgpr(dst))) = (code, d.get("sdst").map(enc::src)) {
                self.f.code_of.insert(dst);
            }
            if let Some(Src::Sgpr(dst)) = d.get("sdst").map(enc::src) {
                match table {
                    Some(t) => {
                        self.f.table_of.insert(dst, t);
                    }
                    None => {
                        self.f.table_of.remove(&dst);
                    }
                }
            }
        }
        r
    }

    /// The descriptor table or heap a value derives from (its user-data dword): the parameter's own user
    /// SGPR (unwritten), or a value computed from one.
    fn table_origin(&self, n: u8) -> Option<u8> {
        if let Some(t) = self.f.table_of.get(&n) {
            return Some(*t);
        }
        let b = self.bind?;
        if self.f.may_written.contains(&n) {
            return None;
        }
        b.layout
            .iter()
            .find(|p| {
                b.hw.user_sgpr(p.user_data) == n
                    && matches!(
                        p.root,
                        rebuild_shader::native::RootType::Table
                            | rebuild_shader::native::RootType::ResourceHeap
                            | rebuild_shader::native::RootType::SamplerHeap
                    )
            })
            .map(|p| p.user_data)
    }

    fn salu_inner(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let sdst = d.get("sdst");
        // `s_lshr_b32 x, w, k` (k >= 16) or `s_bfe_u32 x, w, off | width << 16` (off >= 16)
        self.hi16 = match name {
            "s_lshr_b32" => self.cval(d, "ssrc1").is_some_and(|k| k & 31 >= 16),
            "s_bfe_u32" => self.cval(d, "ssrc1").is_some_and(|k| k & 31 >= 16),
            _ => false,
        };
        let a = |l: &mut Self| l.usrc(d, "ssrc0");
        let b = |l: &mut Self| l.usrc(d, "ssrc1");
        match name {
            "s_mov_b32" if !self.wave64 && self.is_mask32(d) => self.mov_mask(d, false),
            // a select between masks, or between 0 and -1 (written as both a mask and data)
            "s_cselect_b32"
                if !self.wave64
                    && (self.is_mask32(d)
                        || ["ssrc0", "ssrc1"].iter().all(|f| d.get(f).is_some_and(|c| matches!(enc::src(c), Src::Inline(128 | 193))))) =>
            {
                self.cselect_mask(d, false)
            }
            "s_and_b32" | "s_or_b32" | "s_xor_b32" | "s_andn2_b32" | "s_orn2_b32" | "s_nand_b32" | "s_nor_b32" | "s_xnor_b32"
                if !self.wave64 && self.is_mask32(d) =>
            {
                self.mask_op(name, d)
            }
            // M0 for interpolation (the attribute base): the host interpolates, so it is not needed
            "s_mov_b32" if matches!(self.stage, Stage::Ps(_)) && sdst.map(enc::src) == Some(Src::M0) => Some(()),
            "s_mov_b32" => {
                let x = a(self)?;
                self.swrite(sdst?, x)?;
                let value = match enc::src(d.get("ssrc0")?) {
                    Src::Literal => d.literal,
                    Src::Inline(c @ 128..=192) => Some(u32::from(c - 128)),
                    Src::Inline(c @ 193..=208) => Some((-(i32::from(c) - 192)) as u32),
                    _ => None,
                };
                if let (Some(v), Src::Sgpr(s)) = (value, enc::src(sdst?)) {
                    self.f.consts.insert(s, v);
                }
                Some(())
            }
            "s_movk_i32" => {
                let v = d.get("simm16")? as u16 as i16 as i32 as u32;
                self.swrite(sdst?, format!("{v}u"))?;
                if let Src::Sgpr(s) = enc::src(sdst?) {
                    self.f.consts.insert(s, v);
                }
                Some(())
            }
            "s_brev_b32" => {
                let folded = self.cval(d, "ssrc0").map(u32::reverse_bits);
                let x = a(self)?;
                self.swrite(sdst?, format!("reversebits({x})"))?;
                if let (Some(v), Src::Sgpr(s)) = (folded, enc::src(sdst?)) {
                    self.f.consts.insert(s, v);
                }
                Some(())
            }
            "s_mov_b64" => self.mov_mask(d, true),
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
                // constants through the ops that build them (sampler words, mostly)
                let folded = match (self.cval(d, "ssrc0"), self.cval(d, "ssrc1")) {
                    (Some(x), Some(y)) => match name {
                        "s_and_b32" => Some(x & y),
                        "s_or_b32" => Some(x | y),
                        "s_xor_b32" => Some(x ^ y),
                        "s_andn2_b32" => Some(x & !y),
                        "s_orn2_b32" => Some(x | !y),
                        "s_lshl_b32" => Some(x << (y & 31)),
                        "s_lshr_b32" => Some(x >> (y & 31)),
                        "s_add_u32" | "s_add_i32" => Some(x.wrapping_add(y)),
                        "s_sub_u32" | "s_sub_i32" => Some(x.wrapping_sub(y)),
                        "s_pack_ll_b32_b16" => Some((x & 0xffff) | y << 16),
                        _ => None,
                    },
                    _ => None,
                };
                let dst = sdst?;
                self.swrite(dst, "r".into())?;
                if let (Some(v), Src::Sgpr(s)) = (folded, enc::src(dst)) {
                    self.f.consts.insert(s, v);
                }
                self.line("}".into());
                Some(())
            }
            // s_waitcnt_vscnt / vmcnt / expcnt / lgkmcnt (SOPK): no effect on results
            n if n.starts_with("s_waitcnt") => Some(()),
            "s_not_b64" | "s_not_b32" => self.s_not(name, d),
            "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" => {
                let x = self.src64(d.get("ssrc0")?)?;
                let k = b(self)?;
                let e = match name {
                    "s_lshl_b64" => format!("{x} << ({k} & 63u)"),
                    "s_lshr_b64" => format!("{x} >> ({k} & 63u)"),
                    _ => format!("(uint64_t)((int64_t){x} >> ({k} & 63u))"),
                };
                let dst = sdst?;
                self.line(format!("{{ uint64_t sir_s = {e};"));
                self.line("  scc = sir_s != 0;".into());
                self.swrite(dst, "(uint)sir_s".into())?;
                self.swrite(dst + 1, "(uint)(sir_s >> 32)".into())?;
                self.line("}".into());
                Some(())
            }
            "s_bfm_b32" => {
                let (x, y) = (a(self)?, b(self)?);
                self.swrite(sdst?, format!("((1u << ({x} & 31u)) - 1u) << ({y} & 31u)"))
            }
            "s_mul_hi_u32" | "s_mul_hi_i32" => {
                let (x, y) = (a(self)?, b(self)?);
                let e = if name.ends_with("u32") {
                    format!("(uint)(((uint64_t){x} * (uint64_t){y}) >> 32)")
                } else {
                    format!("(uint)(((int64_t)(int){x} * (int64_t)(int){y}) >> 32)")
                };
                self.swrite(sdst?, e)
            }
            // add / subtract with SCC as the carry in and out
            "s_addc_u32" | "s_subb_u32" => {
                let (x, y) = (a(self)?, b(self)?);
                let sign = if name == "s_addc_u32" { "+" } else { "-" };
                self.line(format!("{{ uint64_t sir_s = (uint64_t){x} {sign} (uint64_t){y} {sign} (scc ? 1u : 0u);"));
                self.line("  scc = (sir_s >> 32) != 0;".into());
                self.swrite(sdst?, "(uint)sir_s".into())?;
                self.line("}".into());
                Some(())
            }
            // bit counts, scans and single-bit writes of data (a lane mask read here is refused as data)
            "s_bcnt1_i32_b32" | "s_bcnt0_i32_b32" | "s_ff1_i32_b32" | "s_flbit_i32_b32" | "s_flbit_i32" | "s_abs_i32" | "s_sext_i32_i8"
            | "s_sext_i32_i16" => {
                let x = a(self)?;
                let (e, scc) = match name {
                    "s_bcnt1_i32_b32" => (format!("countbits({x})"), true),
                    "s_bcnt0_i32_b32" => (format!("32u - countbits({x})"), true),
                    "s_ff1_i32_b32" => (format!("firstbitlow({x})"), false),
                    "s_flbit_i32_b32" => (format!("({x} == 0u ? 0xffffffffu : 31u - firstbithigh({x}))"), false),
                    "s_flbit_i32" => (format!("(({x} == 0u || {x} == 0xffffffffu) ? 0xffffffffu : 31u - firstbithigh((int){x}))"), false),
                    "s_abs_i32" => (format!("(uint)abs((int){x})"), true),
                    "s_sext_i32_i8" => (format!("(uint)(((int){x} << 24) >> 24)"), false),
                    _ => (format!("(uint)(((int){x} << 16) >> 16)"), false),
                };
                self.line(format!("{{ uint r = {e};"));
                if scc {
                    self.line("  scc = r != 0;".into());
                }
                self.swrite(sdst?, "r".into())?;
                self.line("}".into());
                Some(())
            }
            "s_bcnt1_i32_b64" => {
                let src = d.get("ssrc0")?;
                let (x, y) = (self.src(src, None)?, self.src(src + 1, None)?);
                self.line(format!("{{ uint r = countbits({x}) + countbits({y});"));
                self.line("  scc = r != 0;".into());
                self.swrite(sdst?, "r".into())?;
                self.line("}".into());
                Some(())
            }
            "s_bitset0_b32" | "s_bitset1_b32" => {
                let (k, x) = (a(self)?, self.usrc(d, "sdst")?);
                let e = if name == "s_bitset1_b32" { format!("{x} | (1u << ({k} & 31u))") } else { format!("{x} & ~(1u << ({k} & 31u))") };
                self.swrite(sdst?, e)
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
            "s_and_b64" | "s_or_b64" | "s_andn2_b64" | "s_xor_b64" | "s_orn2_b64" | "s_nand_b64" | "s_nor_b64" | "s_xnor_b64" => {
                self.mask_op(name, d)
            }
            "s_wqm_b64" | "s_wqm_b32" if name.ends_with(if self.wave64 { "_b64" } else { "_b32" }) => {
                // whole-quad mode: a lane is on if any lane of its quad is. On the host, helper lanes run for
                // derivatives anyway, so on an untouched EXEC this is a no-op. Host control flow is uniform, so
                // the quad reads see every lane.
                let (src, dst) = (d.get("ssrc0")?, sdst?);
                let exec_to_exec = matches!(enc::src(src), Src::ExecLo) && matches!(enc::src(dst), Src::ExecLo);
                if exec_to_exec && !self.f.narrowed {
                    self.mask_scc("ex");
                    return Some(());
                }
                let m = self.mask_read(src)?;
                self.line(format!("{{ bool r = {m} || QuadReadAcrossX({m}) || QuadReadAcrossY({m}) || QuadReadAcrossDiagonal({m});"));
                self.mask_write(dst, "r")?;
                self.mask_scc("r");
                self.line("}".into());
                Some(())
            }
            "s_cselect_b64" => self.cselect_mask(d, true),
            "s_addk_i32" | "s_mulk_i32" => {
                let k = d.get("simm16")? as u16 as i16 as i32;
                let x = self.usrc(d, "sdst")?;
                let e = if name == "s_addk_i32" { format!("(uint)((int){x} + {k})") } else { format!("(uint)((int){x} * {k})") };
                self.line(format!("{{ uint r = {e};"));
                if name == "s_addk_i32" {
                    // signed overflow
                    self.line(format!("  scc = ((int){x} >= 0) == ({k} >= 0) && ((int)r >= 0) != ({k} >= 0);"));
                }
                self.swrite(d.get("sdst")?, "r".into())?;
                self.line("}".into());
                Some(())
            }
            n if n.starts_with("s_cmpk_") => {
                let x = self.usrc(d, "sdst")?;
                let (op, signed) = cmp_op(n.strip_prefix("s_cmpk_")?)?;
                let k = d.get("simm16")? as u16;
                let e = if signed { format!("(int){x} {op} {}", k as i16) } else { format!("{x} {op} {k}u") };
                self.line(format!("scc = {e};"));
                Some(())
            }
            n if (n.contains("_saveexec_") || n.contains("_wrexec_")) && n.ends_with(if self.wave64 { "_b64" } else { "_b32" }) => {
                self.saveexec(n, d)
            }
            _ => None,
        }
    }

    /// SALU logic on lane masks (`_b64`, or `_b32` in wave32): per-lane boolean logic. SCC is whether any
    /// lane's bit is set.
    fn mask_op(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let (x, y) = (self.mask_read(d.get("ssrc0")?)?, self.mask_read(d.get("ssrc1")?)?);
        let e = match name.trim_end_matches("_b64").trim_end_matches("_b32") {
            "s_and" => format!("{x} && {y}"),
            "s_or" => format!("{x} || {y}"),
            "s_andn2" => format!("{x} && !{y}"),
            "s_orn2" => format!("{x} || !{y}"),
            "s_xor" => format!("{x} != {y}"),
            "s_xnor" => format!("{x} == {y}"),
            "s_nand" => format!("!({x} && {y})"),
            "s_nor" => format!("!({x} || {y})"),
            _ => return None,
        };
        if self.scc_live {
            self.line(format!("{{ bool r = {e};"));
            self.mask_write(d.get("sdst")?, "r")?;
            self.mask_scc("r");
            self.line("}".into());
        } else {
            self.mask_write(d.get("sdst")?, &e)?;
        }
        Some(())
    }

    /// `s_mov_b64` (or `s_mov_b32` in wave32): a lane mask copy, a constant (written as both a mask and
    /// data, since either may be read), or a plain 64-bit copy of data.
    fn mov_mask(&mut self, d: &Decoded, wide: bool) -> Option<()> {
        let (src, dst) = (d.get("ssrc0")?, d.get("sdst")?);
        let is_mask = |l: &Self, c: u32| match enc::src(c) {
            Src::ExecLo | Src::VccLo => true,
            Src::Sgpr(n) => l.f.mask_cur.contains(&n),
            _ => false,
        };
        match (enc::src(src), enc::src(dst)) {
            (Src::Inline(c @ (128 | 193)), Src::Sgpr(t)) => {
                let (m, v) = if c == 128 { ("false", "0u") } else { ("true", "0xffffffffu") };
                self.mask_write(dst, m)?;
                self.swrite_keep(t, v);
                if wide {
                    self.swrite_keep(t + 1, v);
                }
                Some(())
            }
            (_, Src::ExecLo | Src::VccLo) => {
                let m = self.mask_read(src)?;
                self.mask_write(dst, &m)
            }
            _ if is_mask(self, src) => {
                let m = self.mask_read(src)?;
                self.mask_write(dst, &m)
            }
            (Src::Sgpr(s), Src::Sgpr(t)) if wide => {
                let (a, b) = (self.src(u32::from(s), None)?, self.src(u32::from(s) + 1, None)?);
                self.line(format!("{{ uint a = {a}, b = {b};"));
                self.swrite(u32::from(t), "a".into())?;
                self.swrite(u32::from(t) + 1, "b".into())?;
                self.line("}".into());
                Some(())
            }
            _ => None,
        }
    }

    /// Writes data SGPR `t` without forgetting that its lane mask is current too (the value is both).
    fn swrite_keep(&mut self, t: u8, v: &str) {
        self.out.sgprs.insert(t);
        self.f.written.insert(t);
        self.f.may_written.insert(t);
        self.f.stale.remove(&t);
        match v {
            "0u" => self.f.consts.insert(t, 0),
            "0xffffffffu" => self.f.consts.insert(t, u32::MAX),
            _ => self.f.consts.remove(&t),
        };
        self.line(format!("s{t} = {v};"));
    }

    /// `s_cselect_b64` (or `_b32` in wave32) between lane masks or the constants 0 and -1: a mask by SCC. With
    /// two constants the pair holds the same value as data too (code tests it either way).
    fn cselect_mask(&mut self, d: &Decoded, wide: bool) -> Option<()> {
        let (a, b, dst) = (d.get("ssrc0")?, d.get("ssrc1")?, d.get("sdst")?);
        let (x, y) = (self.mask_read(a)?, self.mask_read(b)?);
        self.mask_write(dst, &format!("(scc ? {x} : {y})"))?;
        let word = |c: u32| match enc::src(c) {
            Src::Inline(128) => Some("0u"),
            Src::Inline(193) => Some("0xffffffffu"),
            _ => None,
        };
        if let (Some(p), Some(q), Src::Sgpr(t)) = (word(a), word(b), enc::src(dst)) {
            let v = format!("(scc ? {p} : {q})");
            self.swrite_keep(t, &v);
            if wide {
                self.swrite_keep(t + 1, &v);
            }
        }
        Some(())
    }

    /// In wave32, a 32-bit logic op or move is lane-mask work when it touches EXEC or VCC, or reads only
    /// masks (one at least).
    fn is_mask32(&self, d: &Decoded) -> bool {
        let codes: Vec<u32> = ["ssrc0", "ssrc1", "sdst"].iter().filter_map(|f| d.get(f)).collect();
        if codes.iter().any(|c| matches!(enc::src(*c), Src::ExecLo | Src::VccLo)) {
            return true;
        }
        let srcs: Vec<Src> = ["ssrc0", "ssrc1"].iter().filter_map(|f| d.get(f)).map(enc::src).collect();
        let mask = |s: &Src| matches!(s, Src::Sgpr(n) if self.f.mask_cur.contains(n));
        srcs.iter().any(mask) && srcs.iter().all(|s| mask(s) || matches!(s, Src::Inline(128 | 193)))
    }

    /// The save-EXEC family: `D = EXEC; EXEC = f(S, EXEC)` (`*_saveexec`), or `EXEC = f(S, EXEC); D = EXEC`
    /// (`*_wrexec`). SCC is whether any lane is left on.
    fn saveexec(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let s = self.mask_read(d.get("ssrc0")?)?;
        let op = name.split("_saveexec").next()?.split("_wrexec").next()?;
        let e = match op {
            "s_and" => "sir_s && sir_o",
            "s_or" => "sir_s || sir_o",
            "s_xor" => "sir_s != sir_o",
            "s_xnor" => "sir_s == sir_o",
            "s_andn1" => "!sir_s && sir_o",
            "s_andn2" => "sir_s && !sir_o",
            "s_orn1" => "!sir_s || sir_o",
            "s_orn2" => "sir_s || !sir_o",
            "s_nand" => "!(sir_s && sir_o)",
            "s_nor" => "!(sir_s || sir_o)",
            _ => return None,
        };
        self.line(format!("{{ bool sir_s = {s}, sir_o = ex;"));
        self.mask_write(126, e)?;
        self.mask_write(d.get("sdst")?, if name.contains("_wrexec_") { "ex" } else { "sir_o" })?;
        self.mask_scc("ex");
        self.line("}".into());
        Some(())
    }

    fn valu(&mut self, name: &str, d: &Decoded) -> Option<()> {
        if matches!(name, "v_interp_p1ll_f16" | "v_interp_p1lv_f16" | "v_interp_p2_f16") && d.encoding == Encoding::Vop3 {
            return self.interp16(d);
        }
        let f0 = |l: &mut Self| l.fsrc(d, "src0", 0);
        let u0 = |l: &mut Self| l.usrc(d, "src0");
        // VOP2's second operand is `vsrc1` (a VGPR); VOP3's is `src1`
        let s1 = if d.encoding == Encoding::Vop3 { "src1" } else { "vsrc1" };
        let f1 = |l: &mut Self| -> Option<String> {
            if d.encoding == Encoding::Vop3 {
                l.fsrc(d, "src1", 1)
            } else if l.sdwa {
                l.fsrc(d, "vsrc1", 1)
            } else {
                let v = d.get("vsrc1")? as u16;
                l.out.vgprs.insert(v);
                Some(format!("asfloat(v{v})"))
            }
        };
        let u1 = |l: &mut Self| -> Option<String> {
            if d.encoding == Encoding::Vop3 {
                l.usrc(d, "src1")
            } else if l.sdwa {
                l.usrc(d, "vsrc1")
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
            // bit scans: -1 when no bit is found, as on the console. `v_ffbh_u32` counts from the top bit;
            // `v_ffbh_i32` finds the first bit that differs from the sign (none for 0 and -1)
            "v_ffbl_b32" | "v_ffbh_u32" | "v_ffbh_i32" | "v_bfrev_b32" | "v_not_b32" => {
                let x = u0(self)?;
                let e = match name {
                    "v_ffbl_b32" => format!("firstbitlow({x})"),
                    "v_ffbh_u32" => format!("({x} == 0u ? 0xffffffffu : 31u - firstbithigh({x}))"),
                    "v_ffbh_i32" => format!("(({x} == 0u || {x} == 0xffffffffu) ? 0xffffffffu : 31u - firstbithigh((int){x}))"),
                    "v_bfrev_b32" => format!("reversebits({x})"),
                    "v_not_b32" => format!("~{x}"),
                    _ => unreachable!(),
                };
                self.vwrite(d, e);
                Some(())
            }
            // float, one operand
            "v_floor_f32" | "v_ceil_f32" | "v_trunc_f32" | "v_rndne_f32" | "v_fract_f32" | "v_sqrt_f32" | "v_rsq_f32"
            | "v_rcp_f32" | "v_rcp_iflag_f32" | "v_exp_f32" | "v_log_f32" | "v_sin_f32" | "v_cos_f32" => {
                let x = f0(self)?;
                let e = match name {
                    "v_floor_f32" => format!("floor({x})"),
                    "v_ceil_f32" => format!("ceil({x})"),
                    "v_trunc_f32" => format!("trunc({x})"),
                    "v_rndne_f32" => format!("round({x})"),
                    "v_fract_f32" => format!("frac({x})"),
                    "v_sqrt_f32" => format!("sqrt({x})"),
                    "v_rsq_f32" => format!("rsqrt({x})"),
                    "v_rcp_f32" | "v_rcp_iflag_f32" => format!("rcp({x})"),
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
            "v_fma_f32" | "v_mad_f32" | "v_mad_legacy_f32" | "v_fma_legacy_f32" | "v_med3_f32" | "v_min3_f32" | "v_max3_f32"
            | "v_cubeid_f32" | "v_cubesc_f32" | "v_cubetc_f32" | "v_cubema_f32" => {
                let (x, y, z) = (f0(self)?, f1(self)?, f2(self)?);
                let e = match name {
                    "v_cubeid_f32" => format!("sir_cube(float3({x}, {y}, {z})).x"),
                    "v_cubesc_f32" => format!("sir_cube(float3({x}, {y}, {z})).y"),
                    "v_cubetc_f32" => format!("sir_cube(float3({x}, {y}, {z})).z"),
                    "v_cubema_f32" => format!("sir_cube(float3({x}, {y}, {z})).w"),
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
                    "v_perm_b32" => format!("sir_perm({x}, {y}, {z})"),
                    _ => unreachable!(),
                };
                self.vwrite(d, e);
                Some(())
            }
            // 16-bit float, computed in f32 from exact f16 operands and rounded to f16 (exact for add, sub, mul)
            "v_add_f16" | "v_sub_f16" | "v_subrev_f16" | "v_mul_f16" | "v_max_f16" | "v_min_f16" | "v_ldexp_f16" => {
                let x = self.fsrc16(d, "src0", 0)?;
                let y = self.fsrc16(d, if d.encoding == Encoding::Vop3 { "src1" } else { "vsrc1" }, 1)?;
                let e = match name {
                    "v_add_f16" => format!("{x} + {y}"),
                    "v_sub_f16" => format!("{x} - {y}"),
                    "v_subrev_f16" => format!("{y} - {x}"),
                    "v_mul_f16" => format!("{x} * {y}"),
                    "v_max_f16" => format!("max({x}, {y})"),
                    "v_min_f16" => format!("min({x}, {y})"),
                    // the exponent is a 16-bit signed integer operand
                    _ => {
                        let k = self.src16(d, if d.encoding == Encoding::Vop3 { "src1" } else { "vsrc1" }, 1)?;
                        format!("ldexp({x}, (float)(((int){k} << 16) >> 16))")
                    }
                };
                self.vwrite16(d, e, true);
                Some(())
            }
            "v_fma_f16" | "v_mad_f16" | "v_fmac_f16" | "v_mac_f16" => {
                let x = self.fsrc16(d, "src0", 0)?;
                let y = self.fsrc16(d, if d.encoding == Encoding::Vop3 { "src1" } else { "vsrc1" }, 1)?;
                let z = if name.contains("mac") {
                    let v = d.get("vdst")? as u16;
                    self.out.vgprs.insert(v);
                    format!("f16tof32(v{v})")
                } else {
                    self.fsrc16(d, "src2", 2)?
                };
                self.vwrite16(d, format!("mad({x}, {y}, {z})"), true);
                Some(())
            }
            "v_rcp_f16" | "v_sqrt_f16" | "v_rsq_f16" | "v_floor_f16" | "v_ceil_f16" | "v_trunc_f16" | "v_rndne_f16" | "v_fract_f16"
            | "v_exp_f16" | "v_log_f16" | "v_sin_f16" | "v_cos_f16" => {
                let x = self.fsrc16(d, "src0", 0)?;
                let e = match name {
                    "v_rcp_f16" => format!("rcp({x})"),
                    "v_sqrt_f16" => format!("sqrt({x})"),
                    "v_rsq_f16" => format!("rsqrt({x})"),
                    "v_floor_f16" => format!("floor({x})"),
                    "v_ceil_f16" => format!("ceil({x})"),
                    "v_trunc_f16" => format!("trunc({x})"),
                    "v_rndne_f16" => format!("round({x})"),
                    "v_fract_f16" => format!("frac({x})"),
                    "v_exp_f16" => format!("exp2({x})"),
                    "v_log_f16" => format!("log2({x})"),
                    "v_sin_f16" => format!("sin(({x}) * 6.2831853f)"),
                    _ => format!("cos(({x}) * 6.2831853f)"),
                };
                self.vwrite16(d, e, true);
                Some(())
            }
            // 64-bit shifts (VOP3): the shift amount from src0, the pair from src1
            "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => {
                let k = u0(self)?;
                let x = self.src64(d.get("src1")?)?;
                let e = match name {
                    "v_lshlrev_b64" => format!("{x} << ({k} & 63u)"),
                    "v_lshrrev_b64" => format!("{x} >> ({k} & 63u)"),
                    _ => format!("(uint64_t)((int64_t){x} >> ({k} & 63u))"),
                };
                let v = d.get("vdst")? as u16;
                self.line(format!("{{ uint64_t sir_s = {e};"));
                self.vwrite_raw(v, "(uint)sir_s".into());
                self.vwrite_raw(v + 1, "(uint)(sir_s >> 32)".into());
                self.line("}".into());
                Some(())
            }
            "v_mul_hi_i32" => {
                let (x, y) = (u0(self)?, u1(self)?);
                self.vwrite(d, format!("(uint)(((int64_t)(int){x} * (int64_t)(int){y}) >> 32)"));
                Some(())
            }
            // byte S1 of the result is S0 as an unsigned byte (rounded to nearest even, saturated, NaN 0); the
            // other bytes are S2's
            "v_cvt_pk_u8_f32" => {
                let (x, y, z) = (f0(self)?, u1(self)?, u2(self)?);
                self.vwrite(d, format!("({z} & ~(0xffu << (({y} & 3u) * 8u))) | ((uint)clamp(round({x}), 0.0f, 255.0f) << (({y} & 3u) * 8u))"));
                Some(())
            }
            // 32-bit add/sub with a carry (borrow) out to a lane mask, and in from one (`_ci`): VCC for VOP2,
            // `sdst` / `src2` for VOP3
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" | "v_add_co_ci_u32" | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => {
                if d.get("clamp") == Some(1) {
                    self.why = Some("clamp".into());
                    return None;
                }
                let (mut x, mut y) = (u0(self)?, u1(self)?);
                if name.starts_with("v_subrev") {
                    std::mem::swap(&mut x, &mut y);
                }
                let c = if name.contains("_ci_") { format!("({} ? 1u : 0u)", self.mask_src(d)?) } else { "0u".into() };
                let sign = if name.starts_with("v_add") { "+" } else { "-" };
                self.line(format!("{{ uint64_t sir_s = (uint64_t){x} {sign} (uint64_t){y} {sign} (uint64_t){c};"));
                self.vwrite(d, "(uint)sir_s".into());
                let dst = if d.encoding == Encoding::Vop3 { d.get("sdst")? } else { 106 };
                let carry = if self.f.narrowed { "ex && (sir_s >> 32) != 0u" } else { "(sir_s >> 32) != 0u" };
                self.mask_write(dst, carry)?;
                self.line("}".into());
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
            // the float's class against a mask of classes (bits: sNaN, qNaN, -inf, -normal, -denormal, -0,
            // +0, +denormal, +normal, +inf), decided from its bits, so denormal flushing cannot change it
            "f32" if op == "class" => {
                let a = self.usrc(d, "src0")?;
                let m = if d.encoding == Encoding::Vop3 {
                    self.usrc(d, "src1")?
                } else if self.sdwa {
                    self.usrc(d, "vsrc1")?
                } else {
                    let v = d.get("vsrc1")? as u16;
                    self.out.vgprs.insert(v);
                    format!("v{v}")
                };
                format!("sir_class({a}, {m})")
            }
            "f32" => {
                let a = self.fsrc(d, "src0", 0)?;
                let b = if d.encoding == Encoding::Vop3 {
                    self.fsrc(d, "src1", 1)?
                } else if self.sdwa {
                    self.fsrc(d, "vsrc1", 1)?
                } else {
                    let v = d.get("vsrc1")? as u16;
                    self.out.vgprs.insert(v);
                    format!("asfloat(v{v})")
                };
                fcmp(op, &a, &b)?
            }
            "f16" if op == "class" => return None,
            "f16" => {
                let a = self.fsrc16(d, "src0", 0)?;
                let b = self.fsrc16(d, if d.encoding == Encoding::Vop3 { "src1" } else { "vsrc1" }, 1)?;
                fcmp(op, &a, &b)?
            }
            "u64" | "i64" => {
                if self.sdwa || self.dpp.is_some() {
                    return None;
                }
                let a = self.src64(d.get("src0")?)?;
                let b = if d.encoding == Encoding::Vop3 { self.src64(d.get("src1")?)? } else { self.src64(256 + d.get("vsrc1")?)? };
                let (o, _) = cmp_op(&format!("{op}_u32"))?;
                let cast = if ty == "i64" { "(int64_t)" } else { "" };
                match op {
                    "t" => "true".into(),
                    "f" => "false".into(),
                    _ => format!("{cast}{a} {o} {cast}{b}"),
                }
            }
            "u32" | "i32" => {
                let a = self.usrc(d, "src0")?;
                let b = if d.encoding == Encoding::Vop3 {
                    self.usrc(d, "src1")?
                } else if self.sdwa {
                    self.usrc(d, "vsrc1")?
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
        let e = if self.f.narrowed { format!("ex && ({e})") } else { format!("({e})") };
        // GFX10: v_cmpx writes EXEC only; v_cmp writes VCC (VOPC) or an SGPR pair (VOP3)
        if x_form {
            return self.mask_write(126, &e);
        }
        let dst = if d.encoding == Encoding::Vop3 {
            d.get("vdst")?
        } else if self.sdwa && d.get("sd")? == 1 {
            d.get("sdst")?
        } else {
            106
        };
        if !matches!(enc::src(dst), Src::VccLo | Src::Sgpr(_)) {
            return None;
        }
        self.mask_write(dst, &e)
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

/// An LDS atomic: (the `Interlocked*` function, returns the old value).
fn lds_atomic(n: &str) -> Option<(&'static str, bool)> {
    let rtn = n.contains("_rtn_");
    let f = match n.replace("_rtn", "").as_str() {
        "ds_add_u32" | "ds_sub_u32" => "InterlockedAdd",
        // (signed min/max would compile unsigned on the `uint` LDS array: not translated)
        "ds_min_u32" => "InterlockedMin",
        "ds_max_u32" => "InterlockedMax",
        "ds_and_b32" => "InterlockedAnd",
        "ds_or_b32" => "InterlockedOr",
        "ds_xor_b32" => "InterlockedXor",
        "ds_wrxchg_b32" if rtn => "InterlockedExchange",
        "ds_cmpst_b32" if rtn => "InterlockedCompareExchange",
        _ => return None,
    };
    Some((f, rtn))
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

/// Translates a straight-line program's instructions (no stage inputs or outputs).
pub fn lift(insts: &[Instruction]) -> Lifted {
    lift_stage(insts, Stage::None, None)
}

/// Translates a straight-line program of a given stage, with its root parameters if known.
pub fn lift_stage<'a>(insts: &[Instruction], stage: Stage<'a>, bind: Option<Bindings<'a>>) -> Lifted {
    let mut l = Lifter::new(stage, bind, true);
    let live = cfg::scc_live_straight(insts);
    l.run(insts, &live);
    l.out
}

/// Translates a whole program, with control flow (v1): blocks from its branches, structured into HLSL
/// `if`/loops ([`cfg`]). `wave64` is the console's wave size for this program (lane-mask ops are `_b64`).
/// An irreducible graph or a branch kind not translated is named in [`Lifted::unsupported`].
pub fn lift_program<'a>(insts: &[Instruction], stage: Stage<'a>, bind: Option<Bindings<'a>>, wave64: bool) -> Lifted {
    lift_program_code(insts, &[], stage, bind, wave64)
}

/// [`lift_program`] with the program's code bytes, so data embedded in the code (read through `s_getpc_b64`;
/// the decoder decodes only reachable code) can be read: [`Lifted::code`]. Without them such reads are refused.
pub fn lift_program_code<'a>(insts: &[Instruction], code: &[u8], stage: Stage<'a>, bind: Option<Bindings<'a>>, wave64: bool) -> Lifted {
    let mut l = Lifter::new(stage, bind, wave64);
    l.code = code.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
    cfg::lift(l, insts)
}

impl<'a> Lifter<'a> {
    fn new(stage: Stage<'a>, bind: Option<Bindings<'a>>, wave64: bool) -> Self {
        Lifter { out: Lifted::default(), stage, bind, f: FlowState::default(), why: None, wave64, scc_live: false, dpp: None, sdwa: false, hi16: false, code: Vec::new() }
    }

    /// Lifts instructions in order; `live[k]` says whether SCC is read after instruction k.
    fn run(&mut self, insts: &[Instruction], live: &[bool]) {
        for (k, i) in insts.iter().enumerate() {
            self.why = None;
            self.hi16 = false;
            self.scc_live = live.get(k).copied().unwrap_or(true);
            if self.inst(i).is_none() {
                let n = i.name().unwrap_or("?").to_string();
                self.out.unsupported.push((i.offset, match self.why.take() {
                    Some(w) => format!("{n} ({w})"),
                    None => n,
                }));
                let _ = writeln!(self.out.body, "    // not translated: {}", i.name().unwrap_or("?"));
            }
        }
    }
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
