# The D3D12.X device: creation, tables, object layouts, startup order (Build 5 input)

Lane `device-research`, 2026-11-27. Design input for Build 5; no code. Every fact names its source:

- **fork** = `git -C D:\Developer\fxceless-victory show triumph/main:<path>` (GPL-3.0: facts and layouts only,
  nothing copied). Paths below are relative to `crates/victory-gpu/src/` unless given in full.
- **timing-run** = the fork's scrubbed run log `diagnostics/timing-run-2026-11-25.txt` (run 278, 18,691
  lines; same tree), `L<n>` its line number. `timing-run-295s` = `diagnostics/timing-run-295s-2026-11-25.txt`
  (run 280, 295 s of play). These are INFO-level logs: device calls are logged only when they warn or
  print a one-time line, so call counts exist only for those.
- **ours** = this repository (`runs/build3-reached.md`, `crates/runtime/src/exports.rs`).

Tables: [`agents/d3d12x-device-slots.csv`](../../agents/d3d12x-device-slots.csv) (all 160 device slots),
[`agents/d3d12x-child-slots.csv`](../../agents/d3d12x-child-slots.csv) (queue, list, allocator, fence,
heaps, resources, DXGI), [`agents/d3d12x-object-layouts.csv`](../../agents/d3d12x-object-layouts.csv) (fields the
title reads inline).

**The one fact that matters most:** in both fork logs (startup to the world, and 295 s of play) there is
**no `method not implemented` line for any object**. The fork's tables log every unwritten slot once, with
the caller's offset (`object.rs:282 missing`). So the slots the fork wired, plus two creation methods that
fail on purpose (0x1c8, 0x218), are everything this title calls. That set is the complete target, and
most of it is needed before the first Present.

---

## 1. `D3D12XboxCreateDevice`

```
HRESULT D3D12XboxCreateDevice(void* adapter, const PARAMS* params, REFIID riid, void** out)
ours (runs/build3-reached.md):  (0, 0xcf04aff8e0, 0x7ffa47660638, 0x7ffa48a5f048)
                                 adapter null · params on the caller's stack · riid and out in a mapped image
```

| arg | what | source |
|:--|:--|:--|
| `adapter` | null (the default adapter) | ours: arg 0 is 0 |
| `params` | starts with a `u32` **interface version, `0x00020004` ("2.4")**. The rest (command ring and scratch sizes, features to leave out) is not decoded by the fork and does not matter to it. | fork `device.rs:98` `CHECKED_VERSION`, `:124-140`; timing-run L52 `D3D12.X device version=0x20004` |
| `riid` | **`ID3D12Device11`** `{5405c344-d457-444e-b4dd-2366e45aee39}`. The fork accepts `ID3D12Device` `{189819f1-…}` and Device1-Device8 too, and returns `E_NOINTERFACE` for anything else. | fork `device.rs:26-92` `LEVEL_IIDS[8]`, `:153` |
| `out` | receives the device pointer. Ours points into a mapped image (a global the title keeps). `out == null` returns `S_FALSE` ("would creation succeed"). | fork `device.rs:151`; ours |

**What the title expects back:** `S_OK` and a pointer to **one device object, static for the process**
(fork `device.rs:114` "one device per process, as on the console"). It is 0x58 bytes, laid out as in §3,
with a 160-slot table (§2).

