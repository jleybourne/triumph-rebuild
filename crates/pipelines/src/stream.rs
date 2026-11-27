//! The title's graphics and compute descriptions as a [`Desc`]: a pipeline state stream (device 0x2b0,
//! `CreatePipelineState`) or a `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (device 0x50).
//!
//! A stream is a run of subobjects, each 8-byte aligned: a `u32` type, then its payload at the payload's own
//! alignment (+4 for the 4-byte structures, +8 for those that hold a pointer), as on PC. The console numbers
//! amplification and mesh shaders 23 and 24 (PC: 24 and 25) and its own types from 25 on
//! ([`Numbering::Console`]). A console type whose size is known (the caller's table) is skipped and counted;
//! any type whose size is not known stops the parse with a [`Refusal`], never a read past it.
//!
//! Programs are known by hash ([`program_hash`]); the title's root signature is not read (the program library
//! brings its own). Nothing here allocates beyond the [`Desc`] it returns. Pointers in a description are read
//! through [`Memory`], so the parser is plain data and is tested without a title.

use crate::state::{Blend, DepthStencil, Desc, GraphicsState, InputElement, StencilOp};

/// The program hash of a console shader: the first 8 bytes (big-endian) of the BLAKE3 of its DXBC container
/// at the container's declared size (bytes past it do not count).
pub fn program_hash(bytecode: &[u8]) -> Option<u64> {
    if bytecode.len() < 0x20 || &bytecode[..4] != b"DXBC" {
        return None;
    }
    let total = u32::from_le_bytes(bytecode[0x18..0x1c].try_into().ok()?) as usize;
    let bytes = bytecode.get(..total)?;
    Some(u64::from_be_bytes(rebuild_shader::blake3::hash(bytes).as_bytes()[..8].try_into().ok()?))
}

/// Where a description's pointers lead.
pub trait Memory {
    /// `len` bytes at `addr`, or `None` (a null or unreadable address).
    fn bytes(&self, addr: u64, len: usize) -> Option<&[u8]>;
    /// The NUL-terminated string at `addr` (without the NUL), at most [`MAX_NAME`] bytes.
    fn cstr(&self, addr: u64) -> Option<&[u8]>;
    /// The hash of the program `len` bytes at `addr` hold.
    fn program(&self, addr: u64, len: usize) -> Option<u64> {
        program_hash(self.bytes(addr, len)?)
    }
}

/// The longest semantic name read.
pub const MAX_NAME: usize = 256;
/// The largest program read (a console container is far smaller).
pub const MAX_PROGRAM: usize = 64 << 20;
/// D3D12's input layout limit.
pub const MAX_INPUTS: u32 = 32;

/// This process's own memory: the title's descriptions as it passes them.
pub struct Process(());

impl Process {
    /// # Safety
    /// Every non-null address a description holds must point at readable memory for the length it states,
    /// and every semantic name pointer at a NUL-terminated string, for as long as the parse runs.
    pub unsafe fn new() -> Process {
        Process(())
    }
}

impl Memory for Process {
    fn bytes(&self, addr: u64, len: usize) -> Option<&[u8]> {
        if addr == 0 || len > MAX_PROGRAM {
            return None;
        }
        // SAFETY: the contract of `Process::new`
        Some(unsafe { std::slice::from_raw_parts(addr as usize as *const u8, len) })
    }

    fn cstr(&self, addr: u64) -> Option<&[u8]> {
        if addr == 0 {
            return None;
        }
        let p = addr as usize as *const u8;
        // SAFETY: the contract of `Process::new`; reading stops at the NUL
        let n = (0..MAX_NAME).find(|&i| unsafe { *p.add(i) } == 0)?;
        Some(unsafe { std::slice::from_raw_parts(p, n) })
    }
}

/// Which numbering a stream's subobject types follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Numbering {
    /// PC D3D12: AS 24, MS 25, DEPTH_STENCIL2 26, RASTERIZER1 27, RASTERIZER2 28, SERIALIZED_ROOT_SIGNATURE 29.
    Pc,
    /// The console: AS 23, MS 24, its own types from 25.
    Console,
}

/// Why a description gives no host pipeline. The title still gets a valid pipeline object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A subobject runs past the stream's end (or less than a type is left).
    Truncated { at: usize, kind: u32 },
    /// A subobject type whose size is not known: the parse stops there.
    UnknownSize { at: usize, kind: u32 },
    /// A stage (or feature) the program library has no programs for: `mesh`, `amplification`, `hull`, `domain`,
    /// `geometry`, `stream output`, `view instancing`.
    Stage(&'static str),
    /// Neither a vertex nor a compute shader.
    NoShader,
    /// A compute shader with graphics stages.
    Mixed,
    /// A shader whose bytecode is not a readable DXBC container: `vs`, `ps` or `cs`.
    Program(&'static str),
    /// An input layout that cannot be read (null with elements, more than 32, an unreadable name).
    Input,
    /// More than 8 render targets.
    Targets,
    /// A graphics description shorter than `D3D12_GRAPHICS_PIPELINE_STATE_DESC`.
    Short,
}

impl Refusal {
    /// A short name, for counting.
    pub fn reason(&self) -> &'static str {
        match self {
            Refusal::Truncated { .. } => "truncated stream",
            Refusal::UnknownSize { .. } => "subobject of unknown size",
            Refusal::Stage(s) => s,
            Refusal::NoShader => "no vertex or compute shader",
            Refusal::Mixed => "compute and graphics stages together",
            Refusal::Program(_) => "bytecode not a DXBC container",
            Refusal::Input => "input layout unreadable",
            Refusal::Targets => "more than 8 render targets",
            Refusal::Short => "description too short",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Truncated { at, kind } => write!(f, "subobject {kind} at +{at:#x} runs past the stream"),
            Refusal::UnknownSize { at, kind } => write!(f, "subobject {kind} at +{at:#x} has no known size"),
            Refusal::Program(s) => write!(f, "{s} bytecode is not a DXBC container"),
            r => f.write_str(r.reason()),
        }
    }
}

/// A description read.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub desc: Desc,
    /// Console subobjects skipped (known size, not read), and the first one's type (0 for none).
    pub skipped: u32,
    pub first_skipped: u32,
    /// `D3D12_PIPELINE_STATE_FLAGS` (not part of the host pipeline: tool debug, dynamic depth bias and strip cut).
    pub flags: u32,
}

#[derive(Clone, Copy)]
enum Sh {
    Vs,
    Ps,
    Ds,
    Hs,
    Gs,
    Cs,
    As,
    Ms,
}

#[derive(Clone, Copy)]
enum Kind {
    Root,
    Shader(Sh),
    StreamOutput,
    Blend,
    SampleMask,
    Raster,
    DepthStencil,
    Input,
    StripCut,
    Topology,
    Targets,
    DepthFormat,
    SampleDesc,
    NodeMask,
    CachedPso,
    Flags,
    DepthStencil1,
    ViewInstancing,
    DepthStencil2,
    Raster1,
    Raster2,
    SerializedRoot,
}

