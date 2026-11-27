//! The pipeline library: one file, built offline from the translated programs, read at start.
//!
//! ```text
//! header   0x00 "RBPIPLIB" Â· 0x08 version Â· 0x0c user-data dwords Â· 0x10 programs Â· 0x14 layouts
//!          0x18 roots Â· 0x1c pairs Â· 0x20 content hash (32 bytes) Â· 0x40 tables
//! program  32 bytes, sorted by (hash, stage): hash u64, stage u32, layout u32, compute root u32 (or !0),
//!          DXIL offset u32, DXIL length u32, 0
//! layout   8 bytes: offset, length of its encoding (user data u8, shadow u8, samplers u16, 16 bytes each)
//! root     8 bytes: offset, length of a serialized root signature (version 1.1)
//! pair     12 bytes, sorted: vertex layout, pixel layout, root â€” every pair of layouts the library's
//!          vertex and pixel shaders use, so any VS + PS the title pairs has its root signature ready
//! blobs    16-byte aligned
//! ```
//!
//! The content hash covers everything after the header: BLAKE3 over the BLAKE3 of each 1 MiB chunk, so load
//! threads verify the chunks they read. Lookups binary-search the program table in place: nothing is built
//! at load beyond reading the file.

use std::path::Path;
use std::time::{Duration, Instant};

use rebuild_shader::blake3;

use crate::layout::{RootDesc, StageLayout};
use crate::Stage;

const MAGIC: &[u8; 8] = b"RBPIPLIB";
const VERSION: u32 = 1;
const HEADER: usize = 0x40;
const PROGRAM: usize = 32;
const CHUNK: usize = 1 << 20;
const NO_ROOT: u32 = u32::MAX;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn encode_layout(l: &StageLayout) -> Vec<u8> {
    let mut v = vec![u8::from(l.user_data), u8::from(l.shadow)];
    v.extend((l.samplers.len() as u16).to_le_bytes());
    for w in &l.samplers {
        for x in w {
            v.extend(x.to_le_bytes());
        }
    }
    v
}

fn decode_layout(b: &[u8]) -> Option<StageLayout> {
    let n = u16::from_le_bytes(b.get(2..4)?.try_into().ok()?) as usize;
    let words = b.get(4..4 + 16 * n)?;
    Some(StageLayout {
        user_data: b[0] != 0,
        shadow: b[1] != 0,
        samplers: words.chunks_exact(16).map(|c| [0, 1, 2, 3].map(|k| u32_at(c, 4 * k))).collect(),
    })
}

/// The hash over the content (everything after the header).
fn content_hash(content: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for c in content.chunks(CHUNK) {
        h.update(blake3::hash(c).as_bytes());
    }
    *h.finalize().as_bytes()
}

/// One program to store.
pub struct Input {
    pub hash: u64,
    pub stage: Stage,
    pub dxil: Vec<u8>,
    pub layout: StageLayout,
}

/// What a build produced, and what it cost.
#[derive(Debug, Default)]
pub struct BuildStats {
    pub programs: usize,
    pub layouts: usize,
    pub compute_roots: usize,
    pub pairs: usize,
    pub roots: usize,
    pub root_bytes: usize,
    pub dxil_bytes: usize,
    pub file_bytes: usize,
    pub serialize: Duration,
    pub write: Duration,
}

