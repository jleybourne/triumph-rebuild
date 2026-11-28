//! The geometry stage that draws the console's rectangle lists (list topology 17, pipeline topology type 19) on
//! the host, which has no such topology.
//!
//! A rectangle comes as three vertices, three corners of a screen-aligned rectangle; the console's hardware draws
//! the whole rectangle, continuing every vertex output linearly into the fourth corner. A pipeline the title makes
//! for rectangles is built as a triangle pipeline with this geometry shader behind its vertex shader: each
//! rectangle reaches it as one triangle, and it emits a four-vertex strip, the fourth vertex `a + b - corner` for
//! the position and every float output. Vertex counts, vertex numbers, indices and indirect arguments stay the
//! title's, so direct, indexed and indirect draws alike work, and so do vertex shaders that make their positions
//! from the vertex number (no vertex buffer for the CPU to expand).
//!
//! The corner is the vertex that shares x with one of the others and y with the other (the title's full-screen
//! rectangles come as top left, bottom left, bottom right: the second; its tiled ones as top left, bottom left,
//! top right: the first). That holds in clip space when all three have one positive w (then it is the same on
//! the screen); three vertices that differ in w, coincide, or have no such corner stay the triangle they make.
//! A rectangle that covers the whole clip square (the title's full-screen passes) is emitted as one triangle
//! instead, its legs from the corner twice as long, every float output continued the same way; the clipper cuts it
//! to the screen (inside the guard band, GPUs rasterise it whole). Then no 2x2 pixel quad of the rectangle is split
//! between two triangles, as on the console, which rasterises a rectangle as one primitive: two triangles leave
//! helper lanes in every quad along their shared diagonal (shading the diagonal's quads twice, and lanes a
//! translated pixel shader's derivatives and wave operations could see), and a full-screen pass is one primitive
//! instead of two.
//! Integer outputs (flat by rule) are the first vertex's on every vertex emitted, whichever the host takes a flat
//! value from. A pixel shader that reads the primitive id gets the rectangle's number (the triangle's).
//!
//! The shader is HLSL made from the vertex shader's output signature (its compiled DXIL's `OSG1`), each element
//! declared in register order with its own width and type, exactly as the translator declares the vertex
//! shader's outputs, so DXC packs the geometry stage's signatures into the same registers; the compiled result is
//! checked against the vertex and pixel shaders' signatures and refused when anything differs. One shader per
//! (signature, primitive id), compiled once a session and kept on disk for the next.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use rebuild_shader::blake3;
use rebuild_shader::signature::{self, Element};
use rebuild_shader::{Container, FourCC};

/// The console pipeline topology type of rectangle lists.
pub const RECT_TOPOLOGY_TYPE: u32 = 19;

/// The scalars a geometry shader may emit per invocation (vertices times output components).
const MAX_SCALARS: usize = 1024;
/// Vertices emitted per rectangle: one strip.
const VERTICES: usize = 4;

/// `D3D_NAME`: position, primitive id.
const SV_POSITION: u32 = 1;
const SV_PRIMITIVE_ID: u32 = 7;

/// A float component type (32 or 16 bits): continued into the fourth corner.
fn is_float(comp: u32) -> bool {
    matches!(comp, 3 | 6)
}

fn hlsl_type(comp: u32, n: usize) -> String {
    let base = match comp {
        1 => "uint",
        2 => "int",
        4 => "uint16_t",
        5 => "int16_t",
        6 => "float16_t",
        _ => "float",
    };
    if n == 1 { base.to_string() } else { format!("{base}{n}") }
}

/// The signature elements the geometry stage passes through, in the order the shader declares them.
fn ordered(outputs: &[Element]) -> Vec<&Element> {
    let mut v: Vec<&Element> = outputs.iter().collect();
    v.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    v
}

