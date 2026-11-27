# Thoughts — why a rewrite, and what makes it faster than what it learns from

## The number we are chasing

```
baseline to beat   131.3 ms · 7.6 fps · ~4,500 draws · 356 dropped   (the layer being replaced)
target             33.3 ms · 30 fps at the same draw count           (FSR doing the upscaling)
```

## Where the 131 ms goes

```
preparing        85-110 ms   copies 31-48 ms (83-115 MB) · textures ~6 ms
copying back     48-54 ms    (60-65 MB)
the card         ~71-90 ms   ~26 ms of it the layer's own hashing, ~45 ms the title's work
list calls       20,293 a frame = 243.2 ms on the submitting thread
present wait     ~68 ms while the submit thread copies ~72 MB up
a copy           5.5 us, of which ~1 us is bytes
```

**Every one of those, except the title's 45 ms, is a consequence of one design decision:** the layer gives
the title ordinary CPU memory, keeps a host mirror, and copies in and out. Nineteen A/B experiments
against that structure came back neutral or worse — it is not under-tuned, it is shaped wrong.

## What the rewrite keeps, and from where

**From the sibling layer's design** (read, reimplemented, not copied):

- sparse windows: the title's addresses are GPU addresses, backed by 64 KiB card pages as it commits;
- write-watch with 64-byte chunk uploads on an async ring behind a fence;
- tiled placement of GPU-owned resources at console addresses, pages shared by address;
- bounded arenas for what cannot be tiled, released after the last user.

**From the layer being replaced** (knowledge, not code):

- which console APIs this title actually calls, and how they behave;
- the shader census shape: refusal reasons, not percentages;
- descriptor layout, and the fact that a raw UAV needs `R32_TYPELESS` + `RAW`;
- the measurement discipline: fps · ms · draws · dropped, one change per run, screenshots for picture.

## What we do that neither of them does

1. **List caching.** Most frames submit the same list shapes. Record → translate in parallel → cache by
   content and replay. That attacks the 243 ms that no memory work touches.
2. **Dirty lists for descriptors.** They scan; we keep a list. 203,127 slots examined per frame is
   12,027 records of real work plus a hundred thousand empty ones.
3. **No fallback mirror.** A fallback becomes the path that ships. There is exactly one memory model and
   every failure surfaces as a bug in it.
4. **Nothing compiles in a frame.** Shaders offline, pipelines into a hash-keyed library at start.

## The honest arithmetic, so nobody is surprised later

```
layer overhead removed           frame ≈ the card's work (~45 ms)  ≈ 22 fps
plus cached/parallel translation  headroom on the CPU side, no stall in present
plus the title's own FSR          card work halves                ≈ 30+ fps
```

30 fps needs both halves. Memory work alone gets into the low twenties; the upscaler is not optional and
is therefore a build step, not a stretch goal.

## Rules this project keeps

- A step closes when its number is on the frame line, not when its code compiles.
- One change per measurement. Batches cannot be attributed — that has already cost a night.
- Never report fps without draws and dropped beside it.
- If a device is removed, run the control (`gputest --bare`) before blaming anything. It found the real
  bug the first time it was used.
