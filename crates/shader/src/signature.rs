//! DXBC signature parts (`ISG1`, `OSG1`, `PSG1`): a stage's inputs or outputs, by semantic.
//!
//! The layout is D3D's public container format: a count and an offset, then 32-byte elements
//! (stream, name offset, semantic index, system value, component type, register, mask, read/write mask,
//! two bytes of padding, minimum precision). Names are NUL-terminated strings in the same part.

use crate::ShaderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub stream: u32,
    pub name: String,
    pub index: u32,
    /// D3D_NAME: 0 undefined, 1 position, 6 vertex id, 7 primitive id, 8 instance id, 9 front face,
    /// 64 target, 65 depth, 66 coverage, ...
    pub system: u32,
    /// D3D_REGISTER_COMPONENT_TYPE: 1 uint32, 2 sint32, 3 float32 (4-6: their 16-bit forms).
    pub comp: u32,
    pub register: u32,
    /// Components the element occupies (bits 0-3: x, y, z, w).
    pub mask: u8,
    /// Components read (input) or never written (output, inverted as D3D stores it).
    pub rw: u8,
    pub min_precision: u32,
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, ShaderError> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or(ShaderError::Malformed("signature truncated"))
}

/// The elements of a signature part, in order. An empty part has none.
pub fn parse(part: &[u8]) -> Result<Vec<Element>, ShaderError> {
    if part.is_empty() {
        return Ok(Vec::new());
    }
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    (0..count)
        .map(|i| {
            let at = base + 32 * i;
            let name_at = u32_at(part, at + 4)? as usize;
            let name = part
                .get(name_at..)
                .and_then(|s| s.split(|&b| b == 0).next())
                .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
            Ok(Element {
                stream: u32_at(part, at)?,
                name: String::from_utf8_lossy(name).into_owned(),
                index: u32_at(part, at + 8)?,
                system: u32_at(part, at + 12)?,
                comp: u32_at(part, at + 16)?,
                register: u32_at(part, at + 20)?,
                mask: *part.get(at + 24).ok_or(ShaderError::Malformed("signature truncated"))?,
                rw: *part.get(at + 25).ok_or(ShaderError::Malformed("signature truncated"))?,
                min_precision: u32_at(part, at + 28)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a part the way D3D lays it out.
    fn part(elems: &[(&str, u32, u32, u32, u32, u8)]) -> Vec<u8> {
        let mut head = Vec::new();
        head.extend((elems.len() as u32).to_le_bytes());
        head.extend(8u32.to_le_bytes());
        let names_at = 8 + 32 * elems.len();
        let mut names = Vec::new();
        for (name, index, system, comp, reg, mask) in elems {
            let off = (names_at + names.len()) as u32;
            names.extend(name.as_bytes());
            names.push(0);
            for v in [0, off, *index, *system, *comp, *reg] {
                head.extend(v.to_le_bytes());
            }
            head.extend([*mask, *mask, 0, 0]);
            head.extend(0u32.to_le_bytes());
        }
        head.extend(names);
        head
    }

    #[test]
    fn reads_names_semantics_registers_and_masks() {
        let p = part(&[("SV_Position", 0, 1, 3, 0, 0xf), ("TEXCOORD", 3, 0, 3, 1, 0x3), ("SV_Target", 1, 64, 3, 1, 0xf)]);
        let e = parse(&p).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].name.as_str(), e[0].system, e[0].register, e[0].mask), ("SV_Position", 1, 0, 0xf));
        assert_eq!((e[1].name.as_str(), e[1].index, e[1].register, e[1].mask), ("TEXCOORD", 3, 1, 0x3));
        assert_eq!((e[2].system, e[2].index), (64, 1));
        assert!(parse(&[]).unwrap().is_empty());
        assert!(parse(&p[..20]).is_err(), "truncated");
    }
}
