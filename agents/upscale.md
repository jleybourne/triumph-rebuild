# Lane `upscale`: the DLSS and FSR backends, standalone (Build 7 piece)

Crate `crates/upscale` (package `rebuild-upscale`), binary `apps/rebuild/src/bin/upscaletest.rs`.
Measured on the dev machine (RTX 4070 Ti), 2026-11-27, with `agents\MEASURING` absent.

| # | task | state | the number |
|:--:|:--|:--|:--|
| P1 | one interface (`Upscaler`, `Inputs`) | done | 4 unit tests: Halton, jitter phases, render sizes per mode, input checks |
| P2 | synthetic scene: moving geometry, Halton jitter, motion vectors, depth, supersampled truth | done | every motion vector followed back lands on the same surface point, worst error < 1e-4 (more than 98 % of pixels; the rest were hidden the frame before); background motion = camera pan exactly |
| P3 | FSR 3.1 through the FidelityFX SDK | done | FSR 3.1.6 provider: 0 NaN, 0 black frames, beats bilinear on every check; side by side with DLSS on the same inputs (below) |
| P4 | DLSS through Streamline | done | 0 NaN, 0 black frames, beats bilinear on every check; side by side with FSR |
| P5 | the conventions table | done; both SDKs measured with the same 16-way sign sweep | below |

11 unit tests in `rebuild-upscale`; the workspace stays green (114 passed, 1 ignored).

## Built

- **`Upscaler` trait and `Inputs`** (`crates/upscale/src/lib.rs`): colour, depth, motion, optional exposure,
  output, render size, output size, jitter, motion scale, depth reversed, reset, HDR, pre-exposure, frame
  time, near/far/fov. **One convention, the layer's** (below); each backend converts inside `evaluate`.
  `Inputs::check` refuses what makes an SDK silently produce garbage (render larger than output, zero size,
  jitter outside +/-0.5, non-finite values). `Signs` is the per-SDK sign mapping, shared by both backends;
  `file_version` reads a DLL's version for the frame line.
- **Scene** (`scene.rs`): analytic CPU renderer, any size of one aspect: panning textured background
  (checker, zone plate, thin stripes), a rotating striped square, a ringed disc on a circular path; one
  sample per pixel at the jittered position, exact motion vectors, device depth (normal or reversed), 4x4
  supersampled truth at output size. 4 threads at most.
- **Image helpers** (`image.rs`): half floats (all 65,536 round-trip), dependency-free PNG writer, PSNR,
  frame-to-frame difference, NaN/black counts, bilinear baseline.
- **DLSS backend** (`dlss.rs`): Streamline 2.14.16 declared from its MIT headers (layouts checked by
  compile-time asserts), loaded at run time with `LoadLibrary`; manual hooking (`slSetD3DDevice` on our
  own device), no OTA, no downloaded plugins. Plugin paths = the Streamline folder, then the DLSS DLL's
  folder. The Streamline log confirms NGX loaded the DLL from the configured path:
  `feature dlss snippet: D:\tools\dlss/nvngx_dlss.dll version: 310.9.16`.
- **FSR backend** (`fsr.rs`): the FFX API of FidelityFX SDK v2.4.0, declared from its MIT headers (layouts
  checked by compile-time asserts) and loaded at run time: `amd_fidelityfx_upscaler_dx12.dll` first, then
  `amd_fidelityfx_loader_dx12.dll`, which finds the module already in the process. The DLL offers the
  providers `3.1.6` and `2.3.4`; the backend asks for the list and picks the newest 3.1.x explicitly
  (`ffxOverrideVersion`), so this is **FSR 3.1.6**. The context is created for the largest output and is
  recreated if depth-reversed or HDR changes, because FSR fixes those flags at creation.
- **`upscaletest`**: `--backend dlss|fsr|both`. It prints every backend's render size per mode, then runs
  64 moving frames and 64 static frames from a reset. Each frame is uploaded once and every backend
  upscales the same inputs. PNGs go to `--out`, outside git: input, truth, bilinear, one per backend, and a
  2x side-by-side crop of truth | bilinear | DLSS | FSR. `--sweep` tries all 16 sign combinations on one
  backend. The GPU time comes from 200 back-to-back evaluations per backend.

