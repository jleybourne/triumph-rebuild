# The title's save format

Lane `saveeditor`, 2026-11-28. Everything here was read from the title's own code in the prepared image
(`%LOCALAPPDATA%\triumph-rebuild\images\game_scarlett_master-….dll`, the loader's output), with
capstone, the way the `gdk` lane read the save thunks. Addresses are the image's virtual addresses
(base `0x140000000`). No leaked material, no other title's code, and no title bytes in git: the
constants below are the few numbers the format needs, each cited.

**Status.** The container layout, both headers, all three checksums and which saves are encrypted
and signed are decoded from the code and implemented in `crates/saves` (tests build saves the way
the writer does and check them the way the reader does). **Not decoded:** the body after the ASAV
header (the game state itself), the meaning of five ASAV words, the stamp's clock, and which kind
`SettingsProfileSettings` uses. All of these need a real save, and no card run has written one yet
(first-run setup never finished in a 120 s run; `agents/gdk.md`).

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
indirect call); its unit is unknown until a real save is read. The same value goes into ASAV +0x20
(`0x1428d204b`).

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

## 4. SettingsProfileSettings

Read the way section 1 describes (by name, both blobs). Its kind, and so whether it is signed, is not
fixed in the code we read: it comes from the request object that names the container. **Needs a real
save**: open it in the Saves screen, which shows the kind. If it is 2-8, the display name and every body
byte are editable now (checksums recomputed); decoding the body into named settings needs that real
file. If it is 1 or 9 it stays read-only.

## 5. Online state

The image also holds a photo format (`0x1428b9560`: chunks `JPEG`, `JSON`, `TITL`, `DESC`, `JEND`) of
the kind shared online; it is not one of the save containers above and the editor does not touch it.
None of the containers we know is online state. The editor refuses to write any container whose name
looks online-related (`store::looks_online`).

## 6. What needs a real save

- the body layout of each kind (game state, settings) and the five undecoded ASAV words;
- the stamp's unit;
- `SettingsProfileSettings`' and `Player`'s kinds;
- that the outer header's +0x18 / +0x1c / +0x20 equal the ASAV copies in a real file (warned, not
  required, by the editor);
- the manager's buffer size `[mgr+0x758]`.
