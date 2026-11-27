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
| D4 | BCPack: what it is, CPU or GPU | âœ… answered, **no decoder built** (the format tables can't be committed: lead/owner decision) | below |

`cargo test --release -p rebuild-dstorage`: **10 tests green**, 0 warnings; 8 repeated runs, no flakes.

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
4. **BCPack (owner decision):** to decode it, the 78 KB tables must come from a path outside git (propose a config
   entry such as `dstorage.bcpack_tables`), and a decoder must be written here (or the fork's vendored by
   exception). Until then texture requests with BCPack fail `E_NOTIMPL`. How many of the title's requests carry
   BCPack is not in the fork's info-level logs; the first run through this crate counts it (`Stats`).
5. **Workers:** the default is half the logical processors. All 17 of the title's queues are real-time priority,
   so the priority classes don't matter for this title; core placement (the fork's `VICTORY_DISK_CORES=slow`,
   unmeasured) is the lead's call with the frame line.

## Next

- D4 decoder, once item 4 is decided.
- Bind into `crates/runtime` (the lead's).
