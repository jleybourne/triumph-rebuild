//! S6: what a translated shader must share with the oracle's translation of the
//! same program, read from the two DXIL containers. No GPU: this compares
//! structure, the lead's execution check compares behaviour.
//!
//! ```text
//! signatures   ISG1 / OSG1 / PSG1 elements: semantic name and index, system value,
//!              component type, register, mask, stream, min precision
//! bindings     PSV0 resources: type, space, lower and upper bound, kind, flags
//! threads      PSV0 thread-group size
//! wave size    PSV0 minimum and maximum wave lane count (what [WaveSize] sets)
//! histogram    per category, from the bitcode: memory, wave, control flow,
//!              barrier, I/O (dx.op calls by opcode; LLVM load/store/atomics count
//!              as memory, br/switch/ret/unreachable as control flow)
//! ```
//!
//! Signature elements and bindings are compared as sorted lists: their order in the
//! container is the compiler's choice, their content is the contract.

use crate::bitcode;
use crate::dxbc::{Container, FourCC};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SigElem {
    pub register: u32,
    pub mask: u8,
    pub name: String,
    pub index: u32,
    pub system_value: u32,
    pub comp_type: u32,
    pub stream: u32,
    pub min_precision: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binding {
    pub space: u32,
    pub res_type: u32,
    pub lower: u32,
    pub upper: u32,
    pub kind: u32,
    pub flags: u32,
}

impl SigElem {
    /// One line for reading a diff: `r3 TEXCOORD1 xyz sv0 f32`.
    pub fn short(&self) -> String {
        let reg = if self.register == u32::MAX { "r-".to_owned() } else { format!("r{}", self.register) };
        let mask: String = "xyzw".chars().enumerate().filter(|(k, _)| self.mask & (1 << k) != 0).map(|(_, c)| c).collect();
        let ty = match self.comp_type {
            1 => "u32",
            2 => "i32",
            3 => "f32",
            4 => "u16",
            5 => "i16",
            6 => "f16",
            t => return format!("{reg} {}{} {mask} sv{} type{t}", self.name, self.index, self.system_value),
        };
        let mp = if self.min_precision != 0 { format!(" mp{}", self.min_precision) } else { String::new() };
        let st = if self.stream != 0 { format!(" stream{}", self.stream) } else { String::new() };
        format!("{reg} {}{} {mask} sv{} {ty}{mp}{st}", self.name, self.index, self.system_value)
    }
}

impl Binding {
    /// One line for reading a diff: `space0 type2 [0..0] kind13 flags0`.
    pub fn short(&self) -> String {
        format!("space{} type{} [{}..{}] kind{} flags{}", self.space, self.res_type, self.lower, self.upper, self.kind, self.flags)
    }
}

/// Two sorted lists merged for reading: `=` in both, `<` only in `ours`, `>` only in `oracle`.
pub fn list_diff(ours: &[String], oracle: &[String]) -> Vec<(char, String)> {
    let mut b: Vec<Option<&String>> = oracle.iter().map(Some).collect();
    let mut out = Vec::new();
    for x in ours {
        match b.iter().position(|y| *y == Some(x)) {
            Some(k) => {
                b[k] = None;
                out.push(('=', x.clone()));
            }
            None => out.push(('<', x.clone())),
        }
    }
    out.extend(b.into_iter().flatten().map(|y| ('>', y.clone())));
    out
}

/// Instruction counts by category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Histogram {
    pub memory: u32,
    pub wave: u32,
    pub control: u32,
    pub barrier: u32,
    pub io: u32,
}

pub const CATEGORIES: [&str; 5] = ["memory", "wave", "control", "barrier", "io"];

