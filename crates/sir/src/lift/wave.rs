//! Wave-wide programs (`docs/design/shader-ir.md`, "Classify first" and "Lane doubling").
//!
//! The classifier ([`rebuild_shader::features::WaveClass`]) splits programs by what they see of the wave. How
//! console lanes map to host lanes then follows from the class, the console wave size and the stage
//! ([`Wave::choose`]):
//!
//! ```text
//! agnostic, or every v_readfirstlane      Agnostic    one console lane per host lane, nothing crosses a quad
//!   operand provably uniform
//! console wave32 (any stage)              Native32    host wave32 = console wave32 ([WaveSize(32)] in compute)
//! console wave64 pixel / vertex           Partial64   a host wave (<= 64 lanes) is a console wave64 whose other
//!                                                     lanes were never launched: graphics packs lanes into waves
//!                                                     as it likes and leaves waves partly filled, so this is an
//!                                                     execution the console itself can produce
//! console wave64 compute                  Doubled     host lane i carries console lanes i and i + 32
//! ```
//!
//! **Doubling, in text.** The lifter writes each instruction's statements for "the lane", in plain names
//! (`v3`, `ex`, `vcc`, `m4`, `sir_lane`). In a doubled program every instruction whose statements name a
//! per-lane value is written twice, once per half, with `_0` / `_1` appended to those names ([`rename`]). A name
//! that already carries its half (`v3_1`) is a read across halves and stays as it is. Statements that must run
//! once (a ballot materialised as data, SCC after a lane-mask op) go in `pre` / `post`, around the two copies.
//! Host control flow stays uniform (EXEC is data), so every wave intrinsic sees every host lane.

use std::fmt::Write as _;

use rebuild_shader::features::WaveClass;
use rebuild_shader::gfx10::enc::{self, Decoded, Src};
use rebuild_shader::gfx10::{Encoding, Instruction};
use rebuild_shader::native::ComputeIo;

use super::{Lifted, Lifter, Stage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, PartialOrd, Ord)]
pub enum Wave {
    /// one console lane per host lane; no lane-dependent cross-lane operation (the v0/v1 path)
    #[default]
    Agnostic,
    /// console wave32 on a host wave of 32
    Native32,
    /// console wave64 graphics: each host wave is a partly filled console wave
    Partial64,
    /// console wave64 compute: host lane i carries console lanes i and i + 32
    Doubled,
    /// for measurement only (`wavetest --cost`): console wave64 compute as two host waves of 32 that exchange
    /// halves through groupshared memory with barriers, the previous layer's design. Valid for groups of exactly
    /// one console wave (64 threads); never chosen by [`Wave::choose`].
    TwoWave,
}

impl Wave {
    /// The mapping for a program of class `class`; `insts` decides a `ReadFirstLane` program.
    pub fn choose(class: WaveClass, wave64: bool, compute: bool, insts: &[Instruction]) -> Wave {
        match class {
            WaveClass::Agnostic => Wave::Agnostic,
            WaveClass::ReadFirstLane if rfl_uniform(insts) => Wave::Agnostic,
            _ if !wave64 => Wave::Native32,
            _ if compute => Wave::Doubled,
            _ => Wave::Partial64,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Wave::Agnostic => "agnostic",
            Wave::Native32 => "native32",
            Wave::Partial64 => "partial64",
            Wave::Doubled => "doubled",
            Wave::TwoWave => "two-wave",
        }
    }
}

/// Wave helpers, called by the statements of non-agnostic programs (unused ones cost nothing).
pub const PRELUDE: &str = r#"
uint sir_first(uint b) { return b == 0u ? 0u : firstbitlow(b); }
// v_readlane in a partly filled console wave64: a lane never launched reads 0
uint sir_rl64(uint x, uint k) { uint n = WaveGetLaneCount(); uint r = WaveReadLaneAt(x, k % n); return k < n ? r : 0u; }
// v_readlane in a doubled wave: lane k is in half k >> 5
uint sir_rl2(uint x0, uint x1, uint k) { uint a = WaveReadLaneAt(x0, k & 31u), b = WaveReadLaneAt(x1, k & 31u); return (k & 32u) != 0u ? b : a; }
// v_readfirstlane: the first lane EXEC has on (lane 0 when none)
uint sir_rfl32(uint x, bool e) { return WaveReadLaneAt(x, sir_first(WaveActiveBallot(e).x)); }
uint sir_rfl64(uint x, bool e) {
    uint4 b = WaveActiveBallot(e);
    uint k = b.x != 0u ? firstbitlow(b.x) : (b.y != 0u ? 32u + firstbitlow(b.y) : 0u);
    return WaveReadLaneAt(x, k);
}
uint sir_rfl2(uint x0, uint x1, bool e0, bool e1) {
    uint lo = WaveActiveBallot(e0).x, hi = WaveActiveBallot(e1).x;
    uint a = WaveReadLaneAt(x0, sir_first(lo)), b = WaveReadLaneAt(x1, sir_first(hi));
    return (lo == 0u && hi != 0u) ? b : a;
}
// v_mbcnt_lo/hi: the set bits of a mask dword below console lane l
uint sir_mbcnt_lo(uint m, uint l) { return l >= 32u ? countbits(m) : countbits(m & ((1u << l) - 1u)); }
uint sir_mbcnt_hi(uint m, uint l) { return l < 32u ? 0u : countbits(m & ((1u << (l - 32u)) - 1u)); }
"#;

