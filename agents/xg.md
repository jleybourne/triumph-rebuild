# Lane `xg` — the console texture layout (`xg_xs.dll`; Build 5 piece)

Crate `crates/xg` (package `rebuild-xg`). No GPU, no global state, no title code, no game bytes.
State: **X1, X2, X3, X4 closed.** 15 tests green (`cargo test --release -p rebuild-xg`), plus one hand-run
measurement (`x4_detile_throughput`, ignored by default). Workspace: 135 passed, 2 ignored.

## Built

- `layout.rs` (vendored, section 3 header, `CREDITS.md`): GFX10 layouts after AMD's addrlib, from the
  fork's `victory-formats/src/texture.rs`: block size per mode and element size, mips smallest first with
  the tail block at offset 0, tail positions, the RB+ micro tile patterns, `Layout::offset`, the DXGI element
  table, depth and stencil as two planes. Its copies are dropped; ours are in `detile.rs`.
- `address.rs` (ours, X2): `Layout::texel_at(offset) -> Option<Texel { mip, x, y, z, byte }>`, the inverse.
  Inside a block every addressable mode is a permutation of address bits, so the inverse reads the
  pattern backwards; the block index splits by the mip's pitch; tail mips are found by their element
  position in the tail block. Padding answers `None`.
- `computer.rs` (X3): `XGSuggestSwizzleMode` and `XGCreateTextureComputer` with the console's structures
  (`ResourceDesc` 0x38, `LayoutOut` 0x1b40, `PlaneOut` 0x6c8, `MipOut` 0x70, `PackedMips`,
  `SubresourceTiling`), size-asserted at compile time, and the six methods the title calls as plain Rust
  (`layout`, `element_offset`, `copy_into`, `copy_from`, `tiling`, plus Release). `SLOTS` names all 13 vtable
  slots. No `extern "system"` and no vtable: the lead binds them in `crates/runtime`.
- `detile.rs` (ours, X4): `Layout::tables(mip, slice)` gives the per-axis address tables of a subresource
  (`rows[z*h + y] + columns[x]`, the tables a GPU detile kernel would read), and `Layout::detile` / `tile`
  fold the column table into contiguous runs once and copy every row from its own base, with fixed-size
  moves when the runs are 4-64 bytes.

## X1 — the layouts the title uses

Sources: the fork's `xg.rs` (the title's calls, its tests and their notes), `docs/design/gpu-textures.md`,
`victory-gpu/src/texture.rs`, and the resource descriptions printed in 13 of the fork's run logs under
`D:\victory-work\logs` (5,961 lines; only resources some warning named, so the mix is biased to depth and
UAV textures). Counts and names only.

