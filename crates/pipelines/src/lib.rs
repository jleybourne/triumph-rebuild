//! Build 4: pipelines built into a hash-keyed library at start (plan decision 4: nothing compiles during a
//! frame).
//!
//! - [`bindings`]: what a translated program binds, read from its compiled DXIL (`PSV0`) and its signatures.
//! - [`layout`]: the host root signatures, built from those bindings: user data as root constants at `b0`,
//!   SM 6.6 heap indexing with both `*_HEAP_DIRECTLY_INDEXED` flags always set, immediate samplers as static
//!   samplers at `space100`, the descriptor shadow table at `t0, space101` (plan decision 9).
//! - [`library`]: one file holding, per program hash, the compiled DXIL and its root signature, stored once,
//!   loaded at start in parallel, looked up by hash with no index to build.
//! - `d3d` (Windows): the host serializer, and a device check that every root signature and pipeline is
//!   accepted.
//! - [`state`]: a pipeline as plain data ([`state::Desc`]) and its [`state::Key`].
//! - [`pool`]: pipeline creation on worker threads; the title's create call returns at once.
//! - `pso` (Windows): the host creator, through a persistent `ID3D12PipelineLibrary1` cache.
//! - [`stream`]: the title's pipeline state streams (device 0x2b0) and graphics descriptions (0x50) as a `Desc`.
//! - [`session`]: the pipelines a session used, for the next session's prewarm.
//! - [`sidecar`]: the translator's `<hash>.<stage>.samplers` files.
//! - [`translate`]: one console program to DXIL (the `translate v1` path), for the binary and for programs
//!   the library does not hold.
//! - [`ondemand`]: those programs, translated on a pool worker when the title first creates them, cached on
//!   disk for the next session.

pub mod bindings;
pub mod layout;
pub mod library;
pub mod ondemand;
pub mod pool;
pub mod session;
pub mod sidecar;
pub mod state;
pub mod stream;
pub mod translate;

#[cfg(windows)]
pub mod d3d;
#[cfg(windows)]
pub mod pso;

/// A program's stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    Vs = 0,
    Ps = 1,
    Cs = 2,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Vs => "vs",
            Stage::Ps => "ps",
            Stage::Cs => "cs",
        }
    }

    pub fn from_name(s: &str) -> Option<Stage> {
        Some(match s {
            "vs" => Stage::Vs,
            "ps" => Stage::Ps,
            "cs" => Stage::Cs,
            _ => return None,
        })
    }

    pub fn from_u32(v: u32) -> Option<Stage> {
        Some(match v {
            0 => Stage::Vs,
            1 => Stage::Ps,
            2 => Stage::Cs,
            _ => return None,
        })
    }
}

/// A program hash as the census names it (16 hex digits) as a number.
pub fn parse_hash(s: &str) -> Option<u64> {
    (s.len() == 16).then(|| u64::from_str_radix(s, 16).ok()).flatten()
}
