//! The console's tiled texture layouts, detiled on the card.
//!
//! A texture the title's CPU writes in a swizzled layout (`SW_4KB_S` and the other GFX10 modes without pipe or bank
//! XOR, `rebuild_xg`) cannot be copied into its host texture as it lies: a host copy takes rows. Every addressable
//! mode is a permutation of address bits in which x never shares a bit with y or z, so element (x, y, z) of a
//! subresource sits at `rows[z * height + y] + columns[x]` (`rebuild_xg::Layout::tables`). The pass here reads the
//! console bytes where they already are on the card (the ring's copy of watched memory, or an upload buffer), writes
//! linear rows into a staging buffer through those two tables, and the caller copies the rows into the texture by
//! footprint, as for a linear texture. Linear layouts a host copy cannot take as they lie (mip chains whose small mips
//! sit off the 512-byte grid, linear 3D) go the same way: their tables are rows of a pitch.
//!
//! ```text
//! root     b0 10 constants   t0 the texture's bytes (raw, from its byte 0)   t1 the tables (raw)   u0 staging (raw)
//! thread   one 4-byte word of one linear row: elements of 4, 8 or 16 bytes read one word each, of 1 or 2 bytes
//!          one word per element (four or two), masked and packed
//! ```
//!
//! **Cost.** The CPU builds the tables once per texture (two `u32` per column and row of each subresource: about
//! 0.1-5 % of the texture's bytes) and copies them with the batch. The card reads each texel once and writes it
//! twice (staging, then the copy).

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

/// One subresource's linear rows in staging, as a footprint copy takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    pub subresource: u32,
    /// from the texture's first byte in staging (a multiple of 512)
    pub offset: u64,
    /// in texels (whole blocks for block compression)
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// rows of elements, per depth slice
    pub rows: u32,
    /// bytes from one row of elements to the next (a multiple of 256)
    pub pitch: u32,
}

/// One subresource for the pass: where its tables are and where its rows go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    /// staging byte of row 0, from the texture's first byte in staging
    pub dst: u32,
    pub pitch: u32,
    /// elements per row
    pub width: u32,
    /// 4-byte words per row (the row's bytes rounded up to a word)
    pub words: u32,
    /// rows of every depth slice together
    pub rows: u32,
    /// word index of the column table and of the row table, from the texture's first table word
    pub columns: u32,
    pub row_table: u32,
    /// log2 of the element's bytes
    pub element_log2: u32,
}

/// A texture's detile: its tables (columns then rows, per subresource), the jobs that read them, the footprints the
/// copies take, and the staging bytes they cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub tables: Vec<u32>,
    pub jobs: Vec<Job>,
    pub footprints: Vec<Footprint>,
    pub staging: u64,
}

/// The plan for a texture of `layout` (one plane), subresource `mip + slice * mips` as the host numbers them. `None`
/// when the layout has no addressing here (pipe or bank XOR, multisampled) or an offset passes 4 GiB.
pub fn plan(layout: &rebuild_xg::Layout) -> Option<Plan> {
    let three_d = layout.desc.dimension == rebuild_xg::Dimension::Tex3d;
    let slices = if three_d { 1 } else { layout.slices };
    let mips = layout.mips.len() as u32;
    let eb = layout.desc.element_bytes;
    let block = layout.desc.block;
    let mut p = Plan { tables: Vec::new(), jobs: Vec::new(), footprints: Vec::new(), staging: 0 };
    for s in 0..slices {
        for m in 0..mips {
            let t = layout.tables(m, s).ok()?;
            let bytes = t.width.checked_mul(eb)?;
            let pitch = bytes.next_multiple_of(256);
            let rows = t.height.checked_mul(t.depth)?;
            let at = p.staging.next_multiple_of(512);
            let columns = u32::try_from(p.tables.len()).ok()?;
            p.tables.extend_from_slice(&t.columns);
            let row_table = u32::try_from(p.tables.len()).ok()?;
            for &r in &t.rows {
                p.tables.push(u32::try_from(r).ok()?);
            }
            p.jobs.push(Job { dst: u32::try_from(at).ok()?, pitch, width: t.width, words: bytes.div_ceil(4), rows, columns, row_table, element_log2: eb.trailing_zeros() });
            p.footprints.push(Footprint { subresource: m + s * mips, offset: at, width: t.width * block, height: t.height * block, depth: t.depth, rows: t.height, pitch });
            p.staging = at + u64::from(pitch) * u64::from(rows);
        }
    }
    Some(p)
}

