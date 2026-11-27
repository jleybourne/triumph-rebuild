//! Wave-wide programs on a GPU: random GFX10 compute programs (`rebuild_sir::lift::wavegen`) lifted with the wave
//! mapping their console wave asks for (`rebuild_sir::lift::Wave`), compiled by DXC, run, and every lane of every
//! output register compared with the reference console (`wavegen::run`), bit for bit.
//!
//! cargo run --release --bin wavetest [-- --programs N] [--card]
//!
//! WARP (the software rasterizer) by default. `--card` runs on the card, only while `agents\MEASURING` is
//! absent and under the lanes' lock (`agents\CARD`); every fence wait is bounded at 5 s. The generated programs
//! have no loops, so none can run away.

use std::path::Path;

use rebuild_pipelines::d3d::{CardLock, Gpu};
use rebuild_pipelines::translate::{compile_file, dxc_path};
use rebuild_pipelines::Stage as HostStage;
use rebuild_shader::gfx10;
use rebuild_shader::native::ComputeIo;
use rebuild_sir::gen::Rng;
use rebuild_sir::lift::wavegen::{self, WaveState, SDATA, VGPRS};
use rebuild_sir::lift::{self, Stage, Wave};

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Byte offsets in the one buffer: SGPRs in, VGPRs in (8 per console lane), VGPRs out, SGPRs out (16 per wave).
const SIN: u32 = 0;
const VIN: u32 = 512;
const VOUT: u32 = VIN + 4 * 8 * 256;
const SOUT: u32 = VOUT + 4 * 8 * 256;
const BYTES: u64 = (SOUT + 4 * 16 * 8) as u64;

struct Dev {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    root: ID3D12RootSignature,
    buf: ID3D12Resource,
    up: ID3D12Resource,
    back: ID3D12Resource,
    timer: ID3D12QueryHeap,
    stamps: ID3D12Resource,
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64, uav: bool, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }
        .expect("buffer");
    r.unwrap()
}

