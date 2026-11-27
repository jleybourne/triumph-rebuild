//! v1 (control flow): the structurizer against the graph it structures, and hand-built programs with
//! branches, loops and divergence, compiled by DXC.

use super::*;
use crate::lift::cfg::{self, Block, Cond, Kind, Term, H};

// ---------------------------------------------------------------------------------------------------------
// The structurizer, checked by execution: a random graph is run with a fixed sequence of branch decisions,
// and so are the HLSL-level statements it becomes (break, continue, `sir_to` jumps and dispatch). Both
// must visit the same blocks in the same order.

/// A deterministic decision for the k-th time block `b` ends in a branch.
fn decide(seed: u64, b: usize, k: usize) -> bool {
    let mut x = seed ^ (b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (k as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    x & 1 == 1
}

const CAP: usize = 400;

fn run_graph(blocks: &[Block], seed: u64) -> Vec<usize> {
    let (mut trace, mut visits, mut b) = (Vec::new(), vec![0usize; blocks.len()], 0usize);
    while trace.len() < CAP {
        trace.push(b);
        visits[b] += 1;
        b = match blocks[b].term {
            Term::Exit => break,
            Term::Goto(t) => t,
            Term::Branch { taken, fall, .. } => {
                if decide(seed, b, visits[b] - 1) {
                    taken
                } else {
                    fall
                }
            }
        };
    }
    trace
}

#[derive(PartialEq)]
enum Sig {
    Normal,
    Break,
    Continue,
}

struct Machine {
    seed: u64,
    trace: Vec<usize>,
    visits: Vec<usize>,
    to: usize,
}

impl Machine {
    /// `Err` when the trace is full.
    fn exec(&mut self, hs: &[H]) -> Result<Sig, ()> {
        for h in hs {
            match h {
                H::Code(b) => {
                    if self.trace.len() >= CAP {
                        return Err(());
                    }
                    self.trace.push(*b);
                    self.visits[*b] += 1;
                }
                H::If(b, t, e) => {
                    let s = if decide(self.seed, *b, self.visits[*b] - 1) { self.exec(t)? } else { self.exec(e)? };
                    if s != Sig::Normal {
                        return Ok(s);
                    }
                }
                H::Construct { looping, body, .. } => loop {
                    match self.exec(body)? {
                        Sig::Continue if *looping => continue,
                        _ => break,
                    }
                },
                H::Break => return Ok(Sig::Break),
                H::Continue => return Ok(Sig::Continue),
                H::Jump(l) => {
                    self.to = l + 1;
                    return Ok(Sig::Break);
                }
                H::Dispatch { here, further } => {
                    if self.to != 0 {
                        if let Some((p, k)) = here {
                            if self.to == p + 1 {
                                self.to = 0;
                                return Ok(if *k == Kind::Exit { Sig::Break } else { Sig::Continue });
                            }
                        }
                        assert!(*further, "a jump with nowhere to go");
                        return Ok(Sig::Break);
                    }
                }
            }
        }
        Ok(Sig::Normal)
    }
}

/// A small xorshift generator for the random graphs.
struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[test]
fn structured_control_flow_visits_the_blocks_the_graph_does() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let (mut graphs, mut loops, mut jumps, mut irreducible) = (0, 0, 0, 0);
    for _ in 0..20_000 {
        let n = 2 + rng.next(11);
        let blocks: Vec<Block> = (0..n)
            .map(|b| {
                let term = match rng.next(10) {
                    0 if b > 0 => Term::Exit,
                    1..=3 => Term::Goto(rng.next(n)),
                    _ => {
                        let (taken, fall) = (rng.next(n), rng.next(n));
                        if taken == fall { Term::Goto(fall) } else { Term::Branch { cond: Cond::Scc1, taken, fall } }
                    }
                };
                // the last block always may end the program
                let term = if b == n - 1 && rng.next(2) == 0 { Term::Exit } else { term };
                Block { first: 0, body_end: 0, term }
            })
            .collect();
        let g = match cfg::analyse(&blocks) {
            Ok(g) => g,
            Err(e) => {
                assert_eq!(e, "irreducible control flow");
                irreducible += 1;
                continue;
            }
        };
        let s = cfg::structure(&blocks, &g).expect("a reducible graph structures");
        let (h, j) = cfg::lower(&s).expect("lowers");
        graphs += 1;
        loops += usize::from(g.header.iter().any(|x| *x));
        jumps += usize::from(j);
        for seed in 0..4u64 {
            let want = run_graph(&blocks, seed);
            let mut m = Machine { seed, trace: Vec::new(), visits: vec![0; n], to: 0 };
            let _ = m.exec(&h);
            assert_eq!(m.trace, want, "graph {blocks:?}\nstatements {h:?}");
        }
    }
    eprintln!("{graphs} reducible graphs ({loops} with loops, {jumps} needing sir_to), {irreducible} irreducible refused");
    assert!(graphs > 5_000 && loops > 1_000 && jumps > 10 && irreducible > 100);
}

// ---------------------------------------------------------------------------------------------------------
// Hand-built programs.

fn sopp(n: &str, simm16: i16) -> u32 {
    0x17F << 23 | op(Encoding::Sopp, n) << 16 | u32::from(simm16 as u16)
}

/// Instruction words, labels and branches to labels, assembled with branch offsets resolved.
enum A {
    W(Vec<u32>),
    L(u32),
    Br(&'static str, u32),
}

fn assemble(items: &[A]) -> Vec<Instruction> {
    let mut at = BTreeMap::new();
    let mut pos = 0usize;
    for i in items {
        match i {
            A::W(w) => pos += w.len(),
            A::L(l) => {
                at.insert(*l, pos);
            }
            A::Br(..) => pos += 1,
        }
    }
    let mut words = Vec::new();
    for i in items {
        match i {
            A::W(w) => words.extend(w),
            A::L(_) => {}
            A::Br(n, l) => {
                let next = words.len() + 1;
                words.push(sopp(n, (at[l] as i64 - next as i64) as i16));
            }
        }
    }
    words.extend(endpgm());
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    gfx10::decode(&bytes).expect("decodes")
}

const EXEC: u32 = 126;
const VCC: u32 = 106;

/// A divergent if/else as the console compiler writes it: save EXEC, narrow, skip a side no lane runs,
/// swap to the other side, restore.
fn if_else(wave64: bool) -> Vec<Instruction> {
    let w = |n: &str| if wave64 { n.replace("_bXX", "_b64") } else { n.replace("_bXX", "_b32") };
    assemble(&[
        A::W(vopc("v_cmp_gt_f32", V + 0, 1)),
        A::W(sop1(&w("s_and_saveexec_bXX"), 0, VCC)),
        A::W(sop2(&w("s_xor_bXX"), 0, EXEC, 0)),
        A::Br("s_cbranch_execz", 1),
        A::W(vop1("v_mov_b32", 2, 242)),
        A::L(1),
        A::W(sop1(&w("s_or_saveexec_bXX"), 0, 0)),
        A::W(sop2(&w("s_xor_bXX"), EXEC, EXEC, 0)),
        A::Br("s_cbranch_execz", 2),
        A::W(vop1("v_mov_b32", 2, 244)),
        A::L(2),
        A::W(sop2(&w("s_or_bXX"), EXEC, EXEC, 0)),
        A::W(vop2("v_add_f32", 3, V + 2, 2)),
    ])
}

#[test]
fn a_divergent_if_else_is_predicated_and_skips_sides_no_lane_runs() {
    for wave64 in [true, false] {
        let l = lift_program(&if_else(wave64), Stage::None, None, wave64);
        assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
        let b = &l.body;
        // both sides are host branches on a wave-uniform test, with the lanes predicated inside
        assert_eq!(b.matches("if (WaveActiveAnyTrue(ex)) {").count(), 2, "{b}");
        assert!(b.contains("if (ex) v2 = asuint(1.0f);") && b.contains("if (ex) v2 = asuint(2.0f);"), "{b}");
        assert!(b.contains("ex = sir_s && sir_o;") && b.contains("ex = sir_s || sir_o;"), "{b}");
        assert!(!b.contains("for (;;)") && !b.contains("sir_to"), "{b}");
        if let Some(dxc) = dxc() {
            compile_cs(&dxc, &l, &format!("cf_if_else_{wave64}"));
        }
    }
}

#[test]
fn a_uniform_counted_loop_is_a_host_loop() {
    let p = assemble(&[
        A::W(sop1("s_mov_b32", 2, 128)),
        A::L(1),
        A::W(vop2("v_add_f32", 1, V + 1, 0)),
        A::W(sop2("s_add_u32", 2, 2, 129)),
        A::W(sopc("s_cmp_lt_u32", 2, 132)),
        A::Br("s_cbranch_scc1", 1),
        A::W(vop1("v_mov_b32", 3, V + 1)),
    ]);
    let l = lift_program(&p, Stage::None, None, true);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("for (;;) {") && b.contains("if (scc) {") && b.contains("continue;"), "{b}");
    // EXEC is never narrowed: no predication
    assert!(!b.contains("if (ex)"), "{b}");
    if let Some(dxc) = dxc() {
        compile_cs(&dxc, &l, "cf_uniform_loop");
    }
}

#[test]
fn nested_loops_with_a_divergent_exit_keep_exec_as_data() {
    // outer: a uniform loop over s4; inner: each lane runs until v1 > v0, the wave until no lane is left
    let p = assemble(&[
        A::W(sop1("s_mov_b32", 4, 128)),
        A::L(1),
        A::W(sop1("s_mov_b64", 0, EXEC)),
        A::W(sop1("s_mov_b64", 2, 128)),
        A::L(2),
        A::W(vop2("v_add_f32", 1, 242, 1)),
        A::W(vopc("v_cmp_gt_f32", V + 1, 0)),
        A::W(sop2("s_or_b64", 2, VCC, 2)),
        A::W(sop2("s_andn2_b64", EXEC, EXEC, 2)),
        A::Br("s_cbranch_execnz", 2),
        A::W(sop1("s_mov_b64", EXEC, 0)),
        A::W(sop2("s_add_u32", 4, 4, 129)),
        A::W(sopc("s_cmp_lt_u32", 4, 131)),
        A::Br("s_cbranch_scc1", 1),
        A::W(vop1("v_mov_b32", 3, V + 1)),
    ]);
    let l = lift_program(&p, Stage::None, None, true);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert_eq!(b.matches("for (;;) {").count(), 2, "{b}");
    assert!(b.contains("if (WaveActiveAnyTrue(ex)) {") && b.contains("if (scc) {"), "{b}");
    // the exit mask starts as the constant 0 and accumulates per lane
    assert!(b.contains("m2 = false;") && b.contains("m2 = vcc || m2;") && b.contains("ex = ex && !m2;"), "{b}");
    // the inner loop's VALU work is predicated from its first iteration on (the back edge narrows EXEC)
    assert!(b.contains("if (ex) v1 = "), "{b}");
    if let Some(dxc) = dxc() {
        compile_cs(&dxc, &l, "cf_nested_loops");
    }
}

#[test]
fn an_early_exit_jumps_out_of_a_loop() {
    // a loop that may end the program from its middle (s_endpgm inside), and continues otherwise
    let p = assemble(&[
        A::L(1),
        A::W(vop2("v_add_f32", 1, 242, 1)),
        A::W(sopc("s_cmp_eq_u32", 5, 128)),
        A::Br("s_cbranch_scc0", 2),
        A::W(vop1("v_mov_b32", 2, V + 1)),
        A::W(endpgm()),
        A::L(2),
        A::W(sop2("s_add_u32", 5, 5, 193)),
        A::Br("s_branch", 1),
    ]);
    let l = lift_program(&p, Stage::None, None, true);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    if let Some(dxc) = dxc() {
        compile_cs(&dxc, &l, "cf_early_exit");
    }
}

#[test]
fn an_irreducible_graph_is_refused_with_its_reason() {
    // entry → A or B; A falls into B; B branches back to A: two entries into one cycle
    let p = assemble(&[
        A::W(sopc("s_cmp_eq_u32", 0, 128)),
        A::Br("s_cbranch_scc1", 2),
        A::L(1),
        A::W(vop1("v_mov_b32", 1, V + 0)),
        A::W(sopc("s_cmp_eq_u32", 1, 128)),
        A::Br("s_cbranch_scc1", 3),
        A::L(2),
        A::W(vop1("v_mov_b32", 2, V + 0)),
        A::W(sopc("s_cmp_eq_u32", 2, 128)),
        A::Br("s_cbranch_scc1", 1),
        A::L(3),
    ]);
    let l = lift_program(&p, Stage::None, None, true);
    assert_eq!(l.unsupported, vec![(0, "irreducible control flow".to_string())]);
}

#[test]
fn a_mask_read_as_data_is_refused() {
    let p = assemble(&[A::W(vopc("v_cmp_gt_f32", V + 0, 1)), A::W(sop1("s_and_saveexec_b64", 0, VCC)), A::W(vop1("v_mov_b32", 1, 0))]);
    let l = lift_program(&p, Stage::None, None, true);
    assert_eq!(l.unsupported.len(), 1, "{:?}", l.unsupported);
    assert!(l.unsupported[0].1.contains("a lane mask read as data"), "{:?}", l.unsupported);
}

#[test]
fn a_quad_permute_reads_its_quad_and_respects_lanes_exec_has_off() {
    // v_mov_b32 v2, v1 quad_perm:[1,0,3,2] (swap neighbours), first with EXEC full, then narrowed
    let dpp = |vdst: u32, src: u32| vec![0x7E00_0000 | vdst << 17 | op(Encoding::Vop1, "v_mov_b32") << 9 | 0xFA, src | 0b10_11_00_01 << 8 | 0xF << 24 | 0xF << 28];
    let p = assemble(&[A::W(dpp(2, 1)), A::W(vopc("v_cmpx_lt_u32", 128 + 3, 0)), A::W(dpp(3, 1))]);
    let l = lift_program(&p, Stage::None, None, true);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("QuadReadLaneAt(v1, 1), QuadReadLaneAt(v1, 0), QuadReadLaneAt(v1, 3), QuadReadLaneAt(v1, 2)"), "{b}");
    assert!(b.contains("v2 = sir_dpp;"), "{b}");
    // narrowed, no bound_ctrl: a lane whose source lane is off keeps its value
    assert!(b.contains("QuadReadLaneAt(ex, 1)") && b.contains("if (ex && sir_dv) v3 = sir_dpp;"), "{b}");
    if let Some(dxc) = dxc() {
        compile_cs(&dxc, &l, "dpp_quad");
    }
}
