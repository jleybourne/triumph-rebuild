//! A pipeline as plain data: which programs (by hash, from the [`crate::library`]) and the fixed-function state.
//! The device turns the title's console descriptions into a [`Desc`]; the creator turns a `Desc` into a host
//! pipeline. A `Desc` encodes to bytes deterministically, so a session can record the pipelines it used and the
//! next one can create them again before the title asks ([`crate::session`]).
//!
//! Numbers are the host's own enumerations (`D3D12_BLEND`, `DXGI_FORMAT`, ...), kept as `u32` so this module
//! builds and is tested without Windows.

use rebuild_shader::blake3;

/// One render target's blend (`D3D12_RENDER_TARGET_BLEND_DESC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blend {
    pub enable: bool,
    pub logic_enable: bool,
    pub src: u32,
    pub dst: u32,
    pub op: u32,
    pub src_alpha: u32,
    pub dst_alpha: u32,
    pub op_alpha: u32,
    pub logic_op: u32,
    pub write_mask: u8,
}

impl Default for Blend {
    /// Blending off, every channel written (ONE, ZERO, ADD; logic NOOP).
    fn default() -> Blend {
        Blend { enable: false, logic_enable: false, src: 2, dst: 1, op: 1, src_alpha: 2, dst_alpha: 1, op_alpha: 1, logic_op: 4, write_mask: 0xf }
    }
}

/// `D3D12_RASTERIZER_DESC`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Raster {
    pub fill: u32,
    pub cull: u32,
    pub front_ccw: bool,
    pub depth_bias: i32,
    pub depth_bias_clamp: f32,
    pub slope_scaled_depth_bias: f32,
    pub depth_clip: bool,
    pub multisample: bool,
    pub antialiased_line: bool,
    pub forced_sample_count: u32,
    pub conservative: bool,
}

impl Default for Raster {
    /// Solid, no culling, depth clip on.
    fn default() -> Raster {
        Raster {
            fill: 3,
            cull: 1,
            front_ccw: false,
            depth_bias: 0,
            depth_bias_clamp: 0.0,
            slope_scaled_depth_bias: 0.0,
            depth_clip: true,
            multisample: false,
            antialiased_line: false,
            forced_sample_count: 0,
            conservative: false,
        }
    }
}

/// `D3D12_DEPTH_STENCILOP_DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StencilOp {
    pub fail: u32,
    pub depth_fail: u32,
    pub pass: u32,
    pub func: u32,
}

impl Default for StencilOp {
    /// KEEP, KEEP, KEEP, ALWAYS.
    fn default() -> StencilOp {
        StencilOp { fail: 1, depth_fail: 1, pass: 1, func: 8 }
    }
}

/// `D3D12_DEPTH_STENCIL_DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthStencil {
    pub depth: bool,
    pub depth_write: bool,
    pub depth_func: u32,
    pub stencil: bool,
    pub read_mask: u8,
    pub write_mask: u8,
    pub front: StencilOp,
    pub back: StencilOp,
}

impl Default for DepthStencil {
    /// Depth and stencil off (depth func LESS, masks 0xff).
    fn default() -> DepthStencil {
        DepthStencil { depth: false, depth_write: true, depth_func: 2, stencil: false, read_mask: 0xff, write_mask: 0xff, front: StencilOp::default(), back: StencilOp::default() }
    }
}

/// `D3D12_INPUT_ELEMENT_DESC`, owning its semantic name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputElement {
    pub name: String,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub per_instance: bool,
    pub step_rate: u32,
}

/// Everything in a graphics pipeline that is not a program or the root signature.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphicsState {
    pub alpha_to_coverage: bool,
    pub independent_blend: bool,
    pub blend: [Blend; 8],
    pub sample_mask: u32,
    pub raster: Raster,
    pub depth_stencil: DepthStencil,
    pub input: Vec<InputElement>,
    /// `D3D12_INDEX_BUFFER_STRIP_CUT_VALUE`.
    pub strip_cut: u32,
    /// `D3D12_PRIMITIVE_TOPOLOGY_TYPE`.
    pub topology: u32,
    /// `DXGI_FORMAT` per render target (the count is the length, at most 8).
    pub targets: Vec<u32>,
    pub depth_format: u32,
    pub samples: u32,
    pub sample_quality: u32,
}

impl Default for GraphicsState {
    /// Triangles, one sample, no targets, no input.
    fn default() -> GraphicsState {
        GraphicsState {
            alpha_to_coverage: false,
            independent_blend: false,
            blend: [Blend::default(); 8],
            sample_mask: u32::MAX,
            raster: Raster::default(),
            depth_stencil: DepthStencil::default(),
            input: Vec::new(),
            strip_cut: 0,
            topology: 3,
            targets: Vec::new(),
            depth_format: 0,
            samples: 1,
            sample_quality: 0,
        }
    }
}

