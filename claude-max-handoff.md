# claude-max-handoff.md — the lead's working state, for the session that takes over

Updated by the lead **every step**. Read this after `secondary-agent.md` sections 8-10, which cover the
project state, the owner's rules and the step order. This file holds what is **in flight**: the step
underway, the decisions made but not yet written anywhere else, and the exact next actions.

**Last updated:** 2026-11-27, during Build 3's "run the title's startup" step (see "In flight").

## Who you are and how the owner works (short)

You are the **lead** of `D:\Developer\triumph-rebuild` (GitHub `jleybourne/triumph-rebuild`, branch `master`).
The owner's rules, in their own words where it matters:

- "keep building until we have no tasks to do"
- "you shouldn't stop ever"
- "just do what is best for us I want it to perform the best": decide technical questions yourself; ask
  only owner-only items (`secondary-agent.md` §9).
- Every push gets a new dated entry at the top of `progress.md` "Updates", with its own bar and notes.
  Also update the README bar and badges, `tasks.md`, `runs/README.md`, `secondary-agent.md` §8, and **this
  file**.
- Commit to `master` and push. Helper agents work in lanes and send pull requests. Before every step, read
  `agents/secondary-agents-updates.md`, merge open PRs first, and answer their "For the lead" items in
  `secondary-agent.md` §10.

## How to work in this repo (practical)

- **Shell:** Git Bash. Run multi-file doc edits as a Python script in the scratchpad, and
  `export PYTHONIOENCODING=utf-8` first: printing a `█` under cp1252 crashed a script mid-edit once.
- **Rust string literals with Windows paths:** use raw strings (`r"G:\title.exe"`). `"G:\t…"` silently
  becomes a tab. Edit such lines with the Edit tool, not `sed` or Python, because shell escaping halves backslashes.
- **Measurement lock:** `touch agents/MEASURING` before any GPU or timed run, and `rm -f agents/MEASURING` after.
- **The title's folder** is read-only, through `REBUILD_GAME_DIR` (its path is `game_dir` in
  `D:\Developer\fxceless-victory\victory.toml`). Never commit game bytes or that path.
- **The fork** (design and data only): `git -C D:\Developer\fxceless-victory show triumph/main:<path>`. Its
  GPU and loader design: `crates/victory-gpu/src/*.rs`, `crates/victory-loader/src/*.rs`,
  `crates/victory-hle/src/modules/*.rs`.
- **Tests:** `cargo test --release`, 121+ green and 0 warnings; keep both true.
- **DXC:** the Windows SDK `dxc.exe` (found automatically by `sirtest`).

## In flight: Build 3, "run the title's startup to its first render call"

**Done in this step so far:**

- `crates/loader/src/prepare.rs`: patch a copy of an image so the **Windows loader** maps it, relocates it,
  allocates its static TLS and registers its unwind tables, but runs nothing (DLL flag, no entry point,
  import directory pointed at its terminator, TLS callbacks cleared, their relocation neutralised).
  - Test: the test binary itself is prepared and loaded; Windows assigns a static TLS index and this thread
    has a block. ✅
  - Why: 4 title images use static TLS (`game_scarlett_master.exe` with 2 callbacks, plus
    `xboxbvhbuilder.dll`, `amd_fidelityfx_upscaler_xbox.dll` and `libhttpclient.gdk.dll`). Manual mapping
    cannot give them static TLS on every thread; the Windows loader can. (The idea is the fork's; the code
    is ours.)

**Next actions, in order:**

1. **`crates/runtime/src/title.rs`** (new): `load_title(dir) -> LoadedTitle`.
   - For each image (the same walk as `maptitle`): `prepare`, write to
     `%LOCALAPPDATA%\triumph-rebuild\images\<name>-<first 16 hex of a hash>.dll`, `LoadLibraryExW` it, and
     `modules::register_title(name, base, "G:\\name", exports…)`. Exe flag for the `.exe`.
   - Then bind imports into each loaded module's IAT. For each `img.imports` entry, slot = `base + iat_rva`:
     `VirtualProtect` it to RW, write the address from `maptitle`'s `Combined` resolver (title DLLs →
     `HostResolver`), then restore the protection. Move `Combined` from `maptitle.rs` into `title.rs` and
     make `maptitle` use it.
   - **Missing imports → trap thunks**: generated x64 code per import, `mov rcx, <index>` (48 B9 imm64),
     `mov rax, <trap>` (48 B8 imm64), `jmp rax` (FF E0), in one `PAGE_EXECUTE_READ` block. The trap is
     `extern "system" fn(u64) -> u64`: it records `(dll, name)` into `modules::misses()`, logs it once,
     returns 0. The first argument is overwritten, which is fine for a trap.