/// Whether `name` is a per-lane name that a doubled program carries twice.
fn per_lane(id: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    matches!(id, "ex" | "vcc" | "sir_lane" | "sir_any")
        || id.strip_prefix('v').is_some_and(digits)
        || id.strip_prefix('m').is_some_and(digits)
        || id.strip_prefix("sir_h").is_some_and(digits)
}

/// Calls `f` for each identifier of `text` (with its byte range), skipping numbers and `.member`s.
fn idents(text: &str, mut f: impl FnMut(usize, usize)) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphabetic() || c == b'_' || c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let member = start > 0 && b[start - 1] == b'.';
            if !c.is_ascii_digit() && !member {
                f(start, i);
            }
        } else {
            i += 1;
        }
    }
}

/// Whether the statements name any per-lane value.
pub fn has_lane_names(text: &str) -> bool {
    let mut any = false;
    idents(text, |a, b| any |= per_lane(&text[a..b]));
    any
}

/// The statements for half `h`: every per-lane name gets `_h`.
pub fn rename(text: &str, h: u8) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut last = 0;
    idents(text, |a, b| {
        if per_lane(&text[a..b]) {
            out.push_str(&text[last..b]);
            let _ = write!(out, "_{h}");
            last = b;
        }
    });
    out.push_str(&text[last..]);
    out
}

/// The register declarations of a non-agnostic program.
pub fn declarations(l: &Lifted) -> String {
    let mut s = String::new();
    for r in &l.sgprs {
        let _ = writeln!(s, "    uint s{r} = 0;");
    }
    let doubled = l.wave == Wave::Doubled;
    let both = |s: &mut String, ty: &str, n: &str, init: &str| {
        if doubled {
            let _ = writeln!(s, "    {ty} {n}_0 = {init}, {n}_1 = {init};");
        } else {
            let _ = writeln!(s, "    {ty} {n} = {init};");
        }
    };
    for r in &l.vgprs {
        both(&mut s, "uint", &format!("v{r}"), "0");
    }
    for m in &l.masks {
        both(&mut s, "bool", &if *m == 106 { "vcc".into() } else { format!("m{m}") }, "false");
    }
    both(&mut s, "bool", "ex", "true");
    s.push_str("    bool scc = false;\n");
    match l.wave {
        Wave::Doubled => s.push_str("    bool sir_any_0 = false, sir_any_1 = false;\n    uint sir_lane_0 = WaveGetLaneIndex(), sir_lane_1 = sir_lane_0 + 32u;\n"),
        Wave::Native32 => s.push_str("    uint sir_lane = WaveGetLaneIndex() & 31u;\n"),
        Wave::TwoWave => s.push_str("    sir_flat = sir_flat_of(tid);\n    uint sir_lane = sir_flat & 63u;\n"),
        _ => s.push_str("    uint sir_lane = WaveGetLaneIndex();\n"),
    }
    s
}

/// A compute program's entry for its wave mapping: the attributes and `numthreads` line, and the thread-id
/// VGPRs' statements. `None`: the plain one (agnostic).
pub fn cs_entry(l: &Lifted, cs: &ComputeIo) -> Option<(String, String)> {
    let [x, y, z] = cs.threads.map(|t| t.max(1));
    let ids = cs.thread_id_vgprs().min(3);
    match l.wave {
        Wave::Agnostic => None,
        Wave::Doubled => {
            let n = x * y * z;
            let host = n.div_ceil(64) * 32;
            let mut init = String::from("    uint sir_f_0 = ((tid.x >> 5u) << 6u) | (tid.x & 31u), sir_f_1 = sir_f_0 + 32u;\n");
            for h in 0..2 {
                for k in 0..ids {
                    if l.vgprs.contains(&u16::from(k)) {
                        let e = match k {
                            0 => format!("sir_f_{h} % {x}u"),
                            1 => format!("(sir_f_{h} / {x}u) % {y}u"),
                            _ => format!("sir_f_{h} / {}u", x * y),
                        };
                        let _ = writeln!(init, "    v{k}_{h} = {e};");
                    }
                }
                if n % 64 != 0 {
                    let _ = writeln!(init, "    ex_{h} = sir_f_{h} < {n}u;");
                }
            }
            Some((format!("[WaveSize(32)]\n[numthreads({host}, 1, 1)]\n"), init))
        }
        _ => {
            let mut init = String::new();
            let pre = if l.wave == Wave::TwoWave {
                let n = x * y * z;
                format!(
                    r#"groupshared uint sir_xw[{n}];
groupshared uint sir_bw[{}];
static uint sir_flat;
uint sir_flat_of(uint3 t) {{ return t.x + {x}u * (t.y + {y}u * t.z); }}
// the console wave's 64-bit ballot: each host wave's half through groupshared memory
uint2 sir_ballot2(bool b) {{
    uint m = WaveActiveBallot(b).x;
    GroupMemoryBarrierWithGroupSync();
    if (WaveIsFirstLane()) {{ sir_bw[sir_flat >> 5] = m; }}
    GroupMemoryBarrierWithGroupSync();
    uint k = (sir_flat >> 6) << 1;
    return uint2(sir_bw[k], sir_bw[k + 1]);
}}
bool sir_any2(bool b) {{ uint2 m = sir_ballot2(b); return (m.x | m.y) != 0u; }}
// another console lane's value, through groupshared memory
uint sir_xread(uint x, uint k) {{
    GroupMemoryBarrierWithGroupSync();
    sir_xw[sir_flat] = x;
    GroupMemoryBarrierWithGroupSync();
    return sir_xw[(sir_flat & ~63u) | (k & 63u)];
}}
uint sir_rflw(uint x, bool e) {{
    uint2 b = sir_ballot2(e);
    uint k = b.x != 0u ? firstbitlow(b.x) : (b.y != 0u ? 32u + firstbitlow(b.y) : 0u);
    return sir_xread(x, k);
}}
"#,
                    n.div_ceil(32)
                )
            } else {
                String::new()
            };
            for k in 0..ids {
                if l.vgprs.contains(&u16::from(k)) {
                    let _ = writeln!(init, "    v{k} = tid.{};", ["x", "y", "z"][k as usize]);
                }
            }
            let attr = if matches!(l.wave, Wave::Native32 | Wave::TwoWave) { "[WaveSize(32)]\n" } else { "" };
            Some((format!("{pre}{attr}[numthreads({x}, {y}, {z})]\n"), init))
        }
    }
}

