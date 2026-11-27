# Lane `gdk` — the GDK runtime behind `xgameruntime.dll` (Build 3 piece)

Branches `agent/gdk` (G1-G4, PR #18, merged) and `agent/gdk-2` (G5), crate `crates/gdk` (package `rebuild-gdk`), binaries `gdkguids` (G1 evidence) and `gdkslots`
(G4 table as CSV). Pure host Rust, no dependencies, no GPU, no title code. Nothing vendored (the fork's
`victory-hle` modules and run logs were read for slot offsets and behaviour, not copied), so no `CREDITS.md` line.

```
G1  entry points, interfaces, slots   closed   the exe's runtime GUID table: 30 / 30 GUIDs named, 0 unknown;
                                               libHttpClient's: 32 named, 12 unknown; XCurl's: 3 named, 1 unknown.
                                               0 "area not implemented" lines in the fork's 240 run logs
G2  XTaskQueue + XAsync               closed   4 dispatch modes, composite queues, delays, monitors, terminate,
                                               block lifetime, cancel, waits: 21 tests; worst submit onto busy
                                               ports 0.11 ms, worst XAsyncBegin+Schedule onto a busy pool 0.07 ms
G3  XGame, XUser, XStore/XPackage,    closed   11 interfaces, 102 slots, each called through its method table in
    XSystem (+ XNetworking offline)            a test; offline calls fail at once with the area's error
G4  (interface, slot) -> function     closed   124 rows in gdk-slots.csv; every listed slot resolves through
                                               QueryApiImpl (177 checks over 12 interfaces, 14 (provider, iid) pairs)
G5  local storage, display (HDR)      closed   the two pairs our first title run asked for: 2 interfaces, 6 slots
                                               (5 + 1), each called through QueryApiImpl in a test; 130 rows,
                                               188 slot checks over 14 interfaces, 17 (provider, iid) pairs
tests                                          46 in crates/gdk (G5: +4), workspace 321 passed, 6 ignored, 0 warnings
```

## Built

- `runtime.rs`: the six entry points (`InitializeApiImpl`, `…Ex`, `…Ex2`, `UninitializeApiImpl`, `QueryApiImpl`,
  `XErrorReport`) and `get_proc(name)` for the lead's `GetProcAddress`. Initialisations are recorded, not checked.
  `QueryApiImpl` for a pair we do not serve leaves `*out` untouched, notes the pair as discovery and returns
  `E_NOINTERFACE`.
- `object.rs`: one static object per interface, built from the definitions in `table.rs`. Every slot a definition
  does not name, slot 0 (`QueryInterface`) and 8 slots past the last known one hold a discovery handler: it notes
  "slot not implemented: <area> <offset>" and returns `E_NOTIMPL`. `AddRef` / `Release` return 1 (static objects).
- `taskqueue.rs`: XTaskQueue. Ports in `Manual`, `ThreadPool`, `SerializedThreadPool` and `Immediate` mode; composite
  queues over another queue's ports; delayed callbacks on one timer thread (a heap by due time, never a thread per
  delay); monitors called on every submit; the process queue (thread pool / thread pool, replaceable, clearable);
  handles are reference counted (`Duplicate` / `Close`). The pool starts a thread whenever more callbacks wait than
  workers idle, so a callback that waits on another never starves; idle threads beyond the core count leave after 10 s.
