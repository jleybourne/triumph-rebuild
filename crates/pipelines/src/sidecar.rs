//! The translator's sampler sidecar, `<hash>.<stage>.samplers`, written beside each `<hash>.<stage>.dxil` (lane
//! shader-cf, `translate`): the immediate samplers' console words, so the library build need not lift the
//! corpus again.
//!
//! ```text
//! count u32 · count x 4 u32 (the console words, in sir_imm<k> order) · comparison mask u32 (bit k: sampler k
//! is read by SampleCmp*) · [optional] highest user-data dword read u32 (!0: none)      all little-endian
//! ```
//! The optional last word is proposed to the lead (`agents/pipelines.md`): with it the build checks the root
//! constants cover every read without lifting the corpus. Files without it parse as before.

use std::path::Path;

/// What a sidecar says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Samplers {
    pub words: Vec<[u32; 4]>,
    pub compare: u32,
    /// The highest user-data dword the program reads, when the sidecar says (`None`: not written, or none read).
    pub top_dword: Option<u32>,
}

/// Parses a sidecar's bytes; `None` when the length does not match its count.
pub fn parse(b: &[u8]) -> Option<Samplers> {
    let u = |at: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)) };
    let n = u(0)? as usize;
    let base = 4 + 16 * n + 4;
    if n > 1024 || (b.len() != base && b.len() != base + 4) {
        return None;
    }
    let words = (0..n).map(|i| [0, 1, 2, 3].map(|k| u(4 + 16 * i + 4 * k).unwrap())).collect();
    let top_dword = if b.len() == base + 4 { u(base).filter(|&d| d != u32::MAX) } else { None };
    Some(Samplers { words, compare: u(4 + 16 * n)?, top_dword })
}

/// The sidecar beside a DXIL file: `Ok(None)` when there is none (the caller falls back to lifting the
/// archive), `Err` when there is one and it is malformed.
pub fn read_beside(dxil: &Path) -> Result<Option<Samplers>, String> {
    let p = dxil.with_extension("samplers");
    match std::fs::read(&p) {
        Ok(b) => parse(&b).map(Some).ok_or_else(|| format!("{}: malformed sampler sidecar ({} bytes)", p.display(), b.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// A sidecar's bytes (for tests and tools).
pub fn encode(s: &Samplers) -> Vec<u8> {
    let mut v = (s.words.len() as u32).to_le_bytes().to_vec();
    for w in s.words.iter().flatten() {
        v.extend(w.to_le_bytes());
    }
    v.extend(s.compare.to_le_bytes());
    if let Some(d) = s.top_dword {
        v.extend(d.to_le_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecars_parse_and_fall_back() {
        let s = Samplers { words: vec![[1, 2, 3, 4], [5, 6, 7, 8]], compare: 0b10, top_dword: None };
        let b = encode(&s);
        assert_eq!(b.len(), 4 + 32 + 4);
        assert_eq!(parse(&b), Some(s.clone()));
        assert_eq!(parse(&b[..b.len() - 1]), None);
        let none = Samplers { words: vec![], compare: 0, top_dword: None };
        assert_eq!(parse(&encode(&none)), Some(none.clone()));
        let top = Samplers { top_dword: Some(14), ..s.clone() };
        assert_eq!(parse(&encode(&top)), Some(top), "the proposed top-dword word");
        let mut b2 = encode(&none);
        b2.extend(u32::MAX.to_le_bytes());
        assert_eq!(parse(&b2), Some(none), "!0: no user data read");

        let dir = std::env::temp_dir().join(format!("rebuild-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dxil = dir.join("0123456789abcdef.ps.dxil");
        assert_eq!(read_beside(&dxil), Ok(None), "absent: fall back to the archive");
        std::fs::write(dir.join("0123456789abcdef.ps.samplers"), &b).unwrap();
        assert_eq!(read_beside(&dxil), Ok(Some(s)));
        std::fs::write(dir.join("0123456789abcdef.ps.samplers"), [1, 0, 0, 0]).unwrap();
        assert!(read_beside(&dxil).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
