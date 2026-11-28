//! One program, ours and the oracle's, on the same inputs: the plan (user data, heap slots and their views),
//! the resources, the runs, and the word-by-word comparison.

use std::collections::{BTreeMap, BTreeSet};

use rebuild_pipelines::layout::{self, RootDesc, Sampler, StageLayout, Visibility};
use rebuild_shader::native::{ResourceType, RootType};
use rebuild_sir::sampler::Static;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::gpu::{buffer_desc, texture_desc, Gpu, Res};
use super::{Heap, Oracle, Prog, Row, View};

/// Slots each descriptor table gets in its heap.
const TABLE_SLOTS: u32 = 256;
/// First resource-heap slot of the tables (slots below are for dynamic indexing from the heap base 0).
const TABLE_BASE: u32 = 4096;
const SAMPLER_TABLE_SLOTS: u32 = 64;
const SAMPLER_TABLE_BASE: u32 = 512;
/// Buffers are 64 KiB (a constant buffer's most).
const BUFFER_BYTES: u64 = 65536;
/// The input sets every program runs on.
pub const SETS: u32 = 3;

/// A seeded word for input set `set`, stream `salt`, index `i`: set 0 small integers (0-7), set 1 floats in
/// [-2, 2), set 2 alternating small integers (0-15) and floats in [0, 4).
pub fn word(set: u32, salt: u64, i: u64) -> u32 {
    let mut x = (salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ i.wrapping_mul(0xD1B5_4A32_D192_ED03) ^ (set as u64 + 1) * 0x94D0_49BB_1331_11EB) | 1;
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    let r = (x >> 32) as u32;
    let unit = (r >> 8) as f32 / (1u32 << 24) as f32;
    match set {
        0 => r & 7,
        1 => (unit * 4.0 - 2.0).to_bits(),
        _ => {
            if i % 2 == 0 {
                r & 15
            } else {
                (unit * 4.0).to_bits()
            }
        }
    }
}

/// Floats in [0, 1): texture contents.
pub fn texel(set: u32, salt: u64, i: u64) -> u32 {
    let r = word(1, salt ^ 0x5151, i ^ (set as u64) << 40);
    ((f32::from_bits(r) + 2.0) / 4.0).to_bits()
}

pub fn bytes_of(words: impl Iterator<Item = u32>) -> Vec<u8> {
    words.flat_map(|w| w.to_le_bytes()).collect()
}

/// What the program's inputs are: user data, and the view at each heap slot.
#[derive(Default)]
pub struct Plan {
    /// user-data dword → (value per input set); tables are fixed, constants vary
    pub ud: [[u32; 16]; SETS as usize],
    pub slots: BTreeMap<u32, View>,
    pub sampler_slots: BTreeMap<u32, View>,
    pub notes: Vec<&'static str>,
    /// raw buffers read by index: the record size the shadow table gives (the access's size, as the oracle assumes)
    pub strides: BTreeMap<u32, u32>,
    /// raw buffer slots the oracle reads as constant buffers: the title's root signature names the range a CBV
    /// (the fork's translation reads a buffer through a CBV range as a CBV; ours reads every heap buffer raw)
    pub oracle_cbv: BTreeSet<u32>,
}