impl Dev {
    fn new(device: ID3D12Device) -> Dev {
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let params = [D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            }];
            let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 1, pParameters: params.as_ptr(), ..Default::default() };
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).expect("signature");
            let b = blob.unwrap();
            let root: ID3D12RootSignature = device.CreateRootSignature(0, std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).unwrap();
            let buf = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, BYTES, true, D3D12_RESOURCE_STATE_COMMON);
            let up = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, BYTES, false, D3D12_RESOURCE_STATE_GENERIC_READ);
            let back = buffer(&device, D3D12_HEAP_TYPE_READBACK, BYTES, false, D3D12_RESOURCE_STATE_COPY_DEST);
            let mut timer: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2, NodeMask: 0 }, &mut timer).unwrap();
            let stamps = buffer(&device, D3D12_HEAP_TYPE_READBACK, 16, false, D3D12_RESOURCE_STATE_COPY_DEST);
            Dev { device, queue, alloc, list, fence, value: 0, root, buf, up, back, timer: timer.unwrap(), stamps }
        }
    }

    /// Records, submits and waits at most 5 s.
    fn submit(&mut self, f: impl FnOnce(&ID3D12GraphicsCommandList)) -> Result<(), String> {
        unsafe {
            self.alloc.Reset().unwrap();
            self.list.Reset(&self.alloc, None).unwrap();
            f(&self.list);
            self.list.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).unwrap();
            let ev = CreateEventW(None, false, false, None).unwrap();
            self.fence.SetEventOnCompletion(self.value, ev).unwrap();
            let r = WaitForSingleObject(ev, 5000);
            let _ = CloseHandle(ev);
            if r != WAIT_OBJECT_0 {
                return Err("a fence wait passed 5 s".into());
            }
            self.device.GetDeviceRemovedReason().map_err(|e| format!("device removed: {e}"))
        }
    }

    /// Uploads `input`, dispatches `groups` groups `reps` times, reads the buffer back; also the GPU time of the
    /// dispatches in microseconds.
    fn run(&mut self, pso: &ID3D12PipelineState, input: &[u32], groups: u32, reps: u32) -> Result<(Vec<u32>, f64), String> {
        unsafe {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            self.up.Map(0, None, Some(&mut p)).unwrap();
            std::ptr::copy_nonoverlapping(input.as_ptr(), p as *mut u32, input.len());
            self.up.Unmap(0, None);
            let (buf, up, back, root, timer, stamps) = (self.buf.clone(), self.up.clone(), self.back.clone(), self.root.clone(), self.timer.clone(), self.stamps.clone());
            // the buffer lives in COMMON between lists: COPY_DEST for the upload, UNORDERED_ACCESS for the dispatch,
            // COPY_SOURCE for the read back (WARP forgives a missing transition; the card does not)
            let barrier = |l: &ID3D12GraphicsCommandList, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES| {
                let b = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: std::mem::transmute_copy(&buf),
                            Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                            StateBefore: from,
                            StateAfter: to,
                        }),
                    },
                    ..Default::default()
                };
                l.ResourceBarrier(&[b]);
            };
            self.submit(|l| {
                barrier(l, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
                l.CopyBufferRegion(&buf, 0, &up, 0, BYTES);
                barrier(l, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
                l.SetPipelineState(pso);
                l.SetComputeRootSignature(&root);
                l.SetComputeRootUnorderedAccessView(0, buf.GetGPUVirtualAddress());
                l.EndQuery(&timer, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                for _ in 0..reps {
                    l.Dispatch(groups, 1, 1);
                }
                l.EndQuery(&timer, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                l.ResolveQueryData(&timer, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &stamps, 0);
                barrier(l, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
                l.CopyBufferRegion(&back, 0, &buf, 0, BYTES);
                barrier(l, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
            })?;
            let freq = self.queue.GetTimestampFrequency().unwrap_or(1) as f64;
            self.stamps.Map(0, None, Some(&mut p)).unwrap();
            let t = std::slice::from_raw_parts(p as *const u64, 2);
            let us = (t[1].saturating_sub(t[0])) as f64 / freq * 1e6;
            self.stamps.Unmap(0, None);
            self.back.Map(0, None, Some(&mut p)).unwrap();
            let out = std::slice::from_raw_parts(p as *const u32, BYTES as usize / 4).to_vec();
            self.back.Unmap(0, None);
            Ok((out, us))
        }
    }

    fn pipeline(&self, dxil: &[u8]) -> Result<ID3D12PipelineState, String> {
        unsafe {
            self.device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&self.root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr().cast(), BytecodeLength: dxil.len() },
                    ..Default::default()
                })
                .map_err(|e| format!("pipeline: {e}"))
        }
    }
}

