# claude-max-handoff.md — the lead's working state, for the session that takes over

Updated by the lead every step. Second lead session (2026-11-27), on the owner's other account, same PC.
Read this first, then `secondary-agent.md` §8-10 (project state, the owner's rules and step order, answers to
helpers).

## SAFETY FIRST (2026-11-27)

`runtitle --run` took the owner's PC down three times (reboots). Root causes found and fixed (commits df6d3a9,
2a2af01; progress.md): uncapped physical-page commit, and `VirtualAlloc2` address requirements + write-watch
spinning in the kernel on a full 4 GiB window (unkillable process). **Never run the title without the owner's
explicit OK for that run.** Test memory patterns with `exitprobe` (no title, capped children) instead.

## Where things stand (one screen)

```
progress    53 % toward 30 fps (progress.md)       measured fps: none yet (no title frame)
built       Builds 0 (a, b), 1, 2 closed; Build 3 ~88 %; Build 4 core proven; Build 7 DLSS + FSR standalone
title       REACHES D3D12XboxCreateDevice in 1.4 s (runs/build3-reached.md): Build 3 closed. Title runs are OK
            with the owner (guarded: job, 12 GiB budget, watchdog); TLS slot 0 is the title's (runtime/src/tls.rs)
runtime     GDK runtime (xgameruntime.dll) ours via LoadLibraryExA override; files 32 + CRT 13 bound
lanes       round 1 and 2 merged (#1-#18); round 3 running: gdk G5, dstorage D5, device-research, shader S7 (agents/secondary-agents-updates.md)
tests       311 green (one files F5 host-comparison test 46 vs 45 since the reboot: for the files lane)
translate   `translate ps` over D:\victory-data\shaders.rpf: v0 set 2,834 / 2,881 compile (PS 1,791, VS 926, CS 117);
            immediate samplers decode to static samplers (sir::sampler, space100: Build 5 must add them to
            the root signature); next: ten single forms, then vertex shaders (s_swappc fetch, exp pos/param)
```

## Who you are and how the owner works

You are the **lead** of `D:\Developer\triumph-rebuild` (GitHub `jleybourne/triumph-rebuild`, branch `master`).
The owner's rules, in their own words:

- "keep building until we have no tasks to do" and "you shouldn't stop ever". Watch token usage, though: the
  owner asked to slow down near limits.
- "just do what is best for us I want it to perform the best": decide technical questions yourself, record
  them in `plan.md` and `tasks.md`; ask only owner-only items (`secondary-agent.md` §9).
- Every push gets a new dated entry at the top of `progress.md` "Updates", with its own bar and notes. Also
  update the README bar and badges, `tasks.md`, `runs/README.md`, `secondary-agent.md` §8, and **this file**.
- Commit to `master` and push. Helper agents work in lanes and send pull requests. Before every step, read
  `agents/secondary-agents-updates.md`, merge open PRs first (review: no game bytes or paths, no NVIDIA or AMD
  binaries, lane boundaries kept), and answer "For the lead" items in `secondary-agent.md` §10.

## How to work in this repo

- **Shell:** Git Bash. Run multi-file doc edits as a Python script in the scratchpad, with
  `export PYTHONIOENCODING=utf-8` first: printing a `█` under cp1252 crashed a script mid-edit once.
- **Windows paths in Rust:** use raw strings (`r"G:\title.exe"`). Edit them with the Edit tool, not
  sed or Python; `"G:\t…"` silently becomes a tab.
- **Measurement lock:** `touch agents/MEASURING` before any GPU or timed run, and `rm -f agents/MEASURING` after.
- **Merging lane PRs:** the root `Cargo.toml` `members` line and `apps/rebuild/Cargo.toml` always conflict. Take
  the union of members, keeping `"apps/rebuild"` last; take both dependency lines; `git checkout --theirs
  Cargo.lock`, then build to regenerate it.