/// `record`: dynamic indices are left to the recording pass ([`super::record`]): no region is filled for them.
pub fn plan(p: &Prog, record: bool) -> Result<Plan, String> {
    let mut pl = Plan::default();
    // each table at its own place: resource tables 256 slots apart, sampler tables 64
    let mut table_base: BTreeMap<u8, (Heap, u32)> = BTreeMap::new();
    let (mut nres, mut nsmp) = (0u32, 0u32);
    let mut consts = Vec::new();
    for prm in &p.layout {
        if prm.user_data >= 16 && !matches!(prm.root, RootType::ResourceHeap | RootType::SamplerHeap) {
            continue; // past the root constants: a read of it is refused by the translator
        }
        if table_base.contains_key(&prm.user_data) {
            continue; // another range of the same table
        }
        match prm.root {
            RootType::Table => {
                let sampler = p.used.iter().any(|u| u.user_data == prm.user_data && u.kind == ResourceType::Sampler);
                let (heap, base) = if sampler {
                    nsmp += 1;
                    (Heap::Sampler, SAMPLER_TABLE_BASE + (nsmp - 1) * SAMPLER_TABLE_SLOTS)
                } else {
                    nres += 1;
                    (Heap::Resource, TABLE_BASE + (nres - 1) * TABLE_SLOTS)
                };
                table_base.insert(prm.user_data, (heap, base));
                let bytes = base * if heap == Heap::Sampler { 16 } else { 32 };
                for s in 0..SETS as usize {
                    pl.ud[s][prm.user_data as usize] = bytes;
                }
            }
            RootType::ResourceHeap | RootType::SamplerHeap => {
                table_base.insert(prm.user_data, (if prm.root == RootType::SamplerHeap { Heap::Sampler } else { Heap::Resource }, 0));
            }
            _ => {
                for k in 0..prm.sgprs.max(1) as u8 {
                    let d = prm.user_data + k;
                    if d < 16 {
                        consts.push(d);
                        for s in 0..SETS {
                            pl.ud[s as usize][d as usize] = word(s, 7, d as u64);
                        }
                    }
                }
            }
        }
    }
    let mut dynamic: BTreeMap<(Heap, u32), View> = BTreeMap::new();
    for u in &p.uses {
        let (heap, base) = *table_base.get(&u.user_data).ok_or("descriptor from no table")?;
        let stride = if heap == Heap::Sampler { 16 } else { 32 };
        let map = if heap == Heap::Sampler { &mut pl.sampler_slots } else { &mut pl.slots };
        match u.byte_off {
            Some(off) => {
                let slot = ((base * stride) as i64 + off as i64) as u32 / stride;
                let limit = if heap == Heap::Sampler { super::gpu::SAMPLER_SLOTS } else { super::gpu::HEAP_SLOTS };
                if slot >= limit {
                    return Err(format!("descriptor slot {slot} past the heap"));
                }
                put(map, slot, u.view)?;
            }
            None if record => pl.notes.push("recorded indices"),
            None => {
                pl.notes.push("dynamic index");
                match dynamic.get(&(heap, base)) {
                    Some(v) if !same_kind(*v, u.view) => return Err("dynamic descriptor index over mixed views".into()),
                    Some(v) => {
                        let m = merge(*v, u.view)?;
                        dynamic.insert((heap, base), m);
                    }
                    None => {
                        dynamic.insert((heap, base), u.view);
                    }
                }
            }
        }
        if let (Some(off), View::Raw, Heap::Resource) = (u.byte_off, u.view, heap) {
            let dword = u32::try_from(off).unwrap_or(0) / 4;
            let range = p.used.iter().filter(|r| r.root == RootType::Table && r.user_data == u.user_data && r.table_offset_dwords <= dword).max_by_key(|r| r.table_offset_dwords);
            if range.is_some_and(|r| r.kind == ResourceType::Cbv) {
                pl.oracle_cbv.insert(((base * 32) as i64 + off as i64) as u32 / 32);
            }
        }
        if u.shadow != 0 {
            if let Some(off) = u.byte_off {
                let (_, base) = table_base[&u.user_data];
                let slot = ((base * 32) as i64 + off as i64) as u32 / 32;
                // the record size the title's root signature gives the range (a structured buffer's stride), as the
                // runtime would write it from the descriptor; with none, the end of the furthest record read
                let dword = u32::try_from(off).unwrap_or(0) / 4;
                let declared = p
                    .used
                    .iter()
                    .filter(|r| r.root == RootType::Table && r.user_data == u.user_data && r.table_offset_dwords <= dword)
                    .max_by_key(|r| r.table_offset_dwords)
                    .map_or(0, |r| r.stride);
                let e = pl.strides.entry(slot).or_insert(0);
                if declared > 0 {
                    pl.notes.push("shadow stride from the root signature");
                    *e = declared;
                } else {
                    pl.notes.push("shadow stride = record end");
                    *e = (*e).max(u32::from(u.shadow).next_multiple_of(4));
                }
            }
        }
    }
    // an index computed from user data must land in the filled region: constants are small integers in every set
    if !dynamic.is_empty() || pl.notes.contains(&"recorded indices") {
        for &d in &consts {
            for s in 0..SETS {
                pl.ud[s as usize][d as usize] = word(0, 7 + s as u64, d as u64);
            }
        }
    }
    for ((heap, base), v) in dynamic {
        let n = if heap == Heap::Sampler { SAMPLER_TABLE_SLOTS } else { TABLE_SLOTS };
        let map = if heap == Heap::Sampler { &mut pl.sampler_slots } else { &mut pl.slots };
        for s in base..base + n {
            put(map, s, v)?;
        }
    }
    Ok(pl)
}

fn same_kind(a: View, b: View) -> bool {
    merge(a, b).is_ok()
}

/// Two reads of one descriptor: a load and a store of the same raw buffer are one UAV.
pub fn merge(a: View, b: View) -> Result<View, String> {
    Ok(match (a, b) {
        _ if a == b => a,
        (View::Raw, View::RwRaw) | (View::RwRaw, View::Raw) => View::RwRaw,
        (View::Typed, View::RwTyped) | (View::RwTyped, View::Typed) => View::RwTyped,
        (View::Sampler, View::SamplerCmp) | (View::SamplerCmp, View::Sampler) => return Err("one sampler read as plain and comparison".into()),
        _ => return Err(format!("one descriptor read as {a:?} and {b:?}")),
    })
}

fn put(map: &mut BTreeMap<u32, View>, slot: u32, v: View) -> Result<(), String> {
    let m = match map.get(&slot) {
        Some(old) => merge(*old, v)?,
        None => v,
    };
    map.insert(slot, m);
    Ok(())
}

/// A slot's resource and how its view is made.
struct Bound {
    res: Res,
    view: View,
}

fn srv_desc(view: View) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut d = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: Default::default(),
    };
    match view {
        View::Raw => {
            d.Format = DXGI_FORMAT_R32_TYPELESS;
            d.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
            d.Anonymous.Buffer = D3D12_BUFFER_SRV { FirstElement: 0, NumElements: (BUFFER_BYTES / 4) as u32, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_RAW };
        }
        View::Typed => {
            d.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
            d.Anonymous.Buffer = D3D12_BUFFER_SRV { FirstElement: 0, NumElements: (BUFFER_BYTES / 16) as u32, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_NONE };
        }
        View::Tex(0) => {
            d.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1D;
            d.Anonymous.Texture1D = D3D12_TEX1D_SRV { MipLevels: u32::MAX, ..Default::default() };
        }
        View::Tex(2) => {
            d.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE3D;
            d.Anonymous.Texture3D = D3D12_TEX3D_SRV { MipLevels: u32::MAX, ..Default::default() };
        }
        View::Tex(3) => {
            d.ViewDimension = D3D12_SRV_DIMENSION_TEXTURECUBE;
            d.Anonymous.TextureCube = D3D12_TEXCUBE_SRV { MipLevels: u32::MAX, ..Default::default() };
        }
        View::Tex(4) => {
            d.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1DARRAY;
            d.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_SRV { MipLevels: u32::MAX, ArraySize: 4, ..Default::default() };
        }
        View::Tex(5) => {
            d.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DARRAY;
            d.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_SRV { MipLevels: u32::MAX, ArraySize: 4, ..Default::default() };
        }
        _ => {
            d.Anonymous.Texture2D = D3D12_TEX2D_SRV { MipLevels: u32::MAX, ..Default::default() };
        }
    }
    d
}