/// The HLSL of the geometry shader for vertex shader outputs `vs_out` (its compiled `OSG1`). `primitive_id`: the
/// pixel shader reads `SV_PrimitiveID` (the vertex shader does not make it), and the shader passes it on, at the
/// pixel shader's register `primitive_register`.
pub fn source(vs_out: &[Element], primitive_id: Option<u32>) -> Result<String, String> {
    let els = ordered(vs_out);
    let pos = els.iter().position(|e| e.system == SV_POSITION && e.mask.count_ones() == 4).ok_or("a rectangle list vertex shader with no four-component position")?;
    if els.iter().any(|e| e.system == SV_PRIMITIVE_ID) && primitive_id.is_some() {
        return Err("the vertex shader writes the primitive id itself".into());
    }
    let scalars: usize = els.iter().map(|e| e.mask.count_ones() as usize).sum::<usize>() + usize::from(primitive_id.is_some());
    if scalars * VERTICES > MAX_SCALARS {
        return Err(format!("{scalars} output components: more than a geometry shader emits"));
    }
    let mut s = String::new();
    let mut fields = String::new();
    for (k, e) in els.iter().enumerate() {
        let n = e.mask.count_ones() as usize;
        if n == 0 {
            return Err(format!("output {}{} has no components", e.name, e.index));
        }
        let semantic = if e.system == SV_POSITION { "SV_Position".to_string() } else { format!("{}{}", e.name, e.index) };
        let _ = writeln!(fields, "    {} e{k} : {semantic};", hlsl_type(e.comp, n));
    }
    let _ = write!(s, "// rectangle list geometry stage (rebuild-pipelines rectgs)\nstruct V {{\n{fields}}};\nstruct G {{\n");
    // the primitive id where the pixel shader has it: among the others by register
    let prim_at = primitive_id.map(|r| els.iter().position(|e| e.register > r).unwrap_or(els.len()));
    for (k, e) in els.iter().enumerate() {
        if prim_at == Some(k) {
            s.push_str("    uint prim : SV_PrimitiveID;\n");
        }
        let semantic = if e.system == SV_POSITION { "SV_Position".to_string() } else { format!("{}{}", e.name, e.index) };
        let _ = writeln!(s, "    {} e{k} : {semantic};", hlsl_type(e.comp, e.mask.count_ones() as usize));
    }
    if prim_at == Some(els.len()) {
        s.push_str("    uint prim : SV_PrimitiveID;\n");
    }
    s.push_str("};\n");
    // a vertex as it is: floats its own, integers the first vertex's
    s.push_str("G pass(V v, V f, uint p) {\n    G o;\n");
    for (k, e) in els.iter().enumerate() {
        let from = if is_float(e.comp) { "v" } else { "f" };
        let _ = writeln!(s, "    o.e{k} = {from}.e{k};");
    }
    if primitive_id.is_some() {
        s.push_str("    o.prim = p;\n");
    }
    s.push_str("    return o;\n}\n");
    // the fourth corner: opposite the corner c, across the diagonal a-b
    s.push_str("G fourth(V a, V b, V c, V f, uint p) {\n    G o;\n");
    for (k, e) in els.iter().enumerate() {
        if is_float(e.comp) {
            let _ = writeln!(s, "    o.e{k} = a.e{k} + b.e{k} - c.e{k};");
        } else {
            let _ = writeln!(s, "    o.e{k} = f.e{k};");
        }
    }
    if primitive_id.is_some() {
        s.push_str("    o.prim = p;\n");
    }
    s.push_str("    return o;\n}\n");
    // a corner of the single triangle that covers a whole-screen rectangle: twice as far from c as a is
    s.push_str("G far(V a, V c, V f, uint p) {\n    G o;\n");
    for (k, e) in els.iter().enumerate() {
        if is_float(e.comp) {
            let _ = writeln!(s, "    o.e{k} = a.e{k} + a.e{k} - c.e{k};");
        } else {
            let _ = writeln!(s, "    o.e{k} = f.e{k};");
        }
    }
    if primitive_id.is_some() {
        s.push_str("    o.prim = p;\n");
    }
    s.push_str("    return o;\n}\n");
    let _ = write!(
        s,
        r#"[maxvertexcount({VERTICES})]
void main(triangle V i[3], uint p : SV_PrimitiveID, inout TriangleStream<G> s) {{
    float4 q0 = i[0].e{pos}, q1 = i[1].e{pos}, q2 = i[2].e{pos};
    bool plain = q0.w > 0 && q1.w == q0.w && q2.w == q0.w && any(q0.xy != q1.xy) && any(q0.xy != q2.xy) && any(q1.xy != q2.xy);
    int c = -1;
    if (plain) {{
        if ((q0.x == q1.x && q0.y == q2.y) || (q0.x == q2.x && q0.y == q1.y)) c = 0;
        else if ((q1.x == q2.x && q1.y == q0.y) || (q1.x == q0.x && q1.y == q2.y)) c = 1;
        else if ((q2.x == q0.x && q2.y == q1.y) || (q2.x == q1.x && q2.y == q0.y)) c = 2;
    }}
    if (c < 0) {{
        s.Append(pass(i[0], i[0], p));
        s.Append(pass(i[1], i[0], p));
        s.Append(pass(i[2], i[0], p));
        return;
    }}
    // (corner, a, b) is a rotation of (v0, v1, v2): the strip turns as the title's vertices do
    V vc = i[0], va = i[1], vb = i[2];
    if (c == 1) {{ vc = i[1]; va = i[2]; vb = i[0]; }}
    else if (c == 2) {{ vc = i[2]; va = i[0]; vb = i[1]; }}
    // a rectangle over the whole clip square: one triangle with legs twice as long, which the clipper cuts to the
    // screen; no diagonal inside the rectangle, so no 2x2 quad of it is split between two triangles
    float lx = min(min(q0.x, q1.x), q2.x), hx = max(max(q0.x, q1.x), q2.x);
    float ly = min(min(q0.y, q1.y), q2.y), hy = max(max(q0.y, q1.y), q2.y);
    if (lx <= -q0.w && hx >= q0.w && ly <= -q0.w && hy >= q0.w) {{
        s.Append(pass(vc, i[0], p));
        s.Append(far(va, vc, i[0], p));
        s.Append(far(vb, vc, i[0], p));
        return;
    }}
    s.Append(pass(vc, i[0], p));
    s.Append(pass(va, i[0], p));
    s.Append(pass(vb, i[0], p));
    s.Append(fourth(va, vb, vc, i[0], p));
}}
"#
    );
    Ok(s)
}