impl Lifter<'_> {
    pub(super) fn set_wave(&mut self, w: Wave) {
        self.wave = w;
        self.out.wave = w;
        if let (Wave::Doubled, Stage::Cs(io)) = (w, &self.stage) {
            if io.threads.iter().map(|t| (*t).max(1)).product::<u32>() % 64 != 0 {
                self.entry_narrowed = true;
                self.f.narrowed = true;
                self.out.exec_narrowed = true;
            }
        }
    }

    /// After one instruction: its per-lane statements twice in a doubled program, between `pre` and `post`.
    pub(super) fn finish(&mut self, start: usize) {
        if self.pre.is_empty() && self.post.is_empty() && self.wave != Wave::Doubled {
            return;
        }
        let chunk = self.out.body.split_off(start);
        let mut s = std::mem::take(&mut self.pre);
        if self.wave == Wave::Doubled && has_lane_names(&chunk) {
            s.push_str(&rename(&chunk, 0));
            s.push_str(&rename(&chunk, 1));
        } else {
            s.push_str(&chunk);
        }
        s.push_str(&std::mem::take(&mut self.post));
        self.out.body.push_str(&s);
    }

    /// The host-uniform test "any lane of this mask is on" (VCC: 106, EXEC: 126), for branches.
    pub(super) fn any_lane(&mut self, code: u32) -> String {
        let m = if code == 106 { self.mask_name(106) } else { "ex".into() };
        if self.wave == Wave::Doubled {
            format!("WaveActiveAnyTrue({m}_0 || {m}_1)")
        } else if self.wave == Wave::TwoWave {
            format!("sir_any2({m})")
        } else {
            format!("WaveActiveAnyTrue({m})")
        }
    }

    fn temp(&mut self) -> String {
        self.tmp += 1;
        format!("sir_t{}", self.tmp)
    }

    /// A lane mask (EXEC, VCC or a traced SGPR pair) read as a number: its ballot, taken once before the
    /// instruction (host control flow is uniform there).
    pub(super) fn mask_as_data(&mut self, code: u32) -> Option<String> {
        if self.wave == Wave::Agnostic {
            return None;
        }
        let (m, dword) = match enc::src(code) {
            Src::ExecLo => ("ex".to_string(), 0),
            Src::ExecHi => ("ex".to_string(), 1),
            Src::VccLo => (self.mask_name(106), 0),
            Src::VccHi => (self.mask_name(106), 1),
            Src::Sgpr(n) if self.f.mask_cur.contains(&n) => (self.mask_name(n), 0),
            Src::Sgpr(n) if self.wave64 && n > 0 && self.f.mask_cur.contains(&(n - 1)) => (self.mask_name(n - 1), 1),
            _ => return None,
        };
        let e = match (self.wave, dword) {
            (Wave::Native32, 1) => return Some("0u".into()),
            (Wave::Native32, _) => format!("WaveActiveBallot({m}).x"),
            (Wave::Partial64, 0) => format!("WaveActiveBallot({m}).x"),
            (Wave::Partial64, _) => format!("WaveActiveBallot({m}).y"),
            (Wave::TwoWave, 0) => format!("sir_ballot2({m}).x"),
            (Wave::TwoWave, _) => format!("sir_ballot2({m}).y"),
            _ => format!("WaveActiveBallot({m}_{dword}).x"),
        };
        let t = self.temp();
        let _ = writeln!(self.pre, "    uint {t} = {e};");
        Some(t)
    }

    /// After a lane mask is written to SGPR `n` (pair in wave64): its data form, written once after the instruction,
    /// so the SGPRs are never stale (a join may meet a mask on one path and data on the other).
    pub(super) fn mask_data(&mut self, n: u8) {
        let m = if n == 106 { "vcc".to_string() } else { format!("m{n}") };
        self.out.sgprs.insert(n);
        let lines: Vec<(u8, String)> = match self.wave {
            Wave::Native32 => vec![(n, format!("WaveActiveBallot({m}).x"))],
            Wave::Partial64 => vec![(n, format!("WaveActiveBallot({m}).x")), (n + 1, format!("WaveActiveBallot({m}).y"))],
            Wave::Doubled => vec![(n, format!("WaveActiveBallot({m}_0).x")), (n + 1, format!("WaveActiveBallot({m}_1).x"))],
            // (a ballot here crosses barriers DXC cannot remove: the pair goes stale instead, as in agnostic programs)
            Wave::Agnostic | Wave::TwoWave => return,
        };
        for (s, e) in lines {
            self.out.sgprs.insert(s);
            let _ = writeln!(self.post, "    s{s} = {e};");
        }
    }

    /// An SGPR (pair) holding data, read as a lane mask: this console lane's bit.
    pub(super) fn data_as_mask(&mut self, n: u8) -> Option<String> {
        if self.wave == Wave::Agnostic {
            return None;
        }
        let lo = self.src(u32::from(n), None)?;
        if !self.wave64 {
            return Some(format!("((({lo}) >> sir_lane) & 1u) != 0"));
        }
        let hi = self.src(u32::from(n) + 1, None)?;
        Some(format!("((((sir_lane & 32u) != 0 ? {hi} : {lo}) >> (sir_lane & 31u)) & 1u) != 0"))
    }

    /// The VGPR an operand field names, if it is one.
    fn vgpr_of(&mut self, d: &Decoded, field: &str) -> Option<u16> {
        match enc::src(d.get(field)?) {
            Src::Vgpr(n) => {
                self.out.vgprs.insert(u16::from(n));
                Some(u16::from(n))
            }
            _ => None,
        }
    }

    /// `v_readfirstlane_b32`: the value in the first lane EXEC has on.
    pub(super) fn readfirstlane(&mut self, d: &Decoded) -> Option<()> {
        let dst = d.get("vdst")?;
        let Some(v) = self.vgpr_of(d, "src0") else {
            let x = self.usrc(d, "src0")?;
            return self.swrite(dst, x);
        };
        let e = match self.wave {
            // agnostic programs read a uniform value here (the classifier or the uniformity pass checked it)
            Wave::Agnostic => format!("WaveReadLaneFirst(v{v})"),
            Wave::Doubled if !self.f.narrowed => format!("WaveReadLaneFirst(v{v}_0)"),
            Wave::TwoWave if !self.f.narrowed => format!("sir_xread(v{v}, 0u)"),
            Wave::Doubled => format!("sir_rfl2(v{v}_0, v{v}_1, ex_0, ex_1)"),
            _ if !self.f.narrowed => format!("WaveReadLaneFirst(v{v})"),
            Wave::Native32 => format!("sir_rfl32(v{v}, ex)"),
            Wave::Partial64 => format!("sir_rfl64(v{v}, ex)"),
            Wave::TwoWave => format!("sir_rflw(v{v}, ex)"),
        };
        self.swrite(dst, e)
    }

    /// Cross-lane VALU operations of wave-wide programs: `v_readlane`, `v_writelane`, `v_mbcnt`, `v_permlane`.
    pub(super) fn cross_valu(&mut self, name: &str, d: &Decoded) -> Option<()> {
        // bit operations the ballot idioms use (lane-local, any class)
        if let Some(f) = match name {
            "v_bfrev_b32" => Some("reversebits(@)"),
            // the lowest set bit; -1 for none (HLSL gives the same)
            "v_ffbl_b32" => Some("firstbitlow(@)"),
            // leading zeros (the first set bit from the top); -1 for none
            "v_ffbh_u32" => Some("((@) == 0u ? 0xffffffffu : 31u - firstbithigh(@))"),
            _ => None,
        } {
            let x = self.usrc(d, "src0")?;
            self.vwrite(d, f.replace('@', &x));
            return Some(());
        }
        if self.wave == Wave::Agnostic {
            return None;
        }
        let last = if self.wave64 { 63 } else { 31 };
        match name {
            "v_readlane_b32" => {
                let v = self.vgpr_of(d, "src0")?;
                let k = self.usrc(d, "src1")?;
                let e = match self.wave {
                    Wave::Native32 => format!("WaveReadLaneAt(v{v}, ({k}) & 31u)"),
                    Wave::Partial64 => format!("sir_rl64(v{v}, ({k}) & 63u)"),
                    Wave::TwoWave => format!("sir_xread(v{v}, ({k}) & 63u)"),
                    _ => format!("sir_rl2(v{v}_0, v{v}_1, ({k}) & 63u)"),
                };
                self.swrite(d.get("vdst")?, e)
            }
            "v_writelane_b32" => {
                // not guarded by EXEC
                let (x, k) = (self.usrc(d, "src0")?, self.usrc(d, "src1")?);
                let v = d.get("vdst")? as u16;
                self.out.vgprs.insert(v);
                self.line(format!("if (sir_lane == (({k}) & {last}u)) v{v} = {x};"));
                Some(())
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                let (m, acc) = (self.usrc(d, "src0")?, self.usrc(d, "src1")?);
                let f = if name.contains("_lo_") { "sir_mbcnt_lo" } else { "sir_mbcnt_hi" };
                self.vwrite(d, format!("{f}({m}, sir_lane) + {acc}"));
                Some(())
            }
            "v_permlane16_b32" | "v_permlanex16_b32" => {
                let v = self.vgpr_of(d, "src0")?;
                let (s1, s2) = (self.usrc(d, "src1")?, self.usrc(d, "src2")?);
                let opsel = d.get("opsel")?;
                let (fi, bound) = (opsel & 1 == 1, opsel & 2 == 2);
                let row = if name == "v_permlane16_b32" { "(sir_l & ~15u)" } else { "((sir_l & ~31u) | ((sir_l & 16u) ^ 16u))" };
                self.line(format!(
                    "{{ uint sir_l = WaveGetLaneIndex(); uint sir_w = (sir_l & 8u) != 0u ? {s2} : {s1}; uint sir_s = {row} | ((sir_w >> ((sir_l & 7u) * 4u)) & 15u);"
                ));
                self.line(format!("  uint sir_dpp = WaveReadLaneAt(v{v}, sir_s); bool sir_dv = true;"));
                if self.f.narrowed && !fi {
                    // (every wave intrinsic outside any short-circuit: all host lanes must reach it)
                    self.line("  bool sir_ok = WaveReadLaneAt((uint)ex, sir_s) != 0u;".into());
                    self.line(if bound { "  if (!sir_ok) sir_dpp = 0u;".into() } else { "  sir_dv = sir_ok;".into() });
                }
                self.dpp = Some(true);
                let dst = d.get("vdst")? as u16;
                self.vwrite_raw(dst, "sir_dpp".into());
                self.dpp = None;
                self.line("}".into());
                Some(())
            }
            _ => None,
        }
    }

    /// Scalar operations that read lane masks as numbers: bit counts and bit scans, 32 and 64 bits.
    pub(super) fn cross_salu(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let sdst = d.get("sdst")?;
        let code = d.get("ssrc0")?;
        // scalar operations the wave-wide programs use beside their ballots
        let two = |l: &mut Self| -> Option<(String, String)> { Some((l.usrc(d, "ssrc0")?, l.usrc(d, "ssrc1")?)) };
        let simple = match name {
            "s_mul_hi_u32" => {
                let (x, y) = two(self)?;
                Some((format!("(uint)(((uint64_t){x} * (uint64_t){y}) >> 32)"), false))
            }
            "s_mul_hi_i32" => {
                let (x, y) = two(self)?;
                Some((format!("(uint)(((int64_t)(int){x} * (int64_t)(int){y}) >> 32)"), false))
            }
            "s_not_b32" => Some((format!("~{}", self.usrc(d, "ssrc0")?), true)),
            _ => None,
        };
        if let Some((e, scc)) = simple {
            self.line(format!("{{ uint r = {e};"));
            if scc {
                self.line("  scc = r != 0u;".into());
            }
            self.swrite(sdst, "r".into())?;
            self.line("}".into());
            return Some(());
        }
        let wide = name.ends_with("_b64");
        if !matches!(
            name.trim_end_matches("_b64").trim_end_matches("_b32"),
            "s_bcnt1_i32" | "s_bcnt0_i32" | "s_ff1_i32" | "s_ff0_i32" | "s_flbit_i32"
        ) {
            return None;
        }
        let (lo, hi) = self.src_pair(code, d.literal, wide)?;
        let (e, scc) = match name.trim_end_matches("_b64").trim_end_matches("_b32") {
            "s_bcnt1_i32" => (format!("countbits({lo}) + countbits({hi})"), true),
            "s_bcnt0_i32" => (format!("{}u - countbits({lo}) - countbits({hi})", if wide { 64 } else { 32 }), true),
            "s_ff1_i32" => (format!("({lo}) != 0u ? firstbitlow({lo}) : (({hi}) != 0u ? 32u + firstbitlow({hi}) : 0xffffffffu)"), false),
            "s_ff0_i32" => (format!("~({lo}) != 0u ? firstbitlow(~({lo})) : (~({hi}) != 0u ? 32u + firstbitlow(~({hi})) : 0xffffffffu)"), false),
            _ if wide => (
                format!("({hi}) != 0u ? 31u - firstbithigh({hi}) : (({lo}) != 0u ? 63u - firstbithigh({lo}) : 0xffffffffu)"),
                false,
            ),
            _ => (format!("({lo}) != 0u ? 31u - firstbithigh({lo}) : 0xffffffffu"), false),
        };
        self.line(format!("{{ uint r = {e};"));
        if scc {
            self.line("  scc = r != 0u;".into());
        }
        self.swrite(sdst, "r".into())?;
        self.line("}".into());
        Some(())
    }

    /// A 64-bit scalar operand as two dwords (the high one `0u` for a 32-bit operation).
    fn src_pair(&mut self, code: u32, literal: Option<u32>, wide: bool) -> Option<(String, String)> {
        let lo = self.src(code, literal)?;
        if !wide {
            return Some((lo, "0u".into()));
        }
        let hi = match enc::src(code) {
            Src::Sgpr(_) | Src::VccLo | Src::ExecLo => self.src(code + 1, None)?,
            Src::Inline(c @ 193..=208) => {
                let _ = c;
                "0xffffffffu".into()
            }
            Src::Inline(_) | Src::Literal => "0u".into(),
            _ => return None,
        };
        Some((lo, hi))
    }

    /// DPP16 (any pattern, row and bank masks) and DPP8 in a program whose lanes are known: each lane reads
    /// src0 from the lane the pattern names within its row of 16 (DPP8: its group of 8), through
    /// `WaveReadLaneAt` with a per-lane index. A source lane outside the row, or off in EXEC without `fi`,
    /// gives 0 with `bound_ctrl` (DPP8: always) and otherwise leaves the destination as it was.
    pub(super) fn dpp_wide(&mut self, name: &str, d: &Decoded, src: u16, dpp8: Option<u32>) -> Option<()> {
        if name.starts_with("v_cmp") || d.get("src1_abs") == Some(1) || d.get("src1_neg") == Some(1) {
            self.why = Some("dpp form".into());
            return None;
        }
        self.out.vgprs.insert(src);
        let (from, inside, fi, zero, masks) = match dpp8 {
            Some(sel) => ("(sir_l & ~7u) | ((SEL >> ((sir_l & 7u) * 3u)) & 7u)".replace("SEL", &format!("{sel}u")), None, d.get("src0")? == enc::SRC_DPP8_FI, true, None),
            None => {
                let ctrl = d.get("dpp_ctrl")?;
                let n = ctrl & 15;
                let (from, inside) = match ctrl {
                    0..=0xFF => (format!("(sir_l & ~3u) | (({ctrl}u >> ((sir_l & 3u) * 2u)) & 3u)"), None),
                    0x101..=0x10F => (format!("sir_l + {n}u"), Some(format!("(sir_l & 15u) + {n}u < 16u"))),
                    0x111..=0x11F => (format!("sir_l - {n}u"), Some(format!("(sir_l & 15u) >= {n}u"))),
                    0x121..=0x12F => (format!("(sir_l & ~15u) | (((sir_l & 15u) - {n}u) & 15u)"), None),
                    0x140 => ("sir_l ^ 15u".into(), None),
                    0x141 => ("sir_l ^ 7u".into(), None),
                    0x150..=0x15F => (format!("(sir_l & ~15u) | {n}u"), None),
                    0x160..=0x16F => (format!("sir_l ^ {n}u"), None),
                    _ => {
                        self.why = Some(format!("dpp_ctrl {ctrl:#x}"));
                        return None;
                    }
                };
                let (rows, banks) = (d.get("row_mask")?, d.get("bank_mask")?);
                let masks = (rows != 0xF || banks != 0xF).then_some((rows, banks));
                (from, inside, d.get("fi")? == 1, d.get("bound_ctrl")? == 1, masks)
            }
        };
        let s = match &inside {
            Some(c) => format!("({c}) ? {from} : sir_l"),
            None => from,
        };
        self.line(format!("{{ uint sir_l = WaveGetLaneIndex(); uint sir_s = {s};"));
        self.line(format!("  uint sir_dpp = WaveReadLaneAt(v{src}, sir_s);"));
        let mut ok: Vec<String> = inside.into_iter().collect();
        if self.f.narrowed && !fi {
            // read before any short-circuit test: every host lane must reach a wave intrinsic
            self.line("  bool sir_on = WaveReadLaneAt((uint)ex, sir_s) != 0u;".into());
            ok.push("sir_on".into());
        }
        let mut enable = Vec::new();
        if let Some((r, b)) = masks {
            enable.push(format!("(({r}u >> ((sir_lane >> 4u) & 3u)) & 1u) != 0u && (({b}u >> ((sir_lane >> 2u) & 3u)) & 1u) != 0u"));
        }
        if !ok.is_empty() {
            let ok = ok.join(" && ");
            if zero {
                self.line(format!("  if (!({ok})) sir_dpp = 0u;"));
            } else {
                enable.push(format!("({ok})"));
            }
        }
        let guard = !enable.is_empty();
        if guard {
            self.line(format!("  bool sir_dv = {};", enable.join(" && ")));
        }
        self.dpp = Some(guard);
        let r = self.valu(name, d);
        self.dpp = None;
        self.line("}".into());
        r
    }

    /// `ds_bpermute_b32`: each lane reads data0 of lane ((addr + offset) / 4) of its wave; a source lane off in
    /// EXEC gives 0. No LDS is touched.
    pub(super) fn bpermute(&mut self, d: &Decoded) -> Option<()> {
        let (a, x, dst) = (d.get("addr")? as u16, d.get("data0")? as u16, d.get("vdst")? as u16);
        self.out.vgprs.insert(a);
        self.out.vgprs.insert(x);
        let off = d.get("offset0")? | d.get("offset1")? << 8;
        let idx = format!("((v{a} + {off}u) >> 2u)");
        match self.wave {
            Wave::Native32 => {
                self.line(format!("{{ uint sir_i = {idx} & 31u; uint r = WaveReadLaneAt(v{x}, sir_i);"));
                self.line("  if (WaveReadLaneAt((uint)ex, sir_i) == 0u) r = 0u;".into());
            }
            Wave::TwoWave => {
                self.line(format!("{{ uint sir_i = {idx} & 63u; uint r = sir_xread(v{x}, sir_i);"));
                self.line("  if (sir_xread((uint)ex, sir_i) == 0u) r = 0u;".into());
            }
            Wave::Partial64 => {
                self.line(format!("{{ uint sir_i = {idx} & 63u, sir_n = WaveGetLaneCount(); uint r = WaveReadLaneAt(v{x}, sir_i % sir_n);"));
                self.line("  uint sir_e = WaveReadLaneAt((uint)ex, sir_i % sir_n);".into());
                self.line("  if (sir_i >= sir_n || sir_e == 0u) r = 0u;".into());
            }
            _ => {
                // both halves read both halves' data before either half writes (the destination may be the data)
                self.tmp += 1;
                let t = self.tmp;
                for h in 0..2 {
                    let _ = writeln!(self.pre, "    uint sir_i{t}_{h} = ((v{a}_{h} + {off}u) >> 2u) & 63u, sir_j{t}_{h} = sir_i{t}_{h} & 31u;");
                    let _ = writeln!(
                        self.pre,
                        "    uint sir_a{t}_{h} = WaveReadLaneAt(v{x}_0, sir_j{t}_{h}), sir_b{t}_{h} = WaveReadLaneAt(v{x}_1, sir_j{t}_{h});"
                    );
                    let _ = writeln!(
                        self.pre,
                        "    uint sir_ea{t}_{h} = WaveReadLaneAt((uint)ex_0, sir_j{t}_{h}), sir_eb{t}_{h} = WaveReadLaneAt((uint)ex_1, sir_j{t}_{h});"
                    );
                    let _ = writeln!(
                        self.pre,
                        "    uint sir_h{t}_{h} = (sir_i{t}_{h} & 32u) != 0u ? (sir_eb{t}_{h} != 0u ? sir_b{t}_{h} : 0u) : (sir_ea{t}_{h} != 0u ? sir_a{t}_{h} : 0u);"
                    );
                }
                self.vwrite_raw(dst, format!("sir_h{t}"));
                return Some(());
            }
        }
        self.vwrite_raw(dst, "r".into());
        self.line("}".into());
        Some(())
    }
}

