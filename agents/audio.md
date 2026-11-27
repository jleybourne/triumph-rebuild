# Lane `audio`: the console's audio hardware

Crate `crates/audio` (package `rebuild-audio`). No GPU, no title bytes (every test builds its own input).
State: **A1-A5 closed.** A5 (round 2): our own XMA2 decoder, `crates/audio/src/wma`, now the XMA contexts'
default. 18 tests green (`cargo test --release -p rebuild-audio`), plus four hand-run checks (ignored by default:
the tone, the title-call timings, and A5's two checks against the title's own streams). Workspace: 245 passed, 6 ignored, 0 warnings.

Sources for A1: the fork's `victory-hle` modules `audio.rs` (acphal), `apu.rs` (xapu), `dsp.rs` (chad), read
through `git show triumph/main:...` and not copied, plus its run log
`diagnostics/timing-run-295s-2026-11-25.txt` (audio lines only). Layouts below are the fork's observations;
the code in this crate is ours.

## A1: the 19 functions

Every one is a function in this crate with the console argument layout on its doc comment. `crates/runtime`
binds them.

### `acphal.dll` (9)

| function | console signature | meaning | what the title passes (fork run log) | here |
|:--|:--|:--|:--|:--|
| `ApuCreateHeap` | `HRESULT (UINT32 cached, UINT32 nonCached)` | sizes the audio heap once, before the first allocation; `0x8ACA0001` after | not seen in the log | `heap.rs`: reserves max(sum, 2 GiB) of address space; sizes' units **unknown**, not enforced |
| `ApuAlloc` | `HRESULT (void** va, UINT32* apu /*opt*/, UINT32 size, UINT32 align, UINT32 flags)` | zeroed audio memory with its 32-bit device address; align a power of two ≥ 4 | flags 1 (meaning **unknown**, taken as cached/non-cached); one 401 MB block at start | `heap.rs` |
| `ApuFree` | `void (void* va)` | frees a block | | `heap.rs` |
| `ApuMapVirtualAddress` | `UINT32 (const void* va)` | CPU → device address; 0 outside | | arithmetic, no lock |
| `ApuMapApuAddress` | `void* (UINT32 apu)` | device → CPU address; null outside | | arithmetic, no lock |
| `ApuIsVirtualAddressValid` | `BOOL (const void* va, UINT32 align)` | inside the heap and device address aligned | | returns 0/1 in `eax` (right for `bool` or `BOOL` callers) |
| `AcpHalAllocateShapeContexts` | `HRESULT (ShapeContexts* io)` | six counts in (rate conversion, EQ/compressor, filter/volume, DMA, XMA, PCM), six arrays + device addresses out; once until released | XMA contexts only are used | `acp.rs`; bytes per context 32/256/256/16/64/256 (XMA, SRC, DMA as the middleware clears them; others an upper bound) |
| `AcpHalReleaseShapeContexts` | `HRESULT (void)` | frees them | | `acp.rs`; stops the XMA thread first |
| `AcpHalCreate` | `HRESULT (IAcpHal** out)` | the audio coprocessor object. Not COM: six slots, `Connect(cmdEntries, msgEntries)`, `Disconnect`, `SubmitCommand(cmd, u64 id, frame, data*, notifyApuAddr)`, `PopMessage(Message*)`, `GetMessageCount`, `Release` | commands seen: 2 (register messages) at start, then 5, 18, 7 (XMA enable, advance read, disable) | `acp.rs`; commands run when submitted, ids acknowledged by message (`0x10`), notification word set to 1. The middleware waits in its start-up for the first acknowledgement |

### `xapu.dll` (5): the Opus decoder

| function | console signature | meaning | title | here |
|:--|:--|:--|:--|:--|
| `XApuConnect` | `HRESULT (const ConnectInput*, ConnectOutput*, Handle*)` | one connection for one processing kind; gives packet/output memory and an optional result event | 256 streams, 2 commands per stream, 1,064,960 bytes, options 1 (bit 1 **unknown**) | `xapu.rs`; memory from the APU heap; kinds other than Opus decode → `E_NOTIMPL` |
| `XApuEnqueueCommand` | `HRESULT (Handle, const CommandId*, UINT32* total /*opt*/)` | activate (channels), process (one packet → float PCM), reset, notify, deactivate; `0x8AC81000` when a stream's queue is full | | queued to a worker thread; never decodes on the title's thread |
| `XApuDequeueResult` | `HRESULT (Handle, ApuResult*)` | oldest result; `0x8AC81001` "pending", which the title polls | | in command order; try-lock, never waits on the worker |
| `XApuGetResultCount` | `UINT32 (Handle, UINT32* dequeued /*opt*/)` | results ready; total dequeued | | atomics |
| `XApuDisconnect` | `HRESULT (Handle)` | ends the connection | | waits for queued work, frees memory |

