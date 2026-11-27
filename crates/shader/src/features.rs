//! Per-program numbers: the census row (S2), the re-encode check (S3), and the
//! feature summary the lead's IR design reads (S4).
//!
//! Definitions, so the numbers can be checked:
//!
//! ```text
//! dpp        VOP1/VOP2/VOPC with src0 = 0xFA (DPP16) or 0xE9/0xEA (DPP8)
//! sdwa       VOP1/VOP2/VOPC with src0 = 0xF9
//! lds        DS instructions without GDS (ds_swizzle/permute counted: they use the LDS crossbar)
//! scratch    SCRATCH-segment instructions (scratch_load/store), plus MUBUF ops whose
//!            soffset is the scratch wave offset is not decidable here, so not counted
//! b16        16-bit work: an opcode name with _f16/_i16/_u16/_b16, v_pk_*, *_mix*, *_d16*,
//!            or a MIMG with d16 or a16 set
//! branches   s_cbranch_scc*: wave-uniform (scalar condition)
//!            s_cbranch_vcc*: wave-uniform test of a lane mask (VCC all zero / not)
//!            s_cbranch_exec*: per-lane (skips a region no lane runs; the region is
//!            entered by an EXEC mask change)
//! regions    instructions that save and narrow EXEC (*saveexec*, *wrexec*): divergent
//!            regions, with or without a skip branch
//! structured the CFG is reducible (every retreating edge targets a dominator) and
//!            has no unresolved indirect jump (s_setpc_b64 not in the far-jump form)
//!            and no s_call_b64
//! ```

use std::collections::BTreeMap;

use crate::gfx10::{self, Encoding, Flow, Instruction, enc};
use crate::native::NativeProgram;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub code_bytes: usize,
    /// Instructions reachable from offset 0; 0 if the code does not decode.
    pub instructions: usize,
    /// Scheduling hints, counters and tracing (s_nop, s_waitcnt*, s_clause...):
    /// no effect on results; the fork's IR drops them.
    pub hints: usize,
    pub decode_error: Option<&'static str>,
    pub dpp: usize,
    pub sdwa: usize,
    pub lds: usize,
    pub gds: usize,
    pub scratch: usize,
    pub b16: usize,
    /// Instructions that re-encode byte-exact.
    pub exact: usize,
    pub blocks: usize,
    pub branch_scc: usize,
    pub branch_vcc: usize,
    pub branch_exec: usize,
    pub branch_always: usize,
    pub branch_other: usize,
    pub regions: usize,
    /// Back edges (loops) in the CFG.
    pub loops: usize,
    pub reducible: bool,
    pub indirect: usize,
    pub calls: usize,
    /// Cross-lane operations of any width (quad ones included).
    pub wave_ops: usize,
    /// Of those, the ones that see beyond a quad (the wave-wide triggers), not
    /// counting v_readfirstlane_b32.
    pub wide: usize,
    /// v_readfirstlane_b32: wave-wide only if its operand is not uniform.
    pub readfirstlane: usize,
    /// Cross-lane operations inside an EXEC-narrowed region (they become predication).
    pub wave_ops_divergent: usize,
    /// The first wave-wide trigger, by opcode name.
    pub wide_reason: Option<&'static str>,
}

/// The wave class, by the rules of `crates/sir/src/classify.rs`, applied per instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaveClass {
    Agnostic,
    /// Wave-wide only through v_readfirstlane_b32: agnostic if every such operand
    /// is uniform, which needs a uniformity pass (the lead's).
    ReadFirstLane,
    WaveWide,
}

impl WaveClass {
    pub fn name(self) -> &'static str {
        match self {
            WaveClass::Agnostic => "agnostic",
            WaveClass::ReadFirstLane => "wave-wide-if-rfl-divergent",
            WaveClass::WaveWide => "wave-wide",
        }
    }
}

impl Row {
    pub fn structured(&self) -> bool {
        self.reducible && self.indirect == 0 && self.calls == 0
    }

    pub fn class(&self) -> WaveClass {
        if self.wide > 0 {
            WaveClass::WaveWide
        } else if self.readfirstlane > 0 {
            WaveClass::ReadFirstLane
        } else {
            WaveClass::Agnostic
        }
    }
}

/// What a whole corpus run found beyond the rows.
#[derive(Debug, Default)]
pub struct Findings {
    /// (encoding, opcode) not in the opcode table, with counts.
    pub unknown: BTreeMap<(String, u16), usize>,
    /// Re-encode mismatches by kind, with counts.
    pub mismatches: BTreeMap<String, usize>,
    /// Instructions by layout (encoding family, SDWA/DPP forms counted apart).
    pub by_layout: BTreeMap<&'static str, usize>,
}

