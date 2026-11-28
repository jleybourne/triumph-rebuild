//! Host pipelines: a [`Desc`] made into an `ID3D12PipelineState`, through a persistent `ID3D12PipelineLibrary1`.
//!
//! - [`Programs`]: the program library ([`crate::library`]) on a device: bytecode by hash, and root signatures
//!   created once per layout (or layout pair) from the library's serialized blobs. Compute programs the library
//!   does not hold come from [`OnDemand`] when it is attached ([`Programs::with_ondemand`]): translated on the
//!   worker that creates the pipeline.
//! - [`PsoCache`]: the pipeline library file. Loaded at start; every pipeline created is stored under its
//!   [`Key`]; written on exit and periodically. A library the driver refuses (a driver update, another GPU, a
//!   damaged file) is logged and replaced by an empty one, never trusted in part.
//! - [`PsoCreator`]: the [`Creator`] the [`crate::pool::Pool`] runs: from the cache when it holds the key, else
//!   created by the driver and stored.
//!
//! ```text
//! cache file  "RBPSOCAC" · version u32 · keys u32 · blob length u64 · BLAKE3 of blob and keys (32 bytes)
//!             · the driver's serialized library · keys, 25 bytes each (compute u8, shaders 2 x u64, state u64)
//! ```

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rebuild_shader::blake3;
use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::layout::{Param, RootDesc, StageLayout, Visibility};
use crate::library::Library;
use crate::ondemand::{self, OnDemand};
use crate::pool::Creator;
use crate::rectgs::{RectGs, RECT_TOPOLOGY_TYPE};
use crate::state::{Desc, GraphicsState, Key};
use crate::Stage;

const MAGIC: &[u8; 8] = b"RBPSOCAC";
const VERSION: u32 = 1;
const HEADER: usize = 8 + 4 + 4 + 8 + 32;
const KEY_BYTES: usize = 25;

/// `D3D12_ERROR_ADAPTER_NOT_FOUND`, `D3D12_ERROR_DRIVER_VERSION_MISMATCH`.
const ADAPTER_NOT_FOUND: i32 = 0x887E_0001_u32 as i32;
const DRIVER_VERSION_MISMATCH: i32 = 0x887E_0002_u32 as i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A root signature's identity: a library one by (kind, layout, layout), or an on-demand program's by its layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RootKey {
    Library(u8, u32, u32),
    Own(StageLayout),
    /// a graphics pair with a program outside the library: (vertex, pixel) layouts
    OwnGraphics(StageLayout, StageLayout),
}

/// Where a program comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Library,
    OnDemand,
}

/// A program's bytecode: in the library, or translated on demand.
pub enum Code<'a> {
    Library(&'a [u8]),
    OnDemand(Arc<ondemand::Program>),
}

impl std::ops::Deref for Code<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Code::Library(b) => b,
            Code::OnDemand(p) => &p.dxil,
        }
    }
}

/// The program library on a device.
pub struct Programs {
    lib: Library,
    ondemand: Option<OnDemand>,
    device: ID3D12Device,
    roots: RwLock<HashMap<RootKey, ID3D12RootSignature>>,
    /// The geometry stages of rectangle-list pipelines, one per vertex shader signature.
    rect: RectGs,
    /// Bench only: an extra root constant at `b<salt>, space900`, so every root signature is new to the
    /// driver's own shader cache (a cold measurement without deleting the driver's cache).
    salt: Option<u32>,
}

impl Programs {
    pub fn new(device: &ID3D12Device, lib: Library, salt: Option<u32>) -> Programs {
        Programs { lib, ondemand: None, device: device.clone(), roots: RwLock::default(), rect: RectGs::new(Some(RectGs::default_dir())), salt }
    }

    /// Rectangle-list geometry stages from `rect` (another disk cache, or another DXC).
    pub fn with_rect_gs(mut self, rect: RectGs) -> Programs {
        self.rect = rect;
        self
    }

    pub fn rect_gs(&self) -> &RectGs {
        &self.rect
    }

    /// Compute programs the library does not hold are translated on demand.
    pub fn with_ondemand(mut self, od: OnDemand) -> Programs {
        self.ondemand = Some(od);
        self
    }

    pub fn library(&self) -> &Library {
        &self.lib
    }

    pub fn ondemand(&self) -> Option<&OnDemand> {
        self.ondemand.as_ref()
    }

    /// The title created compute program `cs` from `bytes`: where its pipeline's program will come from
    /// (`None`: nowhere, do not request it). A library lookup, then for a program outside it one hash lookup
    /// (its container copied the first time); never translates.
    pub fn offer_compute(&self, cs: u64, bytes: &[u8]) -> Option<Source> {
        if self.lib.get(cs, Stage::Cs).is_some() {
            return Some(Source::Library);
        }
        self.ondemand.as_ref().map(|od| {
            od.offer(cs, bytes);
            Source::OnDemand
        })
    }

    fn root(&self, k: RootKey, blob: Option<&[u8]>, desc: impl FnOnce() -> RootDesc) -> Result<ID3D12RootSignature, String> {
        if let Some(r) = self.roots.read().unwrap().get(&k) {
            return Ok(r.clone());
        }
        let owned;
        let blob = match (blob, self.salt) {
            (Some(b), None) => b,
            _ => {
                let mut d = desc();
                if let Some(s) = self.salt {
                    d.params.push(Param::Constants { register: s, space: 900, count: 1, vis: Visibility::All });
                }
                owned = crate::d3d::serialize(&d)?;
                &owned[..]
            }
        };
        let r: ID3D12RootSignature = unsafe { self.device.CreateRootSignature(0, blob) }.map_err(|e| format!("root signature: {e}"))?;
        Ok(self.roots.write().unwrap().entry(k).or_insert(r).clone())
    }

    fn layout(&self, i: u32) -> StageLayout {
        self.lib.layout(i).unwrap_or_default()
    }