impl Histogram {
    pub fn get(&self, i: usize) -> u32 {
        [self.memory, self.wave, self.control, self.barrier, self.io][i]
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub sig_in: Vec<SigElem>,
    pub sig_out: Vec<SigElem>,
    pub sig_patch: Vec<SigElem>,
    /// Input elements the program never reads (name, index): their read mask in `ISG1` is 0.
    pub unread_in: Vec<(String, u32)>,
    /// Every input element's read mask (name, index, components read).
    pub in_reads: Vec<(String, u32, u8)>,
    pub bindings: Vec<Binding>,
    pub stage: u8,
    pub threads: [u32; 3],
    pub wave: (u32, u32),
    pub hist: Histogram,
    /// The bitcode's instructions, for the mutation test.
    pub insts: Vec<bitcode::Inst>,
}

/// dx.op opcode categories (DXIL operation table).
pub fn category(op: u32) -> Option<usize> {
    Some(match op {
        // temp registers, cbuffer, sample, texture and buffer load/store, gather,
        // sample positions, atomics, LOD, raw buffers
        0..=3 | 58..=79 | 81 | 139 | 140 | 223..=225 => 0,
        // wave and quad operations
        110..=123 | 135 | 136 | 165..=167 | 222 => 1,
        // discard
        82 => 2,
        80 => 3,
        // inputs (with eval and attribute-at-vertex), coverage, outputs, thread ids,
        // streams, patch constants, mesh outputs
        4 | 5 | 87..=100 | 103..=106 | 137 | 168..=172 => 4,
        _ => return None,
    })
}

fn llvm_category(code: u32) -> Option<usize> {
    match code {
        // load, store, atomicrmw, cmpxchg (groupshared and local memory)
        20 | 24 | 37 | 38 | 41 | 42 | 44 | 45 | 46 => Some(0),
        bitcode::INST_BR | bitcode::INST_SWITCH | bitcode::INST_RET | bitcode::INST_UNREACHABLE | 31 => Some(2),
        _ => None,
    }
}

pub fn histogram(insts: &[bitcode::Inst]) -> Histogram {
    let mut h = Histogram::default();
    for i in insts {
        let c = match i.dx_op {
            Some(op) => category(op),
            None => llvm_category(i.code),
        };
        match c {
            Some(0) => h.memory += 1,
            Some(1) => h.wave += 1,
            Some(2) => h.control += 1,
            Some(3) => h.barrier += 1,
            Some(4) => h.io += 1,
            _ => {}
        }
    }
    h
}

fn u32_at(d: &[u8], at: usize) -> Result<u32, String> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| "truncated".into())
}

/// A signature part: u32 count, u32 8, then 32-byte elements; names are offsets into the part.
pub fn signature(part: &[u8]) -> Result<Vec<SigElem>, String> {
    let n = u32_at(part, 0)? as usize;
    let first = u32_at(part, 4)? as usize;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let e = first + 32 * k;
        let name_at = u32_at(part, e + 4)? as usize;
        let name = part
            .get(name_at..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .ok_or("signature name out of bounds")?;
        out.push(SigElem {
            stream: u32_at(part, e)?,
            name,
            index: u32_at(part, e + 8)?,
            system_value: u32_at(part, e + 12)?,
            comp_type: u32_at(part, e + 16)?,
            register: u32_at(part, e + 20)?,
            mask: *part.get(e + 24).ok_or("truncated")?,
            min_precision: u32_at(part, e + 28)?,
        });
    }
    out.sort();
    Ok(out)
}

/// Each element's read/write mask (the byte after the mask: for an input, the components the program reads;
/// DXC fills it), as (name, index, mask).
fn reads(part: &[u8]) -> Result<Vec<(String, u32, u8)>, String> {
    let n = u32_at(part, 0)? as usize;
    let first = u32_at(part, 4)? as usize;
    let mut out = Vec::new();
    for k in 0..n {
        let e = first + 32 * k;
        let name_at = u32_at(part, e + 4)? as usize;
        let name = part.get(name_at..).and_then(|s| s.split(|&b| b == 0).next()).ok_or("signature name out of bounds")?;
        out.push((String::from_utf8_lossy(name).into_owned(), u32_at(part, e + 8)?, *part.get(e + 25).ok_or("truncated")?));
    }
    out.sort();
    Ok(out)
}

/// The elements of a signature part the program never reads, as (name, index).
fn unread(part: &[u8]) -> Result<Vec<(String, u32)>, String> {
    Ok(reads(part)?.into_iter().filter(|r| r.2 == 0).map(|r| (r.0, r.1)).collect())
}

/// Pixel input components the oracle reads and ours does not, as `TEXCOORD4.y` (SV_Barycentrics aside, see
/// [`inputs_match`]). Not an interface difference (the signatures can be equal): a program that skips an
/// interpolated component the oracle uses computes from something else. S7 found 62 v0 programs this way.
pub fn reads_missing(ours: &Summary, oracle: &Summary) -> Vec<String> {
    let mut out = Vec::new();
    for (name, index, read) in &oracle.in_reads {
        if name == "SV_Barycentrics" {
            continue;
        }
        let mine = ours.in_reads.iter().find(|r| r.0 == *name && r.1 == *index).map_or(0, |r| r.2);
        let lost = read & !mine;
        if lost != 0 {
            let comps: String = "xyzw".chars().enumerate().filter(|(k, _)| lost >> k & 1 == 1).map(|(_, c)| c).collect();
            out.push(format!("{name}{index}.{comps}"));
        }
    }
    out
}

