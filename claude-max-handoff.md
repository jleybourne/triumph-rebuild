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
  D:ictory-data
ebuild-oracle (backup in oracle-swapped-backup); `apps/rebuild/src/bin/oracle.rs` must settle
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
