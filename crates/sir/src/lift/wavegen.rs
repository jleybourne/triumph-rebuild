//! Random wave-wide GFX10 compute programs, and a reference console that runs them lane by lane.
//!
//! The reference is a plain reading of the ISA for the instructions the generator writes: a wave of 32 or 64
//! lanes, EXEC and VCC as bit masks, SGPRs and VGPRs as numbers. It shares no code with the lifter (only the
//! decoder's field names), so a translated program that matches it on every lane checks the lifter's wave
//! mapping ([`super::Wave`]) and its cross-lane operations: ballots read as data, `v_mbcnt`, `v_readlane`,
//! `v_readfirstlane`, `v_writelane`, DPP16 (every row pattern, row and bank masks, `bound_ctrl`, `fi`), DPP8,
//! `v_permlane16` / `v_permlanex16`, `ds_bpermute`, `ds_swizzle`, 64-bit bit counts and scans of masks, lane
//! masks made from data, and EXEC narrowed by `s_and_saveexec` with `s_cbranch_execz` skips.
//!
//! Register use, so outputs stay data: v0 is the thread id, v1..v7 inputs and results; s8..s15 inputs and
//! results; s20..s23 lane masks (or data read as masks), s24.. saved EXEC per nesting depth.

use rebuild_shader::gfx10::enc::{self, Src};
use rebuild_shader::gfx10::{self, Encoding, Instruction};

use crate::gen::Rng;

pub const VGPRS: usize = 8;
/// SGPRs s8..s16 are data in and out.
pub const SDATA: std::ops::Range<usize> = 8..16;

fn op(e: Encoding, n: &str) -> u32 {
    (0..1024u16).find(|&o| gfx10::name(e, o) == Some(n)).unwrap_or_else(|| panic!("no {n} in {e:?}")) as u32
}
fn vop1(n: &str, vdst: u32, src0: u32) -> Vec<u32> {
    vec![0x7E00_0000 | vdst << 17 | op(Encoding::Vop1, n) << 9 | src0]
}
fn vop2(n: &str, vdst: u32, src0: u32, vsrc1: u32) -> Vec<u32> {
    vec![op(Encoding::Vop2, n) << 25 | vdst << 17 | vsrc1 << 9 | src0]
}
fn vopc(n: &str, src0: u32, vsrc1: u32) -> Vec<u32> {
    vec![0x7C00_0000 | op(Encoding::Vopc, n) << 17 | vsrc1 << 9 | src0]
}
fn vop3(n: &str, vdst: u32, s: [u32; 3], opsel: u32) -> Vec<u32> {
    vec![0x35 << 26 | op(Encoding::Vop3, n) << 16 | opsel << 11 | vdst, s[2] << 18 | s[1] << 9 | s[0]]
}
fn sop2(n: &str, sdst: u32, a: u32, b: u32) -> Vec<u32> {
    vec![0b10 << 30 | op(Encoding::Sop2, n) << 23 | sdst << 16 | b << 8 | a]
}
fn sop1(n: &str, sdst: u32, a: u32) -> Vec<u32> {
    vec![0x17D << 23 | sdst << 16 | op(Encoding::Sop1, n) << 8 | a]
}
fn sopp(n: &str, imm: u16) -> Vec<u32> {
    vec![0x17F << 23 | op(Encoding::Sopp, n) << 16 | u32::from(imm)]
}
fn ds(n: &str, vdst: u32, addr: u32, data0: u32, offset: u32) -> Vec<u32> {
    vec![0x36 << 26 | op(Encoding::Ds, n) << 18 | (offset >> 8 & 0xFF) << 8 | (offset & 0xFF), vdst << 24 | data0 << 8 | addr]
}

const V: u32 = 256;
const VCC: u32 = 106;
const EXEC: u32 = 126;
const M1: u32 = 193; // inline -1
const fn int(k: u32) -> u32 {
    128 + k
}