/// The same rows on the CPU (the reference the pass is checked against, and the path a host without the pass
/// takes): `tiled` is the texture from its byte 0, `staging` receives `plan.staging` bytes.
pub fn reference(plan: &Plan, tiled: &[u8], staging: &mut [u8]) {
    for j in &plan.jobs {
        let eb = 1usize << j.element_log2;
        let columns = &plan.tables[j.columns as usize..][..j.width as usize];
        for r in 0..j.rows as usize {
            let base = plan.tables[j.row_table as usize + r] as usize;
            let row = j.dst as usize + r * j.pitch as usize;
            for (x, &c) in columns.iter().enumerate() {
                let from = base + c as usize;
                staging[row + x * eb..][..eb].copy_from_slice(&tiled[from..from + eb]);
            }
        }
    }
}

const HLSL: &str = r"
cbuffer Job : register(b0) {
    uint dst;
    uint pitch;
    uint width;
    uint words;
    uint rows;
    uint row0;
    uint columns;
    uint row_table;
    uint element_log2;
    uint pad;
};
ByteAddressBuffer Source : register(t0);
ByteAddressBuffer Tables : register(t1);
RWByteAddressBuffer Staging : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint w = id.x;
    uint r = id.y + row0;
    if (w >= words || r >= rows) {
        return;
    }
    uint base = Tables.Load((row_table + r) * 4);
    uint b = w * 4;
    uint v = 0;
    if (element_log2 >= 2) {
        uint x = b >> element_log2;
        v = Source.Load(base + Tables.Load((columns + x) * 4) + (b & ((1u << element_log2) - 1)));
    } else {
        uint bits = 8u << element_log2;
        uint mask = (1u << bits) - 1;
        uint first = b >> element_log2;
        uint per = 4u >> element_log2;
        for (uint i = 0; i < per; i++) {
            uint x = first + i;
            if (x < width) {
                uint a = base + Tables.Load((columns + x) * 4);
                uint e = (Source.Load(a & ~3u) >> ((a & 3u) * 8)) & mask;
                v |= e << (i * bits);
            }
        }
    }
    Staging.Store(dst + r * pitch + b, v);
}
";

