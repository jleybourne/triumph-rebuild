//! HLSL from the IR, compiled offline by DXC (`docs/design/shader-ir.md`).
//!
//! Test layout (the GPU harness and the interpreter use the same one):
//!
//! ```text
//! In  (t0)   vin[n][64] u32 · sin[4] u32 · exec lo u32 · exec hi u32
//! Out (u0)   out[k][64] u32
//! ```
//!
//! [`Mode::Agnostic`] runs one console lane per thread (64 threads, two host waves): only for
//! programs [`crate::classify`] calls agnostic. [`Mode::Doubled`] runs one host wave of 32 threads,
//! each carrying console lanes `h` and `h + 32` as a `uint2`, with EXEC as data.
//!
//! **Live masks** ([`plan`]): each `If` becomes a real host branch when its condition is uniform or
//! its region crosses no lanes; otherwise it is predicated (both parts run, EXEC narrows as data,
//! the phi selects). Wave and quad operations therefore never run with host lanes switched off.

use crate::{Op, Program, Region};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Agnostic,
    Doubled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lowering {
    /// a real branch on the host: the condition is uniform, or nothing inside reads other lanes
    HostBranch,
    /// both parts run; EXEC narrows as data; the merge is a select
    Predicated,
}

/// varying inputs: 0-1 random, 2 = lane × 4 (a per-lane address offset), 3 = finite half pairs
pub const VIN: usize = 4;

pub fn sin_offset() -> usize {
    VIN * 64 * 4
}
pub fn exec_offset() -> usize {
    sin_offset() + 4 * 4
}

/// The lowering of every region, in `If` order.
pub fn plan(p: &Program) -> Vec<(Region, Lowering)> {
    let uniform = p.uniformity();
    p.regions()
        .into_iter()
        .map(|r| {
            let crosses = p.insts[r.at as usize + 1..r.end as usize].iter().any(|o| o.crosses_lanes());
            let low = if uniform[r.cond.0 as usize] || !crosses { Lowering::HostBranch } else { Lowering::Predicated };
            (r, low)
        })
        .collect()
}