- **Termination**: new submits get `E_ABORT`; every pending callback of that queue, delayed ones included (pulled out
  of the timer at once), is delivered through its own port with `canceled = true` (a manual port still needs
  dispatching, so callbacks stay on their port's thread); when the queue has nothing pending the terminated callback
  runs once; `wait` blocks until it has returned. A dispatcher waiting on a terminated queue's empty port returns
  false; one waiting through another queue on the same ports keeps going.
- `xasync.rs`: XAsync with the console's `XAsyncBlock` (56 bytes) and `XAsyncProviderData` (32 bytes). Our call state
  is keyed by block address; the block's system area holds status (word 1) and result size (word 2), so they stay
  readable after the call ends. First completion wins; a call ends (provider `Cleanup`, exactly once) when its result
  is taken, or at completion if it failed or has no data, and it leaves the table before its status is published, so
  a block is free for the next call the moment a wait sees it complete (test: 2,000 back-to-back reuses). Cancel runs
  the provider's Cancel op and completes with `E_ABORT`. `XAsyncRun`, and `finish` / `take` for areas whose outcome
  is known at the start.
- `areas/`: the task-queue object (`queue.rs`), XUser, XGame (title, features, invites, events), XSystem (+ analytics),
  XPackage, XStore, XNetworking. `profile.rs` holds what they report: gamertag (1-15 bytes), XUID, title id, Store id,
  package id and size, console model, sandbox; `configure()` checks it.
- `known.rs`: every runtime GUID we can name (served or not), and `gdkguids`, which walks the title's GUID tables.

## Measured

### G1 — what the title asks `QueryApiImpl` for

`REBUILD_GAME_DIR=<game_dir> gdkguids` (reads the top-level images; their size and mtime are identical before and
after). Each image that links the GDK runtime stub carries a contiguous table of the GUIDs it passes to
`QueryApiImpl`:

| image | table | entries | named | unknown | named but not served here |
|:--|:--|--:|--:|--:|--:|
| `game_scarlett_master.exe` | 0x5f3fe20, stride 16 | 30 | **30** | **0** | 13 |
| `libHttpClient.GDK.dll` | 0xbbc90, stride 24 | 44 (+2 strings) | 32 | 12 | 15 |
| `XCurl.dll` | 0x202d0, stride 16 | 4 | 3 | 1 | 0 |

The fork's 240 run logs that reach `InitializeApiImplEx2` contain **0** "runtime API area not implemented" and **0**
"runtime method not implemented" lines: in every run, every (provider, iid) the title and its libraries asked for was
one the fork serves, and no slot the fork lacks was called. So the 12 distinct unknown GUIDs in the two libraries'
tables were never asked for in those runs.

**Entry points** (looked up by name in `xgameruntime.dll`; loader L5):

| name | arguments | seen |
|:--|:--|:--|
| `InitializeApiImplEx2` | `version: u64, build: u64, flags: u64, options: *const void` → HRESULT | twice per run (two stubs): version 0x6336_0000_2712, build 0x65f4_0000_1edd flags 2, and build 0x65f4_0000_1832 flags 3; options null. Meaning of the values: **unknown** |
| `InitializeApiImplEx` | `version, build, flags: u64` → HRESULT | not seen (the stub prefers Ex2) |
| `InitializeApiImpl` | `version, build: u64` → HRESULT | not seen |
| `UninitializeApiImpl` | `()` | not seen |
| `QueryApiImpl` | `provider: *const GUID, iid: *const GUID, out: *mut *mut Object` → HRESULT | every runtime call |
| `XErrorReport` | `hr: HRESULT, message: *const char` | not seen |

**Interfaces.** "exe" / "lib" = the GUID is in that image's table; the pairing (which iid goes with which provider)
is the fork's, which served the title through 240 runs.

| area | provider | iid | in | served here |
|:--|:--|:--|:--|:--|
| task queue / async | `073b7dcb-1fcf-4030-94be-e3c9eb623428` | = provider | exe, lib, XCurl | yes |
| user | `01acd177-91f9-4763-a38e-ccbb55ce32e0` | = provider | exe, lib | yes |
| user gamertag | `01acd177-…` | `cef4fac0-7676-4a94-a119-4c43f9eb5b74` | exe, lib | yes |
| title | `973a344e-24bf-4d0f-8457-56c534892b29` | `2549f142-6419-4a06-97b5-931aab7c2f34` | exe, lib | yes |
| features | `8836fe87-edb9-4fe3-8dad-05f0d2cd5b40` | = provider | exe, lib | yes |
| events | `bbfbdcc7-bfe7-409b-a5ca-edf054960b4d` | = provider | exe, lib | yes |
| invites | `0651aae2-4012-4077-bf84-8b9097090e2c` | = provider | lib | yes |
| system | `e349bd1a-fc20-4e40-b99c-4178cc6b409f` | `1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf` | exe, lib | yes |
| analytics | `b884675d-b738-4a9c-815d-9a9a1e0c6c9b` | = provider | exe, lib | yes |
| package | `af406016-e850-4aa8-a88d-2f3dcb9dac7e` | `0b9d556e-ab70-43f4-baa0-9999ac4cb040` | exe (lib: provider only) | yes |
| store | `0dd112ac-7c24-448c-b92b-3960fb5bd30c` | `b09d803c-2414-4a05-82c6-66dfdc9e9a44`, `2d42fea5-e71d-4b76-97cd-c50afbb3ae5d` | exe, lib | yes |
| networking | `37e56907-2f10-41e8-b72f-36edb185331a` | `bf2346b2-39af-4658-b5ea-44713c7e83b3` (and = provider) | exe, lib, XCurl | yes |
| display (HDR) | `03f0fe74-fdd9-4e5c-b630-f9339c47acc5` | `35f07670-706e-4bfb-9476-090798c5ebf3` | exe, lib | yes (G5) |
| shell (URIs, deferrals) | `1b339674-328d-4283-a200-3171f18d3639` | = provider | exe, lib | **no** |
| capture | `a4f1aee2-4bf1-4485-b008-a7c26d52ac27` | `22e672d7-b4e3-406c-bd50-8f0d25236f9e` | exe, lib | **no** |
| game save | `704c3f58-e629-4cc2-b197-30511b996fe2` | `1bfff3af-f14a-40a3-8e35-9ada906593f9` | exe, lib | **no** |
| game UI | `dfcd4649-4ff8-4043-ba07-35d607df98b0` | `eaf669df-5542-4590-99a3-8dc061f837cc` | exe, lib | **no** |
| local storage | `f4faf4d4-2d04-4fce-b3e0-474a713a3e84` | `41a4e10c-5a7e-41d9-8c37-37bde62a07d6` (and the older `d29411df-0794-4553-8b27-95fc02d0f75d`) | exe, lib | yes (G5) |
| speech / accessibility | `3e241977-6237-41e9-8559-102c2d9983f1` | `d722b373-8c4d-4692-9e51-d6ad9b37aa7d` | exe, lib | **no** |
| user devices | `7d824997-10dc-45ab-86b7-2737767c0bf1` | `0cc6a956-e7e1-4fdf-9341-9d5da649ebc8` | lib | no |

**Unknown** (in a library's table, next to the GUIDs shown; never asked for in the fork's runs): `8ca467f7-22e8-4096-8456-bb8aa13f79d8`
(lib and XCurl, before task queue), `ab4ae4fb-6508-4950-a032-45fd4bf8c43b` (lib, after game save),
`26f3c674-a2fe-44fa-b6c4-a323bc94ff53` (lib, between user and gamertag), `e2a4734b-2f4a-456d-aa8f-d065e04fb209` (lib,
after package provider: likely its package iid, not shown), `60b09f4e-1b85-45b1-826c-169118e230e1`,
`5c48dedf-0b67-4492-a4b5-6829b8e796e1`, `186d5592-a72d-45fb-9560-11aed0e6647a` (lib, among store),
`0a2192aa-b2d5-4d58-83be-383b6d80799e`, `57786622-6605-46d0-b917-0f22bbcd9c52`, `aebdfe78-4394-49ff-ba3a-001956457044`,
`95fd18d2-74dd-4d7c-aa1b-0b51827665d6`, `026b010c-06c3-4cdd-bbcb-43f229db1cff` (lib, after local storage). If the
title ever asks for one, `QueryApiImpl` names the pair as discovery.

**Slots.** Implemented slots, with the API each stands for and its console argument layout, are the 124 rows of
[`gdk-slots.csv`](gdk-slots.csv) (`gdkslots`). Every table starts with `QueryInterface` (0x00, unknown whether the
title calls it: discovery), `AddRef` (0x08), `Release` (0x10). Slots we do not implement, with the fork's label where
it has one (a label from the fork, not confirmed here) and **unknown** where it has none; each answers with discovery:

| interface | slots not implemented |
|:--|:--|
| task queue (31 slots) | 0x40 unknown · 0xa0 / 0xa8 register / unregister a wait handle (fork) · 0xd8 thread time-sensitivity (fork) · 0xe0 unknown · 0xf0 unknown |
| user (35) | 0x30 max users · 0x80-0x90 gamer picture · 0xd0-0xe0 token and signature, UTF-16 · 0xe8 / 0xf0 resolve issue with UI · 0xf8 / 0x100 the same, UTF-16 (all fork) |
| title (5) | 0x20 `XLaunchNewGame` (fork): ends the process, the lead's lifecycle |
| package (40) | 0x40 update monitor · 0x50 unregister progress · 0x58 user locale · 0x68 enumerate chunk availability · 0x78 install chunks (blocking) · 0xb8 mount (deprecated) · 0xc0 mount path size · 0xe0 enumerate packages (older) (all fork) · 0xa0, 0xa8, 0xb0, 0xd8, 0xe8-0x108, 0x118, 0x120 unknown |
| store (76) | 0x38 / 0x40 products by id · 0x58 / 0x60 product for the game · 0x68 / 0x70 for a package · 0xc0-0xd8 can acquire · 0xf0-0x100 add-on licences · 0x158-0x180 licence token and related · 0x198 / 0x1a0 rate and review · 0x1d0 / 0x1d8 download updates · 0x248 / 0x250 associated products page (all fork) · 0x120, 0x190, 0x1b0, 0x230 unknown (results of fulfillment, purchase UI, redeem UI by position; not confirmed) |
| networking (18) | 0x30 / 0x38 port changed · 0x58-0x68 security information, UTF-16 · 0x70 verify server certificate (all fork) |
| local storage (8) | none |
| display (5) | 0x18 unknown (the fork left it unimplemented and logged no call) |
| gamertag, features, invites, events, system, analytics | none |

### G2 — numbers

| | |
|:--|:--|
| ordering | manual port: 5 / 5 in submit order; serialized port: 200 / 200 in order, all on one thread (not the submitter's); delayed: 40 ms before 120 ms; 4 async calls complete in dispatch order |
| ports | DoWork ran on the thread dispatching the work port, the completion routine on the thread dispatching the completion port (two manual ports, two threads) |
| cancel | Cancel op once, `E_ABORT`, Cleanup once, a later `XAsyncComplete(S_OK)` ignored |
| terminate | 3 pending (work, completion, a 60 s delayed one) all delivered canceled at once, then the terminated callback once; new submits `E_ABORT`; a waiting dispatcher of the queue returns, one of a composite queue on the same ports does not; `wait` returns after a 50 ms callback finished; an async call whose work was queued fails `E_ABORT` and its routine still runs |
| no blocking | worst of 64 submits onto ports whose callbacks sleep 30 ms: **0.11 ms**; worst of 64 `XAsyncBegin` + `Schedule` onto a busy pool: **0.07 ms** (alone; the tests allow 20 ms under a parallel run, below the 30 ms a waiting implementation would take) |
| waits | `XAsyncGetStatus(wait)` returns when the call completes, never waiting on the completion routine (a manual completion port nobody dispatches does not hang it) |

### G3 — what the areas answer

| area | answers |
|:--|:--|
| user | `XUserAddAsync` (any options) → the one user at once; XUID and gamertag from the profile, local id 1, signed in, not a guest, adult, every privilege granted; tokens fail `E_GAMEUSER_FAILED_TO_GET_TOKEN` (0x89245111); change events never fire |
| title, features | title id from the profile; `XGameRuntimeIsFeatureAvailable` true for exactly the 12 features whose areas are served (test keeps the list and the table in step) |
| invites, events | registration tokens, no invite ever; events accepted and dropped (named once in discovery) |
| system | console id (38 characters), sandbox `RETAIL` (profile), app-specific device id (44 characters), each fitting the documented buffer size; analytics info (0x90 bytes) through the hidden return pointer: Xbox / Xbox Series X or S, OS 10.0.22000.0 |
| package | packaged; identifier from the profile; installation complete and launchable (size from the profile), every chunk ready, nothing to download; other packages `E_GAMEPACKAGE_NO_PACKAGE_IDENTIFIER` (0x89245208); mounting another package fails the same way |
| store | game licence active, full, not a trial, SKU `<store id>/0010`; our package's licence valid, durables not; no add-ons (empty query, no more pages); store id → our package id; catalog, consumables, collections and purchase ids, purchase / redeem / product pages, update checks and downloads fail `E_GAMESTORE_NETWORK_ERROR` (0x89245301) at once, through their result functions too |
| networking | initialized, connectivity level None; UDP port 3074; a URL's security information fails `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)` at once |

Every call the fork's logs name is answered: `InitializeApiImplEx2` (both stubs) and `XUserAddAsync` (options 1, 232
runs). The fork logged no other GDK call.

### G5 — the two interfaces the title asks for at start

Our first run of the title (`runs/build3-reached.txt`) left two discovery lines, both "interface not implemented".
Both are in the exe's GUID table (G1) and in `known.rs`; the fork's `storage.rs` and `display.rs` name them and their
slots, and its run logs under `D:\victory-work\logs` (242 that reach `InitializeApiImplEx2`) show which slots run.

| pair | what it is | slot | API | evidence the title calls it | answer |
|:--|:--|:--|:--|:--|:--|
| `f4faf4d4-…` / `41a4e10c-…` | **XPersistentLocalStorage** (the iid the 2026 title asks with; `d29411df-…` is the older one, same slots, also served) | 0x18 | `XPersistentLocalStorageGetPathSize` | the fork mounts the storage drive only inside 0x18 / 0x20; its "virtual drive" line for the storage folder is in 239 of 242 runs (in the latest, 36 ms after `InitializeApiImplEx2`, before the first HDR call) | 4 (`S:\` and its NUL) |
| | | 0x20 | `XPersistentLocalStorageGetPath` | the fork's note: the title calls 0x20 and 0x28; the drive mount above | `S:\` (the drive's root, from `Profile::storage_drive`; the runtime's `drives::STORAGE`). Too small: `E_FAIL`, nothing written (the fork's reading of the docs; not confirmed); null buffer `E_INVALIDARG` |
| | | 0x28 | `XPersistentLocalStorageGetSpaceInfo` | the fork's note (no log line: the fork does not log it) | 32 bytes: used = bytes of the files under `Profile::storage_host` (the overlay's `S` folder), total = `Profile::storage_bytes` (default 2048 MB, the manifest value the fork's runs show), free = quota rest, available = that, capped by the host disk's free bytes |
| | | 0x30 / 0x38 | `XPersistentLocalStoragePromptUserForSpaceAsync` / `…Result` | public header order; no call seen | nobody to ask: `S_OK` if the quota's rest covers the request, else `E_ABORT` (a user who cancels), completed at once through XAsync |
| `03f0fe74-…` / `35f07670-…` | **XDisplay, HDR mode** | 0x20 | `XDisplayTryEnableHdrMode` | 1,402 calls in 242 runs (6 per run in 232), every one with preference 0 (prefer HDR) | `Disabled` (2), `info` untouched, while we present SDR; `Enabled` (1) with the display's luminance (`XDisplayHdrModeInfo`, 3 x f32) once the lead sets `Profile::hdr` |
| | | 0x18 | unknown | none | discovery (`E_NOTIMPL`) |

`XGameRuntimeIsFeatureAvailable` now says yes for XDisplay (4) and XPersistentLocalStorage (12): 14 features. Unknown
slots (display 0x18, anything past each table) keep the discovery handler (tests call display 0x18 / 0x28 and local
storage 0x40). Tests (`xlocalstorage.rs`, `xdisplay.rs`) take every slot from the object `QueryApiImpl` hands out,
through its first word, for each interface id.

## Decided

- **Objects from definitions.** The G4 table is the source the method tables are built from, so the table cannot
  drift from what the title calls; `every_listed_slot_resolves` reads each slot through the object's first word, as
  title code does.
- **8 discovery slots past each table's end**, so a call past what we know names itself instead of reading beyond
  the table (a test calls store 0x260).
- **Waits don't wait for completion routines.** Unknown whether the console's `XAsyncGetStatus(wait)` returns before
  or after the routine; returning at completion can't hang on an undispatched manual port.
- **A failed Begin** completes the call with that failure and `XAsyncBegin` returns `S_OK` (the fork's choice, which
  ran the title); the console's choice is unknown. A block already in flight gets `E_INVALIDARG`.
- **Completion on a terminated queue**: the routine still runs, on the completing thread (unknown on the console).
- **Offline error codes** per area, from `XGameErr.h` / `winerror.h` names; the exact codes the console returns
  offline are not known.
- **Local storage is the drive's root**, never a host folder (the fork's finding: titles mount what precedes the
  path's last `:\`). Used bytes are walked from the host folder on each space call; the title calls it rarely, and
  a cache would go stale under the title's own writes.
- **HDR is off until the presentation can output it.** Answering enabled while we present SDR would make the title
  render HDR values into an SDR swap chain. The answer is one profile field away from enabled.
- **Unserved interfaces** get `E_NOINTERFACE` and a discovery line (the console's code for this is unknown; the
  title's stubs only test for failure, by the fork's runs).

## Next

- The five interfaces the exe's table names that are still not served: game save, game UI, shell (URIs, display
  deferrals), capture, speech. The fork served all of them, but our first run asked for none (so far); serve each
  when a run's discovery names it.

## For the lead

1. **Profile and config.** Call `rebuild_gdk::profile::configure(Profile { … })` before the title runs: gamertag from
   config (proposed registry entry `user.gamertag`, string, 1-15 bytes, default `Player`, restart), title id and Store
   id from the title's `MicrosoftGame.config`, `package_bytes` from the game folder, console model from config.
   `configure` refuses a gamertag that won't fit the runtime's 16-byte buffer.
2. **Wiring.** `GetProcAddress(xgameruntime, name)` → `rebuild_gdk::runtime::get_proc(name)` (6 names; a miss is noted
   in `rebuild_gdk::discovery`). `LoadLibraryExA("xgameruntime.dll")` needs a pseudo handle. Drain
   `rebuild_gdk::discovery::lines()` into the Build 3 discovery list.
3. **G5 (closed):** both pairs from `runs/build3-reached.txt` answer now. Fill the new profile fields at start-up:
   `storage_drive` = `drives::STORAGE` ('S', the default), `storage_host` = the overlay's `S` folder
   (`Drives::overlay.join("S")`: `rebuild_vfs` keeps a writable drive in `<overlay root>/<letter>`), `storage_bytes` =
   `PersistentLocalStorage/SizeMB` from `MicrosoftGame.config` x 2^20 (default 2048 MB), and `hdr` = the host
   display's luminance once Build 5 presents HDR (until then `None`: the title is told HDR is disabled).
4. **The game folder is being written by another process.** A new subfolder in `$REBUILD_GAME_DIR` has been filling
   since 14:27 today (multi-GB `.rpf` files), and `data\ttdb\ui\ttdb_ui.ttpf` changed at 14:19. Not this lane: its
   tools only read top-level images (unchanged before and after). Whole-folder before/after checks in other lanes
   (vfs V2, launcher U3, loader) will see changes that aren't theirs.
5. Root `Cargo.toml` `members` gained `crates/gdk` (one line).
