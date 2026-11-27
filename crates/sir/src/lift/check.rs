//! Execution checks for lifted forms (`sirtest`): small GFX10 programs lifted to HLSL, run on a device over
//! 64 lanes of inputs, and compared with the console's result computed here. Each program reads v0-v2 from
//! the input buffer (`In`, lane-major per register) and writes its output VGPRs to `Out`.
//!
//! Covered: the bit scans and byte permute, SDWA operand selects and destination merges, and the buffer
//! format unpack of typed fetches ([`super::fmt`]). Integer and fp16-derived results are compared bit for
//! bit; a normalised format is allowed `ulp` units in the last place (HLSL's float division is not exact), and
//! a float format's NaN matches any NaN (the device picks the payload).

use rebuild_shader::gfx10::enc::{self, Layout};
use rebuild_shader::gfx10::{self, Encoding};

use super::fmt;
use super::{declarations, lift, PRELUDE};

/// One check: an HLSL compute program and its console reference.
pub struct Case {
    pub name: String,
    pub hlsl: String,
    /// output VGPRs the program writes, in `Out` order
    pub outputs: usize,
    /// allowed difference in units in the last place (0: bit-exact)
    pub ulp: u32,
    /// a NaN result matches any NaN (the payload of an fp16 NaN widened to f32 is the device's: NVIDIA's
    /// `f16tof32` returns the canonical 0x7fffffff, WARP keeps the payload)
    pub nan_any: bool,
    /// the outputs are f16 halves: a NaN half matches any NaN half
    pub half_nan: bool,
    reference: Box<dyn Fn([u32; 3]) -> Vec<u32>>,
}

impl Case {
    /// The console's outputs for one lane's v0-v2.
    pub fn expect(&self, v: [u32; 3]) -> Vec<u32> {
        (self.reference)(v)
    }

    /// Whether `got` matches `want` within the case's tolerance.
    pub fn matches(&self, got: u32, want: u32) -> bool {
        let nan = |x: u32| f32::from_bits(x).is_nan();
        got == want
            || (self.ulp > 0 && (got as i32).wrapping_sub(want as i32).unsigned_abs() <= self.ulp)
            || (self.nan_any && nan(got) && nan(want))
            || (self.half_nan && (0..2).all(|k| {
                let (g, w) = (got >> (16 * k) & 0xffff, want >> (16 * k) & 0xffff);
                let hnan = |x: u32| x & 0x7c00 == 0x7c00 && x & 0x3ff != 0;
                g == w || (hnan(g) && hnan(w))
            }))
    }
}

/// Input values for lane `l` of a run seeded `seed`: edge values first, then pseudo-random ones.
pub fn inputs(seed: u64, l: usize) -> [u32; 3] {
    const EDGE: [u32; 8] = [0, 1, 0xffff_ffff, 0x8000_0000, 0x7fff_ffff, 0x0000_ff00, 0x00ff_0080, 0x3c00_bc00];
    let mut x = seed ^ (l as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u32
    };
    let mut v = [next(), next(), next()];
    if l < 8 {
        v[0] = EDGE[l];
        v[1] = EDGE[(l + 3) % 8];
    }
    v
}

fn op(e: Encoding, n: &str) -> u32 {
    (0..1024u16).find(|&o| gfx10::name(e, o) == Some(n)).unwrap_or_else(|| panic!("no {n} in {e:?}")) as u32
}
fn vop1(n: &str, vdst: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | op(Encoding::Vop1, n) << 9 | src0
}
fn vop2(n: &str, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op(Encoding::Vop2, n) << 25 | vdst << 17 | vsrc1 << 9 | src0
}
fn vopc(n: &str, src0: u32, vsrc1: u32) -> u32 {
    0x7C00_0000 | op(Encoding::Vopc, n) << 17 | vsrc1 << 9 | src0
}
fn vop3(n: &str, vdst: u32, s: [u32; 3]) -> [u32; 2] {
    [0x35 << 26 | op(Encoding::Vop3, n) << 16 | vdst, s[2] << 18 | s[1] << 9 | s[0]]
}
/// A two-word instruction from named fields of its layout (tag bits set, `op` from the name).
fn build(layout: &Layout, e: Encoding, n: &str, fields: &[(&str, u32)]) -> Vec<u32> {
    let mut w = vec![0u32; layout.words as usize];
    let mut put = |word: u8, lo: u8, width: u8, v: u32| {
        let m = (((1u64 << width) - 1) << lo) as u32;
        w[word as usize] = (w[word as usize] & !m) | ((v << lo) & m);
    };
    for &(word, lo, width, v) in layout.tag {
        put(word, lo, width, v);
    }
    let all = [("op", op(e, n))];
    for (name, v) in all.iter().copied().chain(fields.iter().copied()) {
        let f = layout.fields.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no field {name}"));
        put(f.word, f.lo, f.width, v);
    }
    w
}

