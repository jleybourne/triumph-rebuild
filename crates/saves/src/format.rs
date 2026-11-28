//! The title's save blobs, as its own code writes and checks them (`docs/research/save-format.md`; every
//! offset and rule below carries the exe address it was read from).
//!
//! A save container holds two blobs, written in one update by `0x1428d1ea0`:
//!
//! ```text
//! "Save Game Header"  0x128 bytes   version 6, data size, data hash, header hash, stamp, display name
//! "Save Game Data"    data size     a 0x170-byte "ASAV" header, then the body
//! ```
//!
//! Three hashes guard it, all Jenkins one-at-a-time (`0x144dd1900`): the data hash over the whole data
//! blob, the header hash over six header fields chained (`0x1428d1930`), and the inner hash over the ASAV
//! header with its own hash field zeroed, seeded `0x4bf672ed` (`0x1428d0410`). Kinds 1 and 9 also carry an
//! encrypted body and a 32-byte signature made by a protected crypto library (`0x1428d4480`): those stay
//! read-only here, because nothing we can compute would make the title accept an edit.
//!
//! The parser is strict. It refuses what the title's writer never produces (a header of another size,
//! non-zero bytes after a name's terminator, ...) so that whatever it accepts serializes back to the
//! same bytes, and an edit changes only what it means to change.

use std::fmt;

/// The header blob's name (`0x14677e1da`).
pub const HEADER_BLOB: &str = "Save Game Header";
/// The data blob's name (`0x146785e6e`).
pub const DATA_BLOB: &str = "Save Game Data";
/// The header blob's size: written (`0x1428d1f99`) and required on load (`0x1428d18e9`, `0x1428d3513`).
pub const HEADER_SIZE: usize = 0x128;
/// The header's version: written (`0x1428d1ed8`) and required (`0x1428d1914`, `0x1428d3557`).
pub const HEADER_VERSION: u32 = 6;
/// The ASAV header at the start of the data (`0x1428d0a60` writes it, `0x1428d0450` reads it).
pub const INNER_SIZE: usize = 0x170;
/// The ASAV header's first four bytes (`0x145848299`: `0x256415341`, low half).
pub const INNER_MAGIC: [u8; 4] = *b"ASAV";
/// The ASAV header's format word as its constructor sets it (`0x145848299`, high half). Not checked on load.
pub const INNER_FORMAT: u32 = 2;
/// The seed of the ASAV header's own hash (`0x1428d0421`).
pub const INNER_HASH_SEED: u32 = 0x4bf6_72ed;
/// The smallest data blob the load path takes (`0x1428d3583`: `cmp edi, 0x171; jb fail`).
pub const MIN_DATA: usize = 0x171;
/// The signature after a signed body (`0x1414e78f0` returns 0x20).
pub const SIGNATURE_SIZE: usize = 0x20;
/// UTF-16 units in a display name, with its terminator (`0x1428d1ee5`, `0x1428bd160` copies at most 127).
pub const NAME_UNITS: usize = 0x80;
/// Bytes in the ASAV header's text field at 0x128, with its terminator (`0x1428d02e7`: at most 0x2a).
pub const TAG_BYTES: usize = 0x2b;

/// The word each kind must carry at ASAV +0x0c (`0x141b1d2b0`, table `0x1462fe8ac`), kinds 1-9.
const KIND_WORD: [u32; 9] = [3, 1, 1, 1, 1, 1, 1, 2, 1];

