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

/// An open `If` while generating: its condition, whether we are in its else-part, and the values
/// each part defined (visible only inside that part, merged by a `Phi` after `EndIf`).
struct Frame {
    cond: V,
    in_else: bool,
    then_vals: Vec<V>,
    else_vals: Vec<V>,
}

/// A random program of about `len` instructions after its inputs. With `wave_ops` false it uses
/// only lane-local arithmetic and quad swizzles, so it classifies as agnostic. With `branches`, it
/// nests structured ifs (depth ≤ 2) whose parts are merged by phis.
pub fn program_with(rng: &mut Rng, len: usize, wave_ops: bool, branches: bool) -> Program {
    let mut p = Program::default();
    let mut outer: Vec<V> = vec![p.push(Op::VInput(0)), p.push(Op::VInput(1)), p.push(Op::SInput(0))];
    if wave_ops {
        outer.push(p.push(Op::LaneId));
    }
    let mut frames: Vec<Frame> = Vec::new();
    // what an instruction may read: the outer values, plus each open frame's current part
    let visible = |outer: &Vec<V>, frames: &Vec<Frame>| {
        let mut v = outer.clone();
        for f in frames {
            v.extend(if f.in_else { &f.else_vals } else { &f.then_vals });
        }
        v
    };
    let mut steps = 0;
    while steps < len || !frames.is_empty() {
        steps += 1;
        let vis = visible(&outer, &frames);
        let pick = |rng: &mut Rng| vis[rng.below(vis.len() as u64) as usize];
        let closing = steps >= len;
        // control flow
        if branches && !closing && frames.len() < 2 && rng.below(8) == 0 {
            let c = pick(rng);
            p.push(Op::If(c));
            frames.push(Frame { cond: c, in_else: false, then_vals: vec![], else_vals: vec![] });
            continue;
        }
        if let Some(f) = frames.last_mut() {
            if !f.in_else && (closing || rng.below(6) == 0) {
                p.push(Op::Else);
                f.in_else = true;
                continue;
            }
            if f.in_else && (closing || rng.below(6) == 0) {
                p.push(Op::EndIf);
                let f = frames.pop().unwrap();
                let parent = visible(&outer, &frames);
                let from = |vals: &Vec<V>, rng: &mut Rng| {
                    if vals.is_empty() { parent[rng.below(parent.len() as u64) as usize] } else { vals[rng.below(vals.len() as u64) as usize] }
                };
                let a = from(&f.then_vals, rng);
                let b = from(&f.else_vals, rng);
                let phi = p.push(Op::Phi(f.cond, a, b));
                match frames.last_mut() {
                    Some(g) if g.in_else => g.else_vals.push(phi),
                    Some(g) => g.then_vals.push(phi),
                    None => outer.push(phi),
                }
                continue;
            }
        }
        if closing {
            continue;
        }
        let (a, b, c) = (pick(rng), pick(rng), pick(rng));
        let choice = if wave_ops { rng.below(16) } else { [0, 1, 2, 3, 4, 5, 6, 7, 15, 14][rng.below(10) as usize] };
        let op = match choice {
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
        let v = p.push(op);
        match frames.last_mut() {
            Some(f) if f.in_else => f.else_vals.push(v),
            Some(f) => f.then_vals.push(v),
            None => outer.push(v),
        }
    }
    let n = outer.len();
    p.outputs = outer[n.saturating_sub(4)..].to_vec();
    p
}

/// A straight-line random program (no branches).
pub fn program(rng: &mut Rng, len: usize, wave_ops: bool) -> Program {
    program_with(rng, len, wave_ops, false)
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
