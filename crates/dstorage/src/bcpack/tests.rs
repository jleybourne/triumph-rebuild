//! D5 tests: exactness (against the fork's decoder, through recorded output hashes), both lane machines
//! agreeing, corrupt input refused without a panic, table loading, the cache, and the engine path.

use std::path::PathBuf;
use std::sync::Arc;

use super::synth::{self, Rng};
use super::{decode, decode_with, load_tables, Decoder, Error, Lanes, Tables, TABLES_SIZE};
use crate::cache::Cache;

const FORMATS: [u8; 6] = [1, 3, 4, 5, 6, 7];

fn block(bc: u8) -> usize {
    super::block_size(bc).unwrap()
}

/// FNV-1a 64.
fn fnv(v: &[u8]) -> u64 {
    v.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn ours() -> Tables {
    Tables::parse(&synth::tables(1)).unwrap()
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rebuild-bcpack-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Output of the fork's decoder (victory-formats `bcpack::decode`, run outside this repository) on the same
/// synthetic streams: 700 blocks per format, records seed 0xbc00 + BC, encoder seed 0x5100 + BC.
/// (stream FNV, output FNV) per format 1, 3, 4, 5, 6, 7.
/// Synthetic tables (`synth::tables(1)`).
const FORK_SYNTHETIC: [(u64, u64); 6] = [
    (0x481181feb54ec988, 0xc966b129e845a9b6),
    (0xde5b39d08c7f2f26, 0xa042b747b7d1471a),
    (0xf51c77adecdee6c7, 0xa254f6f8b7e80558),
    (0x008f07be3e0a0a7b, 0x3b98cf6e208d0b67),
    (0x2a311fde526b4c9d, 0x8068e63ef0538407),
    (0x5bad5653ce2f7620, 0x2c250686463a0278),
];
/// The real format tables (SHA-256 1f1d4760...c36b).
const FORK_REAL: [(u64, u64); 6] = [
    (0xd42079185fae1b45, 0x59f1fbc22ef4b783),
    (0xc0849be00951db1d, 0x7324c0637c1a9230),
    (0x4a3a067e23e69715, 0x48e361185a2884de),
    (0x2b33d290afb9472f, 0x2df349f131937a71),
    (0xccdba0bc30084515, 0x643a54ab9e5d2b8c),
    (0x19bb91aebfd150c5, 0xd18f35a873cd8a6c),
];

fn golden(t: &Tables, want: &[(u64, u64); 6]) {
    for (k, bc) in FORMATS.into_iter().enumerate() {
        let records = synth::records(bc, 700, 0xbc00 + u64::from(bc), t);
        let src = synth::encode(bc, &records, t, 0x5100 + u64::from(bc));
        assert_eq!(fnv(&src), want[k].0, "BC{bc}: the synthetic stream changed; the recorded hash no longer applies");
        for path in [Lanes::Best, Lanes::Scalar] {
            let mut out = vec![0u8; 700 * block(bc)];
            decode_with(path, bc, &src, &mut out, t).unwrap();
            assert_eq!(fnv(&out), want[k].1, "BC{bc} {path:?}: output differs from the fork's");
        }
    }
}

#[test]
fn d5b_output_equals_the_forks_on_synthetic_tables() {
    golden(&ours(), &FORK_SYNTHETIC);
}

/// With the real format tables (`REBUILD_BCPACK_TABLES`, outside git); skipped without them.
#[test]
fn d5b_output_equals_the_forks_on_the_real_tables() {
    let Some(path) = super::tables_path(None) else {
        eprintln!("REBUILD_BCPACK_TABLES not set: skipped");
        return;
    };
    golden(&load_tables(Some(&path)).unwrap(), &FORK_REAL);
}

#[test]
fn d5b_lanes_give_back_every_record() {
    let t = ours();
    for bc in FORMATS {
        for (k, blocks) in [1usize, 2, 17, 256, 257, 1000].into_iter().enumerate() {
            for seed in 0..5u64 {
                let records = synth::records(bc, blocks, seed * 100 + k as u64 + 1, &t);
                // `encode` picks fragment lengths 1, 7, 300, 5000 or unbounded from its seed.
                let src = synth::encode(bc, &records, &t, seed * 7 + 3);
                assert_eq!(synth::lane_records(bc, &src, blocks, &t).unwrap(), records, "BC{bc} {blocks} blocks");
            }
        }
    }
}

#[test]
fn d5b_vector_and_scalar_lanes_agree() {
    if !super::vector_lanes() {
        eprintln!("no AVX2: only the scalar lanes run here");
    }
    let t = ours();
    let mut rng = Rng(77);
    for bc in FORMATS {
        for blocks in [1usize, 3, 255, 600] {
            let records = synth::records(bc, blocks, rng.next(), &t);
            let src = synth::encode(bc, &records, &t, rng.next());
            let mut streams = vec![src.clone()];
            for m in 0..40u64 {
                let mut v = src.clone();
                let at = rng.below(v.len() as u64) as usize;
                match m % 4 {
                    0 => v[at] ^= 1 << rng.below(8),
                    1 => v.truncate(at),
                    2 => {
                        v.remove(at);
                    }
                    _ => v.insert(at, rng.next() as u8),
                }
                streams.push(v);
            }
            for v in streams {
                let mut a = vec![0u8; blocks * block(bc)];
                let mut b = vec![0u8; blocks * block(bc)];
                let ra = decode_with(Lanes::Best, bc, &v, &mut a, &t);
                let rb = decode_with(Lanes::Scalar, bc, &v, &mut b, &t);
                assert_eq!(ra.is_ok(), rb.is_ok(), "BC{bc}");
                if ra.is_ok() {
                    assert!(a == b, "BC{bc}: vector and scalar outputs differ");
                }
            }
        }
    }
}

#[test]
fn d5b_corrupt_input_is_refused_without_a_panic() {
    let t = ours();
    let records = synth::records(4, 64, 5, &t);
    let src = synth::encode(4, &records, &t, 9);
    let mut dst = vec![0u8; 64 * 8];
    decode(4, &src, &mut dst, &t).unwrap();
    // Truncated, empty, too many blocks asked for.
    assert!(matches!(decode(4, &src[..src.len() / 2], &mut dst, &t), Err(Error::Stream(_))));
    assert!(decode(4, &[], &mut dst, &t).is_err());
    assert!(decode(4, &src, &mut vec![0u8; 65 * 8], &t).is_err());
    // Destination not whole blocks, empty; formats BCPack does not carry.
    assert_eq!(decode(4, &src, &mut dst[..12], &t), Err(Error::Destination));
    assert_eq!(decode(4, &src, &mut [], &t), Err(Error::Destination));
    for bc in [0, 2, 8, 15] {
        assert_eq!(decode(bc, &src, &mut dst, &t), Err(Error::Format(bc)));
    }
    // Noise, in every format, through both lane machines: any result, never a panic.
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..300 {
        let noise: Vec<u8> = (0..round * 7 % 900).map(|_| rng.next() as u8).collect();
        for bc in FORMATS {
            let mut out = vec![0u8; 16 * (1 + round % 40)];
            let _ = decode_with(Lanes::Best, bc, &noise, &mut out, &t);
            let _ = decode_with(Lanes::Scalar, bc, &noise, &mut out, &t);
        }
    }
}

#[test]
fn d5a_tables_load_or_stay_refused() {
    let dir = temp("tables");
    std::fs::create_dir_all(&dir).unwrap();
    // Missing, wrong size, malformed: one-line errors, never a panic.
    assert!(load_tables(None).unwrap_err().contains("no BCPack tables"));
    assert!(load_tables(Some(&dir.join("missing.bin"))).is_err());
    let short = dir.join("short.bin");
    std::fs::write(&short, [0u8; 100]).unwrap();
    assert!(load_tables(Some(&short)).unwrap_err().contains("expected 77982"));
    let mut bad = synth::tables(3);
    bad[0] ^= 1; // model 0 no longer tiles 65536
    assert_eq!(Tables::parse(&bad).err(), Some(Error::Tables("a model does not tile 65536")));
    let zero = vec![0u8; TABLES_SIZE];
    assert_eq!(Tables::parse(&zero).err(), Some(Error::Tables("a model has an empty bucket")));
    // A good file loads; the engine's decoder comes up with it.
    let good = dir.join("good.bin");
    std::fs::write(&good, synth::tables(3)).unwrap();
    let t = load_tables(Some(&good)).unwrap();
    assert_eq!(t.hash(), *blake3::hash(&synth::tables(3)).as_bytes());
    assert!(Decoder::from_config(Some(&short)).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn d5c_cache_miss_then_hit() {
    let dir = temp("hit");
    let t = Arc::new(ours());
    let cache = Cache::open(&dir, 1 << 30).unwrap().with_min_bytes(0);
    let d = Decoder::new(t.clone(), Some(cache));
    let records = synth::records(7, 500, 11, &t);
    let src = synth::encode(7, &records, &t, 12);
    let mut want = vec![0u8; 500 * 16];
    decode(7, &src, &mut want, &t).unwrap();
    let mut out = vec![0u8; 500 * 16];
    d.decode(7, &src, &mut out).unwrap();
    assert!(out == want);
    let mut again = vec![0xAAu8; 500 * 16];
    d.decode(7, &src, &mut again).unwrap();
    assert!(again == want);
    let st = d.stats();
    assert_eq!((st.decoded, st.cache_hits), (1, 1));
    let cs = d.cache().unwrap().stats();
    assert_eq!((cs.misses, cs.hits, cs.writes, cs.bytes), (1, 1, 1, 8000));
    // The same bytes as another format, or another output size, are other keys.
    let c = d.cache().unwrap();
    assert_ne!(c.key(&t, 7, &src, 8000), c.key(&t, 6, &src, 8000));
    assert_ne!(c.key(&t, 7, &src, 8000), c.key(&t, 7, &src, 8016));
    // A new cache over the same folder (the next start) finds the entry.
    let reopened = Decoder::new(t.clone(), Some(Cache::open(&dir, 1 << 30).unwrap().with_min_bytes(0)));
    let mut third = vec![0u8; 500 * 16];
    reopened.decode(7, &src, &mut third).unwrap();
    assert!(third == want);
    assert_eq!(reopened.stats().cache_hits, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn d5c_corrupt_or_partial_entries_are_ignored_and_rebuilt() {
    let dir = temp("corrupt");
    let t = Arc::new(ours());
    let d = Decoder::new(t.clone(), Some(Cache::open(&dir, 1 << 30).unwrap().with_min_bytes(0)));
    let records = synth::records(1, 300, 21, &t);
    let src = synth::encode(1, &records, &t, 22);
    let mut want = vec![0u8; 300 * 8];
    decode(1, &src, &mut want, &t).unwrap();
    let mut out = vec![0u8; 300 * 8];
    d.decode(1, &src, &mut out).unwrap();
    let c = d.cache().unwrap();
    let path = c.path(&c.key(&t, 1, &src, out.len()));
    let entry = std::fs::read(&path).unwrap();
    // A flipped output byte (CRC), a cut-off file, a header for another size.
    let mut flipped = entry.clone();
    flipped[100] ^= 0x40;
    let mut other_size = entry.clone();
    other_size[16] ^= 1;
    for (k, bad) in [flipped, entry[..entry.len() / 2].to_vec(), entry[..10].to_vec(), other_size].into_iter().enumerate() {
        std::fs::write(&path, &bad).unwrap();
        let mut got = vec![0u8; 300 * 8];
        d.decode(1, &src, &mut got).unwrap();
        assert!(got == want, "case {k}: decoded output, not the bad entry");
        assert_eq!(std::fs::read(&path).unwrap(), entry, "case {k}: the entry was rebuilt");
    }
    let cs = c.stats();
    assert_eq!(cs.corrupt, 4);
    assert_eq!(d.stats().decoded, 5);
    // A temporary file left by a crash is removed at the next start.
    let leftover = dir.join(format!("{}.tmp1-0", path.file_name().unwrap().to_string_lossy()));
    std::fs::write(&leftover, b"half").unwrap();
    let _ = Cache::open(&dir, 1 << 30).unwrap();
    assert!(!leftover.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn d5c_cap_evicts_the_least_recently_used() {
    let dir = temp("evict");
    let t = Arc::new(ours());
    // Room for two 8,000-byte outputs.
    let d = Decoder::new(t.clone(), Some(Cache::open(&dir, 16_500).unwrap().with_min_bytes(0)));
    let streams: Vec<Vec<u8>> = (0..3).map(|k| synth::encode(7, &synth::records(7, 500, 40 + 2 * k, &t), &t, 50 + 2 * k)).collect();
    let mut out = vec![0u8; 8000];
    d.decode(7, &streams[0], &mut out).unwrap();
    d.decode(7, &streams[1], &mut out).unwrap();
    // Use 0 again, so 1 is the oldest when 2 arrives.
    d.decode(7, &streams[0], &mut out).unwrap();
    d.decode(7, &streams[2], &mut out).unwrap();
    let c = d.cache().unwrap();
    let exists = |k: usize| c.path(&c.key(&t, 7, &streams[k], 8000)).exists();
    assert!(exists(0) && !exists(1) && exists(2), "{:?} {:?} {:?}", [exists(0), exists(1), exists(2)], c.stats(), d.stats());
    assert_eq!(c.stats().evicted, 1);
    assert_eq!(c.stats().bytes, 16_000);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn d5c_small_outputs_skip_the_cache() {
    let dir = temp("small");
    let t = Arc::new(ours());
    let d = Decoder::new(t.clone(), Some(Cache::open(&dir, 1 << 30).unwrap()));
    let src = synth::encode(4, &synth::records(4, 100, 3, &t), &t, 4);
    let mut out = vec![0u8; 800];
    d.decode(4, &src, &mut out).unwrap();
    d.decode(4, &src, &mut out).unwrap();
    assert_eq!(d.stats().decoded, 2);
    assert_eq!(d.cache().unwrap().stats().writes, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A BCPack request (zlib, then BCPack, as the console orders them) through the engine, from a memory
/// source: the destination holds what `decode` gives.
#[test]
fn d5_engine_requests_decode_through_the_decoder() {
    use crate::engine::{Config, Engine};
    use crate::{Request, StatusArray, MEMORY_SOURCE, S_OK, ZLIB};
    let dir = temp("engine");
    std::fs::create_dir_all(dir.join("base")).unwrap();
    let vfs = rebuild_vfs::VfsBuilder::new(dir.join("overlay"))
        .package('G', dir.join("base"), &rebuild_vfs::ModSet::default())
        .build()
        .unwrap();
    let t = Arc::new(ours());
    let decoder = Arc::new(Decoder::new(t.clone(), Some(Cache::open(&dir.join("cache"), 1 << 30).unwrap().with_min_bytes(0))));
    let engine = Engine::new(Arc::new(vfs), Config { workers: 2, bcpack: Some(decoder.clone()), ..Config::default() });
    let packed = synth::encode(5, &synth::records(5, 400, 61, &t), &t, 62);
    let z = crate::zlib::deflate(&packed, 6);
    let mut want = vec![0u8; 400 * 16];
    decode(5, &packed, &mut want, &t).unwrap();
    let q = engine.create_queue(64, 2);
    let status = StatusArray::new(2);
    let mut outs = [vec![0u8; 400 * 16], vec![0u8; 400 * 16]];
    for (i, out) in outs.iter_mut().enumerate() {
        let r = Request {
            options: ZLIB | MEMORY_SOURCE | (5 << 1),
            destination: out.as_mut_ptr().cast(),
            page_offset: 0,
            destination_size: out.len() as u32,
            source: z.as_ptr().cast(),
            offset: 0,
            source_size: z.len() as u32,
            intermediate_size: packed.len() as u32,
            tag: 0,
            name: std::ptr::null(),
        };
        // SAFETY: the destination and source outlive the queue (closed below).
        unsafe { engine.enqueue_request(q, r) };
        engine.enqueue_status(q, &status, i as u32);
        engine.submit(q);
        while !status.is_complete(i as u32) {
            std::thread::yield_now();
        }
        assert_eq!(status.result(i as u32), S_OK);
    }
    engine.close(q);
    assert!(outs[0] == want && outs[1] == want);
    let st = decoder.stats();
    assert_eq!((st.decoded, st.cache_hits), (1, 1));
    assert_eq!(engine.stats().bcpack_refused, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
