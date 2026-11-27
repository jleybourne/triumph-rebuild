# Lane `ondemand`: compute programs outside the library, translated when the title creates them (Build 5)

Crates `crates/pipelines` (modules `translate`, `ondemand`; `pso::Programs` changed) and `crates/runtime`
(`pipeline.rs`). Binary `translate` now calls the library path; `pipelines ondemand` measures it. Nothing vendored;
nothing game-derived in git.

## Built

- **`rebuild_pipelines::translate`**: one console program to DXIL, the `translate v1` path as a library:
  `translate_container(bytes) -> Result<Translated, String>` (hash, stage, HLSL, DXIL, sampler words and comparison
  mask, highest user dword, `StageLayout`, time per step). Underneath: `native_of` (container to native program),
  `check_kind` (refuses what v1 refuses: undecodable, not wave-agnostic, with the reason and wave32/64),
  `lift_hlsl` (lift + assemble, v0 or v1), `compile_file` / `compile_hlsl` (DXC, `-T <stage>_6_6 -E main -O3
  -enable-16bit-types`), `layout_of` (PSV0 bindings + sampler words, refusing user dwords past the 16 root
  constants), `dxc_path` (`REBUILD_DXC`, else the newest SDK), `program_hash`.
  It lives in `crates/pipelines`, not `crates/sir`: `sir` is the lead's (section 2), what it returns is what
  this crate consumes (layouts, sidecar words), and the IR crate should not shell out to `dxc.exe`.
- **`translate` binary**: its lift/assemble/sidecar/DXC code replaced by those calls. Output checked
  byte-identical: `translate v1` before and after the change, and `D:\victory-data\translate` (117 / 117 cs).
- **`rebuild_pipelines::ondemand::OnDemand`**: `offer(hash, bytes)` (title thread: one mutex-guarded hash lookup,
  container copied the first time only) and `get(hash)` (pool worker: translate, or read the disk cache).
  Disk cache `%LOCALAPPDATA%\triumph-rebuild\pipelines\ondemand\<hash>.od` (follows `REBUILD_PSO_CACHE_DIR`):
  header, BLAKE3 of (DXC args, HLSL), BLAKE3 of the content, the container, the DXIL; written beside and renamed.
  A warm load lifts again (~0.3 ms) and compares the HLSL hash, so a translator change never serves stale DXIL
  (it recompiles and rewrites: "stale"). The file carries the container, so a pipeline prewarmed from the
  session record is created before the title offers it. Failures are kept per program with their reason.
- **`pso::Programs`**: `with_ondemand(OnDemand)`, `offer_compute(cs, bytes) -> Option<Source>`, `compute()` now
  returns `Code` (library slice or on-demand `Arc`), root signatures for on-demand programs created once per
  layout (`RootKey::Own`).
- **Runtime** (`crates/runtime/src/pipeline.rs`): the pool's `Programs` has on-demand attached;
  `create_compute` calls `offer_compute` then `request`, so missing programs go through the pool too and `host()`
  works for them. `report()` adds offered / translated / from disk / stale / failed, DXC and lift ms, and the
  first 8 failures with reasons. `MISSING` now only counts programs with no pool at all. The two
  `CreateComputePipelineState*` exports got `#[allow(non_snake_case)]` (they were the workspace's 2 warnings).

## Measured (RTX 4070 Ti, 16 logical processors; `pipelines ondemand`, empty library, corpus compute containers)

The first 300 compute containers in the corpus, as the title would create them: all requested at load, then each
waited for. 57 translate (the rest are refused, below); the warm session takes all 57 from disk.

```
title's create call      3.7-10 us each (hash the container, offer it, request): the copy happens once per program
per program, 1 worker    parse + wave class 1.9 ms (features::analyse; most of it on the 168 non-agnostic ones)
                         lift + HLSL 0.1-0.3 ms · dxc 50-64 ms (about 30 ms of it process start: a trivial
                         cs_6_6 compile is 36 ms) · cache write 0.3-0.9 ms
                         PSO 13.2 ms driver-cold, 0.75-0.92 ms when the driver's own cache is warm
per program, 12 workers  dxc 188 ms each (12 DXC processes compete for 16 threads)
all ready, cold          5.3 s on 1 worker, 1.4 s on 12 (57 programs translated + 243 refused)
all ready, warm          0.72 s on 1 worker, 0.16 s on 12: no DXC call; cache read 0.07 ms a program
identity                 translate v1 before and after the change: all 22,200 outputs byte-identical (hlsl,
                         samplers, dxil); 6 PS that DXC died on in the after-run (no error line) compiled
                         byte-identical when run again, so `compile_file` now retries such a failure once
tests                    workspace 450 passed, 0 failed, 11 ignored, 0 warnings (cargo test --release)
```

For the title's 80 BVH programs, expect about 80 x (55 ms DXC + 10 ms PSO) / 12 workers, or about 0.5 s
spread over load in the first session, and well under 0.1 s after that.

## Tests

- `translate::a_container_translates_to_what_translate_v1_writes`: 4 corpus compute containers through
  `translate_container_with`: DXIL and sampler sidecar byte-identical to `D:\victory-data\translate`, root checked.
- `pso::programs_outside_the_library_are_translated_on_demand` (GPU, skips under `agents\MEASURING`): 3 corpus
  programs absent from a 2-program library, offered + requested through the pool, PSOs on the device; a second
  session (prewarm, nothing offered) takes all 3 from disk with 0 DXC calls.
- `ondemand`: cache file round trip and damage; never-offered and junk programs fail with their reason, once.

## For the lead

1. **What the corpus says about kind**: of the first 300 compute containers in `shaders.rpf`, 168 are refused as
   not wave-agnostic (88 `wave-wide`, 80 `wave-wide-if-rfl-divergent`) and 75 by untranslated opcodes (most:
   packed f16 `v_pk_add/mul/max_f16` 27, `v_perm_b32` 6, `ds_max/min/or_u32`, `s_getpc_b64`, `v_add/sub_co_ci_u32`).
   Only 57 (19 %) translate. A BVH builder (LDS reductions, wave ops, packed f16 boxes) is likely to fall in the
   refused groups. The run report now prints each on-demand failure's
   reason (wave class, wave32/64, first trigger); a capture of the 80 BVH containers would tell us exactly.
2. A `wait` on an on-demand pipeline no worker has started translates on the waiting thread (the pool's rule
   for every pipeline). Requests are made at load, so this needs a dispatch within ~0.1 s of creation.
3. Possible follow-up: DXC in-process (`dxcompiler.dll`, `IDxcCompiler3`) would save the ~30 ms process start
   of ~50-75 ms per program; only on a cold cache, so not done.
4. Refusals are not cached on disk (a translator change must be able to lift them), so each session pays the
   wave-class check again: 1.9 ms a program on one worker, off the title's thread.
5. Measurement lock: `agents\MEASURING` appeared twice while this lane worked. Once it appeared during a build and
   the GPU bench refused to start, as it should. Once it showed up just before a GPU test that had already been
   launched. That test checks the lock itself and still ran, so the lock was probably gone again by then, but
   one of your timed runs may have overlapped this lane's build. Please treat that run's numbers with care.
6. Earlier proposal stands: the sidecar's optional top-dword word; `Hlsl::top_dword` now has it ready.
