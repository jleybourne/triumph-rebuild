//! What a compiled program binds: the resource bindings in its `PSV0` part (the table the D3D12 runtime itself
//! checks a root signature against) and its input and output signatures (`ISG1`, `OSG1`).
//!
//! `PSV0` layout (DXC's `DxilPipelineStateValidation.h`): the runtime-info size and block, the resource count,
//! then (if any) the size of one binding record and the records: type, space, lower bound, upper bound
//! (and, from version 1, kind and flags).

use rebuild_shader::signature::{self, Element};
use rebuild_shader::{Container, FourCC};

/// `PSVResourceType`.
pub mod res {
    pub const SAMPLER: u32 = 1;
    pub const CBV: u32 = 2;
    pub const SRV_TYPED: u32 = 3;
    pub const SRV_RAW: u32 = 4;
    pub const SRV_STRUCTURED: u32 = 5;
    pub const UAV_TYPED: u32 = 6;
    pub const UAV_RAW: u32 = 7;
    pub const UAV_STRUCTURED: u32 = 8;
    pub const UAV_STRUCTURED_COUNTER: u32 = 9;
}

/// One binding: a register range in a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binding {
    pub kind: u32,
    pub space: u32,
    pub lower: u32,
    pub upper: u32,
}

impl Binding {
    pub fn is_srv(&self) -> bool {
        matches!(self.kind, res::SRV_TYPED | res::SRV_RAW | res::SRV_STRUCTURED)
    }

    pub fn describe(&self) -> String {
        let (c, what) = match self.kind {
            res::SAMPLER => ('s', "sampler"),
            res::CBV => ('b', "cbv"),
            k if (3..=5).contains(&k) => ('t', "srv"),
            k if (6..=9).contains(&k) => ('u', "uav"),
            _ => ('?', "unknown"),
        };
        if self.lower == self.upper {
            format!("{what} {c}{} space{}", self.lower, self.space)
        } else {
            format!("{what} {c}{}-{} space{}", self.lower, self.upper, self.space)
        }
    }
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| "PSV0 truncated".to_string())
}

/// The bindings of a `PSV0` part, in its order.
pub fn psv_bindings(psv: &[u8]) -> Result<Vec<Binding>, String> {
    let info = u32_at(psv, 0)? as usize;
    let mut at = 4 + info;
    let count = u32_at(psv, at)? as usize;
    at += 4;
    if count == 0 {
        return Ok(Vec::new());
    }
    let stride = u32_at(psv, at)? as usize;
    at += 4;
    if stride < 16 {
        return Err(format!("PSV0 binding record of {stride} bytes"));
    }
    if psv.len() < at + count * stride {
        return Err("PSV0 truncated".into());
    }
    (0..count)
        .map(|i| {
            let r = at + i * stride;
            Ok(Binding { kind: u32_at(psv, r)?, space: u32_at(psv, r + 4)?, lower: u32_at(psv, r + 8)?, upper: u32_at(psv, r + 12)? })
        })
        .collect()
}

/// A compiled program as the library needs it.
#[derive(Debug, Clone, Default)]
pub struct Compiled {
    pub bindings: Vec<Binding>,
    pub inputs: Vec<Element>,
    pub outputs: Vec<Element>,
}

/// Reads a compiled DXIL container.
pub fn read(dxil: &[u8]) -> Result<Compiled, String> {
    let c = Container::parse(dxil).map_err(|e| e.to_string())?;
    let psv = c.part(FourCC::PSV0).ok_or("no PSV0 part")?;
    let sig = |f: FourCC| c.part(f).map(|p| signature::parse(p.data).map_err(|e| e.to_string())).unwrap_or(Ok(Vec::new()));
    Ok(Compiled { bindings: psv_bindings(psv.data)?, inputs: sig(FourCC::ISG1)?, outputs: sig(FourCC::OSG1)? })
}

#[cfg(test)]
pub(crate) fn psv_part(bindings: &[Binding]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(24u32.to_le_bytes());
    v.extend([0u8; 24]);
    v.extend((bindings.len() as u32).to_le_bytes());
    if !bindings.is_empty() {
        v.extend(24u32.to_le_bytes());
        for b in bindings {
            for w in [b.kind, b.space, b.lower, b.upper, 0, 0] {
                v.extend(w.to_le_bytes());
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psv_round_trip() {
        let b = [
            Binding { kind: res::CBV, space: 0, lower: 0, upper: 0 },
            Binding { kind: res::SAMPLER, space: 100, lower: 0, upper: 2 },
            Binding { kind: res::SRV_RAW, space: 101, lower: 0, upper: 0 },
        ];
        assert_eq!(psv_bindings(&psv_part(&b)).unwrap(), b);
        assert_eq!(psv_bindings(&psv_part(&[])).unwrap(), vec![]);
        assert_eq!(b[1].describe(), "sampler s0-2 space100");
    }

    #[test]
    fn truncated_psv_is_an_error() {
        let b = [Binding { kind: res::CBV, space: 0, lower: 0, upper: 0 }];
        let p = psv_part(&b);
        assert!(psv_bindings(&p[..p.len() - 4]).is_err());
    }
}