/// Builds a library file from programs. `serialize` turns a root signature into its blob (the host's
/// serializer, or a stand-in in tests); it runs on `threads` threads.
pub fn build<F>(path: &Path, user_dwords: u32, mut programs: Vec<Input>, threads: usize, serialize: F) -> Result<BuildStats, String>
where
    F: Fn(&RootDesc) -> Result<Vec<u8>, String> + Sync,
{
    programs.sort_by_key(|p| (p.hash, p.stage));
    if let Some(w) = programs.windows(2).find(|w| (w[0].hash, w[0].stage) == (w[1].hash, w[1].stage)) {
        return Err(format!("{:016x} {} stored twice", w[0].hash, w[0].stage.name()));
    }
    // distinct layouts, in a stable order
    let mut layouts: Vec<StageLayout> = programs.iter().map(|p| p.layout.clone()).collect();
    layouts.sort();
    layouts.dedup();
    let layout_of = |l: &StageLayout| layouts.binary_search(l).unwrap() as u32;
    let used = |s: Stage| {
        let mut v: Vec<u32> = programs.iter().filter(|p| p.stage == s).map(|p| layout_of(&p.layout)).collect();
        v.sort();
        v.dedup();
        v
    };
    let (cs, vs, ps) = (used(Stage::Cs), used(Stage::Vs), used(Stage::Ps));
    // the root signatures: one per compute layout, one per (vertex, pixel) layout pair
    let mut descs: Vec<RootDesc> = cs.iter().map(|&l| RootDesc::compute(&layouts[l as usize], user_dwords)).collect();
    let mut pairs: Vec<(u32, u32, u32)> = Vec::new();
    for &v in &vs {
        for &p in &ps {
            pairs.push((v, p, descs.len() as u32));
            descs.push(RootDesc::graphics(&layouts[v as usize], &layouts[p as usize], user_dwords));
        }
    }
    let t = Instant::now();
    let blobs: Vec<Result<Vec<u8>, String>> = parallel_map(&descs, threads, |d| d.check().and_then(|_| serialize(d)));
    let serialize_time = t.elapsed();
    let roots: Vec<Vec<u8>> = blobs.into_iter().enumerate().map(|(i, r)| r.map_err(|e| format!("root signature {i}: {e}"))).collect::<Result<_, _>>()?;

    let t = Instant::now();
    let tables = HEADER + PROGRAM * programs.len() + 8 * layouts.len() + 8 * roots.len() + 12 * pairs.len();
    let mut out = vec![0u8; tables];
    let put = |out: &mut Vec<u8>, b: &[u8]| -> (u32, u32) {
        out.resize(out.len().next_multiple_of(16), 0);
        let at = out.len();
        out.extend_from_slice(b);
        (at as u32, b.len() as u32)
    };
    let mut table = Vec::with_capacity(tables - HEADER);
    let mut dxil_bytes = 0;
    for p in &programs {
        let (off, len) = put(&mut out, &p.dxil);
        dxil_bytes += p.dxil.len();
        let l = layout_of(&p.layout);
        let root = if p.stage == Stage::Cs { cs.binary_search(&l).unwrap() as u32 } else { NO_ROOT };
        table.extend(p.hash.to_le_bytes());
        for w in [p.stage as u32, l, root, off, len, 0] {
            table.extend(w.to_le_bytes());
        }
    }
    for l in &layouts {
        let (off, len) = put(&mut out, &encode_layout(l));
        table.extend(off.to_le_bytes());
        table.extend(len.to_le_bytes());
    }
    let mut root_bytes = 0;
    for r in &roots {
        let (off, len) = put(&mut out, r);
        root_bytes += r.len();
        table.extend(off.to_le_bytes());
        table.extend(len.to_le_bytes());
    }
    for (v, p, r) in &pairs {
        for w in [*v, *p, *r] {
            table.extend(w.to_le_bytes());
        }
    }
    out[HEADER..tables].copy_from_slice(&table);
    if out.len() > u32::MAX as usize {
        return Err("library over 4 GiB".into());
    }
    out[0..8].copy_from_slice(MAGIC);
    for (k, w) in [VERSION, user_dwords, programs.len() as u32, layouts.len() as u32, roots.len() as u32, pairs.len() as u32].iter().enumerate() {
        out[8 + 4 * k..12 + 4 * k].copy_from_slice(&w.to_le_bytes());
    }
    let h = content_hash(&out[HEADER..]);
    out[0x20..0x40].copy_from_slice(&h);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    // written beside, then renamed over: a reader never sees half a library
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(BuildStats {
        programs: programs.len(),
        layouts: layouts.len(),
        compute_roots: cs.len(),
        pairs: pairs.len(),
        roots: roots.len(),
        root_bytes,
        dxil_bytes,
        file_bytes: out.len(),
        serialize: serialize_time,
        write: t.elapsed(),
    })
}

