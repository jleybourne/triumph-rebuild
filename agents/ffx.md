# Lane `ffx`: the title's FidelityFX API, served by us (Build 7 groundwork)

Crate `crates/ffx` (package `rebuild-ffx`). Research: [`docs/research/ffx.md`](../docs/research/ffx.md).
Measured on the dev machine (RTX 4070 Ti), 2026-11-27, `agents\MEASURING` absent, `agents\CARD` held.

## Found

- The title's exe imports **the FFX API** (`ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`,
  `ffxQuery`) from `amd_fidelityfx_loader_xbox.dll` 1.0.2.0. It asks for **upscaler API 4.1.1**; every
  descriptor has the PC SDK v2.4.0 layout. The loader loads `amd_fidelityfx_upscaler_xbox.dll` 3.1.5.0,
  which is **FSR 4 (int8/fp8 model) for Xbox**. It builds its pipelines through
  `DeserializeComputePipelineStateX` = device slot **0x218**, which fails on purpose, so today the title's
  upscale has no pipelines.
- The shipping DLL takes one configuration: output 1920x1080, render 1280x720, HDR + inverted depth + auto
  exposure. Whatever the title asks of us, we serve.
- Title code: one wrapper (create exe+0x28ef1d0, jitter exe+0x28ef560, dispatch exe+0x28ef700, destroy
  exe+0x28ef470). Per frame: one jitter query and one dispatch. Jitter into the dispatch is the negated
  query value. The motion-vector scale is (1, 1). The command list is the render context's console list.

## Built

| piece | state | the number |
|:--|:--|:--|
| the FFX API, our implementation (`ffxCreateContext/Destroy/Configure/Query/Dispatch`) | done | 8 unit tests: the title's creation chain, its dispatch translated (jitter negated, mode by ratio, flags), refusals, FSR's phase count and offsets, bad contexts refused, every name exported |
| served as console DLLs | done | `CONSOLE_DLLS` + `console_export` (both DLL names); `title.rs` does not map the title's two DLLs (`REBUILD_FFX=title` maps them again); runtime test `the_fidelityfx_dlls_are_ours` |
| dispatch → `crates/upscale` | done | console resources via `Resources::host_resource`; the upscale and the barriers (title's states → SRV/UAV → back) go into the console list's open host list (`xlist::console::segments(list).last()`) |
| backends | done | DLSS when Streamline attaches (NVIDIA), else FSR 3.1; `prepare_dlss()` runs before the device is created (`device.rs`); `install_xbox()` runs once the resources exist |
| typeless host textures | done | `rebuild_upscale::typed()`: both backends name the typed format (d3d12x makes the title's textures typeless on the host) |
| synthetic FFX client, end to end on the card | done | `crates/ffx/tests/scene.rs`, below |

**`tests/scene.rs`**: a client built like the title's wrapper. It creates with upscale + backend
`0x01000002` + version 4.1.1, queries the phase count and one jitter offset per frame, and dispatches with
the jitter negated, MV scale (1, 1), inputs `PIXEL_COMPUTE_READ` and the output resting in `COPY_SRC`. The
`rebuild-upscale` synthetic scene runs 1280x720 → 1920x1080, 32 frames, quality on the last 8:

| backend through `ffxDispatch` | moving PSNR (bilinear) | static PSNR (bilinear) | static frame-to-frame (bilinear) | NaN | black frames |
|:--|--:|--:|--:|--:|--:|
| DLSS 310.9.16 (Streamline 2.14.16) | 27.83 dB (23.39) | 31.76 dB (26.61) | 0.431 (6.330) | 0 | 0 |
| FSR 3.1.6 (SDK v2.4.0) | 29.15 dB (23.39) | 32.80 dB (26.61) | 0.175 (6.330) | 0 | 0 |

128 dispatches, 128 recorded, 0 refused, 0 contexts left. It runs only with a backend configured
(`REBUILD_FIDELITYFX_DIR`, `REBUILD_STREAMLINE_DIR` + `REBUILD_DLSS_DLL`) and the card lock; otherwise it
says "skipped" and passes. Run it with:
`cargo test --release -p rebuild-ffx --test scene -- --nocapture`.

## For the lead

1. **The state after the upscale: done in xlist, at your request.** The upscale binds its own descriptor
   heaps, root signature and pipeline on the host list. `rebuild_xlist::console::forget_bound_state(list)`
   (`list.rs`) runs after it in `XboxHost::record`:
   - It clears the elision shadow, so the title's re-binds after `ffxDispatch` are applied.
   - It binds the title's descriptor heaps again at once, so its tables keep mapping.
   - It marks the pipeline unbound. A draw before the title sets one is dropped, not run with FSR's or
     DLSS's pipeline.
   - Test: `after_outside_commands_the_titles_rebinds_are_applied_not_elided`.
2. **The runtime edits are small.** `exports.rs`: two names in `CONSOLE_DLLS`, one match arm,
   `replaces_title_dll`, one test. `title.rs`: skip those two files. `device.rs`: `prepare_dlss()` before
   `D3D12CreateDevice`, and `install_xbox(resources, &host)` after the resources. `Cargo.toml`: the member
   and the dependency.
3. **The registry**: the backends read `REBUILD_FIDELITYFX_DIR`, `REBUILD_STREAMLINE_DIR`,
   `REBUILD_DLSS_DLL` and `REBUILD_UPSCALER=dlss|fsr` (the same names as `upscaletest`). Map them to
   `upscaler.*` when the launcher sets them.
4. **Inputs the `Upscaler` interface does not carry yet**: reactive and transparency masks, sharpening,
   auto exposure, display-resolution and jittered motion vectors. They are logged once if the title sends
   them. Auto exposure matters if the title follows its DLL's rule (HDR + auto exposure). Adding
   `Inputs::auto_exposure` (FSR: a creation flag; DLSS: leave exposure untagged) is a small change in
   `crates/upscale`.
5. **Streamline wants `presentCommon()` every frame** (logged in the test, which has no swap chain). This
   is the upscale lane's note 3, now relevant: lane `present` should call `slUpgradeInterface` on the swap
   chain.

## What I need from the next title run

Run with `REBUILD_FIDELITYFX_DIR` set (and the DLSS pair if you want DLSS), then send the stderr lines that
start with `ffx:`:

1. `ffx: the title's FidelityFX calls are served by rebuild-ffx (...)`: the host installed.
2. `not mapped: amd_fidelityfx_loader_xbox.dll`: the takeover happened.
3. **`ffx: context: flags ..., max render ..., max upscale ..., api 4.1.1 ...`**: the sizes and flags the
   title asks for, and which backend it got. This answers whether it creates at all here, and at what
   resolution.
4. **`ffx: first dispatch: ...`**: the descriptor in full: list (is it one of our console lists), every
   resource's size, format and state, jitter, MV scale, render → output, sharpening, reset, near/far/fov,
   flags.
5. any `ffx: ffxDispatch: ...` refusal (a resource without a host resource, or a list that is not ours).
6. `rebuild_ffx::stats()` at exit, if the frame line can carry it (dispatches and recorded should match).
7. The device trap count for **0x218** with ours in place. It should drop by the FSR 4 pipeline creations.

## Next

- the first frame with the upscale in place;
- auto exposure in `Inputs`; reactive/T&C into FSR (DLSS has no such inputs);
- then lowering the internal resolution goes through the title's own FSR quality setting, since the
  render size now reaches us as a number rather than as texture sizes.
