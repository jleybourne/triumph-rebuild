//! The translator version: a hash of every source file whose change can change what the shader library holds
//! (the lifter and IR in `crates/sir`, the console program reader in `crates/shader`, and this crate's
//! translation, binding, layout, sidecar and library code). `rebuild_pipelines::shaderlib` stores it with the
//! library; a library built by another version is rebuilt (incrementally: only programs whose HLSL changed go
//! through DXC again).

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// FNV-1a, 64 bits, over a byte stream (no build dependency needed; this is an identity, not a security hash).
struct Fnv(u64);

impl Fnv {
    fn update(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= u64::from(*x);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
}

fn main() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = here.parent().unwrap().to_path_buf();
    let mut list = Vec::new();
    for d in [crates.join("sir").join("src"), crates.join("shader").join("src")] {
        println!("cargo:rerun-if-changed={}", d.display());
        files(&d, &mut list);
    }
    for f in ["translate.rs", "bindings.rs", "layout.rs", "library.rs", "sidecar.rs", "d3d.rs", "lib.rs"] {
        let p = here.join("src").join(f);
        println!("cargo:rerun-if-changed={}", p.display());
        list.push(p);
    }
    list.sort();
    // two independent 64-bit hashes (different offset bases), so the version is 128 bits
    let (mut a, mut b) = (Fnv(0xcbf2_9ce4_8422_2325), Fnv(0x6c62_272e_07bb_0142));
    for p in &list {
        let rel = p.strip_prefix(&crates).unwrap_or(p).to_string_lossy().replace('\\', "/");
        // line endings do not count: a checkout with CRLF and one with LF are the same translator
        let text = std::fs::read(p).unwrap_or_default().into_iter().filter(|c| *c != b'\r').collect::<Vec<u8>>();
        for h in [&mut a, &mut b] {
            h.update(rel.as_bytes());
            h.update(&[0]);
            h.update(&text);
            h.update(&[0]);
        }
    }
    println!("cargo:rustc-env=REBUILD_TRANSLATOR_VERSION={:016x}{:016x}", a.0, b.0);
}
