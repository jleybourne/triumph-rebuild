//! v1: programs with control flow (`docs/design/shader-ir.md`, "Translation from GFX10", step 4).
//!
//! The decoded instructions are cut into basic blocks at branches. A reducible graph is structured with
//! Ramsey's algorithm ("Beyond Relooper", ICFP 2022): each block is placed under its immediate dominator; a
//! block with two or more forward predecessors (a merge) follows a `block` construct its predecessors
//! break out of; a loop header heads a `loop` construct its back edges continue. In HLSL a `loop` is
//! `for (;;) { ... break; }` and a `block` is `do { ... } while (false)`. A labelled branch becomes `break` or
//! `continue` when its target is the innermost construct (or a `break` falls through to the target), else a
//! jump through the `sir_to` variable that each crossed construct hands outwards. A branch that would only
//! fall through is dropped, and a `block` nothing breaks out of is not emitted, so an if/else diamond is a
//! plain `if`/`else`.
//!
//! **EXEC stays data.** Every console branch becomes a host branch on a wave-uniform value: SCC (scalar), or
//! whether any lane of VCC or EXEC is set (`WaveActiveAnyTrue`). Divergence is predication by `ex`, as in
//! v0, so control flow on the host is uniform: quad and wave operations (derivatives included) always see
//! every lane, and a region no lane of the host wave runs (`s_cbranch_execz`) is still skipped.
//!
//! What the lifter knows (loaded descriptors, constants, which SGPRs hold lane masks, whether EXEC is
//! narrowed) is a forward dataflow over the blocks: met where paths join, and iterated to a fixed point
//! around loops.

use std::collections::BTreeSet;

use rebuild_shader::gfx10::{Encoding, Instruction};

use super::{FlowState, Lifted, Lifter};

/// A conditional branch's test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Scc0,
    Scc1,
    Vccz,
    Vccnz,
    Execz,
    Execnz,
}

impl Cond {
    fn from_name(n: &str) -> Option<Cond> {
        Some(match n {
            "s_cbranch_scc0" => Cond::Scc0,
            "s_cbranch_scc1" => Cond::Scc1,
            "s_cbranch_vccz" => Cond::Vccz,
            "s_cbranch_vccnz" => Cond::Vccnz,
            "s_cbranch_execz" => Cond::Execz,
            "s_cbranch_execnz" => Cond::Execnz,
            _ => return None,
        })
    }
}

/// How a block ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Term {
    /// `s_endpgm`, or the end of the code
    Exit,
    Goto(usize),
    /// to `taken` when the test holds, else to `fall`
    Branch { cond: Cond, taken: usize, fall: usize },
}

/// A basic block: instructions `first..body_end` are lifted; a branch instruction after them is the
/// terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub first: usize,
    pub body_end: usize,
    pub term: Term,
}