    /// (bytecode, root signature) of a compute program: from the library, else translated on demand (on this
    /// thread if it is not ready: call it from a pool worker).
    pub fn compute(&self, cs: u64) -> Result<(Code<'_>, ID3D12RootSignature), String> {
        let ud = self.lib.user_dwords();
        if let Some(e) = self.lib.get(cs, Stage::Cs) {
            let root = self.root(RootKey::Library(0, e.layout, 0), e.root, || RootDesc::compute(&self.layout(e.layout), ud))?;
            return Ok((Code::Library(e.dxil), root));
        }
        let od = self.ondemand.as_ref().ok_or_else(|| format!("cs {cs:016x} is not in the program library"))?;
        let p = od.get(cs)?;
        if p.stage != Stage::Cs {
            return Err(format!("{cs:016x} is a {} program, not compute", p.stage.name()));
        }
        let root = self.root(RootKey::Own(p.layout.clone()), None, || RootDesc::compute(&p.layout, ud))?;
        Ok((Code::OnDemand(p), root))
    }

    /// The title created a program of `stage` from `bytes`: where it will come from (`None`: nowhere). As
    /// [`Programs::offer_compute`], for any stage (vertex and pixel programs outside `shaders.rpf` too).
    pub fn offer(&self, h: u64, stage: Stage, bytes: &[u8]) -> Option<Source> {
        if self.lib.get(h, stage).is_some() {
            return Some(Source::Library);
        }
        self.ondemand.as_ref().map(|od| {
            od.offer(h, bytes);
            Source::OnDemand
        })
    }

    /// A program of `stage` outside the library, translated on demand (on this thread if not ready).
    fn on_demand(&self, h: u64, stage: Stage) -> Result<Arc<ondemand::Program>, String> {
        let od = self.ondemand.as_ref().ok_or_else(|| format!("{} {h:016x} is not in the program library", stage.name()))?;
        let p = od.get(h)?;
        if p.stage != stage {
            return Err(format!("{h:016x} is a {} program, not {}", p.stage.name(), stage.name()));
        }
        Ok(p)
    }

    /// (vertex bytecode, pixel bytecode, root signature) of a graphics pair: from the library, else (either
    /// program missing there) translated on demand, with a root signature from the two programs' layouts.
    pub fn graphics(&self, vs: u64, ps: Option<u64>) -> Result<(Code<'_>, Option<Code<'_>>, ID3D12RootSignature), String> {
        let ud = self.lib.user_dwords();
        let v = self.lib.get(vs, Stage::Vs);
        let p = ps.map(|ps| self.lib.get(ps, Stage::Ps));
        match (v, p) {
            (Some(v), Some(Some(p))) => {
                let blob = self.lib.graphics_root(&v, &p);
                let root = self.root(RootKey::Library(1, v.layout, p.layout), blob, || RootDesc::graphics(&self.layout(v.layout), &self.layout(p.layout), ud))?;
                Ok((Code::Library(v.dxil), Some(Code::Library(p.dxil)), root))
            }
            (Some(v), None) => {
                let root = self.root(RootKey::Library(2, v.layout, 0), None, || RootDesc::graphics(&self.layout(v.layout), &StageLayout::default(), ud))?;
                Ok((Code::Library(v.dxil), None, root))
            }
            (v, p) => {
                let (vc, vl) = match v {
                    Some(e) => (Code::Library(e.dxil), self.layout(e.layout)),
                    None => {
                        let o = self.on_demand(vs, Stage::Vs)?;
                        let l = o.layout.clone();
                        (Code::OnDemand(o), l)
                    }
                };
                let (pc, pl) = match (ps, p.flatten()) {
                    (None, _) => (None, StageLayout::default()),
                    (Some(_), Some(e)) => (Some(Code::Library(e.dxil)), self.layout(e.layout)),
                    (Some(ps), None) => {
                        let o = self.on_demand(ps, Stage::Ps)?;
                        let l = o.layout.clone();
                        (Some(Code::OnDemand(o)), l)
                    }
                };
                let root = self.root(RootKey::OwnGraphics(vl.clone(), pl.clone()), None, || RootDesc::graphics(&vl, &pl, ud))?;
                Ok((vc, pc, root))
            }
        }
    }
}

fn bytecode(b: Option<&[u8]>) -> D3D12_SHADER_BYTECODE {
    b.map_or_else(D3D12_SHADER_BYTECODE::default, |b| D3D12_SHADER_BYTECODE { pShaderBytecode: b.as_ptr() as *const _, BytecodeLength: b.len() })
}