/// A random program (words, ending in `s_endpgm`) for a console wave of 64 (`wave64`) or 32 lanes.
/// `partial`: lanes 32..63 never exist (a partly filled graphics wave), so no lane write reaches them.
pub fn generate(rng: &mut Rng, wave64: bool, len: usize, partial: bool) -> Vec<u32> {
    let mut parts: Vec<Vec<u32>> = Vec::new();
    // open regions: (index of the skip branch if any, saved-EXEC SGPR)
    let mut open: Vec<(Option<usize>, u32)> = Vec::new();
    let w = if wave64 { 64 } else { 32 };
    let b = if wave64 { "_b64" } else { "_b32" };
    let pick = |rng: &mut Rng, n: u32| rng.below(n as u64) as u32;
    let vg = |rng: &mut Rng| V + rng.below(VGPRS as u64) as u32;
    let vd = |rng: &mut Rng| 1 + rng.below(VGPRS as u64 - 1) as u32;
    let sd = |rng: &mut Rng| SDATA.start as u32 + rng.below(SDATA.len() as u64) as u32;
    // a scalar or inline operand
    let ss = |rng: &mut Rng| match rng.below(4) {
        0 => int(rng.below(65) as u32),
        1 => M1,
        _ => SDATA.start as u32 + rng.below(SDATA.len() as u64) as u32,
    };
    // (a partly filled wave has no bits for lanes that never exist: no data SGPR becomes a mask there)
    let mask_src = |rng: &mut Rng| [EXEC, VCC, 20, 22, M1, 8][rng.below(if partial { 5 } else { 6 }) as usize];
    for _ in 0..len {
        let k = rng.below(24);
        let p = match k {
            0 => vop2(["v_add_nc_u32", "v_xor_b32", "v_min_u32", "v_lshrrev_b32", "v_and_b32"][pick(rng, 5) as usize], vd(rng), if rng.below(2) == 0 { vg(rng) } else { ss(rng) }, vg(rng) - V),
            1 => vop3("v_mul_lo_u32", vd(rng), [vg(rng), vg(rng), 0], 0),
            2 => vop2("v_cndmask_b32", vd(rng), vg(rng), vg(rng) - V),
            3 => vop3("v_cndmask_b32", vd(rng), [vg(rng), vg(rng), [VCC, 20, 22][pick(rng, 3) as usize]], 0),
            4 => vopc("v_cmp_gt_u32", vg(rng), vg(rng) - V),
            5 => vop3("v_cmp_lt_u32", [20, 22][pick(rng, 2) as usize], [vg(rng), ss(rng), 0], 0),
            6 => {
                // mbcnt over a mask's low then high dword (the lane-id and prefix-count idioms)
                let m = mask_src(rng);
                let d = vd(rng);
                let hi = match m {
                    EXEC | VCC | 20 | 22 | 8 => m + 1,
                    _ => m,
                };
                let mut v = vop3("v_mbcnt_lo_u32_b32", d, [m, if rng.below(2) == 0 { int(0) } else { vg(rng) }, 0], 0);
                if wave64 {
                    v.extend(vop3("v_mbcnt_hi_u32_b32", d, [hi, V + d, 0], 0));
                }
                v
            }
            7 => vop3("v_readlane_b32", sd(rng), [vg(rng), if rng.below(2) == 0 { int(rng.below(w) as u32) } else { sd(rng) }, 0], 0),
            8 => vop1("v_readfirstlane_b32", sd(rng), vg(rng)),
            9 => {
                let lanes = if partial { 32 } else { w };
                vop3("v_writelane_b32", vd(rng), [ss(rng), int(rng.below(lanes) as u32), 0], 0)
            }
            10 | 11 => {
                // DPP16
                let ctrl = match rng.below(8) {
                    0 => rng.below(256) as u32,
                    1 => 0x101 + rng.below(15) as u32,
                    2 => 0x111 + rng.below(15) as u32,
                    3 => 0x121 + rng.below(15) as u32,
                    4 => 0x140 + rng.below(2) as u32,
                    5 => 0x150 + rng.below(16) as u32,
                    _ => 0x160 + rng.below(16) as u32,
                };
                let (row, bank) = if rng.below(3) == 0 { (rng.below(16) as u32, rng.below(16) as u32) } else { (15, 15) };
                let x = (vg(rng) - V) | ctrl << 8 | (rng.below(2) as u32) << 18 | (rng.below(2) as u32) << 19 | bank << 24 | row << 28;
                let mut v = if k == 10 { vop1("v_mov_b32", vd(rng), 0xFA) } else { vop2("v_add_nc_u32", vd(rng), 0xFA, vg(rng) - V) };
                v.push(x);
                v
            }
            12 => {
                let sel = rng.below(1 << 24) as u32;
                let mut v = vop1("v_mov_b32", vd(rng), if rng.below(2) == 0 { 0xE9 } else { 0xEA });
                v.push((vg(rng) - V) | sel << 8);
                v
            }
            13 => vop3(["v_permlane16_b32", "v_permlanex16_b32"][pick(rng, 2) as usize], vd(rng), [vg(rng), sd(rng), sd(rng)], rng.below(4) as u32),
            14 => ds("ds_bpermute_b32", vd(rng), vg(rng) - V, vg(rng) - V, rng.below(4) as u32 * 4),
            15 => {
                let off = if rng.below(2) == 0 { 0x8000 | rng.below(256) as u32 } else { rng.below(0x8000) as u32 };
                ds("ds_swizzle_b32", vd(rng), vg(rng) - V, 0, off)
            }
            16 => sop1(&format!("{}{b}", ["s_bcnt1_i32", "s_ff1_i32", "s_bcnt0_i32", "s_flbit_i32"][pick(rng, 4) as usize]), sd(rng), mask_src(rng)),
            17 => sop2("s_add_u32", sd(rng), ss(rng), ss(rng)),
            18 => vop1("v_mov_b32", vd(rng), [EXEC, EXEC + 1, VCC, VCC + 1, 20, 21, 22, 23][pick(rng, if wave64 { 8 } else { 2 }) as usize * if wave64 { 1 } else { 2 }]),
            19 => sop2(&format!("{}{b}", ["s_and", "s_or", "s_xor", "s_andn2"][pick(rng, 4) as usize]), [20, 22][pick(rng, 2) as usize], [VCC, EXEC, 20, 22][pick(rng, 4) as usize], [VCC, EXEC, 20, 22][pick(rng, 4) as usize]),
            20 if partial => continue,
            20 => sop1(&format!("s_mov{b}"), [20, 22][pick(rng, 2) as usize], [8, 10, 12][pick(rng, 3) as usize]),
            21 => vop1(["v_bfrev_b32", "v_ffbl_b32", "v_ffbh_u32"][pick(rng, 3) as usize], vd(rng), vg(rng)),
            22 if open.len() < 2 => {
                // open a region: EXEC &= a compare, maybe skipped when no lane is left
                parts.push(vopc("v_cmp_gt_u32", vg(rng), vg(rng) - V));
                let save = 24 + 2 * open.len() as u32;
                parts.push(sop1(&format!("s_and_saveexec{b}"), save, VCC));
                let skip = (rng.below(2) == 0).then(|| {
                    parts.push(sopp("s_cbranch_execz", 0));
                    parts.len() - 1
                });
                open.push((skip, save));
                continue;
            }
            _ => match open.pop() {
                Some(r) => close(&mut parts, r, b),
                None => continue,
            },
        };
        parts.push(p);
    }
    while let Some(r) = open.pop() {
        let p = close(&mut parts, r, b);
        parts.push(p);
    }
    parts.push(sopp("s_endpgm", 0));
    parts.concat()
}