/// PSV0: runtime info (size-prefixed), then the resource bindings.
fn psv(part: &[u8], s: &mut Summary) -> Result<(), String> {
    let size = u32_at(part, 0)? as usize;
    let info = part.get(4..4 + size).ok_or("PSV0 runtime info out of bounds")?;
    if size >= 24 {
        s.wave = (u32_at(info, 16)?, u32_at(info, 20)?);
    }
    if size >= 36 {
        s.stage = info[24];
    }
    if size >= 48 {
        s.threads = [u32_at(info, 36)?, u32_at(info, 40)?, u32_at(info, 44)?];
    }
    let mut at = 4 + size;
    let count = u32_at(part, at)? as usize;
    at += 4;
    if count > 0 {
        let stride = u32_at(part, at)? as usize;
        at += 4;
        for k in 0..count {
            let r = at + stride * k;
            s.bindings.push(Binding {
                res_type: u32_at(part, r)?,
                space: u32_at(part, r + 4)?,
                lower: u32_at(part, r + 8)?,
                upper: u32_at(part, r + 12)?,
                kind: if stride >= 24 { u32_at(part, r + 16)? } else { 0 },
                flags: if stride >= 24 { u32_at(part, r + 20)? } else { 0 },
            });
        }
    }
    s.bindings.sort();
    Ok(())
}

/// The DXIL part: program header, then the bitcode at its offset.
fn program(part: &[u8]) -> Result<Vec<bitcode::Inst>, String> {
    if part.get(8..12) != Some(b"DXIL") {
        return Err("DXIL part without its program header".into());
    }
    let offset = u32_at(part, 16)? as usize;
    let size = u32_at(part, 20)? as usize;
    let code = part.get(8 + offset..8 + offset + size).ok_or("bitcode out of bounds")?;
    let top = bitcode::parse(code)?;
    Ok(bitcode::functions(&top)?.into_iter().flatten().collect())
}

pub fn summarise(container: &[u8]) -> Result<Summary, String> {
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let mut s = Summary::default();
    for (fourcc, list) in [(FourCC::ISG1, 0), (FourCC::OSG1, 1), (FourCC::PSG1, 2)] {
        if let Some(p) = c.part(fourcc) {
            let sig = signature(p.data)?;
            match list {
                0 => {
                    s.sig_in = sig;
                    s.unread_in = unread(p.data)?;
                    s.in_reads = reads(p.data)?;
                }
                1 => s.sig_out = sig,
                _ => s.sig_patch = sig,
            }
        }
    }
    if let Some(p) = c.part(FourCC::PSV0) {
        psv(p.data, &mut s)?;
    }
    let dxil = c.part(FourCC::DXIL).ok_or("no DXIL part")?;
    s.insts = program(dxil.data)?;
    s.hist = histogram(&s.insts);
    Ok(s)
}

/// One difference between ours and the oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff {
    SigIn,
    SigOut,
    SigPatch,
    /// Pixel input components the oracle reads and ours does not ([`reads_missing`]); not interface.
    InputReads,
    Bindings,
    Threads,
    WaveSize,
    Stage,
    /// A histogram category: (name, ours, oracle).
    Count(&'static str, u32, u32),
}

impl Diff {
    pub fn name(&self) -> String {
        match self {
            Diff::Count(c, a, b) => format!("{c} {a}/{b}"),
            d => format!("{d:?}"),
        }
    }
}

/// The register space of our immediate samplers (static samplers decoded from the console words; the lead's
/// `sir::lift::IMMEDIATE_SAMPLER_SPACE`).
pub const OURS_IMMEDIATE_SAMPLER_SPACE: u32 = 100;
/// The register space of our descriptor shadow table (`sir::lift::SHADOW_SPACE`, plan decision 9).
pub const OURS_SHADOW_SPACE: u32 = 101;

/// The fork's descriptor-kind table: a raw buffer at `t255, space1`, one word per heap descriptor, which its
/// translation reads to choose between a raw-buffer and a constant-buffer load when a scalar buffer load goes
/// through a descriptor whose index is read at run time (S7: `df939974040ca239`, the only v0 program with it).
pub fn is_oracle_kind_table(b: &Binding) -> bool {
    (b.space, b.res_type, b.lower, b.upper, b.kind) == (1, 4, 255, 255, 11)
}

