//! A synthetic recorded frame in the title's shape: ~4,500 draws and ~20,300 list calls
//! (4.5 calls a draw) spread over many lists, with the repeats a real title sends.
//!
//! The mix per draw (probabilities; average 4.5 calls):
//!
//! ```text
//! pipeline 0.35   table0 0.90   table1 0.50   cbv 0.90   constants 0.30
//! vertex 0.30     index 0.20    barrier 0.10  draw 1 (half indirect, as the title's ~4,500 indirect commands)
//! ```
//! A third of table and cbv sets repeat the value already bound, so elision has work to do.

use crate::{Cmd, CONSOLE_DESCRIPTOR};

pub struct Shape {
    pub lists: usize,
    pub draws: usize,
    pub pipelines: usize,
    pub descriptor_slots: u64,
    pub indirect_share: f64,
}

impl Default for Shape {
    fn default() -> Shape {
        Shape { lists: 48, draws: 4_500, pipelines: 1_433, descriptor_slots: 200_000, indirect_share: 0.5 }
    }
}

/// Console address layout the frame is written against.
pub struct Layout {
    pub descriptor_base: u64,
    pub memory_base: u64,
    pub memory_bytes: u64,
    pub barrier_targets: Vec<u64>,
    pub pipeline_handles: Vec<u64>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn p(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn layout(shape: &Shape) -> Layout {
    let mut rng = Rng(0x5EED);
    Layout {
        descriptor_base: 0x10_0000_0000,
        memory_base: 0x20_0000_0000,
        memory_bytes: 64 << 20,
        barrier_targets: (0..16).map(|i| 0x20_0000_0000 + (32 << 20) + i * 0x1_0000).collect(),
        pipeline_handles: (0..shape.pipelines).map(|_| rng.next() | 1).collect(),
    }
}

/// The frame: one `Vec<Cmd>` per list.
pub fn frame(shape: &Shape, lay: &Layout, seed: u64) -> Vec<Vec<Cmd>> {
    let mut rng = Rng(seed);
    let per = shape.draws.div_ceil(shape.lists);
    let mut lists = Vec::with_capacity(shape.lists);
    let mut left = shape.draws;
    for _ in 0..shape.lists {
        let n = per.min(left);
        left -= n;
        let mut l = Vec::with_capacity(n * 5);
        let (mut t0, mut t1, mut cb) = (0u64, 0u64, 0u64);
        for _ in 0..n {
            if rng.p(0.35) || l.is_empty() {
                let h = lay.pipeline_handles[rng.below(lay.pipeline_handles.len() as u64) as usize];
                l.push(Cmd::SetPipeline(h));
            }
            if rng.p(0.90) {
                if t0 == 0 || !rng.p(0.33) {
                    t0 = lay.descriptor_base + rng.below(shape.descriptor_slots - 16) * CONSOLE_DESCRIPTOR;
                }
                l.push(Cmd::SetTable(0, t0));
            }
            if rng.p(0.50) {
                if t1 == 0 || !rng.p(0.33) {
                    t1 = lay.descriptor_base + rng.below(shape.descriptor_slots - 16) * CONSOLE_DESCRIPTOR;
                }
                l.push(Cmd::SetTable(1, t1));
            }
            if rng.p(0.90) {
                if cb == 0 || !rng.p(0.33) {
                    cb = lay.memory_base + rng.below(16 << 20 >> 8) * 256;
                }
                l.push(Cmd::SetCbv(2, cb));
            }
            if rng.p(0.30) {
                let v = rng.next();
                l.push(Cmd::SetConstants([v as u32, (v >> 32) as u32, 0, 0]));
            }
            if rng.p(0.30) {
                l.push(Cmd::SetVertexBuffer(lay.memory_base + (16 << 20) + rng.below(4096) * 256, 4096, 16));
            }
            if rng.p(0.20) {
                l.push(Cmd::SetIndexBuffer(lay.memory_base + (24 << 20) + rng.below(4096) * 256, 2048));
            }
            if rng.p(0.10) {
                l.push(Cmd::Barrier(lay.barrier_targets[rng.below(lay.barrier_targets.len() as u64) as usize]));
            }
            if rng.p(shape.indirect_share) {
                l.push(Cmd::DrawIndirect(lay.memory_base + (40 << 20) + rng.below(1024) * 32, 1));
            } else {
                l.push(Cmd::Draw(3, 1, 0));
            }
        }
        lists.push(l);
    }
    lists
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_has_the_titles_shape() {
        let shape = Shape::default();
        let lay = layout(&shape);
        let f = frame(&shape, &lay, 1);
        let calls: usize = f.iter().map(|l| l.len()).sum();
        let draws: usize = f.iter().flatten().filter(|c| c.is_draw()).count();
        assert_eq!(draws, 4_500);
        assert!((19_000..22_000).contains(&calls), "{calls} calls");
    }
}
