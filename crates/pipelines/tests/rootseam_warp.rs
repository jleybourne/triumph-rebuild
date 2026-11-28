//! The root seam on WARP: translated compute programs from the program library, dispatched through a console
//! list (`rebuild_xlist`) with the title's kind of root arguments (its own parameter numbers, read from the
//! console program's root layout), write the words the console would.
//!
//! By hand, with a copy of the title's shaders (never the game folder) and the library built from it:
//!
//! ```text
//! set REBUILD_SHADERS=D:\victory-data\shaders.rpf
//! set REBUILD_PIPELINES_LIB=D:\victory-data\pipelines\library.bin
//! cargo test -p rebuild-pipelines --release --test rootseam_warp -- --nocapture
//! ```
//!
//! Without both variables it says so and passes (nothing to run against).
#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use rebuild_pipelines::d3d::Gpu;
use rebuild_pipelines::library::Library;
use rebuild_pipelines::pso::Programs;
use rebuild_pipelines::{rootmap, Stage};
use rebuild_shader::census;
use rebuild_shader::native::NativeProgram;
use rebuild_xlist::console::{self, HeapBinding, HostPipeline, ListHost, MemoryRange, PipelineSource, Raw, UserMap, UserSource};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Console handle of the title's view heap (its own memory on the console: any address works here).
const CONSOLE_HEAP: u64 = 0x5_0000_0000;
const HEAP_TOKEN: Raw = 0xD000 as Raw;
const PIPELINE_TOKEN: Raw = 0x6000 as Raw;
const DESCRIPTORS: u32 = 64;
/// The title's table starts at console descriptor 3 of its heap.
const TABLE: u32 = 3;

struct WarpHost {
    pso: usize,
    root: usize,
    user: &'static UserMap,
    heap: HeapBinding,
}
unsafe impl Send for WarpHost {}
unsafe impl Sync for WarpHost {}

impl PipelineSource for WarpHost {
    fn pipeline(&self, console: Raw) -> Option<HostPipeline> {
        (console == PIPELINE_TOKEN).then_some(HostPipeline { pso: self.pso as Raw, root_signature: self.root as Raw, compute: true, user: Some(self.user), topology: 0, rects: false })
    }
}

impl ListHost for WarpHost {
    fn resource(&self, _: Raw) -> Option<Raw> {
        None
    }
    fn descriptor_heap(&self, console: Raw) -> Option<HeapBinding> {
        (console == HEAP_TOKEN).then_some(self.heap)
    }
    fn cpu_descriptor(&self, _: usize) -> Option<usize> {
        None
    }
    fn memory(&self, _: u64) -> Option<MemoryRange> {
        None
    }
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }.expect("buffer");
    r.expect("buffer")
}

