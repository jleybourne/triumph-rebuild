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

## Next (needs a real save)

- read one: `SettingsProfileSettings`' and `Player`'s kinds, the stamp's unit, the body layout; then typed
  fields for the settings body.

## For the lead

1. Root `Cargo.toml` `members` gained `crates/saves` (one line). The launcher depends on `rebuild-saves` and
   `rebuild-gdk`.
2. `crates/gdk/src/areas/xgamesave.rs` gained `pub mod file` (no behaviour change) — a line in the gdk lane's file.
3. For the storage lane: replace `rebuild_saves::store::saves_root` with `storage::saves_dir(game, user)` and
   `store::export_all` / `import_all` with `storage::export` / `storage::import` when they land.
