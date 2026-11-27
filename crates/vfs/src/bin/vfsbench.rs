//! V4 benchmark: open + size + read + close on 10,000 small files through the overlay file system,
//! against the OS floor (`File::open` + `read_to_end` on the same host files).
//!
//! The files are synthetic, written to a temp folder: a base of 100 directories x 100 files
//! (512 B - 4 KiB), two mods replacing 1,000 files each, and 1,000 files rewritten into the
//! overlay, so lookups land in every layer. Console paths are given in a different case from the
//! disk's. Every read is checked against the expected bytes.
//!
//! cargo run --release -p rebuild-vfs --bin vfsbench [-- --rounds N]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rebuild_vfs::{Disposition, Layer, ModSet, OpenOptions, Vfs, VfsBuilder};

const DIRS: usize = 100;
const PER_DIR: usize = 100;

fn body(d: usize, f: usize, tag: u8) -> Vec<u8> {
    let len = 512 + (d * 131 + f * 37) % 3585;
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(tag).wrapping_add(f as u8)).collect()
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// One pass: every file opened, sized, read whole, closed. Returns (seconds, bytes, wrong).
fn pass(v: &Vfs, files: &[(String, Vec<u8>)], buf: &mut Vec<u8>) -> (f64, u64, usize) {
    let t = Instant::now();
    let (mut bytes, mut wrong) = (0u64, 0usize);
    for (path, want) in files {
        let h = v.open(path, OpenOptions::read()).unwrap();
        let n = v.size(h).unwrap() as usize;
        buf.resize(n, 0);
        let got = v.read_at(h, 0, buf).unwrap();
        v.close(h).unwrap();
        bytes += got as u64;
        wrong += usize::from(&buf[..got] != want.as_slice());
    }
    (t.elapsed().as_secs_f64(), bytes, wrong)
}

