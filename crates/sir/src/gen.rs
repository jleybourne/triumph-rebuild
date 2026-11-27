//! Random programs, inputs and EXEC masks, for the interpreter tests and the GPU harness.

use crate::{Op, Program, V};

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A random program of `len` instructions after its inputs. With `wave_ops` false it uses only
/// lane-local arithmetic and quad swizzles, so it classifies as agnostic.
pub fn program(rng: &mut Rng, len: usize, wave_ops: bool) -> Program {
    let mut p = Program::default();
    p.push(Op::VInput(0));
    p.push(Op::VInput(1));
    p.push(Op::SInput(0));
    if wave_ops {
        p.push(Op::LaneId);
    }
    for _ in 0..len {
        let n = p.insts.len() as u64;
        let a = V(rng.below(n) as u32);
        let b = V(rng.below(n) as u32);
        let c = V(rng.below(n) as u32);
        let pick = if wave_ops { rng.below(16) } else { [0, 1, 2, 3, 4, 5, 6, 7, 15, 14][rng.below(10) as usize] };
        let op = match pick {
            0 => Op::Const(rng.next() as u32 & 0xFF),
            1 => Op::Add(a, b),
            2 => Op::Mul(a, b),
            3 => Op::And(a, b),
            4 => Op::Xor(a, b),
            5 => Op::Shr(a, b),
            6 => Op::Lt(a, b),
            7 => Op::Select(c, a, b),
            8 => Op::WaveSum(a),
            9 => Op::WaveMin(a),
            10 => Op::BallotLo(a),
            11 => Op::BallotHi(a),
            12 => Op::ReadLane(a, rng.below(64) as u8),
            13 => Op::ReadFirstLane(a),
            14 => Op::Swizzle(a, if wave_ops { rng.below(64) } else { rng.below(4) } as u8),
            _ => Op::Or(a, b),
        };
        p.push(op);
    }
    let n = p.insts.len() as u32;
    p.outputs = (n.saturating_sub(4)..n).map(V).collect();
    p
}

pub fn exec(rng: &mut Rng) -> u64 {
    match rng.below(5) {
        0 => u64::MAX,
        1 => rng.next() & 0xFFFF_FFFF,
        2 => rng.next() & 0xFFFF_FFFF_0000_0000,
        3 => 1 << rng.below(64),
        _ => rng.next(),
    }
    .max(1)
}

pub fn inputs(rng: &mut Rng) -> ([u32; 1], [[u32; 64]; 2]) {
    let sin = [rng.next() as u32];
    let mut vin = [[0u32; 64]; 2];
    for w in vin.iter_mut() {
        for l in w.iter_mut() {
            *l = rng.next() as u32 & if rng.below(3) == 0 { 0xF } else { u32::MAX };
        }
    }
    (sin, vin)
}