/// The join of a region: patches its skip branch to land here, and restores EXEC.
fn close(parts: &mut [Vec<u32>], (skip, save): (Option<usize>, u32), b: &str) -> Vec<u32> {
    if let Some(at) = skip {
        let words: usize = parts[at + 1..].iter().map(|p| p.len()).sum();
        parts[at][0] = parts[at][0] & 0xFFFF_0000 | words as u32;
    }
    sop2(&format!("s_or{b}"), EXEC, EXEC, save)
}

/// One console wave's registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveState {
    pub lanes: usize,
    pub v: Vec<[u32; 64]>,
    pub s: [u32; 128],
    pub exec: u64,
    pub vcc: u64,
    pub scc: bool,
}

impl WaveState {
    pub fn new(lanes: usize, exec: u64) -> WaveState {
        WaveState { lanes, v: vec![[0; 64]; VGPRS], s: [0; 128], exec, vcc: 0, scc: false }
    }

    fn on(&self, l: usize) -> bool {
        self.exec >> l & 1 == 1
    }

    fn full(&self) -> u64 {
        if self.lanes == 64 { u64::MAX } else { (1u64 << self.lanes) - 1 }
    }

    /// A scalar operand (32 bits).
    fn sval(&self, code: u32, literal: Option<u32>) -> u32 {
        match enc::src(code) {
            Src::Sgpr(n) => self.s[n as usize],
            Src::VccLo => self.vcc as u32,
            Src::VccHi => (self.vcc >> 32) as u32,
            Src::ExecLo => self.exec as u32,
            Src::ExecHi => (self.exec >> 32) as u32,
            Src::Inline(c @ 128..=192) => u32::from(c - 128),
            Src::Inline(c @ 193..=208) => (-(i32::from(c) - 192)) as u32,
            Src::Literal => literal.unwrap_or(0),
            other => panic!("operand {other:?}"),
        }
    }