pub fn emit(p: &Program, mode: Mode) -> String {
    let plan = plan(p);
    let uniform = p.uniformity();
    let by_at = |i: usize| plan.iter().find(|(r, _)| r.at as usize == i || r.else_at == Some(i as u32) || r.end as usize == i).copied();
    let t = if mode == Mode::Doubled { "uint2" } else { "uint" };

    let mut body = String::new();
    let mut indent = 1usize;
    let pad = |n: usize| "    ".repeat(n);
    // the current EXEC expression (doubled mode), and the stack of (outer EXEC, region lowering)
    let mut exec = String::from("e");
    let mut stack: Vec<(String, Lowering, String, bool, crate::V)> = Vec::new();
    let mut masks = 0usize;

    for (i, op) in p.insts.iter().enumerate() {
        let v = |x: crate::V| format!("v{}", x.0);
        match *op {
            Op::If(c) => {
                let (_, low) = by_at(i).unwrap();
                let uni = uniform[c.0 as usize];
                // both parts' EXEC, made here while the condition is at hand; kept in both modes so
                // that stores are always masked, whether or not the region is a host branch
                masks += 1;
                let (et, ef) = (format!("e{masks}t"), format!("e{masks}f"));
                match mode {
                    Mode::Agnostic => body.push_str(&format!(
                        "{p}bool {et} = {exec} && ({c} != 0u);
{p}bool {ef} = {exec} && ({c} == 0u);
",
                        p = pad(indent),
                        c = v(c)
                    )),
                    Mode::Doubled => body.push_str(&format!(
                        "{p}bool2 {et} = and({exec}, {c} != 0u);
{p}bool2 {ef} = and({exec}, {c} == 0u);
",
                        p = pad(indent),
                        c = v(c)
                    )),
                }
                stack.push((exec.clone(), low, ef, uni, c));
                if low == Lowering::HostBranch {
                    let test = match (mode, uni) {
                        (Mode::Agnostic, _) => format!("{} != 0u", v(c)),
                        (Mode::Doubled, true) => format!("{}.x != 0u", v(c)),
                        (Mode::Doubled, false) => format!("any({} != 0u)", v(c)),
                    };
                    body.push_str(&format!("{}if ({test}) {{
", pad(indent)));
                    indent += 1;
                }
                exec = et;
                continue;
            }
            Op::Else => {
                let (_, low, ef, uni, c) = stack.last().cloned().unwrap();
                if low == Lowering::HostBranch {
                    if mode == Mode::Agnostic || uni {
                        body.push_str(&format!("{}}} else {{
", pad(indent - 1)));
                    } else {
                        body.push_str(&format!("{}}}
{}if (any({} == 0u)) {{
", pad(indent - 1), pad(indent - 1), v(c)));
                    }
                }
                exec = ef;
                continue;
            }
            Op::EndIf => {
                let (outer, low, ..) = stack.pop().unwrap();
                if low == Lowering::HostBranch {
                    indent -= 1;
                    body.push_str(&format!("{}}}
", pad(indent)));
                }
                exec = outer;
                continue;
            }
            _ => {}
        }
        let e = &exec;
        let rhs = match (mode, *op) {
            (_, Op::Const(c)) => format!("{t}({c}u{})", if mode == Mode::Doubled { format!(", {c}u") } else { String::new() }),
            (_, Op::SInput(n)) => {
                let x = format!("In.Load({})", sin_offset() + n as usize * 4);
                if mode == Mode::Doubled { format!("uint2({x}, {x})") } else { x }
            }
            (Mode::Agnostic, Op::VInput(n)) => format!("In.Load({} + l * 4)", n as usize * 256),
            (Mode::Doubled, Op::VInput(n)) => format!("uint2(In.Load({o} + h * 4), In.Load({o} + (h + 32) * 4))", o = n as usize * 256),
            (Mode::Agnostic, Op::LaneId) => "l".into(),
            (Mode::Doubled, Op::LaneId) => "uint2(h, h + 32)".into(),
            (_, Op::Add(a, b)) => format!("{} + {}", v(a), v(b)),
            (_, Op::Mul(a, b)) => format!("{} * {}", v(a), v(b)),
            (_, Op::And(a, b)) => format!("{} & {}", v(a), v(b)),
            (_, Op::Or(a, b)) => format!("{} | {}", v(a), v(b)),
            (_, Op::Xor(a, b)) => format!("{} ^ {}", v(a), v(b)),
            (_, Op::Shr(a, b)) => format!("{} >> ({} & 31u)", v(a), v(b)),
            (_, Op::Lt(a, b)) => format!("{t}({} < {})", v(a), v(b)),
            (_, Op::Select(c, a, b)) | (_, Op::Phi(c, a, b)) => format!("select({} != 0u, {}, {})", v(c), v(a), v(b)),
            // agnostic programs reach these only with uniform operands or quad masks
            (Mode::Agnostic, Op::ReadFirstLane(a)) => v(a),
            (Mode::Agnostic, Op::Swizzle(a, m)) => match m & 63 {
                0 => v(a),
                1 => format!("QuadReadAcrossX({})", v(a)),
                2 => format!("QuadReadAcrossY({})", v(a)),
                3 => format!("QuadReadAcrossDiagonal({})", v(a)),
                _ => v(a), // uniform operand: every lane holds the same value
            },
            (Mode::Doubled, Op::WaveSum(a)) => format!("WaveActiveSum(({e}.x ? {a}.x : 0u) + ({e}.y ? {a}.y : 0u)).xx", a = v(a)),
            (Mode::Doubled, Op::WaveMin(a)) => format!("WaveActiveMin(min({e}.x ? {a}.x : 0xFFFFFFFFu, {e}.y ? {a}.y : 0xFFFFFFFFu)).xx", a = v(a)),
            (Mode::Doubled, Op::BallotLo(a)) => format!("WaveActiveBallot({e}.x && {}.x != 0u).xx", v(a)),
            (Mode::Doubled, Op::BallotHi(a)) => format!("WaveActiveBallot({e}.y && {}.y != 0u).xx", v(a)),
            (Mode::Doubled, Op::ReadLane(a, k)) => {
                let k = k as u32 & 63;
                if k < 32 { format!("WaveReadLaneAt({}.x, {k}u).xx", v(a)) } else { format!("WaveReadLaneAt({}.y, {}u).xx", v(a), k - 32) }
            }
            (Mode::Doubled, Op::ReadFirstLane(a)) => format!("rfl({}, {e})", v(a)),
            (Mode::Doubled, Op::Swizzle(a, m)) => {
                let m = m as u32 & 63;
                let (flip, xm) = (m & 32 != 0, m & 31);
                let (from_lo, from_hi) = if flip { ("y", "x") } else { ("x", "y") };
                format!("uint2(WaveReadLaneAt({a}.{from_lo}, h ^ {xm}u), WaveReadLaneAt({a}.{from_hi}, h ^ {xm}u))", a = v(a))
            }
            (Mode::Agnostic, Op::Load(lo, hi)) => format!("{}({}, {})", if uniform[hi.0 as usize] { "ldu" } else { "ld" }, v(lo), v(hi)),
            (Mode::Doubled, Op::Load(lo, hi)) => {
                let f = if uniform[hi.0 as usize] { "ldu" } else { "ld" };
                format!("uint2({f}({lo}.x, {hi}.x), {f}({lo}.y, {hi}.y))", lo = v(lo), hi = v(hi))
            }
            (Mode::Agnostic, Op::Store(lo, hi, x)) => {
                body.push_str(&format!("{}if ({e}) st({}, {}, {});
", pad(indent), v(lo), v(hi), v(x)));
                continue;
            }
            (Mode::Doubled, Op::Store(lo, hi, x)) => {
                body.push_str(&format!(
                    "{p}if ({e}.x) st({lo}.x, {hi}.x, {x}.x);
{p}if ({e}.y) st({lo}.y, {hi}.y, {x}.y);
",
                    p = pad(indent),
                    lo = v(lo),
                    hi = v(hi),
                    x = v(x)
                ));
                continue;
            }
            (Mode::Agnostic, Op::PkAddF16(a, b)) => format!("pkf16({}, {}, false)", v(a), v(b)),
            (Mode::Agnostic, Op::PkMulF16(a, b)) => format!("pkf16({}, {}, true)", v(a), v(b)),
            (Mode::Doubled, Op::PkAddF16(a, b)) => format!("uint2(pkf16({a}.x, {b}.x, false), pkf16({a}.y, {b}.y, false))", a = v(a), b = v(b)),
            (Mode::Doubled, Op::PkMulF16(a, b)) => format!("uint2(pkf16({a}.x, {b}.x, true), pkf16({a}.y, {b}.y, true))", a = v(a), b = v(b)),
            (Mode::Agnostic, op) => panic!("{op:?} is wave-wide; classify the program first"),
            (_, Op::If(_) | Op::Else | Op::EndIf) => unreachable!(),
        };
        body.push_str(&format!("{}v{i} = {rhs};\n", pad(indent)));
    }

    let mut s = String::new();
    s.push_str("ByteAddressBuffer In : register(t0);\nRWByteAddressBuffer Out : register(u0);\n");
    if mode == Mode::Doubled && p.insts.iter().any(|o| matches!(o, Op::ReadFirstLane(_))) {
        // the first active console lane: lo half first, then hi; none active: lane 0 (index uniform)
        s.push_str(
            "uint2 rfl(uint2 a, bool2 e) {\n    uint bl = WaveActiveBallot(e.x).x;\n    uint bh = WaveActiveBallot(e.y).x;\n    uint r = bl != 0u ? WaveReadLaneAt(a.x, firstbitlow(bl)) : (bh != 0u ? WaveReadLaneAt(a.y, firstbitlow(bh)) : WaveReadLaneAt(a.x, 0u));\n    return r.xx;\n}\n",
        );
    }
    if p.insts.iter().any(|o| matches!(o, Op::Load(..) | Op::Store(..))) {
        // console memory: the window is a bindless raw buffer at heap slot `hi`; the offset is 32 bits.
        // A wave-uniform window reads its descriptor once (WaveReadLaneFirst); a varying one is
        // NonUniformResourceIndex. No page directory, no clamp: an unmapped tile reads 0, drops writes.
        s.push_str(concat!(
            "uint ld(uint lo, uint hi) { RWByteAddressBuffer w = ResourceDescriptorHeap[NonUniformResourceIndex(hi)]; return w.Load(lo & ~3u); }\n",
            "uint ldu(uint lo, uint hi) { RWByteAddressBuffer w = ResourceDescriptorHeap[WaveReadLaneFirst(hi)]; return w.Load(lo & ~3u); }\n",
            "void st(uint lo, uint hi, uint x) { RWByteAddressBuffer w = ResourceDescriptorHeap[NonUniformResourceIndex(hi)]; w.Store(lo & ~3u, x); }\n",
        ));
    }
    if p.insts.iter().any(|o| matches!(o, Op::PkAddF16(..) | Op::PkMulF16(..))) {
        // native half precision: two float16_t lanes in a u32, round to nearest even
        s.push_str(concat!(
            "uint pkf16(uint a, uint b, bool mul) {\n",
            "    float16_t2 x = asfloat16(uint16_t2(uint16_t(a & 0xFFFFu), uint16_t(a >> 16)));\n",
            "    float16_t2 y = asfloat16(uint16_t2(uint16_t(b & 0xFFFFu), uint16_t(b >> 16)));\n",
            "    uint16_t2 r = asuint16(mul ? x * y : x + y);\n",
            "    return uint(r.x) | (uint(r.y) << 16);\n",
            "}\n",
        ));
    }
    match mode {
        Mode::Agnostic => {
            s.push_str("[numthreads(64, 1, 1)] [WaveSize(32)]\nvoid main(uint l : SV_GroupIndex) {\n");
            s.push_str(&format!(
                "    bool e = (((l < 32u) ? In.Load({lo}) : In.Load({hi})) >> (l & 31u)) & 1u;\n",
                lo = exec_offset(),
                hi = exec_offset() + 4
            ));
        }
        Mode::Doubled => {
            s.push_str("[numthreads(32, 1, 1)] [WaveSize(32)]\nvoid main(uint h : SV_GroupIndex) {\n");
            s.push_str(&format!(
                "    bool2 e = bool2((In.Load({lo}) >> h) & 1, (In.Load({hi}) >> h) & 1);\n",
                lo = exec_offset(),
                hi = exec_offset() + 4
            ));
        }
    }
    // every value declared up front, so values flow out of branches into phis
    for (i, op) in p.insts.iter().enumerate() {
        if !matches!(op, Op::If(_) | Op::Else | Op::EndIf | Op::Store(..)) {
            s.push_str(&format!("    {t} v{i} = ({t})0;\n"));
        }
    }
    s.push_str(&body);
    for (k, o) in p.outputs.iter().enumerate() {
        match mode {
            Mode::Agnostic => s.push_str(&format!("    Out.Store({} + l * 4, v{});\n", k * 256, o.0)),
            Mode::Doubled => {
                s.push_str(&format!("    Out.Store({} + h * 4, v{}.x);\n", k * 256, o.0));
                s.push_str(&format!("    Out.Store({} + (h + 32) * 4, v{}.y);\n", k * 256, o.0));
            }
        }
    }
    s.push_str("}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Op, Program};

    #[test]
    fn a_doubled_reduction_carries_exec_as_data() {
        let mut p = Program::default();
        let x = p.push(Op::VInput(0));
        let s = p.push(Op::WaveSum(x));
        p.outputs.push(s);
        let h = emit(&p, Mode::Doubled);
        assert!(h.contains("WaveActiveSum((e.x ? v0.x : 0u) + (e.y ? v0.y : 0u))"), "{h}");
        assert!(h.contains("[WaveSize(32)]"));
    }

    #[test]
    #[should_panic(expected = "wave-wide")]
    fn agnostic_emission_refuses_wave_wide_ops() {
        let mut p = Program::default();
        let x = p.push(Op::VInput(0));
        p.push(Op::WaveSum(x));
        emit(&p, Mode::Agnostic);
    }

    fn branchy(wave_in_then: bool, uniform_cond: bool) -> Program {
        let mut p = Program::default();
        let x = p.push(Op::VInput(0));
        let s = p.push(Op::SInput(0));
        let c = if uniform_cond { s } else { x };
        p.push(Op::If(c));
        let a = if wave_in_then { p.push(Op::WaveSum(x)) } else { p.push(Op::Add(x, x)) };
        p.push(Op::Else);
        let b = p.push(Op::Mul(x, x));
        p.push(Op::EndIf);
        let m = p.push(Op::Phi(c, a, b));
        p.outputs.push(m);
        p
    }

    #[test]
    fn live_masks_choose_branch_or_predication() {
        assert_eq!(plan(&branchy(false, false))[0].1, Lowering::HostBranch, "no cross-lane op inside");
        assert_eq!(plan(&branchy(true, true))[0].1, Lowering::HostBranch, "uniform condition");
        assert_eq!(plan(&branchy(true, false))[0].1, Lowering::Predicated, "wave op under a varying condition");
        let h = emit(&branchy(true, false), Mode::Doubled);
        assert!(h.contains("bool2 e1t = and(e, v0 != 0u);") && h.contains("e1t.x ? v0.x"), "{h}");
        assert!(!h.contains("if ("), "a predicated region has no host branch:\n{h}");
    }
}