/// Basic blocks, in code order; block 0 is the entry.
pub fn blocks(insts: &[Instruction]) -> Result<Vec<Block>, String> {
    #[derive(Clone, Copy)]
    enum F {
        Next,
        Stop,
        Jump(usize),
        Br(Cond, usize),
    }
    let n = insts.len();
    if n == 0 {
        return Err("no instructions".into());
    }
    let index_of = |off: usize| insts.binary_search_by_key(&off, |i| i.offset).ok();
    let mut flow = Vec::with_capacity(n);
    let mut leader = vec![false; n];
    leader[0] = true;
    for (k, i) in insts.iter().enumerate() {
        let name = i.name().unwrap_or("");
        let next = i.offset + 4 * i.words.len();
        let target = || -> Result<usize, String> {
            let t = next as i64 + 4 * i64::from(i.words[0] as u16 as i16);
            usize::try_from(t).ok().and_then(index_of).ok_or_else(|| format!("{name} (target is no instruction)"))
        };
        let f = match (i.encoding, name) {
            (Encoding::Sopp, "s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" | "s_code_end") => F::Stop,
            (Encoding::Sopp, "s_branch") => F::Jump(target()?),
            (Encoding::Sopp, b) if b.starts_with("s_cbranch_") => {
                F::Br(Cond::from_name(b).ok_or_else(|| format!("{b} (branch kind)"))?, target()?)
            }
            (_, "s_setpc_b64") => return Err("s_setpc_b64 (indirect jump)".into()),
            (_, "s_call_b64") => return Err("s_call_b64 (call)".into()),
            _ => F::Next,
        };
        if matches!(f, F::Next | F::Br(..)) && k + 1 < n && insts[k + 1].offset != next {
            return Err(format!("{name} (falls into undecoded code)"));
        }
        match f {
            F::Jump(t) | F::Br(_, t) => {
                leader[t] = true;
                if k + 1 < n {
                    leader[k + 1] = true;
                }
            }
            F::Stop if k + 1 < n => leader[k + 1] = true,
            _ => {}
        }
        flow.push(f);
    }
    let mut block_of = vec![0usize; n];
    let mut starts = Vec::new();
    for k in 0..n {
        if leader[k] {
            starts.push(k);
        }
        block_of[k] = starts.len() - 1;
    }
    let mut out = Vec::with_capacity(starts.len());
    for (b, &first) in starts.iter().enumerate() {
        let last = starts.get(b + 1).copied().unwrap_or(n) - 1;
        let (body_end, term) = match flow[last] {
            F::Stop => (last + 1, Term::Exit),
            F::Next => (last + 1, if last + 1 < n { Term::Goto(block_of[last + 1]) } else { Term::Exit }),
            F::Jump(t) => (last, Term::Goto(block_of[t])),
            F::Br(cond, t) => {
                if last + 1 >= n {
                    return Err("a branch at the end of the code".into());
                }
                let (taken, fall) = (block_of[t], block_of[last + 1]);
                (last, if taken == fall { Term::Goto(fall) } else { Term::Branch { cond, taken, fall } })
            }
        };
        out.push(Block { first, body_end, term });
    }
    Ok(out)
}

/// Successors of a block (fall-through first).
pub fn succs(t: Term) -> Vec<usize> {
    match t {
        Term::Exit => vec![],
        Term::Goto(b) => vec![b],
        Term::Branch { taken, fall, .. } => vec![fall, taken],
    }
}

/// The graph facts the structurizer needs.
#[derive(Debug, Clone)]
pub struct Graph {
    pub preds: Vec<Vec<usize>>,
    /// reverse postorder number (`usize::MAX`: unreachable)
    pub num: Vec<usize>,
    pub idom: Vec<usize>,
    /// the target of a back edge
    pub header: Vec<bool>,
    /// two or more forward predecessors
    pub merge: Vec<bool>,
}

/// Reverse postorder, dominators (Cooper, Harvey, Kennedy), loop headers and merges. `Err` for an
/// irreducible graph (a retreating edge to a block that does not dominate its source).
pub fn analyse(blocks: &[Block]) -> Result<Graph, String> {
    let n = blocks.len();
    let succ: Vec<Vec<usize>> = blocks.iter().map(|b| succs(b.term)).collect();
    let mut seen = vec![false; n];
    let mut post = Vec::with_capacity(n);
    let mut stack = vec![(0usize, 0usize)];
    seen[0] = true;
    while let Some(&mut (v, ref mut e)) = stack.last_mut() {
        if let Some(&w) = succ[v].get(*e) {
            *e += 1;
            if !seen[w] {
                seen[w] = true;
                stack.push((w, 0));
            }
        } else {
            post.push(v);
            stack.pop();
        }
    }
    let order: Vec<usize> = post.into_iter().rev().collect();
    let mut num = vec![usize::MAX; n];
    for (i, &v) in order.iter().enumerate() {
        num[v] = i;
    }
    let mut preds = vec![Vec::new(); n];
    for &v in &order {
        for &w in &succ[v] {
            preds[w].push(v);
        }
    }
    let mut idom = vec![usize::MAX; n];
    idom[0] = 0;
    let mut changed = true;
    while changed {
        changed = false;
        for &v in order.iter().skip(1) {
            let mut new = usize::MAX;
            for &p in &preds[v] {
                if idom[p] == usize::MAX {
                    continue;
                }
                new = if new == usize::MAX {
                    p
                } else {
                    let (mut a, mut b) = (p, new);
                    while a != b {
                        while num[a] > num[b] {
                            a = idom[a];
                        }
                        while num[b] > num[a] {
                            b = idom[b];
                        }
                    }
                    a
                };
            }
            if new != idom[v] {
                idom[v] = new;
                changed = true;
            }
        }
    }
    let dominates = |a: usize, mut b: usize| loop {
        if a == b {
            return true;
        }
        if b == 0 {
            return false;
        }
        b = idom[b];
    };
    let mut header = vec![false; n];
    for &v in &order {
        for &w in &succ[v] {
            if num[w] <= num[v] {
                if !dominates(w, v) {
                    return Err("irreducible control flow".into());
                }
                header[w] = true;
            }
        }
    }
    let merge = (0..n).map(|v| preds[v].iter().filter(|&&p| num[p] < num[v]).count() >= 2).collect();
    Ok(Graph { preds, num, idom, header, merge })
}