Layouts (checked by size in tests): `ConnectInput` 24, `ConnectOutput` 16, `CommandId` 24, `Activate` 56,
`Process` 64, `ApuResult` 80 bytes. Pitch (`target_pitch`, `pitch_ramp`) is **ignored**: no resampler (1.0 reported).

### `chad.dll` (5): the DSP

| function | console signature | meaning | title | here |
|:--|:--|:--|:--|:--|
| `XDspConnectWithMaximumStreamLimit` | `HRESULT (void* mem, UINT32 bytes, UINT32 irSeconds, UINT32 maxStreams, UINT32 maxChannels, Dsp** out)` | connect with DSP memory and limits | 393,216 bytes, 30 s, 128 streams, mono | `xdsp.rs` |
| `XDspActivate` | `HRESULT (Dsp*, const StreamDesc*, const Status** status, Stream** out)` | a convolution (reverb) or other block-transform stream; 512/1024-frame blocks | | status block: submitted, completed ticket, result |
| `XDspSubmitCommand` | `HRESULT (Stream*, const Command*, UINT32* ticket /*opt*/)` | one block: gains, input, output | | completes at once; **output is silence** |
| `XDspDeactivate` | `HRESULT (Stream*)` | | | |
| `XDspDisconnect` | `HRESULT (Dsp*)` | | | |

The DSP's impulse response arrives in the hardware's partitioned frequency-domain layout, which is **unknown**, so
the transform is not computed and reverb returns are dry (same as the fork).

## Built

- `heap.rs` (A2): one reserved host range; device address = `0x00100000 + offset`, so both maps are arithmetic and
  exact for every byte. First-fit free list with merging. Committed in 1 MiB chunks on first use (steady state makes
  no system calls); free memory always reads zero (small blocks cleared on free, large ones get fresh pages).
- `xma.rs` (A3): XMA2 packet/frame walk (packet headers, frames across packets, packet skips, padding), the
  context's two inputs, loop start/end/count with subframe trims and skips, the 256-byte-block output ring with
  write/read blocks and the "writable" bit, error status 4 when both inputs run out. Its own thread; a voice's lock is
  held one block at a time. The frame decoder is the `FrameDecoder` trait (`Hal::set_xma_decoder`); the default
  `Silence` keeps timing, loops and ends exact.
- `acp.rs`: the SHAPE arrays and the coprocessor object (message queue with drop counting, registration mask).
- `xapu.rs`: Opus through `opus-pure` 0.2.5 (BSD-3-Clause, pure Rust).
- `xdsp.rs`, `ring.rs` (lock-free SPSC), `wasapi.rs` (A4: shared mode, event driven, float32, its own thread).
- Audio threads run at `THREAD_PRIORITY_HIGHEST` so a title thread never waits behind a preempted lock holder.

## Measured

```
tests          14 green when measured (18 now, see the header): heap 4, xma 2, acp 4 (layouts, messages, XMA ring, loops + input switch), xapu 2, xdsp 1, ring 1
A4 tone        0.5 s, 440 Hz, amplitude 0.05, 48 kHz stereo: 24,000 frames pushed in 64 pushes, worst push 4.4 us;
               26,400 frames played over 56 periods (device buffer 1,056 frames); device opened in 20 ms
title calls    a4_title_calls_never_block_1ms, 3 runs (median / p99 / worst max):
  ApuAlloc 256 B         0.1 / 0.1 / 13.9 us      ApuFree 256 B        0.1 / 0.1 / 13.7 us
  ApuMap* round trip     0.0 / 0.1 / 12.4 us      ApuAlloc 401 MB      322-416 us (start-up, once)
  XApuEnqueueCommand     0.3-0.5 / 4-17 / 54.7 us XApuDequeueResult    0.0 / 0.1 / 68.1 us
  AcpHal SubmitCommand   0.2 / 3.5 / 57.6 us      XDspSubmitCommand    0.0 / 0.1 / 14.4 us
  worst title-side call  27.6-68.1 us  (< 1 ms: closes A4)
Opus           20 ms stereo packet on the worker: 28-30 us median, 83 us max; enqueue -> result ready 34-37 us mean
XMA thread     64 stereo voices, silence decoder: ~10,100 frames walked, ~80,900 blocks written in ~0.4 s
```

Before the lock and heap fixes the same measurement failed: worst 4.6 ms (`ApuAlloc`, because `free` decommitted
the whole merged free range every time) and 3.8 ms (`SubmitCommand`, waiting for the XMA thread holding a voice
through a whole ring fill).