    /// A 64-bit scalar operand (a lane mask, or a pair of data SGPRs).
    fn sval64(&self, code: u32) -> u64 {
        let m = match enc::src(code) {
            Src::VccLo => self.vcc,
            Src::ExecLo => self.exec,
            Src::Sgpr(n) => u64::from(self.s[n as usize]) | (u64::from(self.s[n as usize + 1]) << 32),
            Src::Inline(193..=208) => u64::MAX,
            Src::Inline(c @ 128..=192) => u64::from(c - 128),
            other => panic!("operand {other:?}"),
        };
        m & self.full()
    }

    fn set64(&mut self, code: u32, m: u64) {
        let m = m & self.full();
        match enc::src(code) {
            Src::VccLo => self.vcc = m,
            Src::ExecLo => self.exec = m,
            Src::Sgpr(n) => {
                self.s[n as usize] = m as u32;
                if self.lanes == 64 {
                    self.s[n as usize + 1] = (m >> 32) as u32;
                }
            }
            other => panic!("mask destination {other:?}"),
        }
    }

    /// A lane's value of an operand (VGPR, or a scalar).
    fn lval(&self, code: u32, literal: Option<u32>, l: usize) -> u32 {
        match enc::src(code) {
            Src::Vgpr(n) => self.v[n as usize][l],
            _ => self.sval(code, literal),
        }
    }
}

/// The lane DPP16 control `ctrl` reads for lane `l`, if it is in the row.
fn dpp_from(ctrl: u32, l: usize) -> Option<usize> {
    let (row, pos, n) = (l & !15, l & 15, (ctrl & 15) as usize);
    Some(match ctrl {
        0..=0xFF => (l & !3) | (ctrl as usize >> (2 * (l & 3)) & 3),
        0x101..=0x10F => (pos + n < 16).then_some(l + n)?,
        0x111..=0x11F => (pos >= n).then(|| l - n)?,
        0x121..=0x12F => row | (pos.wrapping_sub(n) & 15),
        0x140 => row | (15 - pos),
        0x141 => row | (pos & 8) | (7 - (pos & 7)),
        0x150..=0x15F => row | n,
        0x160..=0x16F => row | (pos ^ n),
        _ => panic!("dpp_ctrl {ctrl:#x}"),
    })
}

