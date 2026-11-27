//! Does a program observe the wave size? If not, it runs as plain wave32 with zero overhead.

use crate::{Op, Program};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// nothing wider than a quad crosses lanes, and the lane id is never seen beyond its quad bits
    Agnostic,
    /// the program sees all 64 lanes: lane doubling (compute) or two host waves (graphics)
    WaveWide,
}

/// Why a program is wave-wide: the first instruction that makes it so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reason {
    pub inst: u32,
    pub op: Op,
}

pub fn classify(p: &Program) -> (Class, Option<Reason>) {
    let uniform = p.uniformity();
    // lane ids used only through `& 3` (the lane in its quad) are wave-size agnostic
    let quad_only_lane = |id: u32| {
        p.insts.iter().all(|op| {
            !Program::operands(op).contains(&crate::V(id))
                || matches!(*op, Op::And(a, b) if (a.0 == id && is_const(p, b, 3)) || (b.0 == id && is_const(p, a, 3)))
        }) && !p.outputs.contains(&crate::V(id))
    };
    for (i, op) in p.insts.iter().enumerate() {
        let wide = match *op {
            Op::WaveSum(_) | Op::WaveMin(_) | Op::BallotLo(_) | Op::BallotHi(_) => true,
            Op::ReadLane(_, _) => true,
            // the first active lane of a 32-lane half is not the first of 64, unless all agree
            Op::ReadFirstLane(a) => !uniform[a.0 as usize],
            Op::Swizzle(a, m) => m >= 4 && !uniform[a.0 as usize],
            Op::LaneId => !quad_only_lane(i as u32),
            _ => false,
        };
        if wide {
            return (Class::WaveWide, Some(Reason { inst: i as u32, op: *op }));
        }
    }
    (Class::Agnostic, None)
}

fn is_const(p: &Program, v: crate::V, k: u32) -> bool {
    matches!(p.insts[v.0 as usize], Op::Const(c) if c == k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Program;

    #[test]
    fn plain_arithmetic_and_quad_ops_are_agnostic() {
        let mut p = Program::default();
        let a = p.push(Op::VInput(0));
        let s = p.push(Op::SInput(0));
        let m = p.push(Op::Mul(a, s));
        let q = p.push(Op::Swizzle(m, 1));
        let id = p.push(Op::LaneId);
        let three = p.push(Op::Const(3));
        let quad = p.push(Op::And(id, three));
        let o = p.push(Op::Add(q, quad));
        p.outputs.push(o);
        assert_eq!(classify(&p).0, Class::Agnostic);
    }

    #[test]
    fn reductions_ballots_and_wide_swizzles_are_wave_wide() {
        for op in [Op::WaveSum(crate::V(0)), Op::BallotHi(crate::V(0)), Op::ReadLane(crate::V(0), 40), Op::Swizzle(crate::V(0), 32)] {
            let mut p = Program::default();
            p.push(Op::VInput(0));
            let r = p.push(op);
            p.outputs.push(r);
            let (c, why) = classify(&p);
            assert_eq!(c, Class::WaveWide, "{op:?}");
            assert_eq!(why.unwrap().inst, 1);
        }
    }

    #[test]
    fn uniform_operands_do_not_make_a_program_wave_wide() {
        let mut p = Program::default();
        let s = p.push(Op::SInput(0));
        let f = p.push(Op::ReadFirstLane(s));
        let w = p.push(Op::Swizzle(s, 32));
        let o = p.push(Op::Add(f, w));
        p.outputs.push(o);
        assert_eq!(classify(&p).0, Class::Agnostic);
    }

    #[test]
    fn a_lane_id_seen_whole_is_wave_wide() {
        let mut p = Program::default();
        let id = p.push(Op::LaneId);
        p.outputs.push(id);
        assert_eq!(classify(&p).0, Class::WaveWide);
    }
}