/// Submit `list` and wait for it, at most 5 s.
fn run(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, value: u64, list: &ID3D12GraphicsCommandList) {
    unsafe {
        queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
        queue.Signal(fence, value).unwrap();
        let t = Instant::now();
        while fence.GetCompletedValue() < value {
            assert!(t.elapsed() < Duration::from_secs(5), "the GPU did not finish in 5 s");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The console program `hash` names (or the one its container is a duplicate of).
fn native<'a>(walk: &'a census::Walk, hash: &str) -> &'a NativeProgram {
    let code = walk.aliases.iter().find(|a| a.0 == hash).map(|a| a.1.as_str());
    &walk.programs.iter().find(|p| p.hash == hash || Some(p.code_hash.as_str()) == code).unwrap_or_else(|| panic!("{hash} is not in the shaders")).native
}

/// What one dispatch wrote: the program `hash` from the library, its table's descriptor `TABLE + view` a raw
/// UAV of a 4 KiB buffer, `shadow` the shadow-table entry of that descriptor, the title's root arguments set by
/// the parameter numbers its root layout names, `constant` in every root constant the program reads.
fn dispatch(gpu: &Gpu, programs: &Programs, walk: &census::Walk, hash: &str, view: u32, shadow: [u32; 4], constant: u32, groups: u32) -> Vec<u32> {
    let device = &gpu.device;
    let h = rebuild_pipelines::parse_hash(hash).unwrap();
    let map = rootmap::compute(native(walk, hash)).expect("the program's root layout");
    let user: &'static UserMap = Box::leak(Box::new(map));
    let (code, root) = programs.compute(h).expect("in the program library");
    let pso = gpu.compute(&root, &code).expect("the host takes the pipeline");
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        // the host heap: console descriptor i at host index i, a raw UAV of the output at the table's view
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: DESCRIPTORS, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
            .unwrap();
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        let out = buffer(device, D3D12_HEAP_TYPE_DEFAULT, 4096, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Buffer: D3D12_BUFFER_UAV { FirstElement: 0, NumElements: 1024, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW } },
        };
        let mut cpu = heap.GetCPUDescriptorHandleForHeapStart();
        cpu.ptr += ((TABLE + view) * step) as usize;
        device.CreateUnorderedAccessView(&out, None, Some(&uav), cpu);
        // the shadow table: 16 bytes per console descriptor
        let table = buffer(device, D3D12_HEAP_TYPE_UPLOAD, u64::from(DESCRIPTORS) * 16, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ);
        let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
        table.Map(0, None, Some(&mut p)).unwrap();
        let words = std::slice::from_raw_parts_mut(p as *mut u32, DESCRIPTORS as usize * 4);
        words.fill(0);
        words[((TABLE + view) * 4) as usize..][..4].copy_from_slice(&shadow);
        table.Unmap(0, None);

        let host = Arc::new(WarpHost {
            pso: pso.as_raw() as usize,
            root: root.as_raw() as usize,
            user,
            heap: HeapBinding {
                heap: heap.as_raw(),
                console_base: CONSOLE_HEAP,
                console_bytes: u64::from(DESCRIPTORS) * 32,
                console_stride: 32,
                host_base: heap.GetGPUDescriptorHandleForHeapStart().ptr,
                host_stride: step,
                shadow_va: table.GetGPUVirtualAddress(),
                pages: None,
            },
        });
        let a = console::create_allocator(0, device, host).unwrap();
        let l = console::create_list(0, a, std::ptr::null_mut()).unwrap();
        let method = |offset: usize| -> *const () { *(*(l as *const *const *const ())).add(offset / 8) };
        type Heaps = unsafe extern "system" fn(Raw, u32, *const Raw);
        type SetRaw = unsafe extern "system" fn(Raw, Raw);
        type Table = unsafe extern "system" fn(Raw, u32, u64);
        type Constant = unsafe extern "system" fn(Raw, u32, u32, u32);
        type Dispatch = unsafe extern "system" fn(Raw, u32, u32, u32);
        type Close = unsafe extern "system" fn(Raw) -> windows::core::HRESULT;
        std::mem::transmute::<_, Heaps>(method(0x100))(l, 1, &HEAP_TOKEN);
        std::mem::transmute::<_, SetRaw>(method(0xc8))(l, PIPELINE_TOKEN);
        // the title's arguments, by its parameter numbers: the table at its first descriptor, the constants
        let mut set = Vec::new();
        for src in &user.stages[0].src[..user.stages[0].dwords as usize] {
            match *src {
                UserSource::Table { slot, .. } => {
                    std::mem::transmute::<_, Table>(method(0x118))(l, u32::from(slot), CONSOLE_HEAP + u64::from(TABLE) * 32);
                    set.push(format!("table at {slot}"));
                }
                UserSource::Constant { slot, dword } => {
                    std::mem::transmute::<_, Constant>(method(0x128))(l, u32::from(slot), constant, u32::from(dword));
                    set.push(format!("constant {dword} of {slot}"));
                }
                UserSource::Zero => {}
                s => panic!("{hash}: {s:?} not exercised here"),
            }
        }
        std::mem::transmute::<_, Dispatch>(method(0x70))(l, groups, 1, 1);
        assert_eq!(std::mem::transmute::<_, Close>(method(0x48))(l).0, 0);
        let st = console::stats(l).unwrap();
        println!("{hash}: title arguments {}; {} user-data write, {} shadow bind", set.join(", "), st.user_sets, st.shadow_sets);
        assert_eq!((st.user_sets, st.shadow_sets, st.dispatches, st.refused, st.user_unset), (1, 1, 1, 0, 0), "{st:?}");
        run(&queue, &fence, 1, console::host_list(l).expect("one host list"));

        // read the output back
        let readback = buffer(device, D3D12_HEAP_TYPE_READBACK, 4096, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST);
        let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let copy: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
        copy.CopyBufferRegion(&readback, 0, &out, 0, 4096);
        copy.Close().unwrap();
        run(&queue, &fence, 2, &copy);
        readback.Map(0, None, Some(&mut p)).unwrap();
        let got = std::slice::from_raw_parts(p as *const u32, 1024).to_vec();
        readback.Unmap(0, None);
        let _ = (console_release(l), console_release(a));
        got
    }
}

