# HANDOFF — review this before any more code is written

You are being asked to check a plan, not to write code. Be blunt. The owner has explicitly asked for
errors to be found, and the author of this handoff has asked to be corrected where he is wrong.

## The machine

```
CPU   i7-10700KF        GPU  RTX 4070 Ti, 12,282 MiB       RAM 64 GB
BAR1  16,384 MiB, 16,355 used — aperture fully mapped, ReBAR healthy (measured)
OS    Windows 11        Shell: git-bash
```

## What this repository is

`D:\Developer\triumph-rebuild` — a from-scratch Rust rewrite of an Xbox-Series-to-Windows compatibility
layer, built around one decision: **the title's memory is the GPU's memory**. It exists because the
previous layer (`D:\Developer\fxceless-victory`) is dominated by emulating shared memory with copies, and
nineteen measured attempts to tune that away all failed.

Read, in this order:

```
README.md                    what it is, what's proven, what isn't
thoughts.md                  the reasoning and the arithmetic
plan.md                      the five decisions, work order, what we refuse to build
tasks.md                     the build list, each item closed by a measurement
crates/uma/src/lib.rs        address space: 4 GiB windows, 64 KiB pages, placement, sharing (8 tests)
crates/uma/src/windows.rs    the GPU/CPU shared-memory proof (D3D12)
apps/rebuild/src/bin/gputest.rs   the proof binary
```

## What is actually proven (this ran on the card above)

```
$ cargo test         8 tests green  (page-per-page backing, sharing by address, lifetime by user
                                    count, atomic refusal, window-relative addresses, covering page)
$ cargo run --bin gputest
backend   D3D12 custom heap (WRITE_BACK, L0), persistently mapped
cpu read  [CA, FA, BE, BA, 00, 00, 00, 00, 2A, 00, 00, 00]
RESULT    the GPU wrote into memory the CPU reads, with no copy

$ cargo run --bin gputest -- --bare
RESULT    a bare dispatch completes: the device is healthy
```

The control run matters: the first three attempts removed the device (`0x887A0005`) and the control proved
the machine was healthy, which is how the real bug was found — a `RWByteAddressBuffer` needs a **raw UAV**
(`DXGI_FORMAT_R32_TYPELESS` + `BUFFER_UAV_FLAG_RAW`), otherwise the dispatch removes the device.

## The numbers this plan is built on (measured on the machine above)

From the previous layer's own runs (`D:\victory-work\wt-docs\ROADBLOCK.md`, and its `docs/runs.md`):

```
frame            131.3 ms · 7.6 fps median · ~4,500 draws · 356 dropped
                 best seen 111.3 ms / 9.00 fps at 3,962 draws
preparing        85-110 ms   copies 31-48 ms (83-115 MB) · textures ~6 ms
copying back     48-54 ms    (60-65 MB)
on the GPU       ~71-90 ms   ~26 ms attributed to the layer's own hashing, ~45 ms the title's work
list calls       20,293 a frame = 243.2 ms on the submitting thread
present wait     ~68 ms, while the submit thread copies ~72 MB up
a copy           5.5 us each, of which ~1 us is bytes
descriptor sync  203,127 slots examined to mirror 12,027 records
```

Measured and did not help, all A/B on the same machine: copy merging (4 attempts), double buffering
(refused from code), barrier de-duplication, `rep movsb` copies with a fault handler, runtime-precompiled
pipelines (0.115 vs 0.029 ms/draw), VRS (inert — zero shading-rate calls), FSR quality 4 (black screen),
GPU upload heaps (2× slower, −1,370 draws), guard epochs, present tickets, ordered copies, wide prepare.
Wave64 divergence is 16.5 % of 4.64 M instructions, so removing all of it caps at 13.2 fps.

## The claims to check

**A.** The five decisions in `plan.md` — one memory/no mirror; CPU writes as 64-byte chunks on an async
ring; descriptors in the title's own memory with dirty lists; list calls recorded, translated in parallel
and cached; nothing compiling during a frame — are sufficient to reach **33.3 ms / 30 fps** at ~4,500
draws with the title's own FSR doing the upscaling.

**B.** Memory work alone reaches roughly 22 fps (the card's ~45 ms); the upscaler is required for 30.

**C.** A rewrite in Rust is the right vehicle, versus retrofitting the existing layer's memory model.

**D.** `crates/uma/src/windows.rs` demonstrates what it claims, and is a meaningful first step rather
than a toy.

**E.** `tasks.md` has nothing material missing between the current state and a running title.

## Where the author already thinks he is weak (attack these first)

1. **"30 fps at what internal resolution?"** The sibling layer's README claims "steady 30 fps" without
   stating resolution or the FSR level used. If that 30 is at a much lower internal resolution, our
   33.3 ms target silently inherits that assumption. Draw count staying constant does not mean pixels do.
2. **The ~45 ms card number is the weakest in the brief.** It came from a frame line reporting ~71-90 ms
   "on the GPU", with ~26 ms attributed to our hashing by subtraction — not from per-pass timestamp
   measurements, which were never successfully captured. If the real split is different, the 22 fps
   intermediate is wrong by up to 15 ms.
3. **Rewrite vs hybrid is under-priced.** A rewrite must re-earn the loader, the console runtime, the
   descriptor model and the shader recompiler — the parts that took the existing layer months — while only
   the memory layer must change. A hybrid that keeps the working translator and replaces the allocator and
   copy path may reach the same result with far less risk. The plan asserts rewrite without costing that.
4. **The proof is narrower than the architecture.** One buffer, one custom heap, one dispatch. It does not
   demonstrate tiled placement, page sharing across two resources at one address, or a title's arbitrary
   commit pattern at scale.
5. **`tasks.md` is missing whole surfaces**: console descriptor heaps (T#/V#), the title's own DLLs (Bink,
   the FSR upscaler, GDK HTTP), audio, input, and the loop for discovering which GDK calls this title
   actually makes.

## The reference implementation to compare against

`D:\Developer\xcl` (GPL-2.0) — a sibling layer whose README claims a retail title at steady 30 fps on PC
with *"title graphics memory shared by CPU and GPU without copies, up to 16 GB (4 GiB windows)"* and the
console FSR 4 upscaler running natively. Read it as design, not code:

```
crates/xcl/src/gpu/uma.rs              unified block and sparse windows
crates/xcl/src/hle/d3d12x/sparse.rs    tiles, commits, write-watch, 64-byte chunk uploads on a ring
crates/xcl/src/hle/d3d12x/vram.rs      tiled placement of targets at console addresses, page sharing
crates/xcl/src/hle/d3d12x/console_heap.rs   console descriptor heaps and the PC descriptors beside them
```

## What to return

1. A verdict on each of A-E, and the reasoning that would change your mind.
2. Every place the documents overstate what is proven, or quote a number the runs do not support.
3. The single biggest technical risk between here and 30 fps, and the cheapest evidence that settles it.
4. What the reference implementation does that this plan has missed, and what it does that this plan
   should deliberately not copy.
5. A blunt verdict: will this plan reach 30+ fps on this hardware, and if not, what specifically changes.

## The test nobody has run yet

**Run `D:\Developer\xcl` against this same title and measure it.** If it reaches 30 on this machine, the
target is real and its architecture is worth adopting mechanism-for-mechanism. If it does not, the central
assumption of this plan is wrong and the plan should change before more code is written. Everything else
here is cheaper than that answer.
