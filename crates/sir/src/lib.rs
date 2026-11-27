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
    /// structured control flow: lanes where the value is non-zero run the then-part. The
    /// instructions up to the matching `Else` / `EndIf` see EXEC narrowed to them.
    If(V),
    /// the else-part: lanes active at the `If` where the condition was zero
    Else,
    EndIf,
    /// after an `EndIf`: per lane, cond != 0 ? then-value : else-value (the SSA merge)
    Phi(V, V, V),
    /// the u32 at console address (hi << 32 | lo); `hi` is the 4 GiB window, `lo` the offset in it.
    /// An unmapped page reads zero (tiled resources, Build 1).
    Load(V, V),
    /// store a value at (lo, hi) in active lanes only; a store into an unmapped page is dropped
    Store(V, V, V),
    /// two IEEE half-precision lanes packed in a u32 (lo bits 0-15, hi 16-31): add, rounded to nearest even
    PkAddF16(V, V),
    /// packed half-precision multiply
    PkMulF16(V, V),
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
            Const(_) | SInput(_) | VInput(_) | LaneId | Else | EndIf => vec![],
            If(c) => vec![c],
            Add(a, b) | Mul(a, b) | And(a, b) | Or(a, b) | Xor(a, b) | Shr(a, b) | Lt(a, b) | Load(a, b) | PkAddF16(a, b) | PkMulF16(a, b) => vec![a, b],
            Store(a, b, c) => vec![a, b, c],
            Select(c, a, b) | Phi(c, a, b) => vec![c, a, b],
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
                Const(_) | SInput(_) | If(_) | Else | EndIf => true,
                VInput(_) | LaneId => false,
                WaveSum(_) | WaveMin(_) | BallotLo(_) | BallotHi(_) | ReadLane(..) | ReadFirstLane(_) => true,
                Swizzle(a, _) => uniform[a.0 as usize],
                _ => Self::operands(op).iter().all(|v| uniform[v.0 as usize]),
            };
            uniform.push(u);
        }
        uniform
    }

    /// Each `If`'s matching `Else` (if any) and `EndIf`, by instruction index.
    pub fn regions(&self) -> Vec<Region> {
        let mut open: Vec<Region> = Vec::new();
        let mut out = Vec::new();
        for (i, op) in self.insts.iter().enumerate() {
            match op {
                Op::If(c) => open.push(Region { at: i as u32, cond: *c, else_at: None, end: 0 }),
                Op::Else => open.last_mut().expect("Else without If").else_at = Some(i as u32),
                Op::EndIf => {
                    let mut r = open.pop().expect("EndIf without If");
                    r.end = i as u32;
                    out.push(r);
                }
                _ => {}
            }
        }
        assert!(open.is_empty(), "If without EndIf");
        out.sort_by_key(|r| r.at);
        out
    }
}

/// One structured if: `If` at `at`, optional `Else`, `EndIf` at `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub at: u32,
    pub cond: V,
    pub else_at: Option<u32>,
    pub end: u32,
}

impl Op {
    /// Reads other lanes (so it must not run under a host branch that leaves lanes inactive).
    pub fn crosses_lanes(&self) -> bool {
        matches!(self, Op::WaveSum(_) | Op::WaveMin(_) | Op::BallotLo(_) | Op::BallotHi(_) | Op::ReadLane(..) | Op::ReadFirstLane(_) | Op::Swizzle(..))
    }
}
