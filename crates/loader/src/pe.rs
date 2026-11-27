//! PE32+ parsing. Everything is read from the file bytes with bounds checks; tables are walked with
//! limits so garbage cannot loop forever. The result owns its data and does not borrow the file.

use std::fmt;

pub const MACHINE_AMD64: u16 = 0x8664;
const MAGIC_PE32_PLUS: u16 = 0x20b;

/// Data directory indices.
pub mod dir {
    pub const EXPORT: usize = 0;
    pub const IMPORT: usize = 1;
    pub const RESOURCE: usize = 2;
    pub const EXCEPTION: usize = 3;
    pub const SECURITY: usize = 4;
    pub const BASERELOC: usize = 5;
    pub const DEBUG: usize = 6;
    pub const TLS: usize = 9;
    pub const LOAD_CONFIG: usize = 10;
    pub const IAT: usize = 12;
    pub const DELAY_IMPORT: usize = 13;
    pub const CLR: usize = 14;
}

/// Base relocation types (`IMAGE_REL_BASED_*`).
pub mod rel {
    pub const ABSOLUTE: u8 = 0;
    pub const HIGH: u8 = 1;
    pub const LOW: u8 = 2;
    pub const HIGHLOW: u8 = 3;
    pub const HIGHADJ: u8 = 4;
    pub const DIR64: u8 = 10;
}

/// Limits that keep a hostile image bounded.
const MAX_IMPORT_DLLS: usize = 4096;
const MAX_THUNKS: usize = 1 << 16;
const MAX_TLS_CALLBACKS: usize = 1024;
const MAX_NAME: usize = 4096;
const MAX_EXPORTS: u32 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeError {
    /// No `MZ` at offset 0.
    NotMz,
    /// No `PE\0\0` where `e_lfanew` points.
    NotPe,
    /// A PE32 (32-bit) or other optional-header magic.
    NotPe32Plus(u16),
    /// A read ran past the end of the file or the image.
    Truncated(&'static str),
    /// A structure is present but malformed.
    Malformed(&'static str),
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::NotMz => f.write_str("no MZ header"),
            PeError::NotPe => f.write_str("no PE signature"),
            PeError::NotPe32Plus(m) => write!(f, "optional header magic 0x{m:x}, not PE32+"),
            PeError::Truncated(w) => write!(f, "truncated: {w}"),
            PeError::Malformed(w) => write!(f, "malformed: {w}"),
        }
    }
}

impl std::error::Error for PeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataDir {
    pub rva: u32,
    pub size: u32,
}

impl DataDir {
    pub fn present(&self) -> bool {
        self.rva != 0 && self.size != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub rva: u32,
    pub virtual_size: u32,
    pub file_offset: u32,
    pub file_size: u32,
    pub characteristics: u32,
}

impl Section {
    /// The bytes the loader copies from the file: `min(raw, virtual)` (raw if virtual is 0).
    pub fn mapped_file_size(&self) -> u32 {
        if self.virtual_size == 0 {
            self.file_size
        } else {
            self.file_size.min(self.virtual_size)
        }
    }
    /// The extent the section covers in the image.
    pub fn extent(&self) -> u32 {
        self.virtual_size.max(self.file_size)
    }
}

/// One base relocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reloc {
    pub rva: u32,
    pub kind: u8,
}

/// An imported symbol: by name (with its hint) or by ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Symbol {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl Symbol {
    pub fn key(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::Name { name, .. } => f.write_str(name),
            Symbol::Ordinal(o) => write!(f, "#{o}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub symbol: Symbol,
    /// RVA of the 8-byte IAT slot the loader fills.
    pub iat_rva: u32,
}

/// One imported DLL and what is taken from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportModule {
    /// The DLL name as written in the image.
    pub dll: String,
    pub entries: Vec<Import>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub ordinal: u32,
    pub name: Option<String>,
    /// Code or data RVA; `None` for a forwarder.
    pub rva: Option<u32>,
    /// `"dll.symbol"` for a forwarded export.
    pub forward: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tls {
    pub start_va: u64,
    pub end_va: u64,
    pub index_va: u64,
    pub callbacks_va: u64,
    pub zero_fill: u32,
    /// Callback RVAs in call order.
    pub callbacks: Vec<u32>,
}

/// `RUNTIME_FUNCTION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin: u32,
    pub end: u32,
    pub unwind: u32,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub machine: u16,
    pub characteristics: u16,
    pub timestamp: u32,
    pub entry_rva: u32,
    pub image_base: u64,
    pub section_alignment: u32,
    pub file_alignment: u32,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub subsystem: u16,
    pub dll_characteristics: u16,
    pub stack_reserve: u64,
    pub stack_commit: u64,
    pub dirs: [DataDir; 16],
    pub sections: Vec<Section>,
    pub relocs: Vec<Reloc>,
    pub imports: Vec<ImportModule>,
    pub delay_imports: Vec<ImportModule>,
    /// Name of the image in its export directory, if any.
    pub export_name: Option<String>,
    pub exports: Vec<Export>,
    pub tls: Option<Tls>,
    pub pdata: Vec<RuntimeFunction>,
    /// RVA of every string the import, delay-load and export tables point at (DLL names, import
    /// names, export names, forwarders), so a string scan can tell table entries from other strings.
    pub table_strings: Vec<u32>,
}