/// Calls `f` with the host description of a graphics pipeline (names and arrays live for the call). `gs`: the
/// geometry stage of a rectangle-list pipeline ([`crate::rectgs`]).
pub fn with_graphics_desc<R>(root: &ID3D12RootSignature, vs: &[u8], ps: Option<&[u8]>, gs: Option<&[u8]>, s: &GraphicsState, f: impl FnOnce(&D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> R) -> R {
    let names: Vec<CString> = s.input.iter().map(|e| CString::new(e.name.as_str()).unwrap_or_default()).collect();
    let input: Vec<D3D12_INPUT_ELEMENT_DESC> = s
        .input
        .iter()
        .zip(&names)
        .map(|(e, n)| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(n.as_ptr() as *const u8),
            SemanticIndex: e.index,
            Format: DXGI_FORMAT(e.format as i32),
            InputSlot: e.slot,
            AlignedByteOffset: e.offset,
            InputSlotClass: if e.per_instance { D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA } else { D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA },
            InstanceDataStepRate: e.step_rate,
        })
        .collect();
    let mut rt = [D3D12_RENDER_TARGET_BLEND_DESC::default(); 8];
    for (o, b) in rt.iter_mut().zip(&s.blend) {
        *o = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: b.enable.into(),
            LogicOpEnable: b.logic_enable.into(),
            SrcBlend: D3D12_BLEND(b.src as i32),
            DestBlend: D3D12_BLEND(b.dst as i32),
            BlendOp: D3D12_BLEND_OP(b.op as i32),
            SrcBlendAlpha: D3D12_BLEND(b.src_alpha as i32),
            DestBlendAlpha: D3D12_BLEND(b.dst_alpha as i32),
            BlendOpAlpha: D3D12_BLEND_OP(b.op_alpha as i32),
            LogicOp: D3D12_LOGIC_OP(b.logic_op as i32),
            RenderTargetWriteMask: b.write_mask,
        };
    }
    let mut rtv = [DXGI_FORMAT_UNKNOWN; 8];
    for (o, t) in rtv.iter_mut().zip(&s.targets) {
        *o = DXGI_FORMAT(*t as i32);
    }
    let r = &s.raster;
    let d = &s.depth_stencil;
    let op = |o: &crate::state::StencilOp| D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP(o.fail as i32),
        StencilDepthFailOp: D3D12_STENCIL_OP(o.depth_fail as i32),
        StencilPassOp: D3D12_STENCIL_OP(o.pass as i32),
        StencilFunc: D3D12_COMPARISON_FUNC(o.func as i32),
    };
    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        VS: bytecode(Some(vs)),
        PS: bytecode(ps),
        GS: bytecode(gs),
        BlendState: D3D12_BLEND_DESC { AlphaToCoverageEnable: s.alpha_to_coverage.into(), IndependentBlendEnable: s.independent_blend.into(), RenderTarget: rt },
        SampleMask: s.sample_mask,
        RasterizerState: D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE(r.fill as i32),
            CullMode: D3D12_CULL_MODE(r.cull as i32),
            FrontCounterClockwise: r.front_ccw.into(),
            DepthBias: r.depth_bias,
            DepthBiasClamp: r.depth_bias_clamp,
            SlopeScaledDepthBias: r.slope_scaled_depth_bias,
            DepthClipEnable: r.depth_clip.into(),
            MultisampleEnable: r.multisample.into(),
            AntialiasedLineEnable: r.antialiased_line.into(),
            ForcedSampleCount: r.forced_sample_count,
            ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE(i32::from(r.conservative)),
        },
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
            DepthEnable: d.depth.into(),
            DepthWriteMask: if d.depth_write { D3D12_DEPTH_WRITE_MASK_ALL } else { D3D12_DEPTH_WRITE_MASK_ZERO },
            DepthFunc: D3D12_COMPARISON_FUNC(d.depth_func as i32),
            StencilEnable: d.stencil.into(),
            StencilReadMask: d.read_mask,
            StencilWriteMask: d.write_mask,
            FrontFace: op(&d.front),
            BackFace: op(&d.back),
        },
        InputLayout: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: if input.is_empty() { std::ptr::null() } else { input.as_ptr() }, NumElements: input.len() as u32 },
        IBStripCutValue: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(s.strip_cut as i32),
        // 1-4 only (point, line, triangle, patch): an undefined or unknown console value builds as triangles, and
        // so do rectangle lists (19), drawn as triangles through the geometry stage or expanded by the list
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE(if (1..=4).contains(&s.topology) { s.topology as i32 } else { 3 }),
        NumRenderTargets: s.targets.len().min(8) as u32,
        RTVFormats: rtv,
        DSVFormat: DXGI_FORMAT(s.depth_format as i32),
        SampleDesc: DXGI_SAMPLE_DESC { Count: s.samples, Quality: s.sample_quality },
        ..Default::default()
    };
    let out = f(&desc);
    drop(ManuallyDrop::into_inner(desc.pRootSignature));
    out
}

fn with_compute_desc<R>(root: &ID3D12RootSignature, cs: &[u8], f: impl FnOnce(&D3D12_COMPUTE_PIPELINE_STATE_DESC) -> R) -> R {
    let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC { pRootSignature: ManuallyDrop::new(Some(root.clone())), CS: bytecode(Some(cs)), ..Default::default() };
    let out = f(&desc);
    drop(ManuallyDrop::into_inner(desc.pRootSignature));
    out
}

fn key_bytes(k: &Key, out: &mut Vec<u8>) {
    out.push(u8::from(k.compute));
    for v in [k.shaders[0], k.shaders[1], k.state] {
        out.extend(v.to_le_bytes());
    }
}

fn key_from(b: &[u8]) -> Key {
    let q = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    Key { compute: b[0] != 0, shaders: [q(1), q(9)], state: q(17) }
}

/// How opening the cache went.
#[derive(Debug, Clone, Default)]
pub struct OpenReport {
    pub path: PathBuf,
    /// Pipelines the loaded library holds.
    pub keys: usize,
    pub bytes: usize,
    /// Reading and checking the file.
    pub read: Duration,
    /// `CreatePipelineLibrary` on the blob.
    pub create: Duration,
    /// Why the file was not used, when it was not (missing is not a refusal).
    pub refused: Option<String>,
}

/// How a save went.
#[derive(Debug, Clone, Copy, Default)]
pub struct SaveReport {
    pub keys: usize,
    pub bytes: usize,
    pub serialize: Duration,
    pub write: Duration,
}

/// The persistent pipeline library.
pub struct PsoCache {
    // the library reads the blob it was created from for as long as it lives: dropped first
    lib: ID3D12PipelineLibrary1,
    _blob: Vec<u8>,
    path: PathBuf,
    known: RwLock<HashSet<Key>>,
    unsaved: AtomicUsize,
    last_save: Mutex<Instant>,
    /// Keys stored this session that the library refused to store (already there with another
    /// description: a key collision or a stale entry).
    pub refused_stores: AtomicUsize,
    pub report: OpenReport,
    pub events: Mutex<Vec<String>>,
}