/// The pass's root signature and pipeline (one per device).
pub struct Detiler {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Detiler {}
unsafe impl Sync for Detiler {}

/// Rows per dispatch (the group count's limit is 65,535).
const ROWS_PER_DISPATCH: u32 = 65_535;

impl Detiler {
    pub fn new(device: &ID3D12Device) -> Result<Detiler, String> {
        let code = compile()?;
        unsafe {
            let descriptor = |register| D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: register, RegisterSpace: 0 } };
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 10 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER { ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV, Anonymous: descriptor(0), ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL },
                D3D12_ROOT_PARAMETER { ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV, Anonymous: descriptor(1), ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL },
                D3D12_ROOT_PARAMETER { ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV, Anonymous: descriptor(0), ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL },
            ];
            let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: params.len() as u32, pParameters: params.as_ptr(), Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE, ..Default::default() };
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).map_err(|e| format!("root signature: {e}"))?;
            let blob = blob.ok_or("root signature")?;
            let root: ID3D12RootSignature =
                device.CreateRootSignature(0, std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())).map_err(|e| format!("root signature: {e}"))?;
            let pd = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.GetBufferPointer(), BytecodeLength: code.GetBufferSize() },
                ..Default::default()
            };
            let pso = device.CreateComputePipelineState(&pd);
            // the pipeline holds its own reference: drop the borrowed copy without a Release
            let _: std::mem::ManuallyDrop<Option<ID3D12RootSignature>> = std::mem::transmute_copy(&pd.pRootSignature);
            Ok(Detiler { root, pso: pso.map_err(|e| format!("pipeline: {e}"))? })
        }
    }

    /// Binds the pass (once per list, before [`Detiler::record`]).
    ///
    /// # Safety
    /// `list` is open.
    pub unsafe fn bind(&self, list: &ID3D12GraphicsCommandList) {
        unsafe {
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
        }
    }

    /// One texture's jobs: its bytes from GPU address `source` (its byte 0), its tables at `tables` (the plan's
    /// first table word), its rows into staging at `staging` (its first staging byte). Every address is a multiple
    /// of 4. Jobs write disjoint bytes: no barrier between them.
    ///
    /// # Safety
    /// `list` is open with the pass bound; the addresses hold what they name until the list has run.
    pub unsafe fn record(&self, list: &ID3D12GraphicsCommandList, plan: &Plan, source: u64, tables: u64, staging: u64) {
        unsafe {
            list.SetComputeRootShaderResourceView(1, source);
            list.SetComputeRootShaderResourceView(2, tables);
            list.SetComputeRootUnorderedAccessView(3, staging);
            for j in &plan.jobs {
                let mut row0 = 0;
                while row0 < j.rows {
                    let n = (j.rows - row0).min(ROWS_PER_DISPATCH);
                    let c = [j.dst, j.pitch, j.width, j.words, j.rows, row0, j.columns, j.row_table, j.element_log2, 0];
                    list.SetComputeRoot32BitConstants(0, c.len() as u32, c.as_ptr().cast(), 0);
                    list.Dispatch(j.words.div_ceil(64), n, 1);
                    row0 += n;
                }
            }
        }
    }
}

fn compile() -> Result<ID3DBlob, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // optimisation level 3
    let flags = 1 << 15;
    let r = unsafe { D3DCompile(HLSL.as_ptr().cast(), HLSL.len(), PCSTR::null(), None, None, PCSTR(c"main".as_ptr().cast()), PCSTR(c"cs_5_0".as_ptr().cast()), flags, 0, &mut code, Some(&mut errors)) };
    match (r, code) {
        (Ok(()), Some(c)) => Ok(c),
        (r, _) => {
            let msg = errors.map(|e| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize())).into_owned() });
            Err(format!("detile shader: {r:?} {}", msg.unwrap_or_default()))
        }
    }
}

/// The layout of a console texture description when it is one plane `rebuild_xg` can address. `Err(true)`: a layout
/// with no addressing here (pipe or bank XOR, multisampled, two planes), `Err(false)`: no layout at all.
pub fn layout_of(console: &D3D12_RESOURCE_DESC) -> Result<rebuild_xg::Layout, bool> {
    let c = rebuild_xg::Computer::new(&rebuild_d3d12x::format::xg_desc(console)).map_err(|_| false)?;
    if c.texture.planes.len() != 1 {
        return Err(true);
    }
    let l = c.texture.planes.into_iter().next().ok_or(false)?.layout;
    if !l.addressable() || l.desc.samples != 1 {
        return Err(true);
    }
    Ok(l)
}

