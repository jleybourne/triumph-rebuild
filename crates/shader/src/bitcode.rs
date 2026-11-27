//! A reader for the LLVM 3.7 bitcode inside a DXIL part, enough to list each
//! function's instructions and resolve calls to `dx.op.*` with their opcode.
//!
//! Written from LLVM's bitstream format description ("LLVM Bitcode File Format")
//! and the 3.7 record codes; no LLVM code is used. The bitstream is parsed into a
//! tree of blocks and records (abbreviations and BLOCKINFO applied), then the module
//! is walked for value numbering: globals and functions, module constants, then per
//! function its arguments, its constants and the instructions that produce values.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub code: u32,
    pub ops: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Record(Record),
    Block(Block),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: u32,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
enum Op {
    Literal(u64),
    Fixed(u32),
    Vbr(u32),
    Array,
    Char6,
    Blob,
}

type Abbrev = Vec<Op>;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

type R<T> = Result<T, &'static str>;

impl Bits<'_> {
    fn read(&mut self, n: u32) -> R<u64> {
        if n == 0 {
            return Ok(0);
        }
        if n > 64 || self.pos + n as usize > self.data.len() * 8 {
            return Err("bitstream runs past its end");
        }
        let mut v = 0u64;
        for i in 0..n as usize {
            let p = self.pos + i;
            v |= u64::from(self.data[p / 8] >> (p % 8) & 1) << i;
        }
        self.pos += n as usize;
        Ok(v)
    }

    fn vbr(&mut self, n: u32) -> R<u64> {
        let hi = 1u64 << (n - 1);
        let (mut v, mut shift) = (0u64, 0u32);
        loop {
            let piece = self.read(n)?;
            if shift < 64 {
                v |= (piece & (hi - 1)) << shift;
            }
            if piece & hi == 0 {
                return Ok(v);
            }
            shift += n - 1;
            if shift > 70 {
                return Err("vbr too long");
            }
        }
    }

    fn align32(&mut self) {
        self.pos = self.pos.div_ceil(32) * 32;
    }

    fn at_end(&self) -> bool {
        self.pos + 32 > self.data.len() * 8
    }
}

const BLOCKINFO: u32 = 0;

struct Parser<'a> {
    bits: Bits<'a>,
    blockinfo: HashMap<u32, Vec<Abbrev>>,
}

