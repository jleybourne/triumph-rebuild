# 📊 Runs

Every measurement, newest first. Each has a write-up (what was measured, the answer, the verdict, the caveats)
with the raw output beside it. Machine: i7-10700KF, RTX 4070 Ti 12 GB, 64 GB, PCIe 3.0, Windows 11.

| | run | answer | raw |
|:--:|:--|:--|:--|
| ✅ | [Build 3 — the title's own code runs](build3-startup.md) | 16/16 DLL entry points ok; exe TLS callbacks and entry run 60 s without a crash; waits before its first render call | [runtitle --run](build3-startup.txt) |
| ✅ | [Build 3 — the title mapped and bound](build3-maptitle.md) | 17 images, 8,257 relocations; 1,876 of 1,924 imports bound (97.5 %) | [maptitle](build3-maptitle.txt) |
| ✅ | [Build 3 — what the title's imports bind to](build3-runtime-census.md) | 624 of 697 imports (89.5 %) bind straight to the host; 34 overrides; 39 console-only in 7 DLLs | [runtimecensus](build3-runtime-census.txt) |
| ✅ | [Build 4 — the shader core on the card](build4-sirtest.md) | IR → HLSL → DXC → GPU: 300 runs exact on every lane; per-lane `WaveReadLaneAt` exact | [sirtest](build4-sirtest.txt) |
| ✅ | [Build 2 — labels into title memory](build2-labels.md) | title memory opens as a GPU heap; GPU labels seen 65 µs before the fence; 200/200 in order | [gputest --labels](build2-labels.txt) |
| ✅ | [Build 2 — the upload ring](build2-ring.md) | submit thread 32 µs a frame; 34 MB / 342k runs land in 19.4 ms; verified after 60 frames | [gputest --ring](build2-ring.txt) |
| ✅ | [Build 1 — tiled VRAM at console addresses](build1-tiles.md) | 7 proofs on the card; only stencil formats need the arena; 10 GiB committed, 0 wrong; no tile penalty on uploads | [scale](build1-scale10.txt) · [upload on tiles](build1-upload-tiled.txt) |
| ✅ | [Build 0 (b) — per-draw translation](build0b-lists.md) | 0.43 µs a draw with the driver, 1.9 ms a frame; descriptors from a dirty list 2.0 ms; list cache dropped | [listbench](build0b-lists.txt) |
| ✅ | [Build 0 (a) — dirty-chunk upload](build0a-upload.md) | 12.64 GB/s; hash + scatter + 4 workers, 16-30 ms at the title's shape; copy-per-run rejected; 40/40 verified | [uploadbench](build0a-upload.txt) |
| ⏸️ | Build 0 (c) — the title's GPU time | needs a title frame; closes in Build 5 | |