/// `f` over `items` on `threads` threads, results in order.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = threads.clamp(1, items.len().max(1));
    let per = items.len().div_ceil(threads);
    std::thread::scope(|s| {
        let hs: Vec<_> = items.chunks(per.max(1)).map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<R>>())).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

/// A program in the library.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub hash: u64,
    pub stage: Stage,
    /// The compiled DXIL container, 16-byte aligned.
    pub dxil: &'a [u8],
    /// The layout index (graphics root signatures are found by the vertex and pixel layouts).
    pub layout: u32,
    /// A compute program's serialized root signature.
    pub root: Option<&'a [u8]>,
}

/// How a load went.
#[derive(Debug, Clone, Copy)]
pub struct LoadStats {
    pub bytes: usize,
    pub threads: usize,
    pub read: Duration,
    pub verified: bool,
    pub total: Duration,
}

/// A loaded library.
pub struct Library {
    data: Vec<u8>,
    programs: usize,
    layouts: usize,
    roots: usize,
    pairs: usize,
    user_dwords: u32,
}

#[cfg(windows)]
fn read_at(f: &std::fs::File, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, at)
}

#[cfg(unix)]
fn read_at(f: &std::fs::File, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, at)
}

fn read_exact_at(f: &std::fs::File, mut buf: &mut [u8], mut at: u64) -> Result<(), String> {
    while !buf.is_empty() {
        let n = read_at(f, buf, at).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("library shorter than its size".into());
        }
        buf = &mut buf[n..];
        at += n as u64;
    }
    Ok(())
}

