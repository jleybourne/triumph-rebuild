# Build 3 — the title's own code runs: first startup

`REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin runtitle -- --run --seconds 60` · 2026-11-27.
Code: `crates/loader/src/prepare.rs`, `crates/runtime/src/title.rs` (load, bind, trap thunks),
`apps/rebuild/src/bin/runtitle.rs`. Raw output: [`build3-startup.txt`](build3-startup.txt).

## What happened

```
loaded    17 images; 1881 imports bound (467 from the title's own DLLs); 43 bound to trap thunks (unbuilt)
hooks     the exe's __XMemAllocationHooks__ installed
dllmain   16 of 16 title DLLs ok (vcruntime140, bink2scarlett, xcurl, msvcp140, xboxbvhbuilder,
          amd_fidelityfx_loader_xbox, amd_fidelityfx_upscaler_xbox, concrt140, libhttpclient.gdk, …)
tls       2 callbacks of the exe run
entry     the exe's entry point on an 8 MiB stack
TIMEOUT   no render call within 60 s
discovery: no trapped import called; no failed GetProcAddress / LoadLibrary lookups
```

## Verdict

- **The title's own code runs in our process**, through prepared images loaded by the Windows loader (static
  TLS, unwind tables, relocation all Windows'). All 16 DLL entry points succeeded, the exe's TLS callbacks ran,
  and its entry point ran for 60 s without a crash.
- **It did not reach `D3D12XboxCreateDevice` in 60 s, and it called none of the 43 unbuilt imports.** So it is
  **waiting** (a wait, a sleep loop, or a spin), not failing on a missing function.
- **Next:** find what the title's main thread waits on. Suspend `title-main` every few seconds, read its
  RIP and stack (`GetThreadContext` plus `RtlLookupFunctionEntry` / `RtlVirtualUnwind`, since Windows
  registered the unwind tables), and map the addresses to image and RVA. The fork's notes on the title's
  startup (`CLAUDE.md`: its hang watchdog sleeps in one-second steps; its job scheduler) are the first
  suspects.