pub const FILE_DLL: u16 = 0x2000;
pub const FILE_RELOCS_STRIPPED: u16 = 0x0001;

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o.checked_add(2)?).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o.checked_add(4)?).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}
fn u64_at(d: &[u8], o: usize) -> Option<u64> {
    d.get(o..o.checked_add(8)?).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// Reads the image's structures from the file bytes by RVA.
struct View<'a, 's> {
    data: &'a [u8],
    size_of_headers: u32,
    sections: &'s [Section],
}

impl<'a> View<'a, '_> {
    /// File offset of `rva`, and how many bytes of the file back it from there.
    fn offset(&self, rva: u32) -> Option<(usize, usize)> {
        if rva < self.size_of_headers {
            let o = rva as usize;
            return (o < self.data.len()).then(|| (o, self.data.len() - o));
        }
        for s in self.sections {
            let Some(within) = rva.checked_sub(s.rva) else { continue };
            if within < s.extent() {
                let raw = s.mapped_file_size();
                if within >= raw {
                    return None; // zero-fill, not in the file
                }
                let o = (s.file_offset as usize).checked_add(within as usize)?;
                let avail = ((raw - within) as usize).min(self.data.len().saturating_sub(o));
                return (avail > 0).then_some((o, avail));
            }
        }
        None
    }

    fn bytes(&self, rva: u32, len: usize) -> Option<&'a [u8]> {
        let (o, avail) = self.offset(rva)?;
        (len <= avail).then(|| &self.data[o..o + len])
    }
    fn u16(&self, rva: u32) -> Option<u16> {
        self.bytes(rva, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&self, rva: u32) -> Option<u32> {
        self.bytes(rva, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&self, rva: u32) -> Option<u64> {
        self.bytes(rva, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    /// A NUL-terminated ASCII string.
    fn cstr(&self, rva: u32) -> Option<String> {
        let (o, avail) = self.offset(rva)?;
        let s = &self.data[o..o + avail.min(MAX_NAME)];
        let n = s.iter().position(|&b| b == 0)?;
        Some(String::from_utf8_lossy(&s[..n]).into_owned())
    }
}

impl Image {
    /// Parses a PE32+ image from its file bytes.
    pub fn parse(data: &[u8]) -> Result<Image, PeError> {
        if data.get(0..2) != Some(b"MZ") {
            return Err(PeError::NotMz);
        }
        let nt = u32_at(data, 0x3c).ok_or(PeError::Truncated("DOS header"))? as usize;
        if data.get(nt..nt.saturating_add(4)) != Some(b"PE\0\0") {
            return Err(PeError::NotPe);
        }
        let fh = nt + 4;
        let t = PeError::Truncated("file header");
        let machine = u16_at(data, fh).ok_or(t.clone())?;
        let nsections = u16_at(data, fh + 2).ok_or(t.clone())? as usize;
        let timestamp = u32_at(data, fh + 4).ok_or(t.clone())?;
        let opt_size = u16_at(data, fh + 16).ok_or(t.clone())? as usize;
        let characteristics = u16_at(data, fh + 18).ok_or(t)?;
        let oh = fh + 20;
        let magic = u16_at(data, oh).ok_or(PeError::Truncated("optional header"))?;
        if magic != MAGIC_PE32_PLUS {
            return Err(PeError::NotPe32Plus(magic));
        }
        let t = PeError::Truncated("optional header");
        let o32 = |off: usize| u32_at(data, oh + off).ok_or(t.clone());
        let o64 = |off: usize| u64_at(data, oh + off).ok_or(t.clone());
        let entry_rva = o32(16)?;
        let image_base = o64(24)?;
        let section_alignment = o32(32)?;
        let file_alignment = o32(36)?;
        let size_of_image = o32(56)?;
        let size_of_headers = o32(60)?;
        let subsystem = u16_at(data, oh + 68).ok_or(t.clone())?;
        let dll_characteristics = u16_at(data, oh + 70).ok_or(t.clone())?;
        let stack_reserve = o64(72)?;
        let stack_commit = o64(80)?;
        let ndirs = o32(108)?.min(16) as usize;
        let mut dirs = [DataDir::default(); 16];
        for (i, d) in dirs.iter_mut().enumerate().take(ndirs) {
            let off = 112 + i * 8;
            if off + 8 > opt_size {
                break;
            }
            *d = DataDir { rva: o32(off)?, size: o32(off + 4)? };
        }

        let st = oh + opt_size;
        let mut sections = Vec::with_capacity(nsections);
        for i in 0..nsections {
            let s = st + i * 40;
            let hdr = data.get(s..s + 40).ok_or(PeError::Truncated("section table"))?;
            let raw_name = &hdr[0..8];
            let n = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
            let rd = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
            sections.push(Section {
                name: String::from_utf8_lossy(&raw_name[..n]).into_owned(),
                virtual_size: rd(8),
                rva: rd(12),
                file_size: rd(16),
                file_offset: rd(20),
                characteristics: rd(36),
            });
        }

        let mut img = Image {
            machine,
            characteristics,
            timestamp,
            entry_rva,
            image_base,
            section_alignment,
            file_alignment,
            size_of_image,
            size_of_headers,
            subsystem,
            dll_characteristics,
            stack_reserve,
            stack_commit,
            dirs,
            sections,
            relocs: Vec::new(),
            imports: Vec::new(),
            delay_imports: Vec::new(),
            export_name: None,
            exports: Vec::new(),
            tls: None,
            pdata: Vec::new(),
            table_strings: Vec::new(),
        };
        let v = View { data, size_of_headers, sections: &img.sections };
        let relocs = parse_relocs(&v, dirs[dir::BASERELOC])?;
        let mut ts = Vec::new();
        let imports = parse_imports(&v, dirs[dir::IMPORT], &mut ts)?;
        let delay_imports = parse_delay_imports(&v, dirs[dir::DELAY_IMPORT], image_base, &mut ts)?;
        let (export_name, exports) = parse_exports(&v, dirs[dir::EXPORT], &mut ts)?;
        ts.sort_unstable();
        ts.dedup();
        let tls = parse_tls(&v, dirs[dir::TLS], image_base)?;
        let pdata = parse_pdata(&v, dirs[dir::EXCEPTION])?;
        img.relocs = relocs;
        img.imports = imports;
        img.delay_imports = delay_imports;
        img.export_name = export_name;
        img.exports = exports;
        img.tls = tls;
        img.pdata = pdata;
        img.table_strings = ts;
        Ok(img)
    }

    pub fn is_dll(&self) -> bool {
        self.characteristics & FILE_DLL != 0
    }

    /// Reads `len` bytes at `rva` from the file (None if not file-backed).
    pub fn read<'a>(&self, data: &'a [u8], rva: u32, len: usize) -> Option<&'a [u8]> {
        View { data, size_of_headers: self.size_of_headers, sections: &self.sections }.bytes(rva, len)
    }

    /// File offset of `rva`, if the file backs it.
    pub fn file_offset(&self, data: &[u8], rva: u32) -> Option<usize> {
        View { data, size_of_headers: self.size_of_headers, sections: &self.sections }.offset(rva).map(|(o, _)| o)
    }

    /// The RVA of file offset `off`, if a section or the headers map it.
    pub fn rva_of_offset(&self, off: usize) -> Option<u32> {
        if off < self.size_of_headers as usize {
            return Some(off as u32);
        }
        self.sections.iter().find_map(|s| {
            let within = off.checked_sub(s.file_offset as usize)?;
            (within < s.mapped_file_size() as usize).then(|| s.rva + within as u32)
        })
    }

    /// Finds an export by name.
    pub fn export(&self, name: &str) -> Option<&Export> {
        self.exports.iter().find(|e| e.name.as_deref() == Some(name))
    }
}

fn parse_relocs(v: &View, d: DataDir) -> Result<Vec<Reloc>, PeError> {
    let mut out = Vec::new();
    if !d.present() {
        return Ok(out);
    }
    let end = d.rva as u64 + d.size as u64;
    let mut at = d.rva as u64;
    while at + 8 <= end {
        let page = v.u32(at as u32).ok_or(PeError::Truncated("relocation block"))?;
        let size = v.u32(at as u32 + 4).ok_or(PeError::Truncated("relocation block"))?;
        if size == 0 {
            break; // some linkers pad the directory with zeros
        }
        if size < 8 || at + size as u64 > end {
            return Err(PeError::Malformed("relocation block size"));
        }
        let n = (size as usize - 8) / 2;
        let entries = v.bytes(at as u32 + 8, n * 2).ok_or(PeError::Truncated("relocation entries"))?;
        for e in entries.chunks_exact(2) {
            let e = u16::from_le_bytes([e[0], e[1]]);
            let kind = (e >> 12) as u8;
            if kind == rel::ABSOLUTE {
                continue;
            }
            out.push(Reloc { rva: page.wrapping_add((e & 0xfff) as u32), kind });
        }
        at += size as u64;
    }
    Ok(out)
}

/// Walks one import lookup table. `names` is the ILT (or the IAT if there is none).
fn parse_thunks(v: &View, names: u32, iat: u32, ts: &mut Vec<u32>) -> Result<Vec<Import>, PeError> {
    let mut out = Vec::new();
    for i in 0..MAX_THUNKS {
        let t = v
            .u64(names.wrapping_add(i as u32 * 8))
            .ok_or(PeError::Truncated("import lookup table"))?;
        if t == 0 {
            return Ok(out);
        }
        let symbol = if t & (1 << 63) != 0 {
            Symbol::Ordinal(t as u16)
        } else {
            let hn = (t & 0x7fff_ffff) as u32;
            let hint = v.u16(hn).ok_or(PeError::Truncated("import hint"))?;
            let name = v.cstr(hn + 2).ok_or(PeError::Truncated("import name"))?;
            ts.push(hn + 2);
            Symbol::Name { hint, name }
        };
        out.push(Import { symbol, iat_rva: iat.wrapping_add(i as u32 * 8) });
    }
    Err(PeError::Malformed("import lookup table has no end"))
}

fn parse_imports(v: &View, d: DataDir, ts: &mut Vec<u32>) -> Result<Vec<ImportModule>, PeError> {
    let mut out = Vec::new();
    if !d.present() {
        return Ok(out);
    }
    for i in 0..MAX_IMPORT_DLLS {
        let at = d.rva + i as u32 * 20;
        let desc = v.bytes(at, 20).ok_or(PeError::Truncated("import descriptor"))?;
        let rd = |o: usize| u32::from_le_bytes(desc[o..o + 4].try_into().unwrap());
        let (ilt, name, iat) = (rd(0), rd(12), rd(16));
        if name == 0 && iat == 0 {
            return Ok(out);
        }
        let dll = v.cstr(name).ok_or(PeError::Truncated("import DLL name"))?;
        ts.push(name);
        let entries = parse_thunks(v, if ilt != 0 { ilt } else { iat }, iat, ts)?;
        out.push(ImportModule { dll, entries });
    }
    Err(PeError::Malformed("import directory has no end"))
}

fn parse_delay_imports(v: &View, d: DataDir, base: u64, ts: &mut Vec<u32>) -> Result<Vec<ImportModule>, PeError> {
    let mut out = Vec::new();
    if !d.present() {
        return Ok(out);
    }
    for i in 0..MAX_IMPORT_DLLS {
        let at = d.rva + i as u32 * 32;
        let desc = v.bytes(at, 32).ok_or(PeError::Truncated("delay-load descriptor"))?;
        let rd = |o: usize| u32::from_le_bytes(desc[o..o + 4].try_into().unwrap());
        let (attrs, name, iat, int) = (rd(0), rd(4), rd(12), rd(16));
        if name == 0 {
            return Ok(out);
        }
        // Attribute bit 0 clear: the old form, where the fields hold VAs, not RVAs.
        let fix = |x: u32| if attrs & 1 == 0 { (x as u64).wrapping_sub(base) as u32 } else { x };
        let dll = v.cstr(fix(name)).ok_or(PeError::Truncated("delay-load DLL name"))?;
        ts.push(fix(name));
        let entries = parse_thunks(v, fix(int), fix(iat), ts)?;
        out.push(ImportModule { dll, entries });
    }
    Err(PeError::Malformed("delay-load directory has no end"))
}

fn parse_exports(v: &View, d: DataDir, ts: &mut Vec<u32>) -> Result<(Option<String>, Vec<Export>), PeError> {
    let mut out = Vec::new();
    if !d.present() {
        return Ok((None, out));
    }
    let t = PeError::Truncated("export directory");
    let hdr = v.bytes(d.rva, 40).ok_or(t)?;
    let rd = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
    let (name, base, nfuncs, nnames, funcs, names, ords) = (rd(12), rd(16), rd(20), rd(24), rd(28), rd(32), rd(36));
    if nfuncs > MAX_EXPORTS || nnames > MAX_EXPORTS {
        return Err(PeError::Malformed("export count"));
    }
    let dll_name = if name != 0 { v.cstr(name) } else { None };
    if dll_name.is_some() {
        ts.push(name);
    }
    let mut by_index: Vec<Option<String>> = vec![None; nfuncs as usize];
    for i in 0..nnames {
        let n = v.u32(names.wrapping_add(i * 4)).ok_or(PeError::Truncated("export names"))?;
        let o = v.u16(ords.wrapping_add(i * 2)).ok_or(PeError::Truncated("export ordinals"))? as usize;
        let s = v.cstr(n).ok_or(PeError::Truncated("export name"))?;
        ts.push(n);
        match by_index.get_mut(o) {
            Some(slot) if slot.is_none() => *slot = Some(s),
            Some(_) => {} // a second name for one function; the first wins
            None => return Err(PeError::Malformed("export name ordinal out of range")),
        }
    }
    let (lo, hi) = (d.rva as u64, d.rva as u64 + d.size as u64);
    for (i, name) in by_index.into_iter().enumerate() {
        let rva = v.u32(funcs.wrapping_add(i as u32 * 4)).ok_or(PeError::Truncated("export functions"))?;
        if rva == 0 {
            continue; // an unused ordinal
        }
        let forwarded = (rva as u64) >= lo && (rva as u64) < hi;
        if forwarded {
            ts.push(rva);
        }
        out.push(Export {
            ordinal: base.wrapping_add(i as u32),
            name,
            rva: (!forwarded).then_some(rva),
            forward: if forwarded { Some(v.cstr(rva).ok_or(PeError::Truncated("export forwarder"))?) } else { None },
        });
    }
    Ok((dll_name, out))
}

fn parse_tls(v: &View, d: DataDir, base: u64) -> Result<Option<Tls>, PeError> {
    if !d.present() {
        return Ok(None);
    }
    let t = PeError::Truncated("TLS directory");
    let hdr = v.bytes(d.rva, 40).ok_or(t)?;
    let rd = |o: usize| u64::from_le_bytes(hdr[o..o + 8].try_into().unwrap());
    let (start_va, end_va, index_va, callbacks_va) = (rd(0), rd(8), rd(16), rd(24));
    let zero_fill = u32::from_le_bytes(hdr[32..36].try_into().unwrap());
    let mut callbacks = Vec::new();
    if callbacks_va != 0 {
        let at = callbacks_va.wrapping_sub(base);
        if at > u32::MAX as u64 {
            return Err(PeError::Malformed("TLS callback array outside the image"));
        }
        let mut done = false;
        for i in 0..MAX_TLS_CALLBACKS {
            let cb = v.u64(at as u32 + i as u32 * 8).ok_or(PeError::Truncated("TLS callbacks"))?;
            if cb == 0 {
                done = true;
                break;
            }
            let rva = cb.wrapping_sub(base);
            if rva > u32::MAX as u64 {
                return Err(PeError::Malformed("TLS callback outside the image"));
            }
            callbacks.push(rva as u32);
        }
        if !done {
            return Err(PeError::Malformed("TLS callback array has no end"));
        }
    }
    Ok(Some(Tls { start_va, end_va, index_va, callbacks_va, zero_fill, callbacks }))
}

fn parse_pdata(v: &View, d: DataDir) -> Result<Vec<RuntimeFunction>, PeError> {
    if !d.present() {
        return Ok(Vec::new());
    }
    let n = d.size as usize / 12;
    let b = v.bytes(d.rva, n * 12).ok_or(PeError::Truncated("exception table"))?;
    Ok(b
        .chunks_exact(12)
        .map(|e| {
            let rd = |o: usize| u32::from_le_bytes(e[o..o + 4].try_into().unwrap());
            RuntimeFunction { begin: rd(0), end: rd(4), unwind: rd(8) }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth;

    #[test]
    fn headers_and_sections() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        assert_eq!(img.machine, MACHINE_AMD64);
        assert!(img.is_dll());
        assert_eq!(img.image_base, synth::BASE);
        assert_eq!(img.size_of_image, synth::SIZE_OF_IMAGE);
        let names: Vec<_> = img.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [".text", ".rdata", ".data", ".pdata", ".reloc"]);
        assert_eq!(img.entry_rva, synth::ENTRY);
    }

    #[test]
    fn relocations() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        let mut got: Vec<u32> = img.relocs.iter().map(|r| r.rva).collect();
        got.sort();
        let mut want = s.dir64.clone();
        want.sort();
        assert_eq!(got, want);
        assert!(img.relocs.iter().all(|r| r.kind == rel::DIR64));
    }

    #[test]
    fn imports_by_name_and_ordinal() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        assert_eq!(img.imports.len(), 2);
        let k = &img.imports[0];
        assert_eq!(k.dll, "kernelx.dll");
        assert_eq!(k.entries[0].symbol, Symbol::Name { hint: 7, name: "XMemAlloc".into() });
        assert_eq!(k.entries[1].symbol, Symbol::Ordinal(5));
        assert_eq!(k.entries[0].iat_rva, synth::IAT_KERNELX);
        assert_eq!(k.entries[1].iat_rva, synth::IAT_KERNELX + 8);
        assert_eq!(img.imports[1].dll, "xgameruntime.dll");
        assert_eq!(img.imports[1].entries[0].symbol.to_string(), "XGameRuntimeInitialize");
    }

    #[test]
    fn delay_imports() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        assert_eq!(img.delay_imports.len(), 1);
        assert_eq!(img.delay_imports[0].dll, "bink2.dll");
        assert_eq!(img.delay_imports[0].entries[0].symbol.to_string(), "BinkOpen");
        assert_eq!(img.delay_imports[0].entries[0].iat_rva, synth::DELAY_IAT);
    }

    #[test]
    fn exports_and_forwarders() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        assert_eq!(img.export_name.as_deref(), Some("synth.dll"));
        assert_eq!(img.export("Alpha").unwrap().rva, Some(synth::F0));
        assert_eq!(img.export("Beta").unwrap().ordinal, 2);
        let g = img.export("Gamma").unwrap();
        assert_eq!(g.rva, None);
        assert_eq!(g.forward.as_deref(), Some("kernelx.XMemFree"));
    }

    #[test]
    fn table_strings_and_offsets() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        // 3 import DLL names + 4 import names (one by ordinal has none) ... counted from the builder:
        // kernelx.dll, xgameruntime.dll, bink2.dll; XMemAlloc, XGameRuntimeInitialize, BinkOpen;
        // synth.dll; Alpha, Beta, Gamma; the forwarder
        assert_eq!(img.table_strings.len(), 11);
        for &rva in &img.table_strings {
            let off = img.file_offset(&s.file, rva).unwrap();
            assert_eq!(img.rva_of_offset(off), Some(rva));
            assert!(s.file[off].is_ascii_alphanumeric());
        }
        assert_eq!(img.rva_of_offset(s.file.len() + 1), None);
    }

    #[test]
    fn tls_callbacks() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        let tls = img.tls.unwrap();
        assert_eq!(tls.callbacks, vec![synth::TLS_CB]);
        assert_eq!(tls.index_va, synth::BASE + synth::TLS_INDEX as u64);
    }

    #[test]
    fn exception_table() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        assert_eq!(img.pdata.len(), 2);
        assert_eq!(img.pdata[1].begin, synth::F1);
        assert_eq!(img.read(&s.file, img.pdata[0].unwind, 1), Some(&[1u8][..]));
    }

    #[test]
    fn garbage_is_an_error_never_a_panic() {
        let s = synth::dll(synth::BASE);
        // every truncation of a valid image
        for n in 0..s.file.len() {
            let _ = Image::parse(&s.file[..n]);
        }
        // every single-byte corruption of the headers and tables, with a few values
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..200_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let mut f = s.file.clone();
            let at = (rng as usize >> 8) % f.len();
            f[at] = (rng & 0xff) as u8;
            let at2 = (rng as usize >> 32) % f.len();
            f[at2] ^= 0x80;
            let _ = Image::parse(&f);
        }
        assert!(matches!(Image::parse(b"MZ"), Err(PeError::Truncated("DOS header"))));
    }
}