impl Library {
    /// Reads a library on `threads` threads (each reads, and if `verify`, hashes, its own run of 1 MiB
    /// chunks), then checks its header and tables. A library that fails any check is refused whole.
    pub fn load(path: &Path, threads: usize, verify: bool) -> Result<(Library, LoadStats), String> {
        let t0 = Instant::now();
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = f.metadata().map_err(|e| e.to_string())?.len() as usize;
        if len < HEADER {
            return Err("library shorter than its header".into());
        }
        let mut data = vec![0u8; len];
        let (head, content) = data.split_at_mut(HEADER);
        read_exact_at(&f, head, 0)?;
        let chunks = content.len().div_ceil(CHUNK);
        let threads = threads.clamp(1, chunks.max(1));
        let per = chunks.div_ceil(threads).max(1);
        let hashes: Vec<Result<Vec<[u8; 32]>, String>> = std::thread::scope(|s| {
            let hs: Vec<_> = content
                .chunks_mut(per * CHUNK)
                .enumerate()
                .map(|(k, part)| {
                    let f = &f;
                    s.spawn(move || {
                        read_exact_at(f, part, (HEADER + k * per * CHUNK) as u64)?;
                        Ok(if verify { part.chunks(CHUNK).map(|c| *blake3::hash(c).as_bytes()).collect() } else { Vec::new() })
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let read = t0.elapsed();
        let mut all = Vec::with_capacity(chunks);
        for h in hashes {
            all.extend(h?);
        }
        if &data[0..8] != MAGIC || u32_at(&data, 8) != VERSION {
            return Err("not a pipeline library of this version".into());
        }
        if verify {
            let mut h = blake3::Hasher::new();
            for c in &all {
                h.update(c);
            }
            if h.finalize().as_bytes() != &data[0x20..0x40] {
                return Err("library content hash mismatch".into());
            }
        }
        let lib = Library {
            user_dwords: u32_at(&data, 0x0c),
            programs: u32_at(&data, 0x10) as usize,
            layouts: u32_at(&data, 0x14) as usize,
            roots: u32_at(&data, 0x18) as usize,
            pairs: u32_at(&data, 0x1c) as usize,
            data,
        };
        lib.check_tables()?;
        Ok((lib, LoadStats { bytes: len, threads, read, verified: verify, total: t0.elapsed() }))
    }

    fn layouts_at(&self) -> usize {
        HEADER + PROGRAM * self.programs
    }
    fn roots_at(&self) -> usize {
        self.layouts_at() + 8 * self.layouts
    }
    fn pairs_at(&self) -> usize {
        self.roots_at() + 8 * self.roots
    }

    fn blob(&self, off: u32, len: u32) -> &[u8] {
        &self.data[off as usize..off as usize + len as usize]
    }

    /// Every offset and index in bounds, programs sorted: after this, lookups cannot panic.
    fn check_tables(&self) -> Result<(), String> {
        let end = self.pairs_at() + 12 * self.pairs;
        if end > self.data.len() {
            return Err("library tables out of bounds".into());
        }
        let span = |at: usize| {
            let (o, l) = (u32_at(&self.data, at) as usize, u32_at(&self.data, at + 4) as usize);
            (o >= end && o + l <= self.data.len()).then_some(()).ok_or_else(|| format!("blob at table offset {at:#x} out of bounds"))
        };
        let mut last = None;
        for i in 0..self.programs {
            let at = HEADER + PROGRAM * i;
            let key = (u64_at(&self.data, at), u32_at(&self.data, at + 8));
            if Stage::from_u32(key.1).is_none() || last.is_some_and(|l| l >= key) {
                return Err(format!("program {i} out of order or of no stage"));
            }
            last = Some(key);
            let (l, r) = (u32_at(&self.data, at + 12), u32_at(&self.data, at + 16));
            if l as usize >= self.layouts || (r != NO_ROOT && r as usize >= self.roots) {
                return Err(format!("program {i} refers past its tables"));
            }
            span(at + 20)?;
        }
        for i in 0..self.layouts {
            span(self.layouts_at() + 8 * i)?;
            if self.layout(i as u32).is_none() {
                return Err(format!("layout {i} unreadable"));
            }
        }
        for i in 0..self.roots {
            span(self.roots_at() + 8 * i)?;
        }
        for i in 0..self.pairs {
            let at = self.pairs_at() + 12 * i;
            if u32_at(&self.data, at + 8) as usize >= self.roots {
                return Err(format!("pair {i} refers past the roots"));
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.programs
    }

    pub fn is_empty(&self) -> bool {
        self.programs == 0
    }

    /// Root-constant dwords per stage the root signatures were built with.
    pub fn user_dwords(&self) -> u32 {
        self.user_dwords
    }

    fn entry(&self, i: usize) -> Entry<'_> {
        let at = HEADER + PROGRAM * i;
        let root = u32_at(&self.data, at + 16);
        Entry {
            hash: u64_at(&self.data, at),
            stage: Stage::from_u32(u32_at(&self.data, at + 8)).unwrap(),
            layout: u32_at(&self.data, at + 12),
            root: (root != NO_ROOT).then(|| self.root(root as usize)),
            dxil: self.blob(u32_at(&self.data, at + 20), u32_at(&self.data, at + 24)),
        }
    }

    /// The program with this hash and stage.
    pub fn get(&self, hash: u64, stage: Stage) -> Option<Entry<'_>> {
        let key = (hash, stage as u32);
        let (mut lo, mut hi) = (0, self.programs);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = HEADER + PROGRAM * mid;
            match (u64_at(&self.data, at), u32_at(&self.data, at + 8)).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.entry(mid)),
            }
        }
        None
    }

    /// The root signature for a vertex and pixel shader pair.
    pub fn graphics_root(&self, vs: &Entry, ps: &Entry) -> Option<&[u8]> {
        let key = (vs.layout, ps.layout);
        let (mut lo, mut hi) = (0, self.pairs);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = self.pairs_at() + 12 * mid;
            match (u32_at(&self.data, at), u32_at(&self.data, at + 4)).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.root(u32_at(&self.data, at + 8) as usize)),
            }
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = Entry<'_>> {
        (0..self.programs).map(|i| self.entry(i))
    }

    /// Every serialized root signature.
    pub fn roots(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.roots).map(|i| self.root(i))
    }

    fn root(&self, i: usize) -> &[u8] {
        let at = self.roots_at() + 8 * i;
        self.blob(u32_at(&self.data, at), u32_at(&self.data, at + 4))
    }

