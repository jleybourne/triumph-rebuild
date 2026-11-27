# Lane `upscale`: the DLSS and FSR backends, standalone (Build 7 piece)

Crate `crates/upscale` (package `rebuild-upscale`), binary `apps/rebuild/src/bin/upscaletest.rs`.
Measured on the dev machine (RTX 4070 Ti), 2026-11-27, with `agents\MEASURING` absent.

| # | task | state | the number |
|:--:|:--|:--|:--|
| P1 | one interface (`Upscaler`, `Inputs`) | done | 4 unit tests: Halton, jitter phases, render sizes per mode, input checks |
| P2 | synthetic scene: moving geometry, Halton jitter, motion vectors, depth, supersampled truth | done | every motion vector followed back lands on the same surface point, worst error < 1e-4 (more than 98 % of pixels; the rest were hidden the frame before); background motion = camera pan exactly |
| P3 | FSR 3.1 through the FidelityFX SDK | blocked | the SDK is not on disk; it has to be downloaded (see "for the lead") |
| P4 | DLSS through Streamline | done | the table below; 0 NaN, 0 black frames; beats bilinear on every check |
| P5 | the conventions table | done for DLSS (measured), FSR column from the SDK docs, to be confirmed by the same sweep once P3 runs | below |

11 unit tests in `rebuild-upscale`; the workspace stays green.

## Built

- **`Upscaler` trait and `Inputs`** (`crates/upscale/src/lib.rs`): colour, depth, motion, optional exposure,
  output, render size, output size, jitter, motion scale, depth reversed, reset, HDR, pre-exposure, frame
  time, near/far/fov. **One convention, the layer's** (below); each backend converts inside `evaluate`.
  `Inputs::check` refuses what makes an SDK silently produce garbage (render larger than output, zero size,
  jitter outside +/-0.5, non-finite values).
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
- **`upscaletest`**: per mode, the SDK's render size range; a 64-frame moving run and a 64-frame static run
  from a reset; PNGs (input, bilinear, DLSS, truth, a 2x side-by-side crop) to `--out`, outside git;
  `--sweep` tries all 16 sign combinations of jitter and motion; then 200 back-to-back evaluations for the
  GPU time.

## Measured

`upscaletest` at 2560x1440 output, 64 frames, PSNR and stability on the last 8. The raw output is in
`D:\victory-data\upscale-out\{quality,performance,sweep}.txt` and the PNGs are beside it (outside git).

| mode (render) | scene | PSNR DLSS | PSNR bilinear | static frame-to-frame (mean abs diff, 0-255) | GPU time of the upscale |
|:--|:--|--:|--:|--:|--:|
| quality (1707x960) | moving | **30.96 dB** | 26.50 dB | n/a (the scene moves) | **0.981 ms** median, 0.987 p90 |
| quality (1707x960) | static | **33.84 dB** | 26.14 dB | **0.284** (bilinear 6.139) | |
| performance (1280x720) | moving | **31.30 dB** | 24.76 dB | n/a | **1.147 ms** median, 1.151 p90 |
| performance (1280x720) | static | **34.73 dB** | 25.33 dB | **0.590** (bilinear 6.104) | |

NaN pixels 0 and black frames 0 in every run. GPU time comes from timestamps around `slEvaluateFeature`,
200 evaluations back to back so the card stays clocked up. Timing inside the CPU-rendered sequence read
18-31 ms because the card idles and clocks down between frames, so that number is not reported.
Performance mode costs more than quality because DLSS 310 picks a different default preset per mode.

Render sizes Streamline gives at 2560x1440: DLAA 2560x1440, quality 1707x960, balanced 1485x835,
performance 1280x720, ultra-performance 853x480. Use the SDK's number, not the ratio rule. Balanced
differs most: 1485x835 against 1505x847.

**The sign sweep** (quality, moving, 24 frames, PSNR on the last 8):

| jitter x | jitter y | motion x | motion y | PSNR |
|:--:|:--:|:--:|:--:|--:|
| + | + | + | + | 27.16 dB |
| - | + | + | + | 27.98 dB |
| + | - | + | + | 29.87 dB |
| **-** | **-** | **+** | **+** | **31.58 dB** |
| any | any | - on either axis | | 18.3-19.8 dB |

So in the layer's convention, **Streamline's `jitterOffset` is the negated sample offset on both axes**.
Motion vectors pass through unchanged (previous - current, +y down, scaled to [-1, 1] divided by render size).
`Signs::STREAMLINE` is that mapping and is the default.

## P5: the conventions table

