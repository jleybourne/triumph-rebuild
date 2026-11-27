//! A bit-complete field model of GFX10 (RDNA2) instructions, and its encoder.
//!
//! Every bit of every instruction word belongs to exactly one of: the encoding
//! tag (a constant regenerated from [`Encoding`]), a named field (kept), or a
//! reserved range (not kept; the encoder writes zeros). Trailing dwords (the
//! literal, the SDWA/DPP word, the MIMG NSA address words) are not taken from the
//! instruction's length: the encoder derives their presence from the fields, the
//! way the hardware does. So `encode(decode(i)) == i.words` holds only if the
//! layouts name every bit the corpus sets, the tags are right, and the length
//! rules agree with the boundary decoder. A mismatch names the range that differs.
//!
//! Layouts follow AMD's "RDNA 2" Instruction Set Architecture reference, chapter 13
//! (microcode formats), written here from the field tables, not copied from code.

use super::{Encoding, Instruction, name};

/// A named bit field: `width` bits from bit `lo` of word `word`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub word: u8,
    pub lo: u8,
    pub width: u8,
}

const fn f(name: &'static str, word: u8, lo: u8, width: u8) -> Field {
    Field { name, word, lo, width }
}

/// The fixed words of one encoding (or of a trailing SDWA/DPP word).
#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: &'static str,
    pub words: u8,
    /// (word, lo, width, value): the encoding's constant bits.
    pub tag: &'static [(u8, u8, u8, u32)],
    pub fields: &'static [Field],
    /// (word, lo, width): bits the ISA reserves; must be zero.
    pub reserved: &'static [(u8, u8, u8)],
}

macro_rules! layout {
    ($id:ident, $name:literal, $words:literal, tag [$($t:expr),*], fields [$($f:expr),* $(,)?], reserved [$($r:expr),*]) => {
        pub static $id: Layout = Layout {
            name: $name,
            words: $words,
            tag: &[$($t),*],
            fields: &[$($f),*],
            reserved: &[$($r),*],
        };
    };
}

layout!(SOP2, "sop2", 1, tag [(0, 30, 2, 0b10)],
    fields [f("ssrc0", 0, 0, 8), f("ssrc1", 0, 8, 8), f("sdst", 0, 16, 7), f("op", 0, 23, 7)],
    reserved []);
layout!(SOPK, "sopk", 1, tag [(0, 28, 4, 0b1011)],
    fields [f("simm16", 0, 0, 16), f("sdst", 0, 16, 7), f("op", 0, 23, 5)],
    reserved []);
layout!(SOP1, "sop1", 1, tag [(0, 23, 9, 0x17D)],
    fields [f("ssrc0", 0, 0, 8), f("op", 0, 8, 8), f("sdst", 0, 16, 7)],
    reserved []);
layout!(SOPC, "sopc", 1, tag [(0, 23, 9, 0x17E)],
    fields [f("ssrc0", 0, 0, 8), f("ssrc1", 0, 8, 8), f("op", 0, 16, 7)],
    reserved []);
layout!(SOPP, "sopp", 1, tag [(0, 23, 9, 0x17F)],
    fields [f("simm16", 0, 0, 16), f("op", 0, 16, 7)],
    reserved []);
layout!(SMEM, "smem", 2, tag [(0, 26, 6, 0x3D)],
    fields [f("sbase", 0, 0, 6), f("sdata", 0, 6, 7), f("dlc", 0, 14, 1), f("glc", 0, 16, 1),
        f("op", 0, 18, 8), f("offset", 1, 0, 21), f("soffset", 1, 25, 7)],
    reserved [(0, 13, 1), (0, 15, 1), (0, 17, 1), (1, 21, 4)]);
layout!(VOP2, "vop2", 1, tag [(0, 31, 1, 0)],
    fields [f("src0", 0, 0, 9), f("vsrc1", 0, 9, 8), f("vdst", 0, 17, 8), f("op", 0, 25, 6)],
    reserved []);
layout!(VOP1, "vop1", 1, tag [(0, 25, 7, 0x3F)],
    fields [f("src0", 0, 0, 9), f("op", 0, 9, 8), f("vdst", 0, 17, 8)],
    reserved []);