| how the title gets a layout | mode (number) | what for | evidence |
|:--|:--|:--|:--|
| `layout` 0 ("library picks"), sampled 2D/3D | `SW_4KB_S` (5) | textures, `.ttpf` textures without a mode of their own | fork's suggestion rule; 0x105 in its tests; flags `0x300000` descriptions with `layout` 0 (BC1/BC3/BC7 chains, 61+25+22+11 lines) |
| `layout` 0, `ALLOW_RENDER_TARGET` | `SW_64KB_R_X` (27) | render targets | rule; host renders them |
| `layout` 0, `ALLOW_DEPTH_STENCIL` | `SW_64KB_Z_X` (24) | depth, shadow maps (R16 typeless arrays with UAV: 4,700+ lines) | rule; host renders them |
| `layout` 0, 1D | `LINEAR` (0) | 1D textures | rule (fork test `18c86a93`) |
| `layout` 0x100 | `LINEAR` (0) | small CPU-written textures (16x4 / 16x8, format 10, flags `0x300000`: the texture of the fork's runs 72/73) | fork texture test; 2 log lines (format 2) |
| `layout` 0x10a, from streamed data | `SW_64KB_D` (10) | BCPack output from DirectStorage arrives in the console's tiled layout (BC7 256x256 chain in the fork's test) | `gpu-textures.md`; `xg.rs` test |
| `layout` 1 | row major | buffers (format 0) | 16 log lines |
| method 0x30 on a 128x4 texture | `SW_4KB_S` (5) | the title reads texels on the CPU through `GetTexelElementOffsetBytes` (format 11, title+0x5151c90 in the fork's notes) | `xg.rs` |

Modes the CPU path must cover (and does): `LINEAR`, `256B_S/D` (1, 2), `4KB_S/D` (5, 6), `64KB_S/D` (9, 10),
thin and thick 3D, mip tails, arrays. The XOR modes (`_T` 17/18, `_X` 21-27) are refused with
`Unsupported`: their addresses depend on the console's pipe and bank configuration, which neither the fork
nor this crate has. The title uses them for render targets and depth, which the host renders, so no CPU
copy needs them so far.

Formats seen (DXGI numbers): block compressed BC1 (71), BC3 (77), BC3 sRGB (78), BC7 (98), BC7 sRGB (99);
R8G8B8A8 (28), R16 typeless (53), R32 typeless (39), R32G8X24 typeless (19, depth + stencil), 118 (Xbox
depth 16 + stencil 8, two planes), R32G32B32A32 float (2), R16G16B16A16 (10, 11), B4G4R4A4 (115). Element
sizes 1, 2, 4, 8, 16 bytes and 4x4 blocks cover all of them; the X4 cases use one format per size.

## X3 — the computer's interface as the title calls it

Imports: `XGCreateTextureComputer(desc*, out**) -> HRESULT`, `XGSuggestSwizzleMode(desc*, flags, out*) ->
HRESULT` (flags 0; the result goes in `desc.layout`).

| slot | method | title calls | ours |
|:--:|:--|:--:|:--|
| 0x00 | AddRef | – | (lead: refcount) |
| 0x08 | Release | ✅ | (lead: refcount, drop) |
| 0x10 | GetResourceLayout `(LayoutOut*)` | ✅ | `Computer::layout` |
| 0x18 | GetResourceSizeBytes | – | `E_NOTIMPL` |
| 0x20 | GetResourceBaseAlignmentBytes | – | `E_NOTIMPL` |
| 0x28 | GetMipLevelOffsetBytes | – | `E_NOTIMPL` |
| 0x30 | GetTexelElementOffsetBytes `(plane, mip, x: u64, y, z, sample, bit*) -> u64` | ✅ | `Computer::element_offset` (0 outside, never an error code) |
| 0x38 | GetTexelCoordinate | – | `E_NOTIMPL` (`Layout::texel_at` is ready if the title ever calls it) |
| 0x40 | CopyIntoSubresource `(tiled*, plane, subresource, linear*, row, depth)` | ✅ | `Computer::copy_into` |
| 0x48 | CopyFromSubresource `(linear*, plane, subresource, sample, tiled*, row, depth)` | ✅ | `Computer::copy_from` |
| 0x50 | GetResourceTiling `(tiles*, PackedMips*, shape*, count* in/out, first, SubresourceTiling*)` | ✅ | `Computer::tiling` |
| 0x58 | GetTextureViewDescriptor | – | `E_NOTIMPL` |
| 0x60 | (tiled resource query) | – | `E_NOTIMPL` |

Subresource index = `mip + slice * mips`. The slots the title does not call are named from the library's
public shape (the fork's comments), not from a run.

## Measured

```
tests                 15 green: layout 5, address 2, computer 5, detile 3   (+1 ignored measurement)
X2 round trip         every element of 170 small layouts (7 modes x (5 element sizes x 4 shapes, 2 BC, 1D),
                      9 thick 3D): offset -> texel_at exact for every byte; every byte of every texture is
                      exactly one element's byte or padding (> 100,000 elements, both directions)
X4 vs the fork        174 cases (8 formats x 7 modes x 3 shapes incl. arrays and full chains, + 6 3D),
                      2,114 subresources, 43,446,949 bytes compared with the fork's Layout::detile on the
                      same synthetic tiled bytes: 0 differ. The fork's FNV-1a per case is committed
                      (fork_hashes.in) and checked by x4_detile_matches_the_fork_on_synthetic_data.
X4 throughput         2048x2048 full chains, one thread (test x4_detile_throughput; harness beside the fork):
                        BC7 64KB_D   0.49-0.54 ms a chain, 10.4-11.5 GB/s   (fork 0.47 ms, 12.0 GB/s)
                        BC7 4KB_S    0.56-0.83 ms, 6.7-10.1 GB/s            (fork 0.63 ms, 8.9 GB/s)
                        RGBA8 4KB_S  3.4-3.5 ms, 6.4-6.6 GB/s               (fork 3.9 ms, 5.7 GB/s)
                        R8 64KB_S    0.92-1.56 ms, 3.6-6.1 GB/s             (fork 1.04 ms, 5.4 GB/s)
                      Same speed as the fork within run-to-run noise.
```

The fork harness lives outside git (it includes the fork's `texture.rs` verbatim, from
`git archive triumph/main crates/victory-formats`): it builds both layouts for each case, fills the tiled
bytes from the same xorshift seed (the case index), detiles every subresource with both, compares, and
writes the fork's hashes. Only synthetic bytes; nothing from the game.

## Decided

- Detile on the CPU through per-axis tables, the same tables a GPU kernel would read (`Layout::tables`),
  so Build 5 can move the detile to the card without new maths.
- XOR modes refused, never guessed (no pipe configuration).
- No `extern "system"` shims here: the lead owns binding (`crates/runtime`).

## For the lead

1. **Binding (`crates/runtime`):** wrap `Computer` in a 13-slot vtable in `SLOTS` order. Slot 0x30 must return
   0 for a texel outside the texture: the fork saw an error code returned as an offset send the title into
   wild memory. `copy_into`/`copy_from` take `tiled` from the texture's start: build the slices with
   `Computer::copy_extent(plane, subresource, row, depth)` (tiled bytes from the start, linear bytes).
2. **Build 5 detile on the card:** `Layout::tables(mip, slice)` returns the column and row tables of a
   subresource; `offset = rows[z*h + y] + columns[x]`. The fork's `gpu-textures.md` item 1 estimates 2-4 ms
   of a 4-6 ms texture refresh moves off the CPU this way.
3. **`XGSuggestSwizzleMode` is a shortcut** (the fork's): the older library's rule. If streamed textures come
   out scrambled, check this first.
4. **Compression metadata planes** (HTILE, CMASK, FMASK, DCC) are not sized, as in the fork: console sizes of
   render targets and depth are smaller here. Matters only if the title sizes a pool from them.

## Next

Nothing in this lane. XOR addressing only if a title copies into or out of a render target or depth
texture on the CPU (it would need the Series X pipe configuration from a run).