/// The test's compute program around a lifted body: registers in from the buffer, the body, registers out.
fn harness(l: &lift::Lifted, n: u32, wave: Wave, emulate: bool) -> String {
    let io = ComputeIo { rsrc2: 0, threads: [n, 1, 1] };
    let mut s = String::from(lift::PRELUDE);
    s.push_str(lift::wave::PRELUDE);
    s.push_str("RWByteAddressBuffer io : register(u0);\n");
    let (attr, ids) = lift::wave::cs_entry(l, &io).expect("a wave mapping");
    // Partial64 runs one host wave of 32 as a console wave64 whose lanes 32..63 were never launched
    s.push_str(if wave == Wave::Partial64 { "[WaveSize(32)]\n[numthreads(32, 1, 1)]\n" } else { &attr });
    s.push_str("void main(uint3 tid : SV_GroupThreadID, uint3 gid : SV_GroupID) {\n");
    s.push_str(&lift::declarations(l));
    for r in &l.sgprs {
        s.push_str(&format!("    s{r} = io.Load({});\n", SIN + 4 * u32::from(*r)));
    }
    s.push_str(&ids);
    let (halves, lanes): (Vec<(&str, String)>, u32) = match wave {
        Wave::Doubled => (vec![("_0", "sir_f_0".into()), ("_1", "sir_f_1".into())], 64),
        Wave::Partial64 => (vec![("", "tid.x".into())], 64),
        _ => (vec![("", "tid.x".into())], 32),
    };
    for (h, f) in &halves {
        for r in l.vgprs.iter().filter(|r| **r != 0 && (**r as usize) < VGPRS) {
            s.push_str(&format!("    v{r}{h} = io.Load({VIN} + 4 * ({f} * 8 + {r}));\n"));
        }
    }
    s.push_str(&l.body);
    for (h, f) in &halves {
        s.push_str(&format!("    if ({f} < {n}u) {{\n"));
        for r in l.vgprs.iter().filter(|r| (**r as usize) < VGPRS) {
            s.push_str(&format!("        io.Store({VOUT} + 4 * ({f} * 8 + {r}), v{r}{h});\n"));
        }
        for r in l.sgprs.iter().filter(|r| SDATA.contains(&(**r as usize))) {
            s.push_str(&format!("        io.Store({SOUT} + 4 * (({f} / {lanes}u) * 16 + {}), s{r});\n", r - 8));
        }
        s.push_str("    }\n");
    }
    s.push_str("}\n");
    if emulate {
        s = emulated(&s);
    }
    s
}

/// WARP runs waves of 4 lanes, so a `[WaveSize(32)]` program cannot be created there. For WARP the wave
/// intrinsics become a 32-lane wave in groupshared memory (one host wave per group, every call in uniform
/// control flow, as the lifter promises): this checks the lifter's lane logic; `--card` checks the intrinsics.
fn emulated(src: &str) -> String {
    let mut s = src
        .replace("[WaveSize(32)]\n", "")
        .replace("WaveGetLaneIndex()", "emu_lane")
        .replace("WaveGetLaneCount()", "32u")
        .replace("WaveReadLaneAt(", "emu_read(")
        .replace("WaveReadLaneFirst(", "emu_first(")
        .replace("WaveActiveBallot(", "emu_ballot(")
        .replace("WaveActiveAnyTrue(", "emu_any(")
        .replace("QuadReadLaneAt(", "emu_quad(");
    let at = s.find("void main(").expect("main");
    let body = s[at..].find('{').unwrap() + at + 1;
    s.insert_str(body, "\n    emu_lane = tid.x & 31u;");
    let emu = r#"
groupshared uint emu_x[32];
static uint emu_lane;
uint emu_read(uint x, uint k) { GroupMemoryBarrierWithGroupSync(); emu_x[emu_lane] = x; GroupMemoryBarrierWithGroupSync(); return emu_x[k & 31u]; }
uint emu_first(uint x) { return emu_read(x, 0u); }
uint4 emu_ballot(bool b) {
    GroupMemoryBarrierWithGroupSync(); emu_x[emu_lane] = b ? 1u : 0u; GroupMemoryBarrierWithGroupSync();
    uint m = 0u; for (uint k = 0u; k < 32u; k++) { m |= emu_x[k] << k; } return uint4(m, 0u, 0u, 0u);
}
bool emu_any(bool b) { return emu_ballot(b).x != 0u; }
uint emu_quad(uint x, uint k) { return emu_read(x, (emu_lane & ~3u) | k); }
"#;
    s.insert_str(0, emu);
    s
}

fn compile(dxc: &Path, dir: &Path, name: &str, src: &str) -> Result<Vec<u8>, String> {
    let hl = dir.join(format!("{name}.hlsl"));
    let out = dir.join(format!("{name}.dxil"));
    std::fs::write(&hl, src).map_err(|e| e.to_string())?;
    compile_file(dxc, HostStage::Cs, &hl, &out)?;
    std::fs::read(&out).map_err(|e| e.to_string())
}