impl PsoCache {
    /// Opens the cache file at `path` on `device`; a missing, damaged or refused file gives an empty library
    /// (and the reason in `report.refused` and `events`).
    pub fn open(device: &ID3D12Device, path: &Path) -> Result<PsoCache, String> {
        let dev: ID3D12Device1 = device.cast().map_err(|e| format!("ID3D12Device1: {e}"))?;
        let mut report = OpenReport { path: path.to_path_buf(), ..Default::default() };
        let t = Instant::now();
        let parsed = match std::fs::read(path) {
            Ok(b) => parse_cache(b).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{e}")),
        };
        report.read = t.elapsed();
        let mut events = Vec::new();
        let (blob, keys) = match parsed {
            Ok(Some(x)) => x,
            Ok(None) => (Vec::new(), Vec::new()),
            Err(e) => {
                events.push(format!("pipeline cache {}: {e}; starting an empty one", path.display()));
                report.refused = Some(e);
                (Vec::new(), Vec::new())
            }
        };
        let t = Instant::now();
        let (lib, blob, keys) = match unsafe { dev.CreatePipelineLibrary::<ID3D12PipelineLibrary1>(&blob) } {
            Ok(l) => (l, blob, keys),
            Err(e) => {
                let why = match e.code().0 {
                    DRIVER_VERSION_MISMATCH => "the driver changed (D3D12_ERROR_DRIVER_VERSION_MISMATCH)".to_string(),
                    ADAPTER_NOT_FOUND => "another adapter made it (D3D12_ERROR_ADAPTER_NOT_FOUND)".to_string(),
                    _ => format!("the driver refused it ({e})"),
                };
                events.push(format!("pipeline cache {}: {why}; rebuilding it", path.display()));
                report.refused = Some(why);
                let l = unsafe { dev.CreatePipelineLibrary::<ID3D12PipelineLibrary1>(&[]) }.map_err(|e| format!("an empty pipeline library: {e}"))?;
                (l, Vec::new(), Vec::new())
            }
        };
        report.create = t.elapsed();
        report.keys = keys.len();
        report.bytes = blob.len();
        for e in &events {
            eprintln!("{e}");
        }
        Ok(PsoCache {
            lib,
            _blob: blob,
            path: path.to_path_buf(),
            known: RwLock::new(keys.into_iter().collect()),
            unsaved: AtomicUsize::new(0),
            last_save: Mutex::new(Instant::now()),
            refused_stores: AtomicUsize::new(0),
            report,
            events: Mutex::new(events),
        })
    }

    pub fn contains(&self, k: &Key) -> bool {
        self.known.read().unwrap().contains(k)
    }

    pub fn len(&self) -> usize {
        self.known.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pipelines stored since the last save.
    pub fn unsaved(&self) -> usize {
        self.unsaved.load(Ordering::Relaxed)
    }

    pub fn load_graphics(&self, k: &Key, d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Option<ID3D12PipelineState> {
        unsafe { self.lib.LoadGraphicsPipeline(PCWSTR(wide(&k.name()).as_ptr()), d) }.ok()
    }

    pub fn load_compute(&self, k: &Key, d: &D3D12_COMPUTE_PIPELINE_STATE_DESC) -> Option<ID3D12PipelineState> {
        unsafe { self.lib.LoadComputePipeline(PCWSTR(wide(&k.name()).as_ptr()), d) }.ok()
    }

    /// Stores a pipeline under its key (once; a second store of a key is refused by the library).
    pub fn store(&self, k: &Key, p: &ID3D12PipelineState) {
        if !self.known.write().unwrap().insert(*k) {
            return;
        }
        match unsafe { self.lib.StorePipeline(PCWSTR(wide(&k.name()).as_ptr()), p) } {
            Ok(()) => {
                self.unsaved.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                self.refused_stores.fetch_add(1, Ordering::Relaxed);
                self.events.lock().unwrap().push(format!("pipeline cache: store {} refused: {e}", k.name()));
            }
        }
    }

    /// Writes the library (beside, then renamed over the old file).
    pub fn save(&self) -> Result<SaveReport, String> {
        let mut last = self.last_save.lock().unwrap();
        let r = self.write();
        *last = Instant::now();
        r
    }

    /// Saves if something is unsaved and `every` has passed since the last save; never waits for a save
    /// another thread is making.
    pub fn save_if_due(&self, every: Duration) -> Option<Result<SaveReport, String>> {
        if self.unsaved() == 0 {
            return None;
        }
        let mut last = self.last_save.try_lock().ok()?;
        if last.elapsed() < every {
            return None;
        }
        let r = self.write();
        *last = Instant::now();
        Some(r)
    }

    fn write(&self) -> Result<SaveReport, String> {
        let pending = self.unsaved.swap(0, Ordering::Relaxed);
        let t = Instant::now();
        let keys: Vec<Key> = self.known.read().unwrap().iter().copied().collect();
        let n = unsafe { self.lib.GetSerializedSize() };
        let mut out = vec![0u8; HEADER + n];
        let r = unsafe { self.lib.Serialize(&mut out[HEADER..]) };
        if let Err(e) = r {
            self.unsaved.fetch_add(pending, Ordering::Relaxed);
            return Err(format!("serialize: {e}"));
        }
        let serialize = t.elapsed();
        let t = Instant::now();
        out.reserve(keys.len() * KEY_BYTES);
        for k in &keys {
            key_bytes(k, &mut out);
        }
        out[..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&VERSION.to_le_bytes());
        out[12..16].copy_from_slice(&(keys.len() as u32).to_le_bytes());
        out[16..24].copy_from_slice(&(n as u64).to_le_bytes());
        let h = blake3::hash(&out[HEADER..]);
        out[24..56].copy_from_slice(h.as_bytes());
        let res = (|| {
            if let Some(d) = self.path.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            let tmp = self.path.with_extension("tmp");
            std::fs::write(&tmp, &out).map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))
        })();
        if let Err(e) = res {
            self.unsaved.fetch_add(pending, Ordering::Relaxed);
            return Err(e);
        }
        Ok(SaveReport { keys: keys.len(), bytes: out.len(), serialize, write: t.elapsed() })
    }
}

