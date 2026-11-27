//! Lay an image out in memory at a chosen base, apply its relocations and bind its imports. Nothing in
//! the image is executed: no entry point, no TLS callback, no delay-load helper.

use crate::pe::{rel, Image, ImportModule, Symbol};
use std::fmt;

/// Supplies the address of an imported function. `dll` is the name as the image writes it.
pub trait Resolver {
    fn resolve(&mut self, dll: &str, symbol: &Symbol) -> Option<u64>;
}

/// A resolver that knows nothing yet: every import is missing.
pub struct NullResolver;

impl Resolver for NullResolver {
    fn resolve(&mut self, _: &str, _: &Symbol) -> Option<u64> {
        None
    }
}

/// An import the resolver could not supply. Its IAT slot is left as the file had it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub dll: String,
    pub symbol: Symbol,
    pub delay: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapReport {
    pub base: u64,
    pub delta: u64,
    pub relocs_applied: usize,
    pub imports_bound: usize,
    pub delay_bound: usize,
    pub missing: Vec<Missing>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapError {
    /// The destination is smaller than `SizeOfImage`.
    TooSmall { need: u32, have: usize },
    /// The base is not 64 KiB aligned.
    Unaligned(u64),
    /// The image must move but has no relocations (or they were stripped).
    NotRelocatable,
    /// A relocation type this loader does not apply.
    RelocType(u8),
    /// A relocation, section or IAT slot outside the image.
    OutOfImage(&'static str),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for MapError {}

/// What to bind.
#[derive(Debug, Clone, Copy)]
pub struct MapOptions {
    /// Bind delay-load imports now (their IAT slots otherwise point at the image's own thunks, which
    /// would call the title's delay-load helper).
    pub bind_delay: bool,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions { bind_delay: true }
    }
}

/// Maps `img` (parsed from `file`) into `out`, which stands for memory at `base`. `out` is zeroed
/// first, so section tails read as zero, as the loader guarantees.
pub fn map_into(
    img: &Image,
    file: &[u8],
    base: u64,
    out: &mut [u8],
    resolver: &mut dyn Resolver,
    opts: MapOptions,
) -> Result<MapReport, MapError> {
    let size = img.size_of_image as usize;
    if out.len() < size {
        return Err(MapError::TooSmall { need: img.size_of_image, have: out.len() });
    }
    if base & 0xffff != 0 {
        return Err(MapError::Unaligned(base));
    }
    let out = &mut out[..size];
    out.fill(0);

    let h = (img.size_of_headers as usize).min(file.len()).min(size);
    out[..h].copy_from_slice(&file[..h]);
    for s in &img.sections {
        let n = s.mapped_file_size() as usize;
        let (src, dst) = (s.file_offset as usize, s.rva as usize);
        let n = n.min(file.len().saturating_sub(src));
        if n == 0 {
            continue;
        }
        if dst.checked_add(n).is_none_or(|e| e > size) {
            return Err(MapError::OutOfImage("section"));
        }
        out[dst..dst + n].copy_from_slice(&file[src..src + n]);
    }

    let delta = base.wrapping_sub(img.image_base);
    let mut report = MapReport { base, delta, ..Default::default() };
    if delta != 0 {
        if img.relocs.is_empty() && img.characteristics & crate::pe::FILE_RELOCS_STRIPPED != 0 {
            return Err(MapError::NotRelocatable);
        }
        for r in &img.relocs {
            let at = r.rva as usize;
            let w = match r.kind {
                rel::DIR64 => 8,
                rel::HIGHLOW => 4,
                rel::HIGH | rel::LOW => 2,
                k => return Err(MapError::RelocType(k)),
            };
            let slot = out.get_mut(at..at + w).ok_or(MapError::OutOfImage("relocation"))?;
            match r.kind {
                rel::DIR64 => {
                    let x = u64::from_le_bytes(slot.try_into().unwrap()).wrapping_add(delta);
                    slot.copy_from_slice(&x.to_le_bytes());
                }
                rel::HIGHLOW => {
                    let x = u32::from_le_bytes(slot.try_into().unwrap()).wrapping_add(delta as u32);
                    slot.copy_from_slice(&x.to_le_bytes());
                }
                rel::HIGH => {
                    let x = u16::from_le_bytes([slot[0], slot[1]]).wrapping_add((delta >> 16) as u16);
                    slot.copy_from_slice(&x.to_le_bytes());
                }
                _ => {
                    let x = u16::from_le_bytes([slot[0], slot[1]]).wrapping_add(delta as u16);
                    slot.copy_from_slice(&x.to_le_bytes());
                }
            }
            report.relocs_applied += 1;
        }
    }

    let mut bind = |mods: &[ImportModule], delay: bool, report: &mut MapReport| -> Result<(), MapError> {
        for m in mods {
            for e in &m.entries {
                match resolver.resolve(&m.dll, &e.symbol) {
                    Some(addr) => {
                        let at = e.iat_rva as usize;
                        let slot = out.get_mut(at..at + 8).ok_or(MapError::OutOfImage("IAT slot"))?;
                        slot.copy_from_slice(&addr.to_le_bytes());
                        if delay {
                            report.delay_bound += 1;
                        } else {
                            report.imports_bound += 1;
                        }
                    }
                    None => report.missing.push(Missing { dll: m.dll.clone(), symbol: e.symbol.clone(), delay }),
                }
            }
        }
        Ok(())
    };
    bind(&img.imports, false, &mut report)?;
    if opts.bind_delay {
        bind(&img.delay_imports, true, &mut report)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth;
    use std::collections::HashMap;

    fn u64_at(m: &[u8], rva: u32) -> u64 {
        u64::from_le_bytes(m[rva as usize..rva as usize + 8].try_into().unwrap())
    }

    struct Table(HashMap<(String, String), u64>);
    impl Resolver for Table {
        fn resolve(&mut self, dll: &str, s: &Symbol) -> Option<u64> {
            self.0.get(&(dll.to_ascii_lowercase(), s.to_string())).copied()
        }
    }

    #[test]
    fn relocated_pointers_check_out() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        let mut mem = vec![0xaau8; img.size_of_image as usize];
        for base in [0x7ff6_1234_0000u64, synth::BASE, 0x1_0000, 0x0000_7000_0000_0000] {
            let r = map_into(&img, &s.file, base, &mut mem, &mut NullResolver, MapOptions::default()).unwrap();
            assert_eq!(r.relocs_applied, if base == synth::BASE { 0 } else { s.dir64.len() });
            // each pointer, followed as an RVA in the mapped image, reaches what it pointed at
            let p = u64_at(&mem, synth::P_STR) - base;
            assert_eq!(&mem[p as usize..p as usize + synth::STR_TEXT.len()], synth::STR_TEXT);
            assert_eq!(u64_at(&mem, synth::P_F1), base + synth::F1 as u64);
            assert_eq!(mem[(u64_at(&mem, synth::P_F1) - base) as usize], 0xc3);
            assert_eq!(u64_at(&mem, synth::P_SELF), base + synth::P_SELF as u64);
            assert_eq!(u32::from_le_bytes(mem[synth::PLAIN as usize..][..4].try_into().unwrap()), 0xdead_beef);
            // the TLS directory and callback array moved with the image
            let reparsed_cb = u64_at(&mem, (u64_at(&mem, 0x3040 + 24) - base) as u32);
            assert_eq!(reparsed_cb, base + synth::TLS_CB as u64);
            // the delay-load IAT is left pointing at the image's own (relocated) thunk when unresolved
            assert_eq!(u64_at(&mem, synth::DELAY_IAT), base + synth::DELAY_THUNK as u64);
            // the zero-fill tail of .data is zero
            assert!(mem[0x3200..0x3300].iter().all(|&b| b == 0));
            // unresolved imports are named, never silently filled
            assert_eq!(r.missing.len(), 4);
        }
    }

    #[test]
    fn imports_bind_through_the_resolver() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        let mut t = HashMap::new();
        t.insert(("kernelx.dll".into(), "XMemAlloc".into()), 0x1111);
        t.insert(("kernelx.dll".into(), "#5".into()), 0x2222);
        t.insert(("bink2.dll".into(), "BinkOpen".into()), 0x3333);
        let mut mem = vec![0u8; img.size_of_image as usize];
        let base = 0x2_0000_0000;
        let r = map_into(&img, &s.file, base, &mut mem, &mut Table(t), MapOptions::default()).unwrap();
        assert_eq!(u64_at(&mem, synth::IAT_KERNELX), 0x1111);
        assert_eq!(u64_at(&mem, synth::IAT_KERNELX + 8), 0x2222);
        assert_eq!(u64_at(&mem, synth::DELAY_IAT), 0x3333);
        assert_eq!((r.imports_bound, r.delay_bound), (2, 1));
        assert_eq!(r.missing, vec![Missing { dll: "xgameruntime.dll".into(), symbol: Symbol::Name { hint: 0, name: "XGameRuntimeInitialize".into() }, delay: false }]);
    }

    #[test]
    fn refusals() {
        let s = synth::dll(synth::BASE);
        let img = Image::parse(&s.file).unwrap();
        let mut small = vec![0u8; 0x100];
        assert!(matches!(map_into(&img, &s.file, 0x1_0000, &mut small, &mut NullResolver, MapOptions::default()), Err(MapError::TooSmall { .. })));
        let mut mem = vec![0u8; img.size_of_image as usize];
        assert_eq!(map_into(&img, &s.file, 0x1_0001, &mut mem, &mut NullResolver, MapOptions::default()), Err(MapError::Unaligned(0x1_0001)));
    }
}