/// A pipeline: its programs by hash, and its state.
#[derive(Debug, Clone, PartialEq)]
pub enum Desc {
    Compute { cs: u64 },
    /// `ps: None` for a pipeline with no pixel shader (depth only).
    Graphics { vs: u64, ps: Option<u64>, state: Box<GraphicsState> },
}

/// What a pipeline is known by: its programs' hashes and a hash of its state. Equal keys are the same
/// pipeline; requests are deduplicated by key and the pipeline library stores by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub compute: bool,
    /// Compute: `[cs, 0]`. Graphics: `[vs, ps or 0]`.
    pub shaders: [u64; 2],
    /// Compute: 0. Graphics: the first 8 bytes of the BLAKE3 of the state's encoding, or the caller's own.
    pub state: u64,
}

impl Key {
    /// The key a description has.
    pub fn of(d: &Desc) -> Key {
        match d {
            Desc::Compute { cs } => Key { compute: true, shaders: [*cs, 0], state: 0 },
            Desc::Graphics { vs, ps, state } => {
                let mut e = Vec::with_capacity(256);
                encode_state(&mut e, state);
                let h = blake3::hash(&e);
                Key { compute: false, shaders: [*vs, ps.unwrap_or(0)], state: u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) }
            }
        }
    }

    /// The name the pipeline library stores it under: `c<cs>` or `g<vs><ps><state>`, hex.
    pub fn name(&self) -> String {
        if self.compute {
            format!("c{:016x}", self.shaders[0])
        } else {
            format!("g{:016x}{:016x}{:016x}", self.shaders[0], self.shaders[1], self.state)
        }
    }
}

struct W<'a>(&'a mut Vec<u8>);