/// The blob and keys of a cache file, after its checks.
fn parse_cache(mut b: Vec<u8>) -> Result<(Vec<u8>, Vec<Key>), String> {
    if b.len() < HEADER || &b[..8] != MAGIC {
        return Err("not a pipeline cache".into());
    }
    let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    if u(8) != VERSION {
        return Err(format!("cache version {} (this build reads {VERSION})", u(8)));
    }
    let keys = u(12) as usize;
    let blob = u64::from_le_bytes(b[16..24].try_into().unwrap()) as usize;
    if HEADER.checked_add(blob).and_then(|n| n.checked_add(keys * KEY_BYTES)) != Some(b.len()) {
        return Err("cache length does not match its header".into());
    }
    if blake3::hash(&b[HEADER..]).as_bytes()[..] != b[24..56] {
        return Err("cache content hash mismatch".into());
    }
    let ks = b[HEADER + blob..].chunks_exact(KEY_BYTES).map(key_from).collect();
    b.truncate(HEADER + blob);
    b.drain(..HEADER);
    Ok((b, ks))
}

/// Where pipelines of one kind came from, and what they cost.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub from_cache: u64,
    pub created: u64,
    /// In the cache's key list, but the library would not load it with this description.
    pub cache_mismatch: u64,
    pub refused: u64,
    /// Summed over the threads that loaded or created.
    pub load_time: Duration,
    pub create_time: Duration,
}

impl Counts {
    /// Milliseconds per pipeline loaded from the cache.
    pub fn load_ms(&self) -> f64 {
        per(self.load_time, self.from_cache)
    }
    /// Milliseconds per pipeline the driver created.
    pub fn create_ms(&self) -> f64 {
        per(self.create_time, self.created)
    }
}

fn per(d: Duration, n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        d.as_secs_f64() * 1e3 / n as f64
    }
}

/// Per kind: compute and graphics.
#[derive(Debug, Default, Clone, Copy)]
pub struct CreatorStats {
    pub compute: Counts,
    pub graphics: Counts,
}

impl CreatorStats {
    pub fn total(&self) -> Counts {
        let (a, b) = (self.compute, self.graphics);
        Counts {
            from_cache: a.from_cache + b.from_cache,
            created: a.created + b.created,
            cache_mismatch: a.cache_mismatch + b.cache_mismatch,
            refused: a.refused + b.refused,
            load_time: a.load_time + b.load_time,
            create_time: a.create_time + b.create_time,
        }
    }
}

const FROM_CACHE: usize = 0;
const CREATED: usize = 1;
const MISMATCH: usize = 2;
const REFUSED: usize = 3;
const LOAD_NS: usize = 4;
const CREATE_NS: usize = 5;

/// Makes `ID3D12PipelineState`s for the pool.
pub struct PsoCreator {
    device: ID3D12Device,
    programs: Programs,
    cache: Option<PsoCache>,
    /// Save the cache this often while pipelines are being stored.
    pub save_every: Duration,
    /// [graphics, compute] x the counters above.
    n: [[AtomicU64; 6]; 2],
    /// Rectangle-list pipelines built with the geometry stage.
    rects: RwLock<HashSet<Key>>,
}

impl PsoCreator {
    pub fn new(device: &ID3D12Device, programs: Programs, cache: Option<PsoCache>) -> PsoCreator {
        PsoCreator { device: device.clone(), programs, cache, save_every: Duration::from_secs(30), n: Default::default(), rects: RwLock::default() }
    }

    pub fn cache(&self) -> Option<&PsoCache> {
        self.cache.as_ref()
    }

    pub fn programs(&self) -> &Programs {
        &self.programs
    }

    /// Whether the pipeline `key` (created) draws rectangle lists through the geometry stage: its draws go out
    /// as triangle lists with the title's counts. A rectangle-list pipeline without one (its stage refused)
    /// leaves the list to expand what it can.
    pub fn rect_gs(&self, key: &Key) -> bool {
        self.rects.read().unwrap().contains(key)
    }

    pub fn stats(&self) -> CreatorStats {
        let c = |k: usize| {
            let n = |i: usize| self.n[k][i].load(Ordering::Relaxed);
            Counts {
                from_cache: n(FROM_CACHE),
                created: n(CREATED),
                cache_mismatch: n(MISMATCH),
                refused: n(REFUSED),
                load_time: Duration::from_nanos(n(LOAD_NS)),
                create_time: Duration::from_nanos(n(CREATE_NS)),
            }
        };
        CreatorStats { graphics: c(0), compute: c(1) }
    }

    fn add(&self, key: &Key, i: usize, v: u64) {
        self.n[usize::from(key.compute)][i].fetch_add(v, Ordering::Relaxed);
    }

    fn finish(&self, key: &Key, r: Result<ID3D12PipelineState, String>, t: Instant) -> Result<ID3D12PipelineState, String> {
        self.add(key, CREATE_NS, t.elapsed().as_nanos() as u64);
        match &r {
            Ok(p) => {
                self.add(key, CREATED, 1);
                if let Some(c) = &self.cache {
                    c.store(key, p);
                    if let Some(Err(e)) = c.save_if_due(self.save_every) {
                        c.events.lock().unwrap().push(format!("pipeline cache save: {e}"));
                    }
                }
            }
            Err(_) => self.add(key, REFUSED, 1),
        }
        r
    }

    fn from_cache(&self, key: &Key, load: impl FnOnce(&PsoCache) -> Option<ID3D12PipelineState>) -> Option<ID3D12PipelineState> {
        let c = self.cache.as_ref().filter(|c| c.contains(key))?;
        let t = Instant::now();
        let p = load(c);
        self.add(key, LOAD_NS, t.elapsed().as_nanos() as u64);
        self.add(key, if p.is_some() { FROM_CACHE } else { MISMATCH }, 1);
        p
    }
}

impl Creator for PsoCreator {
    type Out = ID3D12PipelineState;