/// Bindings compared with three named reasons for differing (and nothing else):
/// - our descriptor shadow table (space 101) has no counterpart in the oracle;
/// - our immediate samplers sit in space 100, the oracle's at its own registers: they match when the oracle has
///   as many sampler bindings (`res_type` 1) as we have immediate ones;
/// - the oracle's descriptor-kind table (`t255, space1`, [`is_oracle_kind_table`]) has no counterpart in ours.
///   Like our shadow table it is the translator's own helper, bound by the host layer, never by the title. The
///   oracle declares it in 6,915 of its 25,694 programs.
pub fn bindings_match(ours: &[Binding], oracle: &[Binding]) -> bool {
    let span = |b: &Binding| u64::from(b.upper.saturating_sub(b.lower)) + 1;
    let immediate: u64 = ours.iter().filter(|b| b.space == OURS_IMMEDIATE_SAMPLER_SPACE).map(span).sum();
    // the kind table is left out on both sides: ours never declares it, and the oracle against itself (the
    // self-check) must still compare equal
    let mut a: Vec<Binding> = ours
        .iter()
        .filter(|b| b.space != OURS_IMMEDIATE_SAMPLER_SPACE && b.space != OURS_SHADOW_SPACE && !is_oracle_kind_table(b))
        .cloned()
        .collect();
    let mut b: Vec<Binding> = oracle.iter().filter(|b| !is_oracle_kind_table(b)).cloned().collect();
    if immediate > 0 {
        let oracle_samplers: u64 = b.iter().filter(|x| x.res_type == 1).map(span).sum();
        let our_heap_samplers: u64 = a.iter().filter(|x| x.res_type == 1).map(span).sum();
        if oracle_samplers != immediate + our_heap_samplers {
            return false;
        }
        a.retain(|x| x.res_type != 1);
        b.retain(|x| x.res_type != 1);
    }
    let key = |x: &Binding| (x.space, x.res_type, x.lower, x.upper, x.kind, x.flags);
    a.sort_by_key(key);
    b.sort_by_key(key);
    a == b
}

/// PSV0 stage numbers.
pub const STAGE_PIXEL: u8 = 0;
pub const STAGE_VERTEX: u8 = 1;
/// `SV_Barycentrics`.
pub const SV_BARYCENTRICS: u32 = 23;

/// Input signatures compared with three named reasons for differing (and nothing else):
/// - the order (and so the registers) of the system-generated vertex and instance ids (system values 6 and 8).
///   The GPU generates them and binds them by semantic, so their order changes neither the input layout nor
///   stage linking; the fork adds them in the order its lowering first meets them.
/// - `SV_Barycentrics`. The fork seeds the console's barycentric VGPRs from it, so the untouched high half of a
///   16-bit interpolation's destination keeps the console's bits; in all 12 v0 programs that declare it those
///   bits are dropped (only `legacyF16ToF32` of the low half reads the register). The rasteriser generates it
///   and it has no register (-1): it neither links with the vertex stage nor takes a slot.
/// - vertex inputs the oracle declares but never reads (read mask 0), absent from ours. The fork mirrors every
///   element of the container; ours declares what the fetch loads. The input assembler matches elements by
///   semantic, so a vertex shader may declare fewer than the layout provides, and vertex input registers are
///   not part of the interface: for the vertex stage they are not compared.
///
/// Every other element must match exactly; pixel inputs with their registers (they link by register).
pub fn inputs_match(ours: &[SigElem], oracle: &[SigElem], oracle_unread: &[(String, u32)], stage: u8) -> bool {
    let vertex = stage == STAGE_VERTEX;
    let has = |v: &[SigElem], e: &SigElem| v.iter().any(|x| x.name == e.name && x.index == e.index);
    let kept = |v: &[SigElem], other: &[SigElem], unread: &[(String, u32)]| -> Vec<SigElem> {
        v.iter()
            .filter(|e| e.system_value != SV_BARYCENTRICS)
            .filter(|e| !(vertex && unread.contains(&(e.name.clone(), e.index)) && !has(other, e)))
            .cloned()
            .collect()
    };
    let (ours, oracle) = (kept(ours, oracle, &[]), kept(oracle, ours, oracle_unread));
    let generated = |e: &&SigElem| matches!(e.system_value, 6 | 8);
    let rest = |v: &[SigElem]| -> Vec<SigElem> {
        let mut x: Vec<SigElem> =
            v.iter().filter(|e| !generated(e)).map(|e| SigElem { register: if vertex { 0 } else { e.register }, ..e.clone() }).collect();
        x.sort();
        x
    };
    let ids = |v: &[SigElem]| -> Vec<(String, u32, u8)> {
        let mut x: Vec<_> = v.iter().filter(generated).map(|e| (e.name.clone(), e.index, e.mask)).collect();
        x.sort();
        x
    };
    rest(&ours) == rest(&oracle) && ids(&ours) == ids(&oracle)
}