- **The title's folder** is read-only, through `REBUILD_GAME_DIR` (the path is `game_dir` in
  `D:\Developer\fxceless-victory\victory.toml`). Never commit game bytes or that path. Prepared image copies live
  in `%LOCALAPPDATA%\triumph-rebuild\images`.
- **Shader corpus copy:** `D:\victory-data\shaders.rpf` (SHA-256 `0BC1634594E51CA8E7EF5F2BC6F5B32B081CE779AC094D686DE08ECB75DC24A8`),
  copied once with the owner's OK. All shader work reads this copy, never the game folder.
- **Game folder:** read-only at the file-system level since 2026-11-27 (`attrib +R /S`, all 110 files; a test
  write is refused). `data\ttdb\ui\ttdb_ui.ttpf` was re-dumped by the owner after a helper overwrote it:
  845,115,648 bytes, SHA-256 `DC983ED7F6B78D1693FA5757F17C03BAFB790E851594FDF4D412579B61D867F6`.
- **Minimum spec = the dev machine** (`plan.md`): 3840x2160 output, DLSS Quality. **AMD testing deferred** by the owner.
- **The fork** (design and data only): `git -C D:\Developer\fxceless-victory show triumph/main:<path>`. GPU
  design: `crates/victory-gpu/src/*.rs`; loader: `crates/victory-loader/src/*.rs`; console DLLs:
  `crates/victory-hle/src/modules/*.rs`; findings: `CLAUDE.md`, `ROADBLOCK.md`, `docs/`.

## Next actions, in order (start here)

1. **Find what the title's main thread waits on.** See `runs/build3-startup.md`: the entry point runs 60 s, calls
   no unbuilt import, and never reaches `D3D12XboxCreateDevice`.
   - In `runtitle`, add a sampler: every ~2 s `SuspendThread` `title-main`, then `GetThreadContext` for its RIP.
     Walk the stack with `RtlLookupFunctionEntry` + `RtlVirtualUnwind`; Windows registered the unwind tables
     because the images are prepared copies. `ResumeThread`.
   - Map each address to (image, RVA) through `title::LoadedTitle.images` bases, and print the top frames.
     Also list the title's other threads (`CreateToolhelp32Snapshot`).
   - Suspects: a wait on an event that an unbuilt GDK piece would set; the job scheduler; a `GetProcAddress`
     the title makes through the **host's** `kernel32` that bypasses our override (check whether its IAT slot
     for `GetProcAddress` really points at `modules::GetProcAddress`); the hang watchdog (the fork: 30
     one-second sleeps without a frame).
   - Close it by reaching `D3D12XboxCreateDevice` ("REACHED" in `runtitle`).
2. **Build 5, the D3D12.X device**, where `D3D12XboxCreateDevice` lands (`crates/runtime/src/exports.rs`,
   `on_create_device`). Facts from the fork (design):
   - Console graphics objects begin with a vtable pointer, bookkeeping bits and a refcount. **Title code reads
     object fields directly** (inlined), so the memory layout matters, not only the vtable.
   - Device vtable: IUnknown, object methods, device methods in PC order, then the console's additions.
     **Unwritten slots log their offset and fail**, so every run names the next method to write.
   - Command lists: the title writes packets inline while a list's put pointer is below its limit, and calls
     the list method otherwise. **Set both to 0** and every command arrives as a call: `crates/xlist` translates
     those (0.43 µs a draw, measured).
   - Wire in: our tiles (Build 1) for GPU-owned resources, the ring (Build 2) for
     `exports::graphics_regions()`, labels into title memory, physical pages read in place, `sir` for shaders,
     and the frame line. Then Build 0 (c), the title's GPU time.
3. **Shaders (in progress, the work while the title waits):** `translate ps` over the corpus copy gives
   1,360 / 1,798 v0 pixel shaders translated and compiled (`runs/build4-ps.md`). Next, by blocker count:
   `image_sample` with cube maps or 16-bit/NSA forms, `v_interp_p1ll_f16` (16-bit inputs), `s_brev_b32`,
   `image_store`, `ds_swizzle_b32`, root descriptors (`s_load` from a 64-bit address, the fork's root CBV/SRV
   space). Then vertex shaders (`s_swappc_b64` fetch, `exp pos/param`), compute (thread ids, LDS), the S6
   `oraclediff` interface check, and the GPU execution check that makes programs count. Use
   `REBUILD_TRANSLATE_DUMP=<reason>` to write refused programs' lifted bodies and root layouts.
