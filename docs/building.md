# Building triumph-rebuild

This page is for developers: how to build from source, run the launcher and the layer, run the tests and the
benchmarks, and make the release zip. If you only want to play, read the [player's guide](user-guide.md).

The repository holds no game files, and nothing derived from them may ever be committed: no shader binaries,
no translated shaders, no title files, no paths to them in code. Tools read a game's location from settings or
an environment variable at run time.

## What you need

| | |
|:--|:--|
| Windows | 10 or 11, x64 |
| Rust | 1.100 (`rustup`), MSVC target `x86_64-pc-windows-msvc` |
| C++ build tools | Visual Studio 2022 Build Tools with "Desktop development with C++" (the linker and the Windows SDK) |
| Windows SDK | 10.0.26100 or newer. Its `dxc.exe` (DXC 1.8.2502) compiles shaders, and the tests that need DXC use it |
| GPU | Direct3D 12 with tiled resources tier 2 for runs on the card. Tests use WARP, the software rasteriser, so no card is needed for them |
| Git | to build, and for the version `scripts/package.ps1` stamps |

## Build

```powershell
cargo build --release                        # everything: target\release\
cargo build --release -p rebuild-launcher    # the launcher only: rebuild-launcher.exe
cargo build --release -p rebuild --bin rebuild   # the layer only: rebuild.exe
```

The release profile uses thin LTO, one codegen unit and `panic = "abort"`. It keeps line tables in the PDB, so
crash reports name functions and lines.

## Run from source

**The launcher:**

```powershell
cargo run --release --bin rebuild-launcher
cargo run --release --bin rebuild-launcher -- --check       # the system check, printed (no window)
cargo run --release --bin rebuild-launcher -- --recommend   # this PC's specs and the recommended settings
cargo run --release --bin rebuild-launcher -- --shots <dir> # the screenshot plan (fixture data, no game)
```

From `target\release` the launcher is never portable: cargo marks that folder with `CACHEDIR.TAG`, and saves
must not live in a build's output. It uses `%APPDATA%\triumph-rebuild` and `%LOCALAPPDATA%\triumph-rebuild`
instead. Set `REBUILD_DATA_DIR` to give it a folder of its own ([storage.md](design/storage.md)). Play starts
`rebuild.exe` from beside the launcher, or `REBUILD_EXE`.

**The layer, as the launcher starts it:**

```powershell
target\release\rebuild.exe --config <data>\games\<game-id>\settings.toml [--seconds N] [--sample N]
```

It reads the settings file and checks it against the registry (`crates/config`). Bad values are refused with
a reason. Then it sets the `REBUILD_*` variables the runtime reads and starts the title in this process.
`--seconds N` ends the run after N seconds, and a watchdog ends the process 15 s later. `--sample N` samples
the title's main thread every N seconds, which suspends it, so it is opt-in. Exit codes: 2 is bad arguments or
a settings file that does not parse, 1 is no game folder or a title that did not load, otherwise the run's
own. Started without the launcher, it makes its session folder beside the settings file
(`sessions\<stamp>-<profile>`).

**`runtitle`, the same launch from the environment only:**

```powershell
$env:REBUILD_GAME_DIR = '<the title folder>'
cargo run --release --bin runtitle             # load and bind only (no title code runs)
cargo run --release --bin runtitle -- --run    # and run its startup (default 60 s, --seconds N)
```

Only one title run at a time: the layer holds the mutex `Local\triumph-rebuild-title-run`, and import and save
writes are refused while it is held. A run gets a memory budget scaled to installed RAM, a watchdog and a crash
reporter (`crash-<pid>.txt` in the session folder).

**The shader library.** A title run needs its pipeline library, built from a copy of the title's `shaders.rpf`
(never the game folder itself). First the `translate` bin writes DXIL into `REBUILD_TRANSLATE_OUT` (its modes,
`v1` and `v2`, are described in `apps/rebuild/src/bin/translate.rs`). Then
`cargo run --release -p rebuild-pipelines --bin pipelines -- build` writes `library.bin` into
`REBUILD_PIPELINES_OUT`. Programs outside the archive are translated on demand at run time and cached. These
outputs are game-derived and stay outside git. The launcher will build the library on first run; until it
does, this is a developer step ([deepdive §5](../deepdive.md#5-the-shader-recompiler-in-depth)).

## Tests

```powershell
cargo test --release --no-fail-fast      # 810 tests; no game data and no GPU needed
```

- GPU tests use **WARP**, or skip themselves (passing) when WARP, DXC, a game copy or a corpus copy is missing.
- Tests that need the card are `#[ignore]`d and run by hand, one at a time, only while `agents\MEASURING` is
  absent: `cargo test --release -p <crate> -- --ignored --test-threads=1`.
- Commit only on an exit code of 0 from the full run, never on a summary line.
- On a shared machine, build with `-j 4` and check for `agents\MEASURING` first: while it exists the lead is
  timing a run, and a build skews it.

## Benchmarks and proofs

Each prints its result. `runs/` holds the write-ups and the raw output of the runs the README cites. These use
the GPU: run them only on a quiet machine, while `agents\MEASURING` is absent.

```powershell
cargo run --release --bin gputest                     # the GPU writes, the CPU reads, no copy
cargo run --release --bin gputest -- --bare           # control: never blame the machine
cargo run --release --bin gputest -- --tiled          # Build 1: tiles, console addresses, sharing
cargo run --release --bin gputest -- --arena          # what cannot be tiled on this card
cargo run --release --bin gputest -- --scale 10       # commit 10 GiB through the tiles
cargo run --release --bin gputest -- --ring           # Build 2: the upload ring, 60 frames
cargo run --release --bin gputest -- --labels         # Build 2: GPU-written labels into title memory
cargo run --release --bin uploadbench [-- --tiled]    # Build 0 (a): the upload benchmark
cargo run --release --bin listbench                   # Build 0 (b): per-draw translation
cargo run --release --bin runtimecensus               # Build 3: what the title's imports bind to
$env:REBUILD_GAME_DIR='<title folder>'; cargo run --release --bin maptitle   # Build 3: map and bind the title (no code run)
cargo run --release --bin sirtest                     # Build 4: shader core IR → HLSL → DXC → GPU, exact
```

## Key environment variables

The launcher sets most of these for the layer. The full list, with where each is read, is in
[deepdive §7.5](../deepdive.md#75-every-rebuild_-environment-variable).

| variable | what it does |
|:--|:--|
| `REBUILD_DATA_DIR`, `REBUILD_LOCAL_DIR`, `REBUILD_CACHE_DIR` | the data, log and cache roots ([storage.md](design/storage.md)) |
| `REBUILD_GAME_DIR` | the title's folder, read-only (`runtitle`, `maptitle`, the censuses) |
| `REBUILD_GAME_ID`, `REBUILD_GAME_BUILD` | which game's saves, storage and caches the layer uses (the launcher sets them) |
| `REBUILD_SESSION_DIR`, `REBUILD_FRAME_LOG` | the session folder and the frame-line file |
| `REBUILD_RUN_SECONDS`, `REBUILD_MEASURE_FROM` | a timed run: summary and exit after N s; where the measuring window starts |
| `REBUILD_EXE` | the layer the launcher starts (default `rebuild.exe` beside it) |
| `REBUILD_DXC` | `dxc.exe` to use. Otherwise the one beside the running exe (the release zip's), then the newest in the Windows SDK |
| `REBUILD_PIPELINES_LIB`, `REBUILD_PIPELINES_OUT`, `REBUILD_PSO_CACHE_DIR` | the pipeline library and the driver pipeline cache |
| `REBUILD_SHADERS`, `REBUILD_TRANSLATE_OUT` | a copy of `shaders.rpf`, and the translator's output |
| `REBUILD_BCPACK_TABLES` | the BCPack format tables you supplied (never in git) |
| `REBUILD_STREAMLINE_DIR`, `REBUILD_DLSS_DLL`, `REBUILD_FIDELITYFX_DIR` | upscaler runtimes you supplied |
| `REBUILD_INPUT_SCRIPT` | timed presses and captures, e.g. `40s:A,60s:cap` |
| `REBUILD_HUD` | `0` hides the performance monitor |
| `REBUILD_LIMIT_RAM_MB`, `REBUILD_LIMIT_CPU_CORES`, `REBUILD_LIMIT_CPU_RATE` | hardware limits for testing |
| `REBUILD_MEASURE_LOCK` | while this file exists, the launcher's system check creates no D3D12 device |

## Packaging

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package.ps1              # build, check, zip
powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -SkipBuild   # use target\release as it is
powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -CheckOnly   # only the game-data check
```

It writes `dist\triumph-rebuild-<version>-windows-x64.zip` and its `.sha256`. The version is the workspace
version plus `git describe`: `0.1.0` on the tag `v0.1.0`, otherwise e.g. `0.1.0-9271847` (`-dirty` with
uncommitted changes). The zip holds the launcher, `rebuild.exe` and its PDB, DXC (`dxc.exe`, `dxcompiler.dll`,
`dxil.dll`, from Microsoft's GitHub release, pinned by SHA-256), `LICENSE.txt`, `CREDITS.md`, a `README.txt`,
`FILES.txt` (every file's size and SHA-256) and `licenses\` (DXC's three licences, Inter's OFL, and every
linked crate's licence files). Each bundled file's licence is in [CREDITS.md](../CREDITS.md#the-release-zip).

**The game-data guard.** Before it builds anything, the script checks every tracked and untracked
(not ignored) file in the repository. It refuses, and exits 1, on any of these:

- a title archive, save or shader extension (`.rpf`, `.xvd`, `.gsc`, `.cso`, `.dxil`, `.spv`, `.od`, …);
- a game package's own file names (`MicrosoftGame.config`, `settings.xml`, the console DLLs, `library.bin`,
  `bcpack-tables.bin`);
- a shader or game-data cache folder;
- a file over 5 MB;
- a DXBC/DXIL shader container anywhere in a file's bytes.

It then checks the staged package the same way. The stage must hold exactly the files on its list, and none of
our binaries may contain a shader container (ours compile their shaders at run time). CI runs the same check
first (`-CheckOnly`).

**CI** ([`.github/workflows/release.yml`](../.github/workflows/release.yml)) runs only on a pushed `v*` tag or
by hand (Run workflow). On `windows-latest` it runs the guard, `cargo build --release`, the tests (the card's
are ignored, the rest use WARP or skip) and the package script. It uploads the zip as an artifact, and on a
tag it creates a **draft** release with the zip. Nothing is published until the owner publishes the draft.
