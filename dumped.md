# dumped.md — everything we know about the title's RAGE engine

Every finding about the title's engine, collected from static reverse engineering of the mapped image and from
live runs through our layer, 2026-11-27 to 2026-11-28.

**Sources:**
- [`docs/research/devmenu.md`](docs/research/devmenu.md): script VM, natives, entities, full evidence;
- [`deepdive.md`](deepdive.md): the whole run, memory model, CPU quirks;
- [`claude-max-handoff.md`](claude-max-handoff.md): the session log;
- the run reports.

**Conventions:**
- Addresses are preferred VAs in the mapped image `game_scarlett_master-59e9b414471e3e6d.dll` (base
  `0x140000000`, a prepared copy in `%LOCALAPPDATA%\triumph-rebuild\images\`). `exe+X` means RVA `X`.
- Confidence: **high** = read from code with an independent second site, or measured live; **medium** = one
  consistent reading; **low** = a candidate. *Live* = still needs gameplay to confirm (frames are black on the
  loading screen today).

---

## 1. Identity

| What | Finding | Confidence |
|---|---|---|
| Engine | Rockstar **RAGE** | high |
| Family | a GTA-family title (`GTAPLUS`, `~BLIP_CASH_BAG…~`, `Wanted Tuning`, `deadEyeTargets`, `horse_breaking_start`) | high |
| Build | Xbox Series (Scarlett) master build, `game_scarlett_master.exe` | high |
| Rockstar services | `%s-%s.ros.rockstargames.com`, `%s.scauth.rockstargames.com`, `cloud.rockstargames.com`, `RockstarAccount` | high |
| RAGE markers | `%s.rpf`, `fiDeviceDirectStorage`, `Rage.fwLocalisedStringScript`, `ScriptSaveBlock`, `AMBIENT_SCRIPT_SLOT_%d`, `common:/data/…`, `%sdata/tune/%s.meta` | high |
| Symbols | RTTI only for the Xbox libraries; game classes are unnamed and game strings are sparse | high |
| Class ids | `joaat` (Jenkins one-at-a-time) of the lower-cased class name, as in earlier RAGE titles | high |

## 2. Boot and process

| What | Finding | Confidence |
|---|---|---|
| Images | 15–16 PE32+ images in the title folder; all DLL entry points succeed under our loader | high, live |
| Imports | 697 functions not supplied by the title's own DLLs: 624 bind to the host, 34 are overridden, 39 are console-only (7 DLLs) | high |
| Time to device | the entry point reaches `D3D12XboxCreateDevice` in ~1.4 s; first `ExecuteCommandListsX`/`PresentX` at ~1.44 s | high, live |
| TLS | the exe reads its block as `gs:[0x58]` → `array[0]` without `_tls_index`; it must own TLS slot 0 | high |
| Allocation hooks | the exe exports `__XMemAllocationHooks__`; `XMemAlloc`/`XMemFree` go through the title's own hooks once installed | high |
| Device type | read from `HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM\ConsoleMode` (GDK lookup at `exe+0x55deb88`). Absent = PC (type 1); `0x64` = Series X, `0x60` = Series S, `0x68` = devkit | high |
| CPU instructions | uses `clwb` (1 site) and AMD `clzero` (5 sites), plus `monitorx`/`mwaitx` in spin waits | high |
| Boot state | boot-graph state byte `0x1471B9048` = 2 during the loading screen | medium, live |

## 3. Memory

| What | Finding | Confidence |
|---|---|---|
| D3D allocator | render setup pushes an XMem scope for allocator **0x80** (D3D) at `0x145C8F990` | high |
| Hook dispatcher | `0x144DEB430`; allocator list = XMem attribute bits 16–23; page-size-2 allocations are served from an arena (callback `0x1456A2A60`) | high |
| Arena assert | `int3` at `exe+0x247af2d` if the arena does not end up full: `end − start + [0x1475E6FF0] <= 0x1FFFFF`; arena size `0x4000000` or `0x5200000` | high |
| D3D attributes | resource memory is XMem with attrs `(0x80 << 16) \| (2 << 14)` | high |
| Physical page numbers | the title packs `pfn >> 4` into **21-bit** fields (`0x141E6C348`), so frame numbers must be in 4 KiB units (multiples of 16) | high, live |
| Scatter mapping | built at `0x140F2CB90..0x140F2CC3E` via `XMemMapPhysicalPagesScatter`; page 0 in the list means unmap | high |
| Shader arena | ~420 MB (6,720 × 64 KiB pages), scatter-mapped from a 6.1 GB physical pool view | high, live |
| Content | ~7.1 GiB loaded during boot | high, live |

## 4. Files, archives, DirectStorage

| What | Finding | Confidence |
|---|---|---|
| Archives | 12 `.rpf` archives open at boot, all successfully | high, live |
| Shader archive | `shaders.rpf`: 26,503 shader containers | high |
| Shader databases | init DB at rpf offset `0x10287000` (it holds the `im` effect); main DB `final.fxdb` (`SGD5`) at `shaders.rpf+0x9000`, holding `deferred_lighting` etc. | high |
| Shader record example | init DB + `0x2CAE` = record `e7dc0d00`, packed size `0x3f9` | high |
| Read style | `shaders.rpf` is opened `FILE_FLAG_NO_BUFFERING`; reads are synchronous OVERLAPPED-at-offset | high, live |
| DirectStorage | the title streams through DirectStorage: 64 KiB zlib chunks, ~4,200 requests at boot, 130 MiB in, 262 MiB out | high, live |
| BCPack | textures may be BCPack-compressed (decoder in `crates/dstorage`) | high |
| Main DB load | flag `0x14760AFE1` is set when `final.fxdb` loads; its task is `0x143750CE0` (task id `0x5FDC36D5`); the init DB loads from `0x145EB2E57` (sets `…AFE0`) | medium |

## 5. Rendering

### 5.1 The device

| What | Finding | Confidence |
|---|---|---|
| Interface | asks `D3D12XboxCreateDevice` for `ID3D12Device11` | high, live |
| Device object | byte `+0x18` = 1 makes the title call methods instead of writing inline packets | high |
| Objects at boot | 1 queue, 5 fences, 4 X-queues, 439 allocators, 581 lists, 2 frame events, ~95 compute pipelines, ~57,000 pipelines created in 60 s (53,733 graphics, 599 mesh) | high, live |
| Console pipeline calls | device slot `0x1c8` called ~25,000× and `0x218` (FSR) ~2,700× during the main shader load; refusing both with `*out = null` is tolerated | high, live |
| Frame pacing | `SetFrameIntervalX(33,333 µs)` = 30 fps; the loop is an origin poll plus a flip wait (`WaitFrameEventX`) | high, live |
| GPU-hang handler | `exe+0x2474606`: on device removal or hang, calls device slot `+0x190`, then `int3` | high, live |
| Transient-resource assert | `exe+0x131a2ad`: `0x14131A270` places a render-graph resource (type `0x980` = texture through `0x14193DE10`, else buffer `0x14006DC20`) and asserts when it returns false, i.e. when device slot `0x428` (resource at an address) fails. Called from the page allocator `0x142473E20` (64 KiB pages, mapped through `0x142474660`). At the loading screen's end the title makes a 2560x1440 **B4G4R4A4** render target there; the RTX 4070 Ti refuses that format as a target (`E_INVALIDARG`), WARP takes it. RGBA8 stands in (`format::stand_in`) | high, live |
| Other asserts | `exe+0x2474ce1` (an `E_INVALIDARG` from a list call); `exe+0x14a6390` (unresolved) | medium |

### 5.2 Effects and programs

| What | Finding | Confidence |
|---|---|---|
| Effect map | capacity at `0x148420AA0`, table at `0x148420AA8`; 24 of 32 entries filled before the main DB loads | medium |
| Effect lookup | `0x141ADCC20(name)`, e.g. `"deferred_lighting"` | high |
| Immediate renderer | globals `0x148769BB0` and `0x14983C0D8`, set at `0x140CBD1E6` from that lookup; renderer init `0x140CBD110` | medium |
| Loading-screen effect | the init DB's `"im"` effect at `[0x1476667B8]`; technique byte `[0x1476667CF]` = `0x2a` | medium |
| Program create/lookup | `0x141D65750`; the parser stores its result at `exe+0x247C837` | high |
| Root signatures | built at `0x142476E20` with a chunk check (`0x140FA09F0`); blobs embedded in the exe have a 12-byte PRIV and no root signature (the rpf ones are ~480 B). Root-signature cache at `[0x1475DDB90]` | medium |
| Set-program | engine set-program at `0x140122BD6` stores `[ctx+0x12328]`; `ctx+0x12330` is a pipeline cache | medium |
| State stack | at `[tls0+0x12A8]`; slot reset through `0x140B39840` (id `0xF` = the program slot), from `0x14012494E`; immediate draw at `0x1402AACF0` | medium |

### 5.3 How the title uses the GPU (measured through our layer)

| What | Finding | Confidence |
|---|---|---|
| Descriptors | **untyped**: the title reads a heap slot holding a buffer SRV as a constant buffer (e.g. index 982,941). PC D3D12 cannot do this; we write heap buffers as raw SRVs and read them as raw loads | high, live |
| Descriptor indices | the title uses view indices up to ~1,245,184, past a PC heap's 1,000,000 | high |
| Rectangle lists | console primitive topology **17**, PSO topology type 19; used for full-screen passes | high, live |
| Barriers | enhanced barriers, including a console-only layout **31**, a read layout (treated as `GENERIC_READ`) | high, live |
| Initial layouts | resources are created in SHADER_RESOURCE / UNORDERED_ACCESS layouts and their first barrier names that layout | high, live |
| Resource flags | depth targets created with unordered access; render target + depth combinations | high, live |
| Re-execution | the title executes a command list again while its previous execution is still in flight (the console allows it) | high, live |
| Indirect signatures | 5 made, each with one argument: `DRAW` (16 B), `DRAW_INDEXED` (20 B), `DISPATCH` (12 B), `DISPATCH_MESH` (12 B), console type `0x69` (12 B, never used); none change root parameters | high, live |
| ExecuteIndirect | 0 calls in the menus and loading screen; expected in gameplay | high, live |
| Shaders outside `shaders.rpf` | 1,420 programs are created from bytes elsewhere (the exe, other archives, `xboxbvhbuilder.dll` for the BVH builder's 80) | high, live |
| Shader classes | of 25,694 programs, 9,128 wave-agnostic; the rest wave32 or wave64 | high |
| Descriptor-as-data | many shaders read T#/V# descriptor words 0–3 as data (`v_alignbit_b32`, `s_bfe_u32`, `s_lshr_b64`, `s_and_b32`) | high |
| Timestamps | GPU timestamp labels written into title memory (flag 1) | high, live |
| Back buffer | 3840×2160 `R8G8B8A8_TYPELESS`, render target + UAV | high, live |

### 5.4 The loading screen (first ~40 s)

One list per frame, about 41 calls: a clear of the back buffer to (0, 0, 0, 0), then three draws:
1. A full-screen triangle into an intermediate target: VS `dfafaf385b2f94e8`, PS `06295fb1d1aefb78`. These are
   **not in `shaders.rpf`**. This is the screen's actual content.
2. A rectangle list, clip space, into the intermediate target with depth: VS `58d926085fca4999`, PS `edc1cad8ffb79bc9`.
3. A rectangle list, pixel space, into the back buffer: VS `0602c2384acbb3b2`, PS `a78d088fa55aa495`.

Vertex buffers live in the title's own memory (not resource memory). Viewport (0, 0, 3840, 2160, depth 0–1) and
scissor are full screen. Near the end of the loading screen the title records ~1,500 draws and ~250–370
dispatches.

## 6. Timing

| What | Finding | Confidence |
|---|---|---|
| Clocks | imports `QueryPerformanceCounter` and `GetTickCount64` | high |
| Frame delta | passes FidelityFX a per-frame `frameTimeDelta`: it measures real frame time | high |
| Presentation stats | `GetFrameStatisticsX` is called but cannot drive pacing (we return count 0) | high, live |
| Fixed step | no fixed-time-step strings found; RAGE normally uses a clamped variable step | low |

Unlocking the frame rate most likely does not speed up the main loop. Some subsystems may still assume 33.3 ms
(*live*: a timed run at cap 60 will tell).

## 7. Script VM and natives

Full evidence: [`docs/research/devmenu.md`](docs/research/devmenu.md). Code: `crates/devmenu`.

### 7.1 The interpreter

| What | Address / layout | Confidence |
|---|---|---|
| Interpreter | `0x143de2550`; 16-bit opcodes decoded with xor and multiply (`imul eax, edi, 0x3315`); 545-entry jump table at `0x146553900` | high |
| Entry | `0x143de2540` (tail-jumps to the interpreter; only called from `0x143de204f`) | high |
| Thread run | `0x143de1f10` = vtable slot 17 of the script-thread vtables `0x14654fd20`, `0x14654fde0`, `0x1465604f0`, and the one in `0x1465603f0` | high |
| Active thread | TLS block + `0x1478` | high |
| Thread object | state byte `+0x10` (1 = running), stack info `+0x48`, program `+0x270` | high |
| Program load | `exe+0x1c18340`, by hash through `0x1452ffc50` | high |

### 7.2 Native calls

```
handler = operand + *(u64*)0x1498c1810
handler base = 0x141d109b0 - (rotr32(prog[+0x24], 12) ^ prog[+0x10])   ; set once, by the first program
ctx = { u64* args; u64* ret; i32 argc; u32 retc }                       ; 24 bytes
```

| What | Finding | Confidence |
|---|---|---|
| Registry | **no hash → handler table at run time**; the (obfuscated) bytecode carries handler offsets | high |
| Enumeration | every handler starts with a hash probe (`argc < 0` → `args[0] = hash`, then the body **still runs**). A static scan over `.pdata` finds **6,818 natives**, all hashes unique | high |
| Public lists | no match: the hashes are this title's own (12 well-known hashes from earlier titles: 0 hits) | high |
| Slots | 8 bytes; int/float/bool = 1 slot; vector (x, y, z, w f32) = 2 slots; entity handle `{u64 value, u32 index, u32 tag}` = 2 slots | high |
| Lock | every handler takes the recursive lock `0x14aad44e0` (acquire `0x140004b40`, release `0x144d9dab0`, owner at TLS + `0x1454`) | high |
| Native opcode | the `call r15` at `0x143de750e` (census hook point) | high |

### 7.3 Natives named so far

| Hash | Handler | Signature | Name | Confidence |
|---|---|---|---|---|
| `0x6c660bb25792a0f9` | `exe+0x3c09d00` | `(Handle, bool) -> Vector` | GET_ENTITY_COORDS | medium-high |
| `0x96b4308cdb2afe46` | `exe+0x4461c00` | `() -> Handle` | PLAYER_PED_ID | medium |
| `0x0121bbe16337ceb2` | `exe+0x3c14d00` | `(Handle, Vector, int, bool)` | SET_ENTITY_ROTATION (or relative) | medium |
| `0xac03f3022ba44764` | `exe+0x3c14c80` | `(Handle, Vector, Vector, bool)` | orient along `b − a` | low |
| `0x8c811ee775562ff5` | `exe+0x44bc900` | `(int, Vector, float, int, int, int, bool) -> int` | async creation request (spawn candidate) | low |

Native handlers cluster into ~25 address ranges (compilation units ≈ namespaces); the entity natives are in
`0x143c04080..0x143c1b590` (918 handlers).

### 7.4 Entities

| What | Finding | Confidence |
|---|---|---|
| Local player | global `CPed*` at `0x14ab56e38` (1,842 references) | medium-high, *live* |
| Handle lookup | `0x1404372d0` (reader-writer-locked map → base object at `[entry+0x18]`); each cast then calls `vtable[1](this, joaat id)` | high |
| Casts | CEntity `0x140350170` (`0xb1389614`), CPed `0x14526e340` (`0x94821659`), CVehicle `0x140d35050` (`0xf820aaa9`), CObject `0x14133bbd0` (`0x9cec50b6`), CDynamicEntity `0x1407ba630` (`0x291dd2f1`), fwEntity `0x1404374e0` (`0x9423cddb`) | high |
| `+0x30` | `u8` type: 3 = ped; 2, 3, 4 get a physics sync after a matrix change | high / medium |
| `+0x40` | matrix rows: right, forward, up (16 B each) | medium |
| `+0x70` | packed position `{f32 x, f32 y, f32 z, i16 cell_x, i16 cell_y}`; world = local + cell × 32 | high |
| `+0x9c` | `u32` id; bits 17–21 = type (`0x60000` = ped) | high |
| `vtable+0x170` | `SetMatrix(this, *const [f32x4; 4], bool, bool, u8 warp)` | medium |
| Physics sync | `0x1450bb780(id)` → `0x142080950(phys)`; for type 2 first `0x140a6dda0(id)` → `0x141a57510` | medium |

### 7.5 Teleport recipe (the title's own sequence; medium, *live*)

1. Copy the matrix `ent[+0x40 .. +0x80]`; set row 3 to the packed target position.
2. `vtable(ent)[0x170/8](ent, &m, true, true, warp)` (try `warp = 1`).
3. Physics sync as above.
4. Run on the script thread. Mind streaming at the destination: arrive above ground or freeze briefly.

### 7.6 Threading

- **Safe:** the script thread, at the interpreter call `0x143de204f` inside `0x143de1f10`; `[TLS+0x1478]` already
  holds the script thread there. The minimal patch is a trampoline replacing the `call rel32` there, running our
  queue once per frame, then jumping to `0x143de2540`.
- **Not safe:** the render thread, where our layer sees `PresentX` and `WaitFrameEventX`.
- The script VM had **not started** by the end of a 60 s loading-screen run (handler base `0x1498c1810` was never written).

## 8. Online

| What | Finding | Confidence |
|---|---|---|
| Modes | Xbox multiplayer (`MultiplayerActivity`, `MatchmakingService`, `sessiondirectory.xboxlive.com`), Rockstar online (`ros-SessionTicket`, `SessionKey`, `NetworkDefStore`, `GTAPLUS`), privileges `AllowMultiplayer`, `PlayMultiplayer` | high |
| Our layer | offline by construction: connectivity hint = none, `XUserGetTokenAndSignatureAsync` always fails, so no service sign-in | high |
| Gaps | sockets and WinHTTP bind straight to the host; `XUserCheckPrivilege` grants multiplayer | high |

Any developer menu must refuse every action unless offline is asserted, no session work is seen and no outbound
non-loopback connection exists.

## 9. Open questions

- Names for SET_ENTITY_COORDS, request/has-loaded model, CREATE_VEHICLE, clock, weather, invincibility, the
  network-session check. All come from a runtime census once scripts run.
- Vehicle model names: from the vehicle metadata in the RPF archives, read through our vfs (the game folder is
  never read directly).
- Where the 1,420 out-of-archive shaders come from (the exe, other archives).
- Which OS thread runs the script VM, and whether once per frame.
- The meaning of console indirect argument type `0x69` and of the `warp` byte.
- The assert at `exe+0x14a6390`.
