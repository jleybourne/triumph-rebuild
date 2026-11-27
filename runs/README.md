# 📊 Runs

Every measurement, newest first. Each has a write-up (what was measured, the answer, the verdict, the caveats)
with the raw output beside it. Machine: i7-10700KF, RTX 4070 Ti 12 GB, 64 GB, PCIe 3.0, Windows 11.

| | run | answer | raw |
|:--:|:--|:--|:--|
| ✅ | [Build 3 — closed: the title reaches its first render call](build3-reached.md) | entry point → `D3D12XboxCreateDevice` in 1.4 s; TLS slot 0 given to the title; exit 0, machine unaffected | [runtitle --run](build3-reached.txt) |
| 🔄 | [Build 4 — execution against the oracle](build4-exec.md) | 5,195 shaders bit-exact on WARP (lane `shaderexec`) | [exec csv](build4-exec.csv) |
| 🔄 | [Build 4 — wave-wide programs](build4-wave64.md) | a wave mapping per program from its census class, console wave size and stage (`wavetest`, `wavecost`) | — |
| 🔄 | [Build 4 — v1: structurized control flow](build4-v1.md) | 7,402 / 9,128 agnostic programs compile (81.1 %); v0 2,837 / 2,881 | — |
| 🔄 | [Build 4 — our v0 DXIL against the oracle](build4-oraclediff.md) | interface diffs SigIn 1,231 → 0, SigOut 801 → 0, Bindings 525 → 0 of 2,834 (35 / 3 / 4 before S7, PR #21; 0 / 1 / 0 after it; the last fixed with PR #26); self-check 0, every mutation caught | — |
| 🔄 | [Build 4 — compute programs](build4-cs.md) | 117 / 132 v0 compute programs translate and compile (`cs_6_6`) | [translate cs](build4-cs.txt) |
| 🔄 | [Build 4 — vertex shaders](build4-vs.md) | 926 / 951 v0 vertex shaders translate and compile (`vs_6_6`) | [translate vs](build4-vs.txt) |
| 🔄 | [Build 4 — pixel shaders translated with IO and resources](build4-ps.md) | 1,791 / 1,798 v0 pixel shaders translate and compile with DXC (`ps_6_6`, `-O3`) | [translate ps](build4-ps.txt) |
| ✅ | [Build 4 — the lifter, scalar and vector ALU](build4-lift.md) | ~150 ALU forms; the v0 set has no ALU opcode left untranslated | [translate lift](build4-lift.txt) |
| ✅ | [Build 4 — opcode coverage of the v0 set](build4-coverage.md) | 2,881 programs, 173 opcodes; 119 cover 95.6 %, 143 cover 99.6 % | [translate coverage](build4-coverage.txt) |
| ✅ | [Build 3 — the title's own code runs](build3-startup.md) | 16/16 DLL entry points ok; exe TLS callbacks and entry run 60 s without a crash; waited (later found: graphics placement spinning in the kernel, fixed) | [runtitle --run](build3-startup.txt) |
| ✅ | [Build 3 — the title mapped and bound](build3-maptitle.md) | 17 images, 8,257 relocations; 1,876 of 1,924 imports bound (97.5 %) | [maptitle](build3-maptitle.txt) |
| ✅ | [Build 3 — what the title's imports bind to](build3-runtime-census.md) | 624 of 697 imports (89.5 %) bind straight to the host; 34 overrides; 39 console-only in 7 DLLs | [runtimecensus](build3-runtime-census.txt) |
| ✅ | [Build 4 — the shader core on the card](build4-sirtest.md) | IR → HLSL → DXC → GPU: 300 runs exact on every lane; per-lane `WaveReadLaneAt` exact | [sirtest](build4-sirtest.txt) |
| ✅ | [Build 2 — labels into title memory](build2-labels.md) | title memory opens as a GPU heap; GPU labels seen 65 µs before the fence; 200/200 in order | [gputest --labels](build2-labels.txt) |
| ✅ | [Build 2 — the upload ring](build2-ring.md) | submit thread 32 µs a frame; 34 MB / 342k runs land in 19.4 ms; verified after 60 frames | [gputest --ring](build2-ring.txt) |
| ✅ | [Build 1 — tiled VRAM at console addresses](build1-tiles.md) | 7 proofs on the card; only stencil formats need the arena; 10 GiB committed, 0 wrong; no tile penalty on uploads | [scale](build1-scale10.txt) · [upload on tiles](build1-upload-tiled.txt) |
| ✅ | [Build 0 (b) — per-draw translation](build0b-lists.md) | 0.43 µs a draw with the driver, 1.9 ms a frame; descriptors from a dirty list 2.0 ms; list cache dropped | [listbench](build0b-lists.txt) |
| ✅ | [Build 0 (a) — dirty-chunk upload](build0a-upload.md) | 12.64 GB/s; hash + scatter + 4 workers, 16-30 ms at the title's shape; copy-per-run rejected; 40/40 verified | [uploadbench](build0a-upload.txt) |
| ⏸️ | Build 0 (c) — the title's GPU time | needs a title frame; closes in Build 5 | |