## A5: our XMA2 decoder

### Built

- `wma/mod.rs`: an XMA2 frame is a WMA Pro frame of 512 samples per channel (mono or stereo, subframes of
  512/256/128, no LFE). Tile header, frame flags, then per subframe: channel transform (mid/side per band),
  quantisation step, scale factors (fresh, or resampled from the last set plus run-level changes), coefficients
  (4/2/1-vector codes until the zeros run long, then run-level), inverse quantisation, inverse MDCT, sine-window
  overlap. Written from the bitstream's description; FFmpeg's `wmaprodec.c` and the fork's `xma.rs` were read as
  references, not copied. Prefix codes decode through a 10-bit lookup table, longer codes by search.
- `wma/tx.rs`: the half inverse MDCT through an n/4-point complex FFT between two rotations; unit-tested against
  the same maths done by direct DFT in f64.
- `wma/tables.rs`: **vendored data**: the WMA Pro code and level tables, from the fork's `xma/tables.rs` (which
  converted FFmpeg's `wmaprodata.h`, LGPL-2.1-or-later). Section 3 header and a `CREDITS.md` line. These cannot be
  derived; any decoder needs these exact tables.
- `awc.rs`: enough of the engine's audio container (`ADAT`) to find each stream's format and data chunks, for the
  tests. Codec 5 is XMA2; codec 11, most of the archive, is the Opus framing `xapu.dll` decodes.
- `acp::Hal` now uses `wma::factory()` by default, so XMA voices play real audio.

### Measured

```
reference    FFmpeg 9.0.2 (xma2 decoder), already on disk (WinGet "Gyan.FFmpeg"); nothing downloaded
input        every XMA2 stream in the title's audio.rpf, read at test time through REBUILD_GAME_DIR (nothing kept;
             the archive's size and modified time checked unchanged)
match        1,818 streams, 832,026 frames, 0 refused; 424,867,971 samples compared:
             322,087 (0.076 %) differ by 1, none by more  -> within +-1 LSB, closes A5
             offset 576 samples in every stream (FFmpeg drops the first frame, 512, and trims 64 more)
speed        one thread, two decoders (a stereo stream's work), 20,092 frames x 5, three runs:
             1,122-1,292x real time at the title's 32 kHz; 12.4-14.3 us per stereo frame
             (= ~750-860x at 48 kHz). Aim was 50x.
```

Commands: `REBUILD_GAME_DIR=<title> REBUILD_FFMPEG=<ffmpeg.exe> cargo test --release -p rebuild-audio -- --ignored
a5_matches --nocapture` (`REBUILD_A5_STREAMS=100000` for all; 64 by default, 8 s) and `-- --ignored a5_speed`.

### Not covered

- **Stereo against a reference.** All of the title's XMA2 streams are mono (1,818 of 1,818; multichannel audio is
  separate mono streams), and nothing can make a stereo XMA2 file (FFmpeg has no XMA or WMA Pro encoder). The stereo
  paths (joint tile headers, mid/side, per-channel quantiser offsets) are written and fuzzed, not compared. The
  speed number above is a stereo stream's work measured as two mono channels.
- The frame header's trim fields are read and not applied: trimming is the XMA context's job (its skip fields).

## For the lead

1. **XMA decoder (A5): done, ours.** Only data is vendored (the code tables, credited). If a stereo XMA stream ever
   turns up (a dump through the XMA context), add it to the reference check.
2. **The title's middleware may open WASAPI itself.** The fork's notes say mixed audio leaves through the host's
   WASAPI, reached through COM like on PC (the import census binds those calls to the host). If so, A4's `Output`
   is not on the title's path; it is ready for anything we mix ourselves (in-game menu sounds, a fallback).
3. **Async XApu.** Results arrive ~35 us after enqueue, not synchronously as in the fork (which decoded inside
   `XApuEnqueueCommand` on the title's thread). The title polls for "pending", so this should hold; if a title ever
   stalls on it, the fallback is to decode inline.
4. **Unknowns kept as unknowns:** `ApuAlloc` flags, `ApuCreateHeap` units, `XApuConnect` option bit 1, the DSP
   impulse-response layout, pitch in XApu (no resampler), SHAPE units other than XMA (commands acknowledged, not run).
5. `D:\triumph\Cargo.toml` (0 bytes, 2026-11-27 13:32) is a stray file a failed shell command of mine created outside
   the repositories; deleting it was refused by the permission check. Safe to delete.

## Next

- A stereo XMA2 reference, if one appears.
- Bind the 19 functions in `crates/runtime` (the lead's).