/// One random program: generated, lifted, run on the GPU and on the reference. `Err`: what differed (for a program
/// without branches, the shortest failing prefix is named too).
#[allow(clippy::too_many_arguments)]
fn check(dev: &mut Dev, dxc: &Path, dir: &Path, rng: &mut Rng, k: usize, wave: Wave, n: u32, emulate: bool, len: usize) -> Result<(), String> {
    let wave64 = wave != Wave::Native32;
    let words = wavegen::generate(rng, wave64, len, wave == Wave::Partial64);
    let mut input = vec![0u32; BYTES as usize / 4];
    for x in input.iter_mut().take((VOUT / 4) as usize) {
        *x = rng.next() as u32;
    }
    // small values now and then, so compares and lane selects meet
    for x in input.iter_mut().take((VOUT / 4) as usize).step_by(3) {
        *x &= 0x3F;
    }
    if wave == Wave::Partial64 {
        // masks made from data have no bits for lanes that never exist
        for r in [21, 23] {
            input[(SIN / 4) as usize + r] = 0;
        }
    }
    let r = run_words(dev, dxc, dir, k, wave, n, emulate, &words, &input);
    let Err(e) = r else { return r };
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let insts = gfx10::decode(&bytes).map_err(|e| format!("decode: {e}"))?;
    if e.starts_with("program") && !insts.iter().any(|i| i.name().is_some_and(|n| n.starts_with("s_cbranch"))) {
        for cut in 1..insts.len() {
            let mut w: Vec<u32> = insts[..cut].iter().flat_map(|i| i.words.clone()).collect();
            w.extend(insts.last().unwrap().words.clone());
            if run_words(dev, dxc, dir, 100_000 + k, wave, n, emulate, &w, &input).is_err() {
                return Err(format!("{e}\n  shortest failing prefix: {cut} instructions (p{})", 100_000 + k));
            }
        }
    }
    Err(e)
}