// ---------------------------------------------------------------------------------------------------------
// Uniformity of v_readfirstlane operands, from the decoded instructions (flow-insensitive, conservative).

/// Every `v_readfirstlane_b32` reads a VGPR that holds the same value in every lane: then the program is
/// agnostic after all. A VGPR is uniform when no read precedes its first write (in code order: an input or a
/// loop-carried value is not), and every write is a plain VALU operation of uniform operands (SGPRs, constants,
/// uniform VGPRs) made while EXEC is whole. Memory, interpolation, DPP/SDWA, lane-mask reads (`v_cndmask`,
/// carries) and lane-dependent operations make their results varying.
pub fn rfl_uniform(insts: &[Instruction]) -> bool {
    use std::collections::BTreeSet;
    let n = insts.len();
    let name = |i: &Instruction| i.name().unwrap_or("");
    // where EXEC may be narrowed: after the first write of EXEC, and in any loop that writes it
    let exec_write = |i: &Instruction, d: &Decoded| -> bool {
        let nm = name(i);
        match i.encoding {
            Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopk => {
                nm.contains("saveexec") || nm.contains("wrexec") || d.get("sdst").is_some_and(|c| c == 126 || c == 127)
            }
            Encoding::Vopc => nm.starts_with("v_cmpx"),
            Encoding::Vop3 => nm.starts_with("v_cmpx") || (nm.starts_with("v_cmp") && d.get("vdst").is_some_and(|c| c == 126 || c == 127)),
            _ => false,
        }
    };
    let decoded: Vec<Decoded> = insts.iter().map(enc::decode).collect();
    let mut narrowed = vec![false; n];
    if let Some(first) = (0..n).find(|&k| exec_write(&insts[k], &decoded[k])) {
        for x in narrowed.iter_mut().skip(first + 1) {
            *x = true;
        }
        // loops: a backward branch from k to t covers t..=k
        for k in 0..n {
            let i = &insts[k];
            if i.encoding == Encoding::Sopp && (name(i) == "s_branch" || name(i).starts_with("s_cbranch")) {
                let target = i.offset as i64 + 4 + 4 * i64::from(i.words[0] as u16 as i16);
                let t = insts.partition_point(|x| (x.offset as i64) < target);
                if t <= k && (t..=k).any(|j| exec_write(&insts[j], &decoded[j])) {
                    for x in narrowed.iter_mut().take(k + 1).skip(t) {
                        *x = true;
                    }
                }
            }
        }
    }
    // per instruction: VGPRs read, VGPRs written, and whether the writes may be uniform
    struct Use {
        reads: Vec<u16>,
        writes: Vec<u16>,
        plain: bool,
    }
    let vg = |d: &Decoded, f: &str| d.get(f).and_then(|c| if c >= 256 { Some((c - 256) as u16) } else { None });
    let uses: Vec<Use> = insts
        .iter()
        .zip(&decoded)
        .map(|(i, d)| {
            let nm = name(i);
            let wide = |r: u16| if nm.contains("64") { vec![r, r + 1] } else { vec![r] };
            match i.encoding {
                Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc | Encoding::Vop3 | Encoding::Vop3p => {
                    let mut reads: Vec<u16> = ["src0", "src1", "src2"].iter().filter_map(|f| vg(d, f)).flat_map(wide).collect();
                    if matches!(i.encoding, Encoding::Vop2 | Encoding::Vopc) {
                        if let Some(v) = d.get("vsrc1") {
                            reads.extend(wide(v as u16));
                        }
                    }
                    if let Some((_, x)) = &d.extra {
                        reads.push(x[0] as u16);
                    }
                    let sgpr_dst = nm.starts_with("v_cmp") || nm == "v_readfirstlane_b32" || nm == "v_readlane_b32" || i.encoding == Encoding::Vopc;
                    let writes = if sgpr_dst { vec![] } else { d.get("vdst").map(|v| wide(v as u16)).unwrap_or_default() };
                    let lane = d.extra.is_some()
                        || nm == "v_cndmask_b32"
                        || nm.contains("_co_ci_")
                        || nm.starts_with("v_div_fmas")
                        || nm.starts_with("v_mbcnt")
                        || nm.starts_with("v_permlane")
                        || nm == "v_writelane_b32"
                        || nm.starts_with("v_interp");
                    Use { reads, writes, plain: !lane }
                }
                Encoding::Exp | Encoding::Sopp | Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc | Encoding::Sopk | Encoding::Smem => {
                    Use { reads: vec![], writes: vec![], plain: true }
                }
                _ => {
                    // memory and interpolation: results varying (up to five registers, conservatively)
                    let dst = ["vdst", "vdata"].iter().find_map(|f| d.get(f)).map(|v| v as u16);
                    Use { reads: vec![], writes: dst.map(|v| (v..v + 5).collect()).unwrap_or_default(), plain: false }
                }
            }
        })
        .collect();
    let mut varying: BTreeSet<u16> = BTreeSet::new();
    let mut written: BTreeSet<u16> = BTreeSet::new();
    for u in &uses {
        for r in &u.reads {
            if !written.contains(r) {
                varying.insert(*r);
            }
        }
        written.extend(u.writes.iter().copied());
    }
    for (k, u) in uses.iter().enumerate() {
        if !u.plain || narrowed[k] {
            varying.extend(u.writes.iter().copied());
        }
    }
    loop {
        let before = varying.len();
        for u in &uses {
            if u.reads.iter().any(|r| varying.contains(r)) {
                varying.extend(u.writes.iter().copied());
            }
        }
        if varying.len() == before {
            break;
        }
    }
    insts.iter().zip(&decoded).all(|(i, d)| name(i) != "v_readfirstlane_b32" || vg(d, "src0").is_none_or(|v| !varying.contains(&v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_doubles_per_lane_names_only() {
        let t = "    if (ex) v3 = asuint(asfloat(v12) + asfloat(s4)) + v3_1 + sir_lane + m6.x + 0x1fu + sir_h2;\n";
        assert!(has_lane_names(t));
        assert_eq!(rename(t, 1), "    if (ex_1) v3_1 = asuint(asfloat(v12_1) + asfloat(s4)) + v3_1 + sir_lane_1 + m6_1.x + 0x1fu + sir_h2_1;\n");
        assert!(!has_lane_names("    s4 = s5 + 2u; scc = WaveActiveAnyTrue(vcc_0 || vcc_1);\n"));
    }

    fn decode(words: &[u32]) -> Vec<Instruction> {
        let bytes: Vec<u8> = words.iter().chain(&[0xBF81_0000]).flat_map(|w| w.to_le_bytes()).collect();
        rebuild_shader::gfx10::decode(&bytes).unwrap()
    }

    #[test]
    fn readfirstlane_of_a_uniform_vgpr_is_agnostic_and_of_a_lane_value_is_not() {
        // v_mov_b32 v1, s0; v_readfirstlane_b32 s2, v1
        let uniform = decode(&[0x7E02_0200, 0x7E04_0501]);
        assert!(rfl_uniform(&uniform));
        assert_eq!(Wave::choose(WaveClass::ReadFirstLane, true, true, &uniform), Wave::Agnostic);
        // v_readfirstlane_b32 s2, v0 (the thread id)
        let varying = decode(&[0x7E04_0500]);
        assert!(!rfl_uniform(&varying));
        assert_eq!(Wave::choose(WaveClass::ReadFirstLane, true, true, &varying), Wave::Doubled);
        assert_eq!(Wave::choose(WaveClass::ReadFirstLane, true, false, &varying), Wave::Partial64);
        assert_eq!(Wave::choose(WaveClass::WaveWide, false, true, &varying), Wave::Native32);
    }

    #[test]
    fn a_doubled_program_runs_each_vector_instruction_once_per_half_and_scalar_work_once() {
        // v_add_nc_u32 v1, s0, v0; s_add_u32 s1, s0, 1; v_readfirstlane_b32 s2, v1
        let insts = decode(&[0x4A02_0000, 0x8001_8100, 0x7E04_0501]);
        let io = ComputeIo { rsrc2: 0, threads: [64, 1, 1] };
        let l = super::super::lift_program_wave(&insts, Stage::Cs(io), None, true, Wave::Doubled);
        assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
        assert!(l.body.contains("v1_0 = s0 + v0_0;") && l.body.contains("v1_1 = s0 + v0_1;"), "{}", l.body);
        assert_eq!(l.body.matches("s1 = r;").count(), 1, "{}", l.body);
        // EXEC is whole at entry (64 threads): the first lane of the console wave is host lane 0 of half 0
        assert!(l.body.contains("s2 = WaveReadLaneFirst(v1_0);"), "{}", l.body);
        let (attr, ids) = cs_entry(&l, &io).unwrap();
        assert!(attr.contains("[WaveSize(32)]") && attr.contains("[numthreads(32, 1, 1)]"));
        assert!(ids.contains("v0_1 = sir_f_1 % 64u;") && !ids.contains("ex_1"));
        // a group of 40: the lanes past it start with EXEC off, and the first-lane read becomes exact
        let io = ComputeIo { rsrc2: 0, threads: [40, 1, 1] };
        let l = super::super::lift_program_wave(&insts, Stage::Cs(io), None, true, Wave::Doubled);
        assert!(l.body.contains("sir_rfl2(v1_0, v1_1, ex_0, ex_1)"), "{}", l.body);
        assert!(cs_entry(&l, &io).unwrap().1.contains("ex_1 = sir_f_1 < 40u;"));
    }
}
