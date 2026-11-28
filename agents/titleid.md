# Lane `titleid` — the title id from the package, and the saves' scid

Branch `agent/titleid`. Answers `agents/setup.md` "For the lead" 2 and gdk.md "For the lead" 1.

## Built

- `rebuild_storage::Identity::title_id`: the manifest's `<TitleId>` (1-8 hex digits, `0x` optional; anything else is
  `None`). `Drives::in_store` reads it with the package identity; `drives::install` sets `Profile::title_id`, so
  `XGameGetXboxTitleId` answers 0x7952987a and the title opens its saves with scid
  `00000000-0000-0000-0000-00007952987a`, the scid the older Triumph build used.
- `drives::adopt_zero_scid`, at start, per user folder `<saves>\<user>\`: only `00000000-…-000000000000` → renamed to
  the real scid; both → both kept, one `saves` line on stderr (the title reads the real one); an empty zero folder
  (a run with the title id unset creates one) → removed, not a conflict.
- Test: `zero_scid_saves_follow_the_title_id_and_a_conflict_keeps_both`, plus title id cases in the identity tests.

## Measured (2026-11-29)

| run | what | result |
|:--|:--|:--|
| warp1 | WARP, 60 s, `REBUILD_TRACE_SAVES=1` | `saves moved from 00000000-…-000000000000 to 00000000-…-00007952987a (title id 0x7952987a)`; provider scid `…7952987a`; `Player` 2 blobs, `SettingsProfileSettings` 2 blobs read; no device removal |
| card1 | card, 90 s, no A pressed | the same reads under the new scid; engagement screen shows `ACCOUNT PICKER (Y) · PLAYER (A)`, no narration item (the saved profile is loaded; setup skipped, per `agents/setup.md`); nvlddmkm 453 → 453 |

Between the two runs another lane's master-build run recreated an empty zero-scid folder (00:04:54), which card1
reported as "both hold saves"; that is why an empty zero folder is now removed. nvlddmkm went 451 → 453 at
00:10:40 and 00:11:40, during another lane's card run, before card1 started.

## For the lead

1. Any run on a build without this change opens (and creates) the zero-scid folder and finds no saves, so it shows
   first-run setup again and may write fresh saves there. Once merged, those are kept beside the real ones and logged.
