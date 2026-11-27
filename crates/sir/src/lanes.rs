//! The lane-doubling model, checked against the console.
//!
//! [`console`] is the reference: one 64-lane wave, the console's meaning of every operation.
//! [`doubled`] is what the emitted code does on a 32-lane host wave. Host lane `h` holds console
//! lanes `h` (lo) and `h + 32` (hi), and EXEC is a pair of booleans carried as data. It may use only
//! the operations a real host wave has: reductions and ballots over 32 lanes, reading another host
//! lane, and finding the first active one. If the two agree bit for bit, lane doubling is exact.
//!
//! Semantics shared by both: every ALU value exists in every lane; EXEC decides which lanes take
//! part in wave-wide operations, and which lanes' outputs count.

use crate::{Op, Program};

pub type Wave64 = [u32; 64];

/// Run on the reference 64-lane console wave. Returns each output's 64 lanes.
pub fn console(p: &Program, exec: u64, sin: &[u32], vin: &[Wave64]) -> Vec<Wave64> {
    let mut vals: Vec<Wave64> = Vec::with_capacity(p.insts.len());
    let mut exec = exec;
    let mut stack: Vec<(u64, u64)> = Vec::new(); // (exec at the If, lanes whose condition held)
    for op in &p.insts {
        let active = |l: usize| exec >> l & 1 == 1;
        let g = |v: crate::V| vals[v.0 as usize];
        let mut r = [0u32; 64];
        match *op {
            Op::Const(c) => r = [c; 64],
            Op::SInput(n) => r = [sin[n as usize]; 64],
            Op::VInput(n) => r = vin[n as usize],
            Op::LaneId => (0..64).for_each(|l| r[l] = l as u32),
            Op::Add(a, b) => (0..64).for_each(|l| r[l] = g(a)[l].wrapping_add(g(b)[l])),
            Op::Mul(a, b) => (0..64).for_each(|l| r[l] = g(a)[l].wrapping_mul(g(b)[l])),
            Op::And(a, b) => (0..64).for_each(|l| r[l] = g(a)[l] & g(b)[l]),
            Op::Or(a, b) => (0..64).for_each(|l| r[l] = g(a)[l] | g(b)[l]),
            Op::Xor(a, b) => (0..64).for_each(|l| r[l] = g(a)[l] ^ g(b)[l]),
            Op::Shr(a, b) => (0..64).for_each(|l| r[l] = g(a)[l] >> (g(b)[l] & 31)),
            Op::Lt(a, b) => (0..64).for_each(|l| r[l] = (g(a)[l] < g(b)[l]) as u32),
            Op::Select(c, a, b) => (0..64).for_each(|l| r[l] = if g(c)[l] != 0 { g(a)[l] } else { g(b)[l] }),
            Op::WaveSum(a) => r = [(0..64).filter(|&l| active(l)).fold(0u32, |s, l| s.wrapping_add(g(a)[l])); 64],
            Op::WaveMin(a) => r = [(0..64).filter(|&l| active(l)).map(|l| g(a)[l]).min().unwrap_or(u32::MAX); 64],
            Op::BallotLo(a) => r = [(0..32).filter(|&l| active(l) && g(a)[l] != 0).fold(0, |m, l| m | 1 << l); 64],
            Op::BallotHi(a) => r = [(32..64).filter(|&l| active(l) && g(a)[l] != 0).fold(0, |m, l| m | 1 << (l - 32)); 64],
            Op::ReadLane(a, k) => r = [g(a)[k as usize & 63]; 64],
            Op::ReadFirstLane(a) => r = [g(a)[(0..64).find(|&l| active(l)).unwrap_or(0)]; 64],
            Op::Swizzle(a, m) => (0..64).for_each(|l| r[l] = g(a)[l ^ (m as usize & 63)]),
            Op::Phi(c, a, b) => (0..64).for_each(|l| r[l] = if g(c)[l] != 0 { g(a)[l] } else { g(b)[l] }),
            Op::If(c) => {
                let held = (0..64).filter(|&l| g(c)[l] != 0).fold(0u64, |m, l| m | 1 << l);
                stack.push((exec, held));
                exec &= held;
            }
            Op::Else => {
                let (outer, held) = *stack.last().expect("Else without If");
                exec = outer & !held;
            }
            Op::EndIf => exec = stack.pop().expect("EndIf without If").0,
        }
        vals.push(r);
    }
    p.outputs.iter().map(|v| vals[v.0 as usize]).collect()
}

/// A value in the doubled model: per host lane, the lo and hi console lanes.
#[derive(Clone, Copy)]
struct Pair {
    lo: [u32; 32],
    hi: [u32; 32],
}

// --- the only cross-lane operations a 32-lane host wave offers ----------------------------------

/// WaveActiveSum over host lanes (all host lanes run; console EXEC is data)
fn host_sum(x: &[u32; 32]) -> u32 {
    x.iter().fold(0u32, |s, v| s.wrapping_add(*v))
}
fn host_min(x: &[u32; 32]) -> u32 {
    *x.iter().min().unwrap()
}
/// WaveActiveBallot
fn host_ballot(b: &[bool; 32]) -> u32 {
    (0..32).filter(|&h| b[h]).fold(0, |m, h| m | 1 << h)
}
/// WaveReadLaneAt(x, lane)
fn host_read(x: &[u32; 32], lane: usize) -> u32 {
    x[lane & 31]
}