/// Payload sizes (the PC structures; the console's are the same).
mod size {
    pub const BYTECODE: usize = 16; // D3D12_SHADER_BYTECODE
    pub const STREAM_OUTPUT: usize = 32; // D3D12_STREAM_OUTPUT_DESC
    pub const BLEND: usize = 328; // D3D12_BLEND_DESC
    pub const RASTER: usize = 44; // D3D12_RASTERIZER_DESC, ..._DESC1
    pub const RASTER2: usize = 40; // D3D12_RASTERIZER_DESC2
    pub const DEPTH_STENCIL: usize = 52; // D3D12_DEPTH_STENCIL_DESC
    pub const DEPTH_STENCIL1: usize = 56; // D3D12_DEPTH_STENCIL_DESC1
    pub const DEPTH_STENCIL2: usize = 60; // D3D12_DEPTH_STENCIL_DESC2
    pub const INPUT: usize = 16; // D3D12_INPUT_LAYOUT_DESC
    pub const TARGETS: usize = 36; // D3D12_RT_FORMAT_ARRAY
    pub const VIEW_INSTANCING: usize = 24; // D3D12_VIEW_INSTANCING_DESC
    pub const INPUT_ELEMENT: usize = 32; // D3D12_INPUT_ELEMENT_DESC
}

/// A type's meaning, its payload offset in the subobject, and its payload size.
fn kind(t: u32, n: Numbering) -> Option<(Kind, usize, usize)> {
    use Kind::*;
    let ptr = |k, s| Some((k, 8, s));
    let word = |k, s| Some((k, 4, s));
    match (t, n) {
        (0, _) => ptr(Root, 8),
        (1, _) => ptr(Shader(Sh::Vs), size::BYTECODE),
        (2, _) => ptr(Shader(Sh::Ps), size::BYTECODE),
        (3, _) => ptr(Shader(Sh::Ds), size::BYTECODE),
        (4, _) => ptr(Shader(Sh::Hs), size::BYTECODE),
        (5, _) => ptr(Shader(Sh::Gs), size::BYTECODE),
        (6, _) => ptr(Shader(Sh::Cs), size::BYTECODE),
        (7, _) => ptr(StreamOutput, size::STREAM_OUTPUT),
        (8, _) => word(Blend, size::BLEND),
        (9, _) => word(SampleMask, 4),
        (10, _) => word(Raster, size::RASTER),
        (11, _) => word(DepthStencil, size::DEPTH_STENCIL),
        (12, _) => ptr(Input, size::INPUT),
        (13, _) => word(StripCut, 4),
        (14, _) => word(Topology, 4),
        (15, _) => word(Targets, size::TARGETS),
        (16, _) => word(DepthFormat, 4),
        (17, _) => word(SampleDesc, 8),
        (18, _) => word(NodeMask, 4),
        (19, _) => ptr(CachedPso, size::BYTECODE),
        (20, _) => word(Flags, 4),
        (21, _) => word(DepthStencil1, size::DEPTH_STENCIL1),
        (22, _) => ptr(ViewInstancing, size::VIEW_INSTANCING),
        (23, Numbering::Console) | (24, Numbering::Pc) => ptr(Shader(Sh::As), size::BYTECODE),
        (24, Numbering::Console) | (25, Numbering::Pc) => ptr(Shader(Sh::Ms), size::BYTECODE),
        (26, Numbering::Pc) => word(DepthStencil2, size::DEPTH_STENCIL2),
        (27, Numbering::Pc) => word(Raster1, size::RASTER),
        (28, Numbering::Pc) => word(Raster2, size::RASTER2),
        (29, Numbering::Pc) => ptr(SerializedRoot, size::BYTECODE),
        _ => None,
    }
}