layout!(VOPC, "vopc", 1, tag [(0, 25, 7, 0x3E)],
    fields [f("src0", 0, 0, 9), f("vsrc1", 0, 9, 8), f("op", 0, 17, 8)],
    reserved []);
layout!(VOP3A, "vop3a", 2, tag [(0, 26, 6, 0x35)],
    fields [f("vdst", 0, 0, 8), f("abs", 0, 8, 3), f("opsel", 0, 11, 4), f("clamp", 0, 15, 1),
        f("op", 0, 16, 10), f("src0", 1, 0, 9), f("src1", 1, 9, 9), f("src2", 1, 18, 9),
        f("omod", 1, 27, 2), f("neg", 1, 29, 3)],
    reserved []);
layout!(VOP3B, "vop3b", 2, tag [(0, 26, 6, 0x35)],
    fields [f("vdst", 0, 0, 8), f("sdst", 0, 8, 7), f("clamp", 0, 15, 1), f("op", 0, 16, 10),
        f("src0", 1, 0, 9), f("src1", 1, 9, 9), f("src2", 1, 18, 9), f("omod", 1, 27, 2),
        f("neg", 1, 29, 3)],
    reserved []);
layout!(VOP3P, "vop3p", 2, tag [(0, 23, 9, 0x198)],
    fields [f("vdst", 0, 0, 8), f("neg_hi", 0, 8, 3), f("opsel", 0, 11, 3), f("opsel_hi2", 0, 14, 1),
        f("clamp", 0, 15, 1), f("op", 0, 16, 7), f("src0", 1, 0, 9), f("src1", 1, 9, 9),
        f("src2", 1, 18, 9), f("opsel_hi", 1, 27, 2), f("neg", 1, 29, 3)],
    reserved []);
layout!(VINTRP, "vintrp", 1, tag [(0, 26, 6, 0x32)],
    fields [f("vsrc", 0, 0, 8), f("attrchan", 0, 8, 2), f("attr", 0, 10, 6), f("op", 0, 16, 2),
        f("vdst", 0, 18, 8)],
    reserved []);
layout!(DS, "ds", 2, tag [(0, 26, 6, 0x36)],
    fields [f("offset0", 0, 0, 8), f("offset1", 0, 8, 8), f("gds", 0, 17, 1), f("op", 0, 18, 8),
        f("addr", 1, 0, 8), f("data0", 1, 8, 8), f("data1", 1, 16, 8), f("vdst", 1, 24, 8)],
    reserved [(0, 16, 1)]);
layout!(MUBUF, "mubuf", 2, tag [(0, 26, 6, 0x38)],
    fields [f("offset", 0, 0, 12), f("offen", 0, 12, 1), f("idxen", 0, 13, 1), f("glc", 0, 14, 1),
        f("dlc", 0, 15, 1), f("lds", 0, 16, 1), f("op", 0, 18, 7), f("op_hi", 0, 25, 1),
        f("vaddr", 1, 0, 8), f("vdata", 1, 8, 8), f("srsrc", 1, 16, 5), f("slc", 1, 22, 1),
        f("tfe", 1, 23, 1), f("soffset", 1, 24, 8)],
    reserved [(0, 17, 1), (1, 21, 1)]);
layout!(MTBUF, "mtbuf", 2, tag [(0, 26, 6, 0x3A)],
    fields [f("offset", 0, 0, 12), f("offen", 0, 12, 1), f("idxen", 0, 13, 1), f("glc", 0, 14, 1),
        f("dlc", 0, 15, 1), f("op", 0, 16, 3), f("format", 0, 19, 7), f("vaddr", 1, 0, 8),
        f("vdata", 1, 8, 8), f("srsrc", 1, 16, 5), f("op_hi", 1, 21, 1), f("slc", 1, 22, 1),
        f("tfe", 1, 23, 1), f("soffset", 1, 24, 8)],
    reserved []);