2. **`apps/rebuild/src/bin/runtitle.rs`**: `load_title`, then run.
   - DLL entry points: `DllMain(base, DLL_PROCESS_ATTACH, 0)` for each title DLL. The entry RVA comes from
     the **original** image (`Image::entry_rva`), since the prepared copy's is 0. Order: dependencies first
     (a DLL after the title DLLs it imports).
   - Then the exe's TLS callbacks (`img.tls.callbacks`, RVAs), each `(base, DLL_PROCESS_ATTACH, 0)`.
   - Then the exe entry on a new thread with the image's stack reserve. Run with a timeout, then print
     `modules::misses()` in first-call order and stop.
   - Known gap: threads the title creates later get no `DLL_THREAD_ATTACH` for title DLLs, because their
     entries are cleared. Handle it when a miss or crash shows it matters.
   - Set `exports::set_command_line(r"G:\game_scarlett_master.exe")` and install `__XMemAllocationHooks__` if
     the exe exports it (`exports::set_alloc_hooks`).
3. **The first render call** is `d3d12_xs.dll!D3D12XboxCreateDevice`. Stub it in `exports.rs`
   (`console_export` for `d3d12_xs.dll`): log "first render call reached" with its arguments, return
   `E_FAIL` (0x80004005). Also bind `D3D12SerializeVersionedRootSignature` and
   `D3D12CreateRootSignatureDeserializer` straight to the host's `d3d12.dll` (standard), and
   `D3D12ConfigureVirtualMemoryX` to a no-op returning `S_OK`. Closing number: **the startup path reaches
   `D3D12XboxCreateDevice`**, with the miss list from before it. Record it in
   `runs/build3-startup.md`.
4. **Then Build 5, the D3D12.X device.** Facts from the fork (design):
   - Console graphics objects begin with a vtable pointer, bookkeeping bits and a refcount. The title's
     **inlined code reads object fields directly**, so the layout matters, not only the vtable.
   - Device vtable: IUnknown, the object methods, the device methods in PC order, then the console's
     additions. Unwritten slots log their table offset and fail, so every run says what to write next.
   - **Command lists:** the title writes packets inline while a list's put pointer is below its limit, and
     calls the list method otherwise. Set both to 0 and every command arrives as a call, which is what
     `crates/xlist` translates (0.43 µs a draw measured). Parsing inline packets on workers is a later option,
     measured against the frame line.
   - Device creation wires: our tiles (Build 1) for GPU-owned memory, the ring (Build 2) for graphics
     regions (`exports::graphics_regions()`), labels into title memory, physical pages read in place.

## Open threads (not urgent, don't lose)

- `XMemGetWorkingSetStatistics`: refused until its layout is known (from a run).
- One of the 18 census images did not parse as PE32+ in `maptitle` (17 mapped); name it in the startup log.
- Helper lanes waiting to start (the coordinator starts them): `audio`, `dstorage`, `files`, `xg`, loader L5,
  upscale P3. Running: `shader`, `launcher`.
- Build 5: direct GPU read of title memory vs the ring, per resource, by the frame line.
- "Second title" check after the first title renders (the owner asked whether other games will work).

## Where everything is

| what | where |
|:--|:--|
| state, rules, step order, answers to helpers | `secondary-agent.md` (§8 state, §9 lead, §10 answers) |
| per-upload history with bars | `progress.md` |
| every task and its closing number | `tasks.md` |
| measurements | `runs/` (write-up `.md` + raw `.txt`; index `runs/README.md`) |
| designs | `docs/design/shader-ir.md`, `docs/design/launcher.md` |
| helper status (coordinator's) | `agents/secondary-agents-updates.md` |
| Claude memory for this project | `C:\Users\Jayde\.claude\projects\D--Developer-triumph-rebuild\memory\` |