**The creation IID selects the layout of every child table.** Creating as `ID3D12Device11` means the
title is a "newer" title (April 2026 build, against Minecraft's March 2022). Its queues and lists have two
extra methods each, which moves later slots by +0x10 (§2.2). The interface version stays 2.4, so the IID
is the only sign. The fork calls this a shortcut (`device.rs:157-165`).

**Before it:** `D3D12ConfigureVirtualMemoryX(*{flags u32, page_table_pages u32, preserved_bytes u32; out
lowest u64, highest u64})`. The title passes `flags=0 page_table_pages=4 preserved_bytes=0` (timing-run L27,
before GDK init). The fork returns all of user space as the GPU address range (`device.rs:185-230`).
Ours returns 0 and fills nothing (`exports.rs` `D3D12ConfigureVirtualMemoryX`). **For the lead:** fill
`lowest/highest` the way Build 1's windows need them (the title may size its GPU VA layout from them).

**Other `d3d12_xs` exports:** `D3D12SerializeVersionedRootSignature` (title exe and BVH builder; the
host's own, after dropping console flag bits 24 and up, fork `root.rs:1-6`), and
`D3D12CreateRootSignatureDeserializer` (only the AMD upscaler, Series S path). The fork also exports
`IID_ID3D12Device` as data. This title does not import it (`agents/loader-imports.csv` lists 4
`d3d12_xs` names), so it is not needed.

---

## 2. The tables

### 2.1 Device (160 slots, 0x000-0x4f8): full list in `agents/d3d12x-device-slots.csv`

Layout (fork `device.rs:402-406`): IUnknown 3, `ID3D12Object` 4, then **`ID3D12Device` in PC order
(0x38-0x158, 44 PC methods)**, then **console additions 0x160-0x290**, then Device1-Device8 additions
0x298-0x3a0 (3, 1, 3, 6, 9, 1, 2, 9: five console slots among them, positions unknown), then
Device9-Device11 (console layout unknown).

Slots this title calls (the fork wired them; nothing else is ever logged as called):

| offset | method | PC equivalent | args after `this` |
|:--|:--|:--|:--|
| 0x00-0x30 | QI, AddRef, Release, Get/SetPrivateData, SetName | IUnknown / ID3D12Object | as PC |
| 0x38 | GetNodeCount | same | → 1 |
| 0x40 | CreateCommandQueue | same | `*desc{type,prio,flags,mask}, iid, *out` |
| 0x48 | CreateCommandAllocator | same | `type, iid, *out` |
| 0x50 / 0x58 | CreateGraphics / ComputePipelineState | same | `*desc, iid, *out` |
| 0x60 | CreateCommandList | same | `mask, type, alloc, pso, iid, *out` |
| 0x68 | CheckFeatureSupport | same | features 0-4. The OPTIONS struct is **16 words, one more than PC's** (console field at index 11) |
| 0x70 | CreateDescriptorHeap | same + **placement** | `*desc{type,count,flags,mask}` followed in memory by `{address u64, size u64}`: the title's own memory for the descriptors |
| 0x78 | GetDescriptorHandleIncrementSize | same | the fork returns 32 / 16 / 128 / 128 |
| 0x80 | CreateRootSignature | same | `mask, *blob, size, iid, *out` |
| 0x88-0xb0 | Create CBV / SRV / UAV / RTV / DSV / Sampler | same | as PC (D3D12-shaped desc words, not T#/V#) |
| 0xb8 / 0xc0 | CopyDescriptors / CopyDescriptorsSimple | same | as PC |
| 0xc8 / 0xd0 | GetResourceAllocationInfo / GetCustomHeapProperties | same | hidden `*out` first (struct return) |
| 0xd8 / 0xe0 / 0xe8 | CreateCommittedResource / CreateHeap / CreatePlacedResource | same | as PC |
| 0x120 | CreateFence | same | `initial, flags, iid, *out` |
| 0x130 / 0x138 | GetCopyableFootprints / CreateQueryHeap | same | as PC |
| 0x148 | CreateCommandSignature | same | as PC (most draws are ExecuteIndirect) |
| 0x180 | CreateCommandQueueX | console | `*{type, flags, engine_or_pipe, queue}, iid, *out` |
| **0x1c8** | **pipeline from console code, unknown** | console | `*desc, iid(ID3D12PipelineState), *out`. **Fails on purpose** |
| 0x1e8 | CreateSamplerX | console | `*desc (27 words), handle` |
| 0x1f0 | CreateComputePipelineStateX | console | `*desc, ext count, *exts, iid, *out` |
| **0x218** | **pipeline from console code (FSR passes)** | console | as 0x1c8. **Fails on purpose** |
| 0x228 | debug error filter | console | ignore, `S_OK` |
| 0x230 / 0x238 | placed SRV / UAV (views of memory the title placed) | console | `*res desc (14 words), *view desc (16 words; words 10-11 = address), handle` |
| 0x248 | CreateCommandListX | console | `*{type, flags, mask, barrier batch}, alloc, pso, iid, *out` |
| 0x250 / 0x258 | register / unregister a 64 KiB page pool | console | `address, pages, *handle` / `handle` |
| 0x260 | CreateCommittedResourceX (PC + DCC options) | console | Series S upscaler only |
| 0x270 | set frame interval | console | `output, interval µs (0 = 1/60 s), period, flags`. The title sets **33.333 ms** |
| 0x278 | schedule frame event | console | `kind, offset, *list, flags` |
| 0x280 | wait frame event | console | `kind (0 origin, 1 vblank, 2 flip), timeout ms, *list, flags (2 = token on timeout), *token` |
| 0x288 | frame statistics | console | `token, kinds, *count, *out` |
| 0x2b0 | CreatePipelineState (stream) | Device2 | `*{size, *stream}, iid, *out`. **Console subobject numbers: AS 23, MS 24 (PC 24, 25), console types from 25** |
| 0x360 | GetResourceAllocationInfo2 | Device8 | hidden `*out` first |
| 0x3f8 | CreateCommittedResource3 | Device10 (inferred) | `heap, flags, *desc1, layout, *clear, prot, ncast, *cast, iid, *out`. **The back buffers** (title+0x193e1b4, heap flag "allow display") |
| 0x428 | resource at a title-mapped address | console (Device9-11 range) | `address, *desc1, layout, *clear, iid, *out` |

Sources: fork `device.rs:407-457` (the table), each function's file in the CSV. The "X" names are the
fork's descriptions, not verified console names.

**How often and when** (only what the logs show; INFO level):

| slot | count | when |
|:--|:--|:--|
| 0x1c8 | ≥ 16,384 (power-of-two log lines L184-L582), 8,192 before the first Present | a burst at 2.036-2.040 s wall (timing-run L184-L215), right before the first submission |
| 0x218 | ≥ 2,048 (L173-L532) | starts the same burst (L173), continues through the first frames |
| 0x270 | 2 before the first Present | after the display mode query (L59) and again right before Present (L222) |
| pipelines (0x2b0 / 0x1f0) | ~60,000 in the first 20 s | fork `pipeline.rs:7` |
| CBVs (0x88) | ~14,000 a frame in play | fork `descriptor.rs` `Views` doc (run 65) |
| list calls | 31 a frame in menus, **20,293 a frame in the world** | timing-run-295s L3379; fork `CLAUDE.md` |

### 2.2 Children: queue and list slots move in this title

Full tables in `agents/d3d12x-child-slots.csv`. The device-child prefix is shared: IUnknown 0x00-0x10,
object 0x18-0x30, `GetDevice` 0x38 (answers every device level).

**Queue** (fork `queue.rs:1536-1570`): PC order to 0x68, then **two console methods at 0x70/0x78 in newer
titles**, so for this title:

```
0x50 ExecuteCommandLists   0x80 Signal       0x88 Wait            0x90 GetTimestampFrequency
0xe0 GetDesc               0xf8 page-mapping batch (tiled pages from a registered pool)
0x100 PresentX(planes, *{token u64, count u32, *resources}, *params)
0x110 Signal + top/bottom-of-pipe sync      0x118 ExecuteCommandListsX(count, *lists, frame token, flags)
```

**List** (fork `list.rs:2105-2225`): the console moved BeginQuery / EndQuery / ResolveQueryData to
0x0e0-0x0f0, before SetDescriptorHeaps (0x100). Otherwise PC order to 0x1d0. Newer titles add **two
methods at 0x1d8/0x1e0**, so every 2020 slot from 0x1d8 moves +0x10:

```
0x1e8 ExecuteIndirect   0x1f0 CopyMemoryToMemoryX   0x238/0x240 bottom-of-pipe write 32/64
0x248/0x250 wait for memory value 32/64 (address, comparison, reference)
0x280 DispatchX   0x328 DriverHintX → 0   0x408 DispatchMesh   0x418/0x420 pipe syncs
0x458 ResetX (the title asserts on failure)
0x470 unknown(u64), result unused   0x490 enhanced barrier groups   0x4a0 allocate list-owned upload memory → pointer
```

0x4a0 must return a real pointer or null. Returning `E_NOTIMPL` crashed the BVH builder, which used the
value as a pointer (fork `list.rs:1468-1473`).

**Fence** (fork `fence.rs:306-318`): 0x40 GetCompletedValue, 0x48 SetEventOnCompletion, 0x50 Signal, 0x58
GetCreationFlags, **0x60 GetGpuAddress (console): the title reads the value at that address**.

**DXGI** (fork `dxgi.rs`): device `QueryInterface` → IDXGIDevice (0x38 GetAdapter) → adapter (0x38
EnumOutputs) → output **0xa8 display-mode list (console)**. The fork gives one mode, 3840x2160 at 60 Hz,
and the title looks for a 120 Hz mode before it allows a 120 Hz interval (`dxgi.rs:243`; timing-run L58).

---

## 3. Object memory the title reads inline

Details and sources in `agents/d3d12x-object-layouts.csv`.

```
every object  +0x00 vtable  +0x08 u32 bits (device index bits 0-2; 0)  +0x0C u32 refcount
device        +0x10 u64 null-CB GPU address (0)   +0x18 BYTE != 0 → the title CALLS what it would inline
              +0x18..+0x58 reserved                size 0x58, one static object
queue         +0x10 u32 list type
list          +0x10 u32 type
              +0x18 put   +0x20 plain limit   +0x28 draw limit   +0x30 API limit   +0x38 unbiased API limit
              ALL ZERO → every command arrives as a call (§2.2) instead of packets written inline
              +0x140 graphics / +0x340 compute root user-data headers (inline packet state; dead at 0 limits)
              +0x540..+0x560 pipeline shadow: the title's state flush (title+0x32786) copies the pipeline's
              32-byte packet to +0x544..+0x564 and tests +0x548 even with zero limits
              newer-title fields past +0x560, size unknown: keep our own fields beyond +0x960
pipeline      +0x10 32-byte hardware packet (the title copies it into the list inline)
fence         value at the address GetGpuAddress (0x60) returns; the title polls it from the CPU
descriptor    placed heaps live in title memory; the title copies descriptors in as bytes
```

Notes for our design:

- **Lists.** Putting every limit at 0 is what the fork did, and it is proven to run to the world. Then
  `crates/xlist` sees each command as a call (Build 0 (b): 0.43 µs a draw). The inline-packet route
  (parsing packets the title writes) stays a later choice measured on the frame line (handoff "Open
  threads").
- **The list's first 0x960 bytes belong to the title's layout.** Our recording state must sit after that,
  or in a side table keyed by the list pointer.
- **Fences.** The title reads fence values from memory, and lists wait on memory values in title memory
  (the first is at `0xdd2b00000`, comparison 7, reference 1, 64-bit, already met: timing-run L217). So
  values must land in title-visible memory when the GPU passes the signal. That is Build 2's labels
  (`gputest --labels`: GPU writes straight into title memory, 65 µs before the fence). The fork kept the
  value in the fence object and signalled at submit, so its signals came early or late. tasks.md lists
  that as a known trap.
- **Descriptors.** Views are 32 bytes and samplers 16, the size of RDNA 2's T#/V# and S#. Placed-heap
  detection works by `placement.size == count × increment` (fork `descriptor.rs:1104`), and it found
  placed heaps (CLAUDE.md: 26,393 views of placed memory). So the title sizes view heaps with 32 bytes a
  view. **Inference:** it may hard-code that stride, so do not change it. The title copies records as
  bytes (in menus, ~40 changed heap pages and 20,500 new records a frame: timing-run L623). Plan decision 2
  covers this: keep a dirty list over write-watched placed heaps, never a scan. The fork's records are
  12-byte tags `[tag, index, generation]` (`docs/research/passthrough.md` fact 2), not hardware
  descriptors. Our heap content is our choice, but a record must survive being copied as bytes.

---

## 4. Startup order after device creation (timing-run, wall clock 22:39:xx; image mapped at 00.660)

| t (s) | line | what | slot |
|:--|:--|:--|:--|
| 00.686 | L27 | `D3D12ConfigureVirtualMemoryX` flags 0, 4 page-table pages | export |
| 00.725 | L29 | GDK init (`InitializeApiImplEx2`), GameInput, DirectStorage files opened | - |
| 00.803 | L50-51 | display HDR query (SDR); window fitted | (display module) |
| **00.822** | **L52** | **`D3D12XboxCreateDevice`, version 0x20004** | export |
| 01.240 | L56 | (fork's host D3D12 device made: 0.42 s of ours) | - |
| 01.768 | L57-58 | display mode list read: 3840x2160 | DXGI QI → adapter → output 0xa8 |
| 01.768 | L59 | frame interval 33.333 ms | device 0x270 |
| 01.774 | L61 | the title's `__XMemAllocationHooks__` installed | (xmem) |
| 01.786- | L62-171 | pipelines created (fork: compute first, then VS+PS) | 0x2b0 / 0x1f0 (+ root sigs 0x80) |
| 02.036 | L173-215 | burst: 0x218 ×≥32 and 0x1c8 ×≥8,192 in 4 ms | device 0x218, 0x1c8 |
| 02.043 | L217-218 | first list with a wait for a memory value (cut into 2 parts) | list 0x250 |
| 02.048 | L219 | **first ExecuteCommandLists**: direct queue, 1 list | queue 0x50 or 0x118 |
| 02.049 | L220 | fence Signal → 1 | queue 0x80 |
| 02.049 | L222 | frame interval set again | device 0x270 |
| **02.049** | **L223** | **first Present: frame 1, 0 draws** (the fork's own capture was black, as expected) | queue 0x100 |
| 02.601 | L358 | frame 2: 2 draws (first pipeline bound at frame 2, L319) | |

From the device to the first Present takes **1.23 s**. Ours reaches the device 1.4 s after the entry
point (`runs/build3-reached.md`).

**Not in the logs, and fixed by dependency:** before L217 the title must have created at least one queue
(0x40 or 0x180), an allocator (0x48), a list (0x60 or 0x248), a fence (0x120), and the back buffers
(0x3f8, with RTVs from 0x70/0x78/0xa0). Its frame loop waits on frame events (0x280: an origin poll with
"token on time-out", then a blocking flip wait before each Present, fork `frame.rs:1-8`). The exact order
of those calls is not logged anywhere in the fork. **Recommendation:** our slot trap should log the first
call of every slot with a timestamp and keep counts. One run then writes this table exactly, and it costs
nothing per call after the first.

---

## 5. What the fork got wrong, found hard, or left as shortcuts

| # | where | what happened | rule for us |
|:--:|:--|:--|:--|
| 1 | device 0x1c8, 0x218 | never implemented. The title gives up the pipeline without a word. 0x218 builds FSR passes from console code (`ACCC`/`FSPX` containers, no DXIL; `docs/design/dlss.md` §0). Its implementation (branch `fsr`, `4b3fe7e2`) cost **4×** per draw (0.115 vs 0.029 ms) and **asserted inside D3D12Core at start** (`0x80000003` at `D3D12Core+0x11632`) | first: fail with `E_NOTIMPL` and `*out = null` (proven safe: answering `S_OK` without an object hands the title stack garbage, `device.rs:357-358`). Later: 0x1c8's descriptions carry GPU-code addresses and sizes (`d49420000, 0x200` …), which is **what `sir` translates**, so decode it in Build 5/7 rather than copy the fork's path |
| 2 | list 0x4a0 | returned `E_NOTIMPL`; the BVH builder used it as a pointer and crashed | return memory or null, never an HRESULT, from pointer-returning slots. The fork's trap is "wrong for a slot that returns a value" (`object.rs:280-281`) |
| 3 | layouts | newer queue/list layouts inferred from call shapes (+2 slots at 0x70 / 0x1d8). The IID is the only sign of a newer title (`device.rs:157`) | keep both tables; select by creation IID; log the IID |
| 4 | descriptor heap placement | recognised by a size match, because the plain and extended descs share the slot and version (`descriptor.rs:1100-1103`) | same heuristic first; find the flag that tells them apart if a title trips |
| 5 | fences | a null-event (blocking) wait returns `E_NOTIMPL` (`fence.rs:110-116`); signals completed at submit, not when the GPU passed them | blocking waits are real; values come from GPU-written labels (Build 2) |
| 6 | queues | waits on memory values held submissions whole, so queues waited on each other: **2.4-2.9 s per frame** until lists were cut at their waits (runs 27-30, `queue.rs:199-235`) | the host GPU orders it: a list's wait for a memory value becomes a GPU-side wait, never CPU holding |
| 7 | serial translation | 20,293 list calls a frame = **243 ms** on one thread (ROADBLOCK §3) | plan decision 3: translate at Close on the recording thread |
| 8 | memory mirror | 85-110 ms a frame of copies and copy-back (ROADBLOCK §1-2) | decisions 1-2 (tiles, ring), already built |
| 9 | root signatures | missing `SAMPLER_HEAP_DIRECTLY_INDEXED` beside static samplers: **311 pipelines refused**, black frames (runs 26-27) | always set it (tasks.md "Known traps") |
| 10 | view heap size | the title's shader-visible view heap has **1,245,184** descriptors. The host refused that size and the fork capped it at 1,000,000 (timing-run L322) | check the host limit at start; size the heap to the title's count, or remap |
| 11 | CheckFeatureSupport | format support answers "everything" (`device.rs:309-311`, shortcut) | answer from the host device |
| 12 | frame pacing | a clock of the fork's own, events at every tick, offsets ignored (`frame.rs:1-10`). The title runs its own 30 fps limiter (33-34 ms in menus) | pace from presentation (Build 8). Find the limiter before any 60 fps attempt |
| 13 | first frame | frame 1 has 0 draws and is black, by the title's own doing (L223, L234) | "non-black frame" is judged from frame 2 on |
| 14 | format 86 (B5G5R5A1) | the host does not render to it; the fork stands in format 28 (RGBA8) (timing-run L662) | same stand-in, counted |
| 15 | watchdog | the title's hang watchdog fires after 30 s without a frame (the "crash" at title+0x23cf683) | nothing on the title's threads blocks for seconds (pipelines from the library, built before the world) |

---

## 6. Recommended minimal first set: to the first Present

Order: what the logs show is used before L223, then what one frame loop needs. **Every other slot of the
160 is a trap**: it logs the offset, the caller (module+RVA), the first-call time and a count, and fails.
A slot that returns a pointer or a value must not return an HRESULT (§5.2).

| step | slots | behaviour in the first cut |
|:--:|:--|:--|
| 0 | `D3D12ConfigureVirtualMemoryX`, `D3D12XboxCreateDevice` | fill the GPU VA range; static 0x58-byte device, **+0x18 = 1**; accept Device..Device11, select newer child tables on Device11; log params' first 16 words and the IID once |
| 1 | device 0x00-0x38; DXGI device → adapter → output 0xa8 | refcounts; one adapter, one output, the host display's mode (the title reports 3840x2160 and renders 2560x1440) |
| 2 | device 0x68, 0x78, 0x270 / 0x278 / 0x280 / 0x288 | features 0-4 (16-word OPTIONS); increments 32/16/128/128; frame events on a clock (fork-style) until Build 8 |
| 3 | device 0x40 / 0x180, 0x48, 0x60 / 0x248, 0x120, 0x70 | queue (+0x10 type), allocator, list (**put and limits at 0**, our state past +0x960), fence (value in label memory; 0x60 returns its address), descriptor heaps (placed, write-watched) |
| 4 | device 0x80, 0x2b0, 0x1f0, 0x50 / 0x58 | root signatures (host serializer); pipelines kept as descriptions, made from the `sir` library, **never compiled at creation** (~60,000 in 20 s) |
| 5 | device 0x1c8, 0x218 | `E_NOTIMPL`, `*out = null`, count (expect ≥ 8,192 + ≥ 32 before Present) |
| 6 | device 0xc8 / 0xd0 / 0xd8 / 0xe0 / 0xe8 / 0x130 / 0x360 / 0x3f8 / 0x428, 0x250; resource 0x40-0x70; heap 0x40 | resources at console addresses on Build 1 tiles; back buffers from 0x3f8 |
| 7 | device 0x88-0xc0, 0x1e8, 0x230 / 0x238 | views into the console heap + PC heap beside, dirty list |
| 8 | list 0x040-0x050, 0x0c8, 0x0d0, 0x100-0x1a0, 0x238-0x250, 0x418 / 0x420, 0x458, 0x490, 0x4a0; allocator 0x40 / 0x48 | through `crates/xlist`; memory waits as GPU waits; bottom-of-pipe writes as labels |
| 9 | queue 0x50, 0x80, 0x88, 0x90, 0x110, 0x118, 0x100 (PresentX: one plane, its first resource) | submit; Signal writes the label when the GPU passes it; Present copies plane 0 to the swap chain. **Frame 1 is black by design** |
| 10 | fence 0x40-0x60 | blocking wait (null event) waits for real |

Proof, in order: the trap list after one run is **empty except 0x1c8 / 0x218**. Then L223's
equivalent: Present frame 1 with 0 draws. Then frame 2 with draws, non-black (tasks.md Build 5: "a
non-black frame, captured to PNG").

Left for after the first frame: draws through ExecuteIndirect at scale (0x148, list 0x1e8), queries
(0x138, list 0x0e0-0x0f0), the queue's page mapping (0xf8) with page pools (0x250 / 0x258), mesh
dispatch, and a decode of 0x1c8 / 0x218.
