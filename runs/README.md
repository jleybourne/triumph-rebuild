# 📊 Runs

Every measurement, newest first. Each has a write-up (what was measured, the answer, the verdict, the caveats)
with the raw output beside it. Machine: i7-10700KF, RTX 4070 Ti 12 GB, 64 GB, PCIe 3.0, Windows 11.

| | run | answer | raw |
|:--:|:--|:--|:--|
| ✅ | [Build 2 — labels into title memory](build2-labels.md) | title memory opens as a GPU heap; GPU labels seen 65 µs before the fence; 200/200 in order | [gputest --labels](build2-labels.txt) |
| ✅ | [Build 2 — the upload ring](build2-ring.md) | submit thread 32 µs a frame; 34 MB / 342k runs land in 19.4 ms; verified after 60 frames | [gputest --ring](build2-ring.txt) |
| ✅ | [Build 1 — tiled VRAM at console addresses](build1-tiles.md) | 7 proofs on the card; only stencil formats need the arena; 10 GiB committed, 0 wrong; no tile penalty on uploads | [scale](build1-scale10.txt) · [upload on tiles](build1-upload-tiled.txt) |
| ✅ | [Build 0 (b) — per-draw translation](build0b-lists.md) | 0.43 µs a draw with the driver, 1.9 ms a frame; descriptors from a dirty list 2.0 ms; list cache dropped | [listbench](build0b-lists.txt) |
| ✅ | [Build 0 (a) — dirty-chunk upload](build0a-upload.md) | 12.64 GB/s; hash + scatter + 4 workers, 16-30 ms at the title's shape; copy-per-run rejected; 40/40 verified | [uploadbench](build0a-upload.txt) |
| ⏸️ | Build 0 (c) — the title's GPU time | needs a title frame; closes in Build 5 | |