impl W<'_> {
    fn u(&mut self, v: u32) {
        self.0.extend(v.to_le_bytes());
    }
    fn b(&mut self, v: bool) {
        self.0.push(u8::from(v));
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn u(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn q(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn b(&mut self) -> Option<bool> {
        match self.take(1)?[0] {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
}

fn encode_state(out: &mut Vec<u8>, s: &GraphicsState) {
    let mut w = W(out);
    w.b(s.alpha_to_coverage);
    w.b(s.independent_blend);
    for b in &s.blend {
        w.b(b.enable);
        w.b(b.logic_enable);
        for v in [b.src, b.dst, b.op, b.src_alpha, b.dst_alpha, b.op_alpha, b.logic_op] {
            w.u(v);
        }
        w.0.push(b.write_mask);
    }
    w.u(s.sample_mask);
    let r = &s.raster;
    w.u(r.fill);
    w.u(r.cull);
    w.b(r.front_ccw);
    w.u(r.depth_bias as u32);
    w.u(r.depth_bias_clamp.to_bits());
    w.u(r.slope_scaled_depth_bias.to_bits());
    w.b(r.depth_clip);
    w.b(r.multisample);
    w.b(r.antialiased_line);
    w.u(r.forced_sample_count);
    w.b(r.conservative);
    let d = &s.depth_stencil;
    w.b(d.depth);
    w.b(d.depth_write);
    w.u(d.depth_func);
    w.b(d.stencil);
    w.0.push(d.read_mask);
    w.0.push(d.write_mask);
    for o in [d.front, d.back] {
        for v in [o.fail, o.depth_fail, o.pass, o.func] {
            w.u(v);
        }
    }
    w.u(s.input.len() as u32);
    for e in &s.input {
        w.u(e.name.len() as u32);
        w.0.extend(e.name.as_bytes());
        for v in [e.index, e.format, e.slot, e.offset] {
            w.u(v);
        }
        w.b(e.per_instance);
        w.u(e.step_rate);
    }
    w.u(s.strip_cut);
    w.u(s.topology);
    w.u(s.targets.len() as u32);
    for t in &s.targets {
        w.u(*t);
    }
    for v in [s.depth_format, s.samples, s.sample_quality] {
        w.u(v);
    }
}

fn decode_state(r: &mut R) -> Option<GraphicsState> {
    let mut s = GraphicsState { alpha_to_coverage: r.b()?, independent_blend: r.b()?, ..Default::default() };
    for b in &mut s.blend {
        b.enable = r.b()?;
        b.logic_enable = r.b()?;
        b.src = r.u()?;
        b.dst = r.u()?;
        b.op = r.u()?;
        b.src_alpha = r.u()?;
        b.dst_alpha = r.u()?;
        b.op_alpha = r.u()?;
        b.logic_op = r.u()?;
        b.write_mask = r.byte()?;
    }
    s.sample_mask = r.u()?;
    s.raster = Raster {
        fill: r.u()?,
        cull: r.u()?,
        front_ccw: r.b()?,
        depth_bias: r.u()? as i32,
        depth_bias_clamp: f32::from_bits(r.u()?),
        slope_scaled_depth_bias: f32::from_bits(r.u()?),
        depth_clip: r.b()?,
        multisample: r.b()?,
        antialiased_line: r.b()?,
        forced_sample_count: r.u()?,
        conservative: r.b()?,
    };
    let mut d = DepthStencil { depth: r.b()?, depth_write: r.b()?, depth_func: r.u()?, stencil: r.b()?, read_mask: r.byte()?, write_mask: r.byte()?, ..Default::default() };
    for o in [&mut d.front, &mut d.back] {
        *o = StencilOp { fail: r.u()?, depth_fail: r.u()?, pass: r.u()?, func: r.u()? };
    }
    s.depth_stencil = d;
    let n = r.u()? as usize;
    if n > 32 {
        return None;
    }
    for _ in 0..n {
        let len = r.u()? as usize;
        let name = String::from_utf8(r.take(len)?.to_vec()).ok()?;
        s.input.push(InputElement { name, index: r.u()?, format: r.u()?, slot: r.u()?, offset: r.u()?, per_instance: r.b()?, step_rate: r.u()? });
    }
    s.strip_cut = r.u()?;
    s.topology = r.u()?;
    let n = r.u()? as usize;
    if n > 8 {
        return None;
    }
    for _ in 0..n {
        s.targets.push(r.u()?);
    }
    s.depth_format = r.u()?;
    s.samples = r.u()?;
    s.sample_quality = r.u()?;
    Some(s)
}

impl Desc {
    /// The description as bytes: a tag (0 compute, 1 graphics, 2 graphics with no pixel shader), the program
    /// hashes, then the state.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Desc::Compute { cs } => {
                out.push(0);
                out.extend(cs.to_le_bytes());
            }
            Desc::Graphics { vs, ps, state } => {
                out.push(if ps.is_some() { 1 } else { 2 });
                out.extend(vs.to_le_bytes());
                if let Some(p) = ps {
                    out.extend(p.to_le_bytes());
                }
                encode_state(out, state);
            }
        }
    }

    /// Reads what [`Desc::encode`] wrote; `None` for anything malformed or with bytes left over.
    pub fn decode(b: &[u8]) -> Option<Desc> {
        let mut r = R(b);
        let d = match r.byte()? {
            0 => Desc::Compute { cs: r.q()? },
            t @ (1 | 2) => {
                let vs = r.q()?;
                let ps = if t == 1 { Some(r.q()?) } else { None };
                Desc::Graphics { vs, ps, state: Box::new(decode_state(&mut r)?) }
            }
            _ => return None,
        };
        r.0.is_empty().then_some(d)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample() -> Desc {
        let mut s = GraphicsState::default();
        s.blend[0] = Blend { enable: true, src: 5, dst: 6, ..Default::default() };
        s.raster.depth_bias = -7;
        s.raster.slope_scaled_depth_bias = 1.5;
        s.depth_stencil.depth = true;
        s.depth_stencil.front.func = 3;
        s.input = vec![
            InputElement { name: "POSITION".into(), index: 0, format: 6, slot: 0, offset: 0, per_instance: false, step_rate: 0 },
            InputElement { name: "TEXCOORD".into(), index: 3, format: 16, slot: 1, offset: 12, per_instance: true, step_rate: 1 },
        ];
        s.targets = vec![28, 10];
        s.depth_format = 40;
        Desc::Graphics { vs: 0x1111, ps: Some(0x2222), state: Box::new(s) }
    }

    #[test]
    fn descriptions_round_trip() {
        let depth_only = Desc::Graphics { vs: 9, ps: None, state: Box::default() };
        for d in [sample(), Desc::Compute { cs: 0xabcdef }, depth_only] {
            let mut b = Vec::new();
            d.encode(&mut b);
            assert_eq!(Desc::decode(&b).as_ref(), Some(&d));
            b.push(0);
            assert_eq!(Desc::decode(&b), None, "bytes left over");
            b.truncate(b.len() - 2);
            assert_eq!(Desc::decode(&b), None, "short");
        }
    }

    #[test]
    fn keys_follow_the_state() {
        let a = sample();
        let mut b = a.clone();
        assert_eq!(Key::of(&a), Key::of(&b));
        if let Desc::Graphics { state, .. } = &mut b {
            state.blend[3].write_mask = 1;
        }
        let (ka, kb) = (Key::of(&a), Key::of(&b));
        assert_ne!(ka, kb);
        assert_eq!(ka.shaders, kb.shaders);
        assert_eq!(Key::of(&Desc::Compute { cs: 5 }).name(), "c0000000000000005");
        assert_eq!(ka.name().len(), 49);
    }
}