/// Run the same program as the emitted code would, on one 32-lane host wave.
pub fn doubled(p: &Program, exec: u64, sin: &[u32], vin: &[Wave64]) -> Vec<Wave64> {
    let mut e_lo = [false; 32];
    let mut e_hi = [false; 32];
    for h in 0..32 {
        e_lo[h] = exec >> h & 1 == 1;
        e_hi[h] = exec >> (h + 32) & 1 == 1;
    }
    let splat = |c: u32| Pair { lo: [c; 32], hi: [c; 32] };
    let mut vals: Vec<Pair> = Vec::with_capacity(p.insts.len());
    // EXEC as data: a stack of (exec at the If, where the condition held), per half
    let mut stack: Vec<([bool; 32], [bool; 32], [bool; 32], [bool; 32])> = Vec::new();
    for op in &p.insts {
        let g = |v: crate::V| vals[v.0 as usize];
        let lane = |f: &dyn Fn(usize, bool) -> u32| {
            let mut r = splat(0);
            for h in 0..32 {
                r.lo[h] = f(h, false);
                r.hi[h] = f(h, true);
            }
            r
        };
        let pick = |x: &Pair, h: usize, hi: bool| if hi { x.hi[h] } else { x.lo[h] };
        let r = match *op {
            Op::Const(c) => splat(c),
            Op::SInput(n) => splat(sin[n as usize]),
            Op::VInput(n) => {
                let w = vin[n as usize];
                lane(&|h, hi| w[h + 32 * hi as usize])
            }
            Op::LaneId => lane(&|h, hi| (h + 32 * hi as usize) as u32),
            Op::Add(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| pick(&a, h, hi).wrapping_add(pick(&b, h, hi))) }
            Op::Mul(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| pick(&a, h, hi).wrapping_mul(pick(&b, h, hi))) }
            Op::And(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| pick(&a, h, hi) & pick(&b, h, hi)) }
            Op::Or(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| pick(&a, h, hi) | pick(&b, h, hi)) }
            Op::Xor(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| pick(&a, h, hi) ^ pick(&b, h, hi)) }
            Op::Shr(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| pick(&a, h, hi) >> (pick(&b, h, hi) & 31)) }
            Op::Lt(a, b) => { let (a, b) = (g(a), g(b)); lane(&|h, hi| (pick(&a, h, hi) < pick(&b, h, hi)) as u32) }
            Op::Select(c, a, b) => {
                let (c, a, b) = (g(c), g(a), g(b));
                lane(&|h, hi| if pick(&c, h, hi) != 0 { pick(&a, h, hi) } else { pick(&b, h, hi) })
            }
            // reduction: each host lane combines its two console lanes, inactive ones as the identity
            Op::WaveSum(a) => {
                let a = g(a);
                let mut x = [0u32; 32];
                for h in 0..32 {
                    x[h] = (if e_lo[h] { a.lo[h] } else { 0 }).wrapping_add(if e_hi[h] { a.hi[h] } else { 0 });
                }
                splat(host_sum(&x))
            }
            Op::WaveMin(a) => {
                let a = g(a);
                let mut x = [0u32; 32];
                for h in 0..32 {
                    x[h] = (if e_lo[h] { a.lo[h] } else { u32::MAX }).min(if e_hi[h] { a.hi[h] } else { u32::MAX });
                }
                splat(host_min(&x))
            }
            Op::BallotLo(a) => {
                let a = g(a);
                let mut b = [false; 32];
                for h in 0..32 {
                    b[h] = e_lo[h] && a.lo[h] != 0;
                }
                splat(host_ballot(&b))
            }
            Op::BallotHi(a) => {
                let a = g(a);
                let mut b = [false; 32];
                for h in 0..32 {
                    b[h] = e_hi[h] && a.hi[h] != 0;
                }
                splat(host_ballot(&b))
            }
            Op::ReadLane(a, k) => {
                let a = g(a);
                let k = k as usize & 63;
                splat(if k < 32 { host_read(&a.lo, k) } else { host_read(&a.hi, k - 32) })
            }
            Op::ReadFirstLane(a) => {
                let a = g(a);
                let lo = host_ballot(&e_lo);
                let v = if lo != 0 {
                    host_read(&a.lo, lo.trailing_zeros() as usize)
                } else {
                    let hi = host_ballot(&e_hi);
                    // no lane active at all: lane 0 by convention (hardware never runs it)
                    if hi != 0 { host_read(&a.hi, hi.trailing_zeros() as usize) } else { host_read(&a.lo, 0) }
                };
                splat(v)
            }
            Op::Phi(c, a, b) => {
                let (c, a, b) = (g(c), g(a), g(b));
                lane(&|h, hi| if pick(&c, h, hi) != 0 { pick(&a, h, hi) } else { pick(&b, h, hi) })
            }
            Op::If(c) => {
                let c = g(c);
                let (mut held_lo, mut held_hi) = ([false; 32], [false; 32]);
                for h in 0..32 {
                    held_lo[h] = c.lo[h] != 0;
                    held_hi[h] = c.hi[h] != 0;
                }
                stack.push((e_lo, e_hi, held_lo, held_hi));
                for h in 0..32 {
                    e_lo[h] &= held_lo[h];
                    e_hi[h] &= held_hi[h];
                }
                splat(0)
            }
            Op::Else => {
                let (olo, ohi, hlo, hhi) = *stack.last().expect("Else without If");
                for h in 0..32 {
                    e_lo[h] = olo[h] && !hlo[h];
                    e_hi[h] = ohi[h] && !hhi[h];
                }
                splat(0)
            }
            Op::EndIf => {
                let (olo, ohi, _, _) = stack.pop().expect("EndIf without If");
                e_lo = olo;
                e_hi = ohi;
                splat(0)
            }
            // console lane L reads lane L ^ m: bit 5 of m swaps halves (inside the host lane),
            // bits 0-4 choose another host lane (a host lane read)
            Op::Swizzle(a, m) => {
                let a = g(a);
                let m = m as usize & 63;
                let (flip, xm) = (m & 32 != 0, m & 31);
                lane(&|h, hi| {
                    let src_hi = hi ^ flip;
                    host_read(if src_hi { &a.hi } else { &a.lo }, h ^ xm)
                })
            }
        };
        vals.push(r);
    }
    p.outputs
        .iter()
        .map(|v| {
            let x = vals[v.0 as usize];
            let mut w = [0u32; 64];
            for h in 0..32 {
                w[h] = x.lo[h];
                w[h + 32] = x.hi[h];
            }
            w
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{self, Rng};

    #[test]
    fn doubling_matches_the_console_bit_for_bit_on_100k_random_programs() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..100_000 {
            let p = gen::program(&mut rng, 12, true);
            let exec = gen::exec(&mut rng);
            let (sin, vin) = gen::inputs(&mut rng);
            let want = console(&p, exec, &sin, &vin);
            let got = doubled(&p, exec, &sin, &vin);
            for (k, (w, g)) in want.iter().zip(&got).enumerate() {
                for l in 0..64 {
                    assert_eq!(w[l], g[l], "case {case}, output {k}, lane {l}, exec {exec:#x}\n{:?}", p.insts);
                }
            }
        }
    }

    #[test]
    fn doubling_matches_the_console_with_nested_branches_on_100k_programs() {
        let mut rng = Rng(0x0BAD_5EED_1234_5678);
        let mut branched = 0;
        for case in 0..100_000 {
            let p = gen::program_with(&mut rng, 16, true, true);
            branched += p.insts.iter().any(|o| matches!(o, Op::If(_))) as usize;
            let exec = gen::exec(&mut rng);
            let (sin, vin) = gen::inputs(&mut rng);
            let want = console(&p, exec, &sin, &vin);
            let got = doubled(&p, exec, &sin, &vin);
            assert_eq!(want, got, "case {case}, exec {exec:#x}
{:?}", p.insts);
        }
        assert!(branched > 50_000, "only {branched} programs had branches");
    }

    /// A wave op inside a divergent branch sees only that branch's lanes.
    #[test]
    fn a_reduction_inside_a_branch_sees_only_the_branch() {
        let mut p = Program::default();
        let x = p.push(Op::VInput(0));
        let id = p.push(Op::LaneId);
        let k = p.push(Op::Const(40));
        let c = p.push(Op::Lt(id, k)); // lanes 0..40 take the then-part
        p.push(Op::If(c));
        let s_then = p.push(Op::WaveSum(x));
        p.push(Op::Else);
        let s_else = p.push(Op::WaveSum(x));
        p.push(Op::EndIf);
        let phi = p.push(Op::Phi(c, s_then, s_else));
        p.outputs = vec![phi];
        let vin = [[1u32; 64]];
        let out = doubled(&p, u64::MAX, &[], &vin);
        assert_eq!(out[0][0], 40);
        assert_eq!(out[0][63], 24);
        assert_eq!(out, console(&p, u64::MAX, &[], &vin));
    }

    /// The fork's refusal: a wave64 reduction where not every lane runs, across both halves.
    #[test]
    fn a_reduction_where_not_every_lane_runs_is_exact() {
        let mut p = Program::default();
        let x = p.push(Op::VInput(0));
        let s = p.push(Op::WaveSum(x));
        let m = p.push(Op::WaveMin(x));
        p.outputs = vec![s, m];
        let mut vin = [[0u32; 64]; 1];
        for l in 0..64 {
            vin[0][l] = 1000 + l as u32;
        }
        // lanes 3, 40 and 63 run: two of them in the hi half
        let exec = (1 << 3) | (1 << 40) | (1u64 << 63);
        let out = doubled(&p, exec, &[], &vin);
        assert_eq!(out[0][0], 1003 + 1040 + 1063);
        assert_eq!(out[1][0], 1003);
        assert_eq!(out, console(&p, exec, &[], &vin));
    }
}