pub fn compare(ours: &Summary, oracle: &Summary) -> Vec<Diff> {
    let mut d = Vec::new();
    if ours.stage != oracle.stage {
        d.push(Diff::Stage);
    }
    if !inputs_match(&ours.sig_in, &oracle.sig_in, &oracle.unread_in, oracle.stage) {
        d.push(Diff::SigIn);
    }
    if oracle.stage == STAGE_PIXEL && !reads_missing(ours, oracle).is_empty() {
        d.push(Diff::InputReads);
    }
    if ours.sig_out != oracle.sig_out {
        d.push(Diff::SigOut);
    }
    if ours.sig_patch != oracle.sig_patch {
        d.push(Diff::SigPatch);
    }
    if !bindings_match(&ours.bindings, &oracle.bindings) {
        d.push(Diff::Bindings);
    }
    if ours.threads != oracle.threads {
        d.push(Diff::Threads);
    }
    if ours.wave != oracle.wave {
        d.push(Diff::WaveSize);
    }
    for (i, name) in CATEGORIES.iter().enumerate() {
        let (a, b) = (ours.hist.get(i), oracle.hist.get(i));
        if a != b {
            d.push(Diff::Count(name, a, b));
        }
    }
    d
}

/// Reader debugging: the module's named values (id, name) and every call's callee id.
pub fn raw_calls(container: &[u8]) -> Result<(Vec<(u32, String)>, Vec<u32>), String> {
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let part = c.part(FourCC::DXIL).ok_or("no DXIL part")?.data;
    let offset = u32_at(part, 16)? as usize;
    let size = u32_at(part, 20)? as usize;
    let top = bitcode::parse(part.get(8 + offset..8 + offset + size).ok_or("bitcode out of bounds")?)?;
    let mut names: Vec<(u32, String)> = bitcode::names(&top).into_iter().collect();
    names.sort();
    let callees = bitcode::functions(&top)?.into_iter().flatten().filter_map(|i| i.callee).collect();
    Ok((names, callees))
}

/// For a check of the bitcode reader: every call to a `dx.op.*` function as
/// (function name, resolved opcode or None).
pub fn dx_op_calls(container: &[u8]) -> Result<Vec<(String, Option<u32>)>, String> {
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let part = c.part(FourCC::DXIL).ok_or("no DXIL part")?.data;
    let offset = u32_at(part, 16)? as usize;
    let size = u32_at(part, 20)? as usize;
    let top = bitcode::parse(part.get(8 + offset..8 + offset + size).ok_or("bitcode out of bounds")?)?;
    let names = bitcode::names(&top);
    Ok(bitcode::functions(&top)?
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let n = names.get(&i.callee?)?;
            n.starts_with("dx.op.").then(|| (n.clone(), i.dx_op))
        })
        .collect())
}

/// How well a translation's signatures agree with the container it should come from: the elements
/// (name, index, mask) the two have in common, inputs and outputs. The oracle's pairing (S5) uses it to settle
/// cache entries created in the same clock tick, whose order the creation time cannot tell.
pub fn sig_agreement(container_in: &[SigElem], container_out: &[SigElem], s: &Summary) -> usize {
    let common = |a: &[SigElem], b: &[SigElem]| {
        let mut b: Vec<(&str, u32, u8)> = b.iter().map(|e| (e.name.as_str(), e.index, e.mask)).collect();
        a.iter()
            .filter(|e| {
                let k = b.iter().position(|x| *x == (e.name.as_str(), e.index, e.mask));
                k.map(|k| b.swap_remove(k)).is_some()
            })
            .count()
    };
    common(container_in, &s.sig_in) + common(container_out, &s.sig_out)
}

