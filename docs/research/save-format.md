# The title's save format

Lane `saveeditor`, 2026-11-28. Everything here was read from the title's own code in the prepared image
(`%LOCALAPPDATA%\triumph-rebuild\images\game_scarlett_master-….dll`, the loader's output), with
capstone, the way the `gdk` lane read the save thunks. Addresses are the image's virtual addresses
(base `0x140000000`). No leaked material, no other title's code, and no title bytes in git: the
constants below are the few numbers the format needs, each cited.

**Status.** The container layout, both headers, all three checksums and which saves are encrypted
and signed are decoded from the code and implemented in `crates/saves` (tests build saves the way
the writer does and check them the way the reader does).

**Confirmed on real saves** (second pass, 2026-11-28). The owner's older build (the fork) wrote two
real containers, `Player` (kind 3) and `SettingsProfileSettings` (kind 2). The parser takes both: all
three checksums and every load check pass, and both blobs and both bodies serialize back to identical
bytes (`savedump`, §4). Both bodies are now decoded (§4) and their fields are editable. Copies of the
owner's files are not in git; tests use synthetic saves of the same form.

## 1. Containers and blobs

The title reaches XGameSave through 20 thunks (`0x1455df210`-`0x1455dfcf4`, `agents/gdk.md`). Its
save manager writes one save as **two blobs in one update** (`0x1428d1ea0`):

| blob | size | written at |
|:--|:--|:--|
| `"Save Game Header"` (`0x14677e1da`) | 0x128 | `0x1428d1f9f` (`XGameSaveSubmitBlobWrite`, r9d = 0x128) |
| `"Save Game Data"` (`0x146785e6e`) | `[mgr+0x758]` | `0x1428d1fc4` |

and submits them with `XGameSaveSubmitUpdateAsync` (`0x1428d1fe9`). So both land, or neither does.

The names it uses:

- **Slot enumeration** (`0x1428d12e0` → callback `0x1428d1340`): mode `[mgr+0x2a4]` 0 takes containers
  whose name does not start with `PARC` (`0x1467881a8`) or `GA`; mode 1 takes `PARC…`; mode 2 takes
  `GA…`. Names are cut at 41 bytes (`0x1428d13f2`). For each, the listing reads **only the header**
  (`0x1428d1480`: `XGameSaveEnumerateBlobInfoByName` + `XGameSaveReadBlobData` of "Save Game Header")
  and keeps its stamp (+0x10), +0x18 and display name (+0x24) for the slot list (`0x1428d1649`-`0x1428d16f6`).
- **One container by name** (`0x1428d2e50`: `XGameSaveGetContainerInfo` with `[mgr+0x438]`, then
  `0x1428d33a0` → `XGameSaveReadBlobDataAsync` of both blobs, `0x1428d2be0`; the result is checked in
  `0x1428d3410`). The card runs show the two names asked for this way: `Player` and
  `SettingsProfileSettings`. Neither string is in the image as such (searched ASCII and UTF-16); the
  names are built at run time. `Profile Settings` (`0x14677bf32`, `0x1428bbbfc`) is the fallback
  display text of a localized string (`0xf84abd96`).
- There is also a **local-storage path** (`0x1428d2200`): the same data blob (with its ASAV header and
  signature, through `0x1428d2030`) written as a file `"%s/%s"` (`0x14677cd9f`) under persistent local
  storage. It has no outer header. Not seen in a run yet.

## 2. "Save Game Header" (0x128 bytes)

Built on the stack at `rsp+0x28` in `0x1428d1ea0`, after `0x1428d17b0` and a 0x128-byte `memset`
(`0x1428d1ec7`): every byte not written below is 0.

| offset | type | value | from |
|:--|:--|:--|:--|
| 0x00 | u32 | **6**, the version | `0x1428d1ed8` |
| 0x04 | u32 | data blob size, `[mgr+0x758]` | `0x1428d1ef1` |
| 0x08 | u32 | **data hash**: `joaat(data, size, seed 0)` | `0x1428d1f5b`, stored `0x1428d1f68` |
| 0x0c | u32 | **header hash** (below) | `0x1428d1f6b`, stored `0x1428d1f7a` |
| 0x10 | u64 | stamp: the result of a call through the pointer at `0x1479f75a8` (argument `0x1479f7560`) | `0x1428d1f23`, `0x1428d1f29` |
| 0x18 | u32 | `[mgr+0x404]` (= ASAV +0x154) | `0x1428d1f30` |
| 0x1c | u32 | `[mgr+0x2c8]` (= ASAV +0x18) | `0x1428d1ef4` |
| 0x20 | u32 | `[mgr+0x2cc]` (= ASAV +0x1c), or the data size if that is 0 | `0x1428d1efd`-`0x1428d1f08` |
| 0x24 | u16[128] | display name, `wcsncpy`-style: at most 127 units and a terminator | `0x1428d1f13` → `0x1428bd160` |
| 0x124 | 4 bytes | 0 | the memset |

The stamp's function pointer is filled at run time behind a protected call (`and rcx, ~0xf` before an
indirect call). **The stamp is seconds since 1970, UTC** (real saves): `Player` holds `0x6b077c94` =
2026-11-26 00:25:56 UTC. Its `v7` folder in the old build was written at 10:55:56 +10:30, the same
second. `SettingsProfileSettings` holds `0x6b07786d` = 00:08:13 UTC, the same second as its folder.
The same value goes into ASAV +0x20 (`0x1428d204b`). Both real saves have it equal in both places.

