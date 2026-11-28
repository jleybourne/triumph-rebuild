# Design: the shader library, built on the user's machine

Status: **built 2026-11-28 (lane `shaderlib`, owner decision 2026-11-28).** Code: `crates/pipelines/src/shaderlib.rs`
(the build), `crates/pipelines/src/dxc.rs` (finding DXC), `apps/launcher/src/shaders.rs` and
`apps/launcher/src/screens/shaders.rs` ("Preparing shaders"), `crates/runtime/src/pipeline.rs` (which library the
layer loads).

The owner decided: *the launcher builds the shader library on first run, from the user's own game, into the
per-user cache. Translated shaders are never shipped. A bundled DXC does the compile, and translating on demand at
pipeline creation is the fallback for anything missing.* Translated shaders are derived from the game's own
(copyrighted) shaders, so they can only be made from the user's copy, on the user's machine.

## The pipeline

```text
<game dir>\shaders.rpf (read only)
  └ census walk: every VS, PS and CS program, once per (hash, stage), in file order
      └ per program, on N workers:
          lift + HLSL (translate::lift_hlsl, the `translate v1/v2` path)          ~0.3 ms
          key = BLAKE3(format, stage, DXC args, HLSL, sampler words, top dword, DXC build)
          record on disk with the same key?  → kept, no DXC
          else DXC (-T <stage>_6_6 -E main -O3 -enable-16bit-types)               50 ms to seconds
          → programs\<hash>.<stage>.rec  (written beside, renamed)  → a line in journal.txt
  └ every Ok record → its layout (DXIL bindings + sampler words) → library.bin (crate::library)
  └ manifest.toml: state = "complete"
```