fn console_release(obj: Raw) -> u32 {
    unsafe {
        let f: unsafe extern "system" fn(Raw) -> u32 = std::mem::transmute(*(*(obj as *const *const *const ())).add(2));
        f(obj)
    }
}

#[test]
fn translated_programs_read_the_titles_root_arguments_on_warp() {
    let (Some(shaders), Some(lib)) = (std::env::var_os("REBUILD_SHADERS"), std::env::var_os("REBUILD_PIPELINES_LIB")) else {
        println!("skipped: set REBUILD_SHADERS (a copy of shaders.rpf) and REBUILD_PIPELINES_LIB (its program library)");
        return;
    };
    let t = Instant::now();
    let data = std::fs::read(&shaders).expect("read the shaders.rpf copy");
    let walk = census::walk(&data, 4);
    let (lib, _) = Library::load(std::path::Path::new(&lib), 4, true).expect("the program library");
    println!("{} programs read, library loaded: {:.1} s", walk.programs.len(), t.elapsed().as_secs_f64());
    let gpu = Gpu::warp().expect("WARP");
    let programs = Programs::new(&gpu.device, lib, None);

    // 9146291b38ea9ce1: s4 is a table (user data 4), s5 a constant (5): thread i of group g stores
    // (0, 1, 1, 0) at (64 g + i) x constant through the table's first descriptor
    for p in ["9146291b38ea9ce1"] {
        assert!(programs.library().get(rebuild_pipelines::parse_hash(p).unwrap(), Stage::Cs).is_some());
    }
    let got = dispatch(&gpu, &programs, &walk, "9146291b38ea9ce1", 0, [0; 4], 32, 1);
    for i in 0..64 {
        assert_eq!(&got[i * 8..i * 8 + 8], &[0, 1, 1, 0, 0, 0, 0, 0], "thread {i}: stride 32 from the constant, the table's descriptor");
    }
    assert!(got[64 * 8..].iter().all(|&w| w == 0));

    // e4918eaa6752a8a0: the table's third descriptor (+64 bytes), indexed (idxen): thread v stores 0x7f7fffff at
    // v x stride while v < records, both from the descriptor shadow table at t0 space101
    let got = dispatch(&gpu, &programs, &walk, "e4918eaa6752a8a0", 2, [8, 20, 0, 0], 7, 1);
    for v in 0..32 {
        let want = if v < 20 { 0x7f7f_ffff } else { 0 };
        assert_eq!((got[v * 2], got[v * 2 + 1]), (want, 0), "thread {v}: stride 8 and 20 records from the shadow table");
    }
    println!("both programs wrote what the console would: {:.1} s", t.elapsed().as_secs_f64());
}