/// What a labelled branch does to its construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// leave the construct (a `block`, or the whole program)
    Exit,
    /// start the construct again (a `loop`)
    Continue,
}

/// Structured control flow over blocks, with labelled branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ast {
    Code(usize),
    Seq(Vec<Ast>),
    /// at the end of the block: the taken arm, then the fall-through arm
    If(usize, Box<Ast>, Box<Ast>),
    Block(usize, Box<Ast>),
    /// falling off the end leaves the loop; only a `Br` to its label goes round again
    Loop(usize, Box<Ast>),
    /// (label, branch id)
    Br(usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Top,
    LoopHeadedBy(usize),
    BlockFollowedBy(usize),
}

/// A structured program: label 0 is the whole program (`Exit` from it is `s_endpgm`).
#[derive(Debug, Clone)]
pub struct Structured {
    pub ast: Ast,
    pub labels: Vec<Kind>,
}

struct Builder<'a> {
    blocks: &'a [Block],
    g: &'a Graph,
    children: Vec<Vec<usize>>,
    labels: Vec<Kind>,
    ctx: Vec<(Ctx, usize)>,
    brs: usize,
    lost: bool,
}

impl Builder<'_> {
    fn label(&mut self, k: Kind) -> usize {
        self.labels.push(k);
        self.labels.len() - 1
    }

    fn do_tree(&mut self, x: usize) -> Ast {
        let mut ys: Vec<usize> = self.children[x].iter().copied().filter(|&c| self.g.merge[c]).collect();
        ys.sort_by_key(|&y| std::cmp::Reverse(self.g.num[y]));
        if self.g.header[x] {
            let l = self.label(Kind::Continue);
            self.ctx.push((Ctx::LoopHeadedBy(x), l));
            let body = self.node_within(x, &ys);
            self.ctx.pop();
            Ast::Loop(l, Box::new(body))
        } else {
            self.node_within(x, &ys)
        }
    }

    /// `x`'s code inside a `block` per merge child (the highest-numbered outermost), each child after its block.
    fn node_within(&mut self, x: usize, ys: &[usize]) -> Ast {
        match ys.split_first() {
            None => {
                let b = self.branches(x);
                Ast::Seq(vec![Ast::Code(x), b])
            }
            Some((&y, rest)) => {
                let l = self.label(Kind::Exit);
                self.ctx.push((Ctx::BlockFollowedBy(y), l));
                let inner = self.node_within(x, rest);
                self.ctx.pop();
                let after = self.do_tree(y);
                Ast::Seq(vec![Ast::Block(l, Box::new(inner)), after])
            }
        }
    }

    fn branches(&mut self, x: usize) -> Ast {
        match self.blocks[x].term {
            Term::Exit => self.br_to(Ctx::Top),
            Term::Goto(t) => self.do_branch(x, t),
            Term::Branch { taken, fall, .. } => {
                let a = self.do_branch(x, taken);
                let b = self.do_branch(x, fall);
                Ast::If(x, Box::new(a), Box::new(b))
            }
        }
    }

    fn do_branch(&mut self, x: usize, y: usize) -> Ast {
        if self.g.num[y] <= self.g.num[x] {
            self.br_to(Ctx::LoopHeadedBy(y))
        } else if self.g.merge[y] {
            self.br_to(Ctx::BlockFollowedBy(y))
        } else {
            self.do_tree(y)
        }
    }

    fn br_to(&mut self, c: Ctx) -> Ast {
        let l = match self.ctx.iter().rev().find(|(k, _)| *k == c) {
            Some((_, l)) => *l,
            None => {
                self.lost = true;
                0
            }
        };
        self.brs += 1;
        Ast::Br(l, self.brs - 1)
    }
}