/// An input or output signature part of a compiled program (empty when it has none).
pub fn signature_part(dxil: &[u8], part: FourCC) -> Result<Vec<Element>, String> {
    let c = Container::parse(dxil).map_err(|e| e.to_string())?;
    match c.part(part) {
        Some(p) => signature::parse(p.data).map_err(|e| e.to_string()),
        None => Ok(Vec::new()),
    }
}

fn same_place(a: &Element, b: &Element) -> bool {
    a.name.eq_ignore_ascii_case(&b.name) && a.index == b.index && a.register == b.register && a.mask == b.mask && a.comp == b.comp && a.system == b.system
}

/// The compiled geometry shader's signatures against the vertex shader's outputs and the pixel shader's inputs:
/// its inputs are the vertex shader's outputs, and its outputs are them (and the primitive id the pixel shader
/// reads), register for register.
pub fn check(gs: &[u8], vs_out: &[Element], ps_in: &[Element]) -> Result<(), String> {
    let (gi, go) = (signature_part(gs, FourCC::ISG1)?, signature_part(gs, FourCC::OSG1)?);
    let find = |set: &[Element], e: &Element| set.iter().any(|g| same_place(g, e));
    for e in vs_out {
        if !find(&gi, e) {
            return Err(format!("geometry stage input {}{} is not where the vertex shader writes it (register {})", e.name, e.index, e.register));
        }
        if !find(&go, e) {
            return Err(format!("geometry stage output {}{} is not where the vertex shader writes it (register {})", e.name, e.index, e.register));
        }
    }
    // what the pixel shader reads from the geometry stage: the same registers as before
    for e in ps_in.iter().filter(|e| e.system == SV_PRIMITIVE_ID) {
        if !go.iter().any(|g| g.system == SV_PRIMITIVE_ID && g.register == e.register) {
            return Err(format!("the primitive id is at register {}, not the pixel shader's {}", go.iter().find(|g| g.system == SV_PRIMITIVE_ID).map_or(u32::MAX, |g| g.register), e.register));
        }
    }
    Ok(())
}