layout!(MIMG, "mimg", 2, tag [(0, 26, 6, 0x3C)],
    fields [f("op_hi", 0, 0, 1), f("nsa", 0, 1, 2), f("dim", 0, 3, 3), f("dlc", 0, 7, 1),
        f("dmask", 0, 8, 4), f("unorm", 0, 12, 1), f("glc", 0, 13, 1), f("r128", 0, 15, 1),
        f("tfe", 0, 16, 1), f("lwe", 0, 17, 1), f("op", 0, 18, 7), f("slc", 0, 25, 1),
        f("vaddr", 1, 0, 8), f("vdata", 1, 8, 8), f("srsrc", 1, 16, 5), f("ssamp", 1, 21, 5),
        f("a16", 1, 30, 1), f("d16", 1, 31, 1)],
    reserved [(0, 6, 1), (0, 14, 1), (1, 26, 4)]);
layout!(EXP, "exp", 2, tag [(0, 26, 6, 0x3E)],
    fields [f("en", 0, 0, 4), f("target", 0, 4, 6), f("compr", 0, 10, 1), f("done", 0, 11, 1),
        f("vm", 0, 12, 1), f("vsrc0", 1, 0, 8), f("vsrc1", 1, 8, 8), f("vsrc2", 1, 16, 8),
        f("vsrc3", 1, 24, 8)],
    reserved [(0, 13, 13)]);
layout!(FLAT, "flat", 2, tag [(0, 26, 6, 0x37)],
    fields [f("offset", 0, 0, 12), f("dlc", 0, 12, 1), f("lds", 0, 13, 1), f("seg", 0, 14, 2),
        f("glc", 0, 16, 1), f("slc", 0, 17, 1), f("op", 0, 18, 7), f("addr", 1, 0, 8),
        f("data", 1, 8, 8), f("saddr", 1, 16, 7), f("vdst", 1, 24, 8)],
    reserved [(0, 25, 1), (1, 23, 1)]);

// The trailing word of VOP1/VOP2/VOPC in their SDWA and DPP forms (word 0 here).
layout!(SDWA, "sdwa", 1, tag [],
    fields [f("src0", 0, 0, 8), f("dst_sel", 0, 8, 3), f("dst_unused", 0, 11, 2), f("clamp", 0, 13, 1),
        f("omod", 0, 14, 2), f("src0_sel", 0, 16, 3), f("src0_sext", 0, 19, 1), f("src0_neg", 0, 20, 1),
        f("src0_abs", 0, 21, 1), f("s0", 0, 23, 1), f("src1_sel", 0, 24, 3), f("src1_sext", 0, 27, 1),
        f("src1_neg", 0, 28, 1), f("src1_abs", 0, 29, 1), f("s1", 0, 31, 1)],
    reserved [(0, 22, 1), (0, 30, 1)]);
layout!(SDWAB, "sdwab", 1, tag [],
    fields [f("src0", 0, 0, 8), f("sdst", 0, 8, 7), f("sd", 0, 15, 1), f("src0_sel", 0, 16, 3),
        f("src0_sext", 0, 19, 1), f("src0_neg", 0, 20, 1), f("src0_abs", 0, 21, 1), f("s0", 0, 23, 1),
        f("src1_sel", 0, 24, 3), f("src1_sext", 0, 27, 1), f("src1_neg", 0, 28, 1),
        f("src1_abs", 0, 29, 1), f("s1", 0, 31, 1)],
    reserved [(0, 22, 1), (0, 30, 1)]);
layout!(DPP16, "dpp16", 1, tag [],
    fields [f("src0", 0, 0, 8), f("dpp_ctrl", 0, 8, 9), f("fi", 0, 18, 1), f("bound_ctrl", 0, 19, 1),
        f("src0_neg", 0, 20, 1), f("src0_abs", 0, 21, 1), f("src1_neg", 0, 22, 1),
        f("src1_abs", 0, 23, 1), f("bank_mask", 0, 24, 4), f("row_mask", 0, 28, 4)],
    reserved [(0, 17, 1)]);
layout!(DPP8, "dpp8", 1, tag [],
    fields [f("src0", 0, 0, 8), f("sel", 0, 8, 24)],
    reserved []);

