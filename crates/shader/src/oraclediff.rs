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
fn signature(part: &[u8]) -> Result<Vec<SigElem>, String> {
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
                0 => s.sig_in = sig,
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

/// Bindings compared with two named reasons for differing (and nothing else):
/// - our descriptor shadow table (space 101) has no counterpart in the oracle;
/// - our immediate samplers sit in space 100, the oracle's at its own registers: they match when the oracle has
///   as many sampler bindings (`res_type` 1) as we have immediate ones.
pub fn bindings_match(ours: &[Binding], oracle: &[Binding]) -> bool {
    let span = |b: &Binding| u64::from(b.upper.saturating_sub(b.lower)) + 1;
    let immediate: u64 = ours.iter().filter(|b| b.space == OURS_IMMEDIATE_SAMPLER_SPACE).map(span).sum();
    let mut a: Vec<Binding> = ours.iter().filter(|b| b.space != OURS_IMMEDIATE_SAMPLER_SPACE && b.space != OURS_SHADOW_SPACE).cloned().collect();
    let mut b: Vec<Binding> = oracle.to_vec();
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

pub fn compare(ours: &Summary, oracle: &Summary) -> Vec<Diff> {
    let mut d = Vec::new();
    if ours.stage != oracle.stage {
        d.push(Diff::Stage);
    }
    if ours.sig_in != oracle.sig_in {
        d.push(Diff::SigIn);
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
