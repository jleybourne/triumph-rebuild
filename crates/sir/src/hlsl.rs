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

pub const VIN: usize = 2;

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
    let mut stack: Vec<(String, Lowering)> = Vec::new();
    let mut masks = 0usize;

    for (i, op) in p.insts.iter().enumerate() {
        let v = |x: crate::V| format!("v{}", x.0);
        match *op {
            Op::If(c) => {
                let (r, low) = by_at(i).unwrap();
                let _ = r;
                let uni = uniform[c.0 as usize];
                stack.push((exec.clone(), low));
                match (mode, low) {
                    (Mode::Agnostic, Lowering::HostBranch) => {
                        body.push_str(&format!("{}if ({} != 0u) {{\n", pad(indent), v(c)));
                        indent += 1;
                    }
                    (Mode::Agnostic, Lowering::Predicated) => {}
                    (Mode::Doubled, Lowering::HostBranch) => {
                        let test = if uni { format!("{}.x != 0u", v(c)) } else { format!("any({} != 0u)", v(c)) };
                        body.push_str(&format!("{}if ({test}) {{\n", pad(indent)));
                        indent += 1;
                    }
                    (Mode::Doubled, Lowering::Predicated) => {
                        masks += 1;
                        let m = format!("e{masks}");
                        body.push_str(&format!("{}bool2 {m} = and({exec}, {} != 0u);\n", pad(indent), v(c)));
                        exec = m;
                    }
                }
                continue;
            }
            Op::Else => {
                let (outer, low) = stack.last().cloned().unwrap();
                let (r, _) = by_at(i).unwrap();
                let c = r.cond;
                let uni = uniform[c.0 as usize];
                match (mode, low) {
                    (Mode::Agnostic, Lowering::HostBranch) => body.push_str(&format!("{}}} else {{\n", pad(indent - 1))),
                    (Mode::Agnostic, Lowering::Predicated) => {}
                    (Mode::Doubled, Lowering::HostBranch) => {
                        if uni {
                            body.push_str(&format!("{}}} else {{\n", pad(indent - 1)));
                        } else {
                            body.push_str(&format!("{}}}\n{}if (any({} == 0u)) {{\n", pad(indent - 1), pad(indent - 1), v(c)));
                        }
                    }
                    (Mode::Doubled, Lowering::Predicated) => {
                        masks += 1;
                        let m = format!("e{masks}");
                        body.push_str(&format!("{}bool2 {m} = and({outer}, {} == 0u);\n", pad(indent), v(c)));
                        exec = m;
                    }
                }
                continue;
            }
            Op::EndIf => {
                let (outer, low) = stack.pop().unwrap();
                if low == Lowering::HostBranch {
                    indent -= 1;
                    body.push_str(&format!("{}}}\n", pad(indent)));
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
            (Mode::Agnostic, op) => panic!("{op:?} is wave-wide; classify the program first"),
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
    match mode {
        Mode::Agnostic => s.push_str("[numthreads(64, 1, 1)] [WaveSize(32)]\nvoid main(uint l : SV_GroupIndex) {\n"),
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
        if !matches!(op, Op::If(_) | Op::Else | Op::EndIf) {
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
        assert!(h.contains("bool2 e1 = and(e, v0 != 0u);") && h.contains("e1.x ? v0.x"), "{h}");
        assert!(!h.contains("if ("), "a predicated region has no host branch:\n{h}");
    }
}