impl Parser<'_> {
    fn define_abbrev(&mut self) -> R<Abbrev> {
        let n = self.bits.vbr(5)?;
        let mut ops = Vec::new();
        let mut i = 0;
        while i < n {
            if self.bits.read(1)? == 1 {
                ops.push(Op::Literal(self.bits.vbr(8)?));
            } else {
                ops.push(match self.bits.read(3)? {
                    1 => Op::Fixed(self.bits.vbr(5)? as u32),
                    2 => Op::Vbr(self.bits.vbr(5)? as u32),
                    3 => Op::Array,
                    4 => Op::Char6,
                    5 => Op::Blob,
                    _ => return Err("bad abbreviation encoding"),
                });
            }
            i += 1;
        }
        Ok(ops)
    }

    fn scalar(&mut self, op: &Op) -> R<u64> {
        match *op {
            Op::Literal(v) => Ok(v),
            Op::Fixed(n) => self.bits.read(n),
            Op::Vbr(n) => self.bits.vbr(n),
            // LLVM's 6-bit characters: a-z, A-Z, 0-9, '.', '_' (DXC writes symbol names this way)
            Op::Char6 => self.bits.read(6).map(|c| b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._"[c as usize] as u64),
            Op::Array | Op::Blob => Err("array or blob as an element"),
        }
    }

    fn abbreviated(&mut self, a: &Abbrev) -> R<Record> {
        let mut vals = Vec::new();
        let mut i = 0;
        while i < a.len() {
            match &a[i] {
                Op::Array => {
                    let len = self.bits.vbr(6)?;
                    let elt = a.get(i + 1).ok_or("array without an element type")?.clone();
                    for _ in 0..len {
                        vals.push(self.scalar(&elt)?);
                    }
                    i += 2;
                    continue;
                }
                Op::Blob => {
                    let len = self.bits.vbr(6)? as usize;
                    self.bits.align32();
                    for _ in 0..len {
                        vals.push(self.bits.read(8)?);
                    }
                    self.bits.align32();
                }
                op => vals.push(self.scalar(op)?),
            }
            i += 1;
        }
        if vals.is_empty() {
            return Err("abbreviated record without a code");
        }
        let code = vals.remove(0) as u32;
        Ok(Record { code, ops: vals })
    }

    /// Reads a block's body after its ENTER_SUBBLOCK header.
    fn block(&mut self, id: u32, width: u32) -> R<Block> {
        let mut abbrevs = self.blockinfo.get(&id).cloned().unwrap_or_default();
        let mut items = Vec::new();
        let mut info_target: Option<u32> = None;
        loop {
            match self.bits.read(width)? {
                0 => {
                    self.bits.align32();
                    return Ok(Block { id, items });
                }
                1 => {
                    let sub = self.bits.vbr(8)? as u32;
                    let w = self.bits.vbr(4)? as u32;
                    self.bits.align32();
                    let _words = self.bits.read(32)?;
                    let b = self.block(sub, w)?;
                    if sub != BLOCKINFO {
                        items.push(Item::Block(b));
                    }
                }
                2 => {
                    let a = self.define_abbrev()?;
                    if id == BLOCKINFO {
                        let t = info_target.ok_or("BLOCKINFO abbreviation before SETBID")?;
                        self.blockinfo.entry(t).or_default().push(a);
                    } else {
                        abbrevs.push(a);
                    }
                }
                3 => {
                    let code = self.bits.vbr(6)? as u32;
                    let n = self.bits.vbr(6)?;
                    let mut ops = Vec::with_capacity(n.min(1 << 16) as usize);
                    for _ in 0..n {
                        ops.push(self.bits.vbr(6)?);
                    }
                    let r = Record { code, ops };
                    if id == BLOCKINFO && r.code == 1 {
                        info_target = r.ops.first().map(|&v| v as u32);
                    } else if id != BLOCKINFO {
                        items.push(Item::Record(r));
                    }
                }
                n => {
                    let a = abbrevs.get(n as usize - 4).ok_or("undefined abbreviation")?.clone();
                    let r = self.abbreviated(&a)?;
                    if id != BLOCKINFO {
                        items.push(Item::Record(r));
                    }
                }
            }
        }
    }
}

/// Parses a bitcode stream (`BC` 0xC0DE, or with the 0x0B17C0DE wrapper) into its top-level blocks.
pub fn parse(data: &[u8]) -> R<Vec<Block>> {
    let mut data = data;
    if data.get(..4) == Some(&[0xDE, 0xC0, 0x17, 0x0B]) {
        let word = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
        let (off, len) = (word(8).ok_or("wrapper")?, word(12).ok_or("wrapper")?);
        data = data.get(off..off + len).ok_or("wrapper out of bounds")?;
    }
    if data.get(..4) != Some(b"BC\xC0\xDE") {
        return Err("not LLVM bitcode");
    }
    let mut p = Parser { bits: Bits { data, pos: 32 }, blockinfo: HashMap::new() };
    let mut out = Vec::new();
    while !p.bits.at_end() {
        match p.bits.read(2)? {
            1 => {
                let id = p.bits.vbr(8)? as u32;
                let w = p.bits.vbr(4)? as u32;
                p.bits.align32();
                let _ = p.bits.read(32)?;
                let b = p.block(id, w)?;
                if id != BLOCKINFO {
                    out.push(b);
                }
            }
            _ => break,
        }
    }
    Ok(out)
}

/// One instruction of a function body: its LLVM record code and, for a call to a
/// `dx.op.*` function, the DXIL opcode (the call's first argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inst {
    pub code: u32,
    pub dx_op: Option<u32>,
    /// For a call: the callee's value id (to name it, see [`names`]).
    pub callee: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