/// Jenkins one-at-a-time over `bytes` from `seed`, as `0x144dd1900` computes it: each byte is
/// sign-extended (`movsx`, `0x144dd1930`) before it is added, and the final mix runs on every call, so a
/// chained hash finalises between its parts.
pub fn joaat(bytes: &[u8], seed: u32) -> u32 {
    let mut h = seed;
    for &b in bytes {
        h = h.wrapping_add(b as i8 as i32 as u32);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h.wrapping_add(h << 15)
}

/// Kinds whose body is encrypted and signed (`0x1428d0d60`: `(kind - 1) & !8 == 0`).
pub fn kind_is_signed(kind: u32) -> bool {
    kind.wrapping_sub(1) & !8 == 0
}

/// The word ASAV +0x0c must hold for `kind`; `None` for a kind the title does not know (`0x1428d0a50`).
pub fn kind_word(kind: u32) -> Option<u32> {
    KIND_WORD.get(kind.wrapping_sub(1) as usize).copied()
}

/// Why a blob is not one this parser understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected(pub String);

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn no(s: impl Into<String>) -> Rejected {
    Rejected(s.into())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A UTF-16 name in `units` slots: text, terminator, zeros. The writer copies with a terminator and the
/// slots start zeroed (`0x1428d18cd`, `0x145848294`), so anything else is refused.
fn read_name(b: &[u8], units: usize, what: &str) -> Result<String, Rejected> {
    let all: Vec<u16> = b[..units * 2].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = all.iter().position(|&u| u == 0).ok_or_else(|| no(format!("{what}: no terminator")))?;
    if all[end..].iter().any(|&u| u != 0) {
        return Err(no(format!("{what}: bytes after the terminator")));
    }
    String::from_utf16(&all[..end]).map_err(|_| no(format!("{what}: not UTF-16")))
}

fn write_name(b: &mut [u8], units: usize, s: &str) {
    b[..units * 2].fill(0);
    for (i, u) in s.encode_utf16().enumerate() {
        b[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
}

/// A display name the writer could have produced: at most 127 UTF-16 units, no NUL.
pub fn check_name(s: &str) -> Result<(), Rejected> {
    if s.contains('\0') {
        return Err(no("a name holds no NUL"));
    }
    if s.encode_utf16().count() >= NAME_UNITS {
        return Err(no(format!("a name is at most {} UTF-16 units", NAME_UNITS - 1)));
    }
    Ok(())
}

fn read_tag(b: &[u8]) -> Result<String, Rejected> {
    let raw = &b[..TAG_BYTES];
    let end = raw.iter().position(|&c| c == 0).ok_or_else(|| no("ASAV text: no terminator"))?;
    if raw[end..].iter().any(|&c| c != 0) {
        return Err(no("ASAV text: bytes after the terminator"));
    }
    String::from_utf8(raw[..end].to_vec()).map_err(|_| no("ASAV text: not UTF-8"))
}

/// The "Save Game Header" blob (`0x1428d1ea0` fills it; `0x1428d18e0` and `0x1428d3410` check it).
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// +0x00, 6.
    pub version: u32,
    /// +0x04, the data blob's size (`0x1428d1ef1`; checked `0x1428d355e`).
    pub data_size: u32,
    /// +0x08, `joaat(data, 0)` (`0x1428d1f5b`; checked `0x1428d357d`).
    pub data_hash: u32,
    /// +0x0c, the chained header hash (`0x1428d1f7a`; checked `0x1428d1910`, `0x1428d3553`).
    pub header_hash: u32,
    /// +0x10, a 64-bit stamp from a clock behind a protected pointer (`0x1428d1f23`); also ASAV +0x20.
    pub stamp: u64,
    /// +0x18, the manager's +0x404 = ASAV +0x154 (`0x1428d1f30`).
    pub inner_154: u32,
    /// +0x1c, ASAV +0x18 (`0x1428d1ef4`).
    pub inner_18: u32,
    /// +0x20, ASAV +0x1c, or the data size when that is 0 (`0x1428d1efd`).
    pub inner_1c: u32,
    /// +0x24, 128 UTF-16 units: the display name, ASAV +0x28 (`0x1428d1f13`). The slot list shows it
    /// (`0x1428d16b9`).
    pub name: String,
}

impl Header {
    /// The six fields `0x1428d1930` chains, in its order.
    pub fn compute_hash(&self) -> u32 {
        let b = self.to_bytes();
        let mut h = joaat(&b[0x10..0x18], 0);
        h = joaat(&b[0x24..0x124], h);
        h = joaat(&b[0x18..0x1c], h);
        h = joaat(&b[0x1c..0x20], h);
        h = joaat(&b[0x20..0x24], h);
        joaat(&b[0x08..0x0c], h)
    }

    pub fn parse(b: &[u8]) -> Result<Header, Rejected> {
        if b.len() != HEADER_SIZE {
            return Err(no(format!("header is {} bytes, the title writes {HEADER_SIZE}", b.len())));
        }
        if b[0x124..].iter().any(|&c| c != 0) {
            return Err(no("header: bytes after the name"));
        }
        Ok(Header {
            version: u32_at(b, 0),
            data_size: u32_at(b, 4),
            data_hash: u32_at(b, 8),
            header_hash: u32_at(b, 0xc),
            stamp: u64_at(b, 0x10),
            inner_154: u32_at(b, 0x18),
            inner_18: u32_at(b, 0x1c),
            inner_1c: u32_at(b, 0x20),
            name: read_name(&b[0x24..], NAME_UNITS, "header name")?,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0; HEADER_SIZE];
        put32(&mut b, 0, self.version);
        put32(&mut b, 4, self.data_size);
        put32(&mut b, 8, self.data_hash);
        put32(&mut b, 0xc, self.header_hash);
        put64(&mut b, 0x10, self.stamp);
        put32(&mut b, 0x18, self.inner_154);
        put32(&mut b, 0x1c, self.inner_18);
        put32(&mut b, 0x20, self.inner_1c);
        write_name(&mut b[0x24..], NAME_UNITS, &self.name);
        b
    }
}

/// The ASAV header at the start of "Save Game Data" (`0x1428d0a60` writes it field by field from the
/// manager's +0x2b0; `0x1428d0450` reads it back with the same offsets).
#[derive(Debug, Clone, PartialEq)]
pub struct Inner {
    /// +0x04: 2 from the constructor; copied on load, not checked.
    pub format: u32,
    /// +0x08: the save's kind, 1-9 (`0x1428d04c6`). The title writes 1, 5 and 9 directly (`0x1428d03c9`,
    /// `0x142893a56`, `0x1428a01e9`) and others from request objects.
    pub kind: u32,
    /// +0x0c: must equal the kind's table word (`0x1428d04e9`).
    pub kind_word: u32,
    /// +0x10: flags; the writer sets 0x80 and 0x100 for the signed kinds (`0x1428d0abe`, `0x1428d0ad2`).
    pub flags: u32,
    /// +0x14: 0x170 when written through `0x1428d2052`. Not checked.
    pub header_size: u32,
    /// +0x18, +0x1c: copied to the outer header's +0x1c / +0x20.
    pub value_18: u32,
    pub value_1c: u32,
    /// +0x20: the same stamp as the outer header's +0x10 (`0x1428d204b`).
    pub stamp: u64,
    /// +0x28: 128 UTF-16 units, the display name (`0x1428d0300`).
    pub name: String,
    /// +0x128: up to 42 bytes of text and a terminator; +0x153 must be 0 (`0x1428d0946`).
    pub tag: String,
    /// +0x154 (also the outer header's +0x18), +0x158 (f32), +0x15c, +0x164 (f32): set by the kind's caller
    /// (`0x1428d03da` sets +0x154 and +0x158 for kind 1). Meaning not decoded; kept as they are.
    pub value_154: u32,
    pub value_158: u32,
    pub value_15c: u32,
    /// +0x160: `joaat(header with this field zeroed, 0x4bf672ed)` (`0x1428d0410`; checked `0x1428d0a2b`).
    pub hash: u32,
    pub value_164: u32,
    /// +0x168: the data blob's size (`0x1428d2072`). Not checked on load.
    pub size: u32,
}

impl Inner {
    pub fn parse(b: &[u8]) -> Result<Inner, Rejected> {
        if b.len() < INNER_SIZE {
            return Err(no(format!("data is {} bytes, shorter than its 0x170-byte header", b.len())));
        }
        if b[..4] != INNER_MAGIC {
            return Err(no("data does not start with ASAV"));
        }
        if b[0x16c..0x170].iter().any(|&c| c != 0) {
            // the title refuses these too (`0x1428d09e2`-`0x1428d0a0c`)
            return Err(no("ASAV +0x16c..0x170 not zero"));
        }
        Ok(Inner {
            format: u32_at(b, 4),
            kind: u32_at(b, 8),
            kind_word: u32_at(b, 0xc),
            flags: u32_at(b, 0x10),
            header_size: u32_at(b, 0x14),
            value_18: u32_at(b, 0x18),
            value_1c: u32_at(b, 0x1c),
            stamp: u64_at(b, 0x20),
            name: read_name(&b[0x28..], NAME_UNITS, "ASAV name")?,
            tag: read_tag(&b[0x128..])?,
            value_154: u32_at(b, 0x154),
            value_158: u32_at(b, 0x158),
            value_15c: u32_at(b, 0x15c),
            hash: u32_at(b, 0x160),
            value_164: u32_at(b, 0x164),
            size: u32_at(b, 0x168),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0; INNER_SIZE];
        b[..4].copy_from_slice(&INNER_MAGIC);
        put32(&mut b, 4, self.format);
        put32(&mut b, 8, self.kind);
        put32(&mut b, 0xc, self.kind_word);
        put32(&mut b, 0x10, self.flags);
        put32(&mut b, 0x14, self.header_size);
        put32(&mut b, 0x18, self.value_18);
        put32(&mut b, 0x1c, self.value_1c);
        put64(&mut b, 0x20, self.stamp);
        write_name(&mut b[0x28..], NAME_UNITS, &self.name);
        b[0x128..0x128 + self.tag.len()].copy_from_slice(self.tag.as_bytes());
        put32(&mut b, 0x154, self.value_154);
        put32(&mut b, 0x158, self.value_158);
        put32(&mut b, 0x15c, self.value_15c);
        put32(&mut b, 0x160, self.hash);
        put32(&mut b, 0x164, self.value_164);
        put32(&mut b, 0x168, self.size);
        b
    }

    /// `0x1428d0410`: the header with +0x160 zeroed, seeded `0x4bf672ed`.
    pub fn compute_hash(&self) -> u32 {
        let mut b = self.to_bytes();
        put32(&mut b, 0x160, 0);
        joaat(&b, INNER_HASH_SEED)
    }

    pub fn signed(&self) -> bool {
        kind_is_signed(self.kind)
    }
}

/// One save: the header blob and the data blob, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Save {
    pub header: Header,
    pub inner: Inner,
    /// The data after the ASAV header. For a signed kind: the encrypted body and its signature.
    pub body: Vec<u8>,
}

/// What the editor may change in a save, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editable {
    Yes,
    No(String),
}

impl Save {
    /// Both blobs, decoded strictly (see the module notes); checksums are not judged here (`problems`).
    pub fn parse(header: &[u8], data: &[u8]) -> Result<Save, Rejected> {
        let header = Header::parse(header)?;
        let inner = Inner::parse(data)?;
        if inner.tag.len() >= TAG_BYTES {
            return Err(no("ASAV text too long"));
        }
        let s = Save { header, inner, body: data[INNER_SIZE..].to_vec() };
        // strictness pays off here: anything accepted comes back as the same bytes
        debug_assert_eq!(s.data(), data);
        Ok(s)
    }

    /// The data blob's bytes.
    pub fn data(&self) -> Vec<u8> {
        let mut d = self.inner.to_bytes();
        d.extend_from_slice(&self.body);
        d
    }

    /// Everything the title's load path checks (`0x1428d3410`, `0x1428d18e0`, `0x1428d0450`), in its
    /// order; empty when it would take the save. The one check left out is the manager's buffer size
    /// (`0x1428d3564`, `[mgr+0x758]`), which is not known offline; the editor never grows a save.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        let data = self.data();
        if self.header.header_hash != self.header.compute_hash() {
            p.push(format!("header hash {:#010x}, should be {:#010x}", self.header.header_hash, self.header.compute_hash()));
        }
        if self.header.version != HEADER_VERSION {
            p.push(format!("header version {}, the title needs {HEADER_VERSION}", self.header.version));
        }
        if self.header.data_size as usize != data.len() {
            p.push(format!("header says {} data bytes, the blob has {}", self.header.data_size, data.len()));
        }
        let dh = joaat(&data, 0);
        if self.header.data_hash != dh {
            p.push(format!("data hash {:#010x}, should be {dh:#010x}", self.header.data_hash));
        }
        if data.len() < MIN_DATA {
            p.push(format!("data is {} bytes, the title needs at least {MIN_DATA}", data.len()));
        }
        match kind_word(self.inner.kind) {
            None => p.push(format!("kind {} is not one the title knows (1-9)", self.inner.kind)),
            Some(w) if w != self.inner.kind_word => p.push(format!("kind {} needs word {w} at +0x0c, has {}", self.inner.kind, self.inner.kind_word)),
            Some(_) => {}
        }
        if self.inner.hash != self.inner.compute_hash() {
            p.push(format!("ASAV hash {:#010x}, should be {:#010x}", self.inner.hash, self.inner.compute_hash()));
        }
        p
    }

    /// Fields the title writes in step but does not check on load: worth a warning, not a refusal.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        let n = (INNER_SIZE + self.body.len()) as u32;
        if self.inner.size != n {
            w.push(format!("ASAV size {} differs from the data's {n}", self.inner.size));
        }
        if self.inner.header_size != INNER_SIZE as u32 {
            w.push(format!("ASAV header size {:#x}, the writer puts 0x170", self.inner.header_size));
        }
        if self.inner.name != self.header.name {
            w.push("the header's and the ASAV's names differ".into());
        }
        if self.inner.stamp != self.header.stamp {
            w.push("the header's and the ASAV's stamps differ".into());
        }
        if self.header.inner_154 != self.inner.value_154 || self.header.inner_18 != self.inner.value_18 {
            w.push("header copies of ASAV fields differ".into());
        }
        w
    }

    pub fn editable(&self) -> Editable {
        if self.inner.signed() {
            return Editable::No(format!(
                "kind {} is encrypted and signed by the title's protected crypto library; any change would be refused on load",
                self.inner.kind
            ));
        }
        if !self.problems().is_empty() {
            return Editable::No("the title would not load this save as it is; fix nothing on top of a broken save".into());
        }
        Editable::Yes
    }

    /// Recomputes, in the writer's order, everything derived: the ASAV hash, then the data size and
    /// hash over the data, then the header hash (`0x1428d2030`, `0x1428d1f49`, `0x1428d1f6b`).
    pub fn reseal(&mut self) -> Result<(), Rejected> {
        if self.inner.signed() {
            return Err(no("a signed save cannot be resealed here"));
        }
        self.inner.size = (INNER_SIZE + self.body.len()) as u32;
        self.inner.hash = self.inner.compute_hash();
        let data = self.data();
        self.header.data_size = data.len() as u32;
        self.header.data_hash = joaat(&data, 0);
        self.header.header_hash = self.header.compute_hash();
        Ok(())
    }

    /// The display name, in both places the writer keeps it, resealed.
    pub fn set_name(&mut self, name: &str) -> Result<(), Rejected> {
        self.require_editable()?;
        check_name(name)?;
        self.header.name = name.to_string();
        self.inner.name = name.to_string();
        self.reseal()
    }

    /// Bytes of the body replaced in place (never grown or shrunk), resealed.
    pub fn patch_body(&mut self, at: usize, bytes: &[u8]) -> Result<(), Rejected> {
        self.require_editable()?;
        let end = at.checked_add(bytes.len()).filter(|&e| e <= self.body.len()).ok_or_else(|| no("patch runs past the body"))?;
        self.body[at..end].copy_from_slice(bytes);
        self.reseal()
    }

    fn require_editable(&self) -> Result<(), Rejected> {
        match self.editable() {
            Editable::Yes => Ok(()),
            Editable::No(why) => Err(no(why)),
        }
    }
}

/// A save as the title's writer makes one (`0x1428d1ea0` and `0x1428d2030`), for tests and for a fresh
/// container: kind, its table word, flags as the writer sets them, the stamp in both headers, the name in
/// both, the body after the ASAV header. Signed kinds get no signature (we cannot make one), so they are
/// refused.
pub fn build(kind: u32, name: &str, stamp: u64, tag: &str, body: &[u8]) -> Result<Save, Rejected> {
    let word = kind_word(kind).ok_or_else(|| no(format!("kind {kind} is not one the title knows")))?;
    if kind_is_signed(kind) {
        return Err(no("signed kinds need the title's signature"));
    }
    check_name(name)?;
    if tag.len() >= TAG_BYTES || tag.contains('\0') {
        return Err(no("ASAV text is at most 42 bytes"));
    }
    if INNER_SIZE + body.len() < MIN_DATA {
        return Err(no("the title needs a body of at least one byte"));
    }
    let n = (INNER_SIZE + body.len()) as u32;
    let inner = Inner {
        format: INNER_FORMAT,
        kind,
        kind_word: word,
        flags: 0,
        header_size: INNER_SIZE as u32,
        value_18: 0,
        // `0x1428d206c`: a zero +0x1c becomes the data size
        value_1c: n,
        stamp,
        name: name.into(),
        tag: tag.into(),
        value_154: 0,
        value_158: 0,
        value_15c: 0,
        hash: 0,
        value_164: 0,
        size: n,
    };
    let header = Header {
        version: HEADER_VERSION,
        data_size: 0,
        data_hash: 0,
        header_hash: 0,
        stamp,
        inner_154: inner.value_154,
        inner_18: inner.value_18,
        inner_1c: inner.value_1c,
        name: name.into(),
    };
    let mut s = Save { header, inner, body: body.to_vec() };
    s.reseal()?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published one-at-a-time values (seed 0): the empty string and "a" (Jenkins' own definition,
    /// e.g. en.wikipedia.org/wiki/Jenkins_hash_function: "a" = 0xca2e9442).
    #[test]
    fn joaat_matches_the_published_hash() {
        assert_eq!(joaat(b"", 0), 0);
        assert_eq!(joaat(b"a", 0), 0xca2e_9442);
        assert_eq!(joaat(b"The quick brown fox jumps over the lazy dog", 0), 0x519e_91f5);
    }

    /// `movsx`: a byte >= 0x80 is added as a negative number. Unsigned bytes would give another hash.
    #[test]
    fn joaat_sign_extends_bytes() {
        let signed = joaat(&[0x80], 0);
        let mut h: u32 = 0x80;
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        h = h.wrapping_add(h << 3);
        h ^= h >> 11;
        let unsigned = h.wrapping_add(h << 15);
        assert_ne!(signed, unsigned);
        // by hand: 0xffffff80 through one round and the final mix
        let mut g: u32 = 0xffff_ff80;
        g = g.wrapping_add(g << 10);
        g ^= g >> 6;
        g = g.wrapping_add(g << 3);
        g ^= g >> 11;
        assert_eq!(signed, g.wrapping_add(g << 15));
    }

    #[test]
    fn kinds() {
        assert!(kind_is_signed(1) && kind_is_signed(9));
        assert!((2..=8).all(|k| !kind_is_signed(k)));
        assert_eq!((1..=9).map(|k| kind_word(k).unwrap()).collect::<Vec<_>>(), vec![3, 1, 1, 1, 1, 1, 1, 2, 1]);
        assert_eq!(kind_word(0), None);
        assert_eq!(kind_word(10), None);
    }

    fn sample() -> Save {
        build(5, "Profile Settings", 0x0123_4567_89ab_cdef, "", &(0..200u32).map(|i| (i * 7) as u8).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_built_save_is_one_the_title_loads() {
        let s = sample();
        assert_eq!(s.problems(), Vec::<String>::new());
        assert_eq!(s.warnings(), Vec::<String>::new());
        assert_eq!(s.header.to_bytes().len(), HEADER_SIZE);
        assert_eq!(&s.data()[..4], b"ASAV");
        assert_eq!(s.data().len(), INNER_SIZE + 200);
    }

    #[test]
    fn parse_then_serialize_is_byte_identical() {
        let s = sample();
        let (h, d) = (s.header.to_bytes(), s.data());
        let back = Save::parse(&h, &d).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.header.to_bytes(), h);
        assert_eq!(back.data(), d);
    }

    #[test]
    fn edits_reseal_and_stay_loadable() {
        let mut s = sample();
        s.set_name("Renamed ✓").unwrap();
        assert_eq!(s.problems(), Vec::<String>::new());
        let before = s.header.header_hash;
        s.patch_body(10, &[0xff, 0xfe]).unwrap();
        assert_eq!(s.problems(), Vec::<String>::new());
        assert_eq!(s.body[10..12], [0xff, 0xfe]);
        assert_eq!(s.header.header_hash, s.header.compute_hash());
        assert_ne!(s.header.data_hash, 0);
        let _ = before;
        assert!(s.patch_body(199, &[1, 2]).is_err());
        assert!(s.set_name(&"x".repeat(128)).is_err());
        assert!(s.set_name(&"x".repeat(127)).is_ok());
    }

    /// Every check of the title's load path fires on the byte it guards.
    #[test]
    fn the_load_checks_catch_each_change() {
        let s = sample();
        let (h, d) = (s.header.to_bytes(), s.data());
        let flip = |b: &[u8], at: usize| {
            let mut b = b.to_vec();
            b[at] ^= 1;
            b
        };
        // data byte: data hash
        let t = Save::parse(&h, &flip(&d, 0x200)).unwrap();
        assert!(t.problems().iter().any(|p| p.starts_with("data hash")), "{:?}", t.problems());
        // ASAV field: ASAV hash and data hash
        let t = Save::parse(&h, &flip(&d, 0x154)).unwrap();
        assert!(t.problems().iter().any(|p| p.starts_with("ASAV hash")));
        // each header field the chain covers
        for at in [0x08, 0x10, 0x18, 0x1c, 0x20, 0x24] {
            let t = Save::parse(&flip(&h, at), &d).unwrap();
            assert!(t.problems().iter().any(|p| p.starts_with("header hash")), "{at:#x}");
        }
        // version and size are outside the chain but checked on their own
        let t = Save::parse(&flip(&h, 0), &d).unwrap();
        assert!(t.problems().iter().any(|p| p.starts_with("header version")));
        let t = Save::parse(&flip(&h, 4), &d).unwrap();
        assert!(t.problems().iter().any(|p| p.starts_with("header says")));
        // kind word
        let t = Save::parse(&h, &flip(&d, 0xc)).unwrap();
        assert!(t.problems().iter().any(|p| p.contains("needs word")));
    }

    #[test]
    fn the_parser_refuses_what_the_writer_never_makes() {
        let s = sample();
        let (h, d) = (s.header.to_bytes(), s.data());
        assert!(Save::parse(&h[..0x127], &d).is_err());
        let mut tail = h.clone();
        tail[0x125] = 1;
        assert!(Save::parse(&tail, &d).is_err());
        let mut after = h.clone();
        after[0x24 + 2 * 100] = b'x';
        assert!(Save::parse(&after, &d).is_err());
        let mut magic = d.clone();
        magic[0] = b'B';
        assert!(Save::parse(&h, &magic).is_err());
        assert!(Save::parse(&h, &d[..0x16f]).is_err());
        let mut z = d.clone();
        z[0x16d] = 1;
        assert!(Save::parse(&h, &z).is_err());
    }

    #[test]
    fn signed_kinds_are_read_only() {
        let mut s = sample();
        s.inner.kind = 1;
        s.inner.kind_word = 3;
        s.inner.flags = 0x180;
        s.inner.hash = s.inner.compute_hash();
        let d = s.data();
        s.header.data_hash = joaat(&d, 0);
        s.header.header_hash = s.header.compute_hash();
        assert!(s.problems().is_empty());
        assert!(matches!(s.editable(), Editable::No(_)));
        assert!(s.clone().set_name("x").is_err());
        assert!(s.clone().patch_body(0, &[0]).is_err());
        assert!(s.clone().reseal().is_err());
        assert!(build(1, "x", 0, "", &[0; 64]).is_err());
        assert!(build(9, "x", 0, "", &[0; 64]).is_err());
    }

    #[test]
    fn a_broken_save_is_not_edited() {
        let mut s = sample();
        s.header.data_hash ^= 1;
        assert!(matches!(s.editable(), Editable::No(_)));
        assert!(s.set_name("x").is_err());
    }
}