/// Instructions with no effect on results (the same list as the fork's IR drops).
const HINTS: &[&str] = &[
    "s_nop", "s_waitcnt", "s_waitcnt_vscnt", "s_waitcnt_vmcnt", "s_waitcnt_expcnt",
    "s_waitcnt_lgkmcnt", "s_waitcnt_depctr", "s_wait_idle", "s_inst_prefetch", "s_clause",
    "s_setprio", "s_sleep", "s_ttracedata", "s_ttracedata_imm", "s_incperflevel",
    "s_decperflevel", "v_nop", "v_pipeflush", "v_clrexcp",
];

/// Operand codes of the lane masks a SALU op may read as data.
const MASKS: [u32; 4] = [106, 107, 126, 127];

/// A cross-lane operation: `Some(true)` if it sees beyond a quad, `Some(false)`
/// if it stays in its quad; v_readfirstlane_b32 is reported apart by the caller.
fn cross_lane(name: &str, d: &enc::Decoded, wave64: bool) -> Option<bool> {
    if let Some((x, v)) = &d.extra {
        match x.name {
            // quad_perm is 0x000-0x0ff; row shifts, mirrors, broadcasts and shares see further
            "dpp16" => return Some(v[1] > 0xFF),
            "dpp8" => return Some(true),
            _ => {}
        }
    }
    match name {
        "v_readlane_b32" | "v_writelane_b32" | "ds_permute_b32" | "ds_bpermute_b32" => Some(true),
        // offset bit 15: quad-permute mode; else the bit-mask mode over 32 lanes
        "ds_swizzle_b32" => {
            let offset = d.get("offset0").unwrap_or(0) | d.get("offset1").unwrap_or(0) << 8;
            Some(offset & 0x8000 == 0)
        }
        n if n.starts_with("v_mbcnt") || n.starts_with("v_permlane") => Some(true),
        n if ["s_bcnt0", "s_bcnt1", "s_ff0", "s_ff1", "s_flbit"].iter().any(|p| n.starts_with(p)) => {
            let mask = d.get("ssrc0").is_some_and(|c| MASKS.contains(&c));
            (mask || (wave64 && n.ends_with("_b64"))).then_some(true)
        }
        _ => None,
    }
}

fn is_b16(name: &str) -> bool {
    ["_f16", "_i16", "_u16", "_b16", "v_pk_", "_mix", "_d16"].iter().any(|p| name.contains(p))
}

/// The row for one program; adds its unknown opcodes and mismatches to `found`.
pub fn analyse(p: &NativeProgram, found: &mut Findings) -> Row {
    let mut row = Row { code_bytes: p.code.len(), ..Row::default() };
    let insts = match gfx10::decode(&p.code) {
        Ok(i) => i,
        Err(crate::ShaderError::Isa { reason, .. }) => {
            row.decode_error = Some(reason);
            return row;
        }
        Err(_) => {
            row.decode_error = Some("other");
            return row;
        }
    };
    row.instructions = insts.len();
    let mut depth = 0usize;
    for i in &insts {
        let name = i.name();
        if name.is_none() {
            *found.unknown.entry((format!("{:?}", i.encoding), i.opcode)).or_default() += 1;
        }
        let name = name.unwrap_or("");
        let decoded = enc::round_trip(i);
        if let Ok(d) = &decoded {
            let rfl = name == "v_readfirstlane_b32";
            let cross = cross_lane(name, d, p.wave64);
            if rfl || cross.is_some() {
                row.wave_ops += 1;
                row.wave_ops_divergent += usize::from(depth > 0);
            }
            if rfl {
                row.readfirstlane += 1;
            }
            if cross == Some(true) {
                row.wide += 1;
                row.wide_reason.get_or_insert(i.name().unwrap_or("?"));
            }
            // EXEC-narrowed regions, in program order (an approximation without the CFG):
            // entered by saveexec/wrexec and v_cmpx, left by a SALU or/mov into EXEC (the
            // restore at the join).
            let exec_dst = matches!(i.encoding, Encoding::Sop1 | Encoding::Sop2)
                && d.get("sdst").is_some_and(|c| c == 126 || c == 127);
            // s_or_saveexec is the else of an if/else: the same region, not a new one.
            if name.starts_with("s_or_saveexec") {
            } else if name.contains("saveexec") || name.contains("wrexec") || name.starts_with("v_cmpx") {
                depth += 1;
            } else if exec_dst && (name.starts_with("s_or_") || name.starts_with("s_mov_")) {
                depth = depth.saturating_sub(1);
            }
        }
        match decoded {
            Ok(d) => {
                row.exact += 1;
                *found.by_layout.entry(d.layout.name).or_default() += 1;
                if let Some((x, _)) = &d.extra {
                    *found.by_layout.entry(x.name).or_default() += 1;
                    match x.name {
                        "sdwa" | "sdwab" => row.sdwa += 1,
                        _ => row.dpp += 1,
                    }
                }
                if i.encoding == Encoding::Mimg
                    && (d.get("d16") == Some(1) || d.get("a16") == Some(1))
                    && !is_b16(name)
                {
                    row.b16 += 1;
                }
                if i.encoding == Encoding::Ds {
                    if d.get("gds") == Some(1) {
                        row.gds += 1;
                    } else {
                        row.lds += 1;
                    }
                }
            }
            Err(m) => *found.mismatches.entry(format!("{m:?}")).or_default() += 1,
        }
        if is_b16(name) {
            row.b16 += 1;
        }
        if HINTS.contains(&name) {
            row.hints += 1;
        }
        if i.encoding == Encoding::Scratch {
            row.scratch += 1;
        }
        if name.contains("saveexec") || name.contains("wrexec") {
            row.regions += 1;
        }
        match (i.encoding, name) {
            (Encoding::Sopp, "s_branch") => row.branch_always += 1,
            (Encoding::Sopp, n) if n.starts_with("s_cbranch_scc") => row.branch_scc += 1,
            (Encoding::Sopp, n) if n.starts_with("s_cbranch_vcc") => row.branch_vcc += 1,
            (Encoding::Sopp, n) if n.starts_with("s_cbranch_exec") => row.branch_exec += 1,
            (Encoding::Sopp, n) if n.starts_with("s_cbranch_") => row.branch_other += 1,
            (Encoding::Sopk, "s_call_b64") => row.calls += 1,
            _ => {}
        }
    }
    cfg(&insts, p.code.len(), &mut row);
    row
}