/// The whole subobject's size (type, padding and payload, 8-byte aligned) of a known type: for the table in
/// the docs and for tests.
pub fn subobject_size(t: u32, n: Numbering) -> Option<usize> {
    kind(t, n).map(|(_, at, len)| (at + len + 7) & !7)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn bool_at(b: &[u8], at: usize) -> bool {
    u32_at(b, at) != 0
}

/// A shader's bytecode: (address, length); none when either is zero.
fn code(p: &[u8]) -> Option<(u64, usize)> {
    let (a, n) = (u64_at(p, 0), u64_at(p, 8) as usize);
    (a != 0 && n != 0).then_some((a, n))
}

/// What a description has said so far.
struct Acc {
    vs: Option<(u64, usize)>,
    ps: Option<(u64, usize)>,
    cs: Option<(u64, usize)>,
    /// a stage or feature we do not build (the first; mesh and amplification win)
    other: Option<&'static str>,
    mesh: Option<&'static str>,
    input: (u64, u32),
    state: GraphicsState,
    targets: Option<Refusal>,
    flags: u32,
}

impl Acc {
    /// D3D12's defaults for what a stream leaves out (`CD3DX12_*_DESC(D3D12_DEFAULT)`): back faces culled,
    /// depth on (LESS, written). The topology type keeps triangles.
    fn stream() -> Acc {
        let mut state = GraphicsState::default();
        state.raster.cull = 3;
        state.depth_stencil.depth = true;
        Acc { vs: None, ps: None, cs: None, other: None, mesh: None, input: (0, 0), state, targets: None, flags: 0 }
    }

    fn stencil(p: &[u8], at: usize) -> StencilOp {
        StencilOp { fail: u32_at(p, at), depth_fail: u32_at(p, at + 4), pass: u32_at(p, at + 8), func: u32_at(p, at + 12) }
    }

    fn raster(&mut self, p: &[u8], float_bias: bool, line_mode: bool) {
        let r = &mut self.state.raster;
        r.fill = u32_at(p, 0);
        r.cull = u32_at(p, 4);
        r.front_ccw = bool_at(p, 8);
        r.depth_bias = if float_bias { f32::from_bits(u32_at(p, 12)) as i32 } else { u32_at(p, 12) as i32 };
        r.depth_bias_clamp = f32::from_bits(u32_at(p, 16));
        r.slope_scaled_depth_bias = f32::from_bits(u32_at(p, 20));
        r.depth_clip = bool_at(p, 24);
        if line_mode {
            // D3D12_LINE_RASTERIZATION_MODE: 0 aliased, 1 alpha antialiased, 2 / 3 quadrilateral (multisample)
            let m = u32_at(p, 28);
            r.multisample = m >= 2;
            r.antialiased_line = m == 1;
            r.forced_sample_count = u32_at(p, 32);
            r.conservative = bool_at(p, 36);
        } else {
            r.multisample = bool_at(p, 28);
            r.antialiased_line = bool_at(p, 32);
            r.forced_sample_count = u32_at(p, 36);
            r.conservative = bool_at(p, 40);
        }
    }

    fn apply(&mut self, k: Kind, p: &[u8]) {
        match k {
            Kind::Shader(s) => {
                let c = code(p);
                match s {
                    Sh::Vs => self.vs = c,
                    Sh::Ps => self.ps = c,
                    Sh::Cs => self.cs = c,
                    Sh::As if c.is_some() => self.mesh = self.mesh.or(Some("amplification")),
                    Sh::Ms if c.is_some() => self.mesh = Some("mesh"),
                    Sh::Ds if c.is_some() => self.other = self.other.or(Some("domain")),
                    Sh::Hs if c.is_some() => self.other = self.other.or(Some("hull")),
                    Sh::Gs if c.is_some() => self.other = self.other.or(Some("geometry")),
                    _ => {}
                }
            }
            Kind::StreamOutput => {
                if u64_at(p, 0) != 0 && u32_at(p, 8) != 0 {
                    self.other = self.other.or(Some("stream output"));
                }
            }
            Kind::Blend => {
                let s = &mut self.state;
                s.alpha_to_coverage = bool_at(p, 0);
                s.independent_blend = bool_at(p, 4);
                for (i, b) in s.blend.iter_mut().enumerate() {
                    let at = 8 + 40 * i;
                    *b = Blend {
                        enable: bool_at(p, at),
                        logic_enable: bool_at(p, at + 4),
                        src: u32_at(p, at + 8),
                        dst: u32_at(p, at + 12),
                        op: u32_at(p, at + 16),
                        src_alpha: u32_at(p, at + 20),
                        dst_alpha: u32_at(p, at + 24),
                        op_alpha: u32_at(p, at + 28),
                        logic_op: u32_at(p, at + 32),
                        write_mask: p[at + 36],
                    };
                }
            }
            Kind::SampleMask => self.state.sample_mask = u32_at(p, 0),
            Kind::Raster => self.raster(p, false, false),
            Kind::Raster1 => self.raster(p, true, false),
            Kind::Raster2 => self.raster(p, true, true),
            Kind::DepthStencil | Kind::DepthStencil1 => {
                self.state.depth_stencil = DepthStencil {
                    depth: bool_at(p, 0),
                    depth_write: u32_at(p, 4) != 0,
                    depth_func: u32_at(p, 8),
                    stencil: bool_at(p, 12),
                    read_mask: p[16],
                    write_mask: p[17],
                    front: Acc::stencil(p, 20),
                    back: Acc::stencil(p, 36),
                };
            }
            Kind::DepthStencil2 => {
                // per-face masks: the host state has one pair, the front face's
                self.state.depth_stencil = DepthStencil {
                    depth: bool_at(p, 0),
                    depth_write: u32_at(p, 4) != 0,
                    depth_func: u32_at(p, 8),
                    stencil: bool_at(p, 12),
                    read_mask: p[32],
                    write_mask: p[33],
                    front: Acc::stencil(p, 16),
                    back: Acc::stencil(p, 36),
                };
            }
            Kind::Input => self.input = (u64_at(p, 0), u32_at(p, 8)),
            Kind::StripCut => self.state.strip_cut = u32_at(p, 0),
            Kind::Topology => self.state.topology = u32_at(p, 0),
            Kind::Targets => {
                let n = u32_at(p, 32);
                if n > 8 {
                    self.targets = Some(Refusal::Targets);
                } else {
                    self.targets = None;
                    self.state.targets.clear();
                    self.state.targets.extend((0..n as usize).map(|i| u32_at(p, 4 * i)));
                }
            }
            Kind::DepthFormat => self.state.depth_format = u32_at(p, 0),
            Kind::SampleDesc => {
                self.state.samples = u32_at(p, 0);
                self.state.sample_quality = u32_at(p, 4);
            }
            Kind::Flags => self.flags = u32_at(p, 0),
            Kind::ViewInstancing => {
                if u32_at(p, 0) != 0 {
                    self.other = self.other.or(Some("view instancing"));
                }
            }
            Kind::Root | Kind::NodeMask | Kind::CachedPso | Kind::SerializedRoot => {}
        }
    }

    fn finish(mut self, mem: &impl Memory, skipped: u32, first_skipped: u32) -> Result<Parsed, Refusal> {
        if let Some(m) = self.mesh {
            return Err(Refusal::Stage(m));
        }
        if let Some(o) = self.other {
            return Err(Refusal::Stage(o));
        }
        let hash = |c: (u64, usize), s| mem.program(c.0, c.1).ok_or(Refusal::Program(s));
        let desc = if let Some(cs) = self.cs {
            if self.vs.is_some() || self.ps.is_some() {
                return Err(Refusal::Mixed);
            }
            Desc::Compute { cs: hash(cs, "cs")? }
        } else {
            let vs = hash(self.vs.ok_or(Refusal::NoShader)?, "vs")?;
            let ps = self.ps.map(|p| hash(p, "ps")).transpose()?;
            if let Some(r) = self.targets {
                return Err(r);
            }
            let (at, n) = self.input;
            if n > MAX_INPUTS || (at == 0 && n != 0) {
                return Err(Refusal::Input);
            }
            let s = &mut self.state;
            if n != 0 {
                let e = mem.bytes(at, n as usize * size::INPUT_ELEMENT).ok_or(Refusal::Input)?;
                s.input.reserve_exact(n as usize);
                for e in e.chunks_exact(size::INPUT_ELEMENT) {
                    let name = std::str::from_utf8(mem.cstr(u64_at(e, 0)).ok_or(Refusal::Input)?).map_err(|_| Refusal::Input)?;
                    s.input.push(InputElement {
                        name: name.to_owned(),
                        index: u32_at(e, 8),
                        format: u32_at(e, 12),
                        slot: u32_at(e, 16),
                        offset: u32_at(e, 20),
                        per_instance: u32_at(e, 24) != 0,
                        step_rate: u32_at(e, 28),
                    });
                }
            }
            // canonical forms, so equal pipelines get equal keys: one blend for every target unless independent;
            // no depth or stencil state without a depth format (the device ignores it then)
            if !s.independent_blend {
                let b = s.blend[0];
                s.blend = [b; 8];
            }
            if s.depth_format == 0 {
                s.depth_stencil = DepthStencil::default();
            }
            Desc::Graphics { vs, ps, state: Box::new(std::mem::take(s)) }
        };
        Ok(Parsed { desc, skipped, first_skipped, flags: self.flags })
    }
}

/// A pipeline state stream as a description. `console` gives the whole subobject size (type included, a
/// multiple of 8) of console types the caller knows the size of; they are skipped and counted.
pub fn parse_stream(stream: &[u8], mem: &impl Memory, n: Numbering, console: &[(u32, usize)]) -> Result<Parsed, Refusal> {
    let mut acc = Acc::stream();
    let (mut skipped, mut first_skipped) = (0, 0);
    let mut at = 0;
    while at < stream.len() {
        let Some(t) = stream.get(at..at + 4) else { return Err(Refusal::Truncated { at, kind: u32::MAX }) };
        let t = u32::from_le_bytes(t.try_into().unwrap());
        match kind(t, n) {
            Some((k, off, len)) => {
                let p = stream.get(at + off..at + off + len).ok_or(Refusal::Truncated { at, kind: t })?;
                acc.apply(k, p);
                at += (off + len + 7) & !7;
            }
            None => {
                let size = match n {
                    Numbering::Console if t >= 25 => console.iter().find(|c| c.0 == t).map(|c| c.1).filter(|&s| s >= 8 && s % 8 == 0),
                    _ => None,
                };
                let Some(size) = size else { return Err(Refusal::UnknownSize { at, kind: t }) };
                if at + size > stream.len() {
                    return Err(Refusal::Truncated { at, kind: t });
                }
                if skipped == 0 {
                    first_skipped = t;
                }
                skipped += 1;
                at += size;
            }
        }
    }
    acc.finish(mem, skipped, first_skipped)
}

/// `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (x64): its size and field offsets.
pub mod graphics_desc {
    pub const SIZE: usize = 656;
    pub const VS: usize = 8;
    pub const PS: usize = 24;
    pub const DS: usize = 40;
    pub const HS: usize = 56;
    pub const GS: usize = 72;
    pub const STREAM_OUTPUT: usize = 88;
    pub const BLEND: usize = 120;
    pub const SAMPLE_MASK: usize = 448;
    pub const RASTER: usize = 452;
    pub const DEPTH_STENCIL: usize = 496;
    pub const INPUT: usize = 552;
    pub const STRIP_CUT: usize = 568;
    pub const TOPOLOGY: usize = 572;
    pub const TARGET_COUNT: usize = 576;
    pub const TARGETS: usize = 580;
    pub const DEPTH_FORMAT: usize = 612;
    pub const SAMPLE_DESC: usize = 616;
    pub const FLAGS: usize = 648;
}

/// A `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (device 0x50) as a description: the same reading as a stream's.
pub fn parse_graphics(d: &[u8], mem: &impl Memory) -> Result<Parsed, Refusal> {
    use graphics_desc as g;
    if d.len() < g::SIZE {
        return Err(Refusal::Short);
    }
    let mut acc = Acc::stream();
    let at = |o: usize, n: usize| &d[o..o + n];
    for (o, s) in [(g::VS, Sh::Vs), (g::PS, Sh::Ps), (g::DS, Sh::Ds), (g::HS, Sh::Hs), (g::GS, Sh::Gs)] {
        acc.apply(Kind::Shader(s), at(o, size::BYTECODE));
    }
    acc.apply(Kind::StreamOutput, at(g::STREAM_OUTPUT, size::STREAM_OUTPUT));
    acc.apply(Kind::Blend, at(g::BLEND, size::BLEND));
    acc.apply(Kind::SampleMask, at(g::SAMPLE_MASK, 4));
    acc.apply(Kind::Raster, at(g::RASTER, size::RASTER));
    acc.apply(Kind::DepthStencil, at(g::DEPTH_STENCIL, size::DEPTH_STENCIL));
    acc.apply(Kind::Input, at(g::INPUT, size::INPUT));
    acc.apply(Kind::StripCut, at(g::STRIP_CUT, 4));
    acc.apply(Kind::Topology, at(g::TOPOLOGY, 4));
    // the RT format array's count comes first here, last in the stream's
    let mut rt = [0u8; size::TARGETS];
    rt[..32].copy_from_slice(at(g::TARGETS, 32));
    rt[32..].copy_from_slice(at(g::TARGET_COUNT, 4));
    acc.apply(Kind::Targets, &rt);
    acc.apply(Kind::DepthFormat, at(g::DEPTH_FORMAT, 4));
    acc.apply(Kind::SampleDesc, at(g::SAMPLE_DESC, 8));
    acc.apply(Kind::Flags, at(g::FLAGS, 4));
    acc.finish(mem, 0, 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::state::Raster;

    /// Memory at made-up addresses: regions of bytes, and strings.
    #[derive(Default)]
    pub struct Fake {
        regions: Vec<(u64, Vec<u8>)>,
        /// addresses whose program hash is given, not computed (programs the library holds by hash)
        named: Vec<(u64, u64)>,
    }

    impl Fake {
        pub fn put(&mut self, b: Vec<u8>) -> u64 {
            let at = 0x1000_0000 + 0x10_0000 * self.regions.len() as u64;
            self.regions.push((at, b));
            at
        }
        pub fn name(&mut self, hash: u64) -> u64 {
            let at = self.put(vec![0; 16]);
            self.named.push((at, hash));
            at
        }
    }

    impl Memory for Fake {
        fn bytes(&self, addr: u64, len: usize) -> Option<&[u8]> {
            self.regions.iter().find(|r| (r.0..r.0 + r.1.len() as u64).contains(&addr)).and_then(|r| r.1.get((addr - r.0) as usize..(addr - r.0) as usize + len))
        }
        fn cstr(&self, addr: u64) -> Option<&[u8]> {
            let r = self.regions.iter().find(|r| (r.0..r.0 + r.1.len() as u64).contains(&addr))?;
            let b = &r.1[(addr - r.0) as usize..];
            Some(&b[..b.iter().take(MAX_NAME).position(|&c| c == 0)?])
        }
        fn program(&self, addr: u64, len: usize) -> Option<u64> {
            match self.named.iter().find(|n| n.0 == addr) {
                Some(n) => Some(n.1),
                None => program_hash(self.bytes(addr, len)?),
            }
        }
    }

    /// A DXBC-shaped container of `n` bytes (its own size declared), with a marker byte.
    pub fn container(marker: u8, n: usize) -> Vec<u8> {
        let mut c = vec![marker; n];
        c[..4].copy_from_slice(b"DXBC");
        c[0x18..0x1c].copy_from_slice(&(n as u32).to_le_bytes());
        c
    }

    /// A stream being built: each subobject 8-byte aligned, the payload at +4 or +8.
    #[derive(Default)]
    pub struct Stream(pub Vec<u8>);

    impl Stream {
        pub fn raw(&mut self, t: u32, at: usize, payload: &[u8]) -> &mut Self {
            let start = self.0.len();
            self.0.extend(t.to_le_bytes());
            self.0.resize(start + at, 0);
            self.0.extend(payload);
            self.0.resize((self.0.len() + 7) & !7, 0);
            self
        }
        pub fn words(&mut self, t: u32, w: &[u32]) -> &mut Self {
            let b: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.raw(t, 4, &b)
        }
        /// A root signature subobject (one pointer).
        pub fn root(&mut self, a: u64) -> &mut Self {
            self.raw(0, 8, &a.to_le_bytes())
        }
        pub fn ptr(&mut self, t: u32, a: u64, n: u64) -> &mut Self {
            let b: Vec<u8> = a.to_le_bytes().into_iter().chain(n.to_le_bytes()).collect();
            self.raw(t, 8, &b)
        }
    }

    pub fn blend_words(b: &[Blend; 8], a2c: bool, independent: bool) -> Vec<u32> {
        let mut w = vec![u32::from(a2c), u32::from(independent)];
        for b in b {
            w.extend([u32::from(b.enable), u32::from(b.logic_enable), b.src, b.dst, b.op, b.src_alpha, b.dst_alpha, b.op_alpha, b.logic_op, u32::from(b.write_mask)]);
        }
        w
    }

    fn ds_words(d: &DepthStencil, bounds: Option<bool>) -> Vec<u32> {
        let mut w = vec![u32::from(d.depth), u32::from(d.depth_write), d.depth_func, u32::from(d.stencil), u32::from(d.read_mask) | u32::from(d.write_mask) << 8];
        for o in [d.front, d.back] {
            w.extend([o.fail, o.depth_fail, o.pass, o.func]);
        }
        w.extend(bounds.map(u32::from));
        w
    }

    fn raster_words(r: &Raster) -> Vec<u32> {
        vec![r.fill, r.cull, u32::from(r.front_ccw), r.depth_bias as u32, r.depth_bias_clamp.to_bits(), r.slope_scaled_depth_bias.to_bits(), u32::from(r.depth_clip), u32::from(r.multisample), u32::from(r.antialiased_line), r.forced_sample_count, u32::from(r.conservative)]
    }

    fn input_elements(m: &mut Fake, s: &GraphicsState) -> u64 {
        let mut e = Vec::new();
        for i in &s.input {
            let mut name = i.name.clone().into_bytes();
            name.push(0);
            e.extend(m.put(name).to_le_bytes());
            for v in [i.index, i.format, i.slot, i.offset, u32::from(i.per_instance), i.step_rate] {
                e.extend(v.to_le_bytes());
            }
        }
        m.put(e)
    }

    /// The state every full stream below says, distinct from every default.
    fn state() -> GraphicsState {
        let mut s = GraphicsState::default();
        s.alpha_to_coverage = true;
        s.independent_blend = true;
        s.blend[0] = Blend { enable: true, src: 5, dst: 6, op: 1, src_alpha: 2, dst_alpha: 1, op_alpha: 1, logic_op: 4, write_mask: 0xf, logic_enable: false };
        s.blend[1].write_mask = 7;
        s.sample_mask = 0xff;
        s.raster = Raster { fill: 2, cull: 2, front_ccw: true, depth_bias: -3, depth_bias_clamp: 0.5, slope_scaled_depth_bias: 1.25, depth_clip: false, multisample: true, antialiased_line: false, forced_sample_count: 0, conservative: true };
        s.depth_stencil = DepthStencil { depth: true, depth_write: false, depth_func: 4, stencil: true, read_mask: 0x0f, write_mask: 0xf0, front: StencilOp { fail: 2, depth_fail: 3, pass: 4, func: 5 }, back: StencilOp { fail: 1, depth_fail: 1, pass: 3, func: 8 } };
        s.input = vec![
            InputElement { name: "POSITION".into(), index: 0, format: 6, slot: 0, offset: 0, per_instance: false, step_rate: 0 },
            InputElement { name: "TEXCOORD".into(), index: 2, format: 16, slot: 1, offset: u32::MAX, per_instance: true, step_rate: 1 },
        ];
        s.strip_cut = 2;
        s.topology = 2;
        s.targets = vec![28, 10, 24];
        s.depth_format = 20;
        s.samples = 4;
        s.sample_quality = 1;
        s
    }

    /// A full VS + PS stream (the subobjects the title's template carries, fork test `streams_yield_their_shaders`:
    /// root signature, five shader slots, blend, sample mask, rasterizer, depth-stencil 1, sample desc, input
    /// layout, topology, render target and depth formats), plus strip cut and flags.
    fn full(m: &mut Fake, s: &GraphicsState, first: u32) -> (Stream, Vec<u8>, Vec<u8>) {
        let (vs, ps) = (container(1, 0x40), container(2, 0x48));
        let (va, pa) = (m.put(vs.clone()), m.put(ps.clone()));
        let input = input_elements(m, s);
        let mut t = Stream::default();
        let mut formats = [0u32; 9];
        formats[..s.targets.len()].copy_from_slice(&s.targets);
        formats[8] = s.targets.len() as u32;
        t.root(0xdead_0000)
            .ptr(first, va, vs.len() as u64)
            .ptr(2, pa, ps.len() as u64)
            .ptr(3, 0, 0)
            .ptr(4, 0, 0)
            .ptr(5, 0, 0)
            .words(8, &blend_words(&s.blend, s.alpha_to_coverage, s.independent_blend))
            .words(9, &[s.sample_mask])
            .words(10, &raster_words(&s.raster))
            .words(21, &ds_words(&s.depth_stencil, Some(false)))
            .words(17, &[s.samples, s.sample_quality])
            .ptr(12, input, s.input.len() as u64)
            .words(14, &[s.topology])
            .words(15, &formats)
            .words(16, &[s.depth_format])
            .words(13, &[s.strip_cut])
            .words(20, &[4]);
        (t, vs, ps)
    }

    fn graphics(p: &Parsed) -> (u64, Option<u64>, &GraphicsState) {
        match &p.desc {
            Desc::Graphics { vs, ps, state } => (*vs, *ps, state),
            d => panic!("not graphics: {d:?}"),
        }
    }

    #[test]
    fn a_pc_stream_maps_every_field() {
        let mut m = Fake::default();
        let s = state();
        let (t, vs, ps) = full(&mut m, &s, 1);
        let p = parse_stream(&t.0, &m, Numbering::Pc, &[]).unwrap();
        let (v, pp, got) = graphics(&p);
        assert_eq!((v, pp), (program_hash(&vs).unwrap(), Some(program_hash(&ps).unwrap())));
        assert_eq!(got, &s);
        assert_eq!((p.flags, p.skipped), (4, 0));
        // the console reads the same stream the same way (no AS/MS, no console types)
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &[]).unwrap(), p);
    }

    #[test]
    fn a_stream_leaves_d3d12_defaults_and_canonical_forms() {
        let mut m = Fake::default();
        let vs = container(1, 0x20);
        let va = m.put(vs.clone());
        let mut t = Stream::default();
        t.ptr(1, va, 0x20);
        let p = parse_stream(&t.0, &m, Numbering::Console, &[]).unwrap();
        let (_, ps, s) = graphics(&p);
        assert_eq!(ps, None, "depth only");
        assert_eq!(s.raster.cull, 3, "back faces culled when the stream says nothing");
        assert_eq!(s.depth_stencil, DepthStencil::default(), "no depth format: no depth state");
        // depth on by default once a depth format is set
        t.words(16, &[40]);
        let p = parse_stream(&t.0, &m, Numbering::Console, &[]).unwrap();
        assert!(graphics(&p).2.depth_stencil.depth);
        // blend: not independent, so only the first target's counts
        let mut b = [Blend::default(); 8];
        b[3].write_mask = 1;
        let mut u = Stream::default();
        u.ptr(1, va, 0x20).words(8, &blend_words(&b, false, false));
        let p = parse_stream(&u.0, &m, Numbering::Console, &[]).unwrap();
        assert_eq!(graphics(&p).2.blend, [Blend::default(); 8]);
    }

    #[test]
    fn depth_stencil_kinds_and_rasterizer_kinds() {
        let mut m = Fake::default();
        let va = m.put(container(1, 0x20));
        let d = state().depth_stencil;
        // DEPTH_STENCIL (11), and PC's DEPTH_STENCIL2 (26: per-face masks, the front face's kept)
        let mut a = Stream::default();
        a.ptr(1, va, 0x20).words(16, &[40]).words(11, &ds_words(&d, None));
        let ds2 = [1, 0, 4, 1, 2, 3, 4, 5, 0xf00f, 1, 1, 3, 8, 0xffff, 0];
        let mut b = Stream::default();
        b.ptr(1, va, 0x20).words(16, &[40]).words(26, &ds2);
        let (pa, pb) = (parse_stream(&a.0, &m, Numbering::Pc, &[]).unwrap(), parse_stream(&b.0, &m, Numbering::Pc, &[]).unwrap());
        assert_eq!(graphics(&pa).2.depth_stencil, d);
        let want = DepthStencil { read_mask: 0x0f, write_mask: 0xf0, ..d };
        assert_eq!(graphics(&pb).2.depth_stencil, want);
        // RASTERIZER1 (27, float bias) and RASTERIZER2 (28, line mode)
        let mut r1 = raster_words(&state().raster);
        r1[3] = (-3.0f32).to_bits();
        let mut c = Stream::default();
        c.ptr(1, va, 0x20).words(27, &r1);
        assert_eq!(graphics(&parse_stream(&c.0, &m, Numbering::Pc, &[]).unwrap()).2.raster, state().raster);
        let r2 = [3, 1, 0, 0, 0, 0, 1, 2, 0, 0];
        let mut e = Stream::default();
        e.ptr(1, va, 0x20).words(28, &r2);
        let r = graphics(&parse_stream(&e.0, &m, Numbering::Pc, &[]).unwrap()).2.raster;
        assert!(r.multisample && !r.antialiased_line);
        // on the console 26-28 are console types
        assert_eq!(parse_stream(&b.0, &m, Numbering::Console, &[]), Err(Refusal::UnknownSize { at: 32, kind: 26 }));
    }

    #[test]
    fn console_numbers_amplification_and_mesh_one_lower() {
        let mut m = Fake::default();
        let ms = m.put(container(3, 0x20));
        let mut t = Stream::default();
        t.ptr(24, ms, 0x20);
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &[]), Err(Refusal::Stage("mesh")));
        assert_eq!(parse_stream(&t.0, &m, Numbering::Pc, &[]), Err(Refusal::Stage("amplification")));
        let mut a = Stream::default();
        a.ptr(23, ms, 0x20);
        assert_eq!(parse_stream(&a.0, &m, Numbering::Console, &[]), Err(Refusal::Stage("amplification")));
        assert_eq!(parse_stream(&a.0, &m, Numbering::Pc, &[]), Err(Refusal::UnknownSize { at: 0, kind: 23 }), "PC has no type 23");
        let mut p = Stream::default();
        p.ptr(25, ms, 0x20);
        assert_eq!(parse_stream(&p.0, &m, Numbering::Pc, &[]), Err(Refusal::Stage("mesh")));
        // the title's template: the first shader slot as a mesh shader (fork test, title+0x86f090)
        let (t, ..) = full(&mut m, &state(), 24);
        assert_eq!(t.0.len(), 0x2b0 + 0x10, "the title's 0x2b0-byte template plus strip cut and flags");
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &[]), Err(Refusal::Stage("mesh")));
        // an empty mesh slot is no mesh shader
        let mut e = Stream::default();
        e.ptr(24, 0, 0).ptr(1, ms, 0x20);
        assert!(parse_stream(&e.0, &m, Numbering::Console, &[]).is_ok());
    }

    #[test]
    fn unknown_console_types_are_skipped_by_known_size_and_refused_without_one() {
        let mut m = Fake::default();
        let (vs, ps) = (container(1, 0x20), container(2, 0x20));
        let (va, pa) = (m.put(vs.clone()), m.put(ps.clone()));
        let mut t = Stream::default();
        t.ptr(1, va, 0x20).raw(25, 4, &[0xee; 12]).raw(31, 8, &[0xdd; 24]).raw(25, 4, &[0xee; 12]).ptr(2, pa, 0x20);
        let sizes = [(25, 16), (31, 32)];
        let p = parse_stream(&t.0, &m, Numbering::Console, &sizes).unwrap();
        assert_eq!((p.skipped, p.first_skipped), (3, 25));
        assert_eq!(graphics(&p).1, Some(program_hash(&ps).unwrap()), "parsing went on past them");
        // no size for 31: the parse stops there, nothing after it is read
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &sizes[..1]), Err(Refusal::UnknownSize { at: 40, kind: 31 }));
        // a size that is not a whole number of 8-byte units is no size
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &[(25, 12), (31, 32)]), Err(Refusal::UnknownSize { at: 24, kind: 25 }));
        // the console's table never applies to PC numbering, nor below 25
        assert!(parse_stream(&t.0, &m, Numbering::Pc, &sizes).is_err(), "PC 25 is MS, and its payload is no stream");
    }

    #[test]
    fn a_truncated_stream_is_refused_at_every_length() {
        let mut m = Fake::default();
        let (t, ..) = full(&mut m, &state(), 1);
        // (start + payload end, start + whole size) of each subobject: a prefix ending inside that range holds
        // every subobject whole (padding after the last payload is not needed)
        let mut whole = vec![(0, 0)];
        let mut at = 0;
        while at < t.0.len() {
            let k = u32::from_le_bytes(t.0[at..at + 4].try_into().unwrap());
            let (_, off, len) = kind(k, Numbering::Pc).unwrap();
            whole.push((at + off + len, at + subobject_size(k, Numbering::Pc).unwrap()));
            at = whole.last().unwrap().1;
        }
        for n in 0..t.0.len() {
            let r = parse_stream(&t.0[..n], &m, Numbering::Pc, &[]);
            let truncated = matches!(r, Err(Refusal::Truncated { .. }));
            assert_eq!(truncated, !whole.iter().any(|&(a, b)| (a..=b).contains(&n)), "{n}: {r:?}");
            if !truncated && n >= 0x28 {
                assert!(r.is_ok(), "{n}: {r:?}");
            }
        }
        // a short final subobject whose payload is whole still parses (no padding after it)
        let mut u = Stream::default();
        u.ptr(1, m.put(container(1, 0x20)), 0x20).words(17, &[2, 0]);
        assert!(parse_stream(&u.0[..u.0.len() - 4], &m, Numbering::Pc, &[]).is_ok());
    }

    #[test]
    fn compute_streams_and_bad_programs() {
        let mut m = Fake::default();
        let cs = container(9, 0x30);
        let ca = m.put(cs.clone());
        let mut t = Stream::default();
        t.root(1).ptr(6, ca, 0x30).words(18, &[1]);
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &[]).unwrap().desc, Desc::Compute { cs: program_hash(&cs).unwrap() });
        t.ptr(1, ca, 0x30);
        assert_eq!(parse_stream(&t.0, &m, Numbering::Console, &[]), Err(Refusal::Mixed));
        let bad = m.put(b"DXIL and more bytes than a header, 32 at least".to_vec());
        let mut b = Stream::default();
        b.ptr(1, bad, 0x20);
        assert_eq!(parse_stream(&b.0, &m, Numbering::Console, &[]), Err(Refusal::Program("vs")));
        assert_eq!(parse_stream(&[], &m, Numbering::Console, &[]), Err(Refusal::NoShader));
        let mut g = Stream::default();
        g.ptr(1, ca, 0x30).ptr(5, ca, 0x30);
        assert_eq!(parse_stream(&g.0, &m, Numbering::Console, &[]), Err(Refusal::Stage("geometry")));
        let mut i = Stream::default();
        i.ptr(1, ca, 0x30).ptr(12, 0, 3);
        assert_eq!(parse_stream(&i.0, &m, Numbering::Console, &[]), Err(Refusal::Input));
        let mut r = Stream::default();
        r.ptr(1, ca, 0x30).words(15, &[0, 0, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(parse_stream(&r.0, &m, Numbering::Console, &[]), Err(Refusal::Targets));
    }

    /// A `D3D12_GRAPHICS_PIPELINE_STATE_DESC` built field by field at the documented offsets.
    pub fn graphics_desc_bytes(vs: (u64, u64), ps: (u64, u64), input: u64, s: &GraphicsState) -> Vec<u8> {
        use graphics_desc as g;
        let mut d = vec![0u8; g::SIZE];
        let mut put = |at: usize, w: &[u32]| {
            for (i, v) in w.iter().enumerate() {
                d[at + 4 * i..at + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
            }
        };
        let q = |v: u64| [v as u32, (v >> 32) as u32];
        put(g::VS, &[q(vs.0), q(vs.1)].concat());
        put(g::PS, &[q(ps.0), q(ps.1)].concat());
        put(g::BLEND, &blend_words(&s.blend, s.alpha_to_coverage, s.independent_blend));
        put(g::SAMPLE_MASK, &[s.sample_mask]);
        put(g::RASTER, &raster_words(&s.raster));
        put(g::DEPTH_STENCIL, &ds_words(&s.depth_stencil, None));
        put(g::INPUT, &[q(input), [s.input.len() as u32, 0]].concat());
        put(g::STRIP_CUT, &[s.strip_cut, s.topology, s.targets.len() as u32]);
        put(g::TARGETS, &s.targets);
        put(g::DEPTH_FORMAT, &[s.depth_format, s.samples, s.sample_quality]);
        d
    }

    #[test]
    fn a_graphics_desc_maps_like_a_stream() {
        let mut m = Fake::default();
        let s = state();
        let (t, vs, ps) = full(&mut m, &s, 1);
        let from_stream = parse_stream(&t.0, &m, Numbering::Pc, &[]).unwrap();
        let (va, pa) = (m.put(vs.clone()), m.put(ps.clone()));
        let input = input_elements(&mut m, &s);
        let d = graphics_desc_bytes((va, vs.len() as u64), (pa, ps.len() as u64), input, &s);
        let p = parse_graphics(&d, &m).unwrap();
        assert_eq!(p.desc, from_stream.desc);
        assert_eq!(parse_graphics(&d[..100], &m), Err(Refusal::Short));
    }

    #[cfg(windows)]
    #[test]
    fn sizes_and_offsets_match_the_host_headers() {
        use std::mem::{offset_of, size_of};
        use windows::Win32::Graphics::Direct3D12::*;
        assert_eq!(size_of::<D3D12_SHADER_BYTECODE>(), size::BYTECODE);
        assert_eq!(size_of::<D3D12_STREAM_OUTPUT_DESC>(), size::STREAM_OUTPUT);
        assert_eq!(size_of::<D3D12_BLEND_DESC>(), size::BLEND);
        assert_eq!(size_of::<D3D12_RASTERIZER_DESC>(), size::RASTER);
        assert_eq!(size_of::<D3D12_RASTERIZER_DESC1>(), size::RASTER);
        assert_eq!(size_of::<D3D12_RASTERIZER_DESC2>(), size::RASTER2);
        assert_eq!(size_of::<D3D12_DEPTH_STENCIL_DESC>(), size::DEPTH_STENCIL);
        assert_eq!(size_of::<D3D12_DEPTH_STENCIL_DESC1>(), size::DEPTH_STENCIL1);
        assert_eq!(size_of::<D3D12_DEPTH_STENCIL_DESC2>(), size::DEPTH_STENCIL2);
        assert_eq!(offset_of!(D3D12_DEPTH_STENCIL_DESC2, BackFace), 36);
        assert_eq!(offset_of!(D3D12_DEPTH_STENCILOP_DESC1, StencilReadMask), 16);
        assert_eq!(size_of::<D3D12_INPUT_LAYOUT_DESC>(), size::INPUT);
        assert_eq!(size_of::<D3D12_INPUT_ELEMENT_DESC>(), size::INPUT_ELEMENT);
        assert_eq!(size_of::<D3D12_RT_FORMAT_ARRAY>(), size::TARGETS);
        assert_eq!(size_of::<D3D12_VIEW_INSTANCING_DESC>(), size::VIEW_INSTANCING);
        type G = D3D12_GRAPHICS_PIPELINE_STATE_DESC;
        use graphics_desc as g;
        assert_eq!(size_of::<G>(), g::SIZE);
        let want = [
            (offset_of!(G, VS), g::VS),
            (offset_of!(G, PS), g::PS),
            (offset_of!(G, DS), g::DS),
            (offset_of!(G, HS), g::HS),
            (offset_of!(G, GS), g::GS),
            (offset_of!(G, StreamOutput), g::STREAM_OUTPUT),
            (offset_of!(G, BlendState), g::BLEND),
            (offset_of!(G, SampleMask), g::SAMPLE_MASK),
            (offset_of!(G, RasterizerState), g::RASTER),
            (offset_of!(G, DepthStencilState), g::DEPTH_STENCIL),
            (offset_of!(G, InputLayout), g::INPUT),
            (offset_of!(G, IBStripCutValue), g::STRIP_CUT),
            (offset_of!(G, PrimitiveTopologyType), g::TOPOLOGY),
            (offset_of!(G, NumRenderTargets), g::TARGET_COUNT),
            (offset_of!(G, RTVFormats), g::TARGETS),
            (offset_of!(G, DSVFormat), g::DEPTH_FORMAT),
            (offset_of!(G, SampleDesc), g::SAMPLE_DESC),
            (offset_of!(G, Flags), g::FLAGS),
        ];
        for (host, ours) in want {
            assert_eq!(host, ours);
        }
        // subobject numbers: PC's own, from the host headers
        for (t, k) in [
            (D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS, "AS"),
            (D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, "MS"),
            (D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2, "DS2"),
            (D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2, "R2"),
            (D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING, "VI"),
        ] {
            assert!(subobject_size(t.0 as u32, Numbering::Pc).is_some(), "{k}");
        }
        // these headers end at RASTERIZER2 (28); SERIALIZED_ROOT_SIGNATURE (29) is the newer Agility headers'
        assert_eq!(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MAX_VALID.0, 29, "a new PC type to size");
    }

    #[test]
    fn subobject_sizes_match_the_titles_template() {
        // fork test layout (title+0x86f090): offsets of each subobject in the 0x2b0-byte stream
        let order = [0, 24, 2, 3, 4, 5, 8, 9, 10, 21, 17, 12, 14, 15, 16];
        let at: Vec<usize> = order.iter().scan(0, |a, &t| {
            let here = *a;
            *a += subobject_size(t, Numbering::Console).unwrap();
            Some(here)
        }).collect();
        assert_eq!(at, [0, 0x10, 0x28, 0x40, 0x58, 0x70, 0x88, 0x1d8, 0x1e0, 0x210, 0x250, 0x260, 0x278, 0x280, 0x2a8]);
        assert_eq!(at[14] + subobject_size(16, Numbering::Console).unwrap(), 0x2b0);
    }
}

/// On the card: a stream naming a vertex and pixel shader pair from the program library
/// (`REBUILD_PIPELINES_LIB`, else `D:\victory-data\pipelines\library.bin`) gives a pipeline. Skipped while
/// `agents\MEASURING` exists, without a library, or without an SM 6.6 device.
#[cfg(all(test, windows))]
mod gpu {
    use super::tests::{Fake, Stream};
    use super::*;
    use crate::bindings;
    use crate::library::Library;
    use crate::pool::Creator;
    use crate::pso::{Programs, PsoCreator};
    use crate::state::Key;
    use crate::Stage;
    use rebuild_shader::signature::Element;
    use std::path::{Path, PathBuf};

    const LOCK: &str = r"D:\Developer\triumph-rebuild\agents\MEASURING";

    fn links(vs_out: &[Element], ps_in: &[Element]) -> bool {
        ps_in.iter().filter(|e| !matches!(e.system, 7 | 9 | 10 | 66)).all(|e| {
            vs_out.iter().any(|o| o.name.eq_ignore_ascii_case(&e.name) && o.index == e.index && o.register == e.register && o.comp == e.comp && o.mask & e.mask == e.mask)
        })
    }

    #[test]
    fn a_stream_naming_a_library_pair_gives_a_pipeline() {
        if Path::new(LOCK).exists() {
            eprintln!("skipped: the measurement lock is present");
            return;
        }
        let path = std::env::var_os("REBUILD_PIPELINES_LIB").map_or_else(|| PathBuf::from(r"D:\victory-data\pipelines\library.bin"), PathBuf::from);
        let lib = match Library::load(&path, 4, false) {
            Ok((l, _)) => l,
            Err(e) => {
                eprintln!("skipped: no program library at {}: {e}", path.display());
                return;
            }
        };
        let device = match crate::d3d::Gpu::new(false) {
            Ok(g) => g.device,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
        };
        // the first pixel shader with a vertex shader whose outputs link
        let vss: Vec<_> = lib.iter().filter(|e| e.stage == Stage::Vs).map(|e| (e.hash, bindings::read(e.dxil).unwrap())).collect();
        let (vs, vc, ps, pc) = lib
            .iter()
            .filter(|e| e.stage == Stage::Ps)
            .find_map(|p| {
                let pc = bindings::read(p.dxil).ok()?;
                let (v, vc) = vss.iter().find(|(_, vc)| links(&vc.outputs, &pc.inputs))?;
                Some((*v, vc.clone(), p.hash, pc))
            })
            .expect("a linking pair in the library");
        // the stream, as the title builds one: names, elements and programs at real addresses
        let mut m = Fake::default();
        let (va, pa) = (m.name(vs), m.name(ps));
        let inputs: Vec<&Element> = vc.inputs.iter().filter(|e| e.system == 0).collect();
        let mut elements = Vec::new();
        for e in &inputs {
            let mut n = e.name.clone().into_bytes();
            n.push(0);
            elements.extend(m.put(n).to_le_bytes());
            let format = crate::d3d::format(e.comp, 8 - e.mask.leading_zeros()).0 as u32;
            for v in [e.index, format, 0, u32::MAX, 0, 0] {
                elements.extend(v.to_le_bytes());
            }
        }
        let input = m.put(elements);
        let mut formats = [0u32; 9];
        let mut depth = 0;
        for e in &pc.outputs {
            match e.system {
                64 if e.index < 8 => {
                    formats[e.index as usize] = crate::d3d::target_format(e.comp, 8 - e.mask.leading_zeros()).0 as u32;
                    formats[8] = formats[8].max(e.index + 1);
                }
                65 | 67 | 68 => depth = 40,
                _ => {}
            }
        }
        let mut t = Stream::default();
        t.root(0x1234).ptr(1, va, 16).ptr(2, pa, 16).ptr(24, 0, 0).ptr(12, input, inputs.len() as u64).words(14, &[3]).words(15, &formats).words(16, &[depth]).words(10, &[3, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let p = parse_stream(&t.0, &m, Numbering::Console, &[]).unwrap();
        assert_eq!(Key::of(&p.desc).shaders, [vs, ps]);
        let creator = PsoCreator::new(&device, Programs::new(&device, lib, None), None);
        let r = creator.create(&Key::of(&p.desc), &p.desc);
        assert!(r.is_ok(), "vs {vs:016x} ps {ps:016x}: {:?}", r.err());
        eprintln!("pipeline from a stream: vs {vs:016x} + ps {ps:016x}, {} inputs, {} targets", inputs.len(), formats[8]);
    }
}