/// The GPU address of a borrowed resource pointer.
pub fn gpu_address(r: *mut std::ffi::c_void) -> Option<u64> {
    // SAFETY: a live resource the caller borrowed
    unsafe { ID3D12Resource::from_raw_borrowed(&r) }.map(|r| unsafe { r.GetGPUVirtualAddress() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_xg::{Swizzle, Texture, TextureDesc};

    fn synthetic(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    /// The cases: every addressable mode the title's textures use (and the 256 B ones), each element size, BC, full
    /// chains with mip tails, arrays, 3D, and odd sizes.
    fn cases() -> Vec<(String, rebuild_xg::Layout)> {
        let modes = [Swizzle::LINEAR, Swizzle::SW_256B_S, Swizzle::SW_256B_D, Swizzle::SW_4KB_S, Swizzle::SW_4KB_D, Swizzle::SW_64KB_S, Swizzle::SW_64KB_D];
        // (name, format, width, height, depth or array, mips)
        let shapes: [(&str, u32, u32, u32, u32, u32); 9] = [
            ("R8 256x128", 61, 256, 128, 1, 1),
            ("R8 odd 37x19 chain", 61, 37, 19, 1, 0),
            ("R16 100x60 chain", 56, 100, 60, 1, 0),
            ("RGBA8 300x200 chain", 28, 300, 200, 1, 0),
            ("RGBA8 64x64 array 3", 28, 64, 64, 3, 0),
            ("RGBA16F 96x40", 10, 96, 40, 1, 2),
            ("RGBA32F 33x17", 2, 33, 17, 1, 1),
            ("BC1 256x256 chain", 71, 256, 256, 1, 0),
            ("BC7 520x260 chain", 98, 520, 260, 1, 0),
        ];
        let mut out = vec![];
        for m in modes {
            for (name, format, w, h, d, mips) in shapes {
                let t = TextureDesc { dimension: rebuild_xg::Dimension::Tex2d, width: w, height: h, depth_or_array: d, mips, format, samples: 1, swizzle: m };
                if let Ok(t) = Texture::new(&t) {
                    out.push((format!("{name} mode {}", m.0), t.planes[0].layout.clone()));
                }
            }
        }
        for m in [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_S] {
            let t = TextureDesc { dimension: rebuild_xg::Dimension::Tex3d, width: 40, height: 24, depth_or_array: 10, mips: 0, format: 28, samples: 1, swizzle: m };
            if let Ok(t) = Texture::new(&t) {
                out.push((format!("RGBA8 3D 40x24x10 chain mode {}", m.0), t.planes[0].layout.clone()));
            }
        }
        out
    }

    /// The reference rows are `rebuild_xg`'s own CPU detile (lane `xg`, checked against the fork), row for row.
    #[test]
    fn the_reference_rows_are_the_xg_detile() {
        let cases = cases();
        assert!(cases.len() >= 60, "{} cases", cases.len());
        for (i, (name, l)) in cases.iter().enumerate() {
            let p = plan(l).unwrap_or_else(|| panic!("{name}: a plan"));
            let tiled = synthetic(l.size as usize, i as u64);
            let mut staging = vec![0u8; p.staging as usize];
            reference(&p, &tiled, &mut staging);
            for (k, f) in p.footprints.iter().enumerate() {
                let mips = l.mips.len() as u32;
                let (mip, slice) = (f.subresource % mips, f.subresource / mips);
                let mut want = vec![0u8; (f.pitch * f.rows * f.depth) as usize];
                l.detile(&tiled, mip, slice, &mut want, f.pitch as usize, (f.pitch * f.rows) as usize).unwrap();
                let row = (p.jobs[k].width * l.desc.element_bytes) as usize;
                for r in 0..(f.rows * f.depth) as usize {
                    let at = f.offset as usize + r * f.pitch as usize;
                    assert_eq!(&staging[at..at + row], &want[r * f.pitch as usize..][..row], "{name}: subresource {} row {r}", f.subresource);
                }
            }
        }
    }

    #[test]
    fn plans_place_rows_where_a_footprint_copy_takes_them() {
        let t = TextureDesc { dimension: rebuild_xg::Dimension::Tex2d, width: 256, height: 128, depth_or_array: 1, mips: 0, format: 61, samples: 1, swizzle: Swizzle::SW_4KB_S };
        let l = Texture::new(&t).unwrap().planes[0].layout.clone();
        let p = plan(&l).unwrap();
        assert_eq!(p.footprints.len(), 9);
        assert_eq!(p.footprints[0], Footprint { subresource: 0, offset: 0, width: 256, height: 128, depth: 1, rows: 128, pitch: 256 });
        for f in &p.footprints {
            assert_eq!(f.offset % 512, 0);
            assert_eq!(f.pitch % 256, 0);
        }
        // a 1x1 mip: one element in a 256-byte row
        assert_eq!(p.footprints[8], Footprint { subresource: 8, offset: p.footprints[8].offset, width: 1, height: 1, depth: 1, rows: 1, pitch: 256 });
        // BC: footprints in whole blocks
        let t = TextureDesc { format: 98, width: 520, height: 260, ..t };
        let p = plan(&Texture::new(&t).unwrap().planes[0].layout).unwrap();
        assert_eq!((p.footprints[0].width, p.footprints[0].height, p.footprints[0].rows, p.footprints[0].pitch), (520, 260, 65, 2304), "130 blocks of 16 bytes, rows of 2,080 padded to 2,304");
    }

    /// The pass on WARP, word for word against the CPU reference, for every case: the bytes from a buffer, the rows
    /// read back.
    #[test]
    fn the_pass_matches_the_reference_on_warp() {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory4};
        use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory4>() }) else { return };
        let Ok(adapter) = (unsafe { factory.EnumWarpAdapter::<windows::core::IUnknown>() }) else { return };
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
            return;
        }
        let device = device.unwrap();
        let d = Detiler::new(&device).expect("the pass compiles");
        let cases = cases();
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            list.Close().unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            let buffer = |bytes: u64, kind: D3D12_HEAP_TYPE, uav: bool, state: D3D12_RESOURCE_STATES| {
                let mut r: Option<ID3D12Resource> = None;
                device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &rebuild_d3d12x::resource::buffer_desc(bytes, uav), state, None, &mut r).unwrap();
                r.unwrap()
            };
            let map = |r: &ID3D12Resource| {
                let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
                r.Map(0, None, Some(&mut p)).unwrap();
                p as *mut u8
            };
            let mut words = 0u64;
            for (i, (name, l)) in cases.iter().enumerate() {
                let p = plan(l).unwrap();
                let tiled = synthetic(l.size as usize, 1000 + i as u64);
                let mut want = vec![0u8; p.staging as usize];
                reference(&p, &tiled, &mut want);
                // source and tables in one upload buffer (tables after the bytes, 256-aligned)
                let tables_at = l.size.next_multiple_of(256);
                let up = buffer(tables_at + p.tables.len() as u64 * 4, D3D12_HEAP_TYPE_UPLOAD, false, D3D12_RESOURCE_STATE_GENERIC_READ);
                let u = map(&up);
                std::ptr::copy_nonoverlapping(tiled.as_ptr(), u, tiled.len());
                std::ptr::copy_nonoverlapping(p.tables.as_ptr().cast::<u8>(), u.add(tables_at as usize), p.tables.len() * 4);
                let staging = buffer(p.staging, D3D12_HEAP_TYPE_DEFAULT, true, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
                let back = buffer(p.staging, D3D12_HEAP_TYPE_READBACK, false, D3D12_RESOURCE_STATE_COPY_DEST);
                allocator.Reset().unwrap();
                list.Reset(&allocator, None).unwrap();
                d.bind(&list);
                let va = up.GetGPUVirtualAddress();
                d.record(&list, &p, va, va + tables_at, staging.GetGPUVirtualAddress());
                list.ResourceBarrier(&[transition(&staging, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.CopyBufferRegion(&back, 0, &staging, 0, p.staging);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
                let v = i as u64 + 1;
                queue.Signal(&fence, v).unwrap();
                fence.SetEventOnCompletion(v, event).unwrap();
                assert_eq!(WaitForSingleObject(event, 20_000).0, 0, "{name}: WARP finished");
                let got = std::slice::from_raw_parts(map(&back), p.staging as usize);
                // only the rows' bytes are defined (row padding is left as staging held it)
                for (k, f) in p.footprints.iter().enumerate() {
                    let row = (p.jobs[k].width << p.jobs[k].element_log2) as usize;
                    for r in 0..(f.rows * f.depth) as usize {
                        let at = f.offset as usize + r * f.pitch as usize;
                        assert_eq!(&got[at..at + row], &want[at..at + row], "{name}: subresource {} row {r}", f.subresource);
                    }
                    words += u64::from(p.jobs[k].words * p.jobs[k].rows);
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(event);
            assert!(words > 1_000_000, "{words} words checked");
            eprintln!("detile pass on WARP: {} cases, {words} words equal to the reference", cases.len());
        }
    }

    fn transition(r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: borrowed for the call
                    pResource: unsafe { std::mem::transmute_copy(r) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: from,
                    StateAfter: to,
                }),
            },
        }
    }
}
