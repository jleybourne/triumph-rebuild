# Depth buffers written as UAVs

The console lets one texture be a depth-stencil target and an unordered-access (UAV) resource. PC D3D12 refuses
`ALLOW_DEPTH_STENCIL | ALLOW_UNORDERED_ACCESS` on one resource. Until now `format::host_desc` stripped the UAV flag
and `heap.rs` `create_uav` wrote every such UAV as a null view, so every write the title made into a depth
texture through a UAV was lost.

## Measured (card, master + counters, 75 s run to the start screen, 902 frames)

| what | count |
|:--|:--|
| depth textures created with depth + UAV (console flags `0x80300006`) | ~310, all at load |
| their formats (console) | R16 typeless (D16) almost all; one D32S8 2560x1440; one console D16S8 (118) 2560x1440 |
| sizes | 128-2048 square, arrays of 1, 3 and 6 (shadow maps: spot, cascades, cubes); pools of 76 and 24 |
| initial layout | **UNORDERED_ACCESS** (3) for every one |
| UAV views made of them | 316: R16_UNORM (plane 0) on the D16 ones; R32_FLOAT plane 0 on the D32S8; R16_UNORM plane 0 + R8_UINT plane 1 on the D16S8 |
| enhanced barriers naming any depth texture in the run | 2 (the 4K D32S8, never through UAV) |
| legacy barriers naming one | 0 |

So the views are made at load and the menus never take a depth texture through UAV: the compute writes happen in
gameplay (shadow-map work: the pool sizes and arrays of 3 and 6 are cascade and cube shadow maps), which no run
reaches yet. The title uses enhanced barriers only. Every one of these textures starts in the UAV layout, so its
first barrier is "out of UAV", and whatever compute wrote before it must survive.

## Candidates

