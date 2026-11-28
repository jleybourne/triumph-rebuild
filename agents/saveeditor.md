# Lane `saveeditor` — the launcher's save editor

Branch `agent/saveeditor`. Crate `crates/saves` (package `rebuild-saves`), launcher screen
`apps/launcher/src/screens/saves.rs` (tab **Saves**, Ctrl+6), research
[`docs/research/save-format.md`](../docs/research/save-format.md), design
[`docs/design/save-editor.md`](../docs/design/save-editor.md). No GPU, no card run, no title code run:
the format was read from the prepared image with capstone (scripts in the session scratchpad, not in
git), and no title bytes are in git.

## Built

- **Decoded** (each with its exe address): the two blobs ("Save Game Header" 0x128 bytes, "Save Game
  Data"), the header's fields, the ASAV header (0x170 bytes) at the start of the data, all three
  checksums (Jenkins one-at-a-time with sign-extended bytes: data hash, chained header hash, ASAV hash
  seeded `0x4bf672ed`), the kind table, and which kinds (1, 9) are encrypted and carry a 32-byte
  signature from the title's protected crypto library.
- `format`: strict parse / serialize (byte-identical), the title's load checks (`Save::problems`),
  reseal in the writer's order, edits (display name, body bytes in place), `build` (a save made the way
  the writer makes one).
- `section`: one decoded save as JSON and back (schema-strict, checksums recomputed, signed kinds only
  unchanged).
- `store`: listing, backups, the one write path (refuses while the title runs; backup first; the
  runtime's own encode and all-or-nothing write; read back), restore, container export / import with a
  manifest, every save at once (thin wrapper for the storage lane).
- `rebuild-gdk`: `areas::xgamesave::file` (encode, strict decode, path, write) made public so the
  editor writes exactly what the runtime writes (+1 test).

## Measured

```
tests   rebuild-saves 25 · launcher +1 (the screen's actions end to end: open, rename, patch, refused while running, write with backup, restore) · rebuild-gdk +1 (the file API)
```

## Decided

- **Signed kinds are read-only.** Re-signing would mean reversing a protected crypto library and its
  keys; we do not, and the title would refuse anything else.
- **Strict parser.** Refuse what the writer never produces, so accepted = round-trips = edits change only
  their bytes.
- **No new dependencies.** A 300-line strict JSON reader/writer instead of serde.
- **Backups beside the saves folder** (`GameSave.backups`), never inside it, so the runtime never sees them.

## Second pass: real saves (branch `agent/saveeditor2`)

The owner's older build wrote `Player` (kind 3) and `SettingsProfileSettings` (kind 2). Copies are in the
session scratchpad, never in git.

- **The parser takes both real saves.** Every checksum and load check passes, and both blobs and both
  bodies round-trip byte-identically (`savedump`).
- **Resolved:**
  - the stamp is Unix seconds UTC (it equals the old build's version-folder time to the second);
  - the ASAV words +0x154..+0x164 are 0 for kinds 2 and 3;
  - the header's copies match;
  - the ASAV text is the container's name (the gamertag for `Player`).
- **Player body:** count, a word 3, `joaat(entries)`, (id u32, value u32) × count, then zeros. The ids are
  numeric and unnamed; values are editable.
- **Settings body:** the engine's parser-tree JSON (`0x1440692c0`), with member keys
  `"$0x<joaat(name)>"`.
  - 53 of 219 key hashes are named (each checked by joaat); 166 stay `#hash`.
  - All 260 values have typed editing that keeps the body's length; `$v`, versions and `$data` are
    read-only.
- **Import from Triumph** (`triumph.rs`): reads the fork's `<name>~<fnv>\v<N>\{0,1,manifest}`
  (highest committed version), checks everything, then writes through `write_container`. The fork's
  folder is never touched (a test snapshots it).

## Third pass: one Import (branch `agent/saveeditor3`)

The owner asked for one **Import**, not "Import from Triumph". `import.rs` has `detect` (what a file or
folder is), `plan` (checked, nothing written) and `apply` (plans again, must match, then backup and
write). It recognises:

- a storage export zip (handed to `rebuild_storage`);
- one `.gsc` with its `.gsc.json`;
- a decoded-save JSON;
- a folder of per-container exports;
- older-build saves at any level (`triumph::find`).

Anything else gives "not a save or export we recognise". A folder holding both exports and
older-build saves is refused as ambiguous.

The Saves screen now has one Import (a file or a folder, or drop either on the window). It shows a
preview of each container (added, replaced or unchanged), then **Back up and import**. The old
per-container Import, Import JSON, Import all and Import from Triumph buttons are gone. Export is
unchanged.

The storage export preview (`import_plan_view`) is shared with Diagnostics → Storage.

```
tests   rebuild-saves +8 (import: each kind, older build at 5 levels, unknown/ambiguous, stale plan) · launcher +1 (import_previews_then_writes)
```

## Next

- names for the profile's ids and for the remaining settings members; a progress save (kinds 1/9,
  read-only) when one exists.

## For the lead

1. Root `Cargo.toml` `members` gained `crates/saves` (one line). The launcher depends on `rebuild-saves` and
   `rebuild-gdk`.
2. `crates/gdk/src/areas/xgamesave.rs` gained `pub mod file` (no behaviour change) — a line in the gdk lane's file.
3. For the storage lane: replace `rebuild_saves::store::saves_root` with `storage::saves_dir(game, user)` and
   `store::export_all` / `import_all` with `storage::export` / `storage::import` when they land.
