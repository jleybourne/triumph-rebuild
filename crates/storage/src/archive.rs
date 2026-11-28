//! A small ZIP reader and writer for export and import: deflate or stored entries, UTF-8 names, no
//! encryption, no ZIP64 (an export is far below 4 GiB: a user's saves have a 256 MB quota). Every entry's
//! CRC-32 and size are checked on read; a name that could leave the folder it is extracted to is refused.

use std::io::{Read, Write};
use std::path::Path;

/// The most an archive may hold, decompressed (refused beyond: a zip bomb, or not ours).
pub const MAX_TOTAL: u64 = 2 << 30;
const MAX_ENTRIES: usize = 65_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `/`-separated relative path
    pub name: String,
    pub data: Vec<u8>,
}

/// A safe entry name: relative, `/`-separated, no `..`, `.`, empty part, drive or backslash.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 1024
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains('\0')
        && name.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

fn dos_time(secs: u64) -> (u16, u16) {
    let (y, mo, d, h, mi, s) = crate::utc(secs);
    let y = (y.clamp(1980, 2107) - 1980) as u16;
    ((h as u16) << 11 | (mi as u16) << 5 | (s as u16 / 2), y << 9 | (mo as u16) << 5 | d as u16)
}

/// The archive's bytes. Entries are deflated unless that does not make them smaller.
pub fn build(entries: &[Entry], secs: u64) -> Result<Vec<u8>, String> {
    if entries.len() > MAX_ENTRIES {
        return Err(format!("{} entries: more than a zip without ZIP64 holds", entries.len()));
    }
    let (time, date) = dos_time(secs);
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        if !safe_name(&e.name) {
            return Err(format!("{:?}: not a safe entry name", e.name));
        }
        if e.data.len() as u64 >= u32::MAX as u64 || out.len() as u64 >= u32::MAX as u64 {
            return Err(format!("{}: too large for a zip without ZIP64", e.name));
        }
        let crc = crc32fast::hash(&e.data);
        let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&e.data).map_err(|x| x.to_string())?;
        let deflated = z.finish().map_err(|x| x.to_string())?;
        let (method, body): (u16, &[u8]) = if deflated.len() < e.data.len() { (8, &deflated) } else { (0, &e.data) };
        let offset = out.len() as u32;
        let name = e.name.as_bytes();
        let head = |sig: u32, central: bool| {
            let mut h = Vec::with_capacity(46 + name.len());
            h.extend_from_slice(&sig.to_le_bytes());
            if central {
                h.extend_from_slice(&20u16.to_le_bytes()); // made by
            }
            h.extend_from_slice(&20u16.to_le_bytes()); // needed
            h.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
            h.extend_from_slice(&method.to_le_bytes());
            h.extend_from_slice(&time.to_le_bytes());
            h.extend_from_slice(&date.to_le_bytes());
            h.extend_from_slice(&crc.to_le_bytes());
            h.extend_from_slice(&(body.len() as u32).to_le_bytes());
            h.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // extra
            if central {
                h.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
                h.extend_from_slice(&0u32.to_le_bytes()); // external attributes
                h.extend_from_slice(&offset.to_le_bytes());
            }
            h.extend_from_slice(name);
            h
        };
        out.extend_from_slice(&head(0x0403_4b50, false));
        out.extend_from_slice(body);
        central.extend_from_slice(&head(0x0201_4b50, true));
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// Write an archive atomically.
pub fn write(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let bytes = build(entries, crate::now_unix())?;
    crate::write_atomic(path, &bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn u16_at(b: &[u8], i: usize) -> Result<u16, String> {
    b.get(i..i + 2).map(|x| u16::from_le_bytes([x[0], x[1]])).ok_or_else(|| "truncated archive".to_string())
}
fn u32_at(b: &[u8], i: usize) -> Result<u32, String> {
    b.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])).ok_or_else(|| "truncated archive".to_string())
}