#[allow(clippy::too_many_arguments)]
fn run_words(dev: &mut Dev, dxc: &Path, dir: &Path, k: usize, wave: Wave, n: u32, emulate: bool, words: &[u32], input: &[u32]) -> Result<(), String> {
    let wave64 = wave != Wave::Native32;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let insts = gfx10::decode(&bytes).map_err(|e| format!("decode: {e}"))?;
    // the program as named fields, beside its HLSL (for a failure)
    let mut asm = String::new();
    for i in &insts {
        let d = gfx10::enc::decode(i);
        let fields: Vec<String> = d.layout.fields.iter().zip(&d.values).filter(|(f, _)| f.name != "op").map(|(f, v)| format!("{}={v}", f.name)).collect();
        let extra = d.extra.as_ref().map(|(l, v)| l.fields.iter().zip(v).map(|(f, v)| format!("{}={v:#x}", f.name)).collect::<Vec<_>>().join(" ")).unwrap_or_default();
        asm.push_str(&format!("{:04x} {} {} {extra}\n", i.offset, i.name().unwrap_or("?"), fields.join(" ")));
    }
    let _ = std::fs::write(dir.join(format!("p{k}.asm")), asm);
    let io = ComputeIo { rsrc2: 0, threads: [n, 1, 1] };
    let lf = lift::lift_program_wave(&insts, Stage::Cs(io), None, wave64, wave);
    if !lf.unsupported.is_empty() {
        return Err(format!("not lifted (p{k}, {}): {:?}", wave.name(), lf.unsupported));
    }
    let src = harness(&lf, n, wave, emulate);
    let dxil = compile(dxc, dir, &format!("p{k}"), &src).map_err(|e| format!("dxc: {e}"))?;
    let pso = dev.pipeline(&dxil)?;
    let (out, _) = dev.run(&pso, input, 1, 1)?;
    // the reference, wave by wave
    let lanes = if wave64 { 64 } else { 32 };
    let launched = if wave == Wave::Partial64 { 32 } else { n as usize };
    for w in 0..launched.div_ceil(lanes) {
        let mut exec = 0u64;
        let mut st = WaveState::new(lanes, 0);
        for l in 0..lanes {
            let f = w * lanes + l;
            if f < launched {
                exec |= 1 << l;
            }
            // a doubled group also carries the lanes past the group's end (EXEC off, registers from the buffer);
            // a partly filled graphics wave has none (they read 0)
            if f < launched || wave == Wave::Doubled {
                st.v[0][l] = f as u32 % n;
                for r in 1..VGPRS {
                    st.v[r][l] = input[(VIN / 4) as usize + f * 8 + r];
                }
            }
        }
        st.exec = exec;
        for r in 0..32 {
            st.s[r] = input[(SIN / 4) as usize + r];
        }
        wavegen::run(&insts, &mut st);
        for l in 0..lanes {
            let f = w * lanes + l;
            if f >= launched {
                continue;
            }
            for r in 0..VGPRS as u16 {
                if !lf.vgprs.contains(&r) {
                    continue;
                }
                let got = out[(VOUT / 4) as usize + f * 8 + r as usize];
                if got != st.v[r as usize][l] {
                    return Err(format!("program {k} ({}, {n} threads): lane {f} v{r} = {got:#x}, console {:#x}\n{}", wave.name(), st.v[r as usize][l], dir.join(format!("p{k}.hlsl")).display()));
                }
            }
        }
        for r in SDATA {
            if lf.sgprs.contains(&(r as u8)) {
                let got = out[(SOUT / 4) as usize + w * 16 + r - 8];
                if got != st.s[r] {
                    return Err(format!("program {k} ({}, {n} threads): wave {w} s{r} = {got:#x}, console {:#x}\n{}", wave.name(), st.s[r], dir.join(format!("p{k}.hlsl")).display()));
                }
            }
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let programs: usize = args.iter().position(|a| a == "--programs").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(300);
    let card = args.iter().any(|a| a == "--card");
    let len: usize = args.iter().position(|a| a == "--len").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(40);
    let _lock = if card {
        match CardLock::take() {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("wavetest: not on the card: {e}");
                std::process::exit(3);
            }
        }
    } else {
        None
    };
    let gpu = if card { Gpu::new(false) } else { Gpu::warp() }.unwrap_or_else(|e| panic!("device: {e}"));
    let mut dev = Dev::new(gpu.device.clone());
    let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    unsafe {
        let _ = dev.device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, &mut o1 as *mut _ as *mut _, std::mem::size_of_val(&o1) as u32);
    }
    println!("wave lanes {}..{} ({})", o1.WaveLaneCountMin, o1.WaveLaneCountMax, gpu.adapter);
    let dxc = dxc_path().expect("dxc.exe");
    let dir = std::env::temp_dir().join("rebuild-wavetest");
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng(0x5EED_64);
    // on WARP one host wave per group (the emulated wave is the group)
    let cases: [(Wave, &[u32]); 3] = if card {
        [(Wave::Doubled, &[64, 128, 96, 40]), (Wave::Native32, &[32, 64]), (Wave::Partial64, &[32])]
    } else {
        [(Wave::Doubled, &[64, 40]), (Wave::Native32, &[32]), (Wave::Partial64, &[32])]
    };
    let mut ran = std::collections::BTreeMap::new();
    let mut failed = 0;
    for k in 0..programs {
        let (wave, ns) = cases[k % 3];
        let n = ns[(k / 3) % ns.len()];
        match check(&mut dev, &dxc, &dir, &mut rng, k, wave, n, !card, len) {
            Ok(()) => *ran.entry(wave.name()).or_insert(0usize) += 1,
            Err(e) => {
                failed += 1;
                eprintln!("{e}");
                if failed >= 5 {
                    break;
                }
            }
        }
    }
    println!("wavetest on {} — random wave-wide GFX10 compute programs, every output lane against the reference console", if card { "the card" } else { "WARP" });
    for (w, c) in &ran {
        println!("  {w:<10} {c} exact");
    }
    println!("  failed     {failed}");
    if failed > 0 {
        // process::exit skips destructors: this exit releases the card lock first
        drop(_lock);
        rebuild_cardlock::exit(1);
    }
}
