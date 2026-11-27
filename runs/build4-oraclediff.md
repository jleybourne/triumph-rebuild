# Build 4 — our v0 DXIL against the oracle (S6 `oraclediff`)

Our 2,834 compiled v0 programs (pixel 1,791, vertex 926, compute 117), staged as `<hash>.dxil` outside git, against
the fork's translations (`REBUILD_ORACLE_DIR`):

`cargo run --release --bin oraclediff -- --ours D:\victory-data\translate-dxil --out <csv outside git>`

The v0 row closes when each program compiles and shows **0 interface diffs** (signatures, bindings, thread-group
size, wave size). Instruction-count categories (memory, wave, control, io) are reported but are not interface:
two independent translators emit different code.

| step | SigIn | SigOut | Bindings | note |
|:--|--:|--:|--:|:--|
| first run | 1,231 | 801 | 525 | our side read 0 `dx.op` calls: a reader bug |
| reader decodes Char6 names (DXC writes symbol names as 6-bit characters) | 1,231 | 801 | 525 | histograms now real; self-check still 0 diffs |
| input signatures mirror the console's: every element, in order | 135 | 801 | 525 | |
| output signatures in order, each element with its own mask | 135 | 3 | 525 | |
| named reason: our shadow table (space 101), immediate samplers (space 100) | 135 | 3 | **4** | |
| named reason: order of the GPU-generated vertex and instance ids | **35** | **3** | **4** | |

The named reasons are exactly two kinds, encoded in `crates/shader/src/oraclediff.rs` (`bindings_match`,
`inputs_match`): our own design decisions (plan decisions 9 and the immediate-sampler space), and the order of
system-generated values (bound by semantic, so neither the input layout nor linking depends on it). After them
the oracle against itself is still **0 diffs**, and every injected mutation is still caught (moved binding
25,060 / 25,060, dropped output element 22,551 / 22,551, removed wave op 18,896 / 18,896).

**Update (lane shader S7, PR #21):** all 39 programs classified. 4 oracle files were swapped in two pairs (the fork's
cache entries share creation timestamps; S5 paired them by time): corrected in `D:ictory-dataebuild-oracle`
(originals kept in `oracle-swapped-backup`), self-check still 0. Three more named reasons, each interface-neutral:
vertex inputs the oracle declares but never reads (22), `SV_Barycentrics` the oracle seeds and drops (12), the
fork's own descriptor-kind table at `t255, space1` (1). **Interface diffs now: SigIn 0, SigOut 1, Bindings 0** —
the one is a translator bug (a pixel shader with no outputs gets a dummy `SV_Target0`), sent to lane shader-cf with
a second bug S7 found by reading the diffs (packed fp16 inputs read the wrong components in all 62 v0 programs
that use 16-bit interpolation; the new non-interface check `InputReads` measures it).

**Left (at most 42 programs, before S7):** 35 vertex inputs where the oracle declares an attribute our fetch does not load
(for example `COLOR0`), 3 outputs, 4 bindings. Next: read each, fix ours or name the reason.

Tools: `oraclediff --calls <file>` (every call and its resolved `dx.op`), `oraclediff --sig <ours> <oracle>` (both
sides' signatures and bindings).
