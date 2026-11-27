# Build 3 — the title's images mapped and bound

`REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin maptitle` · 2026-11-27. Code:
`apps/rebuild/src/bin/maptitle.rs` over `crates/loader` and `crates/runtime`. Raw output:
[`build3-maptitle.txt`](build3-maptitle.txt). The game folder is only read; no title code runs.

## What it does

Every image in the title's folder is parsed, placed (at its preferred base, or the next free 64 KiB slot high
in the address space), registered as a title module with its exports, then mapped: sections copied,
relocations applied, imports bound. The title's own DLLs bind first, then our runtime (`xmem`, the module,
memory and process overrides), then the host.

## The answer

```
images    17 mapped (2 at their preferred base, 15 moved); 8257 relocations applied
imports   1876 of 1924 bound (97.5 %): 467 from the title's own DLLs, 1409 from our runtime and the host
missing   48 (the discovery list; each is a function still to build):
    27  console DLL not built yet: acphal.dll 9 · chad.dll 5 · d3d12_xs.dll 5 · xapu.dll 5 · xcrdapi.dll 1 · xg_xs.dll 2
    21  override not built yet: api-ms-win-core-file-l1-1-0.dll 16 · api-ms-win-core-file-l1-2-0.dll 1 · api-ms-win-core-file-l2-1-0.dll 1 · kernel32.dll 3
```

## Verdict

- **97.5 % of the title's imports bind today.** The 48 left are exactly the work already assigned: files (21,
  lane `files`), audio (19, lane `audio`), `d3d12_xs` (5, Build 5, lead), the texture layout (2, lane `xg`) and
  DirectStorage (1, lane `dstorage`).
- 15 of 17 images could not take their preferred base and were relocated: 8,257 relocations, as the PE loader
  was built to do.
- The census counted 18 images; one did not parse as a PE32+ image here (named in the next run's log).
