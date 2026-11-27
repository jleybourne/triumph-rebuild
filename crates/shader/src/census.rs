//! Every native program in the title's shader archive, found and deduplicated
//! the way the fork's `translate` counts them, but in file order (so the run is
//! deterministic).
//!
//! The archive is an SGD5 file of deflate-packed records, each one DXBC
//! container ([`crate::sgd5`]); containers stored as is are found by their magic
//! too. Containers whose `DXIL` part is byte-identical are one: the first in file
//! order is kept and the rest are counted as duplicates. Each kept container
//! yields one native program per hardware stage it carries.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{Container, FourCC, NativeProgram, native_programs, sgd5};

/// One native program and the container it came from.
#[derive(Debug, Clone)]
pub struct Program {
    /// The container's hash, as the fork's logs name it: BLAKE3, 16 hex digits.
    pub hash: String,
    /// The `DXIL` part's hash (8 hex digits): what deduplication keys on.
    pub code_hash: String,
    pub native: NativeProgram,
    /// The container's input and output signature parts (`ISG1`, `OSG1`; empty if absent): what the
    /// stage's inputs and outputs are called, which a host pipeline links by.
    pub isg1: Vec<u8>,
    pub osg1: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Walk {
    pub records: usize,
    pub raw: usize,
    pub corrupt: usize,
    pub containers: usize,
    pub no_dxil: usize,
    pub duplicates: usize,
    /// Every container in file order: its hash and its `DXIL` part's hash.
    pub order: Vec<(String, String)>,
    /// Each duplicate container's hash and the code hash of the kept one.
    pub aliases: Vec<(String, String)>,
    /// Native blob errors by message.
    pub native_errors: BTreeMap<String, usize>,
    pub programs: Vec<Program>,
}

enum Job<'a> {
    Packed(sgd5::Record<'a>),
    Raw(&'a [u8]),
}

enum Found {
    Corrupt,
    NotContainer,
    NoDxil,
    Container {
        hash: String,
        code: blake3::Hash,
        programs: Result<Vec<NativeProgram>, String>,
        isg1: Vec<u8>,
        osg1: Vec<u8>,
    },
}

fn examine(bytes: &[u8]) -> Found {
    let Ok(container) = Container::parse(bytes) else {
        return Found::NotContainer;
    };
    let Some(dxil) = container.part(FourCC::DXIL) else {
        return Found::NoDxil;
    };
    Found::Container {
        hash: blake3::hash(bytes).to_hex()[..16].to_owned(),
        code: blake3::hash(dxil.data),
        programs: native_programs(dxil.data).map_err(|e| e.to_string()),
        isg1: container.part(FourCC::ISG1).map(|p| p.data.to_vec()).unwrap_or_default(),
        osg1: container.part(FourCC::OSG1).map(|p| p.data.to_vec()).unwrap_or_default(),
    }
}

/// Walks one archive file on `threads` threads; the result is in file order.
pub fn walk(file: &[u8], threads: usize) -> Walk {
    let mut jobs: Vec<Job> = Vec::new();
    if sgd5::is_sgd5(file) {
        jobs.extend(sgd5::records(file).map(Job::Packed));
    }
    jobs.extend(
        memchr::memmem::find_iter(file, b"DXBC")
            .filter(|&at| Container::parse(&file[at..]).is_ok())
            .map(|at| Job::Raw(&file[at..])),
    );
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, Found)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        let found = match job {
                            Job::Packed(r) => match r.unpack() {
                                Some(bytes) => examine(&bytes),
                                None => Found::Corrupt,
                            },
                            Job::Raw(bytes) => examine(bytes),
                        };
                        out.push((i, found));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("census thread")).collect()
    });
    results.sort_by_key(|r| r.0);

    let mut w = Walk::default();
    for job in &jobs {
        match job {
            Job::Packed(_) => w.records += 1,
            Job::Raw(_) => w.raw += 1,
        }
    }
    let mut seen = HashSet::new();
    for (_, found) in results {
        match found {
            Found::Corrupt => w.corrupt += 1,
            Found::NotContainer => {}
            Found::NoDxil => {
                w.containers += 1;
                w.no_dxil += 1;
            }
            Found::Container { hash, code, programs, isg1, osg1 } => {
                w.containers += 1;
                w.order.push((hash.clone(), code.to_hex()[..8].to_owned()));
                if !seen.insert(code) {
                    w.duplicates += 1;
                    w.aliases.push((hash, code.to_hex()[..8].to_owned()));
                    continue;
                }
                match programs {
                    Ok(list) => {
                        let code_hash = code.to_hex()[..8].to_owned();
                        w.programs.extend(list.into_iter().map(|native| Program {
                            hash: hash.clone(),
                            code_hash: code_hash.clone(),
                            native,
                            isg1: isg1.clone(),
                            osg1: osg1.clone(),
                        }));
                    }
                    Err(e) => *w.native_errors.entry(e).or_default() += 1,
                }
            }
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw DXBC container with an empty `DXIL` part is found, counted once,
    /// and carries no native program; its byte-identical copy is a duplicate.
    #[test]
    fn finds_and_deduplicates_raw_containers() {
        let one = crate::dxbc::build(&[(FourCC::DXIL, &[0u8; 32])]);
        let other = crate::dxbc::build(&[(FourCC::DXIL, &[1u8; 32])]);
        let mut file = vec![0x55; 64];
        file.extend(&one);
        file.extend(&other);
        file.extend(&one);
        let w = walk(&file, 2);
        assert_eq!((w.raw, w.containers, w.duplicates, w.programs.len()), (3, 3, 1, 0));
    }
}