/// Ramsey's structuring of a reducible graph.
pub fn structure(blocks: &[Block], g: &Graph) -> Result<Structured, String> {
    let n = blocks.len();
    let mut children = vec![Vec::new(); n];
    for v in 1..n {
        if g.num[v] != usize::MAX {
            children[g.idom[v]].push(v);
        }
    }
    let mut b = Builder { blocks, g, children, labels: vec![Kind::Exit], ctx: vec![(Ctx::Top, 0)], brs: 0, lost: false };
    let body = b.do_tree(0);
    if b.lost {
        return Err("a branch target outside its construct".into());
    }
    Ok(Structured { ast: Ast::Block(0, Box::new(body)), labels: b.labels })
}

/// The HLSL-level statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H {
    Code(usize),
    If(usize, Vec<H>, Vec<H>),
    /// `for (;;) { body break; }` when `looping`, else `do { body } while (false);`
    Construct { label: usize, looping: bool, body: Vec<H> },
    Break,
    Continue,
    /// `sir_to = label + 1; break;`
    Jump(usize),
    /// after a construct some jump left: take it here (break or continue the enclosing construct), or pass it on
    Dispatch { here: Option<(usize, Kind)>, further: bool },
}

struct Frame {
    label: usize,
    /// nothing follows the construct inside the enclosing emitted construct
    tail: bool,
    crossing: BTreeSet<usize>,
}

struct Lower<'a> {
    labels: &'a [Kind],
    dropped: BTreeSet<usize>,
    counts: Vec<usize>,
    stack: Vec<Frame>,
    jumps: bool,
}

/// Branches that only fall through to their target (`Exit` kind, every construct on the way in tail position).
fn drops(a: &Ast, tail: bool, labels: &[Kind], stack: &mut Vec<(usize, bool)>, out: &mut BTreeSet<usize>) {
    match a {
        Ast::Code(_) => {}
        Ast::Seq(v) => {
            for (k, x) in v.iter().enumerate() {
                drops(x, tail && k + 1 == v.len(), labels, stack, out);
            }
        }
        Ast::If(_, t, e) => {
            drops(t, tail, labels, stack, out);
            drops(e, tail, labels, stack, out);
        }
        Ast::Block(l, b) | Ast::Loop(l, b) => {
            stack.push((*l, tail));
            drops(b, true, labels, stack, out);
            stack.pop();
        }
        Ast::Br(l, id) => {
            if labels[*l] == Kind::Exit && tail {
                if let Some(p) = stack.iter().rposition(|(x, _)| x == l) {
                    if stack[p + 1..].iter().all(|(_, t)| *t) {
                        out.insert(*id);
                    }
                }
            }
        }
    }
}

fn count(a: &Ast, dropped: &BTreeSet<usize>, counts: &mut [usize]) {
    match a {
        Ast::Code(_) => {}
        Ast::Seq(v) => v.iter().for_each(|x| count(x, dropped, counts)),
        Ast::If(_, t, e) => {
            count(t, dropped, counts);
            count(e, dropped, counts);
        }
        Ast::Block(_, b) | Ast::Loop(_, b) => count(b, dropped, counts),
        Ast::Br(l, id) => {
            if !dropped.contains(id) {
                counts[*l] += 1;
            }
        }
    }
}

