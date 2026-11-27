# Build 4 — execution against the oracle (lane `shaderexec`)

`execcheck` · 2026-11-27 · **WARP** (the lead's rule after three driver resets: translated programs run on the
software rasterizer; the card only for a straight-line smoke set under `agents\CARD`). DXC 1.8.2502.11.
Code: `apps/rebuild/src/bin/execcheck/` (`main.rs` selection and supervisor, `run.rs` inputs and comparison, `ps.rs`
the pixel path, `vs.rs` the vertex path, `gpu.rs` the device). Per-program results:
[`build4-exec.csv`](build4-exec.csv) (hash, stage, v0/v1, verdict, reason, detail).

```
REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin execcheck -- --stage cs|ps|vs [--out <dir>]
```

RESULTS_PLACEHOLDER
