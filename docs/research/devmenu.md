# Developer menu groundwork: the title's script VM, natives, player, entities

Static reverse engineering of the mapped image `game_scarlett_master-59e9b414471e3e6d.dll` (preferred base
`0x140000000`). The goal is an offline single-player developer menu: teleport, spawn vehicles, and cheap extras.
Addresses are preferred VAs; `exe+X` means RVA `X`. Typed constants and the pure logic are in `crates/devmenu`.

Confidence levels:
- **high**: read directly from code, with a second, independent site that agrees;
- **medium**: one consistent reading; needs a live check;
- **low**: a candidate only.

Frames are still black on the loading screen. Everything marked *live* needs gameplay to confirm.

## Summary

| Question | Answer | Confidence |
|---|---|---|
| Engine | Rockstar's RAGE, a GTA-family title (Rockstar services, GTA+ strings) | high |
| Script VM | yes: interpreter `exe+0x3de2550`, natives called through a 24-byte context | high |
| Native registry | **no hash → handler table at run time**: the bytecode carries handler offsets. Every handler embeds its own 64-bit hash (a probe), so a static scan lists all **6,818** natives with hashes | high |
| Public hash lists | no match (12 well-known hashes from the engine's earlier titles: 0 hits in the whole image) | high |
| Local player ped | global `CPed*` at `0x14ab56e38` | medium-high, *live* |
| Player position | packed at `entity+0x70`: `f32 x, f32 y, f32 z, i16 cell_x, i16 cell_y`, world = local + cell × 32 | high |
| Get coords native | hash `0x6c660bb25792a0f9`, handler `exe+0x3c09d00` | medium-high |
| PLAYER_PED_ID | hash `0x96b4308cdb2afe46`, handler `exe+0x4461c00` | medium |
| Teleport | rebuild the matrix, call the entity's `SetMatrix` virtual (`vtable+0x170`), then the physics sync: the title's own sequence | medium, *live* |
| SET_ENTITY_COORDS native | not identified yet | — |
| Vehicle spawn / model streaming | not identified; one async creation-request native is a candidate | low |
| Time, weather, god mode | not identified | — |
| Safe thread | the script thread, inside `THREAD_RUN` around the interpreter call; **not** the render thread | high (static) |
| Online | the title has online modes; our layer is offline by construction (no tokens, no connectivity) | high |

## 1. Engine and scripting

**Engine: RAGE** (high).
- Rockstar strings: `%s-%s.ros.rockstargames.com`, `%s.scauth.rockstargames.com`, `cloud.rockstargames.com`,
  `rockstar.fwui.display.*`, `RockstarAccount`, `signin.rockstarg…`.
- RAGE naming: `%s.rpf`, `fiDeviceDirectStorage`, `Rage.fwLocalisedStringScript`, `ScriptSaveBlock`,
  `AMBIENT_SCRIPT_SLOT_%d`, `common:/data/…` paths, `%sdata/tune/%s.meta`.
- GTA family: `GTAPLUS`, `~BLIP_CASH_BAG…~`, `Wanted Tuning`, `deadEyeTargets`, `horse_breaking_start`,
  `common:/data/personaabilities`, `common:/data/ai/buddymindsetprofiles`.
- Class ids are `joaat` of lower-cased class names (section 3), as in the engine's earlier titles.
- The image keeps RTTI only for the Xbox libraries. Game classes have no names and the game's own strings are
  sparse. Everything below is read from code.

### 1.1 The interpreter

| What | Address | Evidence |
|---|---|---|
| interpreter | `0x143de2550` | `cmp byte [rcx+0x10], 1` (state); 16-bit opcodes decoded with an xor and multiply (`imul eax, edi, 0x3315`); a 545-entry absolute jump table at `0x146553900` (530 entries into this function) |
| entry | `0x143de2540` | tail-jumps to the interpreter; its only reference is `0x143de204f` |
| thread run | `0x143de1f10` | vtable slot 17 of four script-thread vtables: `0x14654fd20`, `0x14654fde0`, `0x1465604f0`, and the one embedded in `0x1465603f0` |
| active thread | TLS block + `0x1478` | `mov r14, [gs:0x58]; mov r14, [r14]` (TLS slot 0); `mov r12, [r14+0x1478]; mov [r14+0x1478], rsi; call entry; mov [r14+0x1478], r12` at `0x143de2041..0x143de2054` |

The thread object passed to the interpreter has its state byte at `+0x10` (1 = running) and stack info at `+0x48`.
Program load: `exe+0x1c18340` looks the program up by hash (`0x1452ffc50`) and stores it at `thread+0x270`.

### 1.2 Native calls (high)

From the native opcode (`0x143de7411..0x143de763a`):

```
operand  = u32 at [rsp+0x30]   (decoded from the instruction stream)
argc     = u8  at [rsp+0x34]
retc     = u8  at [rsp+0x35]
handler  = operand + *(u64*)0x1498c1810
ctx.args = stack_top - argc*8      ; [rsp+0xa8]
ctx.ret  = retc ? stack_top : 0    ; [rsp+0xb0]
ctx.argc = argc                    ; [rsp+0xb8]
ctx.retc = retc                    ; [rsp+0xbc]
[TLS+0xf30-indexed slot] = pc      ; the caller's program counter, for diagnostics
call handler(&ctx)
stack_top += retc*8; results are copied down over the arguments; stack_top -= argc*8
```

`crates/devmenu/src/native.rs::Ctx` mirrors this context.

```rust
#[repr(C)] struct Ctx { args: *mut u64, ret: *mut u64, argc: i32, retc: u32 }
```

**Handler base.** The global `0x1498c1810` is written once, by the first program loaded (`exe+0x1c18379..0x1c1839d`):
`0x141d109b0 - (rotr32(prog[+0x24], 12) ^ prog[+0x10])`, set only while the global is zero and the two dwords
differ. So the bytecode encodes each native as `handler - anchor + key`. The bytecode itself is obfuscated, and
**there is no hash → handler registry at run time**. `native::handler_address` implements the formula.

**Slots.**
- Slots are 8 bytes.
- An `int`, `float` or `bool` takes one slot. Handlers read `dword` (or `float`) from it.
- A vector (`x, y, z, w` as `f32`) takes two slots and is read with one 16-byte load.
- An entity handle takes two slots: `{u64 value, u32 index, u32 tag}` (`native::Handle`). Handlers copy the 16
  bytes to the stack and pass a pointer to a cast helper (section 3). One producer writes `tag = 4` (`0x143c313cc`).

**Lock.** Every handler takes the recursive lock at `0x14aad44e0`:
- acquire `0x140004b40`, release `0x144d9dab0`;
- the owner is the thread index at TLS block + `0x1454`.

Natives are serialised against each other, not against the rest of the game.

### 1.3 Enumerating natives: the hash probe (high)

Every handler begins with:

```
cmp  dword ptr [rcx+0x10], 0      ; argc
mov  rsi, rcx
js   probe
...body...
probe:
mov  rax, [rsi]                   ; args
movabs rcx, <64-bit hash>
mov  [rax], rcx                   ; args[0] = hash
jmp  body
```

The probe writes the hash and then **runs the native anyway**. Never call a handler with a negative `argc`.

Reading the hashes statically is safe. `native::scan` does it over `.pdata`, and the `natives` example prints them:

```
cargo run --release -p rebuild-devmenu --example natives -- <image copy>
```

Results:
- 6,818 handlers, all hashes unique. The test `scan_the_image_copy` pins this count when the image copy is present.
- Handlers sit in a few dense address ranges (compilation units, roughly the namespaces):

| Range | Count |
|---|---|
| `0x143c04080..0x143c1b590` | 918 (entity: coords, rotation, look-at) |
| `0x143ccd530..0x143cda7d0` | 558 |
| `0x143e82440..0x143e93680` | 682 |
| `0x14445c500..0x14446a870` | 614 (holds the player-ped native) |
| `0x1444bbcb0..0x1444ce6a0` | 669 |
| `0x1444fba00..0x14450c0f0` | 696 |
| `0x144a7e1c0..0x144a97cd0` | 1,041 |
| `0x144bc60c0..0x144bdd4f0` | 824 |
| `0x144bf0670..0x144bfac80` | 406 |
| `0x144c12450..0x144c18380` | 254 |
| 15 small ranges | the rest |

**Public lists.** None of the well-known 64-bit hashes of the engine's earlier titles appear anywhere in the image
(checked: `GET_ENTITY_COORDS`, `SET_ENTITY_COORDS`, `CREATE_VEHICLE`, `PLAYER_PED_ID`, `PLAYER_ID`, `WAIT`,
`GET_GAME_TIMER`, network checks: 0 hits). The hashes are this title's own, so names come from behaviour.

**Naming method.** Most handlers are thin: unpack the arguments, call one implementation function, write the
result. A typed signature per handler narrows the search, e.g. `V V d d` = handle, vector, int, int.
- `V`: two slots read as 16 bytes (a handle or a vector);
- `d`: a dword;
- `f`: a float;
- `q`: a qword.

The scratch scripts that produce these signatures are listed in section 8.

## 2. Natives named so far

| Hash | Handler | Signature | Name | Confidence | Evidence |
|---|---|---|---|---|---|
| `0x6c660bb25792a0f9` | `exe+0x3c09d00` | `(Handle, bool) -> Vector` | GET_ENTITY_COORDS | medium-high | impl `0x140350100`: handle → `CEntity*` (`0x140350170`); if the entity is a ped (`+0x30 == 3`) and the flag is clear, take the entity behind id `+0x9c` (`0x144dd7510`); unpack `+0x70` (`0x144dc3790`); return 16 bytes |
| `0x96b4308cdb2afe46` | `exe+0x4461c00` | `() -> Handle` | PLAYER_PED_ID | medium | impl `0x14447f600` reads the local-player global `0x14ab56e38`, maps its id `+0x9c` (`0x144da6540`, ped type `0x60000`), builds a handle from component `+0x2e40` (`0x145efc850`). It is one of 17 zero-argument handle returners and the only one that reads that global and builds a ped handle |
| `0x0121bbe16337ceb2` | `exe+0x3c14d00` | `(Handle, Vector, int, bool)` | SET_ENTITY_ROTATION (or a close relative) | medium | impl `0x143c25b40`: builds a rotation matrix from the vector (order in the int), keeps the position, calls `vtable+0x170` with the bool as the last argument, then syncs physics |
| `0xac03f3022ba44764` | `exe+0x3c14c80` | `(Handle, Vector, Vector, bool)` | orient the entity along `b - a`, keeping its position | low | impl `0x143c25ec0`; same tail as above |
| `0x8c811ee775562ff5` | `exe+0x44bc900` | `(int, Vector, float, int, int, int, bool) -> int` | an asynchronous creation request (spawn candidate) | low | impl `0x1444f8460`: resolves the first argument through a generational handle table (`0x1408bc570`, `index = v >> 8`, class id `0x5c79cb42`), builds a request (`0x1429e8fa0`), queues it (`0x140a54d90` → `0x140d3b910`), returns an `int` request id |

`crates/devmenu/src/native.rs::known` holds the first three.

## 3. Handles, casts and entities

### 3.1 Casts (high)

Each cast takes a `*const Handle` and returns an object pointer, or null when the handle is absent or the object is
another class:
- `0x1404372d0` looks the handle up in a reader-write-locked map and returns the base object (`[entry+0x18]`);
- each class cast then calls the object's class-id virtual (`vtable[1]`, `(this, id) -> bool`) with a `joaat`
  class id.

| Cast | Address | Class id |
|---|---|---|
| `CEntity` | `0x140350170` | `0xb1389614` = joaat("CEntity") |
| `CPed` | `0x14526e340` (also `0x145c30bf0`) | `0x94821659` = joaat("CPed") |
| `CVehicle` | `0x140d35050` (also `0x145c30c50`) | `0xf820aaa9` = joaat("CVehicle") |
| `CObject` | `0x14133bbd0` (also `0x145c30c20`) | `0x9cec50b6` = joaat("CObject") |
| `CDynamicEntity` | `0x1407ba630` | `0x291dd2f1` = joaat("CDynamicEntity") |
| `fwEntity` | `0x1404374e0`, `0x140c51680` | `0x9423cddb` = joaat("fwEntity") |

Unit-tested in `hash.rs`.

### 3.2 Entity layout

| Offset | Field | Confidence | Evidence |
|---|---|---|---|
| `+0x30` | `u8` type: 3 = ped; 2, 3, 4 get a physics sync after a matrix change | high (ped), medium (others) | `0x140350215`; `bt 0x1c` at `0x143c25e96` |
| `+0x40` | matrix rows: right, forward, up (16 bytes each) | medium | copied whole (`vmovups ymm [+0x40]`, `[+0x60]`) and rebuilt by the rotation natives |
| `+0x70` | packed position | high | unpacked by GET_ENTITY_COORDS; rebuilt by the rotation natives; 151 local-player sites read it |
| `+0x9c` | `u32` id; bits 17..21 = type (`0x60000` = ped) | high | 530 local-player sites; `0x144da6540` |
| `vtable+0x170` | `SetMatrix(this, *const [f32x4; 4], bool, bool, u8 warp)` | medium | `0x143c25e62`, `0x143c260aa`, both with `r8b = r9b = 1` |

### 3.3 Packed position (high)

The position is stored as `{f32 x_local, f32 y_local, f32 z, i16 cell_x, i16 cell_y}`, with `world = local + cell × 32`
on x and y.
- Unpacking (`0x144dc3790`) is a fused multiply-add with the constant `(32, 32, 0, 0)` at `0x146159c90`.
- Packing is inlined where the title rebuilds a matrix (`0x143c25e0a..0x143c25e3b`):
  `cell = trunc(pos / 32)` (`vroundps` mode 3), `local = pos − cell × 32`, saturated to `i16` (`vpackssdw`).

`crates/devmenu/src/world.rs` implements both directions, with tests.

## 4. Player and teleport

**Local player.** The global `CPed*` at `0x14ab56e38` (medium-high):
- 1,842 code references;
- the field offsets those sites use are exactly the entity fields: `+0x9c` 530 times, `+0x70` 151 times, `+0x30` 12 times;
- PLAYER_PED_ID's implementation reads it.

Live checks: the pointer is non-null in gameplay, `*(p+0x30) == 3`, and the cast `0x14526e340` accepts the handle
PLAYER_PED_ID returns.

**Read the position.** Either:
- read `p+0x70` and unpack it ([`world::Packed`]); safe on any thread for a display, possibly torn;
- call GET_ENTITY_COORDS through its handler on the script thread.

**Teleport.** This is the title's own sequence, copied from the tail of the rotation natives (medium; *live*):
1. `m = copy of ent[+0x40 .. +0x80]` (four rows), then `m[3] = Packed::pack(target)` (keep the rotation).
2. `(vtable(ent)[0x170/8])(ent, &m, true, true, warp)`. The rotation natives pass their last script argument as
   `warp`. Try `1` first (likely "teleport, no interpolation").
3. If `ent[+0x30] == 2`: `x = 0x140a6dda0(ent[+0x9c])`; if non-null, `0x141a57510(x)`. Then reread the type.
4. If the type is 2, 3 or 4: `phys = 0x1450bb780(ent[+0x9c])`; if non-null, `0x142080950(phys)`.

Do this on the script thread (section 6). Long jumps will hit the usual streaming problem: collision and map may
not be loaded at the destination yet. Teleport a few metres above ground, or freeze the player until the floor
streams in.

**SET_ENTITY_COORDS** itself is not identified yet. The shapes that fit were checked, and their implementations
are velocity or rotation setters. The candidates below and the runtime census (section 7) are how to find it:
- the handlers in the entity range that take `(Handle, Vector, …bools)`;
- implementations that call the matrix virtual or a packing routine.

## 5. Vehicle spawn

Not established yet (low):
- the request native in section 2 (`0x8c811ee775562ff5`) is a candidate for an asynchronous "create at position";
- what its first argument names is unknown (a generational handle, not a model hash);
- the model streaming natives (request, has-loaded, release) are not named yet.

**Model names.** Models are named by `joaat` of the lower-cased name (the class-id evidence shows the hash). The
vehicle list must come from the title's metadata files: the vehicle definitions inside the RPF archives, read
through our vfs at run time. The game folder was not read for this work (rule), and the image holds no model-name
table.

**Next step: the runtime census (section 7).**
- Natives that take a model hash return later with `bool` results while the model streams in.
- A creation native returns a handle that the vehicle cast (`0x140d35050`) accepts.

## 6. Threading and the safe hook point

**Threads.**
- Natives run on whichever thread runs the script VM. Each handler takes the global recursive native lock
  (`0x14aad44e0`), which suggests scripts may be stepped from more than one worker. Some natives read the active
  thread from TLS (`+0x1478`), e.g. to own the entities they create.
- The render thread (where our layer sees `PresentX` and `WaitFrameEventX`) is **not** safe: game state is
  written by the update side without that lock.

**Hook point (high, static).** Inside `THREAD_RUN` (`0x143de1f10`), at the interpreter call `0x143de204f`.
- Just before it, `[TLS+0x1478]` already holds the script thread. Draining our action queue there runs in a real
  script context, on the script thread, between two script steps.
- Run the queue once per frame: on the first thread seen in a new frame, using the layer's frame counter.
- The hook is a 5-byte `call rel32` at `0x143de204f` (target `0x143de2540`). A trampoline that runs the queue and
  then jumps to `0x143de2540` is the minimal patch.
- When calling handlers from there, take the native lock the same way handlers do. The lock is recursive, and
  handlers take it themselves anyway.

**Live checks.**
- Which OS thread runs `0x143de1f10`, and is it the same one every frame?
- Does it run while frames are presented?

A 60-second WARP run on the loading screen (watchpoint on the handler-base global `0x1498c1810`) saw **no write**:
no script program had loaded by then. The VM starts later than the loading screen, so these checks wait for
gameplay.

## 7. Runtime census (the next step once scripts run)

The menu needs names for a few dozen natives. With no registry, the quickest route is a census at the native
opcode:
1. Patch `0x143de750e` (`call r15`) through a trampoline that logs `r15` (handler), `ctx.argc`, `ctx.retc`, the
   first argument slots and the results, then makes the call.
2. Map handler → hash with `native::scan` over the mapped image (`Pe::parse(mapped = true)`).
3. Name by observation:
   - PLAYER_PED_ID: the handle returned every frame by the zero-argument native, also accepted by the ped cast;
   - GET_ENTITY_COORDS: its result equals the unpacked `+0x70`;
   - request/has-loaded: a model hash in, a bool out flipping to true;
   - create: a handle out that the vehicle cast accepts, with the model hash and a position in;
   - clock: three ints in the 0..23 / 0..59 ranges, changing by a minute;
   - weather: a `joaat` name in.

Diagnostic only, behind an environment variable like `REBUILD_PATCH`, never by default.

## 8. Online

**The title has online modes** (high). The image has:
- Xbox multiplayer services (`MultiplayerActivity`, `MatchmakingService`, real-time activity, `sessiondirectory.xboxlive.com`);
- Rockstar online services (`ros.rockstargames.com`, `scauth`, `ros-SessionTicket`, `SessionKey`, `NetworkDefStore`, `GTAPLUS`);
- privilege names `AllowMultiplayer` and `PlayMultiplayer`.

**Our layer is offline by construction** (high):
- `XNetworkingGetConnectivityHint` reports level None (`crates/gdk/src/areas/xnetworking.rs`);
- URL security queries fail with network-unreachable;
- `XUserGetTokenAndSignatureAsync` always fails (`crates/gdk/src/areas/xuser.rs`). Rockstar and Xbox service
  sign-in need those tokens, so no online session can be authenticated.
- Two gaps: sockets and WinHTTP bind straight to the host (`crates/runtime/src/lib.rs`), and `XUserCheckPrivilege`
  grants every privilege, including multiplayer.

**Menu gate.** Refuse every action unless all of these hold:
1. The connectivity hint is offline and no token was ever issued (true by construction; assert it).
2. No session-shaped work has started. The layer can count calls into the multiplayer-activity and
   session-directory paths and outbound `connect` calls to non-loopback addresses; any non-zero count disables
   the menu.
3. Later, once the census names it: the title's own network-session native returns false.

Denying the multiplayer privilege in `xuser` would make the title itself refuse online modes. That is a one-line
change in the GDK lane, not made here.

## 9. Needs gameplay to confirm

- `0x14ab56e38` holds the player `CPed*` in gameplay; type byte 3; cast accepts PLAYER_PED_ID's handle.
- GET_ENTITY_COORDS (`0x6c660bb25792a0f9`) returns the unpacked `+0x70`.
- The teleport sequence moves the player, with the physics synced and no rubber-banding. The meaning of `warp`.
- Which OS thread runs `THREAD_RUN`, and that a queue drained at `0x143de204f` runs once per frame.
- The census names for request-model, has-model-loaded, create-vehicle, clock, weather, invincibility and the
  network-session check.

## Reproducing

- `cargo test --release -p rebuild-devmenu`: class ids, packing, the context layout, the probe on bytes from the
  image, and the full scan when the image copy is present.
- `cargo run --release -p rebuild-devmenu --example natives -- <image copy>`: every handler and hash.
- The scratch Python (capstone) used for signatures and call graphs lives outside the repo:
  - `common.py`: PE, `.pdata`, disassembly;
  - `sig.py` and `tsig.py`: per-native typed signatures;
  - `pack.py`: natives that pack positions;
  - `hashes.py`: the first scan, which missed 12 handlers with long prologues; the Rust scan finds them.