impl Lower<'_> {
    fn lower(&mut self, a: &Ast, tail: bool, out: &mut Vec<H>) -> Result<(), String> {
        match a {
            Ast::Code(b) => out.push(H::Code(*b)),
            Ast::Seq(v) => {
                for (k, x) in v.iter().enumerate() {
                    self.lower(x, tail && k + 1 == v.len(), out)?;
                }
            }
            Ast::If(b, t, e) => {
                let (mut x, mut y) = (Vec::new(), Vec::new());
                self.lower(t, tail, &mut x)?;
                self.lower(e, tail, &mut y)?;
                out.push(H::If(*b, x, y));
            }
            Ast::Block(l, body) if self.counts[*l] == 0 => self.lower(body, tail, out)?,
            Ast::Block(l, body) | Ast::Loop(l, body) => {
                let looping = matches!(a, Ast::Loop(..));
                self.stack.push(Frame { label: *l, tail, crossing: BTreeSet::new() });
                let mut inner = Vec::new();
                self.lower(body, true, &mut inner)?;
                let f = self.stack.pop().expect("pushed");
                out.push(H::Construct { label: *l, looping, body: inner });
                if !f.crossing.is_empty() {
                    let parent = self.stack.last().ok_or("a jump out of the program")?.label;
                    let here = f.crossing.contains(&parent).then(|| (parent, self.labels[parent]));
                    let further = f.crossing.iter().any(|&x| x != parent);
                    out.push(H::Dispatch { here, further });
                }
            }
            Ast::Br(_, id) if self.dropped.contains(id) => {}
            Ast::Br(l, _) => {
                let p = self.stack.iter().rposition(|f| f.label == *l).ok_or("a branch to a construct not emitted")?;
                let kind = self.labels[*l];
                if p + 1 == self.stack.len() {
                    out.push(if kind == Kind::Exit { H::Break } else { H::Continue });
                } else if kind == Kind::Exit && self.stack[p + 1..].iter().all(|f| f.tail) {
                    // leaving the innermost construct falls through every enclosing one to the target's end
                    out.push(H::Break);
                } else {
                    for f in &mut self.stack[p + 1..] {
                        f.crossing.insert(*l);
                    }
                    self.jumps = true;
                    out.push(H::Jump(*l));
                }
            }
        }
        Ok(())
    }
}

/// The HLSL statements for a structured program, and whether it needs the `sir_to` variable.
pub fn lower(s: &Structured) -> Result<(Vec<H>, bool), String> {
    let mut dropped = BTreeSet::new();
    drops(&s.ast, true, &s.labels, &mut Vec::new(), &mut dropped);
    let mut counts = vec![0; s.labels.len()];
    count(&s.ast, &dropped, &mut counts);
    let mut l = Lower { labels: &s.labels, dropped, counts, stack: Vec::new(), jumps: false };
    let mut out = Vec::new();
    l.lower(&s.ast, true, &mut out)?;
    Ok((out, l.jumps))
}

/// Reads SCC.
fn scc_reads(i: &Instruction) -> bool {
    let n = i.name().unwrap_or("");
    n.starts_with("s_cselect") || n.starts_with("s_cmov") || n.starts_with("s_cbranch_scc") || matches!(n, "s_addc_u32" | "s_subb_u32")
}

/// Writes SCC (a conservative list: an op missing here only keeps SCC live longer).
fn scc_writes(i: &Instruction) -> bool {
    let n = i.name().unwrap_or("");
    if n.starts_with("s_cmp") || n.starts_with("s_bitcmp") || n.contains("saveexec") || n.contains("wrexec") {
        return true;
    }
    let base = n.trim_end_matches("_b32").trim_end_matches("_b64").trim_end_matches("_u32").trim_end_matches("_i32");
    matches!(i.encoding, Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopk)
        && matches!(
            base,
            "s_and" | "s_or" | "s_xor" | "s_andn2" | "s_orn2" | "s_nand" | "s_nor" | "s_xnor" | "s_not" | "s_add" | "s_sub"
                | "s_addc" | "s_subb" | "s_addk" | "s_min" | "s_max" | "s_lshl" | "s_lshr" | "s_ashr" | "s_bfe" | "s_absdiff"
                | "s_abs" | "s_lshl1_add" | "s_lshl2_add" | "s_lshl3_add" | "s_lshl4_add" | "s_bcnt0_i32" | "s_bcnt1_i32"
                | "s_quadmask" | "s_wqm"
        )
}

