//! D5 throughput: BCPack decode per format on one thread (vector and scalar lanes), across threads, and the
//! decode cache's hit path against decoding, by output size. Synthetic streams only (our own encoder); the
//! tables are ours, or the real format tables when `REBUILD_BCPACK_TABLES` names them (outside git).
//!
//! `cargo run --release -p rebuild-dstorage --bin bcbench`

use std::sync::Arc;
use std::time::Instant;

use rebuild_dstorage::bcpack::{self, synth, Decoder, Lanes, Tables};
use rebuild_dstorage::Cache;

const FORMATS: [u8; 6] = [1, 3, 4, 5, 6, 7];

/// Best of `runs` timings of `f`, in seconds.
fn best(runs: usize, mut f: impl FnMut()) -> f64 {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

fn main() {
    let real = bcpack::tables_path(None).map(|p| bcpack::load_tables(Some(&p)).expect("REBUILD_BCPACK_TABLES"));
    let synthetic = Tables::parse(&synth::tables(1)).unwrap();
    println!("bcbench: vector lanes {}", if bcpack::vector_lanes() { "AVX2" } else { "not available (scalar)" });
    let mut sets = vec![("synthetic tables", &synthetic)];
    if let Some(t) = &real {
        sets.push(("real tables", t));
    }

    // 1. One thread, 2^14 blocks a stream.
    for (name, t) in &sets {
        for bc in FORMATS {
            let blocks = 1 << 14;
            let size = blocks * bcpack::block_size(bc).unwrap();
            let src = synth::encode(bc, &synth::records(bc, blocks, 77 + u64::from(bc), t), t, 1 << 40);
            let mut out = vec![0u8; size];
            let v = best(9, || bcpack::decode_with(Lanes::Best, bc, std::hint::black_box(&src), &mut out, t).unwrap());
            let s = best(5, || bcpack::decode_with(Lanes::Scalar, bc, std::hint::black_box(&src), &mut out, t).unwrap());
            println!(
                "decode 1 thread  {name:16} BC{bc}  {:6.1} MB/s out ({:5.0} ns/block)  scalar {:5.1} MB/s  in {:.1} B/block",
                size as f64 / v / 1e6,
                v * 1e9 / blocks as f64,
                size as f64 / s / 1e6,
                src.len() as f64 / blocks as f64
            );
        }
    }

    // 2. Threads: each decodes its own copy of a BC7 and a BC1 stream.
    let t: &Tables = sets.last().unwrap().1;
    for bc in [1u8, 7] {
        let blocks = 1 << 14;
        let size = blocks * bcpack::block_size(bc).unwrap();
        let src = Arc::new(synth::encode(bc, &synth::records(bc, blocks, 5, t), t, 1 << 40));
        for threads in [1usize, 2, 4, 8] {
            let runs = 6;
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    let src = src.clone();
                    scope.spawn(move || {
                        let mut out = vec![0u8; size];
                        for _ in 0..runs {
                            bcpack::decode(bc, &src, &mut out, t).unwrap();
                        }
                    });
                }
            });
            let secs = start.elapsed().as_secs_f64();
            println!("decode {threads} threads BC{bc}: {:7.1} MB/s out in all", (threads * runs * size) as f64 / secs / 1e6);
        }
    }

    // 3. The cache: a hit (key hash, file read, CRC) against a decode, by output size. Files in the temp folder.
    let dir = std::env::temp_dir().join(format!("rebuild-bcbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tables = Arc::new(Tables::parse(&synth::tables(1)).unwrap());
    let decoder = Decoder::new(tables.clone(), Some(Cache::open(&dir, 1 << 32).unwrap().with_min_bytes(0)));
    for blocks in [256usize, 1024, 4096, 16384, 65536] {
        let bc = 7;
        let size = blocks * 16;
        let src = synth::encode(bc, &synth::records(bc, blocks, 9, &tables), &tables, 1 << 40);
        let mut out = vec![0u8; size];
        let decode = best(5, || bcpack::decode(bc, &src, &mut out, &tables).unwrap());
        decoder.decode(bc, &src, &mut out).unwrap(); // miss: decode and store
        let hit = best(9, || decoder.decode(bc, &src, &mut out).unwrap());
        println!(
            "cache BC7 {:>6} KiB out: decode {:7.1} us ({:6.1} MB/s)  hit {:7.1} us ({:7.1} MB/s)  hit/decode {:.3}",
            size >> 10,
            decode * 1e6,
            size as f64 / decode / 1e6,
            hit * 1e6,
            size as f64 / hit / 1e6,
            hit / decode
        );
    }
    let st = decoder.stats();
    println!("cache: {} hits, {} decoded", st.cache_hits, st.decoded);
    let _ = std::fs::remove_dir_all(&dir);
}
