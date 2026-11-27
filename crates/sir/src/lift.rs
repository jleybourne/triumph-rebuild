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
    /// Every descriptor read and the host view it is read as, in program order: what a heap must hold for
    /// the program to run (the execution check builds its inputs from this). Translation does not use it.
    pub desc_uses: Vec<res::DescUse>,
}

/// The register space of the descriptor shadow table: per heap index, 16 bytes the runtime writes from the
/// console descriptor (stride, number of records, 0, 0), for what the host view does not carry.
pub const SHADOW_SPACE: u32 = 101;

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

pub mod cfg;
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
    /// loaded descriptors (as in `descs`) whose byte offset from their table's start is a constant
    desc_off: BTreeMap<u8, i32>,
    /// SGPRs in `table_of` that hold their table's offset plus a constant (copies and constant adds)
    table_add: BTreeMap<u8, i32>,
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
            desc_off: same(&self.desc_off, &o.desc_off),
            table_add: same(&self.table_add, &o.table_add),
            consts: same(&self.consts, &o.consts),
            written: self.written.intersection(&o.written).copied().collect(),
            may_written: self.may_written.union(&o.may_written).copied().collect(),
            mask_cur: self.mask_cur.intersection(&o.mask_cur).copied().collect(),
            stale: self.stale.union(&o.stale).copied().collect(),
            narrowed: self.narrowed || o.narrowed,
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
}

/// What an operand reads, as a `uint` expression; `None` if not translated yet.
impl Lifter<'_> {
    fn src(&mut self, code: u32, literal: Option<u32>) -> Option<String> {
        Some(match enc::src(code) {
            Src::Sgpr(n) => {
                if self.f.descs.contains_key(&n) || self.f.desc_words.contains(&n) {
                    return None; // a descriptor word read as data
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
            Src::Null => "0u".into(),
            _ => return None,
        })
    }

    /// A float operand, with VOP3 abs/neg for operand `i`.
    fn fsrc(&mut self, d: &Decoded, field: &str, i: u32) -> Option<String> {
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
            "ds_add_u32" => {
                let x = data(self, "data0", 0)?;
                self.line(format!("{guard}InterlockedAdd(sir_lds[(v{a} + {off}u) >> 2], {x});"));
            }
            _ => return None,
        }
        Some(())
    }

    fn usrc(&mut self, d: &Decoded, field: &str) -> Option<String> {
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
        self.f.desc_off.remove(&s);
        self.f.table_add.remove(&s);
        self.f.desc_words.remove(&s);
        self.f.consts.remove(&s);
        self.f.table_of.remove(&s);
        self.f.stale.remove(&s);
        self.f.mask_cur.remove(&s);
        self.f.mask_cur.remove(&s.wrapping_sub(1));
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
        self.vwrite_raw(v, expr);
    }

    fn vwrite_raw(&mut self, v: u16, expr: String) {
        self.out.vgprs.insert(v);
        if self.dpp == Some(true) {
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
            Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc | Encoding::Sopk => self.salu(name, &d),
            Encoding::Vintrp => self.interp(name, &d),
            Encoding::Smem => self.smem(name, &d),
            Encoding::Mimg => self.mimg(name, &d),
            Encoding::Mubuf => self.mubuf(name, &d),
            Encoding::Exp => self.export(&d),
            Encoding::Vop3p => self.vop3p(name, &d),
            Encoding::Ds if name == "ds_swizzle_b32" => self.swizzle(&d),
            Encoding::Ds => self.lds(name, &d),
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vop3 | Encoding::Vopc => {
                if let Some((x, v)) = &d.extra {
                    if x.name == "dpp16" {
                        return self.dpp16(name, &d, v[0] as u16);
                    }
                    // SDWA / DPP8: not yet
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
        // a copy of a table's offset, or the offset plus a constant: still a known place in the table
        let sgpr = |f: &str| d.get(f).map(enc::src).and_then(|c| if let Src::Sgpr(n) = c { Some(n) } else { None });
        let added = match name {
            "s_mov_b32" | "s_mov_b64" => sgpr("ssrc0").and_then(|n| self.table_offset(n)),
            "s_add_u32" | "s_add_i32" => match (sgpr("ssrc0"), sgpr("ssrc1")) {
                (Some(n), _) if self.table_offset(n).is_some() => self.cval(d, "ssrc1").map(|c| self.table_offset(n).unwrap().wrapping_add(c as i32)),
                (_, Some(n)) if self.table_offset(n).is_some() => self.cval(d, "ssrc0").map(|c| self.table_offset(n).unwrap().wrapping_add(c as i32)),
                _ => None,
            },
            "s_sub_u32" | "s_sub_i32" => sgpr("ssrc0").and_then(|n| self.table_offset(n)).and_then(|k| self.cval(d, "ssrc1").map(|c| k.wrapping_sub(c as i32))),
            _ => None,
        };
        let r = self.salu_inner(name, d);
        if r.is_some() {
            if let Some(Src::Sgpr(dst)) = d.get("sdst").map(enc::src) {
                match table {
                    Some(t) => {
                        self.f.table_of.insert(dst, t);
                        if let Some(k) = added {
                            self.f.table_add.insert(dst, k);
                        }
                    }
                    None => {
                        self.f.table_of.remove(&dst);
                    }
                }
            }
        }
        r
    }

    /// The value SGPR `n` holds as its table's (or heap's) offset plus a constant: the own user SGPR (unwritten) is
    /// plus 0; a copy or constant add of one is tracked in `table_add`; anything else derived is unknown.
    fn table_offset(&self, n: u8) -> Option<i32> {
        if let Some(k) = self.f.table_add.get(&n) {
            return Some(*k);
        }
        if self.f.table_of.contains_key(&n) || self.f.may_written.contains(&n) {
            return None;
        }
        let b = self.bind?;
        b.layout.iter().any(|p| b.hw.user_sgpr(p.user_data) == n && matches!(p.root, rebuild_shader::native::RootType::Table | rebuild_shader::native::RootType::ResourceHeap | rebuild_shader::native::RootType::SamplerHeap)).then_some(0)
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
            // the float's class against a mask of classes (bits: sNaN, qNaN, -inf, -normal, -denormal, -0,
            // +0, +denormal, +normal, +inf), decided from its bits, so denormal flushing cannot change it
            "f32" if op == "class" => {
                let a = self.usrc(d, "src0")?;
                let m = if d.encoding == Encoding::Vop3 {
                    self.usrc(d, "src1")?
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
        let e = if self.f.narrowed { format!("ex && ({e})") } else { format!("({e})") };
        // GFX10: v_cmpx writes EXEC only; v_cmp writes VCC (VOPC) or an SGPR pair (VOP3)
        if x_form {
            return self.mask_write(126, &e);
        }
        let dst = if d.encoding == Encoding::Vop3 { d.get("vdst")? } else { 106 };
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
    let l = Lifter::new(stage, bind, wave64);
    cfg::lift(l, insts)
}

impl<'a> Lifter<'a> {
    fn new(stage: Stage<'a>, bind: Option<Bindings<'a>>, wave64: bool) -> Self {
        Lifter { out: Lifted::default(), stage, bind, f: FlowState::default(), why: None, wave64, scc_live: false, dpp: None }
    }

    /// Lifts instructions in order; `live[k]` says whether SCC is read after instruction k.
    fn run(&mut self, insts: &[Instruction], live: &[bool]) {
        for (k, i) in insts.iter().enumerate() {
            self.why = None;
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
