# Save editor (launcher screen "Saves")

Lane `saveeditor`, 2026-11-28. Format: [`docs/research/save-format.md`](../research/save-format.md).
Code: `crates/saves` (package `rebuild-saves`, no dependencies beyond our `rebuild-gdk` and
`rebuild-vfs`) and `apps/launcher/src/screens/saves.rs`.

The editor is for **offline single-player saves**. It never talks to a network (no network crate in its
dependency tree), and never writes a container whose name looks online-related.

## Screen

```
Saves                                                    [the title is running: read-only]   (when it is)
┌ Containers ──────────────[⟳][📂]┐ ┌ SettingsProfileSettings ─────────────[Export][Import]┐
│ user 0009000000000001 · <scid>  │ │ display name, modified, file, blobs (name, size)      │
│ ▸ Player        time · size · n │ ├ Save header ───────────────────────────────────────────┤
│ ▸ SettingsProf… time · size · n │ │ ✓ the title's load checks pass / ✗ + each failure      │
│ ...                             │ │ version, data size + hash, header hash, stamp, name    │
├ Every save at once ─────────────┤ │ ASAV kind / word / flags, text, values, hash, body     │
│ [Export all] [Import all]       │ ├ Edit (unsigned kinds only) ───────────────────────────┤
└─────────────────────────────────┘ │ display name [....] [Set]                              │
                                    │ body bytes  offset [..] hex [....] [Patch]             │
                                    │ [Write] [Discard]             [Export JSON][Import JSON]│
                                    ├ Hex: blob ▾, pages ───────────────────────────────────┤
                                    ├ Backups (undo): newest first, [Restore] each ─────────┤
                                    └────────────────────────────────────────────────────────┘
```

- **List**: every `<user>/<scid>/<container>.gsc` under the game's saves folder, with the container's
  own time, the file size and the blob count. Files that do not decode are listed as unreadable and
  cannot be opened for editing.
- **Open**: the container's blobs. When it holds `"Save Game Header"` and `"Save Game Data"` they are
  decoded (both headers, all three checksums judged exactly as the title's load path judges them,
  warnings for fields the title writes in step but does not check). Anything else is hex only.
- **Edit**: only what the research proves: the display name (kept in both places the writer keeps it)
  and body bytes replaced in place (never grown or shrunk). Every edit reseals: ASAV hash, then data
  size and hash, then header hash, in the writer's order (`Save::reseal`). Edits stay in the editor
  until **Write**.
- **Read-only** when: the save's kind is 1 or 9 (encrypted and signed by the title's protected crypto
  library: no edit could be made acceptable), the save already fails the title's checks, the parser
  refuses the blobs, the container is not a save container, or the title is running.

## Safety

| rule | how |
|:--|:--|
| never edit in place | an edit changes a copy in memory; **Write** produces a whole new container |
| backup first | `store::write_container` copies the current file to `GameSave.backups/<user>/<scid>/<file>.<UTC stamp>.gsc` (create-new, flushed; never overwrites a backup) before anything else is written |
| all or nothing, the runtime's way | the write is `rebuild_gdk::areas::xgamesave::file::write`: beside the file, flushed, renamed over. The bytes are the runtime's own `file::encode`, re-decoded before writing and read back after |
| not while the title runs | the launcher passes `is_running()`; checked before and again just before the rename. Also refused while a runtime update is half written in that folder (a `.gsc.tmp`) |
| never write what the title would refuse | `store::refuse`: a container name the runtime would reject, a duplicate blob name, save blobs the parser does not understand, or save blobs that fail the title's load checks |
| undo | **Restore** writes a backup back through the same path (so the version it replaces is backed up too) |
| online state | `store::looks_online` names (online, multiplayer, mp…, network, social, cloud) are never written |

The parser is strict on purpose: it refuses what the title's writer never produces (wrong header
size, bytes after a name's terminator, the reserved ASAV bytes non-zero, …). Whatever it accepts
serializes back to the same bytes, so an edit changes only the bytes it means to.

## Import and export

| what | export | import |
|:--|:--|:--|
| one container | `<file>.gsc` (the bytes as stored) and `<file>.gsc.json`, a manifest: container, display, time, user, scid, file size and FNV-1a 64, each blob's name, size and FNV-1a 64 | manifest required; every field must match the file; the container must decode strictly and pass `store::refuse`; written into the open container's folder through `write_container` (backup first, not while running) |
| one decoded save (a "section") | JSON: header fields, ASAV fields (floats exact, 64-bit values as hex text), the body as hex, the current checksums | every key required, unknown keys refused, types and ranges checked, the kind and the body length cannot change; unsigned kinds are resealed and must pass the title's checks; a signed kind is accepted only if nothing changed. The result goes into the editor; **Write** saves it |
| every save of the game | a folder `saves-<stamp>/<user>/<scid>/` of per-container exports | every container checked first; nothing is written unless all pass |

Exporting and importing with nothing edited gives back the same bytes, for a container and for a
section (tests). A flipped byte, a manifest that does not match, a missing manifest, an unknown JSON
field, an invalid value or an import while the title runs is refused and changes nothing (tests).
FNV-1a only catches accidents; a deliberately forged pair still has to pass the save's own checksums
and the title's checks.

**The storage lane.** Paths come from one function, `rebuild_saves::store::saves_root(game)` (today
`<overlay>\GameSave`, the runtime's `saves_host`); it becomes `storage::saves_dir(game, user)` when that
lane lands. `store::export_all` / `import_all` are the thin wrappers that become `storage::export` /
`storage::import` (one archive of all saves and settings).

## Typed fields (second pass, from real saves)

There is a **Settings** / **Profile entries** card under Edit for the two kinds real saves use
(research §4):

- **`SettingsProfileSettings` (kind 2, JSON):**
  - one row per value, named where the member's hash is known and `#hash` where it isn't, with a
    filter box;
  - booleans are checkboxes; numbers are text fields checked against their type, and floats keep
    their decimals;
  - `$v`, the version words and `$data` are read-only;
  - the body keeps its length: a shorter value is padded with spaces, and a longer one takes its room
    from its line's indentation or is refused.
- **Player profile (kind 3):** one row per (id, value) entry. The value is edited in place and the
  entry hash recomputed.

Every field edit reseals the save (ASAV hash, data hash, header hash) and stays in the editor until
**Write**.

## Import from Triumph

**Import from Triumph** (under "Every save at once") reads the owner's older build's save folders:
`…\victory\data\<title>` or its `saves` folder, the newest committed `v<N>` of each container
(research §7).

- It writes nothing there.
- Every container is checked first (the folder hash, names our runtime uses, `store::refuse`).
- Then each one is written through `store::write_container`: backup first, refused while the title
  runs.

## Not yet

- names for the profile's ids and for the 166 hash-only settings members;
- fields of the ASAV header with unknown meaning are shown, not editable.