/// SCC liveness after each instruction of a straight line (dead at its end).
pub(super) fn scc_live_straight(insts: &[Instruction]) -> Vec<bool> {
    let mut live = vec![false; insts.len()];
    let mut l = false;
    for (k, i) in insts.iter().enumerate().rev() {
        live[k] = l;
        l = (l && !scc_writes(i)) || scc_reads(i);
    }
    live
}

/// SCC liveness after each instruction, over the graph.
fn scc_live(insts: &[Instruction], blocks: &[Block]) -> Vec<bool> {
    let n = blocks.len();
    let mut live_in = vec![false; n];
    let mut live = vec![false; insts.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (b, blk) in blocks.iter().enumerate().rev() {
            let mut l = succs(blk.term).iter().any(|&s| live_in[s]);
            if matches!(blk.term, Term::Branch { cond: Cond::Scc0 | Cond::Scc1, .. }) {
                l = true;
            }
            for k in (blk.first..blk.body_end).rev() {
                live[k] = l;
                l = (l && !scc_writes(&insts[k])) || scc_reads(&insts[k]);
            }
            if l != live_in[b] {
                live_in[b] = l;
                changed = true;
            }
        }
    }
    live
}

struct Emit<'a, 'b, 'l> {
    l: &'b mut Lifter<'l>,
    insts: &'a [Instruction],
    blocks: &'a [Block],
    g: &'a Graph,
    live: &'a [bool],
    prev: &'a [Option<FlowState>],
    cur: Vec<Option<FlowState>>,
    text: String,
}

impl Emit<'_, '_, '_> {
    fn line(&mut self, depth: usize, s: &str) {
        for _ in 0..depth {
            self.text.push_str("    ");
        }
        self.text.push_str(s);
        self.text.push('\n');
    }

    /// The state on entry to block `b`: the meet over its predecessors' exits (this pass's, or the last
    /// pass's for a back edge).
    fn entry(&self, b: usize) -> Result<FlowState, String> {
        let mut st = (b == 0).then(|| FlowState { narrowed: self.l.entry_narrowed, ..FlowState::default() });
        for &p in &self.g.preds[b] {
            if let Some(ps) = self.cur[p].as_ref().or(self.prev[p].as_ref()) {
                st = Some(match st {
                    None => ps.clone(),
                    Some(s) => s.meet(ps),
                });
            }
        }
        st.ok_or_else(|| "a block placed before all its predecessors".into())
    }

    fn cond(&mut self, b: usize) -> String {
        let Term::Branch { cond, .. } = self.blocks[b].term else { return "true".into() };
        match cond {
            Cond::Scc0 => "!scc".into(),
            Cond::Scc1 => "scc".into(),
            // VCC holding data (written as a spare SGPR pair on every path): the branch tests the data
            Cond::Vccz | Cond::Vccnz if self.l.f.written.contains(&106) => {
                let hi = if self.l.wave64 && self.l.f.written.contains(&107) { " | s107" } else { "" };
                let z = if cond == Cond::Vccz { "==" } else { "!=" };
                format!("(s106{hi}) {z} 0u")
            }
            Cond::Vccz => format!("!{}", self.l.any_lane(106)),
            Cond::Vccnz => self.l.any_lane(106),
            Cond::Execz => format!("!{}", self.l.any_lane(126)),
            Cond::Execnz => self.l.any_lane(126),
        }
    }

    fn emit(&mut self, hs: &[H], depth: usize) -> Result<(), String> {
        for h in hs {
            match h {
                H::Code(b) => {
                    self.l.f = self.entry(*b)?;
                    let blk = &self.blocks[*b];
                    let start = self.l.out.body.len();
                    self.l.run(&self.insts[blk.first..blk.body_end], &self.live[blk.first..blk.body_end]);
                    let chunk = self.l.out.body.split_off(start);
                    for line in chunk.lines() {
                        // the lifter's lines carry one level of indent already
                        self.line(depth - 1, line);
                    }
                    self.cur[*b] = Some(self.l.f.clone());
                }
                H::If(b, t, e) => {
                    let c = self.cond(*b);
                    match (t.is_empty(), e.is_empty()) {
                        (true, true) => {}
                        (true, false) => {
                            let n = match c.strip_prefix('!') {
                                Some(r) if !r.contains(' ') => r.to_string(),
                                _ => format!("!({c})"),
                            };
                            self.line(depth, &format!("if ({n}) {{"));
                            self.emit(e, depth + 1)?;
                            self.line(depth, "}");
                        }
                        _ => {
                            self.line(depth, &format!("if ({c}) {{"));
                            self.emit(t, depth + 1)?;
                            if !e.is_empty() {
                                self.line(depth, "} else {");
                                self.emit(e, depth + 1)?;
                            }
                            self.line(depth, "}");
                        }
                    }
                }
                H::Construct { looping: true, body, .. } => {
                    self.line(depth, "for (;;) {");
                    self.emit(body, depth + 1)?;
                    self.line(depth + 1, "break;");
                    self.line(depth, "}");
                }
                H::Construct { looping: false, body, .. } => {
                    self.line(depth, "do {");
                    self.emit(body, depth + 1)?;
                    self.line(depth, "} while (false);");
                }
                H::Break => self.line(depth, "break;"),
                H::Continue => self.line(depth, "continue;"),
                H::Jump(l) => self.line(depth, &format!("sir_to = {}u; break;", l + 1)),
                H::Dispatch { here, further } => {
                    self.line(depth, "if (sir_to != 0u) {");
                    if let Some((p, k)) = here {
                        let go = if *k == Kind::Exit { "break" } else { "continue" };
                        self.line(depth + 1, &format!("if (sir_to == {}u) {{ sir_to = 0u; {go}; }}", p + 1));
                    }
                    if *further {
                        self.line(depth + 1, "break;");
                    }
                    self.line(depth, "}");
                }
            }
        }
        Ok(())
    }
}