enum Ty {
    Void,
    Function { ret: usize, params: usize },
    Other,
}

const MODULE: u32 = 8;
const PARAMATTR: u32 = 9;
const CONSTANTS: u32 = 11;
const FUNCTION: u32 = 12;
const VALUE_SYMTAB: u32 = 14;
const TYPES: u32 = 17;

pub const INST_RET: u32 = 10;
pub const INST_BR: u32 = 11;
pub const INST_SWITCH: u32 = 12;
pub const INST_UNREACHABLE: u32 = 15;
pub const INST_CALL: u32 = 34;

/// Record codes of function-body instructions that never produce a value.
fn no_value(code: u32) -> bool {
    matches!(code, 1 | 10 | 11 | 12 | 15 | 24 | 31 | 33 | 35 | 36 | 39 | 42 | 44 | 45)
}

/// Every defined function's instructions, in order.
pub fn functions(top: &[Block]) -> R<Vec<Vec<Inst>>> {
    let module = top.iter().find(|b| b.id == MODULE).ok_or("no module block")?;
    let records = || module.items.iter().filter_map(|i| if let Item::Record(r) = i { Some(r) } else { None });
    let blocks = |id: u32| module.items.iter().filter_map(move |i| match i {
        Item::Block(b) if b.id == id => Some(b),
        _ => None,
    });
    let relative = records().find(|r| r.code == 1).and_then(|r| r.ops.first()).is_some_and(|&v| v >= 1);

    let mut types = Vec::new();
    for b in blocks(TYPES) {
        for i in &b.items {
            if let Item::Record(r) = i {
                match r.code {
                    1 | 19 => {}
                    2 => types.push(Ty::Void),
                    21 => types.push(Ty::Function {
                        ret: *r.ops.get(1).ok_or("function type")? as usize,
                        params: r.ops.len().saturating_sub(2),
                    }),
                    _ => types.push(Ty::Other),
                }
            }
        }
    }
    let _ = PARAMATTR;

    // Module values: globals, functions and aliases in record order, then constants.
    let mut value_type: Vec<Option<usize>> = Vec::new();
    let mut bodies = Vec::new();
    for r in records() {
        match r.code {
            7 | 9 | 14 => value_type.push(None),
            8 => {
                let ty = *r.ops.first().ok_or("function record")? as usize;
                if r.ops.get(2) == Some(&0) {
                    bodies.push((value_type.len(), ty));
                }
                value_type.push(Some(ty));
            }
            _ => {}
        }
    }
    let mut ints: HashMap<usize, i64> = HashMap::new();
    for b in blocks(CONSTANTS) {
        constants(b, &mut value_type, &mut ints);
    }
    let module_values = value_type.len();
    let mut names: HashMap<usize, String> = HashMap::new();
    for b in blocks(VALUE_SYMTAB) {
        for i in &b.items {
            if let Item::Record(r) = i {
                let skip = match r.code {
                    1 => 1,
                    3 => 2,
                    _ => continue,
                };
                if let Some(&id) = r.ops.first() {
                    let name: String = r.ops.iter().skip(skip).map(|&c| c as u8 as char).collect();
                    names.insert(id as usize, name);
                }
            }
        }
    }
    let is_dx_op = |id: usize| names.get(&id).is_some_and(|n| n.starts_with("dx.op."));

    let mut out = Vec::new();
    for (body, (fn_id, fn_ty)) in blocks(FUNCTION).zip(&bodies) {
        let _ = fn_id;
        let params = match types.get(*fn_ty) {
            Some(Ty::Function { params, .. }) => *params,
            _ => return Err("function without a function type"),
        };
        let mut vt = value_type[..module_values].to_vec();
        let mut local_ints = ints.clone();
        vt.extend(std::iter::repeat_n(None, params));
        let mut insts = Vec::new();
        for i in &body.items {
            match i {
                Item::Block(b) if b.id == CONSTANTS => constants(b, &mut vt, &mut local_ints),
                Item::Block(_) => {}
                Item::Record(r) => {
                    let next = vt.len();
                    let abs = |v: u64| -> usize {
                        if relative { next.wrapping_sub(v as usize) } else { v as usize }
                    };
                    let mut dx_op = None;
                    let mut callee_id = None;
                    let mut produces = !no_value(r.code);
                    if r.code == INST_CALL {
                        let cc = *r.ops.get(1).ok_or("call record")?;
                        let mut k = 2;
                        if cc >> 17 & 1 == 1 {
                            k += 1; // fast-math flags
                        }
                        let explicit = if cc >> 15 & 1 == 1 {
                            k += 1;
                            Some(r.ops[k - 1] as usize)
                        } else {
                            None
                        };
                        let callee = abs(*r.ops.get(k).ok_or("call without a callee")?);
                        callee_id = Some(callee as u32);
                        let fty = explicit.or_else(|| vt.get(callee).copied().flatten());
                        produces = match fty.and_then(|t| types.get(t)) {
                            Some(Ty::Function { ret, .. }) => !matches!(types.get(*ret), Some(Ty::Void)),
                            _ => return Err("call of an unknown function type"),
                        };
                        if is_dx_op(callee) {
                            let arg = abs(*r.ops.get(k + 1).ok_or("dx.op call without an opcode")?);
                            dx_op = local_ints.get(&arg).map(|&v| v as u32);
                        }
                    }
                    if r.code == 1 {
                        continue; // DECLAREBLOCKS
                    }
                    if !matches!(r.code, 33 | 35) {
                        insts.push(Inst { code: r.code, dx_op, callee: callee_id });
                    }
                    if produces {
                        vt.push(None);
                    }
                }
            }
        }
        out.push(insts);
    }
    Ok(out)
}

