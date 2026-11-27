# Lane `profiles` — built-in test profiles in the config and the launcher

Owner request 2026-11-27: "varying profile settings to test baked into the config and the launcher".
Design and the reason for every value: [`docs/design/test-profiles.md`](../docs/design/test-profiles.md).

## Built

- `crates/config/src/profiles.rs`: `BUILTIN_PROFILES` (9), `PROFILE_KEYS` (the 8 keys every built-in sets),
  `builtin(name)`, `Builtin::{apply, matches, stable, experimental_keys, summary}`, `Active::of` (built-in,
  built-in edited, user), `limits_in_force`, `matrix_config`. Re-exported from `rebuild_config`.
- `apps/launcher`: `Model::apply_builtin`; `Home::save_profile` refuses built-in names; `home::write_matrix`;
  Profiles tab card listing the built-ins (built-in / experimental / active pills, purpose, summary, Apply,
  Write test matrix…); Play tab shows the active profile and the limits in force;
  `rebuild-launcher --matrix <dir> [--profiles a,b]`.

## Measured / tested

- config: `every_builtin_validates_against_the_registry`, `the_required_set_is_there_with_its_key_values`
  (stress profiles limit exactly one thing), `profiles_with_experimental_keys_are_not_stable_and_refused_by_a_stable_config`,
  `applying_keeps_every_other_explicit_setting`, `editing_a_builtin_is_a_copy_and_the_builtin_is_unchanged`,
  `matrix_configs_parse_and_keep_the_base`.
- launcher: `applying_a_builtin_then_saving_keeps_unrelated_settings`, `builtins_cannot_be_overwritten`,
  `the_matrix_writes_one_parsing_file_per_builtin`, and the headless smoke flow now applies `low-pc` on a game,
  draws Profiles and Play, writes the matrix, saves, switches away and back.
- Idle: no new repaint requests; the U6 idle tests are unchanged and green.

## For the lead

1. `limits.cpu_cores` is logical processors. The console profiles use 14 (7 title cores x SMT), which on the
   dev machine (16 logical) is a small limit; on an SMT-off host it would be 14 cores. If the lead wants a
   "physical cores" option, it is a registry change (`crates/config` REGISTRY), not made here.
2. The console-class profiles have no CPU-clock model: `limits.cpu_rate` slows every thread evenly and is kept
   for `stress-cpu` only. A per-core frequency cap (Windows CPU sets / power throttling) would be a runtime change.
3. `series-s` VRAM (6 GiB) is a judgement: the console has one 8 GiB title pool; 6 GiB leaves ~2 GiB for
   CPU-side data. Correct it from a run once Build 5 enforces VRAM.
4. Matrix files set `diagnostics.frame_log_path` (per profile). The registry's help says a non-empty path wins
   over `REBUILD_FRAME_LOG`; nothing reads the key yet, so C2 should honour it, or matrix runs share one log.
5. No screenshots: rendering the launcher opens a GL window (no GPU work allowed in this lane). The Profiles and
   Play pages are exercised headless in the smoke test.
