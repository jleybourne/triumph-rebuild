# deepdive.md — the whole repository, in depth

A single document to understand triumph-rebuild without opening another file first, and then to find anything
in it quickly. Every claim is taken from the code or the docs as they stand on `master` at commit `62bf6cf`
(2026-11-27). References are `path:line`; line numbers drift, so treat them as "near here".

Where something is a stub, unbuilt, or where a doc disagrees with the code, this document says so. Section 11
collects the disagreements in one place.

## Contents

1. [What it is, and the life of a title run](#1-what-it-is-and-the-life-of-a-title-run)
2. [Architecture map: every crate and app](#2-architecture-map-every-crate-and-app)
3. [The memory model](#3-the-memory-model)
4. [The GPU path in depth](#4-the-gpu-path-in-depth)
5. [The shader recompiler in depth](#5-the-shader-recompiler-in-depth)
6. [CPU-side console quirks](#6-cpu-side-console-quirks)
7. [Diagnostics and tooling](#7-diagnostics-and-tooling)
8. [Safety rules for runs on the owner's machine](#8-safety-rules-for-runs-on-the-owners-machine)
9. [Progress and what is left](#9-progress-and-what-is-left)
10. [The documents and the helper lanes](#10-the-documents-and-the-helper-lanes)
11. [Inconsistencies: docs vs code](#11-inconsistencies-docs-vs-code)
12. [Glossary](#12-glossary)

---

## 1. What it is, and the life of a title run

### 1.1 The one-paragraph version

triumph-rebuild is a from-scratch Rust compatibility layer that runs an Xbox Series title's own x86-64 code
on Windows. The title is native x86-64 using the Windows calling convention, so most of what it imports binds
straight to the host's own functions. The layer supplies the rest: the console's memory calls (XMem), its file
system, its GDK runtime, audio, DirectStorage, its texture-layout calculator, and its GPU (the D3D12.X device,
translated to host Direct3D 12). The target is 30 fps in-world on the dev machine, against a baseline of
7.6 fps (131.3 ms a frame) from the layer it replaces (`plan.md:19-26`).

The design decision everything hangs on (`plan.md:57-79`, `README.md:37-41`): **GPU-owned memory lives in
VRAM at the title's own addresses and is never copied back; only what the CPU really changed goes up, in
64-byte chunks, off the submit thread.** Section 3 explains how far that is built (the proofs exist; the title
run does not use them yet).

### 1.2 Lineage and licences

| name | what | how it is used |
|:--|:--|:--|
| `fxceless-victory` | the layer being replaced (7.6 fps) | baseline numbers only |
| `jleybourne/Triumph` | the owner's fork of it (git remote `triumph`, GPL-3.0-or-later) | design reference; its shader translations are the **oracle**; non-performance code may be vendored with credit |
| `xcl` | a sibling layer (GPL-2.0) | design reference only, never copied |
| this repo | GPL-3.0-only | ours |

Vendored files and their changes are listed in `CREDITS.md`. They are the DXBC container reader, the native
shader record reader, SGD5 and the GFX10 opcode tables (`crates/shader`), the GFX10 texture layout maths and the
`xg_xs` layouts (`crates/xg`), WMA Pro data tables (`crates/audio/src/wma/tables.rs`) and BCPack constants
(`crates/dstorage/src/bcpack/consts.rs`). Shader IR, lowering and emission are ours by decision
(`plan.md:98-119`).

### 1.3 The life of a title run, start to finish

The title is run by `apps/rebuild/src/bin/runtitle.rs`. The `rebuild` binary (`apps/rebuild/src/main.rs`) is
an 11-line placeholder that builds a `Uma` and prints its size; it ignores `--config`, although the launcher
starts it with one (see 2.20). Everything below is `runtitle --run`.

```
REBUILD_GAME_DIR=<title folder>  runtitle --run [--seconds N] [--sample N]
```

**Step 1 — load and bind** (`runtitle.rs:274` → `crates/runtime/src/title.rs:140-214`).

- Walk the game folder recursively for `.exe`/`.dll` and parse each with `rebuild_loader::Image::parse`.
  Anything that is not PE32+ is skipped.
- Make a **prepared copy** of each image (`crates/loader/src/prepare.rs:62-120`), written once to
  `%LOCALAPPDATA%\triumph-rebuild\images\<name>-<fnv64>.dll`. The copy is marked as a DLL, has its entry
  point, import directory, bound imports and TLS callbacks neutralised, and its checksum and
  `FORCE_INTEGRITY` cleared.
- Load the copy with the real `LoadLibraryExW`. Windows then maps it, relocates it, gives it static TLS and
  registers its unwind tables, but runs nothing and binds nothing (the fork's idea, our code).
- Register each image with `modules::register_title` under the console path `G:\<name>`.
- Resolve every IAT slot, delay-load included, through the `Combined` resolver (`title.rs:43-63`), in this
  order:
  1. the title's own DLL exports;
  2. `HostResolver` (`crates/runtime/src/lib.rs:217-244`), which classifies with `classify`
     (`lib.rs:158-168`) into one of the tiers below.

| tier | rule | bound to |
|:--|:--|:--|
| `Override` | the name is in `OVERRIDES` (`lib.rs:59-145`): registry 5, memory 11, files 32, CRT 13, modules 12, process 2 | `modules::override_export` → `exports::override_export` → `registry` or `rebuild_winfiles` |
| `ConsoleDll` | the DLL does not exist on the host | `exports::console_export` (`exports.rs:459-502`) |
| `ConsoleFunction` | the DLL exists but lacks the function | `exports::console_export` |
| `Host` | the host's System32 DLL exports it | the host function itself, no wrapper |

- A slot nothing resolves gets a 22-byte **trap thunk** (`title.rs:95-112`: `mov rcx, index; mov rax, trap;
  jmp rax`). The thunk logs `discovery: the title called dll!name` on first call, counts calls and returns 0
  (`title.rs:80-92`).
- Slots are written under `VirtualProtect` (`title.rs:201-211`).

The census behind the tiers (`runs/build3-runtime-census.md`): of the 697 functions the title imports that its
own DLLs do not supply, **624 (89.5 %) bind straight to the host**, 34 are overrides and 39 are console-only in
7 DLLs. A `maptitle` run bound 1,876 of 1,924 imports (97.5 %) before more of the runtime existed
(`runs/build3-maptitle.md`).

**Step 2 — diagnostics and CPU patches.**

- `crash::install(images)` and `crash::on_fatal(device_report)` (`runtitle.rs:293-294`).
- `watch::install()`, a no-op unless `REBUILD_WATCH` is set (`runtitle.rs:296`).
- `rebuild_runtime::cpu::install()` (`runtitle.rs:298`): the `clwb`/`clzero` handler (section 6).

**Step 3 — the TLS slot swap** (`runtitle.rs:299-311`, `crates/runtime/src/tls.rs:94-135`). The title reads
its thread-local block as `gs:[0x58]` → `array[0]` without reading `_tls_index`, as exe code may. Loaded beside
our exe it was given slot 6, and slot 0 was ours. `give_slot0` swaps the two blocks in every thread's TLS
array and swaps the indices: the title's becomes 0 and ours 6. Our own code always reads `_tls_index`, so the
swap costs nothing per access. New threads are handled by our exe's TLS callback (`tls.rs:65-85`, in
`.CRT$XLY`), which also runs the title exe's thread-attach callbacks (up to 8, `tls.rs:24`). The fork instead
rewrote every `gs:[0x58]` load into a call.

**Step 4 — the guards** (`guard`, `runtitle.rs:198-259`). A failure prints `REFUSED` and exits 2.

| guard | what it does |
|:--|:--|
| limits | `limits::set(Limits::from_env())` reads `REBUILD_LIMIT_*` (`crates/runtime/src/limits.rs:30-38`) |
| preflight | `budget::host_commit_available()` must be at least `private_cap() + 4 GiB` |
| job object | `KILL_ON_JOB_CLOSE \| PROCESS_MEMORY \| DIE_ON_UNHANDLED_EXCEPTION`, `ProcessMemoryLimit = private_cap`; CPU limits through `limits::apply_cpu(job)`; the handle is leaked on purpose |
| budget | physical-page commit is charged to the system, not the process, so a job cannot cap it; `budget.rs` charges every physical allocation against a limit of min(12 GiB, physical RAM / 3), or `REBUILD_MEMORY_BUDGET_MB` (`budget.rs:38-61`) |
| watchdog | a thread with a 64 KiB stack sleeps `seconds + 15`, writes `WATCHDOG deadline passed` with a raw `WriteFile`, then `TerminateProcess(self, 0xDEAD)` (`runtitle.rs:239-256`) |

Caveat: the job object and memory cap are applied after `load_title` has already loaded every image, so they
cover the run, not the load.

**Step 5 — process setup.**

- `drives::install` (`runtitle.rs:317`, `crates/runtime/src/drives.rs:39-67`) does the following:
  - deletes the overlay's `T:`;
  - builds a `rebuild_vfs::Vfs` with `G:` (game folder plus mods, read-only), `T:` (emptied every run, as on
    the console) and `S:` (persistent storage);
  - configures the GDK profile's storage drive;
  - installs `rebuild_winfiles::Files` with root `G:\`;
  - installs the DirectStorage engine with a `PhysicalPages` writer and an optional BCPack decoder, wrapped
    by the capture when `REBUILD_BCPACK_CAPTURE` is set.
- `set_command_line("G:\<exe>")`.
- If the exe exports `__XMemAllocationHooks__`, its alloc/free pair is handed to `set_alloc_hooks`
  (`runtitle.rs:326-333`). From then on `XMemAlloc`/`XMemFree` call the title's own hooks.
- `on_create_device(reached_device)` registers the callback that prints `REACHED` (`runtitle.rs:86-93`).

**Step 6 — title entry** (`runtitle.rs:337-368`).

- Every DLL's `DllMain(DLL_PROCESS_ATTACH)` runs in dependency order (a DFS over imports, depth capped at 64,
  `runtitle.rs:157-178`).
- Then the exe's TLS callbacks.
- Then the exe entry point on a `title-main` thread whose stack is `max(stack_reserve, 8 MiB)`.

Measured: 16/16 DLL entry points succeed, and the entry point reaches `D3D12XboxCreateDevice` in 1.4 s
(`runs/build3-reached.md`).

**Step 7 — `D3D12XboxCreateDevice`** (`crates/runtime/src/exports.rs:368-390`). It logs the riid (the title
asks for `ID3D12Device11`) and 16 parameter dwords, then calls `device::create` (`device.rs:466-518`), which:

1. creates a host `D3D12CreateDevice(None, FL 12_1)`;
2. creates resource memory: `rebuild_d3d12x::HostMemory::new(&host, 1 GiB)` wrapped in `Accounted` (the 0x80
   accounting, section 3.6); this is a **stand-in** "until the tiles join" (`device.rs:483-484`);
3. builds `rebuild_d3d12x::Resources`;
4. starts the pipeline pool and library (`pipeline::start`, `pipeline.rs:62-84`);
5. sets the global `ListHost` to `rebuild_d3d12x::Host { resources, pipelines }` and installs the d3d12x thunks;
6. hands out a leaked 0x58-byte device object with its method table at +0 and **byte +0x18 = 1**, which makes
   the title call methods instead of writing inline packets;
7. starts the frame-line thread (`framelog::start`).

`D3D12ConfigureVirtualMemoryX` reports the GPU address range as [4 GiB, top of user space]
(`exports.rs:395-404`).

**Step 8 — device children.** The title creates, measured on one run (`claude-max-handoff.md:187`): 1 queue,
5 fences, 4 X-queues, 439 allocators, 581 lists, DXGI objects and 2 frame events. Then it creates root
signatures, ~95 compute pipelines, stream and graphics pipelines, query heaps, command signatures, resources
and descriptor heaps, and loads ~7.1 GiB of content (`progress.md:122-124`). Each object kind is in 4.1-4.6.

**Step 9 — submission and present.**

- Command lists are translated **as the title records them**, on its own threads (`crates/xlist/src/console`).
- `ExecuteCommandListsX` (queue slot 0x118) or `ExecuteCommandLists` (0x50) go to `submit::execute`
  (`crates/runtime/src/submit.rs:52-94`). It turns each list's segments and memory waits into host queue
  waits and batched `ExecuteCommandLists` calls.
- `PresentX` (queue slot 0x100, `present.rs:249-291`) copies or blits plane 0 into a flip-discard swap chain
  on a borderless window, and optionally writes PNG captures.
- Frame pacing is the simple clock in `frame.rs` (`WaitFrameEventX` sleeps to the next interval).

**Step 10 — the frame line and the end of a run.**

- A `frame-line` thread (`framelog.rs:354-387`) wakes on each whole second of process age and appends one line
  to `REBUILD_FRAME_LOG`:
  `t | fps | ms | gpu | draws | dispatches | dropped | wait | late | held | WxH -> WxH`
  (`framelog.rs:202-229`; sample in `crates/runtime/src/frameline.sample`).
- At `REBUILD_RUN_SECONDS` it writes `summary.toml` (`avg_fps`, `frame_ms`, `low1_fps`, `gpu_ms`, `frames`,
  `seconds`, `measured_from`), shuts the presenter down and exits.
- Otherwise runtitle's own wait loop ends with one of these outcomes:

| outcome | when | report |
|:--|:--|:--|
| `EXITED` | the entry point returned | — |
| `DEADLINE` | the deadline passed after the device was reached | `device_report()` (`runtitle.rs:98-155`): pipeline report, every trap list, called device slots, frame counters, clwb/clzero counts, unknown IIDs |
| `TIMEOUT` | no device by the deadline | every other thread's stack is dumped |

- Last comes the discovery list (`runtitle.rs:34-60`): trapped imports called, file discoveries, GDK
  discovery, and `GetProcAddress`/`LoadLibrary` misses.

**Where a run stands today** (`claude-max-handoff.md:219-224`). The title reaches `ExecuteCommandListsX` ×2
and `PresentX` ×1 at about 1.44 s. Frame 1 is presented and captured (black 1920×1080, as the title draws it).
Then several threads fault reading `[0+0x68]` at `exe+0xb5a5f8`: the title draws with no program bound
(section 9.3).

---

## 2. Architecture map: every crate and app

### 2.0 The crate graph

The workspace (`Cargo.toml`) has 17 crates and 2 apps. The release profile uses thin LTO, one codegen unit,
`panic = "abort"`, and `debug = "line-tables-only"` so crash reports can name host frames.

| crate / app | package | depends on (workspace) | used by the title run? |
|:--|:--|:--|:--|
| `crates/loader` | `rebuild-loader` | — | yes (prepare, Resolver trait) |
| `crates/vfs` | `rebuild-vfs` | — | yes (drives) |
| `crates/winfiles` | `rebuild-winfiles` | vfs | yes (file overrides) |
| `crates/runtime` | `rebuild-runtime` | loader, vfs, winfiles, audio, dstorage, xg, gdk, d3d12x, xlist, pipelines, shader | yes: it *is* the run |
| `crates/d3d12x` | `rebuild-d3d12x` | xg, xlist | yes (resources, heaps, views) |
| `crates/xlist` | `rebuild-xlist` | — | yes (lists, allocators) |
| `crates/pipelines` | `rebuild-pipelines` | shader, sir | yes (library, pool, on-demand) |
| `crates/shader` | `rebuild-shader` | — | indirectly (hashing, containers) |
| `crates/sir` | `rebuild-sir` | shader | indirectly (the lifter, through pipelines' on-demand path) |
| `crates/audio` | `rebuild-audio` | — | yes (19 exports) |
| `crates/dstorage` | `rebuild-dstorage` | vfs | yes (`DStorageGetFactory`) |
| `crates/gdk` | `rebuild-gdk` | — | yes (`xgameruntime.dll`) |
| `crates/xg` | `rebuild-xg` | — | yes (`xg_xs.dll`, d3d12x sizes) |
| `crates/uma` | `rebuild-uma` | — | **no**: proofs and benches only (`apps/rebuild`) |
| `crates/upscale` | `rebuild-upscale` | — | **no**: `upscaletest` only |
| `crates/input` | `rebuild-input` | — | **no**: nothing depends on it |
| `crates/config` | `rebuild-config` | — | **no**: the launcher only; the layer reads env vars |
| `apps/rebuild` | `rebuild` | most crates | the bins, including `runtitle` |
| `apps/launcher` | `rebuild-launcher` | config, vfs | starts the layer |

There is no `ffx` crate on `master`. The `ffx` lane (the title's FidelityFX calls onto `crates/upscale`) is
listed as running in `progress.md:64` but has not merged.

Test counts by `#[test]` attribute (486 in all; about 12 are `#[ignore]`d hand-run GPU or title-data tests):

| crate | tests | crate | tests |
|:--|--:|:--|--:|
| runtime | 48 | sir | 32 |
| pipelines | 42 | d3d12x | 30 |
| shader | 42 | uma | 24 |
| gdk | 46 | audio | 22 |
| dstorage | 22 | loader | 18 |
| vfs | 18 | input | 17 |
| xg | 16 | xlist | 15 |
| config | 14 | upscale | 11 |
| winfiles | 11 | launcher | 58 |

### 2.1 `crates/loader` — PE32+ parser, mapper and prepared images

Purpose: parse the title's PE32+ images, map them for census tools, and make the "prepared" copies the Windows
loader maps for real. No dependencies; the `windows` crate is a dev-dependency only.

| file | what | key items |
|:--|:--|:--|
| `pe.rs` | hostile-input-safe parser | header checks (`:276-295`); hard caps (`:36-40`); relocations (`:414-443`), imports (`:469-488`), delay imports (`:490-511`), exports with forwarders (`:513-559`), TLS (`:561-594`), `.pdata` (`:596-609`) |
| `map.rs` | map at a base for tools | `map_into(img, file, base, out, resolver, opts)` (`:77-172`); `trait Resolver` (`:8-10`); DIR64/HIGHLOW/HIGH/LOW relocations |
| `region.rs` | exact-address RW memory | `Region::at(base, len)` (`:24-34`), never executable |
| `prepare.rs` | **the path the runtime uses** | `prepare` (`:62-120`) |
| `strings.rs` | ASCII/UTF-16 string scan for API-like names | used by `imports --getproc` |
| `synth.rs` | a synthetic PE32+ DLL built byte by byte | tests only |

How `prepare` works: the import directory is pointed at its own null terminator, so it exists (Windows 10
still assigns static TLS) but is empty. TLS `AddressOfCallBacks` is zeroed and the relocation that targets it is
neutralised, so a rebase keeps the zero.

Tests (18). The test `matches_the_windows_loader_on_a_system_dll` maps a real System32 DLL and requires every
relocation to match the live module. `garbage_is_an_error_never_a_panic` applies 200,000 random corruptions.
The prepare test loads our own test exe prepared and checks that Windows assigned a TLS index and a block.

Measured (`agents/loader.md`): 4,523 / 4,523 System32 PE32+ images parse; `dbghelp.dll` maps with 9,912
relocations and 0 wrong.

Gaps: `agents/loader.md` predates `prepare.rs` and does not mention it; its "next" items (`.pdata`
registration, running TLS callbacks) are now done by the Windows loader and `runtitle`.

### 2.2 `crates/vfs` — console paths over a read-only base

Purpose: console paths (`G:\…`) onto host files. A lookup checks the write overlay first, then enabled mods
(the last one wins), then the read-only base (`lib.rs:1-16`). There is no global state: a `Vfs` owns its drives
and a handle table.

| file | what |
|:--|:--|
| `path.rs` | `ConsolePath::parse` (`:22-55`): `X:\a`, `X:/a`, `X:a`, `\\?\X:\a`; `..` stops at the root; reserved characters refused; `fold` lowercases |
| `layer.rs` | `ReadLayer`: each directory listed once and cached by folded key; no invalidation (never written) |
| `overlay.rs` | files under `<overlay>/<LETTER>/files/…`; whiteouts in `whiteouts.txt`, saved atomically |
| `mods.rs` | `ModSet::discover`, `set_enabled`, `move_to`, `conflicts()` |
| `lib.rs` | `VfsBuilder::package/writable/build` (`:220-259`); open with the five CreateFile dispositions; copy-up; generation-checked handles; `enumerate`, `rename`, `delete` (whiteouts); F5 hooks `delete_pending`, `settle`, `note_renamed` |

The default overlay root is `%APPDATA%\triumph-rebuild\overlay` (`lib.rs:190-196`).

Tests (18) prove that the base and mods are byte-, time- and attribute-identical after write-heavy runs, that
state persists, that later mods win, and that stale handles are refused. `v2_game_folder_unchanged` runs only
with `REBUILD_GAME_DIR`.

Measured (`agents/vfs.md`): 77.0 µs per file warm against an OS floor of 73.7 µs (+3.3 µs); a lookup 1.54 µs.

Known issue: `build()` calls `fs::create_dir_all(overlay_root/<LETTER>)` (`lib.rs:230`) **before** the overlap
check (`lib.rs:240`), because `canonicalize` needs the folder. A refused overlay that sits inside the base
therefore still creates a directory in the base (section 11).

`bin/vfsbench.rs`: a synthetic tree in temp (100×100 base files, two mods, 1,000 overlay rewrites), cold and
warm passes against an OS floor, and 4 threads on one `Vfs`.

### 2.3 `crates/winfiles` — the Win32 and CRT file functions over vfs

Purpose: each file override has the host's exact signature. It makes the path an absolute console path against
our own current directory (`Files::absolute`, `lib.rs:199-226`), does the vfs bookkeeping, then calls the
**host's own function** on `\\?\<host path>`. Handles are real host handles, so `ReadFile`, `CloseHandle` and
the rest stay bound to the host.

- Global state: `static FILES: RwLock<Option<Arc<Files>>>`, set by `install`/`uninstall` (`lib.rs:121-136`).
- Unknown paths (UNC, devices like `CON`, unmounted letters, streams, the empty path) become `Discovery`
  entries (`lib.rs:141-163`) and fail with `ERROR_PATH_NOT_FOUND` or `ERROR_INVALID_NAME`. They never reach the
  host.
- `Files::create` (`lib.rs:299-384`) is the CreateFile core: write-access detection, copy-up,
  read-only-file refusal like the host, directory materialisation, delete-on-close, and `ERROR_ALREADY_EXISTS`
  set to match the host.
- `find.rs`: search handles placed above the user address space; per layer it runs the host
  `FindFirstFileExW` and keeps only the winning layer's entries.
- `crt.rs` (F4): 13 CRT functions open through our core, then the same `ucrtbase` `_open_osfhandle`/`_fdopen`.
- `extra.rs` (F5):
  - `CopyFile2` and `CreateHardLinkW` (with copy-up);
  - `CreateSymbolicLinkW`, refused with error 1314;
  - `WerRegisterFile`, which succeeds and logs;
  - `SetFileInformationByHandle`.
- `FUNCTIONS` (32, `lib.rs:813-848`) plus `CRT_FUNCTIONS` (13, `crt.rs:501`) are all served by
  `override_export` (`lib.rs:851-888`).
- `REBUILD_TRACE_FILES` (`lib.rs:387-398`) prints the first 200 failed `CreateFile*` calls.

Tests (11) compare the overrides with the host **side by side**: 145 Win32 outcomes, 92 CRT outcomes and
46 F5 outcomes, all equal, with the base hash unchanged.

### 2.4 `crates/runtime` — what the title's imports bind to, and the device

The heart of a run. Every file is described below; sections 3, 4 and 6 go deeper.

| file | purpose | key items |
|:--|:--|:--|
| `lib.rs` | tiers and the host resolver | `Tier` (`:46-55`), `OVERRIDES` (`:59-145`), `classify` (`:158-168`), `host::Host` (System32 only, `:180-208`), `HostResolver` (`:217-244`) |
| `title.rs` | load, prepare, bind, trap thunks | `load_title` (`:140-214`), `LoadedTitle` (`:29-34`), `trap_hits` (`:73-76`) |
| `exports.rs` | console-only functions and memory/process overrides | `STATE` (`:89-107`), `ChunkHeap` (`:19-87`), 13 xmem functions (`:121-303`), `VirtualAlloc` (`:310-316`), `override_export` (`:342-354`), `D3D12XboxCreateDevice` (`:368-390`), `console_export` (`:459-502`), audio binding (`:505-530`) |
| `modules.rs` | `GetProcAddress`, `LoadLibrary*`, `GetModuleHandle*`, `GetModuleFileName*`, `FreeLibrary` | title modules, console pseudo-handles `0xFFFF_8000_0000_0000 + (n+1)*0x10000` (`:47`); every miss recorded (`:112-145`) |
| `xmem.rs` | XMem placement rules | constants (`:20-34`), `address_range` (`:38-47`), `host::virtual_alloc` (`:125-186`) |
| `physical.rs` | 64 KiB physical frames as pagefile-section views | `Physical` (`:32-38`), `map` (`:124-152`), `reserve_placeholder` (`:64-68`) |
| `tls.rs` | TLS slot 0 for the title | `give_slot0` (`:94-135`), `on_thread` (`:65-85`) |
| `device.rs` | the D3D12.X device object | 160 slots, object 0x58 (`:24-25`), `create` (`:466-518`), `Accounted` (`:539-569`) |
| `queue.rs` | command queues | 40 slots, object 0x40 (`:23-28`) |
| `fence.rs` | fences with a value cell the title reads | 24 slots, cell at +0x10 (`:25-26`), watcher threads (`:251-270`) |
| `dxgi.rs` | console-shaped DXGI device, adapter, factory, output | `host_mode` (`:183-189`), console mode list (`:209-236`) |
| `frame.rs` | frame pacing (simple clock) | `WaitFrameEventX` (`:52-79`) |
| `submit.rs` | ExecuteCommandLists(X), memory waits | `execute` (`:52-94`), `place` (`:148-166`), bridge (`:222-283`) |
| `present.rs` | PresentX, swap chain, PNG capture | `PresentX` (`:249-291`), `Presenter::new` (`:294-397`), `present` (`:399-500`) |
| `present_card.rs` | one ignored end-to-end GPU test | `:190-336` |
| `framelog.rs` | the frame line and `summary.toml` | `format` (`:202-229`), thread (`:354-387`) |
| `pipeline.rs` | compute, graphics and stream pipelines | `start` (`:62-84`), compute (`:200-242`), `request` (`:261-302`), `host` (`:338-360`) |
| `cpu.rs` | `clwb` NOPs, `clzero` detours | `cpu.rs:42-308` |
| `registry.rs` | the console's device type | `ConsoleMode` (`:26-31`) |
| `drives.rs` | `G:`, `T:`, `S:`; DirectStorage install | `install` (`:39-67`), `PhysicalPages` (`:72-105`) |
| `limits.rs` | user-set hardware limits | `Limits` (`:30-38`), `apply_cpu` (`:86-114`) |
| `budget.rs` | the physical-page budget | `:38-96` |
| `xg.rs` | the `xg_xs.dll` texture computer object | 13-slot table (`:48-54`), `XGSuggestSwizzleMode` (`:194-198`) |

`CONSOLE_DLLS` (`exports.rs:452-453`): `d3d12_xs`, `xmem`, `acphal`, `xapu`, `chad`, `xcrdapi`, `xg_xs`,
`xgameruntime`. Routing in `console_export`:

| DLL | goes to |
|:--|:--|
| `acphal`, `xapu`, `chad` | `rebuild_audio` |
| `xcrdapi!DStorageGetFactory` | `rebuild_dstorage` |
| `xg_xs` | `crate::xg` |
| `xgameruntime` | `rebuild_gdk::runtime::get_proc` |

Module overrides worth knowing:

- `GetProcAddress` answers title modules from their exports and console modules through `console_export`.
  Ordinals on console modules always miss. Everything else goes to the host.
- `LoadLibraryExW` tries the registry, then console DLLs, then the host (System32 only), and ignores its
  flags (`modules.rs:148-164`). This is how the title's `LoadLibraryExA("xgameruntime.dll")` gets ours, not
  the PC's GDK runtime. That was one of the three causes of the start-up stall (`runs/build3-reached.md`).
- `GetModuleHandleW(NULL)` returns the title's exe.

Tests (48, one ignored). They cover the classification tiers, trap thunks, all 13 xmem imports binding, XMem
alignment and placement, physical-page remap and alias, the root-signature flag strip, `fence_value`, wait
placement, the frame-line format against `frameline.sample`, cpu patching, the registry answer, the xg vtable
layout, and four pipeline-object tests. `queue.rs` and `tls.rs` have none.

Gaps (detail in section 11):

- 9 of the 11 memory functions in `OVERRIDES` have no implementation. Only `VirtualAlloc` and `VirtualFree`
  exist. Because they are tier `Override`, a title import of any other would get a trap thunk, not the host
  function.
  - This is latent today: none of them is in the title's import census (`agents/loader-imports.csv`).
  - It would bite if one were imported, or if the override path were consulted for a `GetProcAddress`.
- `XMemGetWorkingSetStatistics` returns FALSE until its layout is known (`exports.rs:296-299`).
- Resource memory is the 1 GiB host stand-in, not the tiles.
- The VRAM limit is recorded, not enforced.

### 2.5 `crates/d3d12x` — resources, heaps, descriptor heaps, views

The whole crate is `#![cfg(windows)]` (`lib.rs:39`). Its 27 device-slot functions are copied into the device's
table by `runtime/src/device.rs:456-459` (`thunks::device_slots()`, `thunks.rs:179-209`):

```
0x70  0x78  0x80  0x88  0x90  0x98  0xa0  0xa8  0xb0  0xb8  0xc0  0xc8  0xd0  0xd8
0xe0  0xe8  0x130 0x138 0x148 0x1e8 0x230 0x238 0x250 0x258 0x360 0x3f8 0x428
```

| file | what |
|:--|:--|
| `lib.rs` | `Resources` (an `Arc<Inner>`, `:151`), `Events` trait (`:79`), `methods(Object)` (`:216`) |
| `object.rs` | the shared object head, `is()` by vtable pointer (`:75`), `hand_out` (`:117`), `this_and_out` (`:244`) |
| `slots.rs` | 32-slot tables for six object kinds; const-generic traps (`:145-179`); `build()` (`:187`) |
| `thunks.rs` | the device-slot `extern "system"` functions over a global `OnceLock<Resources>` |
| `records.rs` | what goes in the title's descriptor memory; `Entries` (`:155`) |
| `heap.rs` | descriptor heaps, mirroring, the shadow table, every view create call |
| `resource.rs` | `Resource` and `HeapObject`; creation slots |
| `memory.rs` | `Memory` trait (`:19`), `HostMemory` (`:43`), `PlainMemory` (`:120`) |
| `views.rs` | `HostView`, `HostHeap`, `Shadow` (`:169`), `HostDevice` trait (`:204`), `DeviceHost` (`:235`) |
| `host.rs` | `Host<P>` implements `xlist::console::ListHost` (`:22-53`) |
| `root.rs` | root signatures from the title's blob (`:45-62`) |
| `signature.rs` | query heaps and command signatures (a host refusal still yields a valid object) |
| `sampler.rs` | the 27-word `CreateSamplerX` description decoded (`:29`) |
| `format.rs` | console formats, typeless twins, the B5G5R5A1 → RGBA8 stand-in, footprints via `xg` |

Section 4 describes the object layouts, records, mirroring and the shadow table.

Tests (30, 3 ignored GPU tests). They cover header offsets and vtables, trap kinds, the resource ABI (GPU address
= console address, Map, GetDesc in both argument orders), buffer views with raw mirrors and shadow entries,
misaligned CBVs, copies and syncs, the 1,245,184-slot heap, and on the card a shader reading raw, typed and CBV
views at their alternates exactly.

### 2.6 `crates/xlist` — command lists

Two halves:

- **`lib.rs`, `frame.rs`, `d3d.rs`**: the Build 0 (b) abstract model and benchmark. `Cmd` has 9 variants;
  `translate()` (`lib.rs:168`) elides repeats, maps addresses and refuses unknowns. `frame.rs` builds a
  synthetic title-shaped frame (48 lists, 4,500 draws, 1,433 pipelines). `d3d.rs` runs it into real D3D12
  lists for `listbench`.
- **`console/`**: the real D3D12.X list and allocator, used by the runtime (section 4.7).
  - `mod.rs`: `ListHost`, `HeapBinding`, `MemoryWait`.
  - `list.rs`: the 160-slot list, 66 slots implemented.
  - `alloc.rs`: the 24-slot allocator, 7 slots implemented, plus 0x4a0 upload memory.
  - `slots.rs`: tables and traps.
  - `fake.rs`: a recording stand-in for tests.
  - `tests.rs`.

Tests (15, 2 ignored GPU). One reads `agents/d3d12x-child-slots.csv` at compile time and checks that every
list and allocator slot exists with the right return kind. Others check `pso_missing` skipping (`tests.rs:406`)
and memory-wait cut and replay (`tests.rs:435`).

Measured: 0.43 µs a draw including D3D12 and the driver, 1.9 ms for the title-shaped frame on one thread
(`runs/build0b-lists.md`). The console list costs 0.47 µs a draw on the card (PR #25).

### 2.7 `crates/pipelines` — root signatures, the program library, PSOs

| file | what |
|:--|:--|
| `layout.rs` | the fixed host root-signature shape (`:3-15`, `:145-170`) |
| `bindings.rs` | PSV0 bindings and signatures read from DXIL |
| `library.rs` | the offline program library file `RBPIPLIB` v2 (`:29`); `build` (`:101`), `load` (`:272`), `get` (`:410`) |
| `state.rs` | a pipeline as plain data; `Key::of` (`:179`) |
| `stream.rs` | console pipeline streams (0x2b0) and graphics descriptions (0x50) → `Desc`; `program_hash` (`:18-25`) |
| `pool.rs` | asynchronous PSO creation; `Pool` (`:177`), `request` (`:276`), `wait` (`:319`) |
| `pso.rs` | host PSOs and the `ID3D12PipelineLibrary1` cache `RBPSOCAC`; `PsoCreator` (`:578`) |
| `ondemand.rs` | translating programs not in the library; disk cache `RBONDEMD` |
| `translate.rs` | console program → HLSL → DXC; `DXC_ARGS` (`:28`), `MAX_DXC = 4` |
| `session.rs` | the pipelines a session used, for the next session's prewarm (`RBPSOSES`) |
| `sidecar.rs` | `<hash>.<stage>.samplers` files |
| `d3d.rs` | root serializer, `Gpu` (card or WARP), `CardLock` (`:121`) |
| `bin/pipelines.rs` | CLI: `build`, `load`, `check`, `async`, `ondemand` |

Section 4.5 explains the library, pool and on-demand path.

Measured:

- **PR #22:** 82/82 root signatures, 117 compute + 1,374 graphics PSOs, 0 refusals, library load ~5 ms.
- **PR #27:**
  - 12 workers create 1,491 cold PSOs in 0.6 s;
  - the `ID3D12PipelineLibrary` cache returns all 1,491 on the second run;
  - prewarm takes the first-draw block from 605 ms to 0.

Tests: 42.

### 2.8 `crates/shader` — finding, decoding and comparing console shaders

The front end of the recompiler (section 5): the SGD5 archive reader, the DXBC container, the native-program
(FSSM) reader, the GFX10 decoder and field model, the census, the LLVM bitcode reader and `oraclediff`.
Tests: 42.

### 2.9 `crates/sir` — the shader core

Two different things live here (section 5.5):

- **The SIR test IR**, used only to prove the lane model on random programs: `lib.rs`, `classify.rs`,
  `lanes.rs`, `gen.rs`, `hlsl.rs`.
- **The real GFX10 → HLSL translator**: `lift.rs`, `lift/cfg.rs`, `lift/io.rs`, `lift/res.rs`, plus
  `sampler.rs`.

Tests: 32.

### 2.10 `crates/uma` — the memory model's proofs

Window page bookkeeping, tiles, the arena, 64-byte chunk diffing, the upload benchmark, the upload ring and
GPU labels (section 3). Only `gputest`, `uploadbench`, `sirtest` and the `rebuild` placeholder use it.
Tests: 24.

### 2.11 `crates/audio` — `acphal`, `xapu`, `chad`

All 19 exports are bound by `runtime/src/exports.rs:505-530`.

| file | what |
|:--|:--|
| `heap.rs` | the APU heap: one reserved range, device address = `0x00100000` + offset (`:25`); `ApuCreateHeap` (`:241`), `ApuAlloc` (`:266`), `ApuFree`, `ApuMap*` |
| `acp.rs` | `AcpHal*` and the coprocessor object; the default XMA decoder is ours (`:49`) |
| `xma.rs` | XMA2 packet/frame walk and the decode engine on its own thread (`:305`) |
| `wma/` | **our own XMA2 / WMA Pro decoder** (`Decoder::decode_f32`, `wma/mod.rs:335`); only the data tables are vendored |
| `xapu.rs` | Opus through `opus-pure` on a worker thread |
| `xdsp.rs` | `chad.dll` DSP: commands complete at once, **output is silence** (impulse-response layout unknown, `:9-11`) |
| `wasapi.rs`, `ring.rs` | WASAPI shared-mode output on an SPSC ring; **not used by the runtime** |
| `awc.rs` | just enough of the engine's container for tests |

Measured (`agents/audio.md`):

- the worst title-side call takes 27.6-68.1 µs;
- our decoder against FFmpeg over 1,818 XMA2 streams: 0.076 % of samples off by 1 LSB, none by more;
- decoding runs at about 1,200× real time on one thread.

Tests: 22, 4 ignored.

### 2.12 `crates/dstorage` — DirectStorage and BCPack

| file | what |
|:--|:--|
| `lib.rs` | console layouts: `Request` 0x48 bytes (`:94-123`), option bits (`:132-141`), `QueueDesc` 0x20 |
| `com.rs` | factory, queue, status-array and file vtables; `DStorageGetFactory` (`:379`) |
| `engine.rs` | worker engine over vfs; priorities 1000/100/10/1; ordered completion; `Pages` trait (`:40`) for physical-page destinations |
| `zlib.rs` | zlib-rs |
| `bcpack/` | **our BCPack decoder**: AVX2 16-lane rANS plus a scalar fallback; tables loaded at run time from outside git |
| `cache.rs` | on-disk decode cache (BLAKE3 key, LRU, 8 GiB cap) |
| `capture.rs` | saves each BCPack request as `<n>-bc<bc>-<bytes>.bcp` for comparison |
| `bin/dsbench.rs`, `bin/bcbench.rs` | benches |

Measured:

- the engine reads 2,024 MB/s on 8 workers;
- BCPack is bit-exact against the fork's decoder over 960 valid + 32,640 mutated streams;
- on one thread it runs 1.7-3.0× the fork's speed: BC1 60-64 MB/s, BC7 29-36 MB/s.

`drives.rs:61` calls `Decoder::from_config(None)`, so tables come only from `REBUILD_BCPACK_TABLES` or
`%LOCALAPPDATA%\triumph-rebuild\bcpack-tables.bin`. The registry key `dstorage.bcpack_tables` is never read.

Open: D5b (compare against the title's real BCPack requests). The first capture run saw 0 BCPack requests
before the device; streaming starts later (`tasks.md:225`).

### 2.13 `crates/gdk` — the GDK runtime behind `xgameruntime.dll`

It models an offline retail console: one signed-in user, the title owned and installed, no network.

- `runtime.rs`:
  - `InitializeApiImpl*` only record their arguments (`:45-56`);
  - `QueryApiImpl` (`:65`) answers 17 (provider, iid) pairs over 14 interfaces, and anything else with
    `E_NOINTERFACE` plus a discovery line;
  - `get_proc` (`:101`) serves the 6 entry points by name.
- `object.rs`: each vtable gets 8 slack slots past its end, which go to discovery.
- `areas/`: task queue, user, game, system, package, store, networking, local storage, display.
- `taskqueue.rs` and `xasync.rs`: XTaskQueue and XAsync as host code with the console's structure layouts
  (`XAsyncBlock` is 56 bytes).
- `profile.rs`: gamertag "Player", xuid `0x0009000000000001`, title_id 0, Series X, retail.

`agents/gdk-slots.csv` has 130 rows `(area, provider, iids, offset, api, args)`. Measured: 30/30 GUIDs the
exe passes to `QueryApiImpl` are named. Tests: 46.

Gap: the runtime sets only the storage fields of the profile (`drives.rs:52-57`), so gamertag and title id
stay at their defaults. Game save, game UI, shell, capture and speech are not served.

### 2.14 `crates/xg` — the console texture layout (`xg_xs.dll`)

- `layout.rs` (vendored, addrlib-derived GFX10 maths): `Layout::new` (`:271`), `offset` (`:540`).
- `address.rs`: the inverse, `texel_at`.
- `computer.rs`: the 13-slot texture computer, `suggest_swizzle_mode` (`:121`), `element_offset` (returns 0
  outside the texture, `:362`), `copy_into/from`, `tiling`.
- `detile.rs`: a CPU detiler.

XOR swizzle modes are refused. Bound by `runtime/src/xg.rs`; `d3d12x/src/format.rs` uses `Computer` for
footprints. The detiler is not used yet.

Measured: against the fork, 174 cases, 43,446,949 bytes, 0 differ; detile 10-11 GB/s for BC7. Tests: 16.

### 2.15 `crates/input` — pads, keyboard, menu isolation

- `pad.rs`: `PadState` (28 bytes), laid out like the console.
- `keyboard.rs`: the stand-in pad mapping (WASD / IJKL, …).
- `menu.rs`: menu isolation. While the menu is open or draining the title sees a neutral pad and its rumble
  is dropped. The menu key is `MENU_KEY` (F10) unless `Input::with_menu_key` is given another;
  `menu_key_named` maps each `menu.key` choice (F10 / F11 / F12 / Insert / Home) to its key.
- `gameinput.rs`: the one GameInput import, `GameInputCreate`.
- `host.rs`: XInput slots 0-3 plus `GetAsyncKeyState`.

**Nothing depends on this crate.** The GameInput COM object and the in-game menu (L5) are not built. A focused
poll costs about 170 µs, so the plan polls on its own thread (`tasks.md:183`). Tests: 18, 1 ignored.

### 2.16 `crates/upscale` — DLSS and FSR, standalone

- One `Upscaler` trait (`lib.rs:252-269`).
- `Inputs<R>` with a `check()` that refuses bad sizes, jitter or non-finite values.
- The layer's own conventions (`lib.rs:1-21`): jitter in render pixels, +y down; motion is previous −
  current.
- `dlss.rs`: Streamline 2.14.16, loaded at run time.
- `fsr.rs`: FFX API 2.4.0, the newest 3.1.x provider.
- `scene.rs`: an analytic jittered scene with exact motion vectors and supersampled truth.

Measured at 2560×1440 output (`agents/upscale.md`):

| mode | scene | DLSS | FSR 3.1 | bilinear |
|:--|:--|--:|--:|--:|
| quality | moving | 30.96 dB | 33.69 dB | 26.50 dB |
| performance | moving | 31.30 dB | 29.71 dB | 24.76 dB |

DLSS costs 1.0-1.15 ms and FSR 0.4-0.5 ms a frame on the card.

Not wired into the runtime (Build 7). Mode names differ from the registry: `upscaler.mode` uses `dlaa` and
`ultra_performance`, while `Mode::parse` takes `native-aa` and `ultra-performance`. Tests: 11.

### 2.17 `crates/config` — the settings registry

- `REGISTRY` (`lib.rs:136-170`) declares **30 options**: `game.*`, `display.*`, `upscaler.*`,
  `title.fsr_quality` (value 4 is refused: "black screen"), `memory.*`, `shaders.wave64`, `diagnostics.*`,
  `dstorage.bcpack_tables`, `limits.*`, `launcher.theme`, `menu.key`.
- Each `Setting` has a type, range, default, Live/Restart and Stable/Experimental, and refused values with
  reasons.
- `Config::parse` / `to_toml` keep explicit values even when they equal the default, keep unknown keys, and
  carry a `schema` (`SCHEMA = 1`, `lib.rs:183`).
- `profiles.rs` holds 9 built-in test profiles: dev-machine, series-x, series-s, mid-pc, low-pc, and four
  stress profiles (`profiles.rs:66-121`).

**The layer never reads `rebuild.toml`** ("C2", still todo, `tasks.md:178`). Runtime behaviour comes from
environment variables, some of which the launcher derives from the config (`apps/launcher/src/play.rs:37-45`).
Tests: 14.

### 2.18 `apps/rebuild` — the bins

| bin | build | what it does |
|:--|:--|:--|
| `rebuild` (`main.rs`) | — | placeholder: builds a `Uma` (4 windows, 3 GiB) and prints sizes; ignores arguments |
| `runtitle` | 3, 5 | the title run (1.3); `--run`, `--seconds N` (60), `--sample N`; uses `crash.rs`, `watch.rs`, `sampler.rs` via `#[path]` |
| `maptitle` | 3 | maps every image into `Region`s and binds (no title code); prints bound %, misses by tier |
| `imports` | 3 | import census → `agents/loader-imports.csv`; `--scan <dir>` parse sweep; `--getproc` → `agents/loader-getproc.csv`; hashes the game folder before and after |
| `runtimecensus` | 3 | classifies the census's missing imports into host / override / console tiers |
| `exitprobe` | 3 | finds which xmem allocation pattern makes process teardown slow; spawns capped children (`REBUILD_MEMORY_BUDGET_MB=3072`); patterns `ww`, `wwmany`, `heap`, `place`, `placeone` |
| `gputest` | 1, 2 | the uma proofs: default (GPU writes, CPU reads, no copy), `--bare` (control), `--tiled`, `--arena`, `--scale GB`, `--ring`, `--labels`, `--vram` |
| `uploadbench` | 0 a | write-watch + diff + upload matrix: Shadow/Hash × Copy/Scatter; `--tiled`, `--mb`, `--change` |
| `listbench` | 0 b | the synthetic frame through the translator into real D3D12 lists, 1-6 threads |
| `census` | 4 | every program in `shaders.rpf`: decode, re-encode byte-exact, one CSV row each |
| `translate` | 4 | GFX10 → HLSL → DXC over a corpus copy; modes `coverage`, `lift`, `ps`, `vs`, `cs`, `v1` |
| `oracle` | 4 | pairs the fork's warm translation cache with containers → the oracle set, outside git |
| `oraclediff` | 4 | structural diff of our DXIL vs the oracle; `--self --mutate`, `--sig`, `--ties`, `--audit` |
| `sirtest` | 4 | random SIR programs → HLSL → DXC → GPU, compared lane by lane with the 64-lane interpreter |
| `upscaletest` | 7 | DLSS and/or FSR on the synthetic scene: NaN/black checks, PSNR, stability, sign sweep |

Other bins live in their crates: `vfsbench` (vfs), `dsbench` and `bcbench` (dstorage), `gdkguids` and
`gdkslots` (gdk), `pipelines` (pipelines), and `fake-layer` (launcher).

Note: `imports` and `oracle` write their CSVs into the tracked `agents/` folder through `CARGO_MANIFEST_DIR`,
so running them rewrites committed docs.

### 2.19 `apps/launcher` — the desktop launcher

egui/eframe 0.31. It depends on `rebuild-config` and `rebuild-vfs`. Two binaries: `rebuild-launcher` and
`fake-layer`.

| module | what |
|:--|:--|
| `model` | settings widgets generated from `REGISTRY` |
| `home` | the app folder: `REBUILD_LAUNCHER_HOME`, portable, or `%APPDATA%\triumph-rebuild` |
| `library`, `gamemeta` | game detection (a `MicrosoftGame.config` for Scarlett), name and key art |
| `store`, `prefs` | `rebuild.toml` with `.bak` recovery; `launcher.toml` |
| `syscheck` | 8 checks: GPU, driver, tiled tier, SM 6.6, DXC, VRAM (8 GiB provisional), ReBAR |
| `title` | title settings written to a **copy** of `settings.xml` (`fsrQuality`) |
| `mods` | conflicts from `vfs::ModSet::conflicts` |
| `play` | `command_in` (`:27`): `rebuild.exe --config`, `REBUILD_FRAME_LOG`, `REBUILD_SESSION_DIR`, `REBUILD_LIMIT_*` |
| `session` | a folder per launch, `session.toml`, retention |
| `frameline`, `runs` | parse the frame line and `summary.toml`; the profile test matrix |
| `screens/`, `ui/` | the top-bar design, dark and light themes |

CLI modes: `--check`, `--matrix <dir>`, `--shots <dir>`.

`fake-layer` stands in for `rebuild.exe` in tests: it writes frame lines, `summary.toml` and a crash file.

Measured: `--check` passes 8/8 on the dev machine; the original `settings.xml` hash is unchanged. Tests: 58.

The seam: Launch runs `rebuild.exe`, which is still the placeholder. The real run contract (frame log, session
folder, run seconds) is implemented in `crates/runtime` and used by `runtitle`.

---

## 3. The memory model

### 3.1 The decision (`plan.md` decision 1)

Three regions, each with one rule:

```
GPU-owned    render targets, depth, UAV-written buffers, textures after first upload
             -> tiled VRAM: reserved resources, 64 KiB tiles at console addresses, shared by address
             -> never copied back
CPU-written  the title's CPU-side data the GPU reads
             -> host memory with write-watch; changed 64-byte chunks uploaded on an async queue
                behind a fence; the submit thread never copies
GPU->CPU     fence labels, query results, anything the title's CPU polls
             -> a small explicit CPU-visible region, written by the GPU
```

The reason: the old layer gave the title plain CPU memory and kept a mirror, copying 83-115 MB in and 60-65 MB
back each frame. 19 A/B experiments on that structure were neutral or worse (`README.md:85-91`).

### 3.2 Console addresses are GPU addresses: windows and tiles (`crates/uma`)

- The address space is cut into **4 GiB windows** (`uma/src/lib.rs:37`) of **64 KiB pages** (`:36`).
- `Uma::place(address, len)` (`lib.rs:113`) requires 64 KiB alignment and a range inside one window. It
  gives each page not yet backed a card page (reusing freed ones first), counts users per page, and returns
  only the newly backed `(page, card)` pairs.
- `retire` (`lib.rs:152`) returns the card pages whose last user left.
- A refusal leaves the state unchanged.

On the GPU (`uma/src/tiles.rs`):

- Each window is one reserved 4 GiB buffer (`CreateReservedResource`, `:86-93`).
- Card pages live in 64 MiB heaps of 1,024 tiles (`:17`). `map` coalesces runs and issues one
  `UpdateTileMappings` per (window, heap) group.
- Tiled resources tier 2 is required, so unmapped reads return 0 (`:83-85`).
- `alias` maps a second resource (another buffer, or a `64KB_UNDEFINED_SWIZZLE` render target) onto the same
  card pages: **sharing by address**, with no copy.
- `va(address)` = window GPU VA + `address % 4 GiB` (`:224`). A console address becomes a host GPU address
  with one divide and one add.

**The arena** (`uma/src/arena.rs`) holds committed resources for what cannot be tiled. A partial overlap is
refused (`:68-71`). Measured: only `D32_FLOAT_S8X24` and `D24_UNORM_S8` need it on this card
(`runs/build1-tiles.md`).

Measured (Build 1):

- 7 proofs on the card;
- 5,120 × 2 MiB placements (10 GiB) at random addresses, 0 wrong, no device removal;
- uploads onto tiles show no measurable penalty.

### 3.3 The upload ring and the 64-byte chunks (`crates/uma`)

`chunks.rs`: write-watch names the 4 KiB pages written. Each is diffed in 64-byte chunks in one of two ways:

| diff | compares against | memory cost |
|:--|:--|:--|
| `diff_shadow` | a full copy | the whole region |
| `diff_hash` (chosen) | a u64 per chunk | 12.5 % of the region |

Adjacent changed chunks join into runs. `mark_submit_thread()` sets a thread-local flag, and `gather` panics if
title memory is copied on the submit thread, in every build (`chunks.rs:109-120`).

`ring.rs` (`Ring`, `:217`):

- Its own async compute queue and a shared fence.
- **3 slots** of 96 MiB staging (`:39-40`).
- A persistent worker pool (`rebuild-diff-N`) and a coordinator thread (`rebuild-upload`).
- `flush()` (`:267`) only sends a message and returns a fence value. The title's queue does
  `Wait(fence, V)`, so the GPU waits and the CPU does not.
- The coordinator (`:294-439`), per flush:
  1. waits for its slot;
  2. reads write-watch with reset;
  3. hash-diffs on the pool, leaving the sent hashes alone (the new ones are pending);
  4. keeps a slot's worth of runs in console order and carries the pages holding the rest to the next flush;
  5. gathers with window-relative chunk indices;
  6. records **one scatter dispatch per window** (each binds the window's VA as a root UAV);
  7. signals, then commits the pending hashes of what it sent (the scatter is queued ahead of the Signal
     every user waits for). A failed flush commits nothing and carries every page it read.

Measured (Build 2, `runs/build2-ring.md`): `flush()` plus queuing the GPU wait takes 32.5 µs median and
72.5 µs at worst. 34.2 MB in 341,775 runs lands in 19.4 ms on 4 workers, byte-exact after 60 frames.

Build 0 (a), `runs/build0a-upload.md`:

- the link ceiling is 12.64 GB/s;
- hash diff + scatter + 4 workers costs 16-30 ms of worker wall time at the title's shape;
- one `CopyBufferRegion` per run took 146 ms just to record at 440k runs, so it was rejected;
- the detection floor (watch + diff with nothing changed) is ~13-17 ms of 4 workers (`progress.md:113`).

Risks in `ring.rs` that no doc mentions:

- Hashes are marked sent during the diff. A failed flush (more than one slot of change) therefore loses those
  chunks.
- A run can cross two contiguous regions.
- The region order is assumed sorted, not checked (`ring.rs:215-216`).

### 3.4 Labels: the GPU writes into title memory (`uma/src/labels.rs`)

`LabelMemory::open` uses `ID3D12Device3::OpenExistingHeapFromAddress` over the title's own allocation and
places a buffer over it. `write` is `WriteBufferImmediate` in `MARKER_OUT` mode. Title memory, write-watched
included, opens as a heap, and so do pagefile-section views.

Measured (`runs/build2-labels.md`): the label is seen 64.6 µs **before** the fence reports, 200/200 in order,
never early. The CPU-relay fallback lands 17 µs after the fence.

In the title run, `xlist`'s label slots (0x238/0x240) write `WriteBufferImmediate` MARKER_OUT through
`ListHost::label` (`list.rs:1301-1329`).

### 3.5 XMem, physical pages and placeholders (`crates/runtime`)

This is what the title run actually uses today.

**XMem placement** (`runtime/src/xmem.rs`):

- Graphics memory (`GRAPHICS = 3`) is placed inside one 4 GiB window and write-watched. So "its CPU address
  *is* its GPU address: window `addr >> 32`, offset `addr & 0xFFFF_FFFF`" (`xmem.rs:6-9`).
- `address_range(flags)` (`:38-47`): 32-bit memory is placed below 4 GiB, everything else above.
- `alloc_alignment` is `1 << max(4, (attr >> 24) & 0x1f)`.
- `host::virtual_alloc` (`:125-186`) strips console page bits and masks protection to host bits.
  - A fixed base goes straight to `VirtualAlloc2`.
  - Otherwise it searches window by window with a `VirtualQuery` walk (`free_gap`, `:190-212`) and asks for
    that exact address.
  - It **never** uses `MEM_ADDRESS_REQUIREMENTS`, because with write-watch on a full 4 GiB window the kernel
    spun unkillably (`xmem.rs:147-150`; one of the reboots, section 8).

**The 13 xmem exports** (`exports.rs:121-303`):

| export | behaviour |
|:--|:--|
| `XMemAlloc` / `XMemFree` | the title's `__XMemAllocationHooks__` once installed; before that, the default heap |
| `XMemAllocDefault` / `XMemFreeDefault` | `ChunkHeap`: first fit over 64 MiB write-watched graphics chunks; requests of 32 MiB or more go straight to `virtual_alloc` (`:19-87`) |
| `XMemVirtualAlloc` | a 64 KiB-page reserve with a null base becomes a **placeholder** (`physical::reserve_placeholder`); `MEM_REPLACE_PLACEHOLDER` is recorded |
| `XMemVirtualQuery` | the host result plus the real XMem flags |
| physical pages | `XMemAllocatePhysicalPages`, `XMemFreePhysicalPages`, `XMemMapPhysicalPages` (a null page list unmaps), `XMemMapPhysicalPagesScatter` |
| `XMemTransferMemory` | always TRUE |
| `XMemGetWorkingSetStatistics` | FALSE: layout unknown |
| `XMemIsToolsMemAvailable` | FALSE |

**Physical pages** (`runtime/src/physical.rs`):

- Each allocation is one pagefile section (`CreateFileMappingW`), charged to the budget first.
- Frames are 64 KiB views placed into placeholders: `map` splits the placeholder, then
  `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)`.
- Remapping keeps contents, and one frame can be mapped at two addresses (aliasing).
- `frame_ptr` maps a whole section lazily so DirectStorage can write by frame number. `drives.rs` retries a
  frame that does not exist yet (12 × 75 ms), because the title sometimes names one before creating it.
- `gputest --labels` showed these views open as D3D12 heaps, so the GPU can read them in place. That is a
  measured capability; nothing in the runtime opens them yet.

**Placeholders and write-watch** (`xmem.rs:137-142`, `:214-233`): placeholders are never write-watched. A free
with `MEM_PRESERVE_PLACEHOLDER` keeps the region recorded.

### 3.6 D3D's allocator 0x80 and the arena assert

On the console, D3D takes resource memory through the title's XMem hooks. The title's render setup:

1. pushes an XMem scope for allocator 0x80 (D3D) at `0x145C8F990`;
2. its hook dispatcher (`0x144DEB430`, allocator list = attribute bits 16-23) serves page-size-2 allocations
   from an arena (callback `0x1456A2A60`);
3. asserts that arena ends up full — the `int3` at `exe+0x247af2d`
   (`claude-max-handoff.md:192-195`).

Ours does the same: `Accounted<M>` (`device.rs:539-569`) mirrors every committed resource or heap allocation as
`XMemAlloc(bytes, D3D_ATTRIBUTES)`, where `D3D_ATTRIBUTES = (0x80 << 16) | (2 << 14)` (`device.rs:533`), and
frees it with `XMemFree`. The title's address is bookkeeping only: the GPU bytes come from the inner memory.

### 3.7 What the title run uses vs what is proven

| piece | proven | used by the title run |
|:--|:--|:--|
| tiles at console addresses | Build 1 (`gputest --tiled`) | no: resources use `HostMemory`, one 1 GiB mapped UPLOAD heap with a bump allocator whose `free` keeps the bytes (`d3d12x/src/memory.rs:43`) |
| the upload ring | Build 2 (`gputest --ring`) | no |
| GPU labels into title memory | Build 2 (`gputest --labels`) | yes, through xlist's label slots |
| XMem in one window, write-watched | host tests | yes |
| physical pages as views | host tests, GPU open measured | yes (CPU side); not opened by the GPU |

`tasks.md:45` keeps "direct GPU read of title memory vs the ring, per resource" open for Build 5.

---

## 4. The GPU path in depth

### 4.1 The object layout every console object shares

From `docs/research/d3d12x-device.md`, `agents/d3d12x-object-layouts.csv` and `d3d12x/src/object.rs:27-55`:

| offset | field | value |
|:--|:--|:--|
| +0x00 | method table | a static table built by us |
| +0x08 | bits (u32) | 0 |
| +0x0C | reference count | atomic |
| +0x10 | the object's own fields | ours |

**Title code reads object fields directly** (inlined), so the memory layout matters, not only the method
table. The layouts CSV (26 rows) records every field the title reads:

| object | field the title reads |
|:--|:--|
| device | +0x10 null-CB address; **+0x18 = 1** (calls instead of inline packets) |
| queue, allocator, list | type at +0x10 |
| list | `put` and 4 packet limits at +0x18..+0x40 (**all 0**); root user-data headers at +0x140 / +0x340; a pipeline shadow at +0x540..+0x560 (the title copies a pipeline's 32-byte packet to +0x544) |
| pipeline | its 32-byte packet at +0x10 |
| fence | the value cell |
| descriptor heap | placement and increments 32/16/128/128 |
| resource, heap, root signature, query heap, command signature | nothing past +0x10 |

"Is this object ours?" is answered everywhere by comparing the method-table pointer (`object::is`, `:75`).

### 4.2 Slot tables and traps

- Slots are numbered by byte offset (offset = slot × 8).
- Tables are built at first use from `(offset, name, fn)` lists.
- **Every unwritten slot is a trap:** one relaxed counter add per call; on the first call it records the
  time and tells the host once. It returns `E_NOTIMPL` for HRESULT methods and 0 for the rest
  (`d3d12x/src/slots.rs:128-152`, `xlist/src/console/slots.rs:110-149`). The return kind matters: the fork
  returned `E_NOTIMPL` from 0x4a0, and the BVH builder crashed using it as a pointer.
- Every run therefore names the next method to write. `runtitle`'s device report lists the traps hit.

| object | slots | implemented | where |
|:--|--:|:--|:--|
| device | 160 (0x000-0x4f8) | IUnknown, private data, `SetName`, `GetNodeCount`; queues 0x40 / 0x180; fence 0x120; allocator 0x48; lists 0x60 / 0x248; frame 0x270-0x288; pipelines 0x50 / 0x58 / 0x1f0 / 0x2b0; the 27 d3d12x slots | `runtime/src/device.rs:421-462` |
| queue | 40 | IUnknown, `GetDevice`, markers 0x58-0x68 (no-ops), Execute 0x50 / 0x118, `PresentX` 0x100, Signal 0x80 / 0x110, Wait 0x88, timestamp frequency 0x90, `GetDesc` 0xe0 | `runtime/src/queue.rs:236-265` |
| fence | 24 | slots 0-12, incl. `GetCompletedValue`, `SetEventOnCompletion`, `Signal`, `GetGpuAddress` 0x60 | `runtime/src/fence.rs:228-246` |
| command list | 160 | 66 | `xlist/src/console/list.rs:1374-1441` |
| allocator | 24 | 7 | `xlist/src/console/alloc.rs:272-280` |
| resource | 32 | 12 (Map, Unmap, GetDesc, GetGPUVirtualAddress, GetHeapProperties, common) | `d3d12x/src/resource.rs:173-183` |
| heap | 32 | 8 | `resource.rs:199-203` |
| descriptor heap | 32 | 10 | `d3d12x/src/heap.rs:198-206` |
| root signature, query heap, command signature | 32 | common only | |
| pipeline | 16 | 8 | `runtime/src/pipeline.rs:104-118` |
| DXGI device, adapter, factory, output | — | QI, parent, adapter enumeration, `GetDesc`, the mode list | `runtime/src/dxgi.rs` |

Device slots 0x1c8 and 0x218 (pipelines from console code) trap **on purpose**. 0x218 is not an upscale call;
it creates pipelines (`plan.md:121-126`).

**The slot CSVs** (from the device-research lane, PR #19):

- `agents/d3d12x-device-slots.csv`: 73 rows covering all 160 device slots, with name, PC equivalent, arguments,
  return kind, whether the title calls it, and the fork source. 0x2b0 (stream pipelines) is "the bulk of
  ~60,000 pipelines".
- `agents/d3d12x-child-slots.csv`: 74 rows for list, queue, fence, resource, heap, allocator and DXGI objects,
  with this title's offsets beside the 2020 ones. Newer titles add two queue methods at 0x70/0x78 and two list
  methods at 0x1d8/0x1e0, shifting later slots by +0x10 (`queue.rs:3-5`).

Struct-return methods (`GetDesc`, device 0xc8/0xd0/0x360) are called in either `(this, out)` or `(out, this)`
order depending on the caller's ABI. They are accepted both ways, decided by which argument carries our method
table (`object.rs:244`, `thunks.rs:32`).

### 4.3 Console objects vs host objects

| console object | ours | host side |
|:--|:--|:--|
| device | static 0x58-byte object | one `ID3D12Device` (FL 12_1) |
| queue | 0x40 object, `&Queue` at +0x38 | a host queue + a bridge fence + a progress fence |
| fence | 0x40 object, value cell at +0x10 | a host fence + a watcher thread that writes the cell with `fetch_max` |
| allocator | `AllocatorObject` | host allocators; 1 MiB upload chunks for 0x4a0 |
| list | `ListObject`, `ListState` from +0x960 | one or more host graphics lists (segments) |
| resource | `Resource`; GPU address = console address | a host buffer or a typeless host texture |
| descriptor heap | registered range in title memory | a host heap mirror plus the shadow table |
| root signature | the title's blob | a host root signature from that blob |
| pipeline | console object with `Key` and pool handle | a host PSO from the pool, or none |
| query heap, command signature | object | host object, or none if the host refuses (console argument type 0x69) |

Queue and fence reference counts are constants. Objects are leaked for the run.

### 4.4 Descriptors: records, mirrors and the shadow table

**Records in title memory** (`d3d12x/src/records.rs`). The title copies descriptors as bytes, between heaps and
into rings, so every create call writes a self-describing record at the handle, not a hardware descriptor:

- **32-byte records:** `w0 = 0x5844_0000 | kind` ('XD'). Kinds: CBV 1, SRV buffer 2, UAV buffer 3, Entry 4,
  and the null SRV/UAV/CBV 5-7. `w7` is a rotate-xor check over w0-w6, so stray bytes never decode.
- **Buffers are named whole:** format, 64-bit address, elements, stride and a RAW flag. There is no table
  entry, because the title makes ~14,000 CBVs a frame at new addresses.
- **Textures, RTVs and DSVs** are `Entry{index, generation}` into a generation-checked table; a stale record
  decodes to nothing.
- **16-byte records** for samplers, deduplicated by their 13 description words.

**Heaps** (`create_descriptor_heap`, `heap.rs:670`):

- Handles are addresses, so the CPU and GPU starts are the same.
- Increments are `[32, 16, 128, 128]`.
- The call always reads 16 bytes after the description as `{address, size}`. If they fit, the heap is
  **placed** in title memory; otherwise we allocate it.

**The host mirror of a shader-visible view heap:**

- `Limits::probe` gives the heap size; tier 2 is 1,000,000 views.
- The null index `P = views − views/4`, which is **750,000** at tier 2 (`heap.rs:87`).
- For i < P, host index i = console index i, and a buffer's primary view at i is a **raw SRV** of exactly its
  bytes from a 16-byte-aligned start.
- Index P is a null view.
- The **alternates** (the descriptor's own kind: CBV, typed SRV, UAV) sit in (P, views): at P+1+i when the
  heap fits, else from a free-list pool. The title's 1,245,184-slot heap uses the pool.
- Indices at or past P are written to title memory but not mirrored; they are counted and noted once.

**The shadow table** (`plan.md` decision 9; `views.rs:166-201`):

- 16 bytes per console heap index: `[stride, records, alternate index, lead]`.
- Read by shaders at `t0, space101`.
- It gives indexed buffer loads the console's exact bounds (an index at or past the record count reads 0).

| view | stride | records | alternate |
|:--|:--|:--|:--|
| CBV | 1 | bytes | host CBV if 256-byte aligned, else null (counted) |
| raw | 1 | bytes | raw UAV for a UAV |
| structured | stride | elements | raw UAV for a UAV |
| typed | element bytes | elements | typed SRV/UAV if element-aligned, else null |

`lead` is the bytes from the aligned raw start to the descriptor's first byte.

**Mirroring** (`heap.rs:371-440`) reads a slot with a volatile load, fingerprints it, and rebuilds host views
only when the fingerprint changed. A thread-local 65,536-slot cache copies buffer views (5-11 ns) instead of
creating them (~120 ns). `copy_descriptors*` validates both ranges (a registered heap or committed memory)
before the byte `memmove` and resync, and refuses bad copies. The fix for a destination at `0x3b7ad918130`
that was unmapped is noted, not explained (`claude-max-handoff.md:210`).

**The open seam** (verified for this document): nothing outside `crates/d3d12x` calls `shadow_va`,
`view_index_limit`, `own_kind_gpu`, `sync` or `sync_heap`.

- xlist forwards the title's root-parameter indices unchanged (`list.rs:1075-1157`).
- A title `Set*RootSignature` binds the host root signature built from **the title's blob**
  (`list.rs:1066`), not the library root signature the translated PSO was compiled with (section 4.5).
- How title root arguments become the library layout (user data at b0, shadow table at `t0 space101`) is not
  yet built.
- UAV clears still map through `HeapBinding` to the raw primary, not `own_kind_gpu` (`list.rs:1271`).

### 4.5 Root signatures and pipelines

**Two kinds of root signature:**

1. **The title's** (device 0x80, `d3d12x/src/root.rs:45-62`): a host root signature from the title's blob.
   The console flag bits 0x1000000 and 0x2000000 (which the host rejects) are stripped by our wrapper of
   `D3D12SerializeVersionedRootSignature` (`exports.rs:432-447`, which masks to 0xfff and remembers the bits).
2. **The library's** (`pipelines/src/layout.rs`), one fixed shape for every translated program:

| parameter | compute | graphics |
|:--|:--|:--|
| 0 | 16 root constants at `b0` (user data) | VS user data |
| 1 | root SRV `t0 space101` (shadow table) | PS user data |
| 2 | — | shadow table |

The flags always set both `*_HEAP_DIRECTLY_INDEXED` bits (the fork lost 311 pipelines without them), and
immediate samplers become static samplers in `space100`.

**The program library** (`library.rs`):

- File: `"RBPIPLIB"` v2, built offline by `pipelines build` from the translator's DXIL.
- Records: programs sorted by (hash, stage), layouts, one root per compute layout and **one per
  (VS layout, PS layout) pair**.
- A content hash is verified in parallel at load.
- Program hash: the first 8 bytes, big-endian, of the BLAKE3 of the DXBC container (`stream.rs:18-25`).
- Default location `D:\victory-data\pipelines\library.bin`, or `REBUILD_PIPELINES_LIB` /
  `REBUILD_PIPELINES_OUT` (`runtime/src/pipeline.rs:54-59`).
- Rebuilt in format 2: 2,837 programs, 82 root signatures.

**The pool** (`pool.rs`):

- `request` deduplicates by `Key` and returns a `Handle` at once.
- Two queues: the title's first, then prewarm.
- Slots never move, so `get` is lock-free.
- `wait` runs an unstarted creation inline on the waiting thread.
- Threads: logical processors (capped by `REBUILD_LIMIT_CPU_CORES`) − 2, clamped to 1-12.
- `PsoCreator` tries the `ID3D12PipelineLibrary1` cache, then the driver, and saves on drop.

**On-demand translation** (`ondemand.rs`, PR #31) covers programs not in the library, such as the BVH
builder's 80:

- the container is copied when offered and translated on a pool worker;
- results are cached at `<dir>\ondemand\<hash>.od`;
- each use re-lifts and compares the HLSL hash, so a translator change forces a DXC recompile;
- only wave-agnostic compute programs are translated; there is **no on-demand path for graphics**.

**Creation slots** (`runtime/src/pipeline.rs`):

| slot | what | how |
|:--|:--|:--|
| 0x58 / 0x1f0 | compute | `offer_compute`, then `pool.request` |
| 0x50 | graphics | `stream::parse_graphics` over a 656-byte desc |
| 0x2b0 | stream | `stream::parse_stream` with `Numbering::Console`, max 64 KiB |

The title always gets a valid object back (`hand_out`, `:222-232`). A program missing from the library means
no host PSO. Measured: 14 graphics pipelines created, 0 refused (PR #30).

**Stream subobject numbering** (`stream.rs:205-241`):

| types | console | PC |
|:--|:--|:--|
| 0-22 (root signature … view instancing) | same | same |
| amplification shader / mesh shader | 23 / 24 | 24 / 25 |
| depth-stencil 2, rasterizer 1, rasterizer 2, serialized root signature | — | 26-29 |
| console types ≥ 25 | **unknown** | — |

Unknown console types are skipped only when `REBUILD_STREAM_SUBOBJECTS=type:bytes,...` supplies a size;
otherwise the parse stops (`Refusal::UnknownSize`).

Refused outright: mesh/amplification, HS/DS/GS, active stream output, view instancing, more than 8 targets,
non-DXBC bytecode.

### 4.6 Command-list translation (`crates/xlist/src/console`)

**No recording buffer.** The list's `put` and limits are 0, so the title's inline packet writer always falls
back to a method call. Each call is translated as it arrives, on the recording thread, straight into a host
`ID3D12GraphicsCommandList` through its cached vtable (`console/mod.rs:18-23`, `call!` at `list.rs:215`).

- **State shadow** (`list.rs:102-131`) holds both console and host terms. Repeated sets are elided and
  counted. The same shadow replays state after a cut.
- **Addresses** map through a cached `MemoryRange` (`va`, `:247`). Descriptor tables map through
  `HeapBinding::map` (`console/mod.rs:103`): `host_base + (offset >> log2 console_stride) × host_stride`.
- **Unknown objects or addresses** are refused and counted. Barriers on unknown resources become a global UAV
  barrier (`:890-1024`).
- **`pso_missing`** (`:105`, `set_pipeline` `:302-321`): after a pipeline with no host PSO is set, every draw,
  dispatch, DispatchMesh and ExecuteIndirect is **dropped** (counted in `ListStats::skipped`) until a
  pipeline with a host object is set. Running them with the previously bound PSO was a bug, fixed in
  `crates/xlist` (`progress.md:127-129`).
- **Memory waits** (slots 0x248/0x250, `memory_wait` `:1331`) are console "wait until this memory holds"
  packets. The list is cut (`split`, `:341-369`): the host list closes, a new one opens, and a
  `Segment{list, waits}` is pushed with state replayed. Reset folds segments back and keeps the spares.
  `MemoryWait::holds_for` uses D3D12 comparison numbering (the fork's reading, unverified).
- **Labels** 0x238/0x240 write `WriteBufferImmediate` MARKER_OUT. Timestamp labels are refused.
- **Pipe syncs** 0x418/0x420 become a global barrier only if work ran since the last one.
- **0x4a0** returns real upload memory owned by the allocator and freed at allocator reset.
- **0x1f0 `CopyMemoryToMemoryX`** becomes `CopyBufferRegion` on the resources behind each address.

Not carried: `CopyTextureRegion(X)`, `CopyTiles`, `ResolveSubresource` and queries on lists (trapped). UAV
counters are noted, not carried.

### 4.7 Submission (`crates/runtime/src/submit.rs`)

`execute` (`:52-94`):

1. prepends the presenter's frame-begin timestamp list on the first submission of a frame;
2. for each console list, takes its segments (lists that are not ours count as `FOREIGN`);
3. turns each segment's waits into queue-side actions;
4. batches host lists, up to 64 per `ExecuteCommandLists`.

`place` (`:148-166`) decides how each wait is kept:

| the waited address | outcome |
|:--|:--|
| a fence's value cell, already met | `Met` |
| a fence's value cell, a ≥ / > / == compare | `Fence`: a host queue `Wait` (`fence_value`, `:138-145`) |
| a fence's value cell, otherwise | `Bridge` |
| memory the CPU cannot read | `Refused`, dropped and counted |
| readable memory that already holds | `Met` |
| readable memory, otherwise | `Bridge` |

**The bridge** (`:222-283`): the host queue waits on its bridge fence for value n. A `memory-wait` thread polls
pending waits every 250 µs and signals each once the memory holds, keeping each queue's order. The ignored
card test shows a bridged cross-queue wait keeps GPU order without blocking the thread (under 20 ms).

### 4.8 Present (`crates/runtime/src/present.rs`)

- `PresentX(queue, planes, *Plane, params)` reads **plane 0, resource 0 only**. If another thread holds the
  presenter, the frame counts as busy and late.
- `Presenter::new`:
  - a borderless window on its own message-pump thread;
  - a flip-discard swap chain on the presenting queue's host queue, at the host display mode;
  - 3 buffers with a frame-latency waitable, maximum latency 2;
  - FP16 output in scRGB;
  - a timestamp query heap for `framelog::gpu`.
- `present()`:
  - **Copy path:** same format family, fits, 2D, not MSAA. It does a `CopyTextureRegion` and
    `SetSourceSize`. If `SetSourceSize` fails, it switches to the blit for good.
  - **Blit path:** a full-screen triangle with a shader compiled at run time by `D3DCompile`.
  - Recorded lists are cached by (source, size, path).
  - Then `Present(1, 0)` and a fence signal.
- **Capture:** `REBUILD_CAPTURE_FRAMES=n,m` or a `capture.request` file in the session folder writes
  `captures/frame-<n>.png`.
- `WM_CLOSE` writes the summary and exits.

### 4.9 Frame pacing (`frame.rs`)

A simple clock until Build 8:

- `SetFrameIntervalX` stores the interval (default 16,667 µs).
- `WaitFrameEventX` sleeps to the next multiple of the interval since the first call and returns `S_FALSE`
  on timeout.
- `ScheduleFrameEventX` is only counted.
- `GetFrameStatisticsX` returns count 0.

---

## 5. The shader recompiler in depth

### 5.1 The shape of the problem

The title ships **25,694** GPU programs (vertex, pixel, compute and mesh) in `shaders.rpf`, compiled for the
console's RDNA2 GPU (GFX10). They must become DXIL for the host, offline, with nothing compiled during a frame
(`plan.md` decision 4). The fork already translates all 25,694, but 7 pipelines are refused for wanting
64-lane waves, and its own analysis puts ~10-22 ms of GPU time in its translator's core choices. So the
**core is ours**, built around four ideas (`plan.md:107-114`):

- EXEC (each lane's active bit) carried as data, with live-mask analysis;
- console addresses as window GPU addresses;
- uniform values kept scalar;
- native 16-bit and wave ops, and 32-lane waves only.

All shader work reads a copy of the archive at `D:\victory-data\shaders.rpf`, never the game folder
(`claude-max-handoff.md:57-58`).

### 5.2 From blob to GFX10 code (`crates/shader`)

The containers nest like this:

```
shaders.rpf (SGD5) -> deflate records -> DXBC container -> DXIL part -> XDAT index
    -> XBCH blob (zlib) -> FSSM payload -> per-stage native records -> GFX10 code
```

| step | file | notes |
|:--|:--|:--|
| SGD5 records | `sgd5.rs` (vendored) | found by scanning for the marker `0x000DDCE7`, not via the header tables, which are not decoded (`:4-11`); sizes cross-checked; `records()` `:92` |
| DXBC parts | `dxbc.rs` (vendored) | `Container::parse` `:71`: `DXIL`, `ISG1`, `OSG1`, `PSG1`, `PSV0`, `RTS0` |
| native programs | `native.rs` (vendored, extended) | the console's GFX10 code hides inside the `DXIL` part; layout at `:4-73`; `native_programs()` `:420`; per-stage IO `ps_io` `:624`, `vs_io` `:672`, `ms_io` `:699`; `patched_code()` `:178` applies the console's export patch |
| signatures | `signature.rs` | 32-byte `ISG1`/`OSG1`/`PSG1` elements |

Each `NativeProgram` (`native.rs:155`) carries its stage, wave64 flag, VGPR/SGPR counts, LDS bytes, root
layout, used resources, stage IO and code. Merged stages' stage data is not read (`native.rs:66`).

### 5.3 Decode (`crates/shader/src/gfx10`)

- `decode()` (`mod.rs:73`) follows control flow from offset 0 rather than sweeping, because constant tables
  sit after the code. It recognises the compiler's far-jump pattern (`far_jump`, `:161`).
- Opcode names come from Mesa ACO's tables, the GFX10 column (`opcodes.rs`, vendored).
- `enc.rs` is our own **field model**: 23 layouts in which every bit is a tag, a named field or reserved.
  `encode()` (`:342`) rebuilds trailing dwords from the fields, so `round_trip()` proves the model names every
  bit the corpus sets.

Measured (`agents/shader.md:35-48`): **26,084 native programs, 43,952,121 instructions, 100 % byte-exact
re-encode, 0 unknown opcodes.**

### 5.4 The census and wave classes

`census` (`crates/shader/src/census.rs:82`, bin `apps/rebuild/src/bin/census.rs`) writes one row per program
to `agents/shader-census.csv`:

```
hash, code, stage, wave64, code_bytes, instructions, hints, exact, vgprs, sgprs, lds_bytes, dpp, sdwa,
lds, gds, scratch, b16, blocks, branch_scc, branch_vcc, branch_exec, branch_always, regions, loops,
structured, wave_class, wave_ops, wave_ops_divergent
```

`features::cross_lane` (`features.rs:132`) classifies per instruction, with no dataflow:

| wave class | rule | programs |
|:--|:--|--:|
| `agnostic` | nothing crosses a quad; quad ops are fine | 9,128 |
| `wave-wide-if-rfl-divergent` | only `v_readfirstlane_b32` | 4,631 |
| `wave-wide` | DPP beyond `quad_perm`, DPP8, `v_readlane`/`writelane`, `ds_(b)permute`, non-quad `ds_swizzle`, `v_mbcnt` (lane id), `v_permlane*`, bit counts on EXEC/VCC | 12,325 |

Corpus by stage (from the CSV; vs + ps + cs + ms = 25,694, the corpus the plan counts):

| vs | ps | cs | ms | es | gs |
|--:|--:|--:|--:|--:|--:|
| 2,191 | 12,056 | 2,682 | 8,765 | 195 | 195 |

Other corpus facts: 89.8 % are wave64, and 99.18 % have structured control flow; 20 compute programs are
irreducible and all 195 ES programs use indirect jumps.

### 5.5 Two things called `sir`

`crates/sir` holds two different pieces of work. The design doc reads as if they were one pipeline; they are
not.

**(a) The SIR test IR: the proof of the lane model.**

| file | what |
|:--|:--|
| `lib.rs` | a small SSA IR (`Op`, `:25`): ALU, wave sums and mins, ballots, `ReadLane`, `ReadFirstLane`, swizzles, structured If/Else/EndIf/Phi, `Load`/`Store` at window:offset, packed f16 |
| `lanes.rs` | `console()` (`:114`) is a reference 64-lane wave with EXEC as a u64; `doubled()` (`:198`) is what a 32-lane host wave can do |
| `classify.rs` | wave class from uniformity (`:20`) |
| `hlsl.rs` | `plan` (`:45`) and `emit` (`:57`) |
| `gen.rs` | random programs, EXEC, inputs, memory |

In `doubled()`, host lane h carries console lanes h and h+32, EXEC is a bool pair, and only 32-lane
reductions, ballots and reads are allowed. Inactive lanes contribute the identity element.

**Live masks** (`hlsl.rs:45`): a region becomes a real host branch when its condition is uniform or nothing in
it crosses lanes. Otherwise it is predicated: both sides run, EXEC narrows as data, and the phi becomes a
select.

**Emission:** agnostic mode is `[numthreads(64,1,1)] [WaveSize(32)]` with one console lane per thread; doubled
mode is `[numthreads(32,1,1)] [WaveSize(32)]` with `uint2` values. Memory lowers to
`ResourceDescriptorHeap[window]` at a 32-bit offset, with `WaveReadLaneFirst` for a uniform window.

What it proves (`runs/build4-sirtest*.md`):

- On the CPU: bit-exact against the 64-lane reference on 100,000 random programs, 100,000 branched programs
  and 50,000 with memory and half precision, including the reduction the fork refused.
- On the card (RTX 4070 Ti, DXC 1.8):
  - 300 runs exact (100 agnostic + 200 doubled), per-lane `WaveReadLaneAt` exact;
  - branches: 601 runs exact, 182 host branches and 124 predicated;
  - memory: 342 loads + 331 stores through real tiled windows, and 650 packed-half ops, all exact.

**(b) The real translator: GFX10 → HLSL text** (`lift.rs` + `lift/`). Decoded GFX10 goes **straight to HLSL**.
It does not pass through `sir::Op`, does not use `classify.rs`, and does not emit lane doubling or window
addressing.

- **Registers:** every SGPR/VGPR touched becomes a local `uint sN` / `vN`, and DXC builds the SSA
  (`declarations()`, `lift.rs:1388`).
- **Lane masks:** per-lane `bool`s (`vcc`, `m{n}`, `ex` for EXEC). Once EXEC narrows, every vector write is
  `if (ex) vN = …` (`vwrite_raw`, `:548`).
- **Dispatch:** `Lifter::inst` (`:594`) covers SOPP, SALU, mask ops, VALU, VOPC, VOP3P, `ds_swizzle`, DPP16
  `quad_perm` only (via `QuadReadLaneAt`), LDS, interpolation, fetch, export, SMEM, MUBUF/MTBUF and MIMG. SDWA
  and DPP8 are refused with a named reason (`:619-623`).
- **Console float semantics** in a `PRELUDE` (`:62-104`): legacy multiply, round-toward-zero `pkrtz`,
  `v_cmp_class`, cube helpers, saturating float-to-int, `bfe`. `v_fma_f32` becomes `mad`; `s_setreg` float
  modes are not modelled.
- **Resources** (`res.rs`): user data arrives as root constants at `b0`
  (`cbuffer SirUserData { uint4 sir_ud[16]; }`). Descriptor loads are traced back to their table and become
  heap indices into `ResourceDescriptorHeap[]` / `SamplerDescriptorHeap[]` (`res.rs:120-135`).
- **Two register spaces of our own:**
  - immediate samplers in `space100`, decoded by `sampler::decode` into static samplers and written to a
    `.samplers` sidecar;
  - the descriptor shadow table in `space101`.
- **Stages** (`lift/io.rs`):
  - pixel (`:315`): inputs from `SPI_PS_INPUT_ADDR`; interpolation modifiers; exports → `SV_Target{n}` /
    `SV_Depth`; `void main` when there are no outputs;
  - vertex (`:485`): `s_swappc_b64` (the fetch subroutine) becomes input-assembler loads; v0/v3 =
    `SV_VertexID` / `SV_InstanceID`; `exp pos0` and `exp param<n>` become position and output rows;
  - compute (`:601`): `numthreads` from the dispatch settings, thread and group ids, `groupshared` LDS,
    `s_barrier` → `GroupMemoryBarrierWithGroupSync`.
- **Control flow** (`lift/cfg.rs`):
  1. `blocks()` (`:73`) cuts basic blocks;
  2. `analyse()` (`:173`) finds dominators and refuses irreducible graphs (`:249`);
  3. `structure()` (`:380`) uses Ramsey's "Beyond Relooper";
  4. `lower()` (`:526`) emits `for(;;)` loops, `do{…}while(false)` merges, and a `sir_to` dispatch when a
     branch crosses constructs.

  Every host branch condition is wave-uniform: `scc`, `WaveActiveAnyTrue(vcc)` or `WaveActiveAnyTrue(ex)`
  (`:632`). Flow facts iterate to a fixed point, at most 16 passes (`:735`).
- **DXC** (`crates/pipelines/src/translate.rs`): `dxc -T {vs,ps,cs}_6_6 -E main -O3 -enable-16bit-types`
  (`DXC_ARGS`, `:28`), at most 4 processes at once, one retry if DXC dies. `REBUILD_DXC`, else the newest
  Windows SDK.

**What the lifted programs do not carry:** `[WaveSize(32)]` appears only in the SIR emitter and tests
(`hlsl.rs:244/252`, `lift/tests.rs:172`). Real programs run at the driver's wave size, which is 32 on NVIDIA.
The `translate` path keeps only `WaveClass::Agnostic` (`translate.rs:214`), yet `lift.rs:1231-1235` translates
`v_readfirstlane_b32` with the comment "the classifier checked it".

### 5.6 The oracle and `oraclediff`

**The oracle** is the fork's translation of each program, kept outside git (`D:\victory-data\rebuild-oracle`).

- The fork's cache key includes its own build fingerprint, so `oracle` (`apps/rebuild/src/bin/oracle.rs`) runs
  the fork's `translate --warm all --jobs 1` into an empty cache and pairs entries with containers **by
  creation time** (`:60`).
- The pairing is accepted only when the counts match and every stage agrees.
- Measured: 25,694 paired, 0 stage disagreements, 50/50 byte-identical on re-translation.
- The index is `agents/shader-oracle.csv` (`hash, stage, present, dxil_bytes, entry_bytes`).

**`oraclediff`** (`crates/shader/src/oraclediff.rs`, bin of the same name) is structural only; there is no GPU
in it.

- `bitcode.rs` is our own LLVM 3.7 bitstream reader. It resolves every `dx.op` call. The first bug was that
  DXC writes symbol names in 6-bit characters.
- `summarise()` (`:289`) reads the signatures, PSV0 resources, thread-group size, wave-size range and stage,
  and builds a histogram in five categories (memory, wave, control, barrier, io).
- `compare()` (`:432`) yields `Stage`, `SigIn`, `InputReads`, `SigOut`, `SigPatch`, `Bindings`, `Threads`,
  `WaveSize` and `Count` diffs.
- Named reasons are exempt: our spaces 100 and 101, the oracle's descriptor-kind table at `t255 space1`, the
  vertex/instance id order, `SV_Barycentrics`, and vertex inputs the oracle declares but never reads.

Measured:

- self-check: 0 diffs over 25,694; every injected mutation caught;
- v0 interface diffs: SigIn 1,231 → 35 → 0, SigOut 801 → 3 → 0, Bindings 525 → 4 → 0;
- along the way, 4 swapped oracle files were found (same-tick cache entries);
- the last two translator bugs (fp16 interpolation components, no-output pixel shaders) were fixed in PR #26.

### 5.7 Coverage now

| set | translated and compiled | source |
|:--|:--|:--|
| v0: one block, agnostic | **2,834 / 2,881** (PS 1,791 / 1,798 · VS 926 / 951 · CS 117 / 132) | `runs/build4-{ps,vs,cs}.txt` |
| v1: control flow, agnostic | **7,402 / 9,128 (81.1 %)** (PS 6,097 / 6,789 · VS 970 / 1,654 · CS 335 / 685); 0 DXC errors | `runs/build4-v1.md` |
| counted (execution matches the oracle) | **0 / 25,694** | `progress.md:18` |

The first v1 refusals (`agents/shader-cf.md:245-257`):

| refusal | programs |
|:--|--:|
| VS `s_buffer_load` through a descriptor built in code from a 64-bit address | 638 |
| `image_atomic_umin` | 239 |
| SDWA | 230+ |
| `v_ffbl_b32` | 86 |
| `s_getpc_b64` in PS/CS | 80 |

**Not attempted:**

- the 12,325 wave-wide and 4,631 readfirstlane programs (no lane doubling in the real translator);
- mesh (8,765), ES and GS;
- irreducible and indirect control flow (the design's node splitting and dispatcher are not built).

"Compiled" is not "correct": no translated title program has been executed against the oracle yet. That is
lane `shaderexec`'s job.

---

## 6. CPU-side console quirks

| quirk | cause | fix | where |
|:--|:--|:--|:--|
| `clwb` | the console CPU has it; the Intel host does not (illegal instruction) | a vectored handler, first in line, decodes `66 [REX] 0F AE /6`, overwrites exactly its bytes with NOPs and resumes; sites counted | `runtime/src/cpu.rs:42-115`, `:304-308` |
| `clzero` | AMD-only `0F 01 FC`, absent on the host | detoured per site (below) | `cpu.rs:131-301` |
| TLS slot 0 | the title reads its block at `array[0]` | blocks and indices swapped on every thread | `runtime/src/tls.rs` (1.3, step 3) |
| device type | the title reads `HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM\ConsoleMode` (GDK lookup at `exe+0x55deb88`); absent on a PC, it took itself for a PC (type 1) | the key gets a fake handle `0x7E570E11`; `ConsoleMode` answers 0x64 (Series X), or 0x60 (Series S) with `REBUILD_CONSOLE=series-s` or `s`; other keys go to the host; 0x68 would be a devkit | `runtime/src/registry.rs:3-31` |
| the arena assert | D3D's allocator 0x80 must fill the title's arena | every resource is also `XMemAlloc`'d with D3D's attributes | section 3.6 |
| console root-signature flags | 0x1000000 / 0x2000000 rejected by the host serializer | masked to 0xfff on a copy, remembered | `exports.rs:432-447` |
| other illegal instructions | — | `EXCEPTION_CONTINUE_SEARCH`, so the crash reporter names them | `cpu.rs:102` |

How a `clzero` site is detoured (`patch_clzero`, `cpu.rs:258-301`), in order of preference:

1. **A cave:** a run of at least 6 `int3`s ending on a 16-byte boundary within ±127 bytes. The site becomes a
   `jmp short` into it, then a `jmp rel32` to a stub that saves flags and rdx, zeroes the 64-byte line at
   rax, and jumps back.
2. **No cave in reach:** the site plus its next instruction become a `jmp rel32` to a stub that also runs a
   moved copy of that instruction. Only REX register-only forms are moved.
3. **Neither:** that single execution is emulated in the handler.

Stub pages are 64 KiB RWX within ±0x7000_0000 bytes (`stub_space`, `:197-237`). Measured on the title: 5
`clzero` sites detoured through the relocation path, 0 emulated; 1 `clwb` site.

The registry answer did **not** fix the frame-2 blocker (section 9.3).

---

## 7. Diagnostics and tooling

### 7.1 The crash reporter (`apps/rebuild/src/crash.rs`)

`install` (`:351-373`) stores the image table, parses `REBUILD_PEEK`, precomputes the path
`<REBUILD_SESSION_DIR>\crash-<pid>.txt`, and registers a first-in-line vectored handler plus an unhandled
filter. Every report goes to stderr, stdout and the crash file. Formatting uses a fixed 2,048-byte buffer, and
the file is opened per write with `CreateFileW(FILE_APPEND_DATA)`.

A report (`report_inner`, `:145-234`) contains, in this order:

1. the exception code and the faulting address;
2. for an access violation: read, write or execute, a `VirtualQuery` of the target, and the title budget;
3. all 16 general registers;
4. the peeks;
5. a real unwind of up to 16 frames with `RtlLookupFunctionEntry` / `RtlVirtualUnwind`.

Frames inside the title print as `image+rva`; others are marked `(host)`. Facts are written before anything
that could fault again, and a flag prevents recursion.

What `first_chance` (`:247-283`) reports:

| event | report |
|:--|:--|
| access violation | the first 4 are reported first-chance; the first inside title code also runs `on_fatal` (`runtitle`'s `device_report`) |
| C++ throw (`0xE06D7363`) | the first 6 decoded (`cpp_throw`, `:291-341`): the thrown type's decorated name, its first u32 (an HRESULT, for the BVH builder's `long`), and a 14-frame chain; every pointer checked |
| breakpoint (a title assert), fail-fast, illegal or privileged instruction | the first one reported, then `on_fatal`, then with `REBUILD_THREADS` every other thread |

**`REBUILD_PEEK`** (`:184-206`, `:353-363`) is a comma list. Each item is either:

- a hex preferred address in the exe, rebased by subtracting `0x1_4000_0000`; or
- `reg+hexoff`, e.g. `rcx+68`, `rsp+20`.

Each prints 8 bytes after `VirtualQuery` says the page is committed and readable.

**`REBUILD_THREADS`** (`other_threads`, `:375-452`) suspends each other thread, unwinds 12 frames and prints
the threads that have a title frame. The rest are counted. `runtitle` also calls this at `TIMEOUT`.

**Symbolization** (`:72-121`): title addresses are `image+rva` with no DbgHelp. Host addresses use DbgHelp
(`SymFromAddr`, lazily initialised), falling back to `module+offset`. `NO_SYMBOLS` turns DbgHelp off.
`watch.rs` sets it, because a watchpoint hit inside the host heap corrupted it when DbgHelp allocated
(`claude-max-handoff.md:223`). Our own frames print as `runtitle.exe+off`; the lead symbolises them offline
with a scratch script.

### 7.2 Watchpoints (`apps/rebuild/src/watch.rs`, `REBUILD_WATCH`)

- `REBUILD_WATCH=<hex address>` sets an 8-byte write watchpoint (Dr0, Dr7 RW=write LEN=8) on every thread.
- A `watch-arm` thread polls the thread list every 5 ms **for the first 120 s** and arms new threads.
- The handler prints the first 40 hits with the value (`<-- ZERO` if zero) and a 10-frame chain, then
  continues.

This is how the frame-2 blocker was traced (section 9.3).

### 7.3 The sampler (`apps/rebuild/src/sampler.rs`, `runtitle --sample N`)

Every N seconds it suspends `title-main`, takes its context, copies up to 256 KiB of stack, resumes, then
unwinds the copy (24 frames) and prints `image+rva` frames (or `same stack`). At the end every thread is
sampled once.

### 7.4 Frame capture, frame line and session folder

| what | how |
|:--|:--|
| PNG capture | `REBUILD_CAPTURE_FRAMES=1,5,…` or a `capture.request` file in the session folder → `captures/frame-<n>.png` |
| frame line | `REBUILD_FRAME_LOG`, one line a second; `REBUILD_MEASURE_FROM=after_loading` starts measuring at the first frame with a draw |
| summary | `summary.toml` in `REBUILD_SESSION_DIR` at `REBUILD_RUN_SECONDS`, at `REACHED` and at the end of `runtitle` |
| file failures | `REBUILD_TRACE_FILES`: the first 200 failed `CreateFile*` and the first 100 failed DirectStorage requests |
| refused shaders | `REBUILD_TRANSLATE_DUMP=<reason text>` writes their lifted bodies and root layouts |

### 7.5 Every `REBUILD_*` environment variable

All 47 names in `crates/` and `apps/`, from a grep of the source.

**The title run and the runtime:**

| variable | read at | effect |
|:--|:--|:--|
| `REBUILD_GAME_DIR` | `runtitle.rs:268`, `maptitle.rs:71`, `imports.rs:640`, `census.rs:57`, `oracle.rs:85`, `gdkguids.rs:30`, tests, launcher `main.rs:195` | the title's folder, read-only; required by the runners |
| `REBUILD_SESSION_DIR` | `runtitle.rs:66`, `crash.rs:364`, `framelog.rs:336,357`, `present.rs:678`, `fake-layer.rs:31` | session folder: `summary.toml`, `crash-<pid>.txt`, `capture.request`, `captures/` (the launcher sets it) |
| `REBUILD_FRAME_LOG` | `framelog.rs:355`, `fake-layer.rs:20` | frame-line file (the launcher sets it) |
| `REBUILD_RUN_SECONDS` | `framelog.rs:358`, `fake-layer.rs:33` | at N s of process age: summary, shut down, exit 0 |
| `REBUILD_MEASURE_FROM` | `framelog.rs:161`, `fake-layer.rs:34` | `after_loading`, or the default first present |
| `REBUILD_CAPTURE_FRAMES` | `present.rs:134` | presented frame numbers to save as PNG |
| `REBUILD_CONSOLE` | `registry.rs:27` | `series-s` / `s` → ConsoleMode 0x60; otherwise 0x64 |
| `REBUILD_MEMORY_BUDGET_MB` | `budget.rs:47` | physical-page budget (default min(12 GiB, RAM / 3)); `exitprobe` sets 3072 |
| `REBUILD_LIMIT_RAM_MB` | `limits.rs:33` | budget ≤ RAM / 2 and the job's commit cap |
| `REBUILD_LIMIT_VRAM_MB` | `limits.rs:34` | recorded only; not enforced |
| `REBUILD_LIMIT_CPU_CORES` | `limits.rs:35`, `pipelines/src/pool.rs:196` | affinity to the first N logical processors; caps pipeline workers |
| `REBUILD_LIMIT_CPU_RATE` | `limits.rs:36` | job hard CPU cap in percent |
| `REBUILD_PIPELINES_LIB` | `runtime/src/pipeline.rs:55`, `stream.rs:1041` (test) | path of `library.bin` |
| `REBUILD_PIPELINES_OUT` | `runtime/src/pipeline.rs:58`, `bin/pipelines.rs:55` | library folder (default `D:\victory-data\pipelines`) |
| `REBUILD_PSO_CACHE_DIR` | `pipelines/src/session.rs:30`, `bin/pipelines.rs:509,674` | PSO cache and session records (default `%LOCALAPPDATA%\triumph-rebuild\pipelines`) |
| `REBUILD_STREAM_SUBOBJECTS` | `runtime/src/pipeline.rs:250` | `type:bytes,…` sizes for unknown console stream subobjects |

**Diagnostics:**

| variable | read at | effect |
|:--|:--|:--|
| `REBUILD_PEEK` | `crash.rs:353` | 8-byte peeks at exe addresses or `reg+off` in crash reports |
| `REBUILD_THREADS` | `crash.rs:278` | at the first fatal exception, unwind every other thread |
| `REBUILD_WATCH` | `watch.rs:35` | hardware write watchpoint at a hex address; turns DbgHelp off |
| `REBUILD_SAMPLER_BEATS` | `sampler.rs:30` | print the sampler's per-step heartbeat |
| `REBUILD_TRACE_FILES` | `winfiles/src/lib.rs:394`, `dstorage/src/engine.rs:504` | log failed file opens and DirectStorage requests |

**DirectStorage and BCPack:**

| variable | read at | effect |
|:--|:--|:--|
| `REBUILD_BCPACK_TABLES` | `dstorage/src/bcpack/mod.rs:167`, `bcbench.rs:27` | the format tables (else `%LOCALAPPDATA%\triumph-rebuild\bcpack-tables.bin`); without them BCPack is refused |
| `REBUILD_BCPACK_CACHE` | `dstorage/src/cache.rs:154` | decode cache folder; `off` disables it |
| `REBUILD_BCPACK_CACHE_MB` | `cache.rs:159` | cache cap (default 8192) |
| `REBUILD_BCPACK_CAPTURE` | `dstorage/src/capture.rs:31` | save every BCPack request to this folder |
| `REBUILD_BCPACK_CAPTURE_MB` | `capture.rs:36` | capture cap (default 512) |

**Shaders and the oracle:**

| variable | read at | effect |
|:--|:--|:--|
| `REBUILD_SHADERS` | `translate.rs:53`, `oraclediff.rs:271,415`, `bin/pipelines.rs:104,664`, tests | path to a **copy** of `shaders.rpf` |
| `REBUILD_TRANSLATE_OUT` | `translate.rs:187`, tests | DXIL and `.samplers` output (default `D:\victory-data\translate`) |
| `REBUILD_TRANSLATE_DUMP` | `translate.rs:230` | dump refused programs whose reason contains this text |
| `REBUILD_PIPELINES_IN` | `bin/pipelines.rs:147` | translator output read by `pipelines build` |
| `REBUILD_DXC` | `sirtest.rs:30`, `pipelines/src/translate.rs:32`, tests, `launcher/syscheck.rs:172` | `dxc.exe` path instead of the newest SDK |
| `REBUILD_ORACLE_DIR` | `oracle.rs:84`, `oraclediff.rs:111` | the oracle folder (outside git) |
| `REBUILD_FORK_EXE` | `oracle.rs:137`, `oraclediff.rs:272` | the fork's exe, for `--verify` and `--ties` |
| `REBUILD_FORK_CENSUS` | `census.rs:305` | the fork's census TSV, for a cross-check |
| `REBUILD_FORK_WORLD` | `census.rs:308` | the fork's 1,433-program world-frame CSV |

**Upscaler, audio tests, launcher:**

| variable | read at | effect |
|:--|:--|:--|
| `REBUILD_STREAMLINE_DIR` | `upscaletest.rs:296` | Streamline DLLs |
| `REBUILD_DLSS_DLL` | `upscaletest.rs:296`, launcher `main.rs:1008` | `nvngx_dlss.dll` |
| `REBUILD_SL_LOGS` | `upscaletest.rs:295` | Streamline log folder |
| `REBUILD_FIDELITYFX_DIR` | `upscaletest.rs:308` | FidelityFX DLLs |
| `REBUILD_FFMPEG` | `audio/src/wma/tests.rs:148` | FFmpeg for the A5 comparison |
| `REBUILD_A5_STREAMS` | `wma/tests.rs:196` | streams compared (default 64) |
| `REBUILD_LAUNCHER_HOME` | `launcher/src/home.rs:25` | launcher app folder |
| `REBUILD_LAUNCHER_STAY_OPEN` | `launcher/src/main.rs:482` | do not minimise on launch |
| `REBUILD_MEASURE_LOCK` | `launcher/src/main.rs:168` | while this file exists, the system check makes no D3D12 device |
| `REBUILD_TITLE_SETTINGS` | `launcher/src/main.rs:741`, `title.rs:187` | the title's `settings.xml` |
| `REBUILD_EXE` | `launcher/src/play.rs:14` | the layer exe (default `rebuild.exe` beside the launcher) |
| `REBUILD_MATRIX_ID` | set at `launcher/src/main.rs:794` | **never read** |

Not `REBUILD_`: `REMUX_VRAM` (`gputest.rs:42`) is a legacy name for `--vram`.

### 7.6 Scratch tools outside the repo

The handoff mentions `xref/xrange.py` (RIP-relative references) and `sym.py` (symbolise `runtitle.exe+off`).
They live in the lead's scratchpad, not in git (`claude-max-handoff.md:202, 213`).

---

## 8. Safety rules for runs on the owner's machine

**The history.**

- `runtitle --run` took the PC down three times (reboots). Two root causes were fixed in commits `df6d3a9`
  and `2a2af01` (`claude-max-handoff.md:7-12`):
  - uncapped physical-page commit, charged to the system where a job object cannot cap it (now `budget.rs`);
  - `VirtualAlloc2` address requirements with write-watch on a full 4 GiB window, which spun in the kernel and
    made the process unkillable (now a `VirtualQuery` gap search, `xmem.rs:147-150`).
- GPU resets: three `nvlddmkm` event 153s at 21:18-21:20 happened during title runs. They did not reproduce on
  a monitored run. The handoff says runs now count `nvlddmkm` events before and after; that is a manual step,
  not code in this repo.
- An intermittent hang inside `D3D12XboxCreateDevice` was seen twice, perhaps under helper GPU load.
  `runtitle` now dumps all threads at `TIMEOUT`.

**The rules.**

| rule | how it is enforced |
|:--|:--|
| a title run needs the owner's OK, one at a time, always under the `runtitle` guards | process: the handoff and the project memory; the guards are job object, budget, watchdog and crash reporter (1.3, step 4) |
| test memory patterns without the title | `exitprobe`: capped children, no title code |
| `agents/MEASURING` | the lead creates it before any timed or GPU run and deletes it after; helpers run no builds or tests while it exists. Tests that touch the card skip when it exists (`device.rs:578`, `present_card.rs:26`) |
| `agents/CARD` | `pipelines::d3d::CardLock` (`d3d.rs:119-150`): created exclusively and removed on drop, taken only while `MEASURING` is absent. Lanes use WARP for correctness and take the card only with this lock |
| helpers stay light | `cargo build --release -j 4`; at most 4 DXC workers per lane |
| the game folder is read-only | `attrib +R /S` on all 110 files since 2026-11-27; one explicit path at a time, never a shell wildcard, never a shell whose current folder is inside it (`secondary-agent.md:66-77`) |
| no game bytes in git | no shader binaries, no title files, no paths to them in committed code; data lives in `D:\victory-data` and `%LOCALAPPDATA%\triumph-rebuild` |
| never blame the machine first | `gputest --bare` is the control after any device removal (`tasks.md:201`) |

The read-only rule exists because `xxd -l 512 ui/*.ttpf` matched two files and overwrote the second with a hex
dump. `data\ttdb\ui\ttdb_ui.ttpf` had to be re-dumped by the owner (845,115,648 bytes, SHA-256 in
`claude-max-handoff.md:59-61`).

Both lock paths are hard-coded to `D:\Developer\triumph-rebuild\agents` (`pipelines/src/d3d.rs:124`), so
worktrees elsewhere still see the lead's locks.

---

## 9. Progress and what is left

### 9.1 By build (`progress.md:29-41`)

The percentage is work done, weighted by build; it is not fps. Overall **59 %** (59.55 of 100 points).

| # | build | weight | done | state | closes when |
|--:|:--|--:|--:|:--|:--|
| 0 | three answers before more code | 10 | 70 % | (a) upload ✅ (b) translation ✅ (c) the title's GPU time needs a frame | (c) a measured replacement for the fork's 43-63 ms |
| 1 | tiled VRAM at console addresses | 10 | 100 % | closed | — |
| 2 | CPU-written ring; GPU→CPU labels | 8 | 100 % | closed | — |
| 3 | PE loader + console runtime | 15 | 100 % | closed: the title reaches `D3D12XboxCreateDevice` in 1.4 s | — |
| 4 | shaders RDNA2 → DXIL | 15 | 65 % | front end closed; v1 7,402 / 9,128; execution check and wave64 open | 25,694 translated, validated, built, matching the oracle, 0 refused |
| 5 | device, lists, descriptors, present | 12 | 50 % | frame 1 presented (black); frame 2 blocked | a non-black frame captured |
| 6 | list translation on the title's threads | 8 | 10 % | core measured | ≤ 1 µs a draw on the real stream |
| 7 | upscaler | 10 | 30 % | DLSS and FSR standalone | DLSS and FSR correct on the title, internal resolution below 2560×1440 |
| 8 | frame pacing | 6 | 0 % | — | frame time ≈ the card's own work: 30 fps on NVIDIA |
| 9 | AMD validation by testers | 6 | 0 % | deferred by the owner | 30 fps on AMD |

The launcher track has its own bar (5 / 7; C2 and L5 left) and is not part of the 59 %.

### 9.2 Build 5 piece by piece (`progress.md:43-55`)

| piece | state |
|:--|:--|
| device, queues, fences, allocators, lists, resources, DXGI | done |
| root signatures, query heaps, command signatures | done |
| compute pipelines (library + pool + on-demand) | done |
| graphics and stream pipelines | done (14 created, 0 refused) |
| title start-up to its first submission | done (1.44 s) |
| ExecuteCommandLists + PresentX | done (PR #32) |
| a non-black frame, captured | frame 1 black; frame 2 crashes |
| frame line | written each second; upload, xlate and upscale fields not measured yet (`framelog.rs:21-22`) |
| Build 0 (c), the title's GPU time | needs a frame |

### 9.3 The current blocker: frame 2

From `claude-max-handoff.md:211-224`:

1. Several threads fault reading `[0+0x68]` at `exe+0xb5a5f8` (function `0x140B5A5D0`). Its `rcx` struct's
   first word is null. The caller is `exe+0x32764`: a lookup in a game system returned null.
2. The null comes from the engine's set-program at `0x140122BD6`, which stores `[ctx+0x12328] = null`: **the
   title draws with no program bound.**
3. A watchpoint (`REBUILD_WATCH`) on `ctx+0x12328` shows the title's own state reset (`0x14012494E`, through
   `0x140B39840` with id 0xF and value 0) zeroing it just before an immediate draw (`0x1402AACF0`).
   `ctx+0x12330` (a pipeline cache) is never set.
4. Ruled out:
   - making Execute and Present no-op stubs;
   - answering the registry device type as Series X (0x64).
5. Next lead:
   - find what our run reports differently from a console, so that the title skips choosing a program
     (console type at `0x140075140`, capabilities);
   - follow `0x144FD1Dxx`, which calls the setter (thunk `0x14011F830`), then `0x1401248xx`, which resets
     slot 15 through `0x140B39840` (the state stack at `[tls0+0x12A8]`) to null and draws.

### 9.4 What is left, by area

- **Build 5:**
  - the frame-2 blocker;
  - the tiles and the ring wired into resources (today a 1 GiB host stand-in);
  - the shadow table and library root layout bound at draw time (4.4);
  - descriptor sync from write-watch;
  - placed-texture contents (detiling);
  - `CreateReservedResource` (0xf0), `CreateCommittedResourceX` (0x260), page pools, `UpdateTileMappings`;
  - the VRAM limit;
  - Build 0 (c).
- **Build 4:**
  - the execution check against the oracle (lane `shaderexec`);
  - the last 1,726 agnostic programs (`shadercov`);
  - the ~16,500 wave-wide programs through lane doubling (`wave64`);
  - mesh, ES and GS; irreducible control flow;
  - DXIL validated by the platform validator.
- **Build 6:** measure translation on the real stream.
- **Build 7:**
  - find the title's temporal upscale pass by its bindings;
  - lower the internal resolution (the fork's resize asserted at load);
  - wire DLSS and FSR;
  - lane `ffx`.
- **Build 3 leftovers:**
  - `XMemGetWorkingSetStatistics`;
  - `DLL_THREAD_ATTACH` for title DLLs on title-created threads (their entries are cleared in the prepared
    copies);
  - audio output (DSP is silent, WASAPI unused);
  - GDK areas not served.
- **Launcher track:**
  - C2: the layer reads `rebuild.toml`;
  - L5: the in-game menu on the present path, using `crates/input`;
  - a real `rebuild.exe` behind Launch.
- **D5b:** capture the title's BCPack requests, then compare against the fork's decoder.
- **Hardware limits:** VRAM, worker pools sized to the core limit, a capped proof run.

---

## 10. The documents and the helper lanes

### 10.1 Top-level documents

| file | holds | how current |
|:--|:--|:--|
| `README.md` | what it is, status table, plan table, recent findings, how to run | header bars current (59 %); body tables partly stale (11) |
| `plan.md` | the decisions (9 of them now), the target, the minimum spec (the dev machine, 3840×2160, DLSS Quality), build order, what we refuse to build | current |
| `tasks.md` | every task and the number that closes it; traps from the fork | Build 5 row behind the code |
| `progress.md` | bars by build, Build 5 pieces, one entry per upload | header current; the last two uploads (58 %, 59 %) have no "Updates" entry |
| `thoughts.md` | the first reasoning (list caching, later dropped) | historical |
| `HANDOFF.md`, `REVIEW-PROMPT.md` | a brief for an independent reviewer | early state |
| `claude-max-handoff.md` | the lead's in-flight state, next actions, open threads | the most current document |
| `secondary-agent.md` | helper lanes, their rules, current state (§8), taking over as lead (§9), answers (§10) | §8 current |
| `CREDITS.md` | vendored code and fonts | current |

### 10.2 `docs/`

| file | what |
|:--|:--|
| `docs/design/shader-ir.md` | the shader core design: classify, uniformity, lane doubling, window addressing, HLSL → DXC, translation decisions |
| `docs/design/d3d12x-resources.md` | records, host mirrors, the shadow table, alternates |
| `docs/design/launcher.md` | settings registry, launcher, mods, in-game menu, the L6 session contract |
| `docs/design/test-profiles.md` | the 9 built-in test profiles |
| `docs/research/d3d12x-device.md` | the device, its object layouts and slots, the title's start-up call sequence (from the fork) |

### 10.3 `runs/`

Each measurement is a write-up (`.md`) with its raw output (`.txt`); the index is `runs/README.md`.

| build | runs |
|:--|:--|
| 0 | `build0a-upload`, `build0b-lists` |
| 1 | `build1-tiles` (with `build1-scale10.txt`, `build1-upload-tiled.txt`) |
| 2 | `build2-ring`, `build2-labels` |
| 3 | `build3-runtime-census`, `build3-maptitle`, `build3-startup`, `build3-reached` |
| 4 | `build4-sirtest` (+ branches, memory), `build4-coverage`, `build4-lift`, `build4-ps`, `build4-vs`, `build4-cs`, `build4-v1`, `build4-oraclediff` |

### 10.4 `agents/` and the lanes

The lead owns `crates/{uma,xlist,sir,config}`, the measurements and the shared docs. Helper agents work one
lane each, in their own worktree and branch, and hand back by pull request (`secondary-agent.md:11-15`).
`agents/<lane>.md` is each lane's hand-back note; `agents/secondary-agents-updates.md` is the coordinator's
status page.

| round | lanes (PR) |
|:--|:--|
| 1 | loader #1, L5 #7 · input #2 · vfs #3 · upscale DLSS #4, FSR #8 · shader S1-S5 #5, #10 · dstorage #6 · xg #9 · audio #11 · files #12 · launcher #13 |
| 2 | launcher-idle #14 · shader S6 #15 · files F4-F5 #16 · audio A5 #17 · gdk #18 |
| 3 | device-research #19 · gdk G5 #20 · shader S7 #21 · pipelines #22 · profiles #23 · dstorage BCPack #24 · cmdlist #25 · shader-cf #26 |
| 4 | pipelines-2 #27 · launcher L6 #28 · resources #29 |
| 5 | streampso #30 · ondemand #31 · present #32 · running: `shaderexec`, `shadercov`, `wave64`, `ffx` |

Data files in `agents/`:

| file | rows | what |
|:--|--:|:--|
| `loader-imports.csv` | 962 | `dll, function, kind, importers, resolved_by`: 697 missing, 265 supplied by the title's own DLLs |
| `loader-getproc.csv` | 76 | names the title may look up at run time (42 high, 3 medium, 31 low confidence) |
| `shader-census.csv` | 26,084 | one row per native program (5.4) |
| `shader-oracle.csv` | 25,694 | the oracle index |
| `gdk-slots.csv` | 130 | GDK interface slots |
| `d3d12x-device-slots.csv` | 73 | device slots |
| `d3d12x-child-slots.csv` | 74 | child-object slots |
| `d3d12x-object-layouts.csv` | 26 | inline fields the title reads |
| `*-bench.txt`, `pipelines-run.txt` | — | raw lane measurements |

`agents/MEASURING` and `agents/CARD` are git-ignored lock files.

---

## 11. Inconsistencies: docs vs code

Found while writing this document. None was fixed when it was written.

**Status (2026-11-28, lane `tooling`, branch `agent/tooling`).** Each item below is now marked **fixed** (with
the commit), **doc-corrected** (the code was right, or the comment was the wrong part; the docs now say what the
code does) or **deferred** (with the reason). Items in `crates/sir`, `crates/d3d12x`, `crates/xlist`,
`crates/runtime/src/present*.rs` and `crates/pipelines` are deferred: other lanes are working there.

| # | status | where / why |
|--:|:--|:--|
| 1 | deferred | `crates/xlist`, `crates/pipelines`, `crates/sir` (lane `rootseam`, PR #35, works on this seam) |
| 2 | deferred | `crates/sir` / `crates/pipelines` |
| 3 | deferred | `crates/sir` (`lift.rs`) |
| 4 | fixed before this pass | the resolver binds the host's function when an override is listed but none is built (`runtime/src/lib.rs`, `HostResolver::resolve`) |
| 5 | **fixed** `dfb5943` | `vfs::build()` checks every drive first (the overlay's would-be path, letter case ignored on Windows) and creates folders only after; test `v1_a_refused_overlay_creates_nothing` |
| 6 | **fixed** `aaa2e14` (lane `deferred`) | hashes are pending until the scatter is queued ahead of the Signal, then committed (no CPU wait); a flush larger than a slot sends a slot's worth and carries the rest; a failed flush commits nothing and carries its pages; WARP test `a_flush_larger_than_a_slot_carries_the_rest_and_loses_nothing` |
| 7 | deferred | `apps/rebuild/src/sampler.rs` and `crash.rs`: a change to what runs while a thread is suspended needs a title run to verify, and this lane runs no title |
| 8 | deferred | `crates/d3d12x` |
| 9 | **fixed** `aaa2e14` (lane `deferred`) | fence objects live in one reserved arena; `by_cell` is arithmetic plus one acquire load (no lock, no scan); every fence served is found (past 1M `CreateFence` fails); tests in `fence.rs` |
| 10 | **fixed** `dfb5943` (in part) | `rebuild.exe` reads and checks `--config`, reports the settings and exits 3 ("does not run the title yet") instead of passing silently; running the title from it is C2 |
| 11 | **fixed** `dfb5943` (in part) | the launcher's `title_source` uses `game.title_settings` before `REBUILD_TITLE_SETTINGS` (test `the_title_settings_setting_comes_before_the_variable`); the other keys wait for C2 (the layer reading `rebuild.toml`) and `runtime/src/drives.rs` |
| 12 | **fixed** `dfb5943` | `Config::upscaler_mode()` / `upscaler_mode_name` give the upscaler's spelling (`dlaa` → `native-aa`); test checks every choice against `rebuild_upscale::Mode::parse` |
| 13 | doc-corrected `dfb5943` | a comment at the push says nothing reads it until C2; the session records the id |
| 14 | **fixed** `aaa2e14` (lane `deferred`, in part) | `Input::with_menu_key` and `menu_key_named` take every `menu.key` choice (config test checks each); handing the setting to the layer is C2 |
| 15 | doc-corrected `dfb5943` | a comment above the three entries says the fallback is the layer's once C2 lands; only `upscaletest` reads the variables |
| 16 | doc-corrected `dfb5943` | README table, "Running it" and badge, `secondary-agent.md` §8: 515 green, 12 ignored |
| 17 | doc-corrected `dfb5943` | README: 30 options; `tasks.md` and `agents/launcher.md` say 18 was the count then |
| 18 | doc-corrected `dfb5943` | entries for 58, 59 and 61 % added from the commits that moved the header |
| 19 | doc-corrected `dfb5943` | the README tree lists every crate and both apps |
| 20 | doc-corrected `dfb5943` | the write-ups note the raw outputs' 1,791 and 926; runs index: oraclediff row 0 / 0 / 0, rows for v1, exec, wave64 |
| 21 | doc-corrected `dfb5943` | the paths are restored (also tab-mangled paths in `agents/secondary-agents-updates.md` and the handoff) |
| 22 | doc-corrected `dfb5943` | `HANDOFF.md` 24 uma tests; "nine decisions" in `plan.md`, README, `HANDOFF.md`, `secondary-agent.md`; `runs/build1-tiles.md` is a dated record (21 then) |
| 23 | doc-corrected `dfb5943` | 27 slots in `docs/design/d3d12x-resources.md` and `agents/resources.md`; 0x258 served in the CSV |
| 24 | doc-corrected `dfb5943` (in part) | the four rows' sizes swapped in `agents/shader-oracle.csv`; `oracle.rs` settling same-tick ties is deferred (the shader lane's tool; verifying it needs the oracle data) |
| 25 | doc-corrected `dfb5943` | `agents/audio.md`, `agents/dstorage.md` (22 tests; the decoder is installed when tables exist), `agents/launcher.md` |
| 26 | doc-corrected `dfb5943` | `runtitle.rs` header: the run goes on to the deadline |
| 27 | doc-corrected `dfb5943` | `crash.rs` header: real unwind, DbgHelp at the first crash, skipped inside the heap |
| 28 | doc-corrected `dfb5943` | `watch.rs` header: the chain from the post-write RIP; the 120 s arming window |
| 29 | doc-corrected `dfb5943` (in part) | `lib.rs`, `exports.rs`, `modules.rs`, `frame.rs`, `xmem.rs` comments; `present.rs:845` deferred (`present*.rs`) |
| 30 | doc-corrected `dfb5943` | `docs/design/shader-ir.md`: no SSA of ours, irreducible refused, status and order of work after v1 |
| 31 | deferred | `crates/sir` (`lift.rs`, `gfx10/mod.rs`) |
| 32 | doc-corrected `dfb5943` | `agents/shader.md`: both bugs marked fixed in PR #26 |
| 33 | doc-corrected `dfb5943` | `agents/vfs.md` (`T:` emptied by the runtime), `agents/loader.md` (`prepare.rs`), `agents/files.md` |
| 34 | doc-corrected `dfb5943` | round 5 and `resources` (#29) in `agents/secondary-agents-updates.md`; `agents/shaderexec.md` is on `master` now |
| 35 | doc-corrected `dfb5943` (in part) | `agents/pipelines.md` names `ondemand`; `pool.rs`, `translate.rs` (`crates/pipelines`) deferred; the `xlist` test comment says 0x4f8 (`aaa2e14`, lane `deferred`) |
| 36 | **fixed** `f3108d3` (in part) | the card lock folder follows `REBUILD_LEAD_DIR` (`rebuild_cardlock::agents_dir`); the `D:\victory-data` defaults are in `crates/pipelines` (deferred) |

Also fixed in `f3108d3`: a stale `agents\CARD` no longer blocks card jobs. The lock (`crates/cardlock`, re-exported as
`rebuild_pipelines::d3d::CardLock`) names its owner's pid and start time; a lock whose owner has ended is taken
over; `rebuild_cardlock::exit` and, under `panic = "abort"`, a panic hook release it before the process ends.

### 11.1 Behaviour worth the lead's attention

1. **The library root signature vs the title's root arguments** (4.4).
   - Translated PSOs are built against the library's fixed root layout (user data at `b0`, the shadow table
     at `t0 space101`).
   - xlist binds the title's own root signature and forwards the title's parameter indices.
   - Nothing calls `shadow_va` / `own_kind_gpu` / `sync`. Draws with translated programs cannot bind
     correctly until this seam is built.
2. **Lifted programs carry no `[WaveSize(32)]`** (5.5), although `docs/design/shader-ir.md:22` and plan
   decision 4 say every entry is wave32. On NVIDIA the wave is 32 anyway; on AMD the driver may choose 64.
3. **`v_readfirstlane_b32` is translated on the assumption that "the classifier checked it"**
   (`lift.rs:1231-1235`). The `translate` path filters by the census's wave class, not `sir::classify`. Today
   it is safe only because readfirstlane programs are excluded.
4. **Nine memory overrides have no implementation** (`runtime/src/lib.rs:67-77` vs `exports.rs:343-354`).
   Classified `Override`, they would bind to trap thunks, never to the host. Latent: the title imports none
   of them.
5. **`vfs::build()` creates the overlay folder before the safety check** (`vfs/src/lib.rs:230` vs `:240`).
   A misconfigured overlay inside the game folder would create a directory there. The game folder's read-only
   attribute does not stop directory creation.
6. **The upload ring marks hashes as sent before the send succeeds** (`uma/src/ring.rs:349`, `:365-367`). A
   flush that overflows a slot loses those chunks. Not in use by the title run yet.
7. **`sampler.rs` reads the environment while the target thread is suspended** (`beat()`, `:27-38`, called at
   `:68` and `:96`). If the title thread holds the heap or env lock, this can deadlock. Its header promises no
   allocation while suspended. Likewise `crash.rs:278` reads `REBUILD_THREADS` inside the handler.
8. **`create_descriptor_heap` always reads 16 bytes past the description** as a placement hint
   (`d3d12x/src/heap.rs:677`). A description at the end of a page could fault.
9. **`runtime/src/fence.rs`** finds fences by value cell in a fixed table of 4,096 (`:67-69`). Later fences
   are served but cannot be found by their cell.

### 11.2 The launcher and the layer

10. Launch runs `rebuild.exe --config <file>`, but `apps/rebuild/src/main.rs` is a placeholder that ignores
    arguments. `runtitle` is the real runner.
11. The layer never reads `rebuild.toml` (C2). Registry keys that nothing reads:
    - `memory.*`, `shaders.wave64`, `display.*`, `diagnostics.d3d12_debug`;
    - `dstorage.bcpack_tables` (`drives.rs:61` passes `None`);
    - `game.title_settings` (the launcher's `title_source`, `main.rs:740`, uses only the env var or a search).
12. `upscaler.mode` choices (`dlaa`, `ultra_performance`) do not parse in `rebuild_upscale::Mode::parse`
    (`native-aa`, `ultra-performance`).
13. `REBUILD_MATRIX_ID` is set by the launcher (`main.rs:794`) and read nowhere.
14. `menu.key` (F10 / F11 / F12 / Insert / Home) cannot reach `crates/input`, whose `MENU_KEY` is hard-coded
    to F10. Nothing depends on `crates/input` yet.
15. `crates/config/src/lib.rs:157-159` says empty upscaler paths fall back to `REBUILD_DLSS_DLL` etc. Only
    `upscaletest` reads those variables.

### 11.3 Numbers that disagree

16. **Test counts.**

    | source | count |
    |:--|:--|
    | README badge | 473 green |
    | README "Where it stands" table | 97 green |
    | README "Running it" | 121 tests |
    | `secondary-agent.md` §8 | 473 |
    | `#[test]` attributes in the source | 486 (about 12 ignored) |

17. **Registry size.** `README.md:59` and `tasks.md:177` say 18 options in 8 groups; `agents/launcher.md` says
    18; the registry has 30.
18. **`progress.md` "Updates"** stops at 57 % ("night, later"). The 58 % (PR #30) and 59 % (PR #32) uploads
    moved the header but have no entry, against the one-entry-per-upload rule.
19. **The `README.md` tree** lists 8 crates and says `apps/rebuild` is "the launcher, gputest, uploadbench,
    listbench". The repo has 17 crates, the launcher is `apps/launcher`, and `apps/rebuild` has 14 bins.
20. **`runs/build4-ps.md` and `runs/build4-vs.md`** say 1,790 and 923; their raw `.txt` say 1,791 and 926.
    `runs/README.md` has no `build4-v1` row, and its oraclediff row still reads 35 / 3 / 4 (now 0).
21. **`runs/build4-oraclediff.md` and `claude-max-handoff.md:155-157`** show a mangled path `D:ictory-data…`:
    the backslash sequences were eaten as escapes.
22. **`uma` tests:** `HANDOFF.md` says 15, `runs/build1-tiles.md` says 21; the source has 24. `HANDOFF.md`
    describes "the six decisions"; `plan.md` now has nine.
23. **Device slots served by d3d12x:** `docs/design/d3d12x-resources.md:10,17` and `agents/resources.md:14`
    say 24; `thunks::device_slots()` serves 27 (0x80, 0x138, 0x148 added). `agents/d3d12x-device-slots.csv`
    marks 0x258 "later"; it is served.
24. **`agents/shader-oracle.csv`** still holds the pre-fix sizes for the 4 swapped oracle files that
    `agents/shader.md:302-304` asks to swap. `oracle.rs` does not settle same-tick ties itself.
25. **`agents/audio.md`** says "14 green" in its Measured block and 18 in its header.
    **`agents/dstorage.md`** says 21 tests (22 now) and that BCPack requests fail with `E_NOTIMPL` (a decoder
    is installed when tables exist). **`agents/launcher.md`** says 34 tests (58 now).

### 11.4 Stale comments and docs

26. `runtitle.rs:1-14` says the run stops at `D3D12XboxCreateDevice`; it runs on to the deadline.
27. The `crash.rs` header describes a heuristic stack scan and "without touching the heap". The code does a
    real unwind and may load DbgHelp.
28. The `watch.rs` header promises the writing instruction; it prints the chain from the post-write RIP. Its
    120 s arming limit is undocumented.
29. Several `crates/runtime` comments are stale:
    - `lib.rs:210`: "overrides not built yet";
    - `exports.rs:358-367`: the device call "fails";
    - `modules.rs:1`: 16 module overrides (12 in the code);
    - `frame.rs:5`: `ScheduleFrameEventX` is "recorded" (only counted);
    - `xmem.rs:235`: `written_pages` returns offsets (it returns a count);
    - `present.rs:845`: `counts()` returns three values (two).
30. `docs/design/shader-ir.md`:
    - says "CFG + SSA" (`:12`); the lifter builds no SSA, DXC does;
    - says irreducible graphs are node-split (`:14`); they are refused;
    - its status line (`:3`) and order of work (`:167`) predate v1.
31. The `lift.rs:6-7` header says memory, images, interpolation and exports are unsupported; they are built.
    The `gfx10/mod.rs:9-10` docs name methods that were not vendored.
32. `agents/shader.md` still lists the two translator bugs fixed in PR #26 as open.
33. `agents/vfs.md` says `T:` persists; `drives.rs` empties it every run. `agents/loader.md` does not mention
    `prepare.rs`. `agents/files.md` describes a runtime dev-dependency and a list comparison that the test does
    not make.
34. `agents/secondary-agents-updates.md` stops at round 4 and shows `resources` as running (merged as #29).
    Round 5 is recorded only in `claude-max-handoff.md` and `progress.md`. `agents/shaderexec.md`, referenced
    by the handoff, does not exist on `master`.
35. Smaller code notes:
    - `pipelines/src/pool.rs:14` names `d3d::PsoCreator` (it is `pso::PsoCreator`);
    - `pipelines/src/translate.rs:91` links a `translate_native` that does not exist;
    - `agents/pipelines.md` omits the `ondemand` subcommand;
    - `xlist/src/console/tests.rs:196` says 0x4a8 where the call is 0x4f8.
36. Hard-coded paths: `D:\Developer\triumph-rebuild\agents` (lock files), `D:\victory-data\pipelines` and
    `D:\victory-data\translate` (defaults). They are not game paths, but they tie the code to this machine.

---

## 12. Glossary

| term | meaning |
|:--|:--|
| **title** | the game being run; never named in code, never committed |
| **the fork** | `jleybourne/Triumph`, the owner's fork of the old layer: design reference and the oracle |
| **victory**, `fxceless-victory` | the layer being replaced (7.6 fps) |
| **xcl** | a sibling layer, GPL-2.0, read for design only |
| **lead**, **lane**, **coordinator** | the main session; a helper agent's scoped task and branch; the session that runs helpers |
| **Build 0-9**, **L**, **C1/C2**, **D5b**… | the plan's steps and task ids (`plan.md`, `tasks.md`) |
| **frame line** | `fps · ms · draws · dropped · …`, the one line every result is reported in |
| **GDK** | Microsoft's Game Development Kit runtime (`xgameruntime.dll`); reached through `QueryApiImpl` objects |
| **D3D12.X** | the console's Direct3D 12 variant (`d3d12_xs.dll`): extra `…X` methods, inline packets, objects the title reads directly |
| **slot** | a method-table entry, numbered by byte offset (0x2b0 = entry 86) |
| **trap** | a stand-in for an unwritten slot or import: counts, reports once, returns `E_NOTIMPL` or 0 |
| **discovery** | the logged list of everything the title asked for that we do not serve |
| **tier** | how an import binds: host, override, console function, console DLL |
| **prepared image** | a patched copy of a title image that the Windows loader maps but does not run or bind |
| **XMem** | the console's memory API (`xmem.dll`): attributes, allocator ids, physical pages, hooks |
| **allocator 0x80** | the XMem allocator id D3D uses; the title asserts its arena ends up full |
| **physical page / frame** | a 64 KiB console page; here a view of a pagefile section |
| **placeholder** | a Windows reserved range that views can be mapped into (`MEM_RESERVE_PLACEHOLDER`) |
| **window** | a 4 GiB slice of the address space; a console address is window + 32-bit offset |
| **tile** | a 64 KiB page of a reserved D3D12 resource, mapped onto heap memory |
| **arena** | committed resources for what cannot be tiled (stencil formats) |
| **write-watch** | the OS's record of pages written since the last reset (`MEM_WRITE_WATCH`) |
| **chunk** | 64 bytes, the unit of change detection and upload |
| **ring** | the async upload: diff, gather, one scatter dispatch per window, behind a fence |
| **label** | a value the GPU writes into title memory for the CPU to poll |
| **memory wait** | a console list packet: wait until a memory location holds a value |
| **segment** | the part of a translated list between two memory waits; becomes its own host list |
| **bridge** | a host fence signalled by our thread once a waited memory value holds |
| **`pso_missing`** | xlist state: the bound console pipeline has no host PSO, so draws are skipped |
| **record** | the 32- or 16-byte self-describing descriptor we write into title memory ('XD' tag) |
| **mirror**, **alternate**, **P** | host views rebuilt from records; the descriptor's own-kind view; the null index (750,000) |
| **shadow table** | 16 bytes per heap index (stride, records, alternate, lead) at `t0, space101` |
| **program library** | `library.bin` (`RBPIPLIB`): translated programs and root signatures, loaded at start |
| **on-demand** | translating a program missing from the library when the title creates it (compute only) |
| **stream / stream subobject** | a pipeline described as a list of typed subobjects (device 0x2b0) |
| **BVH builder** | `xboxbvhbuilder.dll`, the title's ray-tracing structure builder; its 80 programs are not in the library |
| **SGD5**, **`shaders.rpf`** | the shader archive format and file |
| **DXBC / DXIL / XDAT / XBCH / FSSM** | the container, its DXIL part, the console's index, its zlib blob, and the native payload with GFX10 code |
| **GFX10 / RDNA2** | the console GPU's instruction set |
| **SGPR / VGPR** | scalar (per-wave) and vector (per-lane) registers |
| **EXEC**, **VCC**, **SCC** | the active-lane mask, the vector condition mask, the scalar condition bit |
| **wave32 / wave64** | 32- or 64-lane waves; the console mostly runs wave64, the host path is wave32 |
| **agnostic**, **wave-wide** | a program whose lanes never look past their quad, or one that does |
| **lane doubling** | one host lane carrying two console lanes, EXEC as a bool pair |
| **predication** | running both sides of a divergent branch with EXEC as data |
| **v0 / v1** | translation sets: straight-line agnostic programs; agnostic programs with control flow |
| **oracle** | the fork's translation of each program, the reference ours is compared with |
| **sidecar** | `<hash>.<stage>.samplers`, the immediate samplers of a translated program |
| **BCPack** | the console's lossless repacking of BCn texture blocks, decoded by our own rANS decoder |
| **XMA2** | the console's audio codec (a WMA Pro variant), decoded by our own decoder |
| **ConsoleMode** | the registry value that tells the title which console it is on (0x64 Series X, 0x60 Series S) |
| **`clwb` / `clzero`** | cache-line write-back / zero instructions the console CPU has and the host lacks |
| **`MEASURING` / `CARD`** | the measurement lock and the GPU lock in `agents/` |
| **WARP** | Windows' software rasterizer, used for correctness checks without the card |