/// Lifts a whole program with control flow (see the module documentation).
pub(super) fn lift(mut l: Lifter<'_>, insts: &[Instruction]) -> Lifted {
    let refuse = |mut l: Lifter<'_>, why: String| {
        l.out.unsupported.push((0, why));
        l.out
    };
    let blocks = match blocks(insts) {
        Ok(b) => b,
        Err(e) => return refuse(l, e),
    };
    let g = match analyse(&blocks) {
        Ok(g) => g,
        Err(e) => return refuse(l, e),
    };
    let (h, jumps) = match structure(&blocks, &g).and_then(|s| lower(&s)) {
        Ok(x) => x,
        Err(e) => return refuse(l, e),
    };
    let live = scc_live(insts, &blocks);
    let mut exits: Vec<Option<FlowState>> = vec![None; blocks.len()];
    let mut masks = BTreeSet::new();
    for _ in 0..16 {
        l.out = Lifted { masks: masks.clone(), wave: l.wave, exec_narrowed: l.entry_narrowed, ..Lifted::default() };
        let mut e = Emit { l: &mut l, insts, blocks: &blocks, g: &g, live: &live, prev: &exits, cur: vec![None; blocks.len()], text: String::new() };
        if jumps {
            e.text.push_str("    uint sir_to = 0u;\n");
        }
        if let Err(why) = e.emit(&h, 1) {
            return refuse(l, why);
        }
        let (cur, text) = (e.cur, e.text);
        let settled = cur == exits && l.out.masks == masks;
        // a refusal with the mask set unchanged stands: facts only shrink from pass to pass (a back edge's state
        // joins in), so a later pass would refuse the same instruction; no more passes over a large program
        // (a refused on-demand program took about 100 ms in up to 16 passes)
        let refused = !l.out.unsupported.is_empty() && l.out.masks == masks;
        exits = cur;
        masks = l.out.masks.clone();
        l.out.body = text;
        if settled || refused {
            return l.out;
        }
    }
    refuse(l, "lifter facts did not settle around a loop".into())
}