/// Every layout, for the partition test.
pub static LAYOUTS: [&Layout; 23] = [
    &SOP2, &SOPK, &SOP1, &SOPC, &SOPP, &SMEM, &VOP2, &VOP1, &VOPC, &VOP3A, &VOP3B, &VOP3P,
    &VINTRP, &DS, &MUBUF, &MTBUF, &MIMG, &EXP, &FLAT, &SDWA, &SDWAB, &DPP16, &DPP8,
];

/// The source code in `src0` of VOP1/VOP2/VOPC that announces a trailing word.
pub const SRC_SDWA: u32 = 0xF9;
pub const SRC_DPP16: u32 = 0xFA;
pub const SRC_DPP8: u32 = 0xE9;
pub const SRC_DPP8_FI: u32 = 0xEA;
pub const SRC_LITERAL: u32 = 0xFF;

/// VOP3 opcodes in the VOP3B form (a scalar destination in bits 14:8).
const VOP3B_OPS: &[&str] = &[
    "v_add_co_u32",
    "v_sub_co_u32",
    "v_subrev_co_u32",
    "v_add_co_ci_u32",
    "v_sub_co_ci_u32",
    "v_subrev_co_ci_u32",
    "v_div_scale_f32",
    "v_div_scale_f64",
    "v_mad_u64_u32",
    "v_mad_i64_i32",
];

const MK_AK: &[&str] = &[
    "v_madmk_f32", "v_madak_f32", "v_fmamk_f32", "v_fmaak_f32",
    "v_madmk_f16", "v_madak_f16", "v_fmamk_f16", "v_fmaak_f16",
];

/// An instruction as named fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub encoding: Encoding,
    pub layout: &'static Layout,
    /// One per `layout.fields`.
    pub values: Vec<u32>,
    /// The SDWA or DPP word's layout and fields.
    pub extra: Option<(&'static Layout, Vec<u32>)>,
    pub literal: Option<u32>,
    /// MIMG NSA: the address VGPRs held in the extra dwords, four per dword.
    pub nsa: Vec<u8>,
}

fn get(word: u32, lo: u8, width: u8) -> u32 {
    (((word as u64) >> lo) & ((1u64 << width) - 1)) as u32
}

fn put(word: &mut u32, lo: u8, width: u8, v: u32) {
    let mask = (((1u64 << width) - 1) << lo) as u32;
    *word = (*word & !mask) | ((v << lo) & mask);
}

fn layout_of(encoding: Encoding, opcode: u16) -> &'static Layout {
    match encoding {
        Encoding::Sop2 => &SOP2,
        Encoding::Sopk => &SOPK,
        Encoding::Sop1 => &SOP1,
        Encoding::Sopc => &SOPC,
        Encoding::Sopp => &SOPP,
        Encoding::Smem => &SMEM,
        Encoding::Vop2 => &VOP2,
        Encoding::Vop1 => &VOP1,
        Encoding::Vopc => &VOPC,
        Encoding::Vop3 => match name(encoding, opcode) {
            Some(n) if VOP3B_OPS.contains(&n) => &VOP3B,
            _ => &VOP3A,
        },
        Encoding::Vop3p => &VOP3P,
        Encoding::Vintrp => &VINTRP,
        Encoding::Ds => &DS,
        Encoding::Mubuf => &MUBUF,
        Encoding::Mtbuf => &MTBUF,
        Encoding::Mimg => &MIMG,
        Encoding::Exp => &EXP,
        Encoding::Flat | Encoding::Global | Encoding::Scratch => &FLAT,
    }
}

impl Decoded {
    /// A field's value by name (main words, then the SDWA/DPP word).
    pub fn get(&self, field: &str) -> Option<u32> {
        let main = self.layout.fields.iter().position(|f| f.name == field);
        if let Some(i) = main {
            return Some(self.values[i]);
        }
        let (l, v) = self.extra.as_ref()?;
        l.fields.iter().position(|f| f.name == field).map(|i| v[i])
    }

