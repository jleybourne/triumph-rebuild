# Design — built-in test profiles

Status: **built 2026-11-27 (owner request: "varying profile settings to test baked into the config and the
launcher"), lane `profiles`.** Code: `crates/config/src/profiles.rs` (`BUILTIN_PROFILES`), the launcher's
Profiles and Play tabs, `rebuild-launcher --matrix`.

## What a built-in is

A named, read-only set of registry values that makes this machine behave like a smaller one, so the same build
can be run as each machine class in turn and a result read against the reference.

- **It sets exactly eight keys** (`PROFILE_KEYS`): `display.width`, `display.height`, `upscaler.backend`,
  `upscaler.mode`, `limits.ram_mb`, `limits.vram_mb`, `limits.cpu_cores`, `limits.cpu_rate`. Every built-in sets
  all eight, so applying one after another leaves nothing of the first behind (a test checks this).
- **Nothing else is touched.** Applying sets those keys (they become explicit) and `profile`; every other
  setting the user set, explicit defaults, unknown keys and mods are kept exactly (the owner's rule: settings
  are never replaced). Only the keys the user chose to apply change.
- **Read-only.** Built-ins are code, never files. `Home::save_profile` refuses a built-in's name in any case. A
  built-in edited after applying shows as "built-in, edited"; the launcher offers "`<name> copy`" as the
  save-as name.
- **Validated against the registry at test time**: every key exists, every value passes `Setting::check`, the
  applied configuration round-trips through `Config::parse` with 0 problems.
- **Stable or not.** `limits.vram_mb` is experimental until Build 5's heaps enforce it, so a profile that sets
  it is not stable: it is marked "experimental" in the launcher and refused (all or nothing, nothing changed)
  by a configuration marked `stable_only`.

## The set

Cores are **logical processors** (`limits.cpu_cores` = "the first N logical processors"). On the dev machine
(i7-10700KF, 8 cores / 16 threads, SMT on) two logical processors are one core, so "N cores" is 2N.
RAM is the title and the layer together (`crates/runtime/src/limits.rs`). "auto" is DLSS on NVIDIA and FSR
on AMD, so a profile means the same machine class on either vendor.

| name | label | output | upscaler | RAM MiB | VRAM MiB | logical CPUs | CPU % | stable |
|:--|:--|:--|:--|--:|--:|--:|--:|:--:|
| `dev-machine` | Dev machine (minimum spec) | 3840 x 2160 | auto quality | 0 | 0 | 0 | 0 | yes |
| `series-x` | Console class (Series X budget) | 3840 x 2160 | auto quality | 13,824 | 10,240 | 14 | 0 | no |
| `series-s` | Console class (Series S budget) | 2560 x 1440 | auto performance | 8,192 | 6,144 | 14 | 0 | no |
| `mid-pc` | Mid-range PC | 2560 x 1440 | auto quality | 12,288 | 8,192 | 12 | 0 | no |
| `low-pc` | Low-end PC | 1920 x 1080 | fsr performance | 12,288 | 6,144 | 8 | 0 | no |
| `stress-ram` | Stress: memory-starved | 3840 x 2160 | auto quality | 8,192 | 0 | 0 | 0 | yes |
| `stress-vram` | Stress: VRAM-starved | 3840 x 2160 | auto quality | 0 | 4,096 | 0 | 0 | no |
| `stress-cores` | Stress: few cores | 3840 x 2160 | auto quality | 0 | 0 | 4 | 0 | yes |
| `stress-cpu` | Stress: CPU-capped | 3840 x 2160 | auto quality | 0 | 0 | 0 | 25 | yes |

0 = no limit.

### Why each value

- **`dev-machine`** is `plan.md`'s minimum spec: 3840 x 2160, DLSS Quality (2560 x 1440 internal, the console's
  own internal resolution), RTX 4070 Ti, no limits. It is the reference every other run is read against.
- **`series-x`**: the console gives a title **13.5 GiB** (13,824 MiB) of its 16 GiB; **10 GiB** of that is the
  fast GPU-optimal pool, so 10,240 MiB VRAM. The console has 8 Zen 2 cores but reserves one for the system, so a
  title sees **7 cores / 14 threads**: 14 logical processors (on the dev machine, 14 of 16). Output 4K with a
  quality upscale, as the task asked. No CPU-rate cap: the rate cap slows every thread evenly and isn't a model
  of the console's lower clock; a run reads the clock difference from `stress-cpu` instead.
- **`series-s`**: about **8 GiB** for the title (of 10 GiB), and roughly **6 GiB** of that is GPU data (the
  estimate leaves ~2 GiB for CPU-side data; the console has one pool, so this is a judgement, marked as one).
  The same 7-core CPU (14 logical). The console's usual output is 1440p, with a performance upscale.
- **`mid-pc`**: the task's 1440p, 6 cores (12 logical), 8 GiB card, 16 GiB RAM. A 16 GiB PC leaves a game about
  **12 GiB** after Windows and the background, so `limits.ram_mb` = 12,288: the limit is what the title and
  layer may use, not what the machine has.
- **`low-pc`**: 1080p, 4 cores (8 logical), a **6 GiB** card (the upper end of the 4-6 GiB asked: a 4 GiB
  card is below what the title's Series S budget gives its GPU data, and `stress-vram` covers 4 GiB on its own),
  16 GiB RAM (12 GiB for the game), **FSR Performance** explicitly (the vendor-neutral path low-end cards use).
- **Stress profiles** change **one limit** from `dev-machine` and nothing else (a test checks the output and
  upscaler equal the dev machine's and exactly one limit is in force), so a failure can be blamed on one cause:
  - `stress-ram` 8 GiB: the Series S title budget, so a `series-s` failure can be split into "memory" or not.
  - `stress-vram` 4 GiB: a 4 GiB card; experimental until Build 5 enforces the VRAM limit.
  - `stress-cores` 4 logical processors: two cores with both threads. Where the title's worker threads queue.
  - `stress-cpu` 25 %: CPU time capped with every core still visible; a slow CPU without fewer threads, the
    complement of `stress-cores`.

## The launcher

- **Profiles tab**: a "Built-in test profiles" card lists every built-in with a "built-in" pill, an
  "experimental" pill when it isn't stable (hover names the key), its purpose line and a one-line summary
  (output, upscaler, limits); **Apply** sets it (Re-apply when edited). User profiles are listed below as before;
  Save profile refuses a built-in's name.
- **Play tab**: the hero line names the active profile ("Low-end PC (built-in)", "… (built-in, edited)", or
  the user's name), and a second line lists the **limits in force** ("RAM 12 GiB · VRAM 6 GiB (from Build 5) ·
  8 logical processors") or says there are none. Both are computed while the page is drawn; nothing new asks
  for frames, so the idle launcher still asks for none (the U6 idle tests).
- Applying does not save: like loading a profile, the save bar appears and Save makes it active.

## The test matrix

`rebuild-launcher --matrix <dir> [--profiles a,b,...]`, or **Write test matrix…** on the Profiles tab, writes
`<dir>/<name>.toml` for each built-in (all, or the named ones): the selected game's settings with that profile
applied (`profiles::matrix_config`), and `diagnostics.frame_log_path` set to `<dir>/<name>.frame-line.log` so
the runs in turn don't overwrite each other's frame line. A matrix file is a test file, so a profile that isn't
stable clears `stable_only` in it rather than being refused. Every file is checked by `Config::parse` (0
problems) before any is written; an unknown name writes nothing. Nothing is run.
