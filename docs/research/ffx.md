# The title's FidelityFX upscale (Build 7 groundwork, lane `ffx`)

Read from the prepared copies under `%LOCALAPPDATA%\triumph-rebuild\images\` (never the game folder) with
`dumpbin /exports /imports /disasm` and small read-only scripts (import table, rip-relative references,
constants). Addresses are the exe at its preferred base `0x140000000`; `exe+X` is an RVA. No title bytes are
quoted here.

## 1. What the title uses: the FFX API, upscaler API 4.1.1, an FSR 4 provider

| file | version | what it is |
|:--|:--|:--|
| `amd_fidelityfx_loader_xbox.dll` | 1.0.2.0 (PDB path `...\amdinternal\api\xbox\Gaming.Xbox.Scarlett.x64\Release\`) | the **FFX API** loader: exports `ffxConfigure`, `ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`, `ffxQuery` |
| `amd_fidelityfx_upscaler_xbox.dll` | 3.1.5.0; internal name `amd_fidelityfx_upscaler_xbox_fsr4_full_720p1080p_input_sanitize_scarlett_opt2.dll` | the effect provider: the same five exports; RTTI `ffxProvider_FSR4_Int8`, `Fsr4Int8UpscalerModel`; passes `fsr4_model_v07_fp8_no_scale_pass0..12`, `fsr_rcas_pass` |

- **The API is the typed-descriptor FFX API** (`ffxCreateContext` / `ffxDispatch` / `ffxQuery` with
  `ffxApiHeader` chains), not the older FSR2/FSR3 SDK entry points. The title asks for **upscaler API 4.1.1**
  (`ffxCreateContextDescUpscaleVersion::version = 0x1001001`, `(4 << 22) | (1 << 12) | 1`). Every
  descriptor it builds has the PC SDK v2.4.0 layout byte for byte (§3).
- **The exe imports four functions statically** from the loader (`ffxConfigure` is not imported). IAT
  slots exe+0x6793fc8 (`ffxCreateContext`), +0x6793fd0 (`ffxDestroyContext`), +0x6793fd8 (`ffxDispatch`),
  +0x6793fe0 (`ffxQuery`), reached through jump stubs at exe+0x5f3ff90/a0/b0/c0. Nothing imports the
  upscaler DLL: the loader loads it.
- **The loader routes by the descriptor's effect id** (`type & 0xff0000`): 0x01 (upscale) →
  `LoadLibraryA("amd_fidelityfx_upscaler_xbox.dll")` + `GetProcAddress` of the five names; 0x02/0x03 (frame
  generation) → `amd_fidelityfx_framegeneration_xbox.dll`, which the title does not ship. Its context is a
  16-byte `{provider table, provider context}` allocated through the caller's callbacks (or `calloc`).
  Anything else returns `FFX_API_RETURN_ERROR_UNKNOWN_DESCTYPE` (2) or `NO_PROVIDER`.
- **The provider is FSR 4 for Xbox** (an int8/fp8 ML model), built as console compute pipelines: it
  deserializes AMD-compiled pipeline blobs (`ACCC` parts, "ACO") through `DeserializeComputePipelineStateX`
  and logs "falling back to NewBE" when that fails. That is **device slot 0x218** ("console pipeline, FSR",
  `docs/research/d3d12x-device.md`): it fails on purpose in our device, so today the title's upscale has no
  pipelines. The DLL also imports `D3D12CreateRootSignatureDeserializer` from `d3d12_xs.dll`.
- **The shipping DLL accepts one configuration only** (its checks at upscaler+0xbb10 and the messages
  beside them): `maxUpscaleSize` 1920x1080; `maxRenderSize` 1280x720 ("tight") or 1920x1080 ("legacy"),
  the per-frame render size 1280x720; the creation flags must include `HIGH_DYNAMIC_RANGE`,
  `DEPTH_INVERTED` and `AUTO_EXPOSURE`; `DYNAMIC_RESOLUTION`, display-resolution or jittered motion vectors,
  non-linear colour and debug views are refused ("only the fixed-resolution Quality shader tables are
  included"). So on a Series X this title upscales **1280x720 → 1920x1080** with FSR 4 if it goes this
  way at all. Which sizes it actually asks for on our runtime comes from globals set at run time
  (exe+0x99fecf8/fcfc render, exe+0x8fdc560 and exe+0x91ac9fc output): **a run must log them** (§6).

## 2. The title's code that calls it

One wrapper object (fields below are offsets into it; the FFX context lives at +0x128).

| site | function | when | what it sends |
|:--|:--|:--|:--|
| exe+0x28ef2cc | create, exe+0x28ef1d0 | context (re)creation | `CreateUpscale` → backend `0x01000002` (device = the title's global device, exe+0x8759d00) → `CreateUpscaleVersion` 4.1.1; alloc callbacks null; the return code is **not checked** |
| exe+0x28ef30f | same function | right after create | `GETJITTERPHASECOUNT` (render width +0xf8, display width +0x100) → +0x10c |
| exe+0x28f03db | dispatch, exe+0x28ef700 | once a frame | `DispatchUpscale` (§3) |
| exe+0x28ef625 | jitter, exe+0x28ef560 | once a frame, before the scene | `GETJITTEROFFSET` (index = frame counter mod phases, +0x110; phases +0x10c). If byte +0x149 is set it uses its own R2 sequence instead (`frac(0.5 + n * (0.754878, 0.569840)) - 0.5`) |
| exe+0x28f0651 | exe+0x28f0610 | after create | `GPU_MEMORY_USAGE` (0x10008), total added to a memory statistic |
| exe+0x28ef512 | destroy, exe+0x28ef470 | teardown / recreate | `ffxDestroyContext`, after releasing its own reactive and transparency textures |

The jitter it gets is stored per frame (+0x118/+0x120); the projection gets `(2x, -2y)` (in render pixels,
turned into clip space by exe+0x6e3250), and the dispatch gets **`(-x, -y)`**.

## 3. Layouts (x64), as the title fills them

All match the PC FidelityFX SDK v2.4.0 headers (MIT); `crates/ffx/src/api.rs` pins each offset with an
assert.

`ffxCreateContextDescUpscale` (0x10000), 0x30 bytes: `+0x10 flags` = HDR (wrapper byte +0x145) |
DEPTH_INVERTED (+0x147) | DEPTH_INFINITE (+0x148) | AUTO_EXPOSURE (+0x146); `+0x14 maxRenderSize`,
`+0x1c maxUpscaleSize` (copied from +0xf8..+0x107); `+0x28 fpMessage` = null.

`FfxApiResource`, 48 bytes: `+0 resource` (the console `ID3D12Resource*`), `+8 type, format, width, height,
depth, mipCount, flags, usage` (built from the resource's `GetDesc`, console slot +0x50, and a DXGI→FFX
format table at exe+0x6300ee0: the SDK's `ffxApiGetResourceDX12`), `+0x28 state`.

`ffxDispatchDescUpscale` (0x10001), 0x1b0 bytes:

| offset | field | the title's value |
|:--|:--|:--|
| 0x10 | commandList | its render context's current **console** command list |
| 0x18 / 0x48 / 0x78 | color / depth / motionVectors | state `PIXEL_COMPUTE_READ` (0xc) |
| 0xa8 | exposure | state 0xc; null when it has none or `AUTO_EXPOSURE` is set |
| 0xd8 / 0x108 | reactive / transparencyAndComposition | its own textures (state `UNORDERED_ACCESS`), only when wrapper byte +0x98 is set |
| 0x138 | output | state `UNORDERED_ACCESS` (2) |
| 0x168 | jitterOffset | `(-x, -y)` of the queried offset |
| 0x170 | motionVectorScale | **(1.0, 1.0)**: the motion vectors are already in render pixels |
| 0x178 / 0x180 | renderSize / upscaleSize | colour and output sizes (from the textures, by mip) |
| 0x188 / 0x18c | enableSharpening / sharpness | `sharpness > 0` / wrapper +0x130 |
| 0x190 | frameTimeDelta | per frame |
| 0x194 | preExposure | 1.0 |
| 0x198 | reset | per frame |
| 0x19c / 0x1a0 | cameraNear / cameraFar | swapped when depth is inverted |
| 0x1a4 | cameraFovAngleVertical | per frame |
| 0x1a8 | viewSpaceToMetersFactor | 0 |
| 0x1ac | flags | wrapper +0x144 |

Queries: `GETJITTERPHASECOUNT` (0x10004: render width +0x10, display width +0x14, `int32*` +0x18),
`GETJITTEROFFSET` (0x10005: index +0x10, phases +0x14, `float*` x +0x18, y +0x20), `GPU_MEMORY_USAGE`
(0x10008: `FfxApiEffectMemoryUsage*` +0x10).

## 4. How often

Create: once at start, again whenever the wrapper is rebuilt (resolution or quality change). Per frame:
one `GETJITTEROFFSET` and one `ffxDispatch`. Destroy: before every recreate. With the title's DLLs, each
create builds the FSR 4 pipelines through device 0x218 (the "≥32, then through the first frames" in
`d3d12x-device.md`); with ours, 0x218 is no longer reached from here.

## 5. What we do instead (`crates/ffx`)

The runtime serves both DLL names itself (`CONSOLE_DLLS`, `console_export` → `rebuild_ffx::export`) and
does not map the title's copies (`replaces_title_dll`; `REBUILD_FFX=title` restores them). Create makes an
`Upscaler` (DLSS through Streamline when it attaches, i.e. NVIDIA; FSR 3.1 through the PC SDK otherwise);
dispatch translates FFX's conventions to `rebuild-upscale`'s (`jitter = -jitterOffset`, measured in
`agents/upscale.md`; `motion_scale = motionVectorScale`), maps each console resource through
`Resources::host_resource`, and records barriers + upscale + barriers back into the host list the console
list is recording now (the last `rebuild_xlist::console::segments` entry). Queries answer with FSR's own
rules (phase count `int(8 * ratio^2)`, Halton(2,3) − 0.5 offsets). Status and the open questions:
`agents/ffx.md`.

## 6. Unknown until a run

- the sizes and flags at create (expect 1920x1080 output and HDR | DEPTH_INVERTED | AUTO_EXPOSURE if the
  title follows its shipping DLL's rule), and whether it creates at all on our runtime;
- whether `commandList` is one of our console lists, and the colour/depth/motion formats and states;
- whether byte +0x98 (reactive and transparency textures) and +0x149 (own jitter) are set.

The first `ffxDispatch` and every create print all of this (`ffx: context: ...`, `ffx: first dispatch: ...`).
