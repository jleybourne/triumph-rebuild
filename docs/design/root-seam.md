# Design: the root seam (title root arguments to the program library's layout)

Status: **built 2026-11-27 (lane `rootseam`).** Stand-in tests prove the mapping and the per-draw path. On WARP,
two translated compute programs from `library.bin` were dispatched through a console list with title-style root
arguments and wrote the words the console would. See [`agents/rootseam.md`](../../agents/rootseam.md) for the
numbers.

Inputs: [`shader-ir.md`](shader-ir.md) (the binding model is the fork's), `crates/sir/src/lift/res.rs` (how a
program reads user data and descriptors), `crates/pipelines/src/layout.rs` (the library root signature),
`deepdive.md` 4.4, 4.5 and 11.1 item 1 (the gap this closes).

## The problem

Every translated program is compiled against **one** root layout, the program library's:

| parameter | compute | graphics |
|:--|:--|:--|
| 0 | 16 root constants at `b0` (CS user data) | VS user data |
| 1 | root SRV `t0 space101` (shadow table) | PS user data |
| 2 | — | shadow table |

The flags always include both `*_HEAP_DIRECTLY_INDEXED` bits, and immediate samplers are static samplers in
`space100`.

The title, however, records root arguments against **its own** root signatures: `Set*Root32BitConstants`,
`Set*RootDescriptorTable` and `Set*Root{Constant,ShaderResource,UnorderedAccess}View`, each with its own
parameter numbers. Before this seam, xlist forwarded those calls to the host list by the title's numbers while the
library root signature was bound, and nothing bound the shadow table. No draw with a translated program could bind
correctly.

## The contract: what user-data dword k holds