/// `scores[i][j]`: agreement of container i with entry j, for one group of ties. The assignment (entry for
/// each container) with the highest total, if exactly one assignment reaches it; groups are small (two
/// entries in the corpus), so every permutation is tried.
pub fn best_assignment(scores: &[Vec<usize>]) -> Option<Vec<usize>> {
    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![vec![]];
        }
        let mut out = Vec::new();
        for p in permutations(n - 1) {
            for k in 0..=p.len() {
                let mut q = p.clone();
                q.insert(k, n - 1);
                out.push(q);
            }
        }
        out
    }
    let n = scores.len();
    if n > 6 || scores.iter().any(|r| r.len() != n) {
        return None;
    }
    let total = |p: &Vec<usize>| p.iter().enumerate().map(|(i, &j)| scores[i][j]).sum::<usize>();
    let all = permutations(n);
    let best = all.iter().map(total).max()?;
    let mut top = all.into_iter().filter(|p| total(p) == best);
    let first = top.next()?;
    top.next().is_none().then_some(first)
}

/// Byte-level mutations of a container, for the closing test.
pub mod mutate {
    use crate::dxbc::{Container, FourCC};

    fn part_range(c: &[u8], fourcc: FourCC) -> Option<(usize, usize)> {
        let parsed = Container::parse(c).ok()?;
        let p = parsed.part(fourcc)?;
        Some((p.offset + 8, p.data.len()))
    }

    /// Moves the first PSV0 binding up by one register (lower and upper bound).
    pub fn move_binding(c: &[u8]) -> Option<Vec<u8>> {
        let (at, _) = part_range(c, FourCC::PSV0)?;
        let mut out = c.to_vec();
        let word = |o: &[u8], k: usize| u32::from_le_bytes(o[k..k + 4].try_into().unwrap());
        let size = word(&out, at) as usize;
        let count_at = at + 4 + size;
        if word(&out, count_at) == 0 {
            return None;
        }
        let rec = count_at + 8;
        for k in [rec + 8, rec + 12] {
            let v = word(&out, k).wrapping_add(1);
            out[k..k + 4].copy_from_slice(&v.to_le_bytes());
        }
        Some(out)
    }

    /// Drops the last element of the output signature (count - 1; the bytes stay).
    pub fn drop_output(c: &[u8]) -> Option<Vec<u8>> {
        let (at, _) = part_range(c, FourCC::OSG1)?;
        let mut out = c.to_vec();
        let n = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        if n == 0 {
            return None;
        }
        out[at..at + 4].copy_from_slice(&(n - 1).to_le_bytes());
        Some(out)
    }
}