/// Blocks, back edges, reducibility and unresolved indirect jumps.
fn cfg(insts: &[Instruction], len: usize, row: &mut Row) {
    let before = |at: usize| {
        let k = insts.partition_point(|i| i.offset < at);
        k.checked_sub(1).map(|k| &insts[k])
    };
    let index_of = |off: usize| insts.binary_search_by_key(&off, |i| i.offset).ok();
    // Successors per instruction, by index.
    let mut succ: Vec<Vec<usize>> = Vec::with_capacity(insts.len());
    let mut leader = vec![false; insts.len()];
    if !insts.is_empty() {
        leader[0] = true;
    }
    for (k, i) in insts.iter().enumerate() {
        let next = i.offset + 4 * i.words.len();
        let flow = gfx10::control(i, len, before).unwrap_or(Flow::Stop);
        if flow == Flow::Stop && i.name() == Some("s_setpc_b64") {
            row.indirect += 1;
        }
        let mut s = Vec::new();
        let push = |off: usize, s: &mut Vec<usize>| {
            if let Some(t) = index_of(off) {
                s.push(t);
            }
        };
        match flow {
            Flow::Next => push(next, &mut s),
            Flow::Stop => {}
            Flow::Jump(t) => push(t, &mut s),
            Flow::Branch(t) => {
                push(t, &mut s);
                push(next, &mut s);
            }
        }
        if !matches!(flow, Flow::Next) {
            for &t in &s {
                leader[t] = true;
            }
            if k + 1 < insts.len() {
                leader[k + 1] = true;
            }
        }
        succ.push(s);
    }
    // Blocks: runs of instructions from each leader.
    let mut block_of = vec![0usize; insts.len()];
    let mut b = 0usize;
    for k in 0..insts.len() {
        if leader[k] && k > 0 {
            b += 1;
        }
        block_of[k] = b;
    }
    let n = if insts.is_empty() { 0 } else { b + 1 };
    row.blocks = n;
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, s) in succ.iter().enumerate() {
        for &t in s {
            let (from, to) = (block_of[k], block_of[t]);
            // Fall-through inside a block is not an edge.
            if from != to || t <= k {
                if !edges[from].contains(&to) {
                    edges[from].push(to);
                }
            }
        }
    }
    row.reducible = true;
    if n == 0 {
        return;
    }
    // Iterative DFS: preorder and reverse postorder, retreating edges.
    let mut state = vec![0u8; n]; // 0 new, 1 on stack, 2 done
    let mut post = Vec::with_capacity(n);
    let mut retreating = Vec::new();
    let mut stack = vec![(0usize, 0usize)];
    state[0] = 1;
    while let Some(&mut (v, ref mut e)) = stack.last_mut() {
        if let Some(&w) = edges[v].get(*e) {
            *e += 1;
            match state[w] {
                0 => {
                    state[w] = 1;
                    stack.push((w, 0));
                }
                1 => retreating.push((v, w)),
                _ => {}
            }
        } else {
            state[v] = 2;
            post.push(v);
            stack.pop();
        }
    }
    // Dominators (Cooper-Harvey-Kennedy) over the reachable blocks.
    let rpo: Vec<usize> = post.iter().rev().copied().collect();
    let mut order = vec![usize::MAX; n];
    for (i, &v) in rpo.iter().enumerate() {
        order[v] = i;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (v, es) in edges.iter().enumerate() {
        if order[v] == usize::MAX {
            continue;
        }
        for &w in es {
            preds[w].push(v);
        }
    }
    let mut idom = vec![usize::MAX; n];
    idom[0] = 0;
    let mut changed = true;
    while changed {
        changed = false;
        for &v in rpo.iter().skip(1) {
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
                        while order[a] > order[b] {
                            a = idom[a];
                        }
                        while order[b] > order[a] {
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
        if b == 0 || idom[b] == usize::MAX {
            return false;
        }
        b = idom[b];
    };
    row.loops = retreating.len();
    row.reducible = retreating.iter().all(|&(v, w)| dominates(w, v));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{HwStage, StageIo};

    const S_ENDPGM: u32 = 0xBF81_0000;
    const fn sopp(op: u32, simm16: i16) -> u32 {
        0xBF80_0000 | op << 16 | simm16 as u16 as u32
    }

    fn program(words: &[u32]) -> NativeProgram {
        NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 4,
            sgprs: 8,
            lds_bytes: 0,
            layout: vec![],
            used: vec![],
            io: StageIo::None,
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        }
    }

    #[test]
    fn counts_branch_kinds_regions_and_a_loop() {
        let code = [
            0xBE80_3C6A,    // 0: s_and_saveexec_b32 s0, vcc_lo (a divergent region)
            sopp(0x08, 3),  // 4: s_cbranch_execz -> 20 (per lane)
            0x7E00_02FA,    // 8: v_mov_b32 with DPP16
            0xFF08_1B01,
            sopp(0x04, -3), // 16: s_cbranch_scc0 -> 8 (uniform, a loop)
            S_ENDPGM,       // 20
        ];
        let mut f = Findings::default();
        let r = analyse(&program(&code), &mut f);
        assert_eq!(r.decode_error, None);
        assert_eq!((r.instructions, r.exact, r.blocks), (5, 5, 3));
        assert_eq!((r.regions, r.branch_exec, r.branch_scc, r.dpp), (1, 1, 1, 1));
        assert_eq!((r.loops, r.reducible, r.structured()), (1, true, true));
        assert!(f.unknown.is_empty() && f.mismatches.is_empty());
    }

    #[test]
    fn irreducible_cfg_is_named() {
        // Two entries into the cycle {A, B}: 0 branches to B, falls to A; A -> B -> A.
        let code = [
            sopp(0x04, 2),  // 0: s_cbranch_scc0 -> 12 (B)
            0x7E00_0200,    // 4: A: v_mov_b32 v0, s0
            sopp(0x05, 0),  // 8: s_cbranch_scc1 -> 12 (B), else 12 anyway
            0x7E00_0200,    // 12: B
            sopp(0x04, -4), // 16: s_cbranch_scc0 -> 4 (A)
            S_ENDPGM,
        ];
        let mut f = Findings::default();
        let r = analyse(&program(&code), &mut f);
        assert!(!r.reducible);
        assert!(!r.structured());
    }

    #[test]
    fn wave_class_and_wave_ops_in_divergent_regions() {
        let mut code = vec![
            0xBE80_3C6A,              // s_and_saveexec_b32 s0, vcc_lo: a region opens
            0x7E02_0500,              // v_readfirstlane_b32 s1, v0: inside it
            0x887E_007E,              // s_or_b32 exec_lo, exec_lo, s0: the join
            0xD8D4_8000, 0x0100_0000, // ds_swizzle_b32 v1, v0 offset:0x8000 (quad mode)
            S_ENDPGM,
        ];
        let mut f = Findings::default();
        let r = analyse(&program(&code), &mut f);
        assert_eq!(r.exact, r.instructions);
        assert_eq!((r.wave_ops, r.readfirstlane, r.wide, r.wave_ops_divergent), (2, 1, 0, 1));
        assert_eq!(r.class(), WaveClass::ReadFirstLane);
        // v_mbcnt_lo_u32_b32 v0, -1, 0: the lane id, seen whole.
        code.splice(0..0, [0xD765_0000, 0x0001_00C1]);
        let r = analyse(&program(&code), &mut f);
        assert_eq!((r.class(), r.wide_reason), (WaveClass::WaveWide, Some("v_mbcnt_lo_u32_b32")));
        assert!(f.mismatches.is_empty());
    }

    #[test]
    fn sixteen_bit_and_lds_counted() {
        let code = [
            0xD834_0000, 0x0000_0100, // ds_write_b32 v0, v1
            0x6400_0000,              // VOP2 op 0x32: v_add_f16
            S_ENDPGM,
        ];
        let mut f = Findings::default();
        let r = analyse(&program(&code), &mut f);
        assert_eq!((r.lds, r.b16, r.exact), (1, 1, 3));
    }
}
