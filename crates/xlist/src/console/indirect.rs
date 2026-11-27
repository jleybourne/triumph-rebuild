//! ExecuteIndirect under the program library's root (`docs/design/root-seam.md`, "ExecuteIndirect").
//!
//! A title command signature whose arguments change root parameters names the title's root parameters. While a
//! translated pipeline is bound, the host holds the library's root instead, and the program reads those
//! arguments as user-data dwords. [`library_arguments`] rewrites such a signature's arguments into the library's
//! layout, consuming the same bytes of each command in the same order:
//!
//! - a `CONSTANT` argument becomes `CONSTANT` arguments into the user-data root constants of the stage that reads
//!   each dword (a run of dwords a stage reads in order is one argument);
//! - a root `CONSTANT_BUFFER_VIEW` / `SHADER_RESOURCE_VIEW` / `UNORDERED_ACCESS_VIEW` (8 bytes: a console GPU
//!   address) becomes `CONSTANT` arguments into the dwords that read its low and high halves: user data holds the
//!   console address as it is, so the bytes in the argument buffer are already what the program wants;
//! - dwords no stage reads go to user-data dwords past the ones a stage reads (written, never read);
//! - draw, draw-indexed, dispatch and mesh arguments are kept.
//!
//! The host's rule (found on WARP, `crates/pipelines/tests/indirect_warp.rs`): a signature's constants must rise in
//! (root parameter, dword) order without overlap. A mapping that would break it is refused ([`Unmappable::Order`]).
//!
//! No GPU pre-pass and no CPU read: the host signature does the mapping, made once per signature and pipeline map.
//! What cannot be mapped this way is refused ([`Unmappable`]), never run under the wrong root.

use windows::Win32::Graphics::Direct3D12::*;

use super::{UserMap, UserSource, USER_DWORDS};

/// [`super::INDIRECT`] indices past the six work kinds.
/// Calls refused: the signature's work is not on the bound pipeline's side (or it has none).
pub const SIDE: usize = 6;
/// Calls refused: vertex or index buffer views in the arguments (console addresses).
pub const BUFFER_VIEWS: usize = 7;
/// Calls that changed root arguments under the library root, through a mapped signature.
pub const MAPPED: usize = 8;
/// Calls refused: root arguments under the library root that cannot be mapped ([`Unmappable`]).
pub const UNMAPPABLE: usize = 9;
/// Calls refused: root arguments in the title's layout under another root, or a root view (a console address).
pub const WRONG_ROOT: usize = 10;
/// Every ExecuteIndirect the title records.
pub const CALLS: usize = 11;
/// Calls dropped before the signature is looked at: the bound console pipeline has no host one (or skipped).
pub const NO_PIPELINE: usize = 12;
pub const COUNTERS: usize = 13;

/// Why a signature cannot run under the library root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmappable {
    /// A vertex or index buffer view: console addresses the host would read as its own (needs a pre-pass).
    BufferView,
    /// One title dword read by two stages (VS and PS) or twice: one argument can write only one place.
    ReadTwice,
    /// Dwords no stage reads, and no spare user-data dwords to put them in.
    NoRoom,
    /// An argument type the host does not know, or no work argument last.
    Other,
    /// The dwords land out of order: the host takes a signature's constants only in rising (root parameter,
    /// dword) order without overlap (`E_INVALIDARG` otherwise, seen on WARP), and a command's bytes cannot be
    /// reordered. For example a title constant the pixel stage reads before one the vertex stage reads.
    Order,
}

/// What a mapped signature writes: its arguments, and the stages whose user data it changes (bit per stage),
/// whose words the list must write again before the next direct draw.
#[derive(Clone)]
pub struct LibraryArguments {
    pub arguments: Vec<D3D12_INDIRECT_ARGUMENT_DESC>,
    pub stages: u8,
}

fn constant(param: u32, dest: u32, n: u32) -> D3D12_INDIRECT_ARGUMENT_DESC {
    D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT,
        Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
            Constant: D3D12_INDIRECT_ARGUMENT_DESC_0_1 { RootParameterIndex: param, DestOffsetIn32BitValues: dest, Num32BitValuesToSet: n },
        },
    }
}

/// Where one dword of a command goes: (stage, user-data dword), or `None` for no reader.
type Target = Option<(usize, u32)>;

