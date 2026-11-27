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

use crate::{Op, Program};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Agnostic,
    Doubled,
}

pub const VIN: usize = 2;

pub fn sin_offset() -> usize {
    VIN * 64 * 4
}
pub fn exec_offset() -> usize {
    sin_offset() + 4 * 4
}

pub fn emit(p: &Program, mode: Mode) -> String {
    let mut s = String::new();
    s.push_str("ByteAddressBuffer In : register(t0);\nRWByteAddressBuffer Out : register(u0);\n");
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
    let t = if mode == Mode::Doubled { "uint2" } else { "uint" };
    for (i, op) in p.insts.iter().enumerate() {
        let v = |x: crate::V| format!("v{}", x.0);
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
            (_, Op::Select(c, a, b)) => format!("select({} != 0u, {}, {})", v(c), v(a), v(b)),
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
            (Mode::Doubled, Op::WaveSum(a)) => format!("WaveActiveSum((e.x ? {a}.x : 0u) + (e.y ? {a}.y : 0u)).xx", a = v(a)),
            (Mode::Doubled, Op::WaveMin(a)) => format!("WaveActiveMin(min(e.x ? {a}.x : 0xFFFFFFFFu, e.y ? {a}.y : 0xFFFFFFFFu)).xx", a = v(a)),
            (Mode::Doubled, Op::BallotLo(a)) => format!("WaveActiveBallot(e.x && {}.x != 0u).xx", v(a)),
            (Mode::Doubled, Op::BallotHi(a)) => format!("WaveActiveBallot(e.y && {}.y != 0u).xx", v(a)),
            (Mode::Doubled, Op::ReadLane(a, k)) => {
                let k = k as u32 & 63;
                if k < 32 { format!("WaveReadLaneAt({}.x, {k}u).xx", v(a)) } else { format!("WaveReadLaneAt({}.y, {}u).xx", v(a), k - 32) }
            }
            (Mode::Doubled, Op::ReadFirstLane(a)) => format!(
                "rfl({a})",
                a = v(a)
            ),
            (Mode::Doubled, Op::Swizzle(a, m)) => {
                let m = m as u32 & 63;
                let (flip, xm) = (m & 32 != 0, m & 31);
                let (from_lo, from_hi) = if flip { ("y", "x") } else { ("x", "y") };
                format!(
                    "uint2(WaveReadLaneAt({a}.{from_lo}, h ^ {xm}u), WaveReadLaneAt({a}.{from_hi}, h ^ {xm}u))",
                    a = v(a)
                )
            }
            (Mode::Doubled, op) => unreachable!("{op:?}"),
        };
        s.push_str(&format!("    {t} v{i} = {rhs};\n"));
    }
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
    if mode == Mode::Doubled && p.insts.iter().any(|o| matches!(o, Op::ReadFirstLane(_))) {
        // the first active console lane: lo half first, then hi (index is uniform)
        let helper = format!(
            "uint2 rfl(uint2 a) {{\n    uint h = WaveGetLaneIndex();\n    bool2 e = bool2((In.Load({lo}) >> h) & 1, (In.Load({hi}) >> h) & 1);\n    uint bl = WaveActiveBallot(e.x).x;\n    uint bh = WaveActiveBallot(e.y).x;\n    uint r = bl != 0u ? WaveReadLaneAt(a.x, firstbitlow(bl)) : WaveReadLaneAt(a.y, bh != 0u ? firstbitlow(bh) : 0u);\n    return r.xx;\n}}\n",
            lo = exec_offset(),
            hi = exec_offset() + 4
        );
        let at = s.find("[numthreads").unwrap();
        s.insert_str(at, &helper);
    }
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
}