    fn create(&self, key: &Key, desc: &Desc) -> Result<ID3D12PipelineState, String> {
        match desc {
            Desc::Compute { cs } => {
                let (code, root) = self.programs.compute(*cs)?;
                with_compute_desc(&root, &code, |d| {
                    if let Some(p) = self.from_cache(key, |c| c.load_compute(key, d)) {
                        return Ok(p);
                    }
                    let t = Instant::now();
                    let r = unsafe { self.device.CreateComputePipelineState(d) }.map_err(|e| format!("compute pipeline {}: {e}", key.name()));
                    self.finish(key, r, t)
                })
            }
            Desc::Graphics { vs, ps, state } => {
                let (v, p, root) = self.programs.graphics(*vs, *ps)?;
                let gs = if state.topology == RECT_TOPOLOGY_TYPE {
                    match self.programs.rect.get(&v, p.as_deref()) {
                        Ok(gs) => Some(gs),
                        Err(e) => {
                            if let Some(c) = &self.cache {
                                c.events.lock().unwrap().push(format!("rectangle geometry stage for {}: {e}", key.name()));
                            }
                            None
                        }
                    }
                } else {
                    None
                };
                let r = with_graphics_desc(&root, &v, p.as_deref(), gs.as_deref(), state, |d| {
                    if let Some(p) = self.from_cache(key, |c| c.load_graphics(key, d)) {
                        return Ok(p);
                    }
                    let t = Instant::now();
                    let r = unsafe { self.device.CreateGraphicsPipelineState(d) }.map_err(|e| format!("graphics pipeline {}: {e}", key.name()));
                    self.finish(key, r, t)
                });
                if r.is_ok() && gs.is_some() {
                    self.rects.write().unwrap().insert(*key);
                }
                r
            }
        }
    }
}

impl Drop for PsoCreator {
    /// The session's pipelines reach the file however the pool ends.
    fn drop(&mut self) {
        if let Some(c) = &self.cache {
            if c.unsaved() > 0 {
                if let Err(e) = c.save() {
                    eprintln!("pipeline cache save on exit: {e}");
                }
            }
        }
    }
}

/// Whether the device supports pipeline libraries (`D3D12_SHADER_CACHE_SUPPORT_LIBRARY`), and its shader
/// cache support flags.
pub fn library_support(device: &ID3D12Device) -> (bool, u32) {
    let mut f = D3D12_FEATURE_DATA_SHADER_CACHE::default();
    let ok = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_SHADER_CACHE, &mut f as *mut _ as *mut _, std::mem::size_of_val(&f) as u32) }.is_ok();
    (ok && f.SupportFlags.0 & D3D12_SHADER_CACHE_SUPPORT_LIBRARY.0 != 0, f.SupportFlags.0 as u32)
}

/// The cache file's default path: `<dir>\pso-cache.bin` (see [`crate::session::default_dir`]).
pub fn default_cache_path() -> PathBuf {
    crate::session::default_dir().join("pso-cache.bin")
}