0. ✅ **Done (2026-11-27): the merged crates are bound in `crates/runtime`:**
   - `rebuild_winfiles`: all 22 overrides (`install(Files::new(vfs, r"G:\"))`, `override_export(name)` in
     `HostResolver`), with the 5 extra (`FindNextFileW/A`, `FindClose`, `SetFileAttributesW`, `GetTempPathW`)
   - `rebuild_audio` (19), `rebuild_dstorage` (`DStorageGetFactory`, `Pages` over `physical` with a retry),
     `rebuild_xg` (13-slot vtable, slot 0x30 → 0 outside) → `exports::console_export`
   - `crates/config`: keep explicit values and unknown keys, add `schema`, the launcher's registry entries
     (`secondary-agent.md` §10)
4. Merge round-2 helper PRs as they arrive (gdk, audio A5, files F4-F5, shader S6, launcher U6) and bind them.
   `dstorage` D5 (BCPack decoder, tables outside git, decode cache; owner-approved) runs as the lead's own
   background agent in `D:\Developer\triumph-rebuild-dstorage`, branch `agent/dstorage-bcpack`; it opens a PR.

## Open threads

- `XMemGetWorkingSetStatistics`: refused until its layout is known (from a run).
- Threads the title creates get no `DLL_THREAD_ATTACH` for title DLLs (their entries are cleared in the prepared
  copies). Fix it if the sampler shows it matters (the fork passes thread start and end on).
- Helper lanes: running `shader`, `launcher`. Waiting for the coordinator to start them: `audio`, `dstorage`,
  `files`, `xg`, loader L5, upscale P3 (FSR; SDK download approved).
- Build 5: direct GPU read of title memory vs the ring, per resource, by the frame line. Inline-packet parsing
  vs calls, by the frame line.
- "Second title" check once the first title renders.
- Owner-only: the minimum spec in `plan.md`; AMD testers; pushing the fork's `dlss-hook` branch.

## Where everything is