/// What the geometry stages cost this session.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Compiled by DXC.
    pub compiled: u64,
    /// From the disk cache.
    pub from_disk: u64,
    /// Refused (no GS: the pipeline is built without one).
    pub refused: u64,
}

type Slot = Arc<OnceLock<Result<Arc<[u8]>, String>>>;

/// The geometry stages, one per (vertex shader signature, primitive id), made on first request.
pub struct RectGs {
    dir: Option<PathBuf>,
    dxc: OnceLock<Option<PathBuf>>,
    made: RwLock<HashMap<[u8; 32], Slot>>,
    n: [AtomicU64; 3],
}

impl RectGs {
    /// `dir`: the disk cache (`None`: none).
    pub fn new(dir: Option<PathBuf>) -> RectGs {
        RectGs { dir, dxc: OnceLock::new(), made: RwLock::default(), n: Default::default() }
    }

    /// The same, with `dxc` instead of [`crate::translate::dxc_path`].
    pub fn with_dxc(self, dxc: Option<PathBuf>) -> RectGs {
        let _ = self.dxc.set(dxc);
        self
    }

    /// The default disk cache: `<pipelines dir>\rectgs`.
    pub fn default_dir() -> PathBuf {
        crate::session::default_dir().join("rectgs")
    }

    pub fn stats(&self) -> Stats {
        let n = |i: usize| self.n[i].load(Ordering::Relaxed);
        Stats { compiled: n(0), from_disk: n(1), refused: n(2) }
    }

    /// The geometry stage for vertex shader `vs` and pixel shader `ps` (compiled DXIL). Made once per signature;
    /// later calls are a hash and a map lookup.
    pub fn get(&self, vs: &[u8], ps: Option<&[u8]>) -> Result<Arc<[u8]>, String> {
        let vs_out = signature_part(vs, FourCC::OSG1)?;
        let ps_in = match ps {
            Some(ps) => signature_part(ps, FourCC::ISG1)?,
            None => Vec::new(),
        };
        let prim = ps_in.iter().find(|e| e.system == SV_PRIMITIVE_ID).filter(|_| !vs_out.iter().any(|e| e.system == SV_PRIMITIVE_ID)).map(|e| e.register);
        let mut h = blake3::Hasher::new();
        for e in &vs_out {
            h.update(e.name.as_bytes());
            for w in [e.index, e.system, e.comp, e.register, u32::from(e.mask), e.min_precision] {
                h.update(&w.to_le_bytes());
            }
        }
        h.update(&prim.unwrap_or(u32::MAX).to_le_bytes());
        let key = *h.finalize().as_bytes();
        let slot = {
            let found = self.made.read().unwrap().get(&key).cloned();
            match found {
                Some(s) => s,
                None => self.made.write().unwrap().entry(key).or_default().clone(),
            }
        };
        let r = slot.get_or_init(|| {
            let r = self.make(&vs_out, &ps_in, prim);
            if let Err(e) = &r {
                // once a signature: its pipelines are built without the stage, their direct draws expanded on the CPU
                eprintln!("pipeline  no rectangle geometry stage for a vertex shader signature: {e}");
                self.n[2].fetch_add(1, Ordering::Relaxed);
            }
            r
        });
        r.clone()
    }

    fn make(&self, vs_out: &[Element], ps_in: &[Element], prim: Option<u32>) -> Result<Arc<[u8]>, String> {
        let src = source(vs_out, prim)?;
        let mut h = blake3::Hasher::new();
        for a in crate::translate::DXC_ARGS {
            h.update(a.as_bytes());
        }
        h.update(src.as_bytes());
        let name = h.finalize().to_hex()[..32].to_string();
        let cached = self.dir.as_ref().map(|d| d.join(format!("{name}.gs.dxil")));
        if let Some(b) = cached.as_ref().and_then(|p| std::fs::read(p).ok()) {
            if check(&b, vs_out, ps_in).is_ok() {
                self.n[1].fetch_add(1, Ordering::Relaxed);
                return Ok(b.into());
            }
        }
        let dxc = self.dxc.get_or_init(crate::translate::dxc_path).as_deref().ok_or("no dxc.exe (Windows SDK, or REBUILD_DXC)")?;
        let work = self.dir.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("rebuild-rectgs-{}", std::process::id())));
        let b = compile(dxc, &work, &name, &src)?;
        self.n[0].fetch_add(1, Ordering::Relaxed);
        check(&b, vs_out, ps_in)?;
        if let Some(p) = cached {
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &b).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
        Ok(b.into())
    }
}