**Header hash** (`0x1428d1930`): Jenkins one-at-a-time, chained, each call finalised (`0x144dd1900`),
over the header's own bytes in this order:

```
h = joaat(hdr[0x10..0x18], 0)        stamp
h = joaat(hdr[0x24..0x124], h)       name, all 256 bytes
h = joaat(hdr[0x18..0x1c], h)
h = joaat(hdr[0x1c..0x20], h)
h = joaat(hdr[0x20..0x24], h)
h = joaat(hdr[0x08..0x0c], h)        the data hash
```

Version, size and the header hash itself are outside the chain; version and size are checked on their own.

**The hash** (`0x144dd1900` → loop `0x144dd1920`): `h = seed; for each byte: h += (i8)byte;
h += h << 10; h ^= h >> 6`, then `h += h << 3; h ^= h >> 11; h += h << 15`. The byte is sign-extended
(`movsx r9d, byte ptr [rcx + r8]`, `0x144dd1930`), which matters for bytes ≥ 0x80. This is Bob Jenkins'
one-at-a-time hash ([Wikipedia: Jenkins hash function](https://en.wikipedia.org/wiki/Jenkins_hash_function);
"a" → 0xca2e9442 is a test in `crates/saves`); RAGE-family titles are publicly known to use it under
the name "joaat", which was the hint to look for it. No other public write-up was relied on.

## 3. "Save Game Data": the ASAV header (0x170 bytes) and the body

`0x1428d2030` prepares the data before the header is built: it refuses data of 0x16f bytes or less
(`0x1428d2040`), sets the ASAV object's stamp, header size and data size, and writes the ASAV header to
the start of the data (`0x1428d0a60`). The object lives at `[mgr+0x2b0]`; its constructor
(`0x145848280`) zeroes it and sets +0 to `0x256415341` (`"ASAV"`, then 2) and the kind (`0x1458482c0`).
The writer copies every byte at its own offset, and the reader (`0x1428d0450`) reads them back at the
same offsets, so the stored header and the in-memory object have one layout:

| offset | type | meaning | write / read |
|:--|:--|:--|:--|
| 0x00 | 4 bytes | `ASAV`, compared byte by byte | `0x1428d0a88` / `0x1428d0463`-`0x1428d048b` |
| 0x04 | u32 | 2 (format); copied, not checked | `0x145848299` / `0x1428d0491` |
| 0x08 | u32 | **kind**, 1-9 (`(k-1) < 9`) | `0x1458482c8` / `0x1428d04c6` → `0x1428d0a50` |
| 0x0c | u32 | must equal `table[kind-1]`, table `0x1462fe8ac` = 3,1,1,1,1,1,1,2,1 | `0x1458482d2` / `0x1428d04e9` → `0x141b1d2b0` |
| 0x10 | u32 | flags; the writer ORs 0x80 and 0x100 for kinds 1 and 9 | `0x1428d0abe`, `0x1428d0ad2` |
| 0x14 | u32 | 0x170, the header size | `0x1428d2052` |
| 0x18, 0x1c | u32 | copied to the outer header's +0x1c, +0x20; +0x1c = data size if 0 | `0x1428d206c` |
| 0x20 | u64 | stamp (as outer +0x10) | `0x1428d204b` |
| 0x28 | u16[128] | display name (outer +0x24 is copied from here) | `0x1428d0300` |
| 0x128 | char[43] | text of at most 42 bytes and a terminator; **+0x153 must be 0** | `0x1428d02c0` (copy ≤ 0x2a) / `0x1428d0946` |
| 0x154 | u32 | set by the kind's caller (kind 1: `0x1428d03da`) | |
| 0x158 | f32 | likewise (`0x1428d03e0`) | |
| 0x15c | u32 | | |
| 0x160 | u32 | **ASAV hash** | `0x1428d0cf8` / `0x1428d0a2b` |
| 0x164 | f32 | | |
| 0x168 | u32 | data size | `0x1428d2072` |
| 0x16c | 4 bytes | **must be 0** | `0x1428d0a7b` / `0x1428d09e2`-`0x1428d0a0c` |

The reader also requires the header to end exactly at +0x170 (`0x1428d0a17`). **ASAV hash**
(`0x1428d0410`): `joaat(object[0..0x170] with +0x160 zeroed, seed 0x4bf672ed)`.

Kinds the title creates directly: 1 (`0x1428d03c9`, with +0x154/+0x158 from its caller), 5
(`0x142893a56`), 9 (`0x1428a01e9`); two more callers pass the kind from a request object
(`0x1428bcedc`, `0x1428bcfc7`). A slot class maps to a kind in `0x14289bad0`: class 3 → 8, class 4 →
9, others → 1.

### Kinds 1 and 9: encrypted and signed

`0x1428d0d60` is true for kinds 1 and 9 only (`(k-1) & ~8 == 0`). For those:

- **Signature.** `0x1428d2030` (`0x1428d208e`-`0x1428d20e3`) asks the save crypto service (a singleton
  at `0x1479f7530`, vtable `0x1462fe5b0`, set up in `0x141cfb380`) for its signature size (slot 0x48,
  `0x1414e78f0`: **0x20**) and has it sign `data[0 .. size-0x20]` into the last 0x20 bytes (slot 0x38,
  `0x1428d4480`: hash object `0x1466fdcb0`, key from table `0x147bdc018` id 4, init / update / final
  `0x145603f60` / `0x145604100` / `0x145604180`). Verification is slot 0x40 (`0x1428d4530`, key id 5).
  The signature covers the ASAV header too, so for these kinds nothing in the data can change.
- **Encryption** (inferred: a keyed transform over 16-byte-padded data; the cipher is not identified).
  `0x14289bad0` checks there is room to pad (slot 0x20, `0x1428d4400`; slot 0x50 `0x1428d45e0` pads to
  a multiple of 16), takes the new size (slot 0x28), and runs slot 0x18 (`0x1428d4390`, key descriptor
  `0x1462fda90`) over the region; slot 0x08 (`0x1428d42f0`, same descriptor) is its pair.
  `0x14289bbf0` checks room for the signature (slot 0x30, `0x1428d4450`) and grows the size by 0x20.
  Both run only for kinds 1 and 9 (`0x14289baf6`, `0x14289bc16`).
- The crypto library is protected (its own status codes, e.g. success `0x21a4eec3`, `0x145603f75`;
  key blobs with a magic `0x65f27725`, `0x145603fbf`; a 0x538-byte context, `0x145603ffb`). We do not
  reverse it, extract its keys, or try to re-sign: the editor shows these saves read-only. Kinds 2-8
  carry no signature and no encryption.

### What the title checks on load

`0x1428d3410` (the result of reading one named container's two blobs), in order:

1. the header blob is exactly 0x128 bytes (`0x1428d3513`), the data blob is present and non-empty;
2. header hash (`0x1428d3547`-`0x1428d3555`), version 6 (`0x1428d3557`);
3. header size field = data blob size (`0x1428d355e`), and ≤ the manager's buffer `[mgr+0x758]`
   (`0x1428d3564`; that capacity is not known offline);
4. data hash (`0x1428d3578`-`0x1428d357d`);
5. data ≥ 0x171 bytes (`0x1428d3583`);
6. the ASAV header (`0x1428d0450`): magic, kind, kind word, +0x153 = 0, +0x16c..+0x170 = 0, ASAV hash.

The slot listing (`0x1428d18e0`) checks only size 0x128, the header hash and version 6.
`crates/saves` (`Save::problems`) implements every check except the buffer capacity; the editor never
grows a save, so that one cannot start failing because of an edit.

## 4. The real saves: `Player` and `SettingsProfileSettings`

The owner's older build wrote two containers. Read with `savedump` (`crates/saves/src/bin/savedump.rs`,
which only reads):

| | `Player` | `SettingsProfileSettings` |
|:--|:--|:--|
| data / header blobs | 2012 / 0x128 bytes | 7344 / 0x128 bytes |
| header | v6, data hash `0xaca14687`, header hash `0xf0514e8a`, name "Player" | v6, `0x02a2bdc3`, `0x9031fdb2`, "Profile Settings" |
| ASAV | format 2, **kind 3** (word 1), flags 0, header size 0x170, text "Player" | format 2, **kind 2** (word 1), flags 0, text "SettingsProfileSettings" |
| load checks (§3) | all pass | all pass |
| parse → serialize | both blobs byte-identical; body byte-identical | both blobs byte-identical; body byte-identical |

Both are unsigned kinds, so both are editable.

**Open items resolved by these saves:**

- **The five undecoded ASAV words:** ASAV +0x154, +0x158, +0x15c and +0x164 are 0 in both saves. They
  are set only by the kind-1 caller (`0x1428d03da`). The editor shows them and does not edit them.
- **ASAV +0x18 and +0x1c:** +0x18 is 0, and +0x1c is 0 on the way in, so the writer stores the data
  size (`0x1428d206c`).
- **The header's copies match.** Outer +0x18 = ASAV +0x154 = 0, outer +0x1c = ASAV +0x18 = 0, outer
  +0x20 = ASAV +0x1c = the data size. The names and stamps match too. The editor's warnings are empty
  for both.
- **The ASAV text** (+0x128) is the container's name. For `Player`, the gamertag: the container is
  named after the gamertag ("Player" is the default gamertag).

### Kind 3: the player profile (`Player`)

The body (2012 − 0x170 = 1644 bytes) has a 12-byte head, then (id, value) pairs, then zeros:

| offset | type | meaning |
|:--|:--|:--|
| +0 | u32 | entry count (10) |
| +4 | u32 | 3 in the real save. Meaning unknown; kept as it is |
| +8 | u32 | `joaat(entries, seed 0)`: the hash over the `count × 8` entry bytes. Reproduced exactly from the real save (`0x09b132df`) |
| +12 | (u32 id, u32 value) × count | the entries |
| then | zeros | to the end of the body |

The image has one joaat over `count × 8` bytes of 8-byte entries with a 16-bit id in front: `0x1453610f3`
in `0x145361030`. That is probably this hash's producer, but I have not confirmed it.

The ids are not named: they are a numeric enum and no name for them is in the image. Entries in the
real save:

| id | value |
|:--|--:|
| `0x00be` | 28 |
| `0x00c8` | 3072 |
| `0x00bf` | 28 |
| `0x00e6` | 32 |
| `0x04b0` | 1 |
| `0x0460` | 0 |
| `0x0461` | 3 |
| `0x0462` | 0 |
| `0x0463` | 0 |
| `0x0464` | 0 |

`body::Profile` parses this strictly: the count must fit, the hash must match and the tail must be zero.
It edits values in place and recomputes the entry hash. Entries are never added, so the body keeps its
length.

### Kind 2: `SettingsProfileSettings`

The body is **JSON text that fills the body exactly** (6976 bytes, no terminator). It is written by the
engine's parser-tree JSON writer, `0x1440692c0`:

- member keys are written as `"$0x%08x":` with the member's name hash (`0x14406946a`, string
  `0x146788bb3`) when names are stripped (flag 8 at +0x25, `0x144069439`); otherwise as `"%s":`;