/// The cache file for one program library: `<dir>\pso-cache-<first 8 bytes of its identity>.bin`. A cache
/// holds PSOs built from the library's DXIL and root signatures; one built from another library is refused
/// entry by entry mid-run (debug #972, "pipeline state desc provided does not match", and a rebuild: stutter),
/// so a new library starts a fresh file. Caches of other libraries in the folder are removed.
pub fn cache_path_for(identity: &[u8; 32]) -> PathBuf {
    let dir = crate::session::default_dir();
    let name = format!("pso-cache-{}.bin", identity[..8].iter().map(|b| format!("{b:02x}")).collect::<String>());
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("pso-cache") && n.ends_with(".bin") && n != name {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{self, Input};
    use crate::ondemand::OnDemand;

    const LOCK: &str = r"D:\Developer\triumph-rebuild\agents\MEASURING";

    /// A device for a GPU test: WARP (correctness, no card), else the card under the exclusive `agents\CARD`
    /// lock (held until the returned guard drops). `None` (test skipped, reason printed) while the measurement
    /// lock is present, while another lane holds the card, or when no device with SM 6.6 exists.
    fn device() -> Option<(ID3D12Device, Option<crate::d3d::CardLock>)> {
        if Path::new(LOCK).exists() {
            eprintln!("skipped: the measurement lock is present");
            return None;
        }
        let warp = match crate::d3d::Gpu::warp() {
            Ok(g) => return Some((g.device, None)),
            Err(e) => e,
        };
        let card = match crate::d3d::CardLock::take() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipped: no WARP ({warp}) and {e}");
                return None;
            }
        };
        match crate::d3d::Gpu::new(false) {
            Ok(g) => Some((g.device, Some(card))),
            Err(e) => {
                eprintln!("skipped: {e}");
                None
            }
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rebuild-pso-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The smallest compute program DXC makes (`cs_6_0`, `[numthreads(1,1,1)] void main() {}`), signed,
    /// if `dxc.exe` is on the machine; tests that need a real pipeline skip without it.
    fn tiny_cs(d: &Path) -> Option<Vec<u8>> {
        let hl = d.join("t.hlsl");
        std::fs::write(&hl, "[numthreads(1,1,1)] void main() {}").ok()?;
        let out = d.join("t.dxil");
        let dxc = std::env::var_os("REBUILD_DXC").map(PathBuf::from).or_else(|| {
            let root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
            let mut v: Vec<_> = std::fs::read_dir(root).ok()?.filter_map(|e| e.ok().map(|e| e.path().join("x64").join("dxc.exe"))).filter(|p| p.exists()).collect();
            v.sort();
            v.pop()
        });
        let Some(dxc) = dxc else {
            eprintln!("skipped: no dxc.exe (Windows SDK or REBUILD_DXC)");
            return None;
        };
        let ok = std::process::Command::new(&dxc).args(["-T", "cs_6_6", "-E", "main", "-Fo"]).arg(&out).arg(&hl).output().ok()?.status.success();
        if !ok {
            eprintln!("skipped: dxc did not compile the test shader");
            return None;
        }
        std::fs::read(&out).ok()
    }

    fn programs(device: &ID3D12Device, d: &Path, cs: Vec<u8>) -> Programs {
        let path = d.join("programs.lib");
        let input = vec![
            Input { hash: 1, stage: Stage::Cs, dxil: cs.clone(), layout: StageLayout { user_data: true, shadow: true, ..Default::default() } },
            // a static sampler: the only thing that makes two compute root signatures differ
            Input { hash: 2, stage: Stage::Cs, dxil: cs, layout: StageLayout { samplers: vec![[0, 15 << 20, 1 << 20 | 1 << 22 | 2 << 26, 0]], ..Default::default() } },
        ];
        library::build(&path, 16, input, 1, crate::d3d::serialize).unwrap();
        Programs::new(device, Library::load(&path, 1, true).unwrap().0, None)
    }

    #[test]
    fn cache_round_trip_and_refused_cache_rebuilt() {
        let Some((device, _card)) = device() else { return };
        let d = dir("cache");
        let Some(cs) = tiny_cs(&d) else { return };
        let cache = d.join("pso-cache.bin");
        let (k1, d1) = (Key::of(&Desc::Compute { cs: 1 }), Desc::Compute { cs: 1 });
        let (k2, d2) = (Key::of(&Desc::Compute { cs: 2 }), Desc::Compute { cs: 2 });

        // store: an empty cache, two pipelines created and stored, saved on drop
        {
            let c = PsoCache::open(&device, &cache).unwrap();
            assert!(c.is_empty() && c.report.refused.is_none(), "a missing file is an empty cache, not a refusal");
            let cr = PsoCreator::new(&device, programs(&device, &d, cs.clone()), Some(c));
            cr.create(&k1, &d1).unwrap();
            cr.create(&k2, &d2).unwrap();
            let s = cr.stats().compute;
            assert_eq!((s.created, s.from_cache), (2, 0));
            assert_eq!(cr.cache().unwrap().unsaved(), 2);
        }
        // reload: both come from the library
        {
            let c = PsoCache::open(&device, &cache).unwrap();
            assert_eq!((c.len(), c.report.refused.as_deref()), (2, None));
            assert!(c.report.bytes > 0);
            let cr = PsoCreator::new(&device, programs(&device, &d, cs.clone()), Some(c));
            cr.create(&k2, &d2).unwrap();
            cr.create(&k1, &d1).unwrap();
            let s = cr.stats().total();
            assert_eq!((s.created, s.from_cache, s.cache_mismatch), (0, 2, 0));
        }
        // a key whose description changed (a key collision, or a stale entry): the library refuses to load it
        // and the driver creates it, never a wrong pipeline
        {
            let c = PsoCache::open(&device, &cache).unwrap();
            let cr = PsoCreator::new(&device, programs(&device, &d, cs.clone()), Some(c));
            cr.create(&k1, &d2).unwrap();
            let s = cr.stats().compute;
            assert_eq!((s.created, s.from_cache, s.cache_mismatch), (1, 0, 1));
        }
        // a blob the driver refuses (right header and hash, damaged driver bytes): rebuilt, logged, usable
        {
            let mut b = std::fs::read(&cache).unwrap();
            let blob = u64::from_le_bytes(b[16..24].try_into().unwrap()) as usize;
            for x in &mut b[HEADER..HEADER + blob.min(64)] {
                *x ^= 0x5a;
            }
            let h = blake3::hash(&b[HEADER..]);
            b[24..56].copy_from_slice(h.as_bytes());
            std::fs::write(&cache, &b).unwrap();
            let c = PsoCache::open(&device, &cache).unwrap();
            let why = c.report.refused.clone().expect("the driver refuses a damaged library");
            assert!(why.contains("refused") || why.contains("driver") || why.contains("adapter"), "{why}");
            assert!(c.is_empty());
            assert_eq!(c.events.lock().unwrap().len(), 1);
            let cr = PsoCreator::new(&device, programs(&device, &d, cs.clone()), Some(c));
            cr.create(&k1, &d1).unwrap();
            assert_eq!(cr.stats().compute.created, 1);
        }
        // ... and the rebuilt file loads clean, with the one pipeline
        {
            let c = PsoCache::open(&device, &cache).unwrap();
            assert_eq!((c.len(), c.report.refused.as_deref()), (1, None));
        }
        // a damaged file (hash mismatch) never reaches the driver
        {
            let mut b = std::fs::read(&cache).unwrap();
            let n = b.len();
            b[n - 1] ^= 1;
            std::fs::write(&cache, &b).unwrap();
            let c = PsoCache::open(&device, &cache).unwrap();
            assert_eq!(c.report.refused.as_deref(), Some("cache content hash mismatch"));
            assert!(c.is_empty());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_pool_creates_real_pipelines_asynchronously() {
        let Some((device, _card)) = device() else { return };
        let d = dir("pool");
        let Some(cs) = tiny_cs(&d) else { return };
        let cr = PsoCreator::new(&device, programs(&device, &d, cs), None);
        let pool = crate::pool::Pool::new(cr, 2);
        let h: Vec<_> = [1u64, 2, 1, 2, 3].iter().map(|&c| pool.request(Key::of(&Desc::Compute { cs: c }), Desc::Compute { cs: c })).collect();
        assert_eq!((h[0], h[1]), (h[2], h[3]));
        assert!(pool.wait(h[0]).is_ok() && pool.wait(h[1]).is_ok());
        assert!(pool.wait(h[4]).unwrap_err().contains("not in the program library"));
        let s = pool.creator().stats().compute;
        assert_eq!((s.created, s.refused), (2, 0));
        drop(pool);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A vertex and a pixel program absent from the library (the loading screen's first draw uses two that are not
    /// in `shaders.rpf`): offered at creation, translated on demand, given a root signature from their layouts.
    #[test]
    fn graphics_programs_outside_the_library_are_translated_on_demand() {
        let Some((device, _card)) = device() else { return };
        let d = dir("ondemand-gfx");
        let Some(cs) = tiny_cs(&d) else { return };
        let rpf = std::env::var_os("REBUILD_SHADERS").map_or_else(|| PathBuf::from(r"D:\victory-data\shaders.rpf"), PathBuf::from);
        let Ok(rpf) = std::fs::read(&rpf) else {
            eprintln!("skipped: no corpus copy at {}", rpf.display());
            return;
        };
        let out = std::env::var_os("REBUILD_TRANSLATE_OUT").map_or_else(|| PathBuf::from(r"D:\victory-data\translate"), PathBuf::from);
        let pick = |stage: Stage| {
            let agnostic = |h: u64| std::fs::read_to_string(out.join(format!("{h:016x}.{}.hlsl", stage.name()))).is_ok_and(|s| !s.contains("[WaveSize("));
            crate::translate::stage_containers(&rpf, stage, 1, |h| out.join(format!("{h:016x}.{}.dxil", stage.name())).exists() && agnostic(h)).pop()
        };
        let (Some((vs, vb)), Some((ps, pb))) = (pick(Stage::Vs), pick(Stage::Ps)) else {
            eprintln!("skipped: no translated vertex and pixel programs in {}", out.display());
            return;
        };
        let p = programs(&device, &d, cs).with_ondemand(OnDemand::new(Some(d.join("ondemand"))));
        assert_eq!(p.offer(vs, Stage::Vs, &vb), Some(Source::OnDemand));
        assert_eq!(p.offer(ps, Stage::Ps, &pb), Some(Source::OnDemand));
        assert_eq!(p.offer(1, Stage::Cs, &[]), Some(Source::Library));
        let (v, f, _root) = p.graphics(vs, Some(ps)).unwrap_or_else(|e| panic!("{vs:016x} + {ps:016x}: {e}"));
        assert!(matches!(v, Code::OnDemand(_)) && matches!(f, Some(Code::OnDemand(_))));
        assert_eq!(&v[..4], b"DXBC");
        assert_eq!(&f.as_deref().unwrap()[..4], b"DXBC");
        // the vertex program alone, and the wrong stage for a hash
        assert!(p.graphics(vs, None).is_ok());
        assert!(p.graphics(ps, None).err().unwrap().contains("not vs"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Compute programs absent from a tiny library: offered as the title creates them, translated on a pool
    /// worker, a real pipeline on the GPU; the next session (a fresh pool, nothing offered: a prewarm from the
    /// session record) takes them from the disk cache with no DXC call.
    #[test]
    fn programs_outside_the_library_are_translated_on_demand() {
        let Some((device, _card)) = device() else { return };
        let d = dir("ondemand");
        let Some(cs) = tiny_cs(&d) else { return };
        let rpf = std::env::var_os("REBUILD_SHADERS").map_or_else(|| PathBuf::from(r"D:\victory-data\shaders.rpf"), PathBuf::from);
        let Ok(rpf) = std::fs::read(&rpf) else {
            eprintln!("skipped: no corpus copy at {}", rpf.display());
            return;
        };
        let out = std::env::var_os("REBUILD_TRANSLATE_OUT").map_or_else(|| PathBuf::from(r"D:\victory-data\translate"), PathBuf::from);
        // compute programs translate compiled (so DXC accepts them), none of them in the tiny library; agnostic ones
        // only (a `translate v2` output also holds wave-class programs, and `[WaveSize(32)]` is refused on WARP)
        let agnostic = |h: u64| std::fs::read_to_string(out.join(format!("{h:016x}.cs.hlsl"))).is_ok_and(|s| !s.contains("[WaveSize("));
        let progs = crate::translate::compute_containers(&rpf, 3, |h| out.join(format!("{h:016x}.cs.dxil")).exists() && agnostic(h));
        if progs.is_empty() {
            eprintln!("skipped: no translate v1 compute output in {}", out.display());
            return;
        }
        let od_dir = d.join("ondemand");
        let session = |offer: bool| {
            let p = programs(&device, &d, cs.clone()).with_ondemand(OnDemand::new(Some(od_dir.clone())));
            let pool = crate::pool::Pool::new(PsoCreator::new(&device, p, None), 2);
            let hs: Vec<_> = progs
                .iter()
                .map(|(h, bytes)| {
                    if offer {
                        assert_eq!(pool.creator().programs().offer_compute(*h, bytes), Some(Source::OnDemand));
                    }
                    let desc = Desc::Compute { cs: *h };
                    if offer { pool.request(Key::of(&desc), desc) } else { pool.prewarm(Key::of(&desc), desc) }
                })
                .collect();
            for (h, (cs, _)) in hs.iter().zip(&progs) {
                pool.wait(*h).unwrap_or_else(|e| panic!("{cs:016x}: {e}"));
                let (code, _root) = pool.creator().programs().compute(*cs).unwrap();
                assert_eq!(&code[..4], b"DXBC");
            }
            // the library's own programs are unaffected
            assert_eq!(pool.creator().programs().offer_compute(1, &[]), Some(Source::Library));
            let s = pool.creator().programs().ondemand().unwrap().stats();
            assert_eq!(pool.creator().stats().compute.created, progs.len() as u64);
            eprintln!(
                "on demand: {} programs · translated {} (lift {:.2} ms, dxc {:.1} ms each) · from disk {} (disk {:.2} ms each) · PSO {:.2} ms each",
                progs.len(),
                s.translated,
                s.lift.as_secs_f64() * 1e3 / progs.len() as f64,
                s.dxc.as_secs_f64() * 1e3 / s.translated.max(1) as f64,
                s.from_disk,
                s.disk.as_secs_f64() * 1e3 / progs.len() as f64,
                pool.creator().stats().compute.create_ms()
            );
            s
        };
        let cold = session(true);
        assert_eq!((cold.offered, cold.translated, cold.from_disk, cold.failed), (progs.len() as u64, progs.len() as u64, 0, 0));
        let warm = session(false);
        assert_eq!((warm.offered, warm.translated, warm.from_disk, warm.failed), (0, 0, progs.len() as u64, 0), "the second session calls no DXC");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
