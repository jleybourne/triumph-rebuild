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

## S5 — the oracle set

```powershell
# 1. the fork's own offline translator, the build that made its 25,694 / 25,694 census
#    (wt-bisect target\vn\diag\victory.exe, sha256 b744f584...8c1b), one thread, a fresh cache:
$env:VICTORY_CACHE_DIR = 'D:\victory-data\rebuild-oracle-work\cache'
victory.exe translate --warm all --jobs 1 <game>\shaders.rpf          # 1,180 s
# 2. pair the entries with hashes, write the oracle and its index
$env:REBUILD_ORACLE_DIR = 'D:\victory-data\rebuild-oracle'; $env:REBUILD_FORK_EXE = '<that victory.exe>'
cargo run --release --bin oracle -- --cache D:\victory-data\rebuild-oracle-work\cache --verify 50
```

**Built:** `apps/rebuild/src/bin/oracle.rs`. The fork's shader cache (`cache-fx`) cannot be used: its key is
BLAKE3 of the converter id, a 32-byte fingerprint of the fork's build and the container, so an entry
cannot be named from a hash without that build. So the fork's own `--warm` translates every container
one after another in file order into an empty cache, and `oracle` pairs the entries, by creation time,
with the containers it expects in the same order. The pairing is accepted only if the counts are equal
and every entry's stage is its container's.

**Measured:**

```
warm run      25,889 containers: 25,694 translated, 195 refused (the ES+GS-only containers) · 1,180 s, 1 thread
pairing       25,694 entries for 25,694 expected · 0 stage disagreements · accepted
spot check    50 of 50 programs re-translated alone, each into its own cache: byte-identical
oracle        25,694 present · 0 missing · 854,844,264 bytes of DXIL (1.8 GB with the entries)
index         agents/shader-oracle.csv: hash, stage, present, dxil_bytes, entry_bytes
read-only     shaders.rpf unchanged
```

