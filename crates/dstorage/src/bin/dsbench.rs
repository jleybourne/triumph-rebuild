//! D3 throughput: zlib inflate on one thread, and whole requests (file read + inflate + write) through
//! the engine at several worker counts. Synthetic data only, written to the temp folder.
//!
//! `cargo run --release -p rebuild-dstorage --bin dsbench`

use std::sync::Arc;
use std::time::Instant;

use rebuild_dstorage::engine::{Config, Engine, StatusArray};
use rebuild_dstorage::zlib::{deflate, inflate};
use rebuild_dstorage::{Request, ZLIB};
use rebuild_vfs::{ModSet, VfsBuilder};

/// Same generator as the tests: about 1.3:1 under zlib (measured).
fn data(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let run = (x & 7) as usize + 1;
        let v = (x >> 8) as u8;
        for i in 0..run {
            out.push(if (x >> (16 + i)) & 1 == 1 { v } else { (x >> 24) as u8 & 0x0f });
        }
    }
    out.truncate(len);
    out
}

fn main() {
    const TOTAL: usize = 256 << 20;
    println!("dsbench: zlib-rs 0.6.9, synthetic data, {} MiB out per case", TOTAL >> 20);

    // 1. inflate alone, one thread.
    for chunk in [64usize << 10, 1 << 20] {
        let plain = data(1, chunk);
        let z = deflate(&plain, 6);
        let mut out = vec![0u8; chunk];
        let n = TOTAL / chunk;
        inflate(&z, &mut out).unwrap();
        let t = Instant::now();
        for _ in 0..n {
            inflate(std::hint::black_box(&z), &mut out).unwrap();
        }
        let s = t.elapsed().as_secs_f64();
        println!(
            "inflate 1 thread  chunk {:>5} KiB  ratio {:.2}  {:>7.0} MB/s out  {:>6.1} us/chunk",
            chunk >> 10,
            chunk as f64 / z.len() as f64,
            (n * chunk) as f64 / s / 1e6,
            s * 1e6 / n as f64
        );
    }

    // 2. requests through the engine: 64 KiB zlib chunks from one file (OS cache warm).
    let dir = std::env::temp_dir().join(format!("rebuild-dsbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("base")).unwrap();
    let chunk = 64 << 10;
    let n = TOTAL / chunk;
    let mut packed = Vec::new();
    let mut index = Vec::with_capacity(n);
    for i in 0..n {
        let z = deflate(&data(i as u64, chunk), 6);
        index.push((packed.len() as u64, z.len()));
        packed.extend_from_slice(&z);
    }
    std::fs::write(dir.join("base").join("pack.z"), &packed).unwrap();
    let mut out = vec![0u8; TOTAL];
    for workers in [1usize, 2, 4, 8] {
        let vfs = VfsBuilder::new(dir.join("overlay"))
            .package('G', dir.join("base"), &ModSet::default())
            .build()
            .unwrap();
        let engine = Engine::new(Arc::new(vfs), Config { workers, ..Config::default() });
        let key = 1usize;
        engine.open_file(key, "G:\\pack.z").unwrap();
        let q = engine.create_queue(u16::MAX, 2);
        let done = StatusArray::new(1);
        let t = Instant::now();
        for (i, &(off, len)) in index.iter().enumerate() {
            let r = Request {
                options: ZLIB,
                destination: out[i * chunk..].as_mut_ptr().cast(),
                page_offset: 0,
                destination_size: chunk as u32,
                source: key as *const _,
                offset: off,
                source_size: len as u32,
                intermediate_size: 0,
                tag: 0,
                name: std::ptr::null(),
            };
            // SAFETY: `out` outlives the queue, and each request has its own range.
            unsafe { engine.enqueue_request(q, r) };
        }
        engine.enqueue_status(q, &done, 0);
        engine.submit(q);
        while !done.is_complete(0) {
            std::thread::yield_now();
        }
        let s = t.elapsed().as_secs_f64();
        assert_eq!(done.result(0), 0);
        let st = engine.stats();
        println!(
            "engine {workers} worker(s)  {n} x 64 KiB zlib requests  {:>7.0} MB/s out  {:>6.0} MB/s in  {:.3} s",
            st.bytes_out as f64 / s / 1e6,
            st.bytes_in as f64 / s / 1e6,
            s
        );
        engine.close(q);
        engine.close_file(key);
    }
    for (i, &(_, _)) in index.iter().enumerate().step_by(97) {
        assert!(out[i * chunk..(i + 1) * chunk] == data(i as u64, chunk)[..], "chunk {i}");
    }
    println!("output checked (every 97th chunk)");
    let _ = std::fs::remove_dir_all(&dir);
}
