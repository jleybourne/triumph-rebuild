//! The shader core's IR (plan decision 4, `docs/design/shader-ir.md`).
//!
//! A program is a list of SSA instructions. Every value is either **uniform** (one per console wave:
//! SGPRs, constants) or **varying** (one per lane: VGPRs, lane ids). Wave-wide operations have the
//! console's 64-lane meaning. Two things are decided here, before any code is emitted:
//!
//! - [`classify`]: does the program observe the wave size at all? Most do not, and run as plain
//!   wave32 at no cost.
//! - [`lanes`]: for those that do, the lane-doubling model (one host lane carries console lanes `i`
//!   and `i + 32`, with EXEC carried as data). It is checked bit for bit against a reference wave64
//!   interpreter.

pub mod classify;
pub mod gen;
pub mod hlsl;
pub mod lanes;

/// An SSA value: the index of the instruction that defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct V(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// a constant (uniform)
    Const(u32),
    /// uniform input register n (an SGPR at entry)
    SInput(u8),
    /// varying input register n (a VGPR at entry)
    VInput(u8),
    /// this lane's index in the console wave, 0..64 (varying)
    LaneId,
    Add(V, V),
    Mul(V, V),
    And(V, V),
    Or(V, V),
    Xor(V, V),
    Shr(V, V),
    /// unsigned a < b, as 0 or 1
    Lt(V, V),
    /// c != 0 ? a : b
    Select(V, V, V),
    /// sum over active lanes (wave-wide)
    WaveSum(V),
    /// min over active lanes (wave-wide); identity u32::MAX
    WaveMin(V),
    /// low 32 bits of the ballot of (v != 0) over active lanes (wave-wide)
    BallotLo(V),
    /// high 32 bits of the ballot (wave-wide)
    BallotHi(V),
    /// the value in a fixed lane 0..64 (wave-wide unless the lane is in this quad)
    ReadLane(V, u8),
    /// the value in the first active lane (wave-wide unless the operand is uniform)
    ReadFirstLane(V),
    /// the value in lane (id ^ mask): quad-local when mask < 4, wave-wide otherwise
    Swizzle(V, u8),
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub insts: Vec<Op>,
    /// values written to the output, one per lane
    pub outputs: Vec<V>,
}

impl Program {
    pub fn push(&mut self, op: Op) -> V {
        self.insts.push(op);
        V(self.insts.len() as u32 - 1)
    }

    pub fn operands(op: &Op) -> Vec<V> {
        use Op::*;
        match *op {
            Const(_) | SInput(_) | VInput(_) | LaneId => vec![],
            Add(a, b) | Mul(a, b) | And(a, b) | Or(a, b) | Xor(a, b) | Shr(a, b) | Lt(a, b) => vec![a, b],
            Select(c, a, b) => vec![c, a, b],
            WaveSum(a) | WaveMin(a) | BallotLo(a) | BallotHi(a) | ReadLane(a, _) | ReadFirstLane(a) | Swizzle(a, _) => vec![a],
        }
    }

    /// Uniformity of every value. Wave-wide reductions and fixed-lane reads produce uniform values;
    /// lane ids, varying inputs and anything computed from them are varying.
    pub fn uniformity(&self) -> Vec<bool> {
        use Op::*;
        let mut uniform = Vec::with_capacity(self.insts.len());
        for op in &self.insts {
            let u = match *op {
                Const(_) | SInput(_) => true,
                VInput(_) | LaneId => false,
                WaveSum(_) | WaveMin(_) | BallotLo(_) | BallotHi(_) | ReadLane(..) | ReadFirstLane(_) => true,
                Swizzle(a, _) => uniform[a.0 as usize],
                _ => Self::operands(op).iter().all(|v| uniform[v.0 as usize]),
            };
            uniform.push(u);
        }
        uniform
    }
}