- a space follows the colon (`0x14406949b`), and `,` separates members (`0x144069392`);
- each member starts a new line with CR LF (`0x144069317`, `0x144069343`) and tab indentation
  (`0x144067160`);
- other keys are literal: `"$v"` (a section's version: 2.0 for audio, 5.0 for controls), `"$x"`/`"$y"`
  (a 2-vector) and `"$data":` (a hashed string, `0x146788c08`, written by `0x14406c3d0`);
- floats are written with 6 decimals (`1.000000`), the `$v` versions with one (`2.0`).

The name hash is joaat (§2), so names can be checked one by one. 53 of the 219 distinct keys have a
name. Each name was found as a word in the image's strings, in `settings.xml`, or in a list of plausible
setting words (camelCase combinations), and confirmed by its 32-bit joaat. About 2.5 million candidates were tried against
219 hashes, so about 0.1 accidental matches are expected in total. Every match is a setting word that
fits its section (`musicLevel` under `audio`, `invertLook` beside `invertMove`, …).

The file has 260 values: 19 under `audio`, 48 under `camera`, 79 under `controls`, 95 under `display`,
17 under `general`, plus the root's two version words. Each section carries `versionMajor` 6 and
`versionMinor` 0. The values named in full:

| field | type | real save |
|:--|:--|:--|
| `audio.sfxLevel` · `uiSfxLevel` · `dialogueLevel` · `musicLevel` | int | 10 each |
| `audio.gpsAudio` | int | 3 |
| `camera.thirdPerson.cameraBob` | int | 10 |
| `camera.firstPerson.fov` | int | 13 |
| `camera.firstPerson.vehiclefov` | int | 10 |
| `camera.firstPerson.autoLevel` | bool | true |
| `controls.{thirdPerson,firstPerson}.lookAcceleration` · `aimAcceleration` | int | 7 each |
| `controls.#e8c91000.invertLook` · `invertMove` · `invertMap` | int | 0 each |
| `controls.#65b0928e.vibration` | int | 10 |
| `controls.accessibility.holdAssist` · `aimMode` | int | 0 · 0 |
| `display.sharpenIntensity` | float | 1.000000 (`settings.xml` `advancedGraphics/sharpenIntensity` is also 1.000000) |
| `display.colorblindMode` · `radarBlipSize` · `mapBlipSize` | int | 0 each |
| `display.#3588d43d.money` | bool | true |
| `display.#b8462278.radarMode` · `compassMode` · `radarSize` | int | 1 · 2 · 0 |
| `display.#b8462278.gps` · `altimeter` | bool | true · true |
| `display.hud.subtitles` · `subtitlesBackground` | int | 0 · 0 |
| `display.hud.ammo` · `tickerFeed` · `help` · `tips` · `objective` · `reticleMessages` | bool | true each |
| `display.hud.aimReticleOpacity` | float | 10.000000 |
| `display.prompts.vehiclePromptMode` | int | 0 |
| `general.language` | `{ "$data": "0x08659ed8" }` | a hashed language name, read-only |

The section names `audio`, `camera`, `controls`, `display`, `general`, `hud`, `prompts`, `targeting`,
`accessibility`, `cinematic`, `controlPresets`, `firstPerson` and `thirdPerson` are named too. The root
(`#8c1c3948`) and 166 members are hash-only, shown as `#hash` with their type and value. Apart from
`sharpenIntensity`, no names overlap with `settings.xml`, which is a different file: the graphics
settings on `S:`.

**Editing.** `body::Settings` records every value's byte range. An edit:

- checks the value against its type (bool, integer, finite float written with the original's number of
  decimals);
- writes it in place. A shorter value is followed by spaces; a longer one takes its room from that
  line's tab indentation, or is refused;
- re-parses the whole body as strict JSON and requires the same keys and types.

`$v`, the version words and `$data` are read-only. The body never changes length, because the load
buffer is sized by the caller (`0x1428d3310` takes it in r9d; `0x1428d3564` refuses a bigger save). The
writer itself emits whitespace between tokens, so the edited text is still the JSON it writes.

## 7. The old build's folders (Import from Triumph)

The owner's fork keeps one folder per container:
`<saves>\<xuid>\<scid>\<name, made safe>~<FNV-1a 64 of the name>\v<N>\`.

- In each version folder, blob *i* is file *i*, and `manifest` holds length-prefixed (u32 LE) strings:
  the container name, the display name, then the blob names (data first, then header).
- The highest `v<N>` is committed; `v<N>.new` is an update that never finished.
- `triumph::import` reads only. It checks the folder hash against the name, takes the time from the
  save's stamp (a copied file's time is wrong), checks every container, then writes each one as our
  `.gsc` through `store::write_container`: backup first, refused while the title runs.

## 5. Online state

The image also holds a photo format (`0x1428b9560`: chunks `JPEG`, `JSON`, `TITL`, `DESC`, `JEND`) of
the kind shared online; it is not one of the save containers above and the editor does not touch it.
None of the containers we know is online state. The editor refuses to write any container whose name
looks online-related (`store::looks_online`).

## 6. Still open

- **Resolved by the real saves (§4):** the stamp's unit, the two kinds, the copies in the header, the
  layout of both bodies, and the ASAV words (0 for these kinds).
- **Still open:**
  - names for the profile's ids and for 166 settings members;
  - the profile's word +4 (3);
  - the `general.language` hash's string;
  - a game-progress save of kinds 1 / 9 (signed, read-only), when one exists;
  - the callers' buffer sizes (`0x14289dcf7`, `0x14289f89a` read them from globals), which the editor
    sidesteps by never growing a body.