It lives in `<cache>\games\<id>\cache\<build>\shaders\` (`GameStore::shaders_dir()`, [storage.md](storage.md)):

| file | what |
|:--|:--|
| `manifest.toml` | format, translator version, game build, corpus size and time, state (`building` / `complete`), counts, library size and content hash, DXC used, build time |
| `journal.txt` | one line per program whose outcome is on disk: `<hash>.<stage> <key>` (valid only for the manifest's translator, build and corpus) |
| `programs\<hash>.<stage>.rec` | the outcome: Ok (DXIL, sampler words, comparison mask, top user dword), Refused (the translator's reason), Failed (DXC's first error line); BLAKE3-checked |
| `library.bin` | the program library the layer loads (`rebuild_pipelines::library`, unchanged format) |

The library holds exactly what `translate v2` followed by `pipelines build` makes from the same corpus: the same
lift, the same DXC arguments, the same layouts. The build adds no translation of its own, so a translator change
(lanes `shadercount`, `wave64`, …) reaches users' libraries by itself.

### Resumable

Every program's outcome is its own file, and the journal line is written only after the file is renamed into place.
A build stopped anywhere (Cancel, "Play now", the launcher closed, a crash, the PC off) starts again where it
stopped: a journaled program whose record still carries the journaled key is counted and skipped without lifting
or compiling. A torn last journal line or a half-written record (never renamed) is simply redone. DXC processes
still running when the launcher closes finish on their own; their files are in `work\`, which the next build
deletes first.

A DXC failure with no error line (DXC itself died, seen under heavy load) or a DXC that could not start is not
journaled, so the next build tries that program again.

### Versioned

`shaderlib::VERSION` is a 128-bit hash, made by `crates/pipelines/build.rs`, of every source file whose change
can change what the library holds: `crates/sir/src/**`, `crates/shader/src/**`, and `translate.rs`,
`bindings.rs`, `layout.rs`, `library.rs`, `sidecar.rs`, `d3d.rs`, `lib.rs` of `crates/pipelines` (line endings
ignored). The manifest names the version, the game build and the corpus (`shaders.rpf` size and modification
time). If any of them differs, the library is **stale**: the launcher rebuilds it, and the layer does not load it
(it translates on demand instead). The game build also picks the folder (`cache\<build>\`), so a game patch starts
in a new one.

### Incremental

A rebuild for a new translator version starts a new journal, so every program is lifted again (the census walk and
the lift of all ~25,000 programs take seconds), but only programs whose **key** changed go through DXC. A
translator change that alters ten programs' HLSL compiles ten; a change to comments compiles none. A new program
(a patched corpus) compiles alone. Records of programs no longer in the corpus are deleted when the library is
written. The key includes the DXC build (BLAKE3 of `dxc.exe`, `dxcompiler.dll`, `dxil.dll`), so a newer bundled
DXC recompiles everything at the next rebuild.

### Parallel

`workers` threads (default: logical processors less two, `REBUILD_SHADER_WORKERS`, the launcher's −/+ on the
Preparing screen, `pipelines library --workers N`), each lifting and then running one DXC process. The build raises
`translate::MAX_DXC`'s process-wide cap to its worker count (`translate::set_dxc_limit`) and starts DXC at
below-normal priority with no console window, so the desktop stays responsive. Developer machines running other
lanes use at most 4 (`--workers 4`).

## Which library the layer loads

`rebuild_pipelines::shaderlib::choose` (tested:
`the_layer_takes_a_dev_override_then_the_storage_library_then_the_dev_library_then_none`):

1. `REBUILD_PIPELINES_LIB` (a file) or `REBUILD_PIPELINES_OUT` (a folder holding `library.bin`): the dev
   override, always first.
2. The current game's library (`rebuild_storage::current()`, from `REBUILD_GAME_ID` / `REBUILD_GAME_BUILD` the
   launcher passes, else `REBUILD_GAME_DIR`), if its manifest says `complete` and its translator is this one.
3. The dev machine's library, `D:\victory-data\pipelines\library.bin` (`shaderlib::DEV_LIBRARY`), if that file
   exists: dev runs keep working while the game's own library is missing or part built. Nothing writes there
   but a developer's own `pipelines build` (whose inputs still default to `D:\victory-data`).
4. None: an empty library (`Library::empty`). Every program is offered to `rebuild_pipelines::ondemand` and
   translated on a pool worker at pipeline creation, cached in `<pipelines dir>\ondemand\` for the next session.
   A missing, part-built or stale library never blocks a launch; the first minutes stutter instead.

## In the launcher

- **First Play** of a game whose library is missing, part built or stale opens **Preparing shaders** on the Play
  tab: what is happening and why it happens once, programs done of total, an ETA from this run's rate, what was
  compiled, kept from before, not translated yet and refused by DXC, the elapsed time and the workers. When the
  build finishes, the game launches. Buttons: **Play now** (stop the build and launch; the layer translates what
  is missing), **In the background** (back to Play; the band shows the progress), **Continue later** (stop; kept).
- **Adding a game** starts its build in the background.
- **The Play band** shows one line: "Shaders ready: 12,981 programs (427 MB)", "Preparing shaders: 4,210 of
  25,694 · about 12 min left", "Shaders paused at …", "Shaders out of date (the translator changed)", with Show or
  Prepare now.
- **Diagnostics → Storage** shows the same line as "Shader library", with Pause or Prepare now, and a progress
  bar while it runs. **Clear cache** stops a running build first, then deletes the folder; the next Play builds it
  again from nothing (tested: `a_job_runs_to_the_end_and_a_cleared_cache_builds_again`).
- A launch passes `REBUILD_DXC` (the DXC the launcher found) to the layer, so on-demand translation uses the same
  bundled compiler.
- A game without `shaders.rpf`, or a machine without DXC, launches straight away on demand (and the band says
  why).

## DXC: found, bundled, licensed

`rebuild_pipelines::dxc::locate` (also the launcher's system check and `translate::dxc_path`):

1. `REBUILD_DXC`, if it exists;
2. **bundled**: `<exe dir>\dxc\dxc.exe`, else `<exe dir>\dxc.exe`, each needing `dxcompiler.dll` (the compiler)
   and `dxil.dll` (the validator, which signs the DXIL; the runtime refuses unsigned DXIL) in the same folder. An
   incomplete copy is an error naming the missing files, never a silent fall back;
3. the newest `dxc.exe` of the Windows SDK (developer machines).

With none, the error says where to put the files. **A release puts the three files in `dxc\` beside the
launcher**: `pipelines bundle-dxc <release folder>` copies them from the machine's DXC, with a `README.txt` naming
the source and licences. DXC binaries are never in git.

**Licence.** DXC is Microsoft's open-source DirectX Shader Compiler (https://github.com/microsoft/DirectXShaderCompiler),
built on LLVM/Clang. `dxc.exe` and `dxcompiler.dll` are under the **University of Illinois/NCSA Open Source
License** (the LLVM Release License; `LICENSE-LLVM.txt` in a DXC release, with `LICENSE-MIT.txt` for parts). It
permits redistribution in binary form with its copyright notice and disclaimer, which the bundle's licence files
carry. `dxil.dll` is distributed by Microsoft under its own licence for redistribution with applications
(`LICENSE-MS.txt` in a DXC release). Bundle from an official DXC release so all three licence files ship beside the
binaries. DXC runs as a separate program, invoked by path; nothing of it is linked into this GPL-3.0-only workspace.

## Measured

See `agents/shaderlib.md` for the full-build measurement on the dev machine (corpus read from the game folder,
scratch cache, 4 workers while other lanes run) and the card run with the runtime on that library.
