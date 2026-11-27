# Build 3 — what the title's imports bind to

`cargo run --release --bin runtimecensus` · 2026-11-27 · Windows 11 host. Code: `crates/runtime` (`classify`,
`host::Host`, `host::HostResolver`). Input: lane `loader`'s census, `agents/loader-imports.csv`. Raw output:
[`build3-runtime-census.txt`](build3-runtime-census.txt).

## The question

The title is native x86-64 code using the Windows calling convention. Of the 697 functions it imports that
its own DLLs don't supply, how many already exist on the host **with the same meaning**? Those can bind
straight to the host's function, with no wrapper and no cost. Each one is looked up for real (System32 only).

## The answer

```
  624   89.5 %  host: bound straight to the host's own function, no wrapper
   34    4.9 %  override: the host has it, the meaning differs here (ours)
   39    5.6 %  console DLL: the whole DLL is console-only (ours)

overrides by reason: files 15 · modules 16 · memory 2 · process 1
console-only DLLs:   xmem 13 · acphal 9 · chad 5 · xapu 5 · d3d12_xs 4 · xg_xs 2 · xcrdapi 1
```

## Verdict

- **Build 3's runtime is 73 functions, not a console kernel.** Threads, events, waits, timers, TLS,
  critical sections, the C runtime, sockets, HTTP and crypto all bind to the host as they are.
- **What remains is ours:**
  - **memory:** `xmem` (13), the title's allocator, feeding Builds 1-2's watched regions and tiles;
  - **files:** 15 overrides through `crates/vfs`;
  - **modules:** 16 overrides; every `GetProcAddress` miss is a discovery entry (the GDK functions
    `XGame*` / `XUser*` are reached that way, not imported);
  - **graphics:** `d3d12_xs` (4: device creation, Build 5) and `xg_xs` (2: the console texture layout);
  - **audio** (`acphal`, `xapu`, `chad`: 19) and **DirectStorage** (`xcrdapi`: 1). These are helper lanes
    `audio` and `dstorage`.
- **Not yet counted:** the functions the title reaches through `GetProcAddress`. The discovery loop measures
  them at run time.
