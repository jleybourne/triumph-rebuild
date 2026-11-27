# Lane `shader` — Build 4 front end: the census and the decoder

Branch `agent/shader`. Crate `crates/shader` (package `rebuild-shader`), binary `census`
(`apps/rebuild/src/bin/census.rs`). Output: [`shader-census.csv`](shader-census.csv), one row per native
program, hashes and counts only.

```powershell
$env:REBUILD_GAME_DIR = '<game_dir from victory.toml>'
# optional cross-checks against the fork's own measurements (files outside git):
$env:REBUILD_FORK_CENSUS = 'D:\victory-work\census\census-full-wt-bisect-vn.tsv'
$env:REBUILD_FORK_WORLD  = 'D:\triumph\victory-work\tools\census_default.csv'
cargo run --release --bin census -- --threads 4      # 14 s; writes agents/shader-census.csv
```

## Built

- **S1 — the archive.** `shaders.rpf` is an SGD5 file: 26,503 deflate-packed records, each one DXBC
  container, 0 corrupt, 0 stored raw. Containers whose `DXIL` part is byte-identical count once (614
  duplicates, the fork's number; the first in file order is kept, so the run is deterministic). The XDAT
  index → XBCH zlib blob → FSSM records give **26,084 native programs: 25,694 VS+PS+CS+MS** (the
  fork's 25,694) plus 195 ES and 195 GS. Programs are named by the container's BLAKE3 hash (16 hex, as the
  fork's logs name them) and the `DXIL` part's hash (8 hex).
- **S2 — the census row** per program: hash, code hash, stage, wave64, code bytes, instructions, hints,
  re-encoded, VGPRs, SGPRs, LDS bytes, DPP / SDWA / LDS / GDS / scratch / 16-bit counts, blocks, branch
  kinds, EXEC regions, loops, structured, **wave class**, wave ops, wave ops in divergent regions.
- **S3 — the decoder.** The fork's instruction boundaries (control flow from offset 0, far jumps
  resolved) and Mesa-derived opcode tables are vendored. The field model and encoder are ours
  (`gfx10/enc.rs`): 23 layouts (19 encodings, VOP3A/B apart, SDWA, SDWA-B, DPP16, DPP8) in which every
  bit is tag, named field or reserved (a test checks the partition). Reserved bits are not kept and the
  trailing dwords (literal, SDWA/DPP word, MIMG NSA words) are derived from the fields, not copied, so a
  byte-exact re-encode proves the model names every bit the corpus sets and its length rules agree.
- 32 unit tests in `crates/shader` (31 of them the vendored readers' and the new decoder's, features and
  census on synthetic code).

## Measured (2026-11-27, `shaders.rpf` blake3 `2b023331518b1b78`, unchanged after the run)

```
programs      26,084 native · 25,694 VS+PS+CS+MS  (vs 2,191 · ps 12,056 · ms 8,765 · cs 2,682 · es 195 · gs 195)
decode        26,084 / 26,084 programs · 43,952,121 instructions (3,303,221 are hints: s_nop, s_waitcnt...)
re-encode     43,952,121 / 43,952,121 byte-exact (100 %) · every program exact
unknown       0 opcodes
wave64        23,430 (89.8 %)
features      DPP in 4,288 programs (66,159 instrs) · SDWA 14,367 (78,249) · LDS 7,148 (127,596) · GDS 0 ·
              scratch 0 · 16-bit 18,557 (879,840)
by layout     vop2 14.96M · vop3a 7.19M · vop1 5.14M · sopp 4.52M · sop2 3.07M · smem 2.64M · sop1 2.45M ·
              vopc 1.11M · mubuf 657k · vop3p 521k · sopc 468k · mimg 437k · vintrp 243k · sopk 193k ·
              exp 133k · ds 128k · sdwa 62k · mtbuf 51k · dpp16 40k · dpp8 26k · sdwab 17k · vop3b 16k · flat 10.5k
```

Against the fork (where comparable):

| | fork | here |
|:--|--:|--:|
| programs (VS+PS+CS+MS) | 25,694 | **25,694**, all matched by (code hash, stage) |
| containers / duplicates | 26,503 / 614 | 26,503 / 614 |
| wave64 per program | — | equal in 25,694 / 25,694 |
| console instructions (their IR, hints dropped) | 40,561,560 | 40,612,875 (+0.13 %); equal in 15,126 programs, the rest differ by a median of +4 (their IR also rewrites some instructions) |
| world frame | 1,433 hashes | **1,433 / 1,433 found** → 2,144,273 console instructions, 1,310 wave64 |
| "4.64M instructions" | 4,641,843 **DXIL** instructions over the 1,433 world programs | not comparable: emitted DXIL, not console code (see "for the lead") |

## S4 — the feature summary, for the IR design

**Headline: 99.18 % of programs are structured (reducible, no indirect jump, no call); 59.7 % of
conditional branches are per-lane (EXEC skips) and 40.3 % wave-uniform; 35 % of programs (9,128) never
look across lanes beyond a quad, another 4,631 only through `v_readfirstlane`.**

```
registers   VGPRs median 40 · p90 119 · p99 200 · max 244   (by stage, median/p99: vs 24/151 · ps 36/144 ·
                                                             ms 46/210 · cs 54/146 · es 18 · gs 25)
            SGPRs median 66 · p90 98 · p99 107 · max 107
branches    1,156,109 conditional: scc 270,873 (23.4 %) · vcc 194,592 (16.8 %) · exec 690,644 (59.7 %); s_branch 177,685
            wave-uniform (scc + vcc) 40.3 % · per-lane (exec) 59.7 %
            EXEC-narrowing regions (saveexec/wrexec) 376,273, in 17,464 programs
structure   25,869 / 26,084 structured (99.18 %): 20 irreducible (all compute) · 195 with an indirect jump (the 195 ES
            programs: the hand-over to the next merged stage) · 0 calls
            3,403 with no branch · 8,314 with a loop
            blocks per program median 35 · p90 206 · p99 882 · max 14,377
            instructions per program median 623 · p90 4,583 · p99 16,978 · max 88,862
wave class  agnostic 9,128 · wave-wide only via v_readfirstlane 4,631 · wave-wide 12,325
            first wave-wide op: v_mbcnt_lo (lane id) 9,329 · DPP row ops on v_min_u32 822 / v_or_b32 502 /
            v_add_nc_u32 32 / float min, add, max 16 · v_writelane 619 · v_readlane 510 · s_ff1_i32_b64 356 ·
            s_bcnt1 75 · ds_swizzle (bit-mask mode) 63 · ds_bpermute 1
wave ops    529,146 cross-lane ops in 19,328 programs; 445,906 inside EXEC-narrowed regions, in 15,357
            programs (these become predication)
```

Definitions (in `crates/shader/src/features.rs`):

- **Branch kinds:** `s_cbranch_scc*` uniform (scalar condition); `s_cbranch_vcc*` uniform test of a lane
  mask; `s_cbranch_exec*` per lane (skips a region no lane runs).
- **Structured:** the CFG is reducible (every retreating DFS edge targets a dominator), with no unresolved
  `s_setpc_b64` and no `s_call_b64`.
- **Wave class:** `classify.rs`'s rules applied per instruction, without a uniformity pass, so it is
  conservative: wave-wide on `v_readlane`/`v_writelane`, `v_mbcnt*` (the lane id; whether only its quad
  bits are used needs data flow), `v_permlane*`, `ds_permute`/`ds_bpermute`, `ds_swizzle` outside
  quad-permute mode, DPP16 beyond `quad_perm`, DPP8, and `s_bcnt*`/`s_ff*`/`s_flbit*` reading VCC or EXEC
  (or any 64-bit one in a wave64 program). `v_readfirstlane_b32` is its own class: agnostic when its
  operand is uniform, which the lead's uniformity pass decides. Quad ops are agnostic.
- **Wave ops in divergent regions:** counted in program order between an EXEC narrowing
  (`*saveexec`/`*wrexec` except `s_or_saveexec`, which is an else; `v_cmpx*`) and a SALU `or`/`mov` into
  EXEC. An approximation without the CFG; pixel and mesh programs that run most of their body under one
  narrowing (helper lanes, the NGG `lane < count` guard) make the share high.

## Decided

- The census counts all 26,084 native programs, and reports the 25,694 VS+PS+CS+MS beside it, because ES
  and GS programs are real code the title ships (merged-stage halves).
- Deduplicate by `DXIL` part, keep the first in file order; compare with the fork by (code hash, stage),
  because its kept container hash among duplicates depends on thread order.

## Next

- S5 (the oracle set), added by the lead after this lane started: see below once done.
- The lead's: GFX10 → `sir`. The field model gives every operand by name (`Decoded::get`, `enc::src`).

## For the lead

1. `secondary-agent.md` S2 and `tasks.md` Build 4 quote "4.64M instructions" as a census total to match.
   That is 4,641,843 **emitted DXIL** instructions over the **1,433 world-frame** programs (the fork's
   `census_default.csv`, column `total`), not console instructions of the corpus. The corpus has
   43,952,121 console instructions (40.6M without hints); the world set 2,144,273.
2. "25,694 programs" is the VS+PS+CS+MS count. The archive holds 26,084 native programs: 195 ES and 195 GS
   more (the fork's own summary says 26,084 translated). The census lists both numbers.
