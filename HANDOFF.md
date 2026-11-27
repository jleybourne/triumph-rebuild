# HANDOFF — review this before more code is written

You are being asked to check a plan, not to write code. Be blunt. The owner has explicitly asked for
errors to be found, and the author has asked to be corrected where he is wrong.

## The machine

```
CPU   i7-10700KF (8C/16T)   GPU  RTX 4070 Ti, 12,282 MiB      RAM 64 GB
PCIe  3.0 x16 — measured 12.64 GB/s staging -> VRAM (runs/build0a-upload.md)
BAR1  16,384 MiB, 16,355 used — aperture fully mapped, ReBAR healthy (measured)
OS    Windows 11        Shell: git-bash / PowerShell
```

## What this repository is

`D:\Developer\triumph-rebuild` — a from-scratch Rust rebuild of an Xbox-Series-to-Windows compatibility
layer: loader, console runtime, memory, shader recompiler, D3D12.X device, lists, descriptors, present,
upscaler. Target: **30 fps in-world (33.3 ms) at a stated output resolution and upscaler mode, on NVIDIA
with DLSS and on AMD with FSR**, at or above a minimum spec. Intel GPUs are not supported.

It learns from two layers by reading their design, never by copying code:

```
D:\Developer\xcl                    sibling layer (GPL-2.0)
D:\Developer\fxceless-victory       the layer being replaced; the owner's fork of it is
                                    github jleybourne/Triumph = git remote `triumph` (branch triumph/main)
```

Read, in this order:

```
README.md                         what it is, what's proven, what isn't
progress.md                       done / not done, % by build step, open problems
plan.md                           the six decisions, work order (Builds 0-9), what we refuse to build
tasks.md                          every task with the number that closes it; known traps from the fork
runs/build0a-upload.md            Build 0 (a): the upload benchmark and its verdict
thoughts.md                       the original reasoning (written before plan.md's corrections)
crates/uma/src/lib.rs             address space bookkeeping (8 tests)
crates/uma/src/chunks.rs          64-byte diff, gather, chunk indices (7 tests)
crates/uma/src/upload.rs          the Build 0 (a) benchmark: write-watch, diff, copy and scatter paths
crates/uma/src/windows.rs         the GPU->CPU proof (D3D12)
```

## The plan in one screen (plan.md)

```
1  GPU-owned memory in tiled VRAM at console addresses, never copied back; CPU-written memory
   uploaded as dirty 64-byte chunks on an async queue; a small explicit GPU->CPU region
2  descriptors in the title's memory, dirty list, no per-frame scan
3  lists recorded as data, translated on workers; cache only if the hit rate pays
4  nothing compiles in a frame; wave64 lowered to wave32 for every vendor
5  one upscaler interface, DLSS (Streamline) on NVIDIA, FSR (FidelityFX SDK) on AMD
6  every number from one frame line: fps · ms · draws · dropped · upload · translation · res · mode · vendor
```

Build 0 comes before any more code and answers three questions on our own code: (a) the cost of dirty
uploads at real volume, (b) per-draw translation cost against 7.4 µs, (c) the title's real GPU time.

## What is actually proven (on the machine above)

```
$ cargo test --release -p rebuild-uma      15 tests green
$ cargo run --bin gputest                  RESULT the GPU wrote into memory the CPU reads, with no copy
$ cargo run --bin gputest -- --bare        RESULT a bare dispatch completes: the device is healthy
$ cargo run --release --bin uploadbench    Build 0 (a), 40 configurations, 40 verified byte-exact
```

**Build 0 (a), the result** (`runs/build0a-upload.md`):

```
ceiling        115 MiB in one copy: 9.54 ms = 12.64 GB/s
best method    hash diff + compute scatter + 4 workers
               108.5 MB all changed, 9.3k runs      33.8 ms wall, 13.4 ms of it GPU
                55.8 MB, 440k runs                  30.1 ms wall,  6.9 ms GPU
                11.4 MB, 167k runs                  16.4 ms wall,  1.2 ms GPU
               nothing changed, 36.8k pages written ~17-31 ms     (the detection floor)
rejected       one CopyBufferRegion per run: 146 ms just to record 440k copies
verdict        decision 1 holds only if the upload is scatter-based AND runs on workers a frame
               ahead; serially it is a whole frame
```