fn uav_desc(view: View) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut d = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: DXGI_FORMAT_R32G32B32A32_FLOAT, ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D, Anonymous: Default::default() };
    match view {
        View::RwRaw => {
            d.Format = DXGI_FORMAT_R32_TYPELESS;
            d.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
            d.Anonymous.Buffer = D3D12_BUFFER_UAV { FirstElement: 0, NumElements: (BUFFER_BYTES / 4) as u32, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW };
        }
        View::RwTyped => {
            d.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
            d.Anonymous.Buffer = D3D12_BUFFER_UAV { FirstElement: 0, NumElements: (BUFFER_BYTES / 16) as u32, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_NONE };
        }
        View::RwTex(0) => d.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D,
        View::RwTex(2) => {
            d.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            d.Anonymous.Texture3D = D3D12_TEX3D_UAV { MipSlice: 0, FirstWSlice: 0, WSize: u32::MAX };
        }
        View::RwTex(4) => {
            d.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            d.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV { MipSlice: 0, FirstArraySlice: 0, ArraySize: 4 };
        }
        View::RwTex(5) => {
            d.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            d.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV { MipSlice: 0, FirstArraySlice: 0, ArraySize: 4, PlaneSlice: 0 };
        }
        _ => {}
    }
    d
}

/// (resource desc, texel count) for a texture view of console `dim`; `rw`: one mip.
fn tex_desc(dim: u8, rw: bool) -> (D3D12_RESOURCE_DESC, D3D12_RESOURCE_FLAGS) {
    let flags = if rw { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE };
    let f = DXGI_FORMAT_R32G32B32A32_FLOAT;
    let m = |n: u16| if rw { 1 } else { n };
    let d = match dim {
        0 => texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE1D, 256, 1, 1, m(9), f, flags),
        2 => texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE3D, 32, 32, 32, m(6), f, flags),
        3 => texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 64, 64, 6, m(7), f, flags),
        4 => texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE1D, 256, 1, 4, m(9), f, flags),
        5 => texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 128, 128, 4, m(8), f, flags),
        _ => texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 128, 128, 1, m(8), f, flags),
    };
    (d, flags)
}

/// Bytes a texture's subresources hold, tightly packed (16 bytes a texel).
fn tex_bytes(d: &D3D12_RESOURCE_DESC) -> usize {
    let (mut w, mut h, mut z) = (d.Width as usize, d.Height as usize, if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { d.DepthOrArraySize as usize } else { 1 });
    let slices = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { d.DepthOrArraySize as usize };
    let mut n = 0;
    for _ in 0..d.MipLevels {
        n += w * h * z;
        w = (w / 2).max(1);
        h = (h / 2).max(1);
        z = (z / 2).max(1);
    }
    n * slices * 16
}

/// Makes the resource behind one view, with input set `set`'s contents.
fn make(g: &Gpu, view: View, set: u32, salt: u64) -> Result<Res, String> {
    let words = |n: usize| bytes_of((0..n as u64).map(|i| word(set, salt, i)));
    let srv = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
    let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    match view {
        View::Cbv => g.resource(buffer_desc(BUFFER_BYTES, false), D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER, false, words(BUFFER_BYTES as usize / 4)),
        // a raw buffer may also be the oracle's CBV (`Plan::oracle_cbv`)
        View::Raw | View::Typed => g.resource(buffer_desc(BUFFER_BYTES, false), srv | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER, false, words(BUFFER_BYTES as usize / 4)),
        View::RwRaw | View::RwTyped => g.resource(buffer_desc(BUFFER_BYTES, true), uav, true, words(BUFFER_BYTES as usize / 4)),
        View::Tex(dim) => {
            let (d, _) = tex_desc(dim, false);
            let n = tex_bytes(&d) / 4;
            g.resource(d, srv, false, bytes_of((0..n as u64).map(|i| texel(set, salt, i))))
        }
        View::RwTex(dim) => {
            let (d, _) = tex_desc(dim, true);
            let n = tex_bytes(&d) / 4;
            g.resource(d, uav, true, words(n))
        }
        View::Sampler | View::SamplerCmp => Err("a sampler is no resource".into()),
    }
}

fn describe(g: &Gpu, slot: u32, b: &Bound) {
    describe_as(g, slot, &b.res, b.view);
}

fn describe_as(g: &Gpu, slot: u32, res: &Res, view: View) {
    unsafe {
        match view {
            View::Cbv => {
                let d = D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: res.r.GetGPUVirtualAddress(), SizeInBytes: BUFFER_BYTES as u32 };
                g.device.CreateConstantBufferView(Some(&d), g.slot(slot));
            }
            View::Raw | View::Typed | View::Tex(_) => g.device.CreateShaderResourceView(&res.r, Some(&srv_desc(view)), g.slot(slot)),
            View::RwRaw | View::RwTyped | View::RwTex(_) => g.device.CreateUnorderedAccessView(&res.r, None, Some(&uav_desc(view)), g.slot(slot)),
            View::Sampler | View::SamplerCmp => {}
        }
    }
}

fn oracle_root(statics: &[[u32; 10]], ours: &RootDesc, vis: Visibility) -> RootDesc {
    let samplers = statics
        .iter()
        .enumerate()
        .map(|(k, s)| Sampler {
            desc: Static {
                filter: s[0],
                address: [s[1], s[2], s[3]],
                mip_lod_bias: f32::from_bits(s[4]),
                max_anisotropy: s[5],
                comparison: s[6],
                border: s[7],
                min_lod: f32::from_bits(s[8]),
                max_lod: f32::from_bits(s[9]),
                approx: None,
            },
            register: k as u32,
            space: 0,
            vis,
        })
        .collect();
    RootDesc { params: ours.params.clone(), samplers, flags: ours.flags }
}

