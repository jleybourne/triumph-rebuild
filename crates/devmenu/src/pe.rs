//! Just enough PE reading for the scanners: section headers, RVA reads and the exception directory (`.pdata`),
//! over either the file layout (the image copy on disk) or the mapped layout (the image in memory, RVA = offset).

/// A PE image's bytes and section table.
pub struct Pe<'a> {
    data: &'a [u8],
    mapped: bool,
    pub image_base: u64,
    sections: Vec<Section>,
    pdata: (u32, u32),
}

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub name: [u8; 8],
    pub va: u32,
    pub vsize: u32,
    pub raw: u32,
    pub rsize: u32,
}

/// One `RUNTIME_FUNCTION`: begin and end RVAs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    pub begin: u32,
    pub end: u32,
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

impl<'a> Pe<'a> {
    /// Parse `data`. `mapped` is true when `data` is the image as mapped in memory (RVA = offset).
    pub fn parse(data: &'a [u8], mapped: bool) -> Option<Pe<'a>> {
        if data.get(0..2)? != b"MZ" {
            return None;
        }
        let pe = u32_at(data, 0x3c)? as usize;
        if data.get(pe..pe + 4)? != b"PE\0\0" {
            return None;
        }
        let nsec = u16_at(data, pe + 6)? as usize;
        let opt = u16_at(data, pe + 20)? as usize;
        let o = pe + 24;
        if u16_at(data, o)? != 0x20b {
            return None; // PE32+ only
        }
        let image_base = u64_at(data, o + 24)?;
        // data directory 3: exception
        let pdata = (u32_at(data, o + 112 + 3 * 8)?, u32_at(data, o + 112 + 3 * 8 + 4)?);
        let mut sections = Vec::with_capacity(nsec);
        for i in 0..nsec {
            let s = o + opt + 40 * i;
            let mut name = [0u8; 8];
            name.copy_from_slice(data.get(s..s + 8)?);
            sections.push(Section {
                name,
                vsize: u32_at(data, s + 8)?,
                va: u32_at(data, s + 12)?,
                rsize: u32_at(data, s + 16)?,
                raw: u32_at(data, s + 20)?,
            });
        }
        Some(Pe { data, mapped, image_base, sections, pdata })
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name.split(|&b| b == 0).next() == Some(name.as_bytes()))
    }

    /// `len` bytes at `rva`, if they lie in one section's initialized data (or anywhere, mapped).
    pub fn bytes(&self, rva: u32, len: usize) -> Option<&'a [u8]> {
        if self.mapped {
            return self.data.get(rva as usize..rva as usize + len);
        }
        let s = self.sections.iter().find(|s| rva >= s.va && rva < s.va + s.rsize.max(s.vsize))?;
        let off = (rva - s.va) as usize;
        if off + len > s.rsize as usize {
            return None;
        }
        self.data.get(s.raw as usize + off..s.raw as usize + off + len)
    }

    /// Every `RUNTIME_FUNCTION` in the exception directory, in RVA order.
    pub fn functions(&self) -> Vec<Function> {
        let (rva, size) = self.pdata;
        let Some(b) = self.bytes(rva, size as usize) else { return Vec::new() };
        b.chunks_exact(12)
            .map(|c| Function { begin: u32::from_le_bytes(c[0..4].try_into().unwrap()), end: u32::from_le_bytes(c[4..8].try_into().unwrap()) })
            .collect()
    }
}