/// Removes the first wave operation from a summary's instructions (the bitcode has
/// no fixed-size slot to blank, so this one is applied after decoding) and recounts.
pub fn drop_wave_op(s: &Summary) -> Option<Summary> {
    let k = s.insts.iter().position(|i| i.dx_op.and_then(category) == Some(1))?;
    let mut m = s.clone();
    m.insts.remove(k);
    m.hist = histogram(&m.insts);
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig_part(elems: &[(&str, u32, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((elems.len() as u32).to_le_bytes());
        out.extend(8u32.to_le_bytes());
        let names_at = 8 + 32 * elems.len();
        let mut names = Vec::new();
        for &(name, reg, mask) in elems {
            let off = (names_at + names.len()) as u32;
            names.extend(name.bytes());
            names.push(0);
            for v in [0, off, 0, 0, 3, reg] {
                out.extend((v as u32).to_le_bytes());
            }
            out.extend([mask, 0, 0, 0]);
            out.extend(0u32.to_le_bytes());
        }
        out.extend(names);
        out
    }

    fn psv_part(bindings: &[(u32, u32, u32)], threads: [u32; 3], wave: (u32, u32)) -> Vec<u8> {
        let mut info = vec![0u8; 48];
        info[16..20].copy_from_slice(&wave.0.to_le_bytes());
        info[20..24].copy_from_slice(&wave.1.to_le_bytes());
        info[24] = 5;
        for (k, t) in threads.iter().enumerate() {
            info[36 + 4 * k..40 + 4 * k].copy_from_slice(&t.to_le_bytes());
        }
        let mut out = 48u32.to_le_bytes().to_vec();
        out.extend(info);
        out.extend((bindings.len() as u32).to_le_bytes());
        if !bindings.is_empty() {
            out.extend(24u32.to_le_bytes());
            for &(ty, space, lower) in bindings {
                for v in [ty, space, lower, lower, 1, 0] {
                    out.extend(v.to_le_bytes());
                }
            }
        }
        out
    }

    fn container() -> Vec<u8> {
        let isg = sig_part(&[("TEXCOORD", 0, 3), ("SV_Position", 1, 15)]);
        let osg = sig_part(&[("SV_Target", 0, 15), ("SV_Target", 1, 15)]);
        let psv = psv_part(&[(2, 0, 3), (1, 1, 0)], [64, 1, 1], (32, 32));
        crate::dxbc::build(&[(FourCC::ISG1, &isg), (FourCC::OSG1, &osg), (FourCC::PSV0, &psv)])
    }

    /// Signatures and bindings without the DXIL part (the histogram has its own test).
    fn summarise_parts(c: &[u8]) -> Summary {
        let parsed = Container::parse(c).unwrap();
        let mut s = Summary {
            sig_in: signature(parsed.part(FourCC::ISG1).unwrap().data).unwrap(),
            sig_out: signature(parsed.part(FourCC::OSG1).unwrap().data).unwrap(),
            ..Summary::default()
        };
        psv(parsed.part(FourCC::PSV0).unwrap().data, &mut s).unwrap();
        s
    }

    #[test]
    fn reads_signatures_bindings_threads_and_wave_size() {
        let s = summarise_parts(&container());
        assert_eq!(s.sig_in.len(), 2);
        assert_eq!((s.sig_in[0].name.as_str(), s.sig_in[0].mask), ("TEXCOORD", 3));
        assert_eq!(s.sig_out[1].index, 0);
        assert_eq!(s.bindings.len(), 2);
        assert_eq!((s.bindings[0].space, s.bindings[1].lower), (0, 0));
        assert_eq!((s.threads, s.wave, s.stage), ([64, 1, 1], (32, 32), 5));
        assert!(compare(&s, &s).is_empty());
    }

    fn el(reg: u32, name: &str, index: u32, mask: u8, sv: u32) -> SigElem {
        SigElem { register: reg, mask, name: name.into(), index, system_value: sv, comp_type: 3, stream: 0, min_precision: 0 }
    }

    #[test]
    fn named_input_reasons_and_only_those() {
        let unread = vec![("COLOR".to_owned(), 0)];
        // vertex: the oracle's unread COLOR0 is absent from ours, TEXCOORD0 moves up a register: the same interface
        let oracle = [el(0, "POSITION", 0, 7, 0), el(1, "COLOR", 0, 15, 0), el(2, "TEXCOORD", 0, 3, 0), el(3, "SV_VertexID", 0, 1, 6)];
        let ours = [el(0, "POSITION", 0, 7, 0), el(1, "TEXCOORD", 0, 3, 0), el(2, "SV_VertexID", 0, 1, 6)];
        assert!(inputs_match(&ours, &oracle, &unread, STAGE_VERTEX));
        // ... but not if the oracle reads it, nor for a pixel shader (registers link there)
        assert!(!inputs_match(&ours, &oracle, &[], STAGE_VERTEX));
        assert!(!inputs_match(&ours, &oracle, &unread, STAGE_PIXEL));
        // an element ours declares and the oracle lacks is never excused
        let extra = [el(0, "POSITION", 0, 7, 0), el(1, "COLOR", 0, 15, 0), el(2, "TEXCOORD", 0, 3, 0), el(3, "NORMAL", 0, 7, 0), el(4, "SV_VertexID", 0, 1, 6)];
        assert!(!inputs_match(&extra, &oracle, &unread, STAGE_VERTEX));
        // a vertex element with another mask still differs
        let masked = [el(0, "POSITION", 0, 15, 0), el(1, "TEXCOORD", 0, 3, 0), el(2, "SV_VertexID", 0, 1, 6)];
        assert!(!inputs_match(&masked, &oracle, &unread, STAGE_VERTEX));
        // pixel: SV_Barycentrics (no register) on one side only is not a difference; anything else is
        let ps = [el(0, "SV_Position", 0, 15, 1), el(1, "TEXCOORD", 0, 3, 0)];
        let mut with_bary = ps.to_vec();
        with_bary.push(el(u32::MAX, "SV_Barycentrics", 0, 7, SV_BARYCENTRICS));
        assert!(inputs_match(&ps, &with_bary, &[], STAGE_PIXEL));
        let moved = [el(0, "SV_Position", 0, 15, 1), el(2, "TEXCOORD", 0, 3, 0)];
        assert!(!inputs_match(&moved, &with_bary, &[], STAGE_PIXEL));
    }

    #[test]
    fn the_oracle_kind_table_is_named_and_nothing_near_it() {
        let b = |space, res_type, lower, kind| Binding { space, res_type, lower, upper: lower, kind, flags: 0 };
        let cb = b(0, 2, 0, 13);
        assert!(bindings_match(&[cb], &[cb, b(1, 4, 255, 11)]));
        assert!(!bindings_match(&[cb], &[cb, b(1, 4, 3, 11)])); // a vertex buffer slot is real
        assert!(!bindings_match(&[cb], &[cb, b(0, 4, 255, 11)]));
        assert!(bindings_match(&[cb, b(1, 4, 255, 11)], &[cb, b(1, 4, 255, 11)])); // the self-check
    }

    #[test]
    fn reads_the_unread_inputs() {
        let mut isg = sig_part(&[("TEXCOORD", 0, 3), ("COLOR", 1, 15)]);
        isg[8 + 25] = 3; // TEXCOORD0 read in x and y; COLOR stays 0
        assert_eq!(unread(&isg).unwrap(), [("COLOR".to_owned(), 0)]);
        // a pixel component the oracle reads and ours does not
        let oracle = Summary { in_reads: reads(&isg).unwrap(), ..Summary::default() };
        isg[8 + 25] = 1;
        let ours = Summary { in_reads: reads(&isg).unwrap(), ..Summary::default() };
        assert_eq!(reads_missing(&ours, &oracle), ["TEXCOORD0.y"]);
        assert!(reads_missing(&oracle, &ours).is_empty());
        assert_eq!(compare(&ours, &oracle), [Diff::InputReads]);
        assert!(compare(&oracle, &oracle).is_empty());
    }

    #[test]
    fn ties_are_settled_by_signatures() {
        let c = container();
        let parsed = Container::parse(&c).unwrap();
        let cin = signature(parsed.part(FourCC::ISG1).unwrap().data).unwrap();
        let cout = signature(parsed.part(FourCC::OSG1).unwrap().data).unwrap();
        let right = summarise_parts(&container());
        let mut wrong = right.clone();
        wrong.sig_out[1].mask = 3; // one output element with another mask: another program
        assert_eq!(sig_agreement(&cin, &cout, &right), 4);
        assert_eq!(sig_agreement(&cin, &cout, &wrong), 3);
        // two containers, two entries created in the same tick, filed the wrong way round
        assert_eq!(best_assignment(&[vec![3, 4], vec![4, 3]]), Some(vec![1, 0]));
        assert_eq!(best_assignment(&[vec![4, 3], vec![3, 4]]), Some(vec![0, 1]));
        // signatures that cannot tell them apart: no answer, never a guess
        assert_eq!(best_assignment(&[vec![4, 4], vec![4, 4]]), None);
    }

    #[test]
    fn list_diff_marks_each_side() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let d = list_diff(&s(&["a", "b", "b"]), &s(&["b", "c"]));
        assert_eq!(d, [('<', "a".into()), ('=', "b".into()), ('<', "b".into()), ('>', "c".into())]);
        let e = SigElem { register: u32::MAX, mask: 7, name: "SV_Barycentrics".into(), index: 0, system_value: 23, comp_type: 3, stream: 0, min_precision: 0 };
        assert_eq!(e.short(), "r- SV_Barycentrics0 xyz sv23 f32");
    }

    #[test]
    fn each_mutation_is_caught() {
        let c = container();
        let base = summarise_parts(&c);
        let moved = summarise_parts(&mutate::move_binding(&c).unwrap());
        assert_eq!(compare(&moved, &base), [Diff::Bindings]);
        let dropped = summarise_parts(&mutate::drop_output(&c).unwrap());
        assert_eq!(compare(&dropped, &base), [Diff::SigOut]);
        let mut with_wave = base.clone();
        with_wave.insts = vec![
            bitcode::Inst { code: bitcode::INST_CALL, dx_op: Some(118), callee: None },
            bitcode::Inst { code: bitcode::INST_CALL, dx_op: Some(68), callee: None },
            bitcode::Inst { code: bitcode::INST_RET, dx_op: None, callee: None },
        ];
        with_wave.hist = histogram(&with_wave.insts);
        assert_eq!((with_wave.hist.wave, with_wave.hist.memory, with_wave.hist.control), (1, 1, 1));
        let less = drop_wave_op(&with_wave).unwrap();
        assert_eq!(compare(&less, &with_wave), [Diff::Count("wave", 0, 1)]);
    }
}