    pub fn layout(&self, i: u32) -> Option<StageLayout> {
        let at = self.layouts_at() + 8 * i as usize;
        decode_layout(self.blob(u32_at(&self.data, at), u32_at(&self.data, at + 4)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in serializer: the description's debug text.
    fn fake(d: &RootDesc) -> Result<Vec<u8>, String> {
        Ok(format!("{d:?}").into_bytes())
    }

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rebuild-pipelines-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> Vec<Input> {
        let s = |n: usize| StageLayout { user_data: n > 0, shadow: n > 1, samplers: vec![[n as u32, 15 << 20, 0, 0]; n] };
        vec![
            Input { hash: 0x30, stage: Stage::Cs, dxil: vec![1; 100], layout: s(1) },
            Input { hash: 0x10, stage: Stage::Vs, dxil: vec![2; 33], layout: s(0) },
            Input { hash: 0x10, stage: Stage::Ps, dxil: vec![3; 2 * CHUNK + 7], layout: s(2) },
            Input { hash: 0x20, stage: Stage::Ps, dxil: vec![4; 5], layout: s(1) },
            Input { hash: 0x40, stage: Stage::Cs, dxil: vec![5; 9], layout: s(1) },
        ]
    }

    #[test]
    fn store_load_round_trip() {
        let path = dir().join("round.lib");
        let st = build(&path, 16, sample(), 3, fake).unwrap();
        assert_eq!((st.programs, st.layouts, st.compute_roots, st.pairs, st.roots), (5, 3, 1, 2, 3));
        for threads in [1, 2, 8] {
            let (lib, ls) = Library::load(&path, threads, true).unwrap();
            assert!(ls.verified);
            assert_eq!((lib.len(), lib.user_dwords()), (5, 16));
            let ps = lib.get(0x10, Stage::Ps).unwrap();
            assert_eq!(ps.dxil, &vec![3u8; 2 * CHUNK + 7][..]);
            assert_eq!(ps.dxil.as_ptr() as usize % 16, lib.data.as_ptr() as usize % 16, "blobs keep 16-byte alignment");
            assert!(ps.root.is_none());
            let vs = lib.get(0x10, Stage::Vs).unwrap();
            assert_eq!(vs.dxil, &[2u8; 33][..]);
            let want = fake(&RootDesc::graphics(&lib.layout(vs.layout).unwrap(), &lib.layout(ps.layout).unwrap(), 16)).unwrap();
            assert_eq!(lib.graphics_root(&vs, &ps).unwrap(), &want[..]);
            let cs = lib.get(0x40, Stage::Cs).unwrap();
            assert_eq!(cs.root.unwrap(), &fake(&RootDesc::compute(&lib.layout(cs.layout).unwrap(), 16)).unwrap()[..]);
            assert_eq!(lib.get(0x30, Stage::Cs).unwrap().root, cs.root, "one root per distinct layout");
            assert!(lib.get(0x10, Stage::Cs).is_none() && lib.get(0x11, Stage::Vs).is_none());
            assert_eq!(lib.iter().map(|e| e.hash).collect::<Vec<_>>(), [0x10, 0x10, 0x20, 0x30, 0x40]);
            assert_eq!(lib.layout(ps.layout).unwrap().samplers.len(), 2);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupted_library_is_refused() {
        let path = dir().join("corrupt.lib");
        build(&path, 16, sample(), 1, fake).unwrap();
        let mut b = std::fs::read(&path).unwrap();
        let n = b.len();
        b[n - 3] ^= 1;
        std::fs::write(&path, &b).unwrap();
        assert_eq!(Library::load(&path, 4, true).err().unwrap(), "library content hash mismatch");
        // unverified loads still check the tables
        b[HEADER + 12] = 0xff;
        std::fs::write(&path, &b).unwrap();
        assert!(Library::load(&path, 4, false).is_err());
        std::fs::write(&path, &b[..40]).unwrap();
        assert!(Library::load(&path, 1, false).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn duplicates_and_bad_roots_are_refused() {
        let path = dir().join("dup.lib");
        let mut p = sample();
        p.push(Input { hash: 0x20, stage: Stage::Ps, dxil: vec![], layout: StageLayout::default() });
        assert!(build(&path, 16, p, 1, fake).unwrap_err().contains("stored twice"));
        // 32 + 32 + 2 dwords is over the host's limit: caught before serializing
        assert!(build(&path, 32, sample(), 1, fake).unwrap_err().contains("limit 64"));
    }
}