/// Module-level value names (functions, globals), by value id.
pub fn names(top: &[Block]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for m in top.iter().filter(|b| b.id == MODULE) {
        for i in &m.items {
            if let Item::Block(b) = i {
                if b.id != VALUE_SYMTAB {
                    continue;
                }
                for j in &b.items {
                    if let Item::Record(r) = j {
                        let skip = match r.code {
                            1 => 1,
                            3 => 2,
                            _ => continue,
                        };
                        if let Some(&id) = r.ops.first() {
                            out.insert(id as u32, r.ops.iter().skip(skip).map(|&c| c as u8 as char).collect());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Adds a constants block's values (every record but SETTYPE), and its integers.
fn constants(b: &Block, values: &mut Vec<Option<usize>>, ints: &mut HashMap<usize, i64>) {
    for i in &b.items {
        if let Item::Record(r) = i {
            match r.code {
                1 => {}
                4 => {
                    let v = r.ops.first().copied().unwrap_or(0);
                    let s = if v & 1 == 1 { -((v >> 1) as i64) } else { (v >> 1) as i64 };
                    ints.insert(values.len(), s);
                    values.push(None);
                }
                _ => values.push(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit writer for the tests: fixed and VBR fields, LSB first.
    #[derive(Default)]
    struct W {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl W {
        fn put(&mut self, v: u64, n: u32) {
            for i in 0..n {
                if self.bit % 8 == 0 {
                    self.bytes.push(0);
                }
                let b = (v >> i & 1) as u8;
                *self.bytes.last_mut().unwrap() |= b << (self.bit % 8);
                self.bit += 1;
            }
        }
        fn vbr(&mut self, mut v: u64, n: u32) {
            let hi = 1u64 << (n - 1);
            loop {
                if v < hi {
                    self.put(v, n);
                    return;
                }
                self.put(v & (hi - 1) | hi, n);
                v >>= n - 1;
            }
        }
        fn align(&mut self) {
            while self.bit % 32 != 0 {
                self.put(0, 1);
            }
        }
        fn enter(&mut self, id: u32, width: u32, outer: u32) {
            self.put(1, outer);
            self.vbr(id.into(), 8);
            self.vbr(width.into(), 4);
            self.align();
            self.put(0, 32); // length in words: not checked by the reader
        }
        fn end(&mut self, width: u32) {
            self.put(0, width);
            self.align();
        }
        fn record(&mut self, code: u64, ops: &[u64], width: u32) {
            self.put(3, width);
            self.vbr(code, 6);
            self.vbr(ops.len() as u64, 6);
            for &o in ops {
                self.vbr(o, 6);
            }
        }
    }

    /// A module with `void dx.op.waveX(i32)`, `i32 dx.op.lane(i32)` and one function that
    /// calls the second (a value), then the first twice, branches and returns.
    fn module() -> Vec<u8> {
        let mut w = W::default();
        w.put(u64::from_le_bytes([b'B', b'C', 0xC0, 0xDE, 0, 0, 0, 0]), 32);
        let wd = 3;
        w.enter(MODULE, wd, 2);
        w.record(1, &[1], wd); // version 1: relative ids
        w.enter(TYPES, wd, wd);
        w.record(2, &[], wd); // 0 void
        w.record(7, &[32], wd); // 1 i32
        w.record(21, &[0, 0, 1], wd); // 2 void(i32)
        w.record(21, &[0, 1, 1], wd); // 3 i32(i32)
        w.record(21, &[0, 0], wd); // 4 void()
        w.end(wd);
        w.record(8, &[2, 0, 1, 0], wd); // value 0: dx.op.waveX, a declaration
        w.record(8, &[3, 0, 1, 0], wd); // value 1: dx.op.lane
        w.record(8, &[4, 0, 0, 0], wd); // value 2: main, defined
        w.enter(CONSTANTS, wd, wd);
        w.record(1, &[1], wd); // settype i32
        w.record(4, &[118 << 1], wd); // value 3: i32 118
        w.record(4, &[111 << 1], wd); // value 4: i32 111
        w.end(wd);
        w.enter(VALUE_SYMTAB, wd, wd);
        for (id, name) in [(0u64, "dx.op.waveX"), (1, "dx.op.lane"), (2, "main")] {
            let mut ops = vec![id];
            ops.extend(name.bytes().map(u64::from));
            w.record(1, &ops, wd);
        }
        w.end(wd);
        w.enter(FUNCTION, wd, wd);
        w.record(1, &[1], wd); // declareblocks
        // value ids: next = 5. call lane(111): callee 1 -> rel 4, arg 4 -> rel 1
        w.record(34, &[0, 0, 4, 1], wd); // produces value 5
        // next = 6: call waveX(118): callee 0 -> rel 6, arg 3 -> rel 3
        w.record(34, &[0, 0, 6, 3], wd);
        w.record(34, &[0, 1 << 15, 2, 6, 3], wd); // explicit type form
        w.record(11, &[0], wd); // br
        w.record(10, &[], wd); // ret void
        w.end(wd);
        w.end(wd);
        w.bytes
    }

    #[test]
    fn calls_resolve_to_dx_ops_with_their_opcodes() {
        let top = parse(&module()).unwrap();
        let f = functions(&top).unwrap();
        assert_eq!(f.len(), 1);
        let ops: Vec<_> = f[0].iter().map(|i| (i.code, i.dx_op)).collect();
        assert_eq!(names(&top).get(&f[0][0].callee.unwrap()).map(String::as_str), Some("dx.op.lane"));
        assert_eq!(
            ops,
            [(INST_CALL, Some(111)), (INST_CALL, Some(118)), (INST_CALL, Some(118)), (INST_BR, None), (INST_RET, None)]
        );
    }

    #[test]
    fn rejects_what_is_not_bitcode() {
        assert!(parse(b"nope").is_err());
        let mut m = module();
        m.truncate(m.len() / 2);
        assert!(parse(&m).is_err() || functions(&parse(&m).unwrap()).is_err());
    }
}