fn word(layout: &Layout, fields: &[(&str, u32)]) -> u32 {
    fields.iter().fold(0, |w, (n, v)| w | v << layout.fields.iter().find(|f| f.name == *n).unwrap().lo)
}
const V: u32 = 256;

/// A compute program around a lifted body: v0-v2 from `In`, the outputs to `Out`.
fn program(words: &[u32], outs: &[u16]) -> String {
    let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    bytes.extend(0xBF81_0000u32.to_le_bytes()); // s_endpgm
    let insts = gfx10::decode(&bytes).expect("check program decodes");
    let l = lift(&insts);
    assert!(l.unsupported.is_empty(), "check program not lifted: {:?}", l.unsupported);
    let mut l = l;
    l.vgprs.extend([0u16, 1, 2]);
    wrap(&declarations(&l), &l.body, outs)
}

fn wrap(decls: &str, body: &str, outs: &[u16]) -> String {
    let mut s = String::from(PRELUDE);
    s.push_str("ByteAddressBuffer In : register(t0);\nRWByteAddressBuffer Out : register(u0);\n");
    s.push_str("[numthreads(64, 1, 1)]\nvoid main(uint l : SV_GroupIndex) {\n");
    s.push_str(decls);
    for r in 0..3 {
        s.push_str(&format!("    v{r} = In.Load(({r} * 64 + l) * 4);\n"));
    }
    s.push_str(body);
    for (k, r) in outs.iter().enumerate() {
        s.push_str(&format!("    Out.Store(({k} * 64 + l) * 4, v{r});\n"));
    }
    s.push_str("}\n");
    s
}

fn ffbh_i(x: u32) -> u32 {
    match x {
        0 | 0xffff_ffff => u32::MAX,
        x if x >> 31 == 1 => (!x).leading_zeros(),
        x => x.leading_zeros(),
    }
}

/// The console's `v_perm_b32`.
pub fn perm(a: u32, b: u32, sel: u32) -> u32 {
    let src = (u64::from(a) << 32) | u64::from(b);
    (0..4).fold(0, |r, i| {
        let s = sel >> (8 * i) & 0xff;
        let x = match s {
            0..=7 => (src >> (8 * s)) as u32 & 0xff,
            8..=11 => {
                let bit = [15, 31, 47, 63][s as usize - 8];
                if src >> bit & 1 == 1 { 0xff } else { 0 }
            }
            12 => 0,
            _ => 0xff,
        };
        r | x << (8 * i)
    })
}

/// The console's value of component `c` of an element whose first dword is `e` (one-dword formats).
pub fn unpack(f: &fmt::Format, e: u32, c: usize) -> u32 {
    let Some(&b) = f.bits.get(c) else {
        return match (c, f.num.is_float()) {
            (3, true) => 1.0f32.to_bits(),
            (3, false) => 1,
            _ => 0,
        };
    };
    let o: u32 = f.bits[..c].iter().sum();
    let raw = if b == 32 { e } else { (e >> o) & ((1u32 << b) - 1) };
    let sext = ((raw << (32 - b)) as i32) >> (32 - b);
    let max_u = ((1u64 << b) - 1) as f32;
    let max_s = ((1u64 << (b - 1)) - 1) as f32;
    match f.num {
        fmt::Num::Uint => raw,
        fmt::Num::Sint => sext as u32,
        fmt::Num::Unorm => (raw as f32 / max_u).to_bits(),
        fmt::Num::Snorm => (sext as f32 / max_s).max(-1.0).to_bits(),
        fmt::Num::Uscaled => (raw as f32).to_bits(),
        fmt::Num::Sscaled => (sext as f32).to_bits(),
        fmt::Num::Float => match b {
            32 => raw,
            16 => f16_to_f32(raw),
            _ => f16_to_f32(raw << (15 - b)),
        },
    }
}