/// How two runs' words differ.
#[derive(Default, Debug)]
pub struct Diff {
    pub words: usize,
    pub changed: usize,
    pub differ: usize,
    pub max_ulp: u64,
    pub nan_mismatch: usize,
    pub nan_payload: usize,
    pub signed_zero: usize,
    pub first: Option<(usize, usize, u32, u32)>,
}

fn ordered(x: u32) -> i64 {
    if x >> 31 == 1 {
        -((x & 0x7fff_ffff) as i64)
    } else {
        x as i64
    }
}

pub fn compare(d: &mut Diff, res: usize, init: &[u8], a: &[u8], b: &[u8]) {
    let w = |s: &[u8], i: usize| u32::from_le_bytes(s[i * 4..i * 4 + 4].try_into().unwrap());
    let n = a.len().min(b.len()).min(init.len()) / 4;
    d.words += n;
    for i in 0..n {
        let (x, y, z) = (w(a, i), w(b, i), w(init, i));
        if x != z || y != z {
            d.changed += 1;
        }
        if x == y {
            continue;
        }
        d.differ += 1;
        if d.first.is_none() {
            d.first = Some((res, i, x, y));
        }
        let (fx, fy) = (f32::from_bits(x), f32::from_bits(y));
        if fx.is_nan() && fy.is_nan() {
            d.nan_payload += 1;
        } else if fx.is_nan() || fy.is_nan() {
            d.nan_mismatch += 1;
        } else if fx == 0.0 && fy == 0.0 {
            d.signed_zero += 1;
        } else {
            d.max_ulp = d.max_ulp.max(ordered(x).abs_diff(ordered(y)));
        }
    }
}

impl Diff {
    /// The verdict and its named reason.
    pub fn verdict(&self) -> (&'static str, String) {
        if self.differ == 0 {
            return match (self.words, self.changed) {
                (0, _) => ("vacuous", "no outputs to compare (the program writes nothing)".into()),
                (_, 0) => ("vacuous", "no output word written".into()),
                _ => ("match", String::new()),
            };
        }
        let other = self.differ - self.nan_payload - self.signed_zero - self.nan_mismatch;
        let reason = if other == 0 && self.nan_mismatch == 0 && self.signed_zero == 0 {
            "NaN payload only".to_string()
        } else if other == 0 && self.nan_mismatch == 0 {
            "signed zero".to_string()
        } else if self.nan_mismatch > 0 && other == 0 {
            "NaN against a number".to_string()
        } else if self.max_ulp <= 1 {
            "float within 1 ulp".to_string()
        } else if self.max_ulp <= 16 {
            "float within 16 ulp".to_string()
        } else if self.max_ulp <= 4096 {
            "float within 4096 ulp".to_string()
        } else {
            "words differ".to_string()
        };
        ("differ", reason)
    }
}

/// What one side's run needs besides its pipeline: the stage's draw or dispatch.
struct Draw<'a> {
    cs: bool,
    vsetup: Option<&'a super::vs::Setup>,
    targets: &'a [super::ps::Target],
    shadow: &'a ID3D12Resource,
    groups: [u32; 3],
}

/// The resources of one input set: one per slot (a dynamic region shares one per view), then the targets (and
/// for a vertex program the vertex and stream-output buffers).
fn resources(g: &Gpu, pl: &Plan, set: u32, d: &Draw) -> Result<(Vec<(Vec<u32>, Bound)>, Vec<Res>), (&'static str, String)> {
    let mut bound: Vec<(Vec<u32>, Bound)> = Vec::new();
    let mut by_view: BTreeMap<View, usize> = BTreeMap::new();
    let dynamic = pl.notes.contains(&"dynamic index");
    for (slot, v) in &pl.slots {
        if dynamic {
            if let Some(&k) = by_view.get(v) {
                bound[k].0.push(*slot);
                continue;
            }
        }
        let res = make(g, *v, set, *slot as u64).map_err(|e| ("input resource", e))?;
        by_view.insert(*v, bound.len());
        bound.push((vec![*slot], Bound { res, view: *v }));
    }
    let mut tres = super::ps::make_targets(g, d.targets, set).map_err(|e| ("render target", e))?;
    if let Some(st) = d.vsetup {
        let (vb, so) = super::vs::make(g, st, set).map_err(|e| ("vertex buffers", e))?;
        tres.push(vb);
        tres.push(so);
    }
    Ok((bound, tres))
}

/// The null bank after `CONSOLE_VIEWS` (`rebuild_sir::lift::NullKind`): a null view of each type translated code
/// declares, where `sir_vi` sends an index past the console range.
fn null_bank(g: &Gpu) {
    use rebuild_sir::lift::{NullKind, CONSOLE_VIEWS};
    for k in NullKind::all() {
        let slot = g.slot(CONSOLE_VIEWS + k.slot());
        let (srv, uav) = match k {
            NullKind::Raw => (Some(View::Raw), None),
            NullKind::Typed => (Some(View::Typed), None),
            NullKind::Tex(d) => (Some(View::Tex(d)), None),
            NullKind::RwRaw => (None, Some((View::RwRaw, None))),
            NullKind::RwTyped => (None, Some((View::RwTyped, None))),
            NullKind::RwTex(d) => (None, Some((View::RwTex(if d == 3 { 5 } else { d }), None))),
            NullKind::RwTexUint(d) => (None, Some((View::RwTex(if d == 3 { 5 } else { d }), Some(DXGI_FORMAT_R32_UINT)))),
        };
        unsafe {
            if let Some(v) = srv {
                g.device.CreateShaderResourceView(None, Some(&srv_desc(v)), slot);
            }
            if let Some((v, f)) = uav {
                let mut d = uav_desc(v);
                if let Some(f) = f {
                    d.Format = f;
                }
                g.device.CreateUnorderedAccessView(None, None, Some(&d), slot);
            }
        }
    }
}

