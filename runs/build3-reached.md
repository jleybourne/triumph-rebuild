# Build 3 closed — the title's own code reaches its first render call

`REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin runtitle -- --run --seconds 20` · 2026-11-27.
Raw output: [`build3-reached.txt`](build3-reached.txt).

```
tls       the title's block moved from slot 6 to slot 0 on every thread; ours to 6
REACHED   the first render call: D3D12XboxCreateDevice   (1.4 s after the entry point)
gdk       2 interfaces asked for and not served yet (named in the raw output)
exit      0; the machine unaffected (job object, budget, watchdog in force)
```

## What stood between the entry point and the first render call

1. **The machine itself** (fixed earlier the same day, `runs/build3-startup.md`, commit 2a2af01): graphics
   reservations placed with `VirtualAlloc2` address requirements and write-watch spun in the kernel on a full
   4 GiB window. The title looked like it was waiting; the process could not be killed.
2. **The PC's own GDK runtime**: the title's stub loads `xgameruntime.dll` with `LoadLibraryExA`, which was not
   overridden, so it got the host's. Now ours (`rebuild-gdk`).
3. **Thread-local storage** (this run): the title's code reaches its thread-local block as `gs:[0x58]` then
   `[array]`, slot 0, as code built for an exe may (`malloc` reads its allocator at block + 0x13A8). Loaded beside
   our host exe it had slot 6, and slot 0 was ours: the allocator pointer was our memory (one run it read the
   text "treambuf"). Found with a crash reporter (`apps/rebuild/src/crash.rs`: code, address, what is mapped
   there, the unwound call chain) and `dumpbin` on the prepared copy.

   **Fix, at no cost per access** (`crates/runtime/src/tls.rs`): our own code always reads `_tls_index`
   (checked: every `gs:[0x58]` in `runtitle.exe` follows a `_tls_index` load). So the two blocks are swapped in
   every thread's TLS array and the indices swapped: the title's becomes 0, ours 6. Existing threads once; new
   threads by a TLS callback in our exe, which also runs the title exe's thread-attach callbacks (the loader
   cannot: they are cleared in the prepared copy). The fork instead rewrote each `gs:[0x58]` load into a thunk
   call, a call on every thread-local access.

## Next

Build 5: `D3D12XboxCreateDevice` lands in `crates/runtime/src/exports.rs` (`on_create_device`). The two GDK
interfaces the title asked for are next for the `gdk` crate.