/// IEEE half to single, exactly (denormals and NaNs included).
pub fn f16_to_f32(h: u32) -> u32 {
    let (s, e, m) = (h >> 15 & 1, h >> 10 & 0x1f, h & 0x3ff);
    let v = match (e, m) {
        (0, 0) => 0.0f32,
        (0, m) => m as f32 * 2f32.powi(-24),
        (31, 0) => f32::INFINITY,
        (31, m) => return s << 31 | 0x7f80_0000 | m << 13,
        (e, m) => (1.0 + m as f32 / 1024.0) * 2f32.powi(e as i32 - 15),
    };
    (if s == 1 { -v } else { v }).to_bits()
}

/// f32 to f16, rounded to nearest even (overflow to infinity, denormals kept, NaN quiet).
pub fn f32_to_f16(x: f32) -> u32 {
    f64_to_f16(f64::from(x))
}

/// f64 to f16, rounded to nearest even, in one rounding.
pub fn f64_to_f16(x: f64) -> u32 {
    let sign = if x.is_sign_negative() { 0x8000 } else { 0 };
    if x.is_nan() {
        return 0x7e00;
    }
    let a = x.abs();
    if a.is_infinite() {
        return sign | 0x7c00;
    }
    if a == 0.0 {
        return sign;
    }
    // quantise to the f16 grid at this magnitude (ulp 2^(e - 10), at least 2^-24), ties to even
    let e = ((a.to_bits() >> 52) & 0x7ff) as i32 - 1023;
    let ulp = 2f64.powi(e.max(-14) - 10);
    let v = (a / ulp).round_ties_even() * ulp;
    if v >= 65520.0 {
        return sign | 0x7c00;
    }
    if v < 2f64.powi(-14) {
        return sign | (v / 2f64.powi(-24)) as u32;
    }
    let e = ((v.to_bits() >> 52) & 0x7ff) as i32 - 1023;
    let m = ((v / 2f64.powi(e) - 1.0) * 1024.0) as u32;
    sign | ((e + 15) as u32) << 10 | m
}

fn h(x: u32) -> f64 {
    f64::from(f32::from_bits(f16_to_f32(x & 0xffff)))
}