/// The heaps for a plan: cleared, then every slot's view and sampler.
fn descriptors(g: &Gpu, pl: &Plan, bound: &[(Vec<u32>, Bound)]) {
    g.clear_heaps();
    null_bank(g);
    for (slots, b) in bound {
        for s in slots {
            describe(g, *s, b);
        }
    }
    for (s, v) in &pl.sampler_slots {
        g.sampler(*s, *v == View::SamplerCmp);
    }
}

/// Records one side's draw or dispatch (between `begin` and `end`).
/// `extra`: a further root SRV (parameter, buffer) the side's root signature has (the oracle's live-slot table).
fn record_draw(g: &Gpu, d: &Draw, pso: &ID3D12PipelineState, rs: &ID3D12RootSignature, ud: &[u32; 16], tres: &[Res], extra: Option<(u32, &ID3D12Resource)>) {
    unsafe {
        let l = &g.list;
        l.SetDescriptorHeaps(&[Some(g.heap.clone()), Some(g.samplers.clone())]);
        l.SetPipelineState(pso);
        let ud = ud.as_ptr() as *const _;
        if d.cs {
            l.SetComputeRootSignature(rs);
            l.SetComputeRoot32BitConstants(layout::param::CS_USER_DATA, 16, ud, 0);
            l.SetComputeRootShaderResourceView(layout::param::CS_SHADOW, d.shadow.GetGPUVirtualAddress());
            if let Some((k, b)) = extra {
                l.SetComputeRootShaderResourceView(k, b.GetGPUVirtualAddress());
            }
            l.Dispatch(d.groups[0], d.groups[1], d.groups[2]);
        } else {
            l.SetGraphicsRootSignature(rs);
            let user = if d.vsetup.is_some() { layout::param::VS_USER_DATA } else { layout::param::PS_USER_DATA };
            l.SetGraphicsRoot32BitConstants(user, 16, ud, 0);
            l.SetGraphicsRootShaderResourceView(layout::param::GFX_SHADOW, d.shadow.GetGPUVirtualAddress());
            if let Some((k, b)) = extra {
                l.SetGraphicsRootShaderResourceView(k, b.GetGPUVirtualAddress());
            }
            match d.vsetup {
                Some(st) => super::vs::draw(g, st, &tres[0], &tres[1]),
                None => super::ps::draw(g, d.targets, tres),
            }
        }
    }
}

/// The shadow table for a plan: raw buffers read by index hold records of the access's size (what the oracle
/// assumes), as many as fit; each slot's alternate (the view translated code reads at the entry's third word) is
/// the slot itself; the lead (fourth word) is 0.
fn shadow_table(pl: &Plan) -> Vec<u8> {
    use rebuild_sir::lift::{CONSOLE_VIEWS, PAGE_TABLE_BYTE, PAGE_TABLE_PAGES, VIEW_PAGE_SHIFT};
    // the whole shadow buffer translated code reads: an entry per console index up to CONSOLE_VIEWS, then the
    // page table (`rebuild_sir::lift::page_index`), here every page at its own index (the harness heap is the
    // console heap); pages past CONSOLE_VIEWS have no host page
    let mut shadow = vec![0u8; PAGE_TABLE_BYTE as usize + PAGE_TABLE_PAGES as usize * 4];
    for page in 0..PAGE_TABLE_PAGES {
        let host = page << VIEW_PAGE_SHIFT;
        let v = if host < CONSOLE_VIEWS { host } else { CONSOLE_VIEWS };
        let at = PAGE_TABLE_BYTE as usize + page as usize * 4;
        shadow[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    for (slot, v) in &pl.slots {
        let at = *slot as usize * 16;
        shadow[at + 8..at + 12].copy_from_slice(&slot.to_le_bytes());
        if matches!(v, View::Raw | View::RwRaw) || (*v == View::Cbv && pl.strides.contains_key(slot)) {
            let stride = pl.strides.get(slot).copied().unwrap_or(16);
            shadow[at..at + 4].copy_from_slice(&stride.to_le_bytes());
            shadow[at + 4..at + 8].copy_from_slice(&((BUFFER_BYTES / stride as u64) as u32).to_le_bytes());
        }
    }
    shadow
}

/// The recording pass ([`super::record`]): our instrumented program on every input set, until the slots it names
/// settle; they join the plan.
/// `at`: the output folder and hash, so a set that never finishes is named (`current`) and `skip`ped next time.
#[allow(clippy::too_many_arguments)]
fn record_plan(g: &mut Gpu, pl: &mut Plan, rec_pso: &ID3D12PipelineState, rs: &ID3D12RootSignature, d: &Draw, kinds: &[View], at: (&std::path::Path, &str), skip: &[u32]) -> Result<(), (&'static str, String)> {
    use super::record::*;
    for _round in 0..ROUNDS {
        let mut grew = false;
        for set in (0..SETS).filter(|s| !skip.contains(s)) {
            let _ = std::fs::write(super::shard_file(at.0, "current"), format!("{} {set}", at.1));
            let (bound, tres) = resources(g, pl, set, d)?;
            // a dummy of every kind the program names, the known table, the record
            let mut dummies = Vec::new();
            for v in kinds.iter().filter(|v| !matches!(v, View::Sampler | View::SamplerCmp)) {
                let res = make(g, *v, set, 0xD0 + bit(*v) as u64).map_err(|e| ("input resource", e))?;
                dummies.push((DUMMY_BASE + bit(*v), Bound { res, view: *v }));
            }
            let known = g
                .resource(buffer_desc(WORDS as u64 * 4, false), D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, false, known(pl))
                .map_err(|e| ("known table", e))?;
            let rec = g.resource(rec_desc(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, true, vec![0u8; WORDS as usize * 4]).map_err(|e| ("record", e))?;
            let shadow = g.upload_buffer(&shadow_table(pl)).map_err(|e| ("shadow table", e))?;
            let d = Draw { shadow: &shadow, ..*d };
            descriptors(g, pl, &bound);
            for (s, b) in &dummies {
                describe(g, *s, b);
            }
            unsafe {
                let raw = |n: u32| D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Buffer: D3D12_BUFFER_SRV { FirstElement: 0, NumElements: n, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_RAW } },
                };
                g.device.CreateShaderResourceView(&known.r, Some(&raw(WORDS)), g.slot(KNOWN_SLOT));
                let u = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Buffer: D3D12_BUFFER_UAV { FirstElement: 0, NumElements: WORDS, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW } },
                };
                g.device.CreateUnorderedAccessView(&rec.r, None, Some(&u), g.slot(REC_SLOT));
            }
            for (s, v) in [(SAMPLER_DUMMY, false), (SAMPLER_DUMMY + 1, true)] {
                g.sampler(s, v);
            }
            let all: Vec<&Res> = bound.iter().map(|(_, b)| &b.res).chain(dummies.iter().map(|(_, b)| &b.res)).chain(tres.iter()).chain([&known, &rec]).collect();
            g.begin();
            for r in &all {
                g.upload(r);
            }
            record_draw(g, &d, rec_pso, rs, &pl.ud[set as usize], &tres, None);
            g.download(&rec);
            g.end().map_err(|e| ("recording pass lost the device", e))?;
            if g.lost() {
                return Err(("recording pass lost the device", String::new()));
            }
            let words = g.read(&rec);
            grew |= absorb(pl, &words).map_err(|e| ("untested", e))?;
        }
        if !grew {
            return Ok(());
        }
    }
    Err(("untested", format!("recorded indices still growing after {} rounds", super::record::ROUNDS)))
}