/// Every entry (directories, names ending in `/`, are skipped). Any damage is an error.
pub fn parse(b: &[u8]) -> Result<Vec<Entry>, String> {
    let min = b.len().saturating_sub(22 + 0xffff);
    let eocd = (min..=b.len().saturating_sub(22)).rev().find(|&i| b[i..].starts_with(&0x0605_4b50u32.to_le_bytes())).ok_or("not a zip archive (no end record)")?;
    let count = u16_at(b, eocd + 10)? as usize;
    let mut at = u32_at(b, eocd + 16)? as usize;
    let mut out: Vec<Entry> = Vec::with_capacity(count);
    let mut total: u64 = 0;
    for _ in 0..count {
        if u32_at(b, at)? != 0x0201_4b50 {
            return Err("damaged central directory".into());
        }
        let flags = u16_at(b, at + 8)?;
        let method = u16_at(b, at + 10)?;
        let crc = u32_at(b, at + 16)?;
        let csize = u32_at(b, at + 20)? as usize;
        let usize_ = u32_at(b, at + 24)? as u64;
        let (nlen, xlen, clen) = (u16_at(b, at + 28)? as usize, u16_at(b, at + 30)? as usize, u16_at(b, at + 32)? as usize);
        let local = u32_at(b, at + 42)? as usize;
        let name = b.get(at + 46..at + 46 + nlen).ok_or("truncated archive")?;
        let name = String::from_utf8(name.to_vec()).map_err(|_| "an entry name is not UTF-8")?;
        at += 46 + nlen + xlen + clen;
        if flags & 1 != 0 {
            return Err(format!("{name}: encrypted entries are not supported"));
        }
        if name.ends_with('/') {
            continue;
        }
        if !safe_name(&name) {
            return Err(format!("{name:?}: an unsafe entry name"));
        }
        if u32_at(b, local)? != 0x0403_4b50 {
            return Err(format!("{name}: damaged local header"));
        }
        let start = local + 30 + u16_at(b, local + 26)? as usize + u16_at(b, local + 28)? as usize;
        let body = b.get(start..start + csize).ok_or_else(|| format!("{name}: truncated"))?;
        total += usize_;
        if total > MAX_TOTAL {
            return Err("the archive is larger than an export can be".into());
        }
        let data = match method {
            0 => body.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(usize_ as usize);
                flate2::read::DeflateDecoder::new(body).take(usize_ + 1).read_to_end(&mut v).map_err(|e| format!("{name}: {e}"))?;
                v
            }
            m => return Err(format!("{name}: compression method {m} is not supported")),
        };
        if data.len() as u64 != usize_ || crc32fast::hash(&data) != crc {
            return Err(format!("{name}: damaged (size or CRC-32 does not match)"));
        }
        if out.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("{name}: twice in the archive"));
        }
        out.push(Entry { name, data });
    }
    Ok(out)
}

pub fn read(path: &Path) -> Result<Vec<Entry>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_TOTAL {
        return Err(format!("{}: larger than an export can be", path.display()));
    }
    parse(&std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_deflated_and_stored() {
        let entries = vec![
            Entry { name: "manifest.toml".into(), data: b"format = 1\n".repeat(40) },
            Entry { name: "saves/0009000000000001/scid/a.gsc".into(), data: (0..5000u32).map(|i| (i * 2654435761u32 >> 24) as u8).collect() },
            Entry { name: "empty".into(), data: Vec::new() },
        ];
        let z = build(&entries, 1_795_900_000).unwrap();
        assert_eq!(parse(&z).unwrap(), entries);
    }

    #[test]
    fn damage_and_unsafe_names_are_refused() {
        let mut x = 1u32;
        let noise: Vec<u8> = (0..300)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (x >> 16) as u8
            })
            .collect();
        let entries = vec![Entry { name: "a.bin".into(), data: noise }];
        let z = build(&entries, 0).unwrap();
        // flip a byte of the body (header 30 + name 5): the CRC no longer matches
        let mut bad = z.clone();
        bad[30 + 5 + 100] ^= 0xff;
        assert!(parse(&bad).is_err());
        assert!(parse(&z[..z.len() - 30]).is_err(), "truncated");
        assert!(parse(b"not a zip").is_err());
        for n in ["../x", "/abs", "a\\b", "c:x", "a//b", "./a", ""] {
            assert!(!safe_name(n), "{n}");
            assert!(build(&[Entry { name: n.into(), data: vec![] }], 0).is_err());
        }
        // a hand-made archive with an unsafe name is refused on read too
        let mut evil = build(&[Entry { name: "xx/evil".into(), data: vec![1] }], 0).unwrap();
        for i in 0..evil.len() - 7 {
            if &evil[i..i + 7] == b"xx/evil" {
                evil[i..i + 3].copy_from_slice(b"../");
            }
        }
        assert!(parse(&evil).unwrap_err().contains("unsafe"));
    }
}