On the console, the hardware loads each root parameter into the program's user SGPRs. The console program's own
record says where (its root layout, `rebuild_shader::native::RootParam`: root type, `user_data` (the first dword),
`sgprs` and `slot` (the title's parameter number)).

The lifter reads user SGPR `s` as user-data dword `k = user_data + (s − first SGPR of the parameter)`
(`Bindings::user_dword`), so `sir_ud[k / 4][k % 4]` at `b0`. **Dword k therefore holds what the console's hardware
would have put in that SGPR:**

| title root parameter (`RootType`) | dwords | value of dword `user_data + j` |
|:--|:--|:--|
| `Constants` | `sgprs` | constant `j` of the parameter as the title set it (0 if unset, counted) |
| `Table` | 1 | **byte offset** of the table's first descriptor from the start of the bound heap that holds it, in console sizes (32 bytes a view, 16 a sampler). The program divides by that size to get the index it passes to `ResourceDescriptorHeap[]` / `SamplerDescriptorHeap[]`. Host index i is console index i (`d3d12x` heap mirror). |
| `Descriptor` (root CBV/SRV/UAV) | 2 | `j = 0`: low 32 bits, `j = 1`: high 32 bits of the **console GPU address** as the title gave it. Console addresses are the translated program's window addresses (plan decision 1), so the address is not translated. |
| `ResourceHeap`, `SamplerHeap` | 1 | 0. The lifter treats the heap base as 0 and does not read the dword. |
| `ImmediateSampler`, other | — | 0. These are static samplers in `space100`, not data. |

This is the fork's reading too (`victory-gpu` `draw.rs` `user_value`: a table is `handle − heap.start`, a constant is
`c[dword]`), and the oracle compares like for like.

- **Not mapped:** dwords at 16 and above. The library refuses programs that read past its 16 root constants
  (`translate::layout_of`).
- **`table_offset_dwords` is not added**, as in the fork. A table range's offset is in the code's own `s_load`
  offsets.

**The shadow table** (`t0 space101`, plan decision 9) is the shader-visible view heap's: `HeapBinding::shadow_va`,
filled by `d3d12x` from `HeapState::shadow`. It holds 16 bytes per console index, `[stride, records, alternate,
lead]`. Indexed buffer access (`idxen`) reads the stride and the record count from it.

## Where it lives

```text
crates/pipelines/src/rootmap.rs     NativeProgram (console root layout) -> StageUserData / UserMap
crates/runtime/src/pipeline.rs      at creation (0x58/0x1f0/0x50/0x2b0): reads each program's layout once per
                                    hash from the bytes the title passes (graphics: the stream parser's
                                    program callback notes where each program lies), interns the UserMap
                                    ('static); host() hands it to the lists in HostPipeline::user
crates/xlist/src/console/mod.rs     UserSource, StageUserData, UserMap; HeapBinding::shadow_va;
                                    ListHost::sync_descriptors
crates/xlist/src/console/list.rs    the per-list state and the writes (below); sync_descriptors(list)
crates/d3d12x/src/{heap,host}.rs    binding() fills shadow_va; Host::sync_descriptors -> Resources::sync
crates/runtime/src/submit.rs        sync_descriptors(list) for each list before it is submitted
```

A pipeline whose programs' layout cannot be read gets **no** host pipeline from `host()`. Its draws are dropped
and counted (`ListStats::skipped`; the run report's "root layout unread"), never bound in the wrong layout.

## In the list: console state, written at the draw

Per side (graphics 0, compute 1), the list's `Shadow` keeps:

- `user[k]`: the bound translated pipeline's map, or `None`;
- `args[k][slot]` and `consts[k]`: the title's root arguments by **its** parameter numbers (it did this before);
- `title_root[k]`: the title's root signature (console object), which bounds its arguments' lifetime;
- `user_words[k][stage]` and `user_valid`: the words last written, so an unchanged stage is not written again;
- `shadow_va[k]`: the shadow table bound;
- `user_dirty[k]`: one flag the draw tests.

```text
title call                         user[k] = Some (translated pipeline)        user[k] = None
Set*Root{Table,View,Constants}     kept as console state; dirty if a stage    forwarded by the title's
                                   reads that slot (UserMap::slots); no host   numbers (as before)
                                   call
Set*RootSignature                  the host keeps the library root; a new      bound on the host (as before)
                                   title root clears the title's arguments
SetPipelineState                   map changes: dirty; the library root is set (a new host root signature
                                   loses what the host list held in the library layout: written again)
SetDescriptorHeaps                 dirty (table offsets and the shadow table follow the heaps)
cut at a memory wait               the fresh host list gets root signature + PSO by replay; the library
                                   layout is marked lost and written at the next draw
Draw*, Dispatch*, ExecuteIndirect  if dirty[k]: for each stage, build its words from the sources (no more
                                   than 16), compare with the last written, and if they differ issue one
                                   Set*Root32BitConstants(param, n, words); bind the shadow table if its
                                   address changed
```

No lock and no allocation: the map is `&'static`, the words are a stack array, and the dirty test is one byte.
`ListStats` counts `user_sets`, `shadow_sets` and `user_unset` (a dword whose argument the title never set, or a
table outside the bound heaps).

## Descriptors read through tables are synced before submission

The title copies descriptor records as plain bytes, and only create calls and `CopyDescriptors` re-mirror a slot.
When a table source is resolved, the list notes the bytes it spans: from the table's first descriptor up to the
furthest one the program reads statically (`UsedResource` offset plus descriptor size). Spans are kept as one
min/max per heap, at most four. Before `ExecuteCommandLists`, `submit.rs` calls `xlist::sync_descriptors(list)`.
That calls `ListHost::sync_descriptors(start, len)`, which `d3d12x` serves with `Resources::sync`: a slot whose
fingerprint is unchanged costs a read and a compare. Write-watch over the heap pages would replace this. Until
then, descriptors a program reads by a dynamic index beyond its static ranges are not covered.

## What this does not cover yet

1. **Resource type at the primary index (for the lead).** For buffer descriptors, d3d12x puts a **raw SRV** at
   host index i and the descriptor's own kind (CBV, typed SRV, UAV) at the shadow entry's alternate. The lifter,
   though, reads `ConstantBuffer<SirCb>` (for `s_buffer_load` through a CBV), `RWByteAddressBuffer` (stores) and
   `RWBuffer<float4>` at index i itself (`lift/res.rs:176, 262, 324`). That is a view of the wrong type: undefined
   on the host, and it would fail on any real title heap. Either the lifter indexes the alternate
   (`sir_shadow.Load(i * 16 + 8)`) for those three, or the mirror makes the primary the descriptor's own kind
   for UAV and CBV records. The WARP test places a raw UAV at the index the program reads, so it proves the seam
   and not this.
2. **`lead` is not applied (for the lead).** A raw view starts at the 16-byte-aligned address below the
   descriptor's first byte, and the shadow entry's `lead` says by how much. The lifted code adds no `lead`, so a
   buffer descriptor not 16-byte aligned reads `lead` bytes early.
3. **ExecuteIndirect** command signatures that change root constants or views are not mapped. Only the
   arguments set before the call are written.
4. **Graphics pipelines** use the same path (VS at parameter 0, PS at 1, the shadow table at 2). They are tested
   on the stand-in, not yet on a device.
5. UAV clears still map through `HeapBinding` to the raw primary, not `own_kind_gpu` (deepdive 4.4).