The fork measured what the title really sends: **~48 MB changed of 115 MB copied, exact CPU-owned
ranges 226,099 a frame / 31.3 MB**. That sits between the 10 % and 50 % rows above.

## The numbers the plan rests on

From the layer being replaced (`D:\victory-work\wt-docs\ROADBLOCK.md`, fork `docs/`):

```
frame            131.3 ms · 7.6 fps median · ~4,500 draws · 356 dropped
preparing        85-110 ms   copies 31-48 ms (83-115 MB, ~11,500 copies) · textures ~6 ms
copying back     48-54 ms    (60-65 MB)
on the GPU       ~71-90 ms   title lists 43-63 ms across runs (RT lighting off); ~80 ms card busy
list calls       20,293 a frame = 243.2 ms, spread over the title's 5 recording threads
descriptor sync  203,127 slots examined to mirror 12,027 records
```

## Fork findings that contradict plan.md (verify these first)

1. **Decision 5's premise is wrong.** Method 0x218 creates compute pipelines from shader containers; it is
   not an upscale call with inputs. Run as a Series X, the title never loads the FidelityFX loader. Its FSR
   is ordinary compute dispatches, which must be recognised by their bindings (fork commit `ee0b94a3`).
2. **No known way to lower the internal resolution.** The scene is 2560x1440 at `fsrQuality` 3 and 4 alike.
   Resizing the title's targets from the layer made it assert at load (fork `docs/research/resolution.md`).
3. **The ~45 ms title GPU figure is low.** The fork's per-list timestamps give 43-63 ms, with RT lighting off.
4. **The 7.4 µs serial per-draw budget may be the wrong frame.** The 243 ms is on 5 recording threads, 0 ms
   on the render thread, and the recorders wait 70 % of the time on the title's own scheduler.
5. **The title caps itself at 30 fps** (menus run at exactly 33-34 ms). This matters only for 60.

## The claims to check

**A.** Decisions 1-6 are sufficient for **33.3 ms / 30 fps at ~4,500 draws** on the RTX 4070 Ti with DLSS,
at a resolution the owner would accept, given fork findings 1-3.

**B.** Build 0 (a) supports decision 1 as amended (scatter, on workers, a frame ahead). Check the
benchmark is a fair model: write pattern, region size, synchronous frames, default heap rather than
tiles. Does the detection floor (~13-17 ms of 4 workers) fit beside the title's own CPU work on 8 cores?

**C.** A full rebuild in Rust is the right vehicle, versus a hybrid that keeps the fork's working loader,
runtime and 100 %-census shader translator and replaces only the memory model.

**D.** Wave32 everywhere (decision 4) is safe, given the fork's census: 25,694 / 25,694 programs translated,
7 pipelines still refused because they need 64-lane waves.

**E.** `tasks.md` has nothing material missing between the current state and 30 fps on NVIDIA.

## Where the author already thinks he is weak (attack these first)

1. **The upscaler route** — fork findings 1 and 2. If the internal resolution cannot be lowered, DLSS buys
   little, and 30 fps rests on the title's own GPU time alone (43-63 ms).
2. **Build 0 (b) and (c) are not done.** The CPU half and the GPU half of the frame are both still estimates.
3. **Rewrite vs hybrid is still not costed.**
4. **The proof is narrower than the architecture.** There are no tiles yet, no sharing on the GPU, and no
   real title commit pattern.
5. **The minimum spec is blank**, so "on every supported PC" cannot be checked yet.

## What to return

1. A verdict on each of A-E, and the reasoning that would change your mind.
2. Every place the documents overstate what is proven, or quote a number the runs do not support.
3. The single biggest technical risk between here and 30 fps, and the cheapest evidence that settles it.
4. What the reference layers do that this plan has missed, and what it should deliberately not copy.
5. A blunt verdict: will this plan reach 30 fps on this hardware, and if not, what specifically changes.

## What we do not do

We do not build or run xcl or the old layer's current builds; we measure only our own code against the
baseline already recorded. Questions that only a run of theirs could answer become Build 0 questions on
ours.