/// Runs a program and the oracle's on every input set.
pub fn program(g: &mut Gpu, p: &Prog, o: &Oracle, out: &std::path::Path, skip: &[u32]) -> Row {
    let trace = std::env::var_os("REBUILD_EXEC_TRACE").is_some();
    let t0 = std::time::Instant::now();
    let step = |s: &str| {
        if trace {
            eprintln!("execcheck {} {:>6} ms: {s}", p.hash, t0.elapsed().as_millis());
        }
    };
    let row = |verdict: &'static str, reason: String, detail: String| Row { hash: p.hash.clone(), stage: p.stage, single: p.single, wave: p.wave, verdict, reason, detail };
    if o.wave > 32 {
        return row("untested", format!("the oracle needs wave size {} (no adapter here runs it)", o.wave), String::new());
    }
    if o.root_views > 0 && !o.live_table {
        return row("untested", "the oracle binds root views".into(), String::new());
    }
    // ours declares a wave size (wave-wide compute: native wave32, or wave64 doubled onto 32 lanes); WARP's waves
    // are 4 lanes, so the pipeline cannot be made here
    if let Some(at) = p.src.find("[WaveSize(") {
        let n = p.src[at + 10..].split(')').next().unwrap_or("?");
        if !g.card {
            return row("untested", format!("ours needs wave size {n} (WARP runs 4 lanes)"), String::new());
        }
    }
    // a dynamic index over several kinds of view: the slots come from the recording pass
    // a dynamic index over several kinds of view: the slots come from the recording pass. One kind keeps the filled
    // region: recording those too (with the recorded slots outside the region given views) turned 30 programs that
    // match into differing ones, where the oracle reads other slots than ours for an index from the data
    let record = std::env::var_os("REBUILD_EXEC_NORECORD").is_none();
    let (mut pl, recording) = match plan(p, false) {
        Ok(pl) => (pl, None),
        Err(e) if e == "dynamic descriptor index over mixed views" && record => {
            let pl = match plan(p, true) {
                Ok(pl) => pl,
                Err(e) => return row("untested", e, String::new()),
            };
            match super::record::instrument(&p.src) {
                Ok(r) => (pl, Some(r)),
                Err(e) => return row("untested", format!("dynamic descriptor index over mixed views; not instrumented: {e}"), String::new()),
            }
        }
        Err(e) => return row("untested", e, String::new()),
    };
    step(&format!("plan {:?}", p.uses));
    let (cs, vsx) = (p.stage == "cs", p.stage == "vs");
    let ours_layout = StageLayout { user_data: true, shadow: true, samplers: p.immediate.clone(), compare: p.compare };
    let ours_root = if cs {
        RootDesc::compute(&ours_layout, layout::USER_DATA_DWORDS)
    } else if vsx {
        let mut r = RootDesc::graphics(&ours_layout, &StageLayout::default(), layout::USER_DATA_DWORDS);
        r.flags |= 0x40; // ALLOW_STREAM_OUTPUT
        r
    } else {
        RootDesc::graphics(&StageLayout::default(), &ours_layout, layout::USER_DATA_DWORDS)
    };
    let vis = if cs {
        Visibility::All
    } else if vsx {
        Visibility::Vertex
    } else {
        Visibility::Pixel
    };
    let mut oracle_root = oracle_root(&o.statics, &ours_root, vis);
    // the fork's live-slot table (t255, space1): a root SRV of its own
    let live_param = o.live_table.then(|| {
        oracle_root.params.push(layout::Param::Srv { register: 255, space: 1, vis });
        oracle_root.params.len() as u32 - 1
    });
    let roots = [&ours_root, &oracle_root].map(|r| rebuild_pipelines::d3d::serialize(r).and_then(|b| g.root_signature(&b)));
    let (ours_rs, oracle_rs) = match roots {
        [Ok(a), Ok(b)] => (a, b),
        [Err(e), _] => return row("error", "our root signature refused".into(), e),
        [_, Err(e)] => return row("error", "the oracle's root signature refused".into(), e),
    };
    step("roots");
    let mut targets = if cs { Vec::new() } else { super::ps::targets_of(&p.outputs) };
    // a pixel program with no outputs (an alpha test that only discards): a depth buffer the rasteriser writes
    // shows which pixels it kept, the program's one effect
    let coverage = p.stage == "ps" && targets.is_empty();
    if coverage {
        targets.push(super::ps::Target::Depth);
    }
    let (mut kept, mut dropped) = (false, false);
    let mut vsetup = None;
    let mut feed = Vec::new();
    // the instrumented program (the recording pass), compiled beside ours
    let rec_dxil = match &recording {
        None => None,
        Some((src, _)) => {
            let stage = if cs {
                rebuild_pipelines::Stage::Cs
            } else if vsx {
                rebuild_pipelines::Stage::Vs
            } else {
                rebuild_pipelines::Stage::Ps
            };
            match compile_rec(src, stage, out, &p.hash) {
                Ok(b) => Some(b),
                Err(e) => return row("error", "the recording pass did not compile".into(), e),
            }
        }
    };
    let pipelines = if cs {
        (g.compute(&ours_rs, &p.dxil), g.compute(&oracle_rs, &o.dxil))
    } else if vsx {
        let oracle_in = rebuild_shader::Container::parse(&o.dxil)
            .ok()
            .and_then(|c| c.part(rebuild_shader::FourCC::ISG1).map(|x| rebuild_shader::signature::parse(x.data).unwrap_or_default()))
            .unwrap_or_default();
        let st = match super::vs::setup(&p.inputs, &oracle_in, &p.outputs) {
            Ok(st) => st,
            Err(e) => return row("untested", e, String::new()),
        };
        let r = (super::vs::pipeline(g, &ours_rs, &p.dxil, &st), super::vs::pipeline(g, &oracle_rs, &o.dxil, &st));
        vsetup = Some(st);
        r
    } else {
        feed = match super::ps::feed_vs(&p.src).and_then(|src| super::ps::compile_vs(&src, out, &p.hash)) {
            Ok(v) => v,
            Err(e) => return row("untested", "no feeding vertex shader".into(), e),
        };
        let mut theirs = super::ps::pipeline(g, &oracle_rs, &feed, &o.dxil, &targets);
        if theirs.is_err() {
            // the oracle packs its inputs into other registers: its own feeding vertex shader, built from its input
            // signature (the same value per semantic and component as ours)
            let oracle_in = rebuild_shader::Container::parse(&o.dxil)
                .ok()
                .and_then(|c| c.part(rebuild_shader::FourCC::ISG1).map(|x| rebuild_shader::signature::parse(x.data).unwrap_or_default()))
                .unwrap_or_default();
            let own = super::ps::oracle_struct(&oracle_in, &super::ps::psv_input_modes(&o.dxil), &p.src)
                .and_then(|st| super::ps::feed_vs(&st))
                .and_then(|src| super::ps::compile_vs(&src, out, &format!("{}.oracle", p.hash)));
            match own {
                Ok(f) => theirs = super::ps::pipeline(g, &oracle_rs, &f, &o.dxil, &targets),
                Err(e) => theirs = Err(format!("the oracle's feeding vertex shader: {e}")),
            }
        }
        (super::ps::pipeline(g, &ours_rs, &feed, &p.dxil, &targets), theirs)
    };
    let (ours, theirs) = match pipelines {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) => return row("error", "our pipeline refused".into(), e),
        // a pixel shader: the one feeding vertex shader (built from our input struct) links to ours; the oracle packs
        // its inputs into other registers (all sampled refusals were this linkage error)
        (_, Err(e)) if p.stage == "ps" => return row("untested", "the oracle's pixel pipeline refused (E_INVALIDARG with no debug message; its signatures match ours)".into(), e),
        (_, Err(e)) => return row("error", "the oracle's pipeline refused".into(), e),
    };
    step("pipelines");
    let t = p.threads;
    let groups = [(64 / t[0].max(1)).clamp(1, 4), (64 / t[1].max(1)).clamp(1, 4), 1];
    let placeholder = match g.upload_buffer(&[0u8; 256]) {
        Ok(b) => b,
        Err(e) => return row("error", "shadow table".into(), e),
    };
    if let (Some(dxil), Some((_, kinds))) = (&rec_dxil, &recording) {
        let rec_pso = if cs {
            g.compute(&ours_rs, dxil)
        } else if let Some(st) = &vsetup {
            super::vs::pipeline(g, &ours_rs, dxil, st)
        } else {
            super::ps::pipeline(g, &ours_rs, &feed, dxil, &targets)
        };
        let rec_pso = match rec_pso {
            Ok(x) => x,
            Err(e) => return row("error", "the recording pass's pipeline refused".into(), e),
        };
        let d = Draw { cs, vsetup: vsetup.as_ref(), targets: &targets, shadow: &placeholder, groups };
        if let Err((what, e)) = record_plan(g, &mut pl, &rec_pso, &ours_rs, &d, kinds, (out, &p.hash), skip) {
            return if what == "untested" { row("untested", e, String::new()) } else { row("crash", what.into(), e) };
        }
        step(&format!("recorded {} slots", pl.slots.len()));
    }
    let shadow = match g.upload_buffer(&shadow_table(&pl)) {
        Ok(b) => b,
        Err(e) => return row("error", "shadow table".into(), e),
    };
    let draw = Draw { cs, vsetup: vsetup.as_ref(), targets: &targets, shadow: &shadow, groups };
    // the oracle's live-slot table: a dword per heap slot (it clamps the index to 2^21 - 1), 1 where a view is
    let live = match live_param {
        None => None,
        Some(k) => {
            let mut t = vec![0u32; 1 << 21];
            for s in pl.slots.keys() {
                t[*s as usize] = 1;
            }
            match g.upload_buffer(&bytes_of(t.into_iter())) {
                Ok(b) => Some((k, b)),
                Err(e) => return row("error", "live-slot table".into(), e),
            }
        }
    };
    let mut diff = Diff::default();
    let mut per_set = Vec::new();
    let mut racing = false;
    for set in 0..SETS {
        if skip.contains(&set) {
            per_set.push(format!("set{set} did not complete"));
            continue;
        }
        let _ = std::fs::write(super::shard_file(out, "current"), format!("{} {set}", p.hash));
        let (bound, tres) = match resources(g, &pl, set, &draw) {
            Ok(x) => x,
            Err((what, e)) => return row("error", what.into(), e),
        };
        step("resources");
        descriptors(g, &pl, &bound);
        step("descriptors");
        let all: Vec<&Res> = bound.iter().map(|(_, b)| &b.res).chain(tres.iter()).collect();
        let mut outs: Vec<Vec<Vec<u8>>> = Vec::new();
        // ours, the oracle's, and (only when they differ) ours again: a program whose own runs differ writes
        // racing values (many lanes or pixels to one place), and is named so
        for side in 0..3 {
            if side == 2 && !all.iter().enumerate().any(|(k, r)| r.written && outs[0][k] != outs[1][k]) {
                break;
            }
            let (pso, rs) = if side == 1 { (&theirs, &oracle_rs) } else { (&ours, &ours_rs) };
            if !pl.oracle_cbv.is_empty() && side > 0 {
                // the oracle reads these slots as CBVs over the same bytes; ours as raw SRVs
                for (slots, b) in &bound {
                    for s in slots.iter().filter(|s| pl.oracle_cbv.contains(s)) {
                        let view = if side == 1 { View::Cbv } else { b.view };
                        describe_as(g, *s, &b.res, view);
                    }
                }
            }
            g.begin();
            for r in &all {
                g.upload(r);
            }
            let extra = if side == 1 { live.as_ref().map(|(k, b)| (*k, b)) } else { None };
            record_draw(g, &draw, pso, rs, &pl.ud[set as usize], &tres, extra);
            for r in &all {
                g.download(r);
            }
            if let Err(e) = g.end() {
                return row("crash", "device lost".into(), e);
            }
            outs.push(all.iter().map(|r| g.read(r)).collect());
        }
        step("ran");
        let mut d = Diff::default();
        for (k, r) in all.iter().enumerate() {
            if r.written {
                compare(&mut d, k, &r.init, &outs[0][k], &outs[1][k]);
            }
        }
        if outs.len() == 3 && all.iter().enumerate().any(|(k, r)| r.written && outs[0][k] != outs[2][k]) {
            racing = true;
        }
        per_set.push(format!("set{set} {}/{}w {}d", d.changed, d.words, d.differ));
        kept |= d.changed > 0;
        dropped |= d.changed < d.words;
        diff.words += d.words;
        diff.changed += d.changed;
        diff.differ += d.differ;
        diff.max_ulp = diff.max_ulp.max(d.max_ulp);
        diff.nan_mismatch += d.nan_mismatch;
        diff.nan_payload += d.nan_payload;
        diff.signed_zero += d.signed_zero;
        if diff.first.is_none() {
            diff.first = d.first.map(|(r, i, x, y)| (set as usize * 1000 + r, i, x, y));
        }
    }
    let (mut verdict, mut reason) = diff.verdict();
    if coverage {
        pl.notes.push("coverage only");
        // only coverage to compare: it counts when some set both kept and discarded pixels
        if verdict == "match" && !(kept && dropped) {
            (verdict, reason) = ("vacuous", "coverage only, and the sets never both keep and discard pixels".into());
        }
    }
    if verdict == "differ" && racing {
        reason = format!("racing writes (our own runs differ); {reason}");
    }
    // named: the oracle samples level 0 where a compute program samples with implicit derivatives (ours takes them
    // from the host quad, whose layout is not the console's either)
    if verdict == "differ" && cs && [".Sample(", ".SampleBias(", ".SampleCmp(", "CalculateLevelOfDetail"].iter().any(|s| p.src.contains(s)) {
        reason = format!("implicit-derivative sampling in compute (the oracle samples level 0); {reason}");
    }
    let mut detail = per_set.join(" ");
    if let Some((r, i, x, y)) = diff.first {
        detail += &format!(" first res{r} word{i} ours {x:#010x} oracle {y:#010x} max_ulp {}", diff.max_ulp);
    }
    if !pl.notes.is_empty() {
        let mut n = pl.notes.clone();
        n.sort();
        n.dedup();
        detail += &format!(" [{}]", n.join(" "));
    }
    row(verdict, reason, detail)
}

/// Compiles the recording pass's HLSL (cached beside our program).
fn compile_rec(src: &str, stage: rebuild_pipelines::Stage, out: &std::path::Path, hash: &str) -> Result<Vec<u8>, String> {
    let hl = out.join(format!("{hash}.rec.{}.hlsl", stage.name()));
    let obj = out.join(format!("{hash}.rec.{}.dxil", stage.name()));
    if std::fs::read_to_string(&hl).is_ok_and(|old| old == src) && obj.exists() {
        return std::fs::read(&obj).map_err(|e| e.to_string());
    }
    std::fs::write(&hl, src).map_err(|e| e.to_string())?;
    let dxc = super::dxc().ok_or("no dxc")?;
    rebuild_pipelines::translate::compile_file(&dxc, stage, &hl, &obj)?;
    std::fs::read(&obj).map_err(|e| e.to_string())
}