    /// The opcode the fields spell (split opcodes joined).
    pub fn opcode(&self) -> u16 {
        let op = self.get("op").unwrap_or(0);
        let hi = self.get("op_hi").unwrap_or(0);
        (match self.encoding {
            Encoding::Mubuf | Encoding::Mimg => op | hi << 7,
            Encoding::Mtbuf => op | hi << 3,
            _ => op,
        }) as u16
    }

    /// The encoding the fields spell (FLAT's segment picks FLAT/SCRATCH/GLOBAL).
    pub fn spelled_encoding(&self) -> Encoding {
        match (self.encoding, self.get("seg")) {
            (Encoding::Flat | Encoding::Global | Encoding::Scratch, Some(0)) => Encoding::Flat,
            (Encoding::Flat | Encoding::Global | Encoding::Scratch, Some(1)) => Encoding::Scratch,
            (Encoding::Flat | Encoding::Global | Encoding::Scratch, Some(2)) => Encoding::Global,
            (e, _) => e,
        }
    }

    /// The trailing word announced by `src0` of VOP1/VOP2/VOPC.
    fn extra_layout(&self) -> Option<&'static Layout> {
        if !matches!(self.encoding, Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc) {
            return None;
        }
        match self.get("src0")? {
            SRC_SDWA if self.encoding == Encoding::Vopc => Some(&SDWAB),
            SRC_SDWA => Some(&SDWA),
            SRC_DPP16 => Some(&DPP16),
            SRC_DPP8 | SRC_DPP8_FI => Some(&DPP8),
            _ => None,
        }
    }

    /// Whether a 32-bit literal follows, from the fields alone.
    pub fn needs_literal(&self) -> bool {
        let lit = |f: &str| self.get(f) == Some(SRC_LITERAL);
        let op_name = name(self.encoding, self.opcode()).unwrap_or("");
        match self.encoding {
            Encoding::Sop2 | Encoding::Sopc => lit("ssrc0") || lit("ssrc1"),
            Encoding::Sop1 => lit("ssrc0"),
            Encoding::Sopk => op_name == "s_setreg_imm32_b32",
            Encoding::Vop1 | Encoding::Vopc => lit("src0"),
            Encoding::Vop2 => lit("src0") || MK_AK.contains(&op_name),
            Encoding::Vop3 | Encoding::Vop3p => lit("src0") || lit("src1") || lit("src2"),
            _ => false,
        }
    }
}

/// Splits an instruction into fields. Never fails: bits a layout does not name
/// are dropped, and the re-encode shows them.
pub fn decode(inst: &Instruction) -> Decoded {
    let layout = layout_of(inst.encoding, inst.opcode);
    let w = |i: usize| inst.words.get(i).copied().unwrap_or(0);
    let values = layout.fields.iter().map(|f| get(w(f.word as usize), f.lo, f.width)).collect();
    let mut d = Decoded {
        encoding: inst.encoding,
        layout,
        values,
        extra: None,
        literal: None,
        nsa: Vec::new(),
    };
    let mut next = layout.words as usize;
    if let Some(x) = d.extra_layout() {
        let word = w(next);
        d.extra = Some((x, x.fields.iter().map(|f| get(word, f.lo, f.width)).collect()));
        next += 1;
    }
    if inst.encoding == Encoding::Mimg {
        let n = d.get("nsa").unwrap_or(0) as usize;
        d.nsa = (0..n).flat_map(|i| w(next + i).to_le_bytes()).collect();
        next += n;
    }
    if d.needs_literal() {
        d.literal = Some(w(next));
    }
    d
}

fn pack(layout: &Layout, values: &[u32], words: &mut [u32]) {
    for &(word, lo, width, value) in layout.tag {
        put(&mut words[word as usize], lo, width, value);
    }
    for (f, &v) in layout.fields.iter().zip(values) {
        put(&mut words[f.word as usize], f.lo, f.width, v);
    }
}