/// Runs a program on one console wave.
pub fn run(insts: &[Instruction], st: &mut WaveState) {
    let mut pc = 0usize;
    let w = st.lanes;
    let mut steps = 0;
    while pc < insts.len() {
        steps += 1;
        assert!(steps < 100_000, "runaway program");
        let i = &insts[pc];
        let name = i.name().expect("named");
        let d = enc::decode(i);
        let g = |f: &str| d.get(f).unwrap_or(0);
        pc += 1;
        match i.encoding {
            Encoding::Sopp => match name {
                "s_endpgm" => return,
                "s_cbranch_execz" => {
                    if st.exec == 0 {
                        let target = i.offset + 4 + 4 * g("simm16") as usize;
                        pc = insts.iter().position(|x| x.offset == target).expect("branch target");
                    }
                }
                _ => panic!("{name}"),
            },
            Encoding::Sop1 => {
                let wide = name.ends_with("_b64");
                let src = if wide || w == 64 && name.contains("saveexec") { st.sval64(g("ssrc0")) } else { u64::from(st.sval(g("ssrc0"), d.literal)) };
                let m = src & if wide { u64::MAX } else { 0xFFFF_FFFF };
                let dst = g("sdst");
                match name.trim_end_matches("_b64").trim_end_matches("_b32") {
                    "s_bcnt1_i32" => {
                        let r = m.count_ones();
                        st.s[dst as usize] = r;
                        st.scc = r != 0;
                    }
                    "s_bcnt0_i32" => {
                        let r = (if wide { 64 } else { 32 }) - m.count_ones();
                        st.s[dst as usize] = r;
                        st.scc = r != 0;
                    }
                    "s_ff1_i32" => st.s[dst as usize] = if m == 0 { u32::MAX } else { m.trailing_zeros() },
                    "s_flbit_i32" => {
                        let bits = if wide { 64 } else { 32 };
                        st.s[dst as usize] = if m == 0 { u32::MAX } else { m.leading_zeros() - (64 - bits) }
                    }
                    "s_mov" => st.set64(dst, src),
                    "s_and_saveexec" => {
                        let old = st.exec;
                        st.exec = src & old;
                        st.set64(dst, old);
                        st.scc = st.exec != 0;
                    }
                    _ => panic!("{name}"),
                }
            }
            Encoding::Sop2 => {
                if name == "s_add_u32" {
                    let (a, b) = (st.sval(g("ssrc0"), d.literal), st.sval(g("ssrc1"), d.literal));
                    let (r, c) = a.overflowing_add(b);
                    st.s[g("sdst") as usize] = r;
                    st.scc = c;
                    continue;
                }
                let (a, b) = (st.sval64(g("ssrc0")), st.sval64(g("ssrc1")));
                let r = match name.trim_end_matches("_b64").trim_end_matches("_b32") {
                    "s_and" => a & b,
                    "s_or" => a | b,
                    "s_xor" => a ^ b,
                    "s_andn2" => a & !b,
                    _ => panic!("{name}"),
                } & st.full();
                st.set64(g("sdst"), r);
                st.scc = r != 0;
            }
            Encoding::Vopc => {
                let mut m = 0u64;
                for l in 0..w {
                    if st.on(l) && st.lval(g("src0"), d.literal, l) > st.v[g("vsrc1") as usize][l] {
                        m |= 1 << l;
                    }
                }
                st.vcc = m;
            }
            Encoding::Ds => {
                let (dst, addr, data) = (g("vdst") as usize, g("addr") as usize, g("data0") as usize);
                let off = (g("offset0") | g("offset1") << 8) as usize;
                let before = st.clone();
                for l in 0..w {
                    if !before.on(l) {
                        continue;
                    }
                    st.v[dst][l] = if name == "ds_bpermute_b32" {
                        let k = ((before.v[addr][l] as usize + off) >> 2) & (w - 1);
                        if before.on(k) { before.v[data][k] } else { 0 }
                    } else if off & 0x8000 != 0 {
                        before.v[addr][(l & !3) | (off >> (2 * (l & 3)) & 3)]
                    } else {
                        let (and, or, xor) = (off & 31, off >> 5 & 31, off >> 10 & 31);
                        before.v[addr][(l & !31) | ((((l & 31) & and) | or) ^ xor)]
                    };
                }
            }
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vop3 => {
                let before = st.clone();
                let lane_src = |f: &str, l: usize| before.lval(d.get(f).unwrap(), d.literal, l);
                let dst = g("vdst");
                if let Some((x, xv)) = &d.extra {
                    // DPP16 / DPP8: src0 from another lane of the row
                    let src = xv[0] as usize;
                    for l in 0..w {
                        if !before.on(l) {
                            continue;
                        }
                        let (from, fi, zero, write) = if x.name == "dpp8" {
                            let sel = xv[1];
                            ((l & !7) | (sel as usize >> (3 * (l & 7)) & 7), g("src0") == enc::SRC_DPP8_FI, true, true)
                        } else {
                            let (ctrl, fi, bc, bank, row) = (xv[1], xv[2], xv[3], xv[8], xv[9]);
                            let write = row >> ((l >> 4) & 3) & 1 == 1 && bank >> ((l >> 2) & 3) & 1 == 1;
                            match dpp_from(ctrl, l) {
                                Some(f) => (f, fi == 1, bc == 1, write),
                                None => (usize::MAX, fi == 1, bc == 1, write),
                            }
                        };
                        let valid = from != usize::MAX && (fi || before.on(from));
                        if !write || (!valid && !zero) {
                            continue;
                        }
                        let a = if valid { before.v[src][from] } else { 0 };
                        st.v[dst as usize][l] = match name {
                            "v_mov_b32" => a,
                            "v_add_nc_u32" => a.wrapping_add(before.v[g("vsrc1") as usize][l]),
                            _ => panic!("{name} dpp"),
                        };
                    }
                    continue;
                }
                match name {
                    "v_readlane_b32" => st.s[dst as usize] = before.v[(g("src0") - V) as usize][lane_src("src1", 0) as usize & (w - 1)],
                    "v_readfirstlane_b32" => {
                        let k = if before.exec == 0 { 0 } else { before.exec.trailing_zeros() as usize };
                        st.s[dst as usize] = lane_src("src0", k);
                    }
                    "v_writelane_b32" => st.v[dst as usize][lane_src("src1", 0) as usize & (w - 1)] = lane_src("src0", 0),
                    n if n.starts_with("v_cmp_lt_u32") => {
                        let mut m = 0u64;
                        for l in 0..w {
                            if before.on(l) && lane_src("src0", l) < lane_src("src1", l) {
                                m |= 1 << l;
                            }
                        }
                        st.set64(dst, m);
                    }
                    _ => {
                        for l in 0..w {
                            if !before.on(l) {
                                continue;
                            }
                            let a = lane_src("src0", l);
                            let b = || if i.encoding == Encoding::Vop2 { before.v[g("vsrc1") as usize][l] } else { lane_src("src1", l) };
                            let mask = |code: u32| before.sval64(code) >> l & 1 == 1;
                            let r = match name {
                                "v_mov_b32" => a,
                                "v_add_nc_u32" => a.wrapping_add(b()),
                                "v_xor_b32" => a ^ b(),
                                "v_and_b32" => a & b(),
                                "v_min_u32" => a.min(b()),
                                "v_lshrrev_b32" => b() >> (a & 31),
                                "v_mul_lo_u32" => a.wrapping_mul(b()),
                                "v_cndmask_b32" => {
                                    let c = if i.encoding == Encoding::Vop2 { mask(VCC) } else { mask(g("src2")) };
                                    if c { b() } else { a }
                                }
                                "v_mbcnt_lo_u32_b32" => {
                                    let below = if l >= 32 { u32::MAX } else { (1u32 << l) - 1 };
                                    (a & below).count_ones() + b()
                                }
                                "v_mbcnt_hi_u32_b32" => {
                                    let below = if l < 32 { 0 } else { (1u32 << (l - 32)) - 1 };
                                    (a & below).count_ones() + b()
                                }
                                "v_bfrev_b32" => a.reverse_bits(),
                                "v_ffbl_b32" => if a == 0 { u32::MAX } else { a.trailing_zeros() },
                                "v_ffbh_u32" => if a == 0 { u32::MAX } else { a.leading_zeros() },
                                "v_permlane16_b32" | "v_permlanex16_b32" => {
                                    let (s1, s2, opsel) = (lane_src("src1", 0), lane_src("src2", 0), g("opsel"));
                                    let word = if l & 8 != 0 { s2 } else { s1 };
                                    let pick = (word >> (4 * (l & 7)) & 15) as usize;
                                    let row = if name == "v_permlane16_b32" { l & !15 } else { (l & !31) | ((l & 16) ^ 16) };
                                    let from = row | pick;
                                    if opsel & 1 == 1 || before.on(from) {
                                        before.v[(g("src0") - V) as usize][from]
                                    } else if opsel & 2 == 2 {
                                        0
                                    } else {
                                        continue;
                                    }
                                }
                                _ => panic!("{name}"),
                            };
                            st.v[dst as usize][l] = r;
                        }
                    }
                }
            }
            _ => panic!("{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_programs_decode_and_run() {
        let mut rng = Rng(7);
        for k in 0..200 {
            let wave64 = k % 2 == 0;
            let words = generate(&mut rng, wave64, 40, false);
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let insts = gfx10::decode(&bytes).expect("decodes");
            let lanes = if wave64 { 64 } else { 32 };
            let mut st = WaveState::new(lanes, if wave64 { u64::MAX } else { 0xFFFF_FFFF });
            run(&insts, &mut st);
        }
    }
}
