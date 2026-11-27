# Lane `dstorage`: the console's DirectStorage (Build 3 piece)

Crate `crates/dstorage` (package `rebuild-dstorage`), branch `agent/dstorage`. No GPU, no title code, no game
bytes: every test and the benchmark make their own files. Reference read for design only: the fork's
`crates/victory-hle/src/modules/dstorage.rs`, `crates/victory-formats/src/bcpack.rs`, `docs/design/gpu-textures.md`,
`docs/asset-policy.md`, `docs/research/{native-paths,caching}.md` and its two timing-run logs of 2026-11-25.

| # | task | state | proof |
|:--:|:--|:--|:--|
| D1 | factory, queue, status array and file objects, by vtable slot | âœ… | `com.rs`; `d1_layouts_match_the_title`, `d1_get_factory_and_interfaces` |
| D2 | requests from files through `crates/vfs` on workers; in-order completion through status arrays | âœ… | `engine.rs`; 6 `d2_*` tests, plus `worker_writes_are_seen_by_write_watch` |
| D3 | zlib (zlib-rs 0.6.9, Zlib licence) | âœ… | `d3_zlib_round_trip`; throughput below and in [`dstorage-bench.txt`](dstorage-bench.txt) |
| D4 | BCPack: what it is, CPU or GPU | ✅ answered (CPU) | below |
| D5a | BCPack tables from a file outside git | ✅ | `bcpack::load_tables`, `Decoder::from_config`; `d5a_tables_load_or_stay_refused` |
| D5b | our own BCPack decoder, exact against the fork's | ✅ on synthetic streams with the real tables; the title's own requests: **for the lead**, item 6 | `bcpack/`; `d5b_*` (5 tests); D5 below |
| D5c | the decode cache | ✅ | `cache.rs`; `d5c_*` (4 tests), `d5_engine_requests_decode_through_the_decoder` |

`cargo test --release -p rebuild-dstorage`: **21 tests green** (10 before D5), 0 warnings. With
`REBUILD_BCPACK_TABLES` set, `d5b_output_equals_the_forks_on_the_real_tables` also checks the real tables; without
it, it skips.

## Built

- **`lib.rs`**: the title's layouts. `Request` (0x48 bytes, every field offset asserted), `QueueDesc` (0x20), the
  option bits, the four interface ids, the HRESULTs.
- **`com.rs`**: the objects the title holds, as method tables of `extern "system"` functions. `DStorageGetFactory`
  hands out the factory that `install(engine)` set. Every slot nobody implemented returns `E_NOTIMPL` and is
  counted per object and slot (`com::missed()`): a call there is a discovery, never a guess.
