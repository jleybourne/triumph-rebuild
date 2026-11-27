//! One program, ours and the oracle's, on the same inputs: the plan (user data, heap slots and their views),
//! the resources, the runs, and the word-by-word comparison.

use std::collections::BTreeMap;

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
const TABLE_BASE: u32 = 256;
const SAMPLER_TABLE_SLOTS: u32 = 64;
const SAMPLER_TABLE_BASE: u32 = 64;
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
fn texel(set: u32, salt: u64, i: u64) -> u32 {
    let r = word(1, salt ^ 0x5151, i ^ (set as u64) << 40);
    ((f32::from_bits(r) + 2.0) / 4.0).to_bits()
}

fn bytes_of(words: impl Iterator<Item = u32>) -> Vec<u8> {
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
}

pub fn plan(p: &Prog) -> Result<Plan, String> {
    let mut pl = Plan::default();
    // each table at its own place: resource tables 256 slots apart, sampler tables 64
    let mut table_base: BTreeMap<u8, (Heap, u32)> = BTreeMap::new();
    let (mut nres, mut nsmp) = (0u32, 0u32);
    for prm in &p.layout {
        if prm.user_data >= 16 {
            return Err(format!("user data dword {} past 16", prm.user_data));
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
        if u.shadow != 0 {
            pl.notes.push("shadow stride = access size");
            if let Some(off) = u.byte_off {
                let (_, base) = table_base[&u.user_data];
                let slot = ((base * 32) as i64 + off as i64) as u32 / 32;
                pl.strides.entry(slot).or_insert(u.shadow as u32);
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
fn merge(a: View, b: View) -> Result<View, String> {
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
        View::Raw | View::Typed => g.resource(buffer_desc(BUFFER_BYTES, false), srv, false, words(BUFFER_BYTES as usize / 4)),
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
    unsafe {
        match b.view {
            View::Cbv => {
                let d = D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: b.res.r.GetGPUVirtualAddress(), SizeInBytes: BUFFER_BYTES as u32 };
                g.device.CreateConstantBufferView(Some(&d), g.slot(slot));
            }
            View::Raw | View::Typed | View::Tex(_) => g.device.CreateShaderResourceView(&b.res.r, Some(&srv_desc(b.view)), g.slot(slot)),
            View::RwRaw | View::RwTyped | View::RwTex(_) => g.device.CreateUnorderedAccessView(&b.res.r, None, Some(&uav_desc(b.view)), g.slot(slot)),
            View::Sampler | View::SamplerCmp => {}
        }
    }
}

fn oracle_root(statics: &[[u32; 10]], ours: &RootDesc) -> RootDesc {
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
            vis: Visibility::All,
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
            return if self.changed == 0 { ("vacuous", "no output word written".into()) } else { ("match", String::new()) };
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

/// Runs a compute program and the oracle's on every input set.
pub fn program(g: &mut Gpu, p: &Prog, o: &Oracle) -> Row {
    let trace = std::env::var_os("REBUILD_EXEC_TRACE").is_some();
    let step = |s: &str| {
        if trace {
            eprintln!("execcheck {}: {s}", p.hash);
        }
    };
    let row = |verdict: &'static str, reason: String, detail: String| Row { hash: p.hash.clone(), stage: p.stage, single: p.single, verdict, reason, detail };
    if o.root_views > 0 {
        return row("untested", "the oracle binds root views".into(), String::new());
    }
    if p.stage != "cs" {
        return row("untested", "stage not run yet".into(), String::new());
    }
    let pl = match plan(p) {
        Ok(pl) => pl,
        Err(e) => return row("untested", e, String::new()),
    };
    step(&format!("plan {:?}", p.uses));
    let ours_layout = StageLayout { user_data: true, shadow: true, samplers: p.immediate.clone(), compare: p.compare };
    let ours_root = RootDesc::compute(&ours_layout, layout::USER_DATA_DWORDS);
    let oracle_root = oracle_root(&o.statics, &ours_root);
    let roots = [&ours_root, &oracle_root].map(|r| rebuild_pipelines::d3d::serialize(r).and_then(|b| g.root_signature(&b)));
    let (ours_rs, oracle_rs) = match roots {
        [Ok(a), Ok(b)] => (a, b),
        [Err(e), _] => return row("error", "our root signature refused".into(), e),
        [_, Err(e)] => return row("error", "the oracle's root signature refused".into(), e),
    };
    step("roots");
    let ours = match g.compute(&ours_rs, &p.dxil) {
        Ok(x) => x,
        Err(e) => return row("error", "our pipeline refused".into(), e),
    };
    let theirs = match g.compute(&oracle_rs, &o.dxil) {
        Ok(x) => x,
        Err(e) => return row("error", "the oracle's pipeline refused".into(), e),
    };
    step("pipelines");
    // the shadow table: raw buffers read by index hold records of the access's size (what the oracle assumes),
    // as many as fit
    let mut shadow = vec![0u8; super::gpu::HEAP_SLOTS as usize * 16];
    for (slot, v) in &pl.slots {
        if matches!(v, View::Raw | View::RwRaw) {
            let stride = pl.strides.get(slot).copied().unwrap_or(16);
            let at = *slot as usize * 16;
            shadow[at..at + 4].copy_from_slice(&stride.to_le_bytes());
            shadow[at + 4..at + 8].copy_from_slice(&((BUFFER_BYTES / stride as u64) as u32).to_le_bytes());
        }
    }
    let shadow = match g.upload_buffer(&shadow) {
        Ok(b) => b,
        Err(e) => return row("error", "shadow table".into(), e),
    };
    let t = p.threads;
    let groups = [(64 / t[0].max(1)).clamp(1, 4), (64 / t[1].max(1)).clamp(1, 4), 1];
    let mut diff = Diff::default();
    let mut per_set = Vec::new();
    for set in 0..SETS {
        // one resource per slot (a dynamic region shares one per view)
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
            match make(g, *v, set, *slot as u64) {
                Ok(res) => {
                    by_view.insert(*v, bound.len());
                    bound.push((vec![*slot], Bound { res, view: *v }));
                }
                Err(e) => return row("error", "input resource".into(), e),
            }
        }
    step("resources");
        g.clear_heaps();
        for (slots, b) in &bound {
            for s in slots {
                step(&format!("slot {s} {:?}", b.view));
                describe(g, *s, b);
            }
        }
        step("samplers");
        for (s, v) in &pl.sampler_slots {
            g.sampler(*s, *v == View::SamplerCmp);
        }
    step("descriptors");
        let mut outs: Vec<Vec<Vec<u8>>> = Vec::new();
        for (pso, rs) in [(&ours, &ours_rs), (&theirs, &oracle_rs)] {
            g.begin();
            for (_, b) in &bound {
                g.upload(&b.res);
            }
            unsafe {
                let l = &g.list;
                l.SetDescriptorHeaps(&[Some(g.heap.clone()), Some(g.samplers.clone())]);
                l.SetComputeRootSignature(rs);
                l.SetPipelineState(pso);
                l.SetComputeRoot32BitConstants(0, 16, pl.ud[set as usize].as_ptr() as *const _, 0);
                l.SetComputeRootShaderResourceView(1, shadow.GetGPUVirtualAddress());
                l.Dispatch(groups[0], groups[1], groups[2]);
            }
            for (_, b) in &bound {
                g.download(&b.res);
            }
            if let Err(e) = g.end() {
                return row("crash", "device lost".into(), e);
            }
            outs.push(bound.iter().map(|(_, b)| g.read(&b.res)).collect());
        }
    step("ran");
        let mut d = Diff::default();
        for (k, (_, b)) in bound.iter().enumerate() {
            if b.res.written {
                compare(&mut d, k, &b.res.init, &outs[0][k], &outs[1][k]);
            }
        }
        per_set.push(format!("set{set} {}/{}w {}d", d.changed, d.words, d.differ));
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
    let (verdict, reason) = diff.verdict();
    let mut detail = per_set.join(" ");
    if let Some((r, i, x, y)) = diff.first {
        detail += &format!(" first res{r} word{i} ours {x:#010x} oracle {y:#010x} max_ulp {}", diff.max_ulp);
    }
    if !pl.notes.is_empty() {
        let mut n = pl.notes.clone();
        n.dedup();
        detail += &format!(" [{}]", n.join(" "));
    }
    row(verdict, reason, detail)
}