/// The one place `want` is read, over the map's stages.
fn target(map: &UserMap, want: impl Fn(UserSource) -> bool) -> Result<Target, Unmappable> {
    let mut found = None;
    for st in 0..(map.stage_count as usize).min(2) {
        let s = &map.stages[st];
        for (k, src) in s.src[..(s.dwords as usize).min(USER_DWORDS)].iter().enumerate() {
            if want(*src) {
                if found.is_some() {
                    return Err(Unmappable::ReadTwice);
                }
                found = Some((st, k as u32));
            }
        }
    }
    Ok(found)
}

/// The arguments of title signature `args` in the library layout of `map`.
pub fn library_arguments(args: &[D3D12_INDIRECT_ARGUMENT_DESC], map: &UserMap) -> Result<LibraryArguments, Unmappable> {
    let mut out = LibraryArguments { arguments: Vec::with_capacity(args.len() + 2), stages: 0 };
    // spare dwords: each stage's past the ones it reads, as (parameter, first), by parameter
    let mut spare = [(0u32, 0u32); 2];
    let spares = (map.stage_count as usize).min(2);
    for (st, s) in spare[..spares].iter_mut().enumerate() {
        *s = (map.stages[st].param, map.stages[st].dwords.min(USER_DWORDS as u32));
    }
    spare[..spares].sort_unstable();
    // where the last constant ended: the next must start at or past it
    let mut cursor = (0u32, 0u32);
    for (i, a) in args.iter().enumerate() {
        let last = i + 1 == args.len();
        // the dwords this argument consumes, each with where it goes
        let mut dwords: [Target; 64] = [None; 64];
        let n = match a.Type {
            D3D12_INDIRECT_ARGUMENT_TYPE_DRAW
            | D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED
            | D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH
            | D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH => {
                if !last {
                    return Err(Unmappable::Other);
                }
                out.arguments.push(*a);
                return Ok(out);
            }
            D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT => {
                // SAFETY: the variant of this type
                let c = unsafe { a.Anonymous.Constant };
                let n = c.Num32BitValuesToSet as usize;
                if n > 64 || c.RootParameterIndex >= 64 {
                    return Err(Unmappable::Other);
                }
                let slot = c.RootParameterIndex as u8;
                for (j, t) in dwords[..n].iter_mut().enumerate() {
                    // a parameter holds 64 dwords at most: one past that no program reads
                    let dword = c.DestOffsetIn32BitValues as usize + j;
                    *t = if dword < 64 { target(map, |s| s == UserSource::Constant { slot, dword: dword as u8 })? } else { None };
                }
                n
            }
            D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW
            | D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW
            | D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW => {
                // SAFETY: the three view variants share their layout
                let slot = unsafe { a.Anonymous.ConstantBufferView.RootParameterIndex };
                if slot >= 64 {
                    return Err(Unmappable::Other);
                }
                let slot = slot as u8;
                dwords[0] = target(map, |s| s == UserSource::AddressLo { slot })?;
                dwords[1] = target(map, |s| s == UserSource::AddressHi { slot })?;
                2
            }
            D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW | D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW => return Err(Unmappable::BufferView),
            _ => return Err(Unmappable::Other),
        };
        // runs: dwords read in order by one stage, or dwords no stage reads
        let mut j = 0;
        while j < n {
            let start = j;
            let (param, dest) = match dwords[j] {
                Some((st, k)) => {
                    j += 1;
                    while j < n && dwords[j] == Some((st, k + (j - start) as u32)) {
                        j += 1;
                    }
                    out.stages |= 1 << st;
                    (map.stages[st].param, k)
                }
                None => {
                    while j < n && dwords[j].is_none() {
                        j += 1;
                    }
                    // the first spare dwords at or past the cursor (the lowest place leaves the most for later runs)
                    let len = (j - start) as u32;
                    spare[..spares]
                        .iter()
                        .filter(|&&(p, _)| p >= cursor.0)
                        .map(|&(p, first)| (p, if p == cursor.0 { first.max(cursor.1) } else { first }))
                        .find(|&(_, at)| at + len <= USER_DWORDS as u32)
                        .ok_or(Unmappable::NoRoom)?
                }
            };
            let len = (j - start) as u32;
            // the host takes constants in rising (parameter, dword) order only, never overlapping
            if (param, dest) < cursor {
                return Err(Unmappable::Order);
            }
            cursor = (param, dest + len);
            out.arguments.push(constant(param, dest, len));
        }
    }
    // no work argument
    Err(Unmappable::Other)
}
