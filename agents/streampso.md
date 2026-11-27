# Lane `streampso`: graphics pipelines from the title (device 0x2b0 and 0x50)

Status: **built, tests green, PR open; not run against the title** (the lead runs `runtitle`).

## Built

- `crates/pipelines/src/stream.rs` (pure data, no Windows needed except its header-size test):
  - `parse_stream(stream, mem, Numbering::Console | Pc, console_sizes) -> Result<Parsed, Refusal>`: a pipeline
    state stream as a `Desc` (Graphics, or Compute when it holds a CS).
  - `parse_graphics(desc, mem)`: a `D3D12_GRAPHICS_PIPELINE_STATE_DESC` (656 bytes) through the same reader.
  - `program_hash` moved here (runtime's `pipeline::program_hash` calls it): BLAKE3 of the DXBC container at
    its declared size, first 8 bytes big-endian.
  - Pointers are read through a `Memory` trait (`Process` for the title's own memory; a fake in tests), so
    VS/PS are hashed only after the stream is accepted, and no allocation happens beyond the `Desc`.
- `crates/runtime/src/pipeline.rs`: `CreateGraphicsPipelineState` (0x50) and `CreatePipelineState` (0x2b0),
  registered in `device.rs` (`WRITTEN`). Both return at once through `Pool::request`. A pipeline whose
  programs are not in the library, a mesh pipeline, or a refused stream still gets a valid object with no host
  pipeline. `host()` takes the root signature from `Programs::graphics(vs, ps)` for graphics keys
  (`compute: false`). The report line now counts graphics, mesh, refusals by reason, and skipped console
  subobjects.
- `pipeline.rs` gets `#![allow(non_snake_case)]` like the other slot modules (master had 2 warnings from the
  compute slots' names; now 0).

## Subobject table

Size is the whole subobject: type, padding and payload, 8-byte aligned. The payload is at +4, or at +8 when
it holds a pointer.

| type | PC number | console number | payload at | size |
|:--|:--:|:--:|:--:|:--:|
| ROOT_SIGNATURE (read, ignored) | 0 | 0 | +8 | 16 |
| VS / PS / DS / HS / GS / CS | 1-6 | 1-6 | +8 | 24 |
| STREAM_OUTPUT (refused when it has entries) | 7 | 7 | +8 | 40 |
| BLEND | 8 | 8 | +4 | 336 |
| SAMPLE_MASK | 9 | 9 | +4 | 8 |
| RASTERIZER | 10 | 10 | +4 | 48 |
| DEPTH_STENCIL | 11 | 11 | +4 | 56 |
| INPUT_LAYOUT | 12 | 12 | +8 | 24 |
| IB_STRIP_CUT_VALUE | 13 | 13 | +4 | 8 |
| PRIMITIVE_TOPOLOGY | 14 | 14 | +4 | 8 |
| RENDER_TARGET_FORMATS | 15 | 15 | +4 | 40 |
| DEPTH_STENCIL_FORMAT | 16 | 16 | +4 | 8 |
| SAMPLE_DESC | 17 | 17 | +4 | 16 |
| NODE_MASK (ignored) | 18 | 18 | +4 | 8 |
| CACHED_PSO (ignored) | 19 | 19 | +8 | 24 |
| FLAGS (in `Parsed.flags`, not in the host pipeline) | 20 | 20 | +4 | 8 |
| DEPTH_STENCIL1 (depth bounds dropped) | 21 | 21 | +4 | 64 |
| VIEW_INSTANCING (refused when count > 0) | 22 | 22 | +8 | 32 |
| AS (no host pipeline, counted as mesh) | 24 | **23** | +8 | 24 |
| MS (no host pipeline, counted as mesh) | 25 | **24** | +8 | 24 |
| DEPTH_STENCIL2 (front face's masks kept) | 26 | console type | +4 | 64 |
| RASTERIZER1 (float bias) | 27 | console type | +4 | 48 |
| RASTERIZER2 (line mode) | 28 | console type | +4 | 48 |
| SERIALIZED_ROOT_SIGNATURE (ignored) | 29 | console type | +8 | 24 |
| console types | n/a | 25+ | ? | **unknown**: skipped if `REBUILD_STREAM_SUBOBJECTS` gives a size, otherwise the parse stops and refuses |

Checked: payload sizes and every `D3D12_GRAPHICS_PIPELINE_STATE_DESC` offset against the windows-rs headers
(test); the offsets of the title's 0x2b0-byte template (fork test, title+0x86f090) come out exactly from these
sizes (test).

Choices: if the stream leaves a subobject out, it gets D3D12's stream default (`CD3DX12_*(D3D12_DEFAULT)`: back
faces culled, depth on with LESS). The topology type stays at triangles. Canonical forms, so that equal
pipelines share a key: when blending is not independent, every target's blend is the first target's; with no
depth format, there is no depth or stencil state. A duplicated subobject: the last one wins.

## Measured

```
cargo test --release --no-fail-fast      458 passed, 0 failed, 0 warnings (master: 445)
stream parser tests                      9 (PC stream, defaults, DS/DS2/R1/R2, console AS/MS, console types
                                         skipped / refused, truncation at every prefix length, compute and bad
                                         programs, 0x50 = stream, header sizes, the title's template)
GPU (library.bin, MEASURING absent)      stream -> Desc -> PSO: vs 03b727aa345d7631 + ps 00679368dcb02f2b,
                                         4 inputs, 1 target: created
runtime                                  2 tests: 0x2b0 graphics / mesh / unknown console type / compute /
                                         null stream; 0x50 gives the same key as the equivalent stream
```

## Not confirmed without a real stream (for the lead)

1. **Console types 25 and above**: their meaning and size. Any one in a real stream stops the parse (the
   report names it, "subobject of unknown size", and the first line logs the type and offset). For each new
   type, capture **the whole stream** (`size` bytes from `*{size, *stream}`), plus the 32 bytes after each
   subobject of type 25 or above. Sizes can be tried without a rebuild: `REBUILD_STREAM_SUBOBJECTS=25:16,...`.
2. **The first shader slot in the title's template**: the fork's test puts a type 24 (MS) there. If the 6
   streams really hold MS, they all count as mesh and get no host pipeline. Capture: the type word at stream
   +0x10, and whether the program the bytecode at +0x18 points at is a mesh or a vertex program (its stage in the census).
3. Whether the console fills in a left-out subobject with the PC defaults (cull back, depth on). Only matters
   when a stream leaves out RASTERIZER or DEPTH_STENCIL; the template has both.
4. The flags word (20): passed on in `Parsed.flags`, but nothing uses it.
5. The input layout against the translated VS: if a pipeline fails at `wait`, compare the stream's
   input elements (address at stream +0x260+8, count at +0x260+16) with the DXIL `ISG1`.

## Next

- With a capture, build the known console sizes into `pipeline::console_sizes()` (or read them as state when a type affects it).
- Mesh pipelines: a host path (MS/AS programs in the library), once the census shows how many the title uses.