/// Builds the words from the fields; reserved bits are zero and the trailing
/// dwords are those the fields call for.
pub fn encode(d: &Decoded) -> Vec<u32> {
    let mut words = vec![0u32; d.layout.words as usize];
    pack(d.layout, &d.values, &mut words);
    if let Some(x) = d.extra_layout() {
        let mut word = [0u32];
        match &d.extra {
            Some((_, v)) => pack(x, v, &mut word),
            None => pack(x, &vec![0; x.fields.len()], &mut word),
        }
        words.push(word[0]);
    }
    if d.encoding == Encoding::Mimg {
        let n = d.get("nsa").unwrap_or(0) as usize;
        for i in 0..n {
            let b = |k: usize| d.nsa.get(4 * i + k).copied().unwrap_or(0);
            words.push(u32::from_le_bytes([b(0), b(1), b(2), b(3)]));
        }
    }
    if d.needs_literal() {
        words.push(d.literal.unwrap_or(0));
    }
    words
}

/// Why a re-encode differs: the first range that disagrees.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mismatch {
    /// Re-encoded to a different number of dwords.
    Length { original: usize, encoded: usize },
    /// A reserved range is set in the original.
    Reserved { layout: &'static str, word: u8, lo: u8, width: u8 },
    /// The encoding tag or opcode the fields spell is not the boundary decoder's.
    Tag { layout: &'static str },
    /// Some other bit (a model error).
    Bits { layout: &'static str, word: usize, xor: u32 },
}

/// Decodes and re-encodes; `Ok` if byte-exact.
pub fn round_trip(inst: &Instruction) -> Result<Decoded, Mismatch> {
    let d = decode(inst);
    if d.opcode() != inst.opcode || d.spelled_encoding() != inst.encoding {
        return Err(Mismatch::Tag { layout: d.layout.name });
    }
    let words = encode(&d);
    if words.len() != inst.words.len() {
        return Err(Mismatch::Length { original: inst.words.len(), encoded: words.len() });
    }
    for (i, (&a, &b)) in inst.words.iter().zip(&words).enumerate() {
        if a == b {
            continue;
        }
        let xor = a ^ b;
        // Main words: reserved, else tag. The trailing word of SDWA/DPP: its own.
        let (layout, word) = if i < d.layout.words as usize {
            (d.layout, i as u8)
        } else if let (Some((x, _)), true) = (&d.extra, i == d.layout.words as usize) {
            (*x, 0)
        } else {
            return Err(Mismatch::Bits { layout: d.layout.name, word: i, xor });
        };
        for &(w, lo, width) in layout.reserved {
            if w == word && get(xor, lo, width) != 0 {
                return Err(Mismatch::Reserved { layout: layout.name, word: w, lo, width });
            }
        }
        for &(w, lo, width, _) in layout.tag {
            if w == word && get(xor, lo, width) != 0 {
                return Err(Mismatch::Tag { layout: layout.name });
            }
        }
        return Err(Mismatch::Bits { layout: layout.name, word: i, xor });
    }
    Ok(d)
}

/// A 9-bit (VALU) or 8-bit (SALU) source operand code, typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Src {
    Sgpr(u8),
    VccLo,
    VccHi,
    Ttmp(u8),
    M0,
    Null,
    ExecLo,
    ExecHi,
    /// 128-208: integers 0..64 and -1..-16; 240-248: float constants.
    Inline(u16),
    /// 250-254: dpp16-adjacent specials, vccz, execz, scc, lds_direct.
    Special(u16),
    Literal,
    Vgpr(u8),
}

pub fn src(code: u32) -> Src {
    match code {
        0..=105 => Src::Sgpr(code as u8),
        106 => Src::VccLo,
        107 => Src::VccHi,
        108..=123 => Src::Ttmp((code - 108) as u8),
        124 => Src::M0,
        125 => Src::Null,
        126 => Src::ExecLo,
        127 => Src::ExecHi,
        128..=208 | 240..=248 => Src::Inline(code as u16),
        255 => Src::Literal,
        256..=511 => Src::Vgpr((code - 256) as u8),
        _ => Src::Special(code as u16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10;

    fn inst(words: &[u32]) -> Instruction {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).chain(0xBF81_0000u32.to_le_bytes()).collect();
        gfx10::decode(&bytes).unwrap().remove(0)
    }

    #[test]
    fn layouts_partition_every_bit() {
        for l in LAYOUTS {
            let mut seen = vec![0u32; l.words as usize];
            let mut claim = |word: u8, lo: u8, width: u8| {
                let mask = (((1u64 << width) - 1) << lo) as u32;
                assert_eq!(seen[word as usize] & mask, 0, "{}: bits claimed twice in word {word}", l.name);
                seen[word as usize] |= mask;
            };
            for &(w, lo, width, _) in l.tag {
                claim(w, lo, width);
            }
            for fl in l.fields {
                claim(fl.word, fl.lo, fl.width);
            }
            for &(w, lo, width) in l.reserved {
                claim(w, lo, width);
            }
            assert!(seen.iter().all(|&m| m == u32::MAX), "{}: unclaimed bits {seen:x?}", l.name);
        }
    }

    #[test]
    fn known_instructions_round_trip_with_their_fields() {
        // s_mov_b32 s0, literal 1.0
        let d = round_trip(&inst(&[0xBE80_03FF, 0x3F80_0000])).unwrap();
        assert_eq!((d.get("ssrc0"), d.literal), (Some(0xFF), Some(0x3F80_0000)));
        // v_add_f32 v0, s0, v0
        let d = round_trip(&inst(&[0x0600_0000])).unwrap();
        assert_eq!((d.get("op"), d.get("src0"), d.get("vsrc1")), (Some(3), Some(0), Some(0)));
        // v_fma_f32 with a literal src2
        let d = round_trip(&inst(&[0xD54B_0000, 0x03FE_0000, 0x4000_0000])).unwrap();
        assert_eq!((d.layout.name, d.get("src2"), d.literal), ("vop3a", Some(0xFF), Some(0x4000_0000)));
        // image_sample with one NSA dword
        let d = round_trip(&inst(&[0xF080_0F0A, 0x0000_0201, 0x0000_0403])).unwrap();
        assert_eq!((d.get("nsa"), d.get("dmask"), d.nsa.as_slice()), (Some(1), Some(0xF), &[3, 4, 0, 0][..]));
        // exp mrt0 v0 v0 v0 v0 done vm
        round_trip(&inst(&[0xF800_180F, 0])).unwrap();
        // v_mov_b32 dpp quad_perm: src0 = 0xFA, the DPP word follows
        let d = round_trip(&inst(&[0x7E00_02FA, 0xFF08_1B01])).unwrap();
        assert_eq!((d.extra.as_ref().unwrap().0.name, d.get("dpp_ctrl"), d.get("row_mask")), ("dpp16", Some(0x1B), Some(0xF)));
        // v_add_co_u32 (VOP3B): vcc_lo in bits 14:8
        let d = round_trip(&inst(&[0xD70F_6A00, 0x0002_0001])).unwrap();
        assert_eq!((d.layout.name, d.get("sdst")), ("vop3b", Some(106)));
    }

    #[test]
    fn reserved_bits_and_lengths_are_named() {
        // ds_write_b32 with reserved bit 16 set.
        let i = inst(&[0xD834_0000 | 1 << 16, 0x0000_0100]);
        assert_eq!(round_trip(&i), Err(Mismatch::Reserved { layout: "ds", word: 0, lo: 16, width: 1 }));
        // A literal the fields do not call for.
        let mut i = inst(&[0x0600_0000]);
        i.words.push(7);
        assert_eq!(round_trip(&i), Err(Mismatch::Length { original: 2, encoded: 1 }));
        // VOP3P with bits 25:23 set: the fork's boundary decoder accepts it, the tag does not.
        let i = inst(&[0xCC80_0000 | 0x0F << 16, 0x0402_0100]);
        assert_eq!(round_trip(&i), Err(Mismatch::Tag { layout: "vop3p" }));
    }

    #[test]
    fn sources_are_typed() {
        assert_eq!(src(3), Src::Sgpr(3));
        assert_eq!(src(106), Src::VccLo);
        assert_eq!(src(128), Src::Inline(128));
        assert_eq!(src(255), Src::Literal);
        assert_eq!(src(256 + 7), Src::Vgpr(7));
        assert_eq!(src(253), Src::Special(253));
    }
}