| what | where |
|:--|:--|
| state, rules, step order, answers to helpers | `secondary-agent.md` (§8 state, §9 lead, §10 answers) |
| per-upload history with bars | `progress.md` |
| every task and its closing number | `tasks.md` |
| measurements | `runs/` (write-up `.md` + raw `.txt`; index `runs/README.md`) |
| designs | `docs/design/shader-ir.md`, `docs/design/launcher.md` |
| the title loader and runtime | `crates/loader` (`pe`, `map`, `prepare`), `crates/runtime` (`title`, `modules`, `exports`, `xmem`, `physical`) |
| tools | `apps/rebuild/src/bin/`: `runtitle`, `maptitle`, `runtimecensus`, `gputest`, `sirtest`, `uploadbench`, `listbench` |
| helper status (coordinator's) | `agents/secondary-agents-updates.md` |
| Claude memory for this project | `C:\Users\Jayde\.claude\projects\D--Developer-triumph-rebuild\memory\` |

## In flight (2026-11-27, usage limit)

- `translate ps|vs|cs` all wired (runs/build4-{ps,vs,cs}.md). Next: the execution check against the oracle, then control flow.
- Vertex shaders: 926 / 951. Descriptor shadow table (plan
  decision 9) must be written by Build 5's descriptor code.
- Owner request, hardware limits: built (limits.rs, config limits.*, launcher env); left: VRAM in Build 5 heaps,
  worker pools sized to the core limit, a capped proof run (owner OK).
- The title has not been run since the kernel-spin fix (2a2af01); ask the owner before any run.
- oraclediff on our v0 DXIL (2,834 programs, D:\victory-data\translate-dxil as <hash>.dxil, CSV
  D:\victory-data\oraclediff-v0.csv): 2,829 "differ", but OUR side shows 0 dx.op calls (memory/wave/io all 0):
  crates/shader/src/bitcode.rs does not recognise dx.op calls in DXC 1.8 output (only checked against the oracle).
  FIXED (Char6 names). Interface diffs now SigIn 35, SigOut 3, Bindings 4 (runs/build4-oraclediff.md);
  read each with `oraclediff --sig`, fix ours or name the reason, then the v0 row closes.
- Build 5 started (lead): D3D12ConfigureVirtualMemoryX fixed (one pointer; fills lowest = 4 GiB, highest = top of
  user space). The title's device request, logged: riid 5405c344-d457-444e-b4dd-2366e45aee39 (ID3D12Device11),
  params [00020004, 0, 00100000, 0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, 00000020]. Next: the static device object
  (0x58 bytes, byte +0x18 = 1), slots per docs/research/d3d12x-device.md section 6, every other slot a trap.
- S7 merged (#21): v0 interface diffs vs the oracle = 1 (translator: void main for PS with no outputs) + the fp16
  component bug, both sent to lane shader-cf (it owns crates/sir for now). Oracle: 4 swapped files fixed in
  D:\victory-data\rebuild-oracle (backup in oracle-swapped-backup); `apps/rebuild/src/bin/oracle.rs` must settle
  same-timestamp cache entries like `oraclediff --ties` before it is run again; agents/shader-oracle.csv sizes of
  those 4 rows to swap (agents/shader.md item 3). Build 5 rule from S7 item 5: the runtime mirrors every buffer
  descriptor the title writes as a RAW view too (a translated program may read any slot as ByteAddressBuffer).
- Merged today after round 3: #21 S7, #22 pipelines (82/82 root signatures, 117 CS + 1,374 graphics PSOs, library
  load ~5 ms, cold PSO 2.2-7.6 ms each -> create on worker threads + ID3D12PipelineLibrary next), #23 built-in test
  profiles (9) + launcher picker + matrix writer, #24 BCPack decoder (bit-exact vs fork, 1.7-3.0x faster) + cache.
  Still running: shader-cf (v1 control flow + 2 translator bugs + sampler sidecar), cmdlist. 360 tests green.
  Open for the owner: D5b needs real BCPack data (capture during a title run, or a helper reads packages read-only).
  Launcher redesign mockups (dark/light, 7 screens + top-bar variant): https://claude.ai/artifact/C4JarmJgfw9jAdFdGnJZLY
- #25 cmdlist merged: `rebuild_xlist::console::{create_allocator, create_list, segments, host_list, traps}`; the lead
  implements `ListHost` (pipeline, resource, descriptor_heap, cpu_descriptor, memory; optional label, memory_wait).
  Submit with segments(l): each segment's waits -> queue Wait on the label fence, then execute. Host resources must
  start in the state the title believes. 0x4a0 memory belongs to the allocator (freed at allocator Reset).
  Still running: shader-cf, launcher L6. Next for the lead: the device object + queue + fence + ListHost impl.
- #26 shader-cf merged: v1 7,402 / 9,128 agnostic compile (runs/build4-v1.md); crates/sir is the lead's again.
  Next shader items: VS s_buffer_load from code-built 64-bit descriptors (638, window addressing), then the list in
  runs/build4-v1.md. Gather4 now guards inactive lanes (lead).
- #27 pipelines-2 merged (Pool::request/get/wait, PsoCache, prewarm; 12 workers: 1,491 cold PSOs in 0.6 s). #28 launcher
  L6 merged (top bar, themes, session folders; contract in docs/design/launcher.md "L6 built": REBUILD_SESSION_DIR,
  frame-line.log `key value | ...` fields fps ms gpu draws dropped upload xlate upscale wait + resolution segment;
  summary.toml [summary] avg_fps low1_fps frame_ms gpu_ms upload_ms frames seconds measured_from; capture.request).
  BCPack: decoder bound, capture via REBUILD_BCPACK_CAPTURE (D5b in tasks.md). Running: resources. 406 tests green.

## 2026-11-27 evening — Build 5 device setup passes (aabacde)
- Built in crates/runtime: device.rs (static 0x58 object, +0x18=1, unwritten slots = timed traps), queue.rs (incl. X queues),
  fence.rs (value cell at obj+0x10 + host fence + watcher), dxgi.rs (console-shaped device/adapter/output, host mode),
  frame.rs (simple clock), cpu.rs (VEH NOPs `clwb` per site on first fault; host CPU lacks it).
- xmem: base-0 commit adds MEM_RESERVE; placeholders get no write-watch; 2M/64K + REPLACE_PLACEHOLDER at a base keeps a
  placeholder; free with PRESERVE_PLACEHOLDER keeps the region; failures logged.
- Guarded run: 1 queue, 5 fences, 4 X-queues, 439 allocators, 581 lists, DXGI, 2 frame events, 1 clwb site; then
  int3 assert at exe+0x247af2d (arena: end-start+[0x1475E6FF0] <= 0x1FFFFF; arena size 0x4000000 or 0x5200000).
- Tests: 437 passed, 0 failed. Next: find which allocation misses the arena; then queue Execute/PresentX, pipelines.

## 2026-11-27 night — render setup passes; root signatures; BVH builder needs compute pipelines
- Arena assert exe+0x247af2d solved: render setup pushes an XMem scope for allocator 0x80 (D3D) via 0x145C8F990;
  the title's XMem hook dispatcher (0x144DEB430, list = attrs bits 16-23) serves page-size-2 allocations from the
  arena (callback 0x1456A2A60) and asserts it ends full. Fix: device.rs `Accounted` Memory: each committed
  resource/heap also XMemAlloc(size, D3D_ATTRIBUTES=(0x80<<16)|(2<<14)), XMemFree on release.
- xboxbvhbuilder.dll then threw HRESULTs (C++ `long`): (1) host serializer rejected console root flags
  0x3000000 -> exports.rs wraps D3D12SerializeVersionedRootSignature masking to 0xfff; (2) device 0x80 ->
  crates/d3d12x/src/root.rs (Object::RootSignature, host ID3D12RootSignature, ListHost::root_signature);
  (3) NOW: device 0x1F0 CreateComputePipelineStateX(this, *D3D12_COMPUTE_PIPELINE_STATE_DESC, 0, 0, iid, out)
  -> E_NOTIMPL thrown from xboxbvhbuilder+0xc525. Next: implement 0x1f0/0x58 via crates/pipelines + translator.
- Diagnostics: crash.rs reports C++ throws (type + value + unwound chain), REBUILD_PEEK=addr,...,rsp+N,
  REBUILD_THREADS=1 (all threads unwound at a fatal). Scratch tool: xref/xrange.py (RIP-relative refs).
- Tests 439 green. Progress 56 %.

## 2026-11-27 night (later) — pipelines, query heaps, command signatures; the title loads content
- runtime/src/pipeline.rs: 0x58/0x1f0 compute pipelines -> Pool<PsoCreator> (library D:\victory-data\pipelines\library.bin,
  rebuilt in format 2 with `REBUILD_PIPELINES_IN=D:\victory-data\translate pipelines build`); hash = BLAKE3(container)[..8] BE.
  Programs not in the library (BVH builder's 80) -> object with no host PSO; xlist drops draws/dispatches after it (skipped).
- d3d12x: Object::QueryHeap (0x138), Object::CommandSignature (0x148, console arg type 0x69 refused by host -> no host).
- cpu.rs: clzero detour (int3-padding cave + stub); heap.rs: descriptor copies validated (dst 0x3b7ad918130 unmapped: TODO find why).
- Run now: 7.1 GiB loaded; list slot 0x3f0 traps; device 0x2b0 CreatePipelineState x6 traps; then AV read [0+0x68] at
  exe+0xb5a5f8 (fn 0x140B5A5D0, rcx struct [0]=null, from exe+0x32764; game-side lookup returned null) on several threads.
- Symbolize our frames offline: python scratch sym.py target\release\runtitle.exe <off>... (crash prints runtitle.exe+off).
- Tests 445 green. Progress 57 %.

- Round 5 helpers launched: lane `ondemand` (on-demand translation of programs missing from the library; status agents/ondemand.md) and lane `streampso` (0x2b0/0x50 graphics pipelines; agents/streampso.md). Lead: the null lookup at exe+0xb5a5f8, then Execute/PresentX.
- Round 5 adds lanes `present` (queue Execute 0x050/0x118, PresentX 0x100, PNG capture, frame line + summary.toml; agents/present.md) and `shaderexec` (execution check vs the oracle; agents/shaderexec.md).

- PR #30 (streampso) merged: 0x50/0x2b0 served, 14 graphics pipelines, 0 refused. clzero: 5 sites detoured (relocation path), 0 emulated. The title calls ExecuteCommandListsX x2 + PresentX x1 at ~1.44 s (both trap: lane `present`), then the null program binding at exe+0xb5a5f8 (engine set-program 0x140122BD6 stores [ctx+0x12328]=null). Intermittent hang inside D3D12XboxCreateDevice seen twice (maybe helper GPU load): runtitle now dumps all threads at TIMEOUT. Tests 460. Progress 58 %.
- GPU resets 21:18-21:20 (nvlddmkm 153 x3) happened during my title runs (MEASURING set; present lane cleared). Not reproduced: a monitored run at ~21:45 left the count at 444. Runs now count nvlddmkm events before/after.
- Round 5 adds lanes shadercov, wave64, ffx. All lanes: card only with MEASURING absent + exclusive agents\CARD lock; WARP for correctness.
- 22:30 resume: PR #31 (ondemand) merged (5e24034, 465 green); lanes present, shaderexec, shadercov, wave64, ffx resumed after the usage limit. No locks held, no new nvlddmkm events.
- 22:47 FIRST PRESENT: PR #32 (present) merged; frame 1 presented and captured (black 1920x1080, no driver events). Frame 2 blocked: null program at exe+0xb5a5f8. Watchpoints (REBUILD_WATCH) show: ctx+0x12328 zeroed by the title's state reset (0x14012494E, B39840 id 0xF value 0) before an immediate draw (0x1402AACF0); ctx+0x12330 (pipeline cache) never set. Execute/Present no-op stubs did not change it. Next: what our run reports differently (console type 0x140075140, caps) that skips the program choice. watch.rs: no DbgHelp in handler (a hit inside the heap corrupted it).
- Device type: the title reads HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM ConsoleMode (GDK lookup at exe+0x55deb88; absent = PC type 1). registry.rs answers 0x64 Series X (REBUILD_CONSOLE=series-s -> 0x60); Reg* in OVERRIDES. Did not fix the null program. Next lead: 0x144FD1Dxx calls setter (0x14011F830 thunk) then 0x1401248xx which resets slot 15 via B39840 (state stack at [tls0+0x12A8]) to null and draws.
- REBUILD_PATCH=exe+OFF=hex (patch.rs). NOP of the fallback call (exe+0x4fd1ddb) does not help: every immediate draw crashes (0x1263f0 path, main 0x5e50fc5): the immediate renderer has no default programs. Two static RE agents running (immediate pipeline init; scene gate).
- Root of frame-2 crash: immediate renderer globals 0x148769BB0/0x14983C0D8 NULL; set at 0x140CBD1E6 from effect lookup 0x141ADCC20("deferred_lighting") -> null. Effect map (cap 0x148420AA0, table 0x148420AA8) has 24/32 entries. All 12 archives open OK (REBUILD_TRACE_FILES=all). Next: why the deferred effects never register (RE agent on it).
- 0x1c8/0x218 now refuse with *out=null (fork behaviour) and log first callers: 0x218 from exe+0x24800db (x42), 0x1c8 from exe+0x247757e (x1; loop over a 1-item batch, failure skips to next). Fork: >=16,384 0x1c8 calls before first Present = the effect/pipeline load we never get. Root: deferred effects never load; renderer init looks up deferred_lighting -> null.
- XAsync race fixed (wait could return E_PENDING between CALLS removal and status write). REBUILD_WATCH takes exe+off / 0x14... preferred addresses. ROOT CAUSE NARROWED: main shader DB (final.fxdb, SGD5 at shaders.rpf+0x9000, contains deferred_lighting) never loads: flag 0x14760AFE1 never set; its task 0x143750CE0 (task id 0x5FDC36D5) never runs (int3 patch never hit). Init DB loads (AFE0 from 0x145EB2E57). RE agent finding the task trigger.
- CORRECTION: renderer init 0x140CBD110 never runs before the crash (int3 patch never hit); boot graph does start (state byte 0x1471B9048 = 2, no lifecycle writes). The crash is the boot loading-screen pump drawing with the init-DB "im" effect ([0x1476667B8], technique byte [0x1476667CF]=0x2a): program null = the chosen pass has desc byte+5 set or a wrong technique. REBUILD_PEEK accepts [0x14global]+off (one deref).
- PINNED: the "im" effect (init DB) gets NULL programs at load: parser store exe+0x247C837 writes 0 = 0x141D65750 (program create/lookup) returned NULL; pass byte+5 = 0 (not by design). RE agent finding NULL paths of 0x141D65750. REBUILD_PEEK takes [reg+inner]+off.
- #35 rootseam merged (493 green). Program NULL cause: 0x142476E20 root-signature build fails with E_NOINTERFACE (0x140FA09F0 chunk check): blobs embedded in the exe have a 12-byte PRIV (no root signature; rpf ones ~480 B); RS cache [0x1475DDB90] exists but misses. RE agent finding the cache source / console path.
- Shader record read from WRONG OFFSET: should be init DB (rpf 0x10287000)+0x2CAE (record e7dc0d00, packed 0x3f9 matches); got bytes from rpf 0x1039D2C8 (db+0x1162C8). Shader obj: +0x30 ptr 0x7ffa48d76a50, +0x38 stream, +0x40 0x2cae. DS requests are 64KiB zlib chunks (no 0x3f9 req). REBUILD_TRACE_FILES logs DS requests (dsreq) and serializer refusals (rootsig). RE agent tracing the stream Seek/Read path.
- REBUILD_TRACE_READS=<name>: ReadFile/SetFilePointerEx traced on matching handles (overrides only when set; resolver now falls back to host for override-listed names with no override). shaders.rpf opened NO_BUFFERING, all reads sync OVERLAPPED-offset, all correct. Bad shader bytes come from chunk 0x10397000+0x62C8 while title tell = 0x2CAE: title stream bookkeeping vs our sync completion / sector-size answers. RE agent reconciling.
- ROOT MECHANISM: the shader arena (420 MB) is scatter-mapped with every page = the same frame (0x1037f0, first page of the 6.1 GB pool view mapped at 79 ms), so it aliases one 64 KiB page; each shader record overwrites the last. Tools: REBUILD_TRACE_ALLOC (overlap, bigalloc, pmap, scatter, alias lines), REBUILD_WATCH=[0x14global]+off (lazy, plus a 0.2 ms poll and a second register on the alias). Physical frame numbers now start at 0x100000 (fork). RE agent finding how the title builds the scatter page list.
- FIXED frame-2 crash: physical page numbers in 4 KiB units (frame<<4, multiples of 16; title packs >>4 into 21-bit fields). Frame 2 presented (black). Title runs ~14 s; next: assert exe+0x2474ce1 with E_INVALIDARG. wave64 merged (#36, 497 green).
- 2026-11-28 00:56 GPU HANG (nvlddmkm 153, DXGI_ERROR_DEVICE_HUNG) on the first run where the title's lists execute without errors (after the barrier-layout fix). TITLE RUNS STOPPED until a safe mode exists (e.g. REBUILD_GPU_SAFE: drop translated compute dispatches / bounded work). Fixed this session: host allocator pool per console allocator (lists recording at once), barrier layout 31 -> GENERIC_READ (unknown layout made lists error, Close/Reset fail, title assert at exe+0x2474ce1), REBUILD_D3D_DEBUG. Debug layer also shows: 24x calls on closed lists, IASetVertexBuffers address 0, invalid topology type, barrier layout vs resource state mismatches.
- shaderexec merged (#38): 5,195/25,694 bit-exact (WARP). The 21:18-21:20 resets were shaderexec card runs, not title runs. Progress 61%. NEXT: GPU safe mode before any title run; merge shadercov #37 (regenerate library + translate test data); lanes' asks: window-view heap range, descriptor word 3, f16 rounding, stuck CARD lock.
- GPU SAFE MODE: REBUILD_WARP=1 (host device on WARP: no driver resets; title runs ~20 s at 30 fps, 60-300 frames) and REBUILD_GPU_SKIP=draw,dispatch,indirect,mesh,submit. RTX bisect caused a 2nd reset (446) with dispatch/indirect/mesh skipped: do not bisect on the card. Fixed from the debug layer: VB address 0 -> empty slot (card page fault), unmapped IB unbinds, present BEGIN list not re-executed in flight (#553), DS+RT/UAV flag combos stripped (#599), topology type 0 -> triangles (#673), ExecuteIndirect null count buffer + max cap, dispatch >65535 dropped. Main shader load now happens (0x1c8 x10241). Remaining: #553 on a title list (re-executed while in flight), #1334 barrier layout vs expected, #611 topology vs PSO group, assert exe+0x2474606 = title GPU-hang handler.
- 2026-11-28 WARP (cfcefe2): #553 fixed (submit progress fence; a list re-executed waits for its earlier run). Then device removals on WARP fixed one by one: Close-failed lists never executed, topology-vs-PSO draws dropped (#611), UAV of non-UAV resource -> null view (#340, 312/run), library work with no shadow table dropped (0 seen). Runs now ~900 lists, frames 1-60 captured. REMAINING ROOT CAUSE (GBV, REBUILD_D3D_GBV=1): compute reads a CBV from a heap slot holding an SRV (#939, index 982941) -> WARP null read in JIT, card page fault = the hangs. Fix in flight (agent, worktree): merge master into #37 + heap-sourced buffers never CBV (sir raw loads; d3d12x CBV -> raw SRV) + regen into D:\victory-data\translate-next and pipelines-next; swap at merge. DRED report prints after a removal (no breadcrumbs on WARP).
- 1b8fa40/ce45430: textures start in the title's initial layout (CreateCommittedResource3; #1334/#1350 gone). Debug layer now shows only #622 (WARP wave32) and #743 (sig type 0x69, 12 B, never executed). Without the debug layer WARP sometimes survives the CBV null read: one run went 60 s, 1,650 frames, 7,454 lists; others removed at ~905 lists. Waiting on the agent (merge #37 + heap no-CBV + regen to *-next dirs). Then: swap data dirs, WARP run w/ REBUILD_D3D_GBV=1, check frames for non-black, then ask owner before a card run.
- 2026-11-28 morning: merged #37 (shadercov + heap no-CBV, b1ca577), #39-#43. Data swapped: D:\victory-data\translate = new (old: translate-prev), pipelines\library.bin = new 417 MB (old: library-prev.bin). WARP+GBV 120 s clean. CARD RUN CLEAN: 60 s, 1,005 frames, 18.6 fps avg, gpu_ms 32.9, nvlddmkm 446 unchanged. Frames black: draw 1 VS dfafaf385b2f94e8/PS 06295fb1d1aefb78 not in shaders.rpf -> agent on ondemand-gfx (+ descriptor alt-pool range vs title indices >750k). Agents: framerate (PR pending), launch C2. Owner: title/card runs need no approval now; dev menu (teleport/spawn vehicles) asked about, proposed L5 + RE agent, awaiting answer. #972 warnings: pso-cache built against the old library (key it on the library version).