- **`engine.rs`**: the request engine. No global state; it owns its workers.
  - **Workers** (default: half the logical processors, at least 2) take reads from four priority classes. Each
    class has a virtual clock charged `bytes x weight` (1000 / 100 / 10 / 1 for low / normal / high / real-time,
    about 10x per level); the class with the earliest clock goes next, FIFO inside it.
  - **Staging budget** (`SetStagingBufferSize`, 32 MiB by default): reads that buffer their source (zlib, BCPack,
    page destinations from a file) hold their source size while they run. A plain read into an address goes
    straight from the file into the destination; memory sources are read in place.
  - **Completion is what is ordered.** Reads finish in any order. A status entry is written once every read
    enqueued before it on its queue has finished, with the first failure among those reads since the previous
    entry, or `S_OK`. A failure is reported once.
  - **Queue behaviour the title depends on** (the fork found it in runs): a queue that holds half its capacity of
    unsubmitted commands submits itself, but leaves reads **into physical pages** (and any status behind one) for
    the title's own `Submit`, because the title writes those page lists right before calling it. `EnqueueRequest`
    blocks while the queue is full (one slot stays free); if the full queue is holding page reads, they go then.
    `Close` drops what has not started and returns once the running reads finish.
  - **Destinations**: an address is written in place; a page destination (bit 21) goes through the `Pages` trait
    (64 KiB frames: the lead binds it over `runtime::physical`). Without one, page reads fail `E_NOTIMPL`.
  - **BCPack** (bits 1-4): through the `BcPack` trait (zlib first into `intermediate_size`, then BCPack: the
    console's order). No decoder is installed, so these requests fail `E_NOTIMPL` and are counted
    (`Stats::bcpack_refused`).
- **`zlib.rs`**: one inflate state per worker, reset per request, one `Finish` call straight into the destination.
- **`bin/dsbench.rs`**: the D3 numbers.

## D1: what the title calls (by slot)

From the fork's reading of the title's call sites and its run logs. The 2020 public DirectStorage order is given
where the console's slot matches it; the console's interfaces have grown since, so their ids are new.

**Export:** `xcrdapi.dll!DStorageGetFactory(REFIID, void**) -> HRESULT`, asked for the factory interface
`62959d11-d45f-4884-a0a3-f7862ea243ae`.

| object (iid) | slot | offset | method | what the title does |
|:--|--:|--:|:--|:--|
| factory | 4 | 0x20 | `OpenFile(const wchar_t* path, REFIID, void** file)` | opens its packages on `G:` (79 per launch in the fork's logs) with file iid `43704f9f-27c6-4a9f-8815-d16527894df4` |
| factory | 5 | 0x28 | `CreateStatusArray(u32 capacity, const char* name, REFIID, void**)` | 17 arrays, capacity 1, iid `cd66d752-2786-44f7-9070-e2a6fdad3f1b` |
| factory | 8 | 0x40 | `SetStagingBufferSize(u32 bytes) -> HRESULT` | 32 MiB; the title stops if this fails |
| factory | 10 | 0x50 | `CreateQueue(const QueueDesc*, REFIID, void**)` | 17 queues: source 0 (files), capacity 4159, priority 2 (real-time), unknown field 1, iid `3c02cb0c-c050-4195-99a1-a1ce94ba33c5` |
| factory | 3, 6, 7 | 0x18, 0x30, 0x38 | (2020: CreateQueue, SetCpuAffinity, SetDebugFlags) | not seen called |
| queue | 3 | 0x18 | `EnqueueRequest(const Request*)` | |
| queue | 4 | 0x20 | `EnqueueStatus(IStatusArray*, u32 index)` | |
| queue | 6 | 0x30 | `Submit()` | right after filling the batch's page lists |
| queue | 8 | 0x40 | `Close()` | the title ends queues with this, not `Release` |
| queue | 5, 7, 9-11 | 0x28, 0x38, 0x48-0x58 | (2020: EnqueueSignal, CancelRequestsWithTag, GetErrorEvent, RetrieveErrorRecord, Query) | not seen called |
| status array | 3 | 0x18 | `IsComplete(u32 index) -> bool` | polled on entry 0 in a `Sleep(1)` loop |
| status array | 4 | 0x20 | `GetHResult(u32 index) -> HRESULT` | the title never reads it |
| file | 3+ | | (2020: Close, GetFileInformation) | not seen called; `Release` closes the vfs handle |

**Request** (0x48): `options u64` Â· `destination ptr` (or a list of 64 KiB page numbers) Â· `page_offset u64` (low 16
bits: start in the first page) Â· `destination_size u32` Â· `source ptr` (a file object, or an address) Â· `offset u64` Â·
`source_size u32` Â· `intermediate_size u32` Â· `tag u64` Â· `name ptr`.
**Options:** bit 0 zlib Â· bits 1-4 BCPack BC format (0: none) Â· bit 21 page destination Â· bit 22 memory source.
Bits 5-20 and 23+ (another transform, a source in physical pages) are not used by the title's packages and fail
`E_NOTIMPL`.

## Measured (D3)

`cargo run --release -p rebuild-dstorage --bin dsbench`, synthetic data (about 1.3:1 under zlib, so mostly
literals: a slow case for inflate), file in the OS cache. Raw output: [`dstorage-bench.txt`](dstorage-bench.txt).

| case | MB/s out | note |
|:--|--:|:--|
| inflate, 1 thread, 64 KiB chunks | **397** | 165 Âµs a chunk |
| inflate, 1 thread, 1 MiB chunks | 376 | |
| engine, 1 worker, 4,096 x 64 KiB zlib requests | 336 | read + inflate + write + completion: 15 % over inflate alone |
| engine, 2 workers | 688 | |
| engine, 4 workers | 1,444 | linear |
| engine, 8 workers | **2,024** | 256 MiB in 0.133 s |

For scale: the fork wrote 10.6 GB through DirectStorage in a session; at 2 GB/s that is about 5 s of work on 8
workers, spread over the session. zlib is not the bottleneck; the disk (550 MB/s SATA in the fork's notes) is.

## D4: BCPack â€” answer

**What it is** (the fork's decoder and notes): a lossless re-packing of BC1/3/4/5/6H/7 block streams that the
console's DirectStorage hardware unpacks on the way to memory (BC2 is not used by the title). A stream has no
header: 12 or 16 interleaved rANS lanes, each a run of fragments coded with one of 128 static models; a fixed
schedule decides which lane takes the next input byte. Per block it codes two back references (one of the last 256
blocks, or one of 255 per-format dictionary entries) and the residuals of endpoints and indices against them. The
output is ordinary BCn blocks **in the console's tiled layout**: detiling is a separate step (lane `xg`, or the GPU
reading console layout in place).

**CPU or GPU:** CPU. Evidence:
1. It is sequential integer entropy decoding with block-to-block references: a stream has 12-16 lanes of
   parallelism at most, which suits a CPU worker, not a GPU dispatch.
2. There is no GPU path to borrow: PC DirectStorage decompresses only GDeflate on the GPU (fork,
   `native-paths.md` (5)); BCPack would need a custom compute decoder.
3. Its output is BCn the GPU samples as is, so a CPU decode costs the GPU **nothing**.
4. Measured cost of the fork's CPU decoder (its own `bcpack_throughput` test on synthetic records and synthetic
   tables, built in a scratch folder outside git, `-j 4`, lock absent): **BC1 65.6, BC4 45.3, BC7 33.4 MB/s out per
   thread** (122 / 177 / 480 ns a block). That is 6-12x slower than inflate per byte, so BCPack, not zlib, will be
   the CPU cost of texture streaming: about 270-520 MB/s on 8 workers, on DirectStorage's own threads, never the
   title's.

**Why no decoder is built here** (section 5: "a decoder only if it is plain code with no performance cost on the
GPU side"). The code would be plain and GPU-free, but:
- **The format tables can't be committed.** Decoding needs about 78 KB of constant tables (entropy models, rank
  tables, BC7 control codes, the per-format dictionaries). The fork's own notes say they cannot be derived; it ships
  them as a single owner-approved exception outside its licence (`docs/asset-policy.md`). This repository's rule is
  "no game bytes in git", so they would have to be loaded at run time from a path outside git, which needs a config
  entry and an owner decision.
- **The fork's decoder can't be vendored.** It carries performance (it is the streaming CPU cost above), and
  section 3 allows vendoring only code that carries none. A decoder written here could only be checked against
  real streams with those tables and reference chunks, which are game-derived and outside git too.
- **The fork's decoder is not proven.** `gpu-textures.md` lists its BC3/BC5/BC7 paths as suspects for red-block
  artefacts.

**What is in place:** the engine's `BcPack` trait, the zlib-then-BCPack order with `intermediate_size`, and a
counter of refused requests. A decoder plugs in without touching the engine.

## D5: BCPack, our decoder and the decode cache (owner-approved, plan decision 8)

### Built

- **`bcpack/tables.rs`**: parses the 77,982-byte table file (same layout as the fork's `bcpack-tables.bin`: it is
  the format, not ours) into working forms: per model and bucket, `start | f << 16` and an exact reciprocal; per
  model and 256-slot window, the bucket, plus a **fine table** (bucket per slot) for windows that hold a bucket
  start, so finding a slot's bucket never searches (the real tables need 440 fine tables, 110 KB); rank tables
  pre-shifted for packed indices; dictionaries pre-packed. Wrong size, an empty bucket or a model that does not
  tile 65536 is an `Error`, never a panic.
- **`bcpack/lanes.rs`**: the lane schedule (12 or 16 rANS lanes, 8 bytes a round, a lane a position ahead waits),
  reproduced step for step. **Insight:** inside a round each lane's step depends only on its own state; lanes are
  coupled only by the byte budget, and which lanes get bytes is a prefix count over the 16-bit mask of lanes that
  ask. So a round is data-parallel: `avx2::run` steps 16 lanes at once (two 8-lane registers; gathers for the model
  tables; a 256-entry rank table assigns the bytes; header bytes, a few per fragment, go through the scalar code).
  `run` is the same schedule a lane at a time: the fallback without AVX2, and what the synthetic encoder drives.
- **`bcpack/blocks.rs`**: records to blocks. The plain formats keep endpoints and indices in two packed words so
  **one SWAR add** sums every element modulo its own width, and the packed word is nearly the output block (the
  index-code mappings are bit tricks). BC6H and BC7 follow the fork's semantics with a 128-bit writer.
- **`bcpack/consts.rs`**: the only thing taken from the fork: constant data (lane widths, BC6H/BC7 layout tables,
  the console's BC6H run order). GPL header, and a line in `CREDITS.md`.
- **`bcpack/synth.rs`**: our own synthetic tables, records and **encoder** (rANS, random fragment lengths, short
  and long lengths, reset, a model chosen per fragment, a parking fragment): every test stream is made here.
- **`cache.rs`**: the decode cache (below). **`Decoder`** (`bcpack/mod.rs`) implements the engine's `BcPack`:
  cache lookup, decode on a miss, store; counters (`DecoderStats`).
- **`bin/bcbench.rs`**: the numbers below.

### Measured: exact

A scratch harness outside git (the fork's `bcpack.rs` read from `triumph/main` with `git show`, never committed)
ran both decoders on the same inputs: our synthetic streams with four table sets (the fork's own uniform tables,
two of ours, **the real tables**), the fork's own test vectors (its stream builder and records), 24 mutations of
every valid stream (a bit flipped, cut short, a byte dropped or added) and noise.

| inputs | identical output | both refused | differ |
|:--|--:|--:|--:|
| 960 valid streams (1-4,096 blocks, BC1/3/4/5/6H/7, 4 table sets) | 960 | 0 | **0** |
| 32,640 mutated streams and noise | 17,595 | 15,045 | **0** |
| the fork's own test vectors | 36 | 0 | **0** |

Vector lanes = scalar lanes = the fork, everywhere. In the repository, `d5b_output_equals_the_forks_on_*` check
our output against FNV hashes of **the fork's output** on fixed synthetic streams (synthetic tables always; the
real tables when `REBUILD_BCPACK_TABLES` is set), plus `d5b_vector_and_scalar_lanes_agree` (valid and mutated
streams), `d5b_lanes_give_back_every_record` and `d5b_corrupt_input_is_refused_without_a_panic`.

**Not done yet: the title's own BCPack requests** (D5b's closing condition), because this lane was told not to
open the game folder. Synthetic streams with the real tables exercise the real models, ranks, control words and
dictionaries, but not the console encoder's own choices (fragment lengths, state carried without reset). See
"for the lead", item 6. The fork's BC3/BC5/BC7 paths, suspects for red blocks, are therefore reproduced **bit
for bit, including any bug they have**; only real data with a reference (a console capture) can tell.

### Measured: speed (one thread, i7-10700KF, the same streams for both decoders, best of 7)

From the harness, fork against ours on identical streams of 2^14 blocks:

| tables | BC1 | BC3 | BC4 | BC5 | BC6H | BC7 |
|:--|--:|--:|--:|--:|--:|--:|
| fork's uniform tables: fork, MB/s out | 59 | 60 | 60 | 61 | 30 | 30 |
| ours | **96** | **97** | **92** | **91** | **50** | **44** |
| real tables: fork | 21-28 | 28 | 25 | 27 | 17 | 17 |
| ours | **60-64** | **58** | **59** | **62** | **41** | **29-36** |
| speed-up, real tables | 2.2-3.0x | 2.0x | 2.3x | 2.3x | 2.5x | 1.7-2.1x |

The fork's D4 numbers (65.6 / 45.3 / 33.4 MB/s) were on its uniform tables; real tables are slower for both
decoders (more bytes per block, narrower buckets). Scalar path (no AVX2): 1.2-1.8x the fork. Threads
(`bcbench`, real tables): BC1 59 / 79 / 157 / 199 MB/s and BC7 30 / 49 / 95 / 160 MB/s on 1 / 2 / 4 / 8 threads.
Other builds shared the machine during these runs (the fork's own numbers moved 15 % between runs): read single
figures as +-15 %; the ratios come from interleaved runs.

What bounds it: 2.5-5 schedule rounds per block, each a chain of dependent gathers (window, then bucket record)
and the byte assignment. Tried and dropped: a branch-free scalar step (selects instead of branches) was
**slower** (0.75x the fork): the cost was register spills, not mispredicts.

### D5c: the cache

`%LOCALAPPDATA%\triumph-rebuild\bcpack-cache` (or `REBUILD_BCPACK_CACHE`; `off` disables it), cap 8 GiB
(`REBUILD_BCPACK_CACHE_MB`). Key: BLAKE3 over the decoder version, the table file's hash, the BC number, the
output size and the packed bytes (128 bits, as the file name). Entry: a 32-byte header (magic, BC, size, CRC-32
of the output) and the output, read straight into the destination and checked; a corrupt or partial entry is
deleted, counted and rebuilt. Writes go to a temporary file renamed over the entry; leftovers are removed at
start. Eviction: least recently used first (a hit refreshes the file time, so the order survives restarts).

| BC7 output | decode | cache hit | hit / decode |
|--:|--:|--:|--:|
| 4 KiB | 122 us | 162 us | 1.33 (slower) |
| 16 KiB | 588 us | 161 us | 0.27 |
| 64 KiB | 2,446 us | 213 us | 0.087 |
| 256 KiB | 7,746 us | 236 us (1.1 GB/s) | 0.031 |
| 1 MiB | 26,601 us | 696 us (1.5 GB/s) | 0.026 |

**Decided:** a hit costs about 160 us whatever the size (opening a file on Windows), so **outputs below 32 KiB
are not cached** (decoding 32 KiB takes about 330 us for BC1 and 1,200 us for BC7). Above that a hit is 10-40x
cheaper than decoding.

### Raw `bcbench` output (synthetic streams; real tables from `REBUILD_BCPACK_TABLES`)

```
bcbench: vector lanes AVX2
decode 1 thread  synthetic tables BC1    96.2 MB/s out (   83 ns/block)  scalar  32.2 MB/s  in 9.0 B/block
decode 1 thread  synthetic tables BC3    81.4 MB/s out (  196 ns/block)  scalar  30.3 MB/s  in 15.8 B/block
decode 1 thread  synthetic tables BC4    64.4 MB/s out (  124 ns/block)  scalar  29.3 MB/s  in 8.6 B/block
decode 1 thread  synthetic tables BC5    78.5 MB/s out (  204 ns/block)  scalar  46.1 MB/s  in 15.7 B/block
decode 1 thread  synthetic tables BC6    48.8 MB/s out (  328 ns/block)  scalar  23.3 MB/s  in 22.8 B/block
decode 1 thread  synthetic tables BC7    45.6 MB/s out (  351 ns/block)  scalar  22.1 MB/s  in 24.4 B/block
decode 1 thread  real tables      BC1    69.3 MB/s out (  116 ns/block)  scalar  31.9 MB/s  in 11.6 B/block
decode 1 thread  real tables      BC3    73.2 MB/s out (  219 ns/block)  scalar  33.6 MB/s  in 21.2 B/block
decode 1 thread  real tables      BC4    44.2 MB/s out (  181 ns/block)  scalar  29.5 MB/s  in 12.2 B/block
decode 1 thread  real tables      BC5    67.0 MB/s out (  239 ns/block)  scalar  31.9 MB/s  in 22.9 B/block
decode 1 thread  real tables      BC6    42.7 MB/s out (  375 ns/block)  scalar  18.5 MB/s  in 30.1 B/block
decode 1 thread  real tables      BC7    37.9 MB/s out (  422 ns/block)  scalar  19.5 MB/s  in 30.3 B/block
decode 1 threads BC1:    59.1 MB/s out in all
decode 2 threads BC1:    78.6 MB/s out in all
decode 4 threads BC1:   156.5 MB/s out in all
decode 8 threads BC1:   199.0 MB/s out in all
decode 1 threads BC7:    29.6 MB/s out in all
decode 2 threads BC7:    48.9 MB/s out in all
decode 4 threads BC7:    94.6 MB/s out in all
decode 8 threads BC7:   159.5 MB/s out in all
cache BC7      4 KiB out: decode   121.5 us (  33.7 MB/s)  hit   161.5 us (   25.4 MB/s)  hit/decode 1.329
cache BC7     16 KiB out: decode   588.4 us (  27.8 MB/s)  hit   160.8 us (  101.9 MB/s)  hit/decode 0.273
cache BC7     64 KiB out: decode  2445.8 us (  26.8 MB/s)  hit   212.8 us (  308.0 MB/s)  hit/decode 0.087
cache BC7    256 KiB out: decode  7746.0 us (  33.8 MB/s)  hit   236.4 us ( 1108.9 MB/s)  hit/decode 0.031
cache BC7   1024 KiB out: decode 26600.8 us (  39.4 MB/s)  hit   695.5 us ( 1507.7 MB/s)  hit/decode 0.026
cache: 45 hits, 5 decoded
```

### The table file (D5a)

Never in git. Source: the fork, `git -C D:\Developer\fxceless-victory show
triumph/main:crates/victory-formats/src/bcpack-tables.bin` (last changed in commit `2849477`, 2026-11-23: the
owner-approved exception in the fork's `docs/asset-policy.md`). **77,982 bytes, SHA-256
`1f1d4760419202bb5672f0c428ce51e456b5d08fbaec8f371d043fd94e01c36b`.** On this machine it is at
`%LOCALAPPDATA%\triumph-rebuild\bcpack-tables.bin`. The registry entry `dstorage.bcpack_tables` (already in
`crates/config`) or `REBUILD_BCPACK_TABLES` names it. A missing, wrong-sized or malformed file gives one log line
(`dstorage: ...; BCPack requests will be refused`), and BCPack requests keep failing `E_NOTIMPL`.

## Decided

- zlib through **zlib-rs** (Zlib licence, pure Rust, no C toolchain): 397 MB/s a thread on a literal-heavy stream.
- Bytes of a destination past the end of a zlib stream that ends early are **unspecified** (zlib-rs may write
  there while copying). They stay inside the request's destination; the title's chunks fill it exactly.
- File objects are registered in the engine by their address, so a request naming an unknown file fails
  `E_INVALIDARG` instead of dereferencing it.

## For the lead

1. **Binding:** `crates/runtime` resolves `xcrdapi.dll!DStorageGetFactory` to `rebuild_dstorage::DStorageGetFactory`,
   after `rebuild_dstorage::install(Engine::new(vfs, Config { pages: Some(..), .. }))` with the `G:` vfs.
2. **`Pages` over `runtime::physical`:** `write(pages, offset, data)` with 64 KiB frames (`physical::FRAME` is the
   same size). The fork retried a failed page write up to 12 times (about 0.9 s) because the title sometimes
   names a page it makes a moment later; its runs 175-178 lost 12 to 446 reads a launch without the retry. Put
   any retry in the `Pages` impl, which knows the page table.
3. **Graphics memory written by workers:** the write-watch on graphics memory (`runtime::xmem`) records every
   page the workers write, so the Build 2 ring sends them with no extra call. Measured by
   `worker_writes_are_seen_by_write_watch`: a 256 KiB destination in a `MEM_WRITE_WATCH` region, **64 of 64 pages
   recorded** both for a plain read (`ReadFile` straight into the destination, a kernel copy) and for a zlib read.
   The fork needed explicit calls (`kernel_will_write`, `title_memory_written`) only because it used guard pages.
   Not checked: an unbuffered (`FILE_FLAG_NO_BUFFERING`, DMA) read; `vfs` opens files buffered.
4. **BCPack (D5, done):** bind it with `Config { bcpack: rebuild_dstorage::Decoder::from_config(<dstorage.bcpack_tables>)
   .map(|d| d as Arc<dyn BcPack>), .. }`. `from_config` reads the setting (or `REBUILD_BCPACK_TABLES`) and opens the
   cache from `REBUILD_BCPACK_CACHE` / `REBUILD_BCPACK_CACHE_MB`. Proposed registry entries: `dstorage.bcpack_cache`
   (path; empty = `%LOCALAPPDATA%\triumph-rebuild\bcpack-cache`, `off` = none) and `dstorage.bcpack_cache_mb`
   (int, default 8192). `Decoder::stats()` gives decoded / hits / failed and their times, for the frame line.
5. **Workers:** the default is half the logical processors. All 17 of the title's queues are real-time priority,
   so the priority classes don't matter for this title; core placement (the fork's `VICTORY_DISK_CORES=slow`,
   unmeasured) is the lead's call with the frame line.
6. **The title's real BCPack requests (D5b's last check).** This lane did not open the game folder (it was told
   not to). To close D5b, either the lead runs the title through this engine with the decoder bound and a switch
   that saves each BCPack request (packed bytes, BC number, size) outside git, or a helper is allowed to read the
   title's packages read-only through `REBUILD_GAME_DIR`. Then decode every request with ours and the fork's (the
   scratch harness does this; it can become a `REBUILD_GAME_DIR`-gated test) and compare block for block.
   Nothing from it would be committed.
7. **Test fix the lead asked for:** `d2_priorities_share_the_workers` no longer reads completion from the polling
   thread. The worker records the order of its own writes through a `Pages` destination, real-time is submitted
   first (submitting low first let the worker start on it alone), and the only wall-clock bound is a 120 s hang
   guard. 40/40 runs passed with 8 copies running at once.

## Next

- D5b on the title's real requests (item 6).
- Bind into `crates/runtime` (the lead's).