fn main() {
    let rounds: usize = std::env::args()
        .skip_while(|a| a != "--rounds")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(7);
    let root: PathBuf = std::env::temp_dir().join(format!("rebuild-vfsbench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let base = root.join("game");
    let mods_dir = root.join("mods");

    // expected bytes per file, and which layer should serve it
    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(DIRS * PER_DIR);
    let mut want_layer = Vec::with_capacity(DIRS * PER_DIR);
    for d in 0..DIRS {
        for f in 0..PER_DIR {
            let b = body(d, f, 0);
            write(&base.join(format!("Dir{d:03}")).join(format!("File{f:03}.bin")), &b);
            let mut want = b;
            let mut layer = Layer::Base;
            if f % 10 == 1 {
                want = body(d, f, 1);
                write(&mods_dir.join("m1").join(format!("dir{d:03}")).join(format!("file{f:03}.bin")), &want);
                layer = Layer::Mod("m1".into());
            }
            if f % 10 == 2 || f % 20 == 1 {
                want = body(d, f, 2);
                write(&mods_dir.join("m2").join(format!("DIR{d:03}")).join(format!("FILE{f:03}.BIN")), &want);
                layer = Layer::Mod("m2".into());
            }
            files.push((format!(r"G:\dir{d:03}\FILE{f:03}.Bin"), want));
            want_layer.push(layer);
        }
    }
    let mods = ModSet::discover(&mods_dir).unwrap();
    let conflicts = mods.conflicts().unwrap().len();
    let overlay = root.join("overlay");
    {
        // 1,000 files rewritten into the overlay
        let v = VfsBuilder::new(&overlay).package('G', &base, &mods).build().unwrap();
        for (i, (path, want)) in files.iter_mut().enumerate() {
            if i % 10 == 3 {
                *want = body(i / PER_DIR, i % PER_DIR, 3);
                let h = v.open(path, OpenOptions::write(Disposition::CreateAlways)).unwrap();
                v.write_at(h, 0, want).unwrap();
                v.close(h).unwrap();
                want_layer[i] = Layer::Overlay;
            }
        }
    }
    let total_bytes: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();

    // cold: a fresh Vfs, every directory listed on first use
    let v = VfsBuilder::new(&overlay).package('G', &base, &mods).build().unwrap();
    let mut buf = Vec::new();
    let (cold, bytes, wrong_cold) = pass(&v, &files, &mut buf);
    assert_eq!(bytes, total_bytes);
    let layers_ok = files.iter().zip(&want_layer).filter(|((p, _), l)| v.resolve(p).unwrap().layer == **l).count();

    // host paths for the OS floor
    let hosts: Vec<PathBuf> = files.iter().map(|(p, _)| v.resolve(p).unwrap().host).collect();

    let in_memory: Vec<&(String, Vec<u8>)> =
        files.iter().zip(&want_layer).filter(|(_, l)| **l != Layer::Overlay).map(|(f, _)| f).collect();
    let mut warm = Vec::new();
    let mut floor = Vec::new();
    let mut resolve = Vec::new();
    let mut wrong_warm = 0;
    for _ in 0..rounds {
        let (s, _, w) = pass(&v, &files, &mut buf);
        warm.push(s);
        wrong_warm += w;
        let t = Instant::now();
        let mut n = 0u64;
        for h in &hosts {
            let mut f = fs::File::open(h).unwrap();
            buf.clear();
            n += f.read_to_end(&mut buf).unwrap() as u64;
        }
        floor.push(t.elapsed().as_secs_f64());
        assert_eq!(n, total_bytes);
        // lookups that stay in memory: files served by the base and the mods (an overlay file's
        // length is read from the host on resolve, so those are left out)
        let t = Instant::now();
        for (p, _) in in_memory.iter() {
            std::hint::black_box(v.resolve(p).unwrap());
        }
        resolve.push(t.elapsed().as_secs_f64());
    }

    // 4 threads sharing one Vfs, each reading all files
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..4 {
            let (v, files) = (&v, &files);
            s.spawn(move || {
                let mut buf = Vec::new();
                assert_eq!(pass(v, files, &mut buf).2, 0);
            });
        }
    });
    let threaded = t.elapsed().as_secs_f64();

    let n = files.len() as f64;
    let us = |s: f64| s / n * 1e6;
    println!("vfsbench  {} files, {:.1} MB, layers: base + 2 mods ({conflicts} conflicts) + overlay; {rounds} warm rounds", files.len(), total_bytes as f64 / 1e6);
    println!("layer check     {layers_ok} / {} files served by the expected layer", files.len());
    println!("cold vfs pass   {:8.1} ms  {:6.2} us/file   (directory listings built on first use)", cold * 1e3, us(cold));
    println!("warm vfs pass   {:8.1} ms  {:6.2} us/file   median open+size+read+close", median(warm.clone()) * 1e3, us(median(warm.clone())));
    println!("OS floor        {:8.1} ms  {:6.2} us/file   median File::open + read_to_end on the same host files", median(floor.clone()) * 1e3, us(median(floor.clone())));
    println!("vfs overhead    {:+8.1} ms  {:+6.2} us/file   warm vfs - OS floor", (median(warm.clone()) - median(floor.clone())) * 1e3, us(median(warm) - median(floor)));
    println!("lookup only     {:8.1} ms  {:6.2} us/file   median resolve() of the {} base and mod files, no host call", median(resolve.clone()) * 1e3, median(resolve) / in_memory.len() as f64 * 1e6, in_memory.len());
    println!("4 threads       {:8.1} ms  {:6.2} us/file   40,000 opens on one shared Vfs, wall", threaded * 1e3, threaded / (4.0 * n) * 1e6);
    println!("wrong           {} cold, {} warm", wrong_cold, wrong_warm);
    drop(v);
    fs::remove_dir_all(&root).unwrap();
    assert_eq!(wrong_cold + wrong_warm, 0);
    assert_eq!(layers_ok, files.len());
}