/// Every check.
pub fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    let mut add = |name: &str, hlsl: String, outputs: usize, ulp: u32, nan_any: bool, f: Box<dyn Fn([u32; 3]) -> Vec<u32>>| {
        let half_nan = name.contains("f16");
        v.push(Case { name: name.into(), hlsl, outputs, ulp, nan_any, half_nan, reference: f });
    };
    // bit scans, reverse, not, byte permute (selector from v2: every byte kind)
    let mut w = vec![
        vop1("v_ffbl_b32", 3, V),
        vop1("v_ffbh_u32", 4, V),
        vop1("v_ffbh_i32", 5, V + 1),
        vop1("v_bfrev_b32", 6, V),
        vop1("v_not_b32", 7, V + 1),
    ];
    // the selector: each byte of v2 mod 16 (0-15 covers bytes, sign fills, zero and 0xff)
    w.push(vop2("v_and_b32", 9, 255, 2));
    w.push(0x0f0f_0f0f);
    w.extend(vop3("v_perm_b32", 8, [V, V + 1, V + 9]));
    add(
        "bit scans, bfrev, not, perm",
        program(&w, &[3, 4, 5, 6, 7, 8]),
        6,
        0,
        false,
        Box::new(|[a, b, c]| {
            let s = c & 0x0f0f_0f0f;
            vec![if a == 0 { u32::MAX } else { a.trailing_zeros() }, if a == 0 { u32::MAX } else { a.leading_zeros() }, ffbh_i(b), a.reverse_bits(), !b, perm(a, b, s)]
        }),
    );
    // SDWA: selects (byte, word, sign extension), merges (pad, sign-extend, preserve), an inline constant
    // operand, a compare into VCC
    let s = &enc::SDWA;
    let w = vec![
        vop1("v_mov_b32", 3, V + 2),
        vop2("v_and_b32", 3, enc::SRC_SDWA, 1),
        word(s, &[("src0", 0), ("src0_sel", 1), ("src1_sel", 5), ("src1_sext", 1), ("dst_sel", 5), ("dst_unused", 2)]),
        vop1("v_cvt_f32_u32", 4, enc::SRC_SDWA),
        word(s, &[("src0", 0), ("src0_sel", 5), ("dst_sel", 6)]),
        vop1("v_mov_b32", 5, enc::SRC_SDWA),
        word(s, &[("src0", 1), ("src0_sel", 3), ("src0_sext", 1), ("dst_sel", 2), ("dst_unused", 1)]),
        vop2("v_add_nc_u32", 6, enc::SRC_SDWA, 1),
        word(s, &[("src0", 128 + 5), ("s0", 1), ("src0_sel", 6), ("src1_sel", 4), ("dst_sel", 0), ("dst_unused", 0)]),
        vopc("v_cmp_lt_u32", enc::SRC_SDWA, 1),
        word(&enc::SDWAB, &[("src0", 0), ("src0_sel", 0), ("src1_sel", 2)]),
        vop2("v_cndmask_b32", 7, V, 1),
    ];
    add(
        "SDWA selects and merges",
        program(&w, &[3, 4, 5, 6, 7]),
        5,
        0,
        false,
        Box::new(|[a, b, c]| {
            let r = ((a >> 8) & 0xff) & ((b as i32 >> 16) as u32);
            let and = (c & 0xffff) | (r & 0xffff) << 16;
            let cvt = ((a >> 16) as f32).to_bits();
            let byte = ((b as i32) >> 24) as u32 & 0xff; // the sign-extended top byte, then its low 8 bits
            let mov = byte << 16 | if byte >> 7 == 1 { 0xff00_0000 } else { 0 };
            let add = 5u32.wrapping_add(b & 0xffff) & 0xff;
            let sel = if (a & 0xff) < ((b >> 16) & 0xff) { b } else { a };
            vec![and, cvt, mov, add, sel]
        }),
    );
    // 16-bit float: VOP2 add/mul, VOP3 sub with op_sel (v0's high half, written into v5's high half), max
    let w = vec![
        vop2("v_add_f16", 3, V, 1),
        vop2("v_mul_f16", 4, V, 1),
        vop1("v_mov_b32", 5, V + 2),
    ];
    let mut w = w;
    w.extend(build(&enc::VOP3A, Encoding::Vop3, "v_sub_f16", &[("vdst", 5), ("opsel", 0b1001), ("src0", V), ("src1", V + 1)]));
    w.extend(build(&enc::VOP3A, Encoding::Vop3, "v_sub_f16", &[("vdst", 6), ("src0", V), ("src1", V + 1), ("neg", 0b10)]));
    add(
        "f16 add, mul, sub with op_sel and neg",
        program(&w, &[3, 4, 5, 6]),
        4,
        0,
        true,
        Box::new(|[a, b, c]| {
            // v3, v4 and v6 start at 0: the result's high half is theirs, 0
            vec![f64_to_f16(h(a) + h(b)), f64_to_f16(h(a) * h(b)), (c & 0xffff) | f64_to_f16(h(a >> 16) - h(b)) << 16, f64_to_f16(h(a) - -h(b))]
        }),
    );
    // packed: add (default selects), mul with the halves swapped and the high half's operand negated, fma,
    // and integer halves
    let mut w = Vec::new();
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_add_f16", &[("vdst", 3), ("src0", V), ("src1", V + 1), ("opsel_hi", 3), ("opsel_hi2", 1)]));
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_mul_f16", &[("vdst", 4), ("src0", V), ("src1", V + 1), ("opsel", 0b001), ("opsel_hi", 0b10), ("opsel_hi2", 1), ("neg_hi", 0b010)]));
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_fma_f16", &[("vdst", 5), ("src0", V), ("src1", V + 1), ("src2", V + 2), ("opsel_hi", 3), ("opsel_hi2", 1)]));
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_add_u16", &[("vdst", 6), ("src0", V), ("src1", V + 1), ("opsel_hi", 3), ("opsel_hi2", 1)]));
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_lshlrev_b16", &[("vdst", 7), ("src0", V), ("src1", V + 1), ("opsel_hi", 3), ("opsel_hi2", 1)]));
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_max_i16", &[("vdst", 8), ("src0", V), ("src1", V + 1), ("opsel_hi", 3), ("opsel_hi2", 1)]));
    w.extend(build(&enc::VOP3P, Encoding::Vop3p, "v_pk_mad_u16", &[("vdst", 9), ("src0", V), ("src1", V + 1), ("src2", V + 2), ("opsel_hi", 3), ("opsel_hi2", 1)]));
    add(
        "packed f16 and u16/i16",
        program(&w, &[3, 4, 5, 6, 7, 8, 9]),
        7,
        0,
        true,
        Box::new(|[a, b, c]| {
            let pk = |lo: u32, hi: u32| (lo & 0xffff) | (hi & 0xffff) << 16;
            let s16 = |x: u32| i32::from(x as u16 as i16);
            vec![
                pk(f64_to_f16(h(a) + h(b)), f64_to_f16(h(a >> 16) + h(b >> 16))),
                pk(f64_to_f16(h(a >> 16) * h(b)), f64_to_f16(h(a) * -h(b >> 16))),
                pk(f64_to_f16(h(a).mul_add(h(b), h(c))), f64_to_f16(h(a >> 16).mul_add(h(b >> 16), h(c >> 16)))),
                pk(a.wrapping_add(b), (a >> 16).wrapping_add(b >> 16)),
                pk((b & 0xffff) << (a & 15), (b >> 16) << ((a >> 16) & 15)),
                pk(s16(a).max(s16(b)) as u32, s16(a >> 16).max(s16(b >> 16)) as u32),
                pk((a & 0xffff) * (b & 0xffff) + (c & 0xffff), (a >> 16) * (b >> 16) + (c >> 16)),
            ]
        }),
    );
    // v_cvt_pk_u8_f32 (halves of 0-255 to round to nearest even, and raw bits: NaN, infinities, huge), carry
    // chains, a 64-bit shift and compare, v_mul_hi_i32
    let mut w = vec![vop1("v_cvt_f32_ubyte0", 10, V), vop2("v_mul_f32", 10, 240, 10)];
    w.extend(vop3("v_cvt_pk_u8_f32", 3, [V + 10, 128 + 2, V + 1]));
    w.extend(vop3("v_cvt_pk_u8_f32", 4, [V, 128 + 1, V + 1]));
    w.extend(build(&enc::VOP3B, Encoding::Vop3, "v_add_co_u32", &[("vdst", 5), ("sdst", 106), ("src0", V), ("src1", V + 1)]));
    w.push(op(Encoding::Vop2, "v_add_co_ci_u32") << 25 | 6 << 17 | 2 << 9 | (V + 1));
    w.extend(vop3("v_cndmask_b32", 7, [128, 129, 106]));
    w.extend(build(&enc::VOP3B, Encoding::Vop3, "v_sub_co_u32", &[("vdst", 8), ("sdst", 106), ("src0", V), ("src1", V + 1)]));
    w.extend(vop3("v_cndmask_b32", 9, [128, 129, 106]));
    w.extend(vop3("v_lshlrev_b64", 11, [V + 2, V, 0]));
    w.extend(build(&enc::VOP3A, Encoding::Vop3, "v_cmp_lt_i64", &[("vdst", 106), ("src0", V), ("src1", V + 1)]));
    w.extend(vop3("v_cndmask_b32", 13, [128, 129, 106]));
    w.extend(vop3("v_mul_hi_i32", 14, [V, V + 1, 0]));
    add(
        "cvt_pk_u8, carries, 64-bit shift and compare, mul_hi_i32",
        program(&w, &[3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14]),
        11,
        0,
        false,
        Box::new(|[a, b, c]| {
            let u8f = |x: f32| if x.is_nan() { 0 } else { x.round_ties_even().clamp(0.0, 255.0) as u32 };
            let put = |old: u32, k: u32, x: u32| (old & !(0xff << (8 * k))) | x << (8 * k);
            let s = u64::from(a) + u64::from(b);
            let s2 = u64::from(b) + u64::from(c) + (s >> 32);
            let x = (u64::from(b) << 32 | u64::from(a)) << (c & 63);
            let p = (u64::from(b) << 32 | u64::from(a)) as i64;
            let q = (u64::from(c) << 32 | u64::from(b)) as i64;
            vec![
                put(b, 2, u8f((a & 0xff) as f32 * 0.5)),
                put(b, 1, u8f(f32::from_bits(a))),
                s as u32,
                s2 as u32,
                (s2 >> 32) as u32,
                a.wrapping_sub(b),
                u32::from(a < b),
                x as u32,
                (x >> 32) as u32,
                u32::from(p < q),
                ((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32,
            ]
        }),
    );
    // the buffer format unpack: every one-dword format, all four components of v0
    for code in 1..=61u32 {
        let Some(f) = fmt::buf_format(code) else { continue };
        if f.bits.iter().sum::<u32>() > 32 {
            continue;
        }
        let comps: Vec<String> = (0..4).map(|c| fmt::component(&f, c).unwrap()).collect();
        let body = format!(
            "    {{ uint sir_e[1] = {{ v0 }}; v3 = {}; v4 = {}; v5 = {}; v6 = {}; }}\n",
            comps[0], comps[1], comps[2], comps[3]
        );
        let decls = "    uint v0 = 0, v1 = 0, v2 = 0, v3 = 0, v4 = 0, v5 = 0, v6 = 0;\n";
        let ulp = u32::from(matches!(f.num, fmt::Num::Unorm | fmt::Num::Snorm));
        let g = f.clone();
        add(&format!("buffer format {code}"), wrap(decls, &body, &[3, 4, 5, 6]), 4, ulp, f.num == fmt::Num::Float, Box::new(move |[a, _, _]| (0..4).map(|c| unpack(&g, a, c)).collect()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_references_follow_the_isa() {
        assert_eq!(perm(0x8899_aabb, 0x0011_2233, 0x0c0d_0700), 0x00ff_8833);
        assert_eq!(perm(0x8000_0000, 0x0000_8000, 0x0b0a_0908), 0xff00_00ff);
        assert_eq!(ffbh_i(0xffff_0000), 16);
        assert_eq!(ffbh_i(0x0000_ffff), 16);
        assert_eq!(f16_to_f32(0x3c00), 1.0f32.to_bits());
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24).to_bits());
        let f = fmt::buf_format(56).unwrap(); // 8_8_8_8 unorm
        assert_eq!(unpack(&f, 0xff00_0080, 3), 1.0f32.to_bits());
        assert_eq!(unpack(&f, 0xff00_0080, 0), (128.0f32 / 255.0).to_bits());
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(65504.0), 0x7bff);
        assert_eq!(f32_to_f16(65520.0), 0x7c00);
        assert_eq!(f32_to_f16(2f32.powi(-24)), 0x0001);
        assert_eq!(f32_to_f16(2f32.powi(-25)), 0x0000); // a tie: to even (0)
        assert_eq!(f32_to_f16(3.0 * 2f32.powi(-25)), 0x0002); // a tie: to even (2)
        assert_eq!(f32_to_f16(1.0 + 2f32.powi(-11)), 0x3c00); // a tie: to even
        assert_eq!(f32_to_f16(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        assert_eq!(f32_to_f16(-2.0), 0xc000);
        for k in 0..0x7c00u32 {
            assert_eq!(f32_to_f16(f32::from_bits(f16_to_f32(k))), k, "{k:#x}");
        }
        assert_eq!(cases().len(), 5 + 61);
    }
}