Files, outside git: `D:\victory-data\rebuild-oracle\<hash>.dxil` (the DXIL the fork emits) and `<hash>.vsh`
(the whole cache entry: DXIL plus the fork's root bindings, user-data, samplers; format `VSH1` in the
fork's `victory-gpu/src/shader_cache.rs`). The translation is the host-agnostic one (`--warm` makes no
device; the fork's runs on a 32-lane card may differ where `VICTORY_HOST_WAVE`/`VICTORY_VIEW_LIMIT` apply).

A first warm with the fork's newer `target\release\victory.exe` gave 25,636 (its 58 known refusals), so it
was not used.

## S6 — `oraclediff`, the oracle diff tool (round 2)

```powershell
$env:REBUILD_ORACLE_DIR = 'D:\victory-data\rebuild-oracle'
cargo run --release --bin oraclediff -- --ours <dir of <hash>.dxil> --out <csv outside git>
cargo run --release --bin oraclediff -- --self --mutate --out D:\victory-data\rebuild-oraclediff\self.csv
cargo run --release --bin oraclediff -- --self --out NUL --audit 50     # checks the bitcode reader
```

**Built:** `crates/shader/src/bitcode.rs`, our own LLVM 3.7 bitstream reader (abbreviations, BLOCKINFO, types,
value numbering with relative ids, constants, the value symbol table), enough to list each function's
instructions and resolve every call to `dx.op.*` to its opcode. `crates/shader/src/oraclediff.rs` reads both
containers and compares:

```
signatures   ISG1 / OSG1 / PSG1 elements, as sorted lists: name, index, system value, component type,
             register, mask, stream, min precision
bindings     PSV0 resources, sorted: type, space, lower and upper bound, kind, flags
threads      PSV0 thread-group size        wave size   PSV0 minimum and maximum wave lane count
stage        PSV0 shader stage
histogram    memory (cbuffer, sample, texture and buffer ops, atomics, LOD, raw buffers; LLVM load,
             store, atomics) · wave (wave and quad ops) · control (br, switch, ret, unreachable,
             discard) · barrier · I/O (inputs, outputs, eval, thread ids, mesh outputs)
```

One CSV row per oracle hash: which parts differ, and the histogram as ours/oracle. No GPU.

**Measured** (the oracle against itself, 25,694 programs, 17.8 s on 4 threads):

```
diffs         0 programs differ (0 unreadable)
mutations     a binding moved up one register          caught in 25,060 of 25,060 programs with a binding
              the last output-signature element dropped caught in 22,551 of 22,551 programs with outputs
              the first wave op removed                 caught in 18,896 of 18,896 programs with a wave op
reader audit  every 50th program (514): 297,657 dx.op calls through 44 functions, 0 unresolved; every
              function name gives exactly its DXIL opcode (waveReadLaneFirst 118, rawBufferLoad 139,
              cbufferLoadLegacy 59, ...) except the unary/binary/tertiary families, which carry several
```

The first two mutations are made in the container's bytes and read back. The third is applied to the
decoded instruction list, because the bitcode has no fixed-size slot to blank. Tests: 4 more (bitcode
reader on a hand-built module; signatures, bindings, threads, wave size and the three mutations on a
hand-built container). Workspace: 242 green.

## S7 — the last v0 interface diffs, each one read (round 2)

```powershell
$env:REBUILD_ORACLE_DIR = 'D:\victory-data\rebuild-oracle'; $env:REBUILD_SHADERS = 'D:\victory-data\shaders.rpf'
cargo run --release --bin oraclediff -- --ours D:\victory-data\translate-dxil --out <csv outside git>
cargo run --release --bin oraclediff -- --sig <ours>\<hash>.dxil <oracle>\<hash>.dxil   # "=" both, "<" ours, ">" oracle
cargo run --release --bin oraclediff -- --console <hash>                            # the console program (printed only)
$env:REBUILD_FORK_EXE = '<the S5 victory.exe>'
cargo run --release --bin oraclediff -- --ties D:\victory-data\rebuild-oracle-work\cache --fix <dir>
```

**Built:** `--sig` prints a merged list per side instead of two dumps; `--console <hash>` prints the console
program's stage IO, its container's signatures and every instruction's fields; `--ties` finds the S5 pairing's
cache entries created in the same clock tick, settles each group by signature agreement with the containers
(`sig_agreement`, `best_assignment`) or, when signatures cannot tell, by re-translating each container alone
with the fork; `--fix` writes the right entries. Three named reasons added (`inputs_match`, `bindings_match`,
each with the reason in a comment), and one new non-interface check, `InputReads` (`reads_missing`): pixel
input components the oracle reads and ours does not (from the signature's read mask, which DXC fills). Tests:
5 more in `crates/shader` (42 there, all green).

**Measured** (our 2,834 v0 programs from `D:\victory-data\translate-dxil`, 2026-11-27):

| run | SigIn | SigOut | Bindings | InputReads | programs with an interface diff |
|:--|--:|--:|--:|--:|--:|
| start (the lead's numbers) | 35 | 3 | 4 | — | 39 |
| + three named reasons | 1 | 3 | 3 | 62 | 4 |
| + the oracle's 4 swapped files corrected (in `D:\victory-data\s7\oracle`, hard links + the 4 fixed) | **0** | **1** | **0** | 62 | **1** |
| the oracle against itself, `--self --mutate` | 0 | 0 | 0 | 0 | 0; mutations caught 25,060 / 25,060, 22,551 / 22,551, 18,896 / 18,896 |

**Counts per class** (39 programs, 42 diffs): **(a) tool 3** (6 diffs) · **(b) design 35** (35 diffs) · **(c)
translator 1** (1 diff). And one translator bug the interface columns cannot see, found while reading them: 62
pixel programs (InputReads).

### (a) Tool: the oracle has four files the wrong way round

S5 paired the fork's cache entries with containers by creation time. **8 groups of 2 entries share a creation
time** (NTFS tick); they then sort by path, a coin toss, and a same-stage swap passes the stage check. `--ties`
with the S5 build (`wt-bisect ...\vn\diag\victory.exe`, sha256 `b744f584...8c1b`) re-translating each container
alone: 2 groups are swapped, 6 are in order (4 of them hold byte-identical DXIL, so their order cannot matter).

| hash | stage | diffs before | ours against the right entry |
|:--|:--|:--|:--|
| `2d539d4d776445e4` | vs | SigOut (TEXCOORD0 xyz/xy), Bindings | 0 diffs, histogram equal (its oracle file is `fbf743aed16d2bcd`'s) |
| `fbf743aed16d2bcd` | vs | SigOut (TEXCOORD0 xy/xyz), Bindings | 0 diffs, histogram equal |
| `2b9c4c58786e197b` | vs | SigIn (SV_InstanceID), Bindings (b0, t3 space1), memory 0/24 | 0 interface diffs (its oracle file is `727b2ac8d96d14ab`'s) |
| `727b2ac8d96d14ab` | vs | not in the v0 set | — |

The four right entries are in `D:\victory-data\s7\oracle-fix\<hash>.dxil|.vsh` (not copied over the lead's
oracle: that folder is read-only to this lane). See "for the lead" 3-4.

### (b) Design differences, each now a named reason

| reason | programs | what differs | why it is not interface |
|:--|--:|:--|:--|
| vertex inputs the oracle declares but never reads | 22 vs | the oracle mirrors every container element; ours declares what the fetch loads (`VsInput`), so the rest move up a register | the input assembler matches by semantic; a VS may declare fewer elements than the layout; VS input registers are not compared. Only elements with read mask 0 in the oracle **and** absent from ours are excused |
| `SV_Barycentrics` | 12 ps | the oracle declares it (register -1) | the fork seeds the console's barycentric VGPRs from it so a 16-bit interpolation's untouched high half keeps the console's bits; in all 12 the oracle reads `.y`, keeps bits 16-31 (`and -65536`), merges the fp16 result into bits 0-15, and the only use is `legacyF16ToF32` of the low half: the barycentric bits are dead. Rasteriser-generated, no register, never linked |
| the oracle's descriptor-kind table `t255, space1` (raw) | 1 ps | the oracle reads one word per heap descriptor to choose a raw-buffer or constant-buffer load for a descriptor index read at run time | the translator's own helper, bound by the host layer, like our shadow table (space 101). The oracle declares it in 6,915 of its 25,694 programs; left out on both sides so the self-check stays 0 |

Vertex inputs (oracle element never read, absent from ours): `118991d5859305d2` TEXCOORD0 · `1f908c28f4c7db57`
TEXCOORD1 · `21539f54e036f79f` COLOR0 · `543a7e6929c56637` COLOR0 · `67af147c69986fd1` TEXCOORD0 ·
`75e499b924d83722` COLOR0, TEXCOORD0 · `7b595eb2a13f4e10` COLOR0 · `8afeb23dccd2c7ad` TEXCOORD0, COLOR0 ·
`8b453459c97f4a9d` COLOR0, TEXCOORD0 · `8ec2cbc5768e53ef` COLOR0 · `96f479a4786d5c8d` COLOR0 · `9a5c2f3b5ccf3d62`
COLOR0 · `9c817639bbab62ec` TEXCOORD0 · `b0714c4f6cb83da2` SV_VertexID · `b26aff78ed4f93b3` TEXCOORD1 ·
`ba58a93f61f24b3f` COLOR0, TEXCOORD0 · `c515e87ab85790f5` NORMAL0 · `c76396b821a35007` NORMAL0 ·
`d3bc6782f8b1de37` TEXCOORD0 · `e8f9e32726e966bf` COLOR0 · `ec8f5247a12a61ad` TEXCOORD0 · `fef345f89e85fa86` COLOR0.

`SV_Barycentrics`: `1d6f5717e55412ab` · `2dce678b290ae4f8` · `377453bce0699d5d` · `43cb7f36fdb7bf37` ·
`71a02e6cb47638bb` · `77c88e56d8b57813` · `90b97fa5a34be3e5` · `aefe055414bc8826` · `b96714b90feacf4b` ·
`d07c1a13a1d87699` · `d20027fbc6dea111` · `e94c932a45ce8441`. Kind table: `df939974040ca239`.

### (c) Translator bugs (`crates/sir`, the lead's)

1. **`68954fce6cd4d34e` (ps), SigOut: an output the console does not have.** The console program writes only
   through `image_store` and ends with `exp null` (target 9, `en=0`, `vm done`); its container's `OSG1` is empty,
   and the oracle declares no output. Ours declares `float4 unused : SV_Target0` and returns 0 in it: with a
   colour target bound at slot 0 the host would write 0 where the console writes nothing. Place:
   `crates/sir/src/lift/io.rs:450-452` (`if l.io.exports.is_empty() && osg1.is_empty()` pushes the dummy
   element). Should be: no output at all (`void main`, no `Out` struct).
2. **62 v0 pixel programs (every v0 program that uses 16-bit interpolation): packed fp16 attributes read the
   wrong components.** `interp16` (`crates/sir/src/lift/io.rs`, around line 180) maps `src0` = (attr, chan,
   high) to component `chan` of `high ? row2 : row`. The packing (`crates/shader/src/native.rs`, `VsOutput`
   `pack2`, which `PsInput` follows) puts two 16-bit values per dword, channel c of the row in dword c/2 (odd
   channels in the high half), then the second row from the next dword: so the component is **c = 2·chan +
   high**, from `row` for c < 4 and `row2` (c − 4) otherwise (`row2 == row` when there is one row). Evidence:
   `1d6f5717e55412ab` has 7 fp16 interpolations of attr 1 (`row` 3 TEXCOORD1 xyz, `row2` 4 TEXCOORD4 xyzw);
   c = 2·chan + high gives exactly TEXCOORD1.xyz + TEXCOORD4.xyzw, the 7 components the oracle reads; ours reads
   TEXCOORD1.w (absent: lowered to `f32tof16(0)`), never TEXCOORD4.y, and hands most values to the wrong register. `377453bce0699d5d`
   (one row, `row2` = `row` = 2): ours reads r2.x for chan 0 high (should be .y) and r2.y for chan 1 low (should
   be .z). `oraclediff` now reports these as `InputReads(TEXCOORD1.z)` (34) and `InputReads(TEXCOORD4.y)` (28);
   the fix should bring InputReads to 0. The read mask (`e.0 |= 1 << chan`) needs the same mapping.

Files outside git: `D:\victory-data\s7\` (`v0*.csv`, `oracle-fix\`, `oracle\` = hard links to the lead's oracle
with the 4 fixed files as separate copies).

## Decided

- The census counts all 26,084 native programs, and reports the 25,694 VS+PS+CS+MS beside it, because ES
  and GS programs are real code the title ships (merged-stage halves).
- Deduplicate by `DXIL` part, keep the first in file order; compare with the fork by (code hash, stage),
  because its kept container hash among duplicates depends on thread order.

## Next

- S7 closed: every v0 interface diff is classified (below). Left: the two translator bugs for the lead.
- The lead's: GFX10 → `sir`. The field model gives every operand by name (`Decoded::get`, `enc::src`).

## For the lead

1. `secondary-agent.md` S2 and `tasks.md` Build 4 quote "4.64M instructions" as a census total to match.
   That is 4,641,843 **emitted DXIL** instructions over the **1,433 world-frame** programs (the fork's
   `census_default.csv`, column `total`), not console instructions of the corpus. The corpus has
   43,952,121 console instructions (40.6M without hints); the world set 2,144,273.
2. "25,694 programs" is the VS+PS+CS+MS count. The archive holds 26,084 native programs: 195 ES and 195 GS
   more (the fork's own summary says 26,084 translated). The census lists both numbers.
3. **S7: four oracle files are the wrong way round** (S5's pairing by creation time; ties in the same tick).
   Copy `D:\victory-data\s7\oracle-fix\{2d539d4d776445e4,fbf743aed16d2bcd,2b9c4c58786e197b,727b2ac8d96d14ab}.{dxil,vsh}`
   over `D:\victory-data\rebuild-oracle`, and in `agents/shader-oracle.csv` swap the sizes of those rows:
   `fbf743aed16d2bcd` 2367/2411, `2d539d4d776445e4` 3147/3207, `727b2ac8d96d14ab` 6457/6534,
   `2b9c4c58786e197b` 3067/3111. `apps/rebuild/src/bin/oracle.rs` (outside this task's files) should settle
   same-tick groups the way `oraclediff --ties` does before it is run again.
4. **S7, translator:** `68954fce6cd4d34e` declares a dummy `SV_Target0` (`crates/sir/src/lift/io.rs:450-452`);
   and `interp16` maps packed fp16 attributes to the wrong components in all 62 v0 programs that use it
   (section S7 (c), with the mapping and the evidence).
5. **S7, a question:** `df939974040ca239` (and the 6,915 oracle programs with the fork's kind table) loads
   through a heap descriptor whose index it reads at run time. Ours always reads it as a `ByteAddressBuffer`;
   the fork checks whether the title put a constant-buffer view there. If the runtime ever leaves a CBV in such
   a slot, a raw view of it is undefined in D3D12. Named as a design difference here; whether every buffer
   descriptor the title writes is mirrored as a raw view is the runtime's to confirm.
