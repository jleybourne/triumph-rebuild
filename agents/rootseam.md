# Lane `rootseam`: the title's root arguments in the program library's layout

Design and contract: [`docs/design/root-seam.md`](../docs/design/root-seam.md). Closes `deepdive.md` 11.1 item 1.

## Built

- **The contract.** User-data dword `user_data + j` of a translated program holds what the console's hardware
  puts in that user SGPR:
  - a constant: the title's constant `j`;
  - a table: the byte offset of its first descriptor from the start of its bound heap (console sizes 32 and 16);
  - a root view: its console GPU address, low then high;
  - heap bases and immediate samplers: 0.
  The shadow table is the bound view heap's, at `t0 space101`.
- `crates/pipelines/src/rootmap.rs`: the console program's root layout (`RootParam`) becomes a `UserMap`
  (per stage: the library parameter and a source for each of up to 16 dwords).
- `crates/runtime/src/pipeline.rs`: reads each program's layout once per hash at creation, from the bytes the
  title passes. Graphics descriptions and streams note where their programs lie. It interns the map
  (`&'static`), and `host()` returns it in `HostPipeline::user`. A pipeline whose layout cannot be read gets no
  host pipeline: its draws are dropped and counted, and the report says "root layout unread N; user-data maps N".
- `crates/xlist` (`console/list.rs`):
  - While a translated pipeline is bound on a side, the title's root-argument calls are kept as console state
    only. Nothing reaches the host list by the title's parameter numbers.
  - The title's `Set*RootSignature` ends its arguments' lifetime, but the library root stays bound.
  - At the next draw or dispatch after a change (arguments a stage reads, the pipeline, the heaps, a new host
    list after a cut), each stage's words are built. If they differ from those last written, one
    `Set*Root32BitConstants` is issued, and the shadow table is bound when its address changed.
  - Lock-free, with no allocation. `ListStats` gains `user_sets`, `shadow_sets` and `user_unset`.
- `ListHost` glue:
  - `HeapBinding::shadow_va` (d3d12x fills it from the heap's shadow table);
  - `ListHost::sync_descriptors(start, len)` (d3d12x: `Resources::sync`);
  - `xlist::console::sync_descriptors(list)`, called by `submit.rs` for each list before submission. It
    re-mirrors the descriptor tables the list's translated programs read, as min/max spans per heap.

## Measured

- **Per-draw cost on the stand-in:** the bench frame (4,500 draws, 20,629 calls), best of 20. Both tests are in
  `console/tests.rs`.

  | path | per draw | per call | frame | user-data writes (last list) | shadow binds (last list) | test |
  |:--|--:|--:|--:|--:|--:|:--|
  | title layout (before) | 0.067 µs | 0.0145 µs | 0.300 ms | — | — | `our_cost_per_draw_on_the_stand_in` (asserts < 0.43) |
  | through the root seam (every pipeline translated, 8 VS + 4 PS user dwords) | 0.112 µs | 0.0244 µs | 0.504 ms | 122 | 1 | `our_cost_per_draw_through_the_root_seam` (asserts < 1 µs) |

  The seam adds about 0.045 µs a draw on the stand-in. On the card, the title's table, CBV and constant calls no
  longer reach the driver: 3-4 forwarded calls become at most 2 constant writes, and only on a change. So the
  listbench figure of 0.43-0.47 µs a draw should not rise. That was not measured on the card (no card time
  taken).
- **WARP** (`crates/pipelines/tests/rootseam_warp.rs`, by hand with `REBUILD_SHADERS` and
  `REBUILD_PIPELINES_LIB` set to the `D:\victory-data` copies): two translated compute programs from
  `library.bin`. Each was dispatched through a console list, with root arguments set by the parameter numbers in
  the console program's own layout.
  - `9146291b38ea9ce1`: table at parameter 0 and constant 0 of parameter 1, i.e. the stride. All 64 threads wrote
    (0, 1, 1, 0) at 32-byte steps through the table's descriptor.
  - `e4918eaa6752a8a0`: the table's third descriptor, indexed, with stride 8 and 20 records from the shadow table.
    Threads 0-19 wrote 0x7f7fffff and threads 20-31 nothing.
  - 1 user-data write and 1 shadow bind each. 26,084 programs read; 1.4 s in all.
- `cargo test --release --no-fail-fast`: exit 0, 0 warnings.

## For the lead

1. **Wrong view type at the primary index.**
   - Where: `crates/sir/src/lift/res.rs:176, 262, 324` against `crates/d3d12x/src/heap.rs` `buffer_plan`.
   - What: the lifter reads `ConstantBuffer<SirCb>`, `RWByteAddressBuffer` and `RWBuffer<float4>` at the console
     index itself, but d3d12x puts a raw SRV there and the CBV/UAV/typed view at the shadow entry's alternate.
   - Effect: on a real heap that is a view of the wrong type (undefined). The WARP test places a raw UAV at the
     index the program reads, so it does not hit this.
   - Fix: either the lifter indexes `sir_shadow.Load(i * 16 + 8)` for those three, or the mirror's primary for
     UAV and CBV records becomes their own kind.
2. **`lead` is not applied.** A buffer descriptor not 16-byte aligned reads `lead` bytes early: the raw view
   starts at the aligned address, and the lifted code adds nothing.
3. Not mapped yet:
   - ExecuteIndirect arguments that change root constants or views;
   - descriptors read by dynamic index past a program's static ranges (the sync covers the static ranges only,
     until write-watch);
   - graphics on a device (stand-in only).

## What a title run should show

- The report's pipelines line: `root layout unread 0` (or a few, named by that count) and `user-data maps N`, with
  N in the tens to hundreds (distinct layouts, not pipelines).
- The lists' stats: `user_sets` and `shadow_sets` above 0 wherever draws or dispatches run.
- `user_unset`: low. A high count means the title binds after the draw, or tables lie outside the bound heaps.
- No D3D12 debug-layer errors about root parameter indices (before, the title's slot numbers hit the library root
  signature).
- Visible output still waits on item 1 above for buffer UAVs and CBVs. Programs that only read textures, read
  root constants, or store through texture UAVs should now bind correctly.