/// `gs_6_6` through DXC, with the arguments every translated program is compiled with.
fn compile(dxc: &Path, work: &Path, name: &str, src: &str) -> Result<Vec<u8>, String> {
    // one at a time (they are few: a handful of signatures a title)
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(work).map_err(|e| format!("{}: {e}", work.display()))?;
    let (hl, obj) = (work.join(format!("{name}.gs.hlsl")), work.join(format!("{name}.gs.out")));
    std::fs::write(&hl, src).map_err(|e| format!("{}: {e}", hl.display()))?;
    let r = std::process::Command::new(dxc).args(["-T", "gs_6_6"]).args(crate::translate::DXC_ARGS).arg(&obj).arg(&hl).output().map_err(|e| format!("run {}: {e}", dxc.display()));
    let out = r.and_then(|r| {
        if r.status.success() {
            std::fs::read(&obj).map_err(|e| format!("{}: {e}", obj.display()))
        } else {
            let err = String::from_utf8_lossy(&r.stderr);
            Err(err.lines().find(|l| l.contains("error")).unwrap_or("dxc failed").to_string())
        }
    });
    let _ = std::fs::remove_file(&hl);
    let _ = std::fs::remove_file(&obj);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(name: &str, index: u32, system: u32, comp: u32, register: u32, mask: u8) -> Element {
        Element { stream: 0, name: name.into(), index, system, comp, register, mask, rw: 0, min_precision: 0 }
    }

    /// A blit's outputs: position, a float2 uv and a uint id packed into one row, a float4 colour.
    fn blit() -> Vec<Element> {
        vec![el("TEXCOORD", 1, 0, 1, 1, 0b0100), el("SV_Position", 0, 1, 3, 0, 0xf), el("TEXCOORD", 0, 0, 3, 1, 0b0011), el("COLOR", 0, 0, 3, 2, 0xf)]
    }

    #[test]
    fn declares_every_output_in_register_order_with_its_width_and_type() {
        let s = source(&blit(), None).unwrap();
        let v = &s[s.find("struct V {").unwrap()..s.find("struct G {").unwrap()];
        let lines: Vec<&str> = v.lines().skip(1).filter(|l| l.contains(':')).map(str::trim).collect();
        assert_eq!(lines, ["float4 e0 : SV_Position;", "float2 e1 : TEXCOORD0;", "uint e2 : TEXCOORD1;", "float4 e3 : COLOR0;"]);
        assert!(!s.contains("prim"), "no primitive id unless the pixel shader reads it");
        assert!(s.contains("[maxvertexcount(4)]") && s.contains("TriangleStream<G>"));
    }

    #[test]
    fn floats_continue_into_the_fourth_corner_and_integers_stay_the_first_vertexs() {
        let s = source(&blit(), None).unwrap();
        let f = &s[s.find("G fourth(").unwrap()..];
        assert!(f.contains("o.e0 = a.e0 + b.e0 - c.e0;"), "position");
        assert!(f.contains("o.e1 = a.e1 + b.e1 - c.e1;"), "uv");
        assert!(f.contains("o.e2 = f.e2;"), "the uint is flat");
        let p = &s[s.find("G pass(").unwrap()..s.find("G fourth(").unwrap()];
        assert!(p.contains("o.e2 = f.e2;") && p.contains("o.e3 = v.e3;"));
        // the corner, then the others, then the fourth: a strip of four
        assert_eq!(s.matches("s.Append(").count(), 3 + 3 + 4);
        assert!(s.contains("s.Append(far(va, vc, i[0], p));") && s.contains("o.e1 = a.e1 + a.e1 - c.e1;"), "the whole-screen triangle");
        assert!(s.contains("s.Append(fourth(va, vb, vc, i[0], p));"));
    }

    #[test]
    fn the_primitive_id_goes_where_the_pixel_shader_reads_it() {
        let s = source(&blit(), Some(3)).unwrap();
        let g = &s[s.find("struct G {").unwrap()..s.find("G pass(").unwrap()];
        let at = |t: &str| g.find(t).unwrap();
        assert!(at("COLOR0") < at("SV_PrimitiveID"));
        let s = source(&blit(), Some(1)).unwrap();
        let g = &s[s.find("struct G {").unwrap()..s.find("G pass(").unwrap()];
        assert!(g.find("SV_PrimitiveID").unwrap() < g.find("COLOR0").unwrap() && g.find("TEXCOORD0").unwrap() < g.find("SV_PrimitiveID").unwrap());
        assert!(s.contains("o.prim = p;"));
    }

    #[test]
    fn refuses_what_it_cannot_make() {
        assert!(source(&[el("TEXCOORD", 0, 0, 3, 0, 0xf)], None).unwrap_err().contains("position"));
        let wide: Vec<Element> = std::iter::once(el("SV_Position", 0, 1, 3, 0, 0xf)).chain((1..=64).map(|r| el("TEXCOORD", r, 0, 3, r, 0xf))).collect();
        assert!(source(&wide, None).unwrap_err().contains("more than"));
        let mut with_prim = blit();
        with_prim.push(el("SV_PrimitiveID", 0, 7, 1, 3, 1));
        assert!(source(&with_prim, Some(4)).is_err());
    }

    #[test]
    fn sixteen_bit_outputs_keep_their_width() {
        let s = source(&[el("SV_Position", 0, 1, 3, 0, 0xf), el("TEXCOORD", 0, 0, 6, 1, 0b0011), el("TEXCOORD", 1, 0, 4, 2, 1)], None).unwrap();
        assert!(s.contains("float16_t2 e1 : TEXCOORD0;") && s.contains("uint16_t e2 : TEXCOORD1;"));
    }

    /// DXC (when present) takes the source, and the compiled signatures are the vertex shader's, register for
    /// register; a second request is the same bytes with no second compile.
    #[test]
    fn compiles_and_links_register_for_register() {
        let Some(dxc) = crate::translate::dxc_path() else {
            eprintln!("skipped: no dxc.exe");
            return;
        };
        let dir = std::env::temp_dir().join(format!("rebuild-rectgs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // a vertex shader's outputs as the translator declares them: a uint packed beside a float2, a lone float
        std::fs::create_dir_all(&dir).unwrap();
        let hl = dir.join("t.vs.hlsl");
        std::fs::write(&hl, "struct Out { float4 pos : SV_Position; float2 a : TEXCOORD0; uint b : TEXCOORD1; float4 c : COLOR0; float d : TEXCOORD2; };\nOut main(uint id : SV_VertexID) { Out o = (Out)0; o.pos = float4(id, 0, 0, 1); o.a = id; o.b = id; o.c = id; o.d = id; return o; }").unwrap();
        let obj = dir.join("t.vs.dxil");
        let ok = std::process::Command::new(&dxc).args(["-T", "vs_6_6"]).args(crate::translate::DXC_ARGS).arg(&obj).arg(&hl).output().unwrap().status.success();
        assert!(ok);
        let vs = std::fs::read(&obj).unwrap();
        let g = RectGs::new(Some(dir.clone())).with_dxc(Some(dxc.clone()));
        let a = g.get(&vs, None).unwrap();
        let b = g.get(&vs, None).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!((g.stats().compiled, g.stats().from_disk), (1, 0));
        check(&a, &signature_part(&vs, FourCC::OSG1).unwrap(), &[]).unwrap();
        // the next session: from disk
        let g2 = RectGs::new(Some(dir.clone())).with_dxc(Some(dxc));
        assert_eq!(&g2.get(&vs, None).unwrap()[..], &a[..]);
        assert_eq!((g2.stats().compiled, g2.stats().from_disk), (0, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