| | the layer (`Inputs`) | DLSS through Streamline (measured) | FSR 3.1 through FidelityFX (SDK docs; confirm with the sweep) |
|:--|:--|:--|:--|
| jitter units | render pixels, offset of the sample from the pixel centre | pixels (`Constants::jitterOffset`) | pixels (`jitterOffset` in the upscale dispatch) |
| jitter sign | +x right, +y **down** | **negated on both axes**: pass `-jitter` (sweep: 31.58 dB against 27.16 dB) | FSR's samples apply `+2*jx/W` and `-2*jy/H` to the projection and pass the same `jx, jy`. To be measured |
| jitter phases | `ceil(8 * (out/render)^2)`, Halton(2,3) | at least 8 per output/render pixel area (same rule) | `ffxFsr3GetJitterPhaseCount`, same formula |
| motion direction | previous - current | previous - current (flipping either axis loses 8-13 dB) | previous - current |
| motion units | stored * `motion_scale` = render pixels | `mvecScale = motion_scale / render size` puts it in [-1, 1] | `motionVectorScale` = what turns stored values into render pixels (`motion_scale` as is) |
| motion jittered | no (unjittered projections) | `motionVectorsJittered = false` | jitter cancellation off (the default) |
| motion resolution | render size | render size (Streamline sets MVLowRes from the tag's extent) | render size (no `DISPLAY_RESOLUTION_MOTION_VECTORS` flag) |
| depth | device depth, `depth_reversed` flag | `depthInverted` = `depth_reversed` | `FFX_FSR3_ENABLE_DEPTH_INVERTED` at context creation; infinite far is also a creation flag |
| exposure | optional 1x1 R32_FLOAT, else `pre_exposure` | tagged `kBufferTypeExposure`; untagged means DLSS auto-exposure; `preExposure` in the options | `exposure` resource or the `AUTO_EXPOSURE` creation flag; `preExposure` per dispatch |
| reset | true on a cut: discard the history | `Constants::reset = eTrue` for that frame | `reset = true` in the dispatch |
| HDR | `hdr` flag, linear colour | `colorBuffersHDR` | `FFX_FSR3_ENABLE_HIGH_DYNAMIC_RANGE` at creation |
| state on return | inputs as SRV, output as UAV; root signature, pipeline and heaps NOT preserved | the host restores the command list state (`eDisableCLStateTracking`) | the same: FSR binds its own heaps |

## For the lead

1. **The registry entries** (`crates/config`, yours): `upscaler.dlss_dll` (path to `nvngx_dlss.dll`; the
   user supplies it; its folder becomes the second Streamline plugin path) and
   `upscaler.streamline_dir` (folder holding `sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll` from the
   Streamline SDK's `bin/x64`; production builds, signed by NVIDIA, never committed). Until then
   `upscaletest` reads `REBUILD_DLSS_DLL` and `REBUILD_STREAMLINE_DIR` (and optionally `REBUILD_SL_LOGS`).
   Locally: `D:\tools\dlss\nvngx_dlss.dll` (310.9.16) and `D:\tools\dlss\sl-runtime` (copied from the SDK
   zip, extracted to `D:\tools\dlss\sl-2.14.16`).
2. **The FidelityFX SDK is not on disk.** P3 needs it downloaded: AMD's FidelityFX SDK (MIT) from GPUOpen
   / GitHub `GPUOpen-LibrariesAndSDKs/FidelityFX-SDK`, the FSR 3.1 path, prebuilt
   `amd_fidelityfx_dx12.dll` (the loader, kept outside git like the DLSS DLL) plus its MIT headers. I did
   not download it, as told. Once it is there, P3 is the same backend shape as `dlss.rs`, and `--sweep`
   settles the FSR column above.
3. **Streamline wants `presentCommon()` every frame** under manual hooking. It logs an error when no
   present is seen: "Streamline presentCommon() was not observed". `upscaletest` has no swap chain, so
   this does not matter there. In Build 7, call `slUpgradeInterface` on the layer's swap chain right after
   creating it so Streamline sees each present.
4. **The NVIDIA driver starts its NGX updater** (`nvngx_update.exe` from the driver store) during
   `slInit`/NGX init, even with OTA off and downloaded plugins off. The log says "OTA'd plugins will not
   be loaded", and they aren't. This is the driver's doing, not ours, but you may want it written down
   for the "no telemetry" line in the launcher design.
5. NGX did not report its version through `slGetFeatureVersion` (it reads 0.0.0, and the log has
   "ngxResult not implemented"). For the frame line, read the DLL's file version (310.9.16 here) instead.

## Next

- P3 once the FidelityFX SDK is on disk; then DLSS and FSR side by side (`--backend fsr`, the same scene and
  PNGs), and `--sweep` for FSR's signs.
- Build 7 wiring (the lead's): feed the title's FSR constant block (jitter, vector scale) through
  `Inputs`. Then the conventions above are the only conversion left.