1. **Aliasing** (a UAV-capable texture placed over the depth texture's memory). Rejected: the host gives depth
   textures an opaque layout (`D3D12_TEXTURE_LAYOUT_UNKNOWN`; depth cannot be row-major or standard swizzle), the
   Enhanced Barriers spec "does not define compatible layouts between aliased resources", and placed-resource
   rules leave an aliased texture's contents undefined until it is fully written. Reading depth through a colour
   alias would be undefined behaviour that might happen to work on one driver.
2. **Translator rewrite of the shaders** (compute writes to a buffer, a later pass converts). Rejected: every
   writer program would need its own path, and the depth would still need a copy in.
3. **Twin + copies at the title's barriers (chosen).** Each depth texture with UAVs gets a twin: a colour texture
   of the depth plane's typeless format with `ALLOW_UNORDERED_ACCESS`. The title's UAVs become views of the twin.
   The title's barriers say exactly when a texture enters and leaves the UAV layout, so the list translator
   copies depth into the twin on the way in and the twin back into the depth on the way out. Copies between the
   two are legal: `CopyTextureRegion` takes formats of one type group, whole subresources for depth
   (learn.microsoft.com, `CopyTextureRegion` remarks), and D16 / D32 are R16 / R32 typeless on the host, the same
   as their twins. D32S8's depth plane (plane 0) copies to an R32 twin per subresource.

## Design

```text
console layout     host depth           twin
not UAV            the title's layout   UNORDERED_ACCESS, idle
UAV (3, 20, 26)    COPY_SOURCE (parked) UNORDERED_ACCESS, the title's UAVs write it

enter (x -> UAV)   depth x -> COPY_SOURCE; twin UAV -> COPY_DEST; copy depth -> twin; twin -> UAV
                   (no copy when x is UNDEFINED, the barrier discards, or this list left UAV with no work since)
exit  (UAV -> y)   twin UAV -> COPY_SOURCE, depth COPY_SOURCE -> COPY_DEST; copy twin -> depth; depth -> y,
                   twin -> UAV (no copy when this list entered the phase itself and recorded no work since)
UAV -> UAV         a barrier on the twin alone
```

- **Resource side** (`crates/d3d12x`): `format::twin_desc` (R16 for D16, R32 for D32 and D32S8; none for the
  console's D16S8, which the host keeps as 24/8). `make_resource` makes the twin with the resource, resting in
  `UNORDERED_ACCESS`; a depth texture the title starts in UAV starts parked in `COPY_SOURCE` (`initial_layout`).
  `create_uav` makes plane-0 UAVs on the twin (stencil-plane UAVs stay null views). `Resources::uav_twin` answers
  the list for textures whose UAV the host stripped.
- **List side** (`crates/xlist/src/console/twin.rs`, a few lines in `list.rs`): `ListHost::uav_twin` (default
  `None`), `twin::plan` turns one title texture barrier into barriers in its group, copies after the group and
  barriers after the copies. UAV clears name the twin (`uav_clear`). Each list remembers, per twin, the last phase
  edge it recorded and its work count then: an exit with no work since its enter copies nothing (the twin was not
  written), and an enter with no work since its exit copies nothing (depth and twin are still equal). An edge whose
  other half is in another list always copies: lists are recorded in parallel, so nothing is assumed across them.
- **Sync.** Twin barriers wait on `SYNC_ALL` where the twin's last user is not known at record time (lists are
  recorded in parallel); copies use `SYNC_COPY`; the title's own `SyncAfter` / `AccessAfter` go on the barrier that
  ends the sequence, so later work waits on the copy. The debug layer requires a `NO_ACCESS` side of a layout
  change to have `SYNC_NONE`: the parked depth is always "read by a copy" and the resting twin "in unordered
  access".
- **Cost.** Nothing for barriers that do not touch UAV (one flag check). A UAV phase costs two copies of the
  texture (a 2048x2048 D16 is 8 MiB each way: microseconds on the card); a phase with no work costs none. Memory:
  one twin per depth texture with UAVs, made with it (~250 MiB of VRAM for the title's shadow-map pools at load).
  Made lazily at the first UAV view instead it would save nothing here (every such texture gets views at load) and
  would let a phase start before its twin exists.

## After (card, same start-screen runs)

| | null UAV views | twins | copies | avg fps | upload pass |
|:--|:--|:--|:--|:--|:--|
| before (master) | 316 | 0 | 0 | 12.4 | |
| `REBUILD_DEPTH_TWINS=0` (this branch, twins off) | 316 | 0 | 0 | 12.1 | 19.7 ms |
| this branch | **2** (the D16S8's) | 314 | 0 (no UAV phase in the menus) | 14.0 | 14.6 ms |

The process's dedicated GPU memory with twins was 6.1 GiB of the card's 12. Two earlier runs of this branch at
3 fps had 250 ms upload passes (watch, diff and gather all ~30x slower: CPU memory work) while other agents' runs and
builds shared the machine; the A/B pair above ran back to back and shows no cost. `REBUILD_DEPTH_TWINS=0` turns
the twins off for such comparisons.

## Not covered

- The console's D16S8 (format 118, one 2560x1440 target): the host has no 16-bit depth with stencil, so its two
  UAVs (R16 depth plane, R8 stencil plane) stay null views. Stencil-plane UAVs of D32S8 likewise.
- Legacy `ResourceBarrier` transitions through `UNORDERED_ACCESS` on depth (the title makes none).
- A depth texture entered into UAV on one queue and left on another works (each edge is self-contained), but a
  phase entered in one list and left in another always copies back.

## Tests

- `twin::tests` (xlist, no GPU): each edge's barriers, copies, and the plane / subresource mapping.
- `format::tests::depth_textures_written_as_uavs_get_a_colour_twin`, `resource::initial_tests`.
- `twin_tests::a_uav_write_into_a_depth_texture_reaches_the_depth_on_warp` (d3d12x, WARP): D32, D16 and D32S8 made
  as the title makes them, a console list that clears the depth, enters UAV, clears half through the UAV, leaves,
  and enters and leaves again with no work. The host depth holds both values, with three copies (none for the
  empty phase). `REBUILD_D3D12_DEBUG=1` runs it under the debug layer and fails on any error.
