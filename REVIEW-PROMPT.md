# REVIEW-PROMPT — the exact brief to give an independent model

Copy the block below into a fresh session with Claude (or any other reviewer) from this directory. It is
self-contained: the reviewer does not need this conversation.

## Run it

```powershell
cd D:\Developer\triumph-rebuild
claude auth status --text        # must say logged in
claude                           # interactive, then paste the prompt below
```

or one-shot, non-interactive:

```powershell
claude -p "Read HANDOFF.md and answer its questions. Check the claims against this repo and D:\Developer\xcl. Be blunt — the owner wants errors found, not agreement. Do not modify files." --model opus --max-turns 25
```

## The prompt

> Read `HANDOFF.md` in this repository, then read `plan.md`, `thoughts.md`, `tasks.md`,
> `crates/uma/src/lib.rs`, `crates/uma/src/windows.rs` and `apps/rebuild/src/bin/gputest.rs`.
> Then read `D:\Developer\xcl` as the reference implementation (`crates/xcl/src/gpu/uma.rs`,
> `crates/xcl/src/hle/d3d12x/sparse.rs`, `vram.rs`, `console_heap.rs`) and
> `D:\victory-work\wt-docs\ROADBLOCK.md` for the measured behaviour of the layer being replaced.
>
> Answer the five questions at the end of `HANDOFF.md`. Verify claims against the code and against the
> numbers, not against the prose. Say plainly where the documents overstate what is proven. Attack, in
> particular, the five weak points the author lists — starting with "30 fps at what internal resolution?".
>
> Be blunt. The owner wants errors found, not agreement. Do not modify any file.

## What the author expects it to say

Recorded up front so that a review which only repeats this is judged as adding nothing:

1. **Internal resolution is unstated.** The sibling layer's "steady 30 fps" does not say at what
   resolution or FSR level. Our 33.3 ms target may be inheriting an assumption about pixels, not draws.
2. **The ~45 ms card figure is soft.** It was derived by subtracting an attributed hashing cost from a
   single "on the GPU" line, never from per-pass timestamps.
3. **Rewrite versus hybrid is under-priced.** A rewrite re-earns the loader, runtime, descriptor model and
   shader recompiler; only the memory layer must change.
4. **The proof is narrower than the architecture.** One buffer, one heap, one dispatch — not tiled
   placement, not sharing, not a title's real commit pattern.
5. **`tasks.md` is missing whole surfaces** — console descriptor heaps, the title's own DLLs, audio,
   input, and the GDK discovery loop.

## The one test that settles it

Run `D:\Developer\xcl` against this same title on this machine and measure it. If it reaches 30 here, the
target is real and its architecture should be adopted mechanism-for-mechanism. If it does not, the central
assumption of `plan.md` is wrong and the plan changes before more code is written.