## Measured

`upscaletest --backend both` at 2560x1440 output, 64 frames, PSNR and stability on the last 8. Both
backends get identical inputs at the first backend's render size (DLSS's: 1707x960 for quality; FSR's
own would be 1706x960). The raw output is in
`D:\victory-data\upscale-out\both-quality.txt`, `both-performance.txt`, `sweep.txt` (DLSS) and
`sweep-fsr.txt`. The PNGs are in `both-quality\` and `both-performance\` beside them (outside git).

| mode (render) | scene | DLSS 310.9.16 | FSR 3.1.6 | bilinear | static frame-to-frame (mean abs diff, 0-255): DLSS · FSR · bilinear |
|:--|:--|--:|--:|--:|:--|
| quality (1707x960) | moving | 30.96 dB | **33.69 dB** | 26.50 dB | n/a (the scene moves) |
| quality (1707x960) | static | 33.84 dB | **35.08 dB** | 26.14 dB | 0.284 · **0.159** · 6.139 |
| performance (1280x720) | moving | **31.30 dB** | 29.71 dB | 24.76 dB | n/a |
| performance (1280x720) | static | **34.73 dB** | 31.18 dB | 25.33 dB | 0.590 · **0.169** · 6.104 |

| GPU time of the upscale, 200 evaluations back to back | DLSS 310.9.16 | FSR 3.1.6 |
|:--|--:|--:|
| quality | 1.001 ms median, 1.008 p90 | **0.508 ms** median, 0.510 p90 |
| performance | 1.147 ms median; p90 12.5 ms in this run, an outlier (probably another process on the card; the earlier DLSS-only run had p90 1.151) | **0.408 ms** median, 0.411 p90 |

NaN pixels 0 and black frames 0 in every run, both backends. The timestamps are taken around
`slEvaluateFeature` / `ffxDispatch`. Inside the CPU-rendered sequence the card idles and clocks down
between frames, so only the back-to-back number is reported.

**What it says, with the caveat:** both beat bilinear by 4.5-9.4 dB and are 10-40x steadier on a static
camera. FSR is ahead at quality and DLSS is ahead at performance. FSR is the steadier of the two on a
static camera, and it costs about half the GPU time on this card. This is a synthetic scene of hard
procedural patterns, scored by PSNR, which is not a perceptual measure. Use it to check that the inputs
are right, not to choose a winner; the title's own frames decide that in Build 7. DLSS runs with its
default preset per mode (`--preset` overrides it).

Render sizes at 2560x1440. Streamline: DLAA 2560x1440, quality 1707x960, balanced 1485x835, performance
1280x720, ultra-performance 853x480. FSR: 2560x1440, 1706x960, 1505x847, 1280x720, 853x480, which is
exactly the ratio rule. Ask each SDK; they differ at quality and most at balanced.

FSR's jitter query (`GETJITTEROFFSET`) returns exactly the layer's Halton(2,3) - 0.5 values, frame for
frame. Its phase count for 1706 to 2560 is **18**; `jitter_phases` gives 19, because ceil(8 * 1.5006^2) =
ceil(18.01). That makes no measurable difference, but the layer can ask the SDK while FSR is active.

**The sign sweeps** (quality, moving, 24 frames, PSNR on the last 8).

| jitter x | jitter y | motion x | motion y | DLSS | FSR 3.1.6 |
|:--:|:--:|:--:|:--:|--:|--:|
| + | + | + | + | 27.16 dB | 27.77 dB |
| - | + | + | + | 27.98 dB | 29.09 dB |
| + | - | + | + | 29.87 dB | 30.48 dB |
| **-** | **-** | **+** | **+** | **31.58 dB** | **33.00 dB** |
| any | any | - on either axis | | 18.3-19.8 dB | 19.98-22.11 dB |

So in the layer's convention, **both SDKs take the negated sample offset as `jitterOffset`, on both axes**,
and both take the motion vectors unchanged (previous - current, +y down). Streamline wants the scale
divided by render size; FSR takes `motion_scale` as is. `Signs::STREAMLINE` and `Signs::FIDELITYFX` hold
these mappings and are the defaults. This agrees with FSR's own sample code: it applies `+2*jx/W`,
`-2*jy/H` to the projection, which moves the geometry by +jitter, so the sample lands at -jitter.

## P5: the conventions table

| | the layer (`Inputs`) | DLSS through Streamline (measured) | FSR 3.1.6 through FidelityFX v2.4.0 (measured) |
|:--|:--|:--|:--|
| jitter units | render pixels, offset of the sample from the pixel centre | pixels (`Constants::jitterOffset`) | pixels (`jitterOffset` in the upscale dispatch) |
| jitter sign | +x right, +y **down** | **negated on both axes**: pass `-jitter` (31.58 dB against 27.16 dB) | **negated on both axes**: pass `-jitter` (33.00 dB against 27.77 dB) |
| jitter phases | `ceil(8 * (out/render)^2)`, Halton(2,3) | at least 8 per output/render pixel area (same rule) | `GETJITTERPHASECOUNT`: 18 for 1706 to 2560 (ours 19); the offsets are identical to ours |
| motion direction | previous - current | previous - current (flipping either axis loses 8-13 dB) | previous - current (flipping either axis loses 11-13 dB) |
| motion units | stored * `motion_scale` = render pixels | `mvecScale = motion_scale / render size` puts it in [-1, 1] | `motionVectorScale = motion_scale` as is |
| motion jittered | no (unjittered projections) | `motionVectorsJittered = false` | `FFX_UPSCALE_ENABLE_MOTION_VECTORS_JITTER_CANCELLATION` off |
| motion resolution | render size | render size (Streamline sets MVLowRes from the tag's extent) | render size (no `DISPLAY_RESOLUTION_MOTION_VECTORS` flag) |
| depth | device depth, `depth_reversed` flag | `depthInverted` = `depth_reversed` | `FFX_UPSCALE_ENABLE_DEPTH_INVERTED` at creation (the backend recreates on change); infinite far is also a creation flag |
| exposure | optional 1x1 R32_FLOAT, else `pre_exposure` | tagged `kBufferTypeExposure`; untagged means DLSS auto-exposure; `preExposure` in the options | `exposure` resource, or the `FFX_UPSCALE_ENABLE_AUTO_EXPOSURE` creation flag, or neither (exposure 1.0); `preExposure` per dispatch |
| reset | true on a cut: discard the history | `Constants::reset = eTrue` for that frame | `reset = true` in the dispatch |
| HDR | `hdr` flag, linear colour | `colorBuffersHDR` | `FFX_UPSCALE_ENABLE_HIGH_DYNAMIC_RANGE` at creation (recreated on change) |
| state on return | inputs as SRV, output as UAV; root signature, pipeline and heaps NOT preserved | the host restores the command list state (`eDisableCLStateTracking`) | the same: FSR binds its own heaps |

## For the lead

1. **The registry entries** (`crates/config`, yours): `upscaler.dlss_dll` (path to `nvngx_dlss.dll`; the
   user supplies it; its folder becomes the second Streamline plugin path), `upscaler.streamline_dir`
   (folder holding `sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll` from the Streamline SDK's
   `bin/x64`; production builds, signed by NVIDIA, never committed) and **`upscaler.fidelityfx_dir`**
   (folder holding `amd_fidelityfx_loader_dx12.dll` and `amd_fidelityfx_upscaler_dx12.dll`; never
   committed). Until then `upscaletest` reads `REBUILD_DLSS_DLL`, `REBUILD_STREAMLINE_DIR`,
   `REBUILD_FIDELITYFX_DIR` (and optionally `REBUILD_SL_LOGS`). Locally: `D:\tools\dlss\nvngx_dlss.dll`
   (310.9.16), `D:\tools\dlss\sl-runtime`, and `D:\tools\fidelityfx\v2.4.0\Kits\FidelityFX\signedbin`.
2. **The FidelityFX SDK, fetched with the owner's approval ("Yes, only the needed files")**: tag
   **v2.4.0** of `GPUOpen-LibrariesAndSDKs/FidelityFX-SDK` (release "FSR SDK v2.4.0", 2026-10-28). Only
   the files below were fetched raw at the tag into `D:\tools\fidelityfx\v2.4.0`, outside git; the sums
   are also in `SHA256SUMS.txt` there. Both DLLs carry a valid Authenticode signature from Advanced
   Micro Devices.

   **Licence:** `docs/license.md` puts every file *not* listed in it under an AMD binary-only licence (no
   reverse engineering). Every file we use, both DLLs included, is in its MIT list
   (`Kits\FidelityFX\signedbin\*`, the `api\include` headers, `upscalers\include\ffx_upscale.h`). `fsr.rs` is
   written from those MIT headers and the DLLs are loaded from disk, never committed: compatible with
   GPL-3.0-only.

   | file (under `Kits/FidelityFX/`) | bytes | SHA-256 |
   |:--|--:|:--|
   | `signedbin/amd_fidelityfx_loader_dx12.dll` (2.3.0.4555) | 39,184 | `e2191d3c0828be45eb8affd4e3ec0378f2293adc345b772a7170edcf3b021ecb` |
   | `signedbin/amd_fidelityfx_upscaler_dx12.dll` (4.1.2.4555) | 57,664,272 | `aad4fccccb29084a655f9a4f9f613bd22c8c03fb4a0f8840148cc6543b4ccace` |
   | `api/include/ffx_api.h` | 10,848 | `4d9dcf243ef5efbf03340a02f16b51a22053243a22e8a9a0b3ce91c26982b4bf` |
   | `api/include/ffx_api_types.h` | 13,637 | `53f07c113a9ba493f297656e4481b6539aed0afc9f3cc8da682aa477d270c46c` |
   | `api/include/ffx_api_loader.h` | 2,977 | `18d1901dbbc881e8131b91c65ad44bb3b4a1de6c31435d5b75236f16727ba80e` |
   | `api/include/dx12/ffx_api_dx12.h` | 11,323 | `c9afbc3c4c673723e9e5369a666f0ff54560543b5a93537f8ff453bf1c3bf530` |
   | `upscalers/include/ffx_upscale.h` | 19,576 | `13091e7fe5fd9c82b1fb7c044b9f08f664d9653f599d2ea500738709236fca4d` |
   | `upscalers/fsr3/include/ffx_fsr3upscaler.h` | 39,982 | `8976d4741471790561d2d7ce73906ccff58076b92c2a4b93ae7ec6859b08334b` |
   | `docs/license.md` | 65,770 | `32357259b965d6a8001e9e508ba60eda6034027c5923a7d226ffbaff813c2bfb` |

3. **Streamline wants `presentCommon()` every frame** under manual hooking. It logs an error when no
   present is seen: "Streamline presentCommon() was not observed". `upscaletest` has no swap chain, so
   this does not matter there. In Build 7, call `slUpgradeInterface` on the layer's swap chain right after
   creating it so Streamline sees each present.
4. **The NVIDIA driver starts its NGX updater** (`nvngx_update.exe` from the driver store) during
   `slInit`/NGX init, even with OTA off and downloaded plugins off. The log says "OTA'd plugins will not
   be loaded", and they aren't. This is the driver's doing, not ours, but you may want it written down
   for the "no telemetry" line in the launcher design.
5. **FSR 4 on AMD:** the upscaler DLL is version 4.1.2, but on this card it offers only the 3.1.6 and 2.3.4
   providers. On RDNA 4 it will probably also offer FSR 4. The backend pins the newest 3.1.x on purpose
   (plan decision 5 says FSR 3.1). Whether AMD cards should prefer FSR 4 when it is offered is your call;
   it is one line in `Fsr::attach`.
6. NGX does not report its version through `slGetFeatureVersion` (0.0.0; the log has "ngxResult not
   implemented"). `Dlss::name()` now shows the DLL's file version instead: `DLSS 310.9.16.0`.

## Next

- Build 7 wiring (the lead's): feed the title's FSR constant block (jitter, vector scale) through
  `Inputs`. With both mappings measured as identical, the only per-SDK conversion left is Streamline's
  divide-by-render-size on the motion scale.
- If wanted: a reversed-Z run (`Scene { depth_reversed: true }`) and an HDR input, the two creation flags
  the synthetic runs don't exercise yet.
