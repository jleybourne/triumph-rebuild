# Public-sources review: our GDK runtime and D3D12 mapping against Microsoft's public docs

Date: 2026-11-28. Branch `agent/publicdocs`. No title runs and no GPU work: this is documentation
and header reading plus reading our code.

## Sources (public only)

| Tag | Source | Used for |
|---|---|---|
| **GDK docs** | learn.microsoft.com/gaming/gdk (pages marked `permissioned-type: public`, monikers gdk-2510 to gdk-2610) | API contracts |
| **GDK headers** | NuGet `Microsoft.GDK.Windows` 2610.0.9661, `native/261000/windows/include/*.h` (the public PC GDK) | struct layouts, enum values, HRESULTs, SAL annotations |
| **Reference impl** | github.com/microsoft/libHttpClient `Source/Task/AsyncLib.cpp`, `TaskQueue.cpp` (MIT). This is Microsoft's open-source XAsync / XTaskQueue, built for the platforms that have no GDK runtime. It is a secondary source. Where it disagrees with the GDK docs, the docs win, and the table says so. | behaviour the docs leave open |
| **D3D specs** | microsoft.github.io/DirectX-Specs | barrier, indirect and resource rules |
| **D3D docs** | learn.microsoft.com/windows/win32/direct3d12, `d3d12.h` from github.com/microsoft/DirectX-Headers | limits, format support |
| **DX blog** | devblogs.microsoft.com/directx (Agility SDK) | SDK opt-in |

No leaked, NDA or console-SDK material was opened. Quotes are kept to a sentence or less; everything
else is paraphrase.

## Prioritized findings

Impact means the effect on correctness or performance if the case is hit. **Fixed** means fixed in
this PR, with a test. **Rec** means a recommendation; the "Lane" column says who owns the code.

| # | Impact | Area | Finding (source) | Our code | Status |
|---|---|---|---|---|---|
| 1 | High, correctness | XAsync cancel | Cancellation belongs to the provider. On `XAsyncOp::Cancel` the provider "should cancel all asynchronous work and then call XAsyncComplete" with `E_ABORT` ([XAsyncOp][xop]). Cancellation "may happen synchronously, asynchronously, or not at all" ([XAsyncCancel][xcancel]). The docs' cancelable provider only signals an event; DoWork notices it and completes ([Set up cancelability][xcancelex]). | `xasync.rs` `cancel` forced the call to `E_ABORT` right after the Cancel op, and that ran the provider's **Cleanup while its DoWork could still be running**, which is a use-after-free in title code for exactly the documented pattern. | **Fixed**: `cancel` only invokes the Cancel op (`xasync.rs:324`). |
| 2 | High, correctness | XAsync cleanup | Reference impl: when a call completes inside DoWork, the provider's Cleanup runs after DoWork returns, and it is deferred while a Cancel op is in flight (`AsyncLib.cpp` `WorkerCallback`, `CleanupProviderForLocation`, `InCancel`). | `complete` called Cleanup re-entrantly, inside the provider's own `XAsyncComplete` call. | **Fixed**: a per-call gate defers Cleanup until the DoWork or Cancel ops in flight return (`xasync.rs:62-125`). |
| 3 | High, correctness | XNetworking | `XNetworkingRegisterConnectivityHintChanged` "registers a 'connectivity hint changed' notification and sends an initial notification callback" ([doc][xnreg]). The title ships libHttpClient and XCurl, which "manage network initialization automatically" ([Network initialization][xninit]), i.e. through this registration. | `register` never called the callback, and it accepted a null callback. | **Fixed**: the initial callback is queued to the queue's completion port with the current hint. `unregister(wait)` waits for a running callback (`xnetworking.rs:154-190`). |
| 4 | Med-high, correctness | Enhanced barriers | "For now, `Offset` must be zero and `Size` must be either the buffer size in bytes or `UINT64_MAX`" ([Enhanced Barriers][eb]). One invalid barrier errors the whole list, so `Close` fails and the title asserts at `Reset`. | `xlist` `barrier_groups` passed the title's buffer ranges through. Our host buffer need not be the console's size either. | **Fixed**: buffer barriers become whole-buffer (`list.rs` `whole_buffer`). |
| 5 | High, performance | Title memory heaps | `OpenExistingHeapFromAddress`: the heap "is created in system memory and permits CPU access" ([doc][oefa]). Cross-adapter memory is system-memory only and "not efficient for discrete/NUMA adapter architectures" ([Shared heaps][shared]). | `runtime/src/titlemem.rs:116-145` and `uma/src/labels.rs:30-36` place buffers in opened heaps with `ALLOW_CROSS_ADAPTER` set unconditionally. | Rec (runtime memory / uma lanes): keep GPU traffic to opened heaps light (labels, small reads). Set `ALLOW_CROSS_ADAPTER` only when `GetDesc().Flags` has `SHARED_CROSS_ADAPTER`. Don't plan reserved/tiled windows on opened heaps, because "Cross-adapter sharing does not work with CreateReservedResource". |
| 6 | Medium, correctness | Initial layouts | "simultaneous-access textures have an immutable layout" (COMMON). "Buffers may only use D3D12_BARRIER_LAYOUT_UNDEFINED as an initial layout" ([Enhanced Barriers][eb]). `ALLOW_CROSS_ADAPTER` "Must be used with heaps that have D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER" ([D3D12_RESOURCE_FLAGS][rflags]). | `d3d12x/src/resource.rs:218-254` (`initial_layout`) ignores `ALLOW_SIMULTANEOUS_ACCESS`. It squashes queue-specific layouts 18-29 to COMMON, so the title's first barrier names a `LayoutBefore` the resource is not in. `format.rs:243` keeps flag 0x10 on DEFAULT-heap textures. `views.rs:395` does not check `OPTIONS12.EnhancedBarriersSupported`. | Rec (d3d12x lane): COMMON when simultaneous. Pass 18-29 through with the same flag checks. Strip 0x10 from host textures. Gate `CreateCommittedResource3` on OPTIONS12. |
| 7 | Medium, correctness | Formats | B5G5R5A1 (86) and B4G4R4A4 (115): render target and blend are "optional" ([DXGI format support for 12.0 hardware][fmt]). B5G6R5 (85) is required. Query with `CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT)` and `Support1 & RENDER_TARGET`. | `d3d12x/src/format.rs:141-150` hard-codes RGBA8 stand-ins for every render-target use of 86 and 115. | Rec (d3d12x lane): query both formats once at device creation and keep the native 16-bpp format where the card supports it (WARP does; the 4070 Ti refuses 115). Use RGBA8 only as the fallback. |
| 8 | Medium, correctness | Buffer views | `D3D12_REQ_BUFFER_RESOURCE_TEXEL_COUNT_2_TO_EXP = 27` (`d3d12.h`); a buffer holds at most 2^27 texels ([resource limits][limits]). | `d3d12x/src/heap.rs:338-410` cuts views to the bytes the memory holds (commit 0f42454, correct), but has no 2^27 cap. `(len / e) as u32` also truncates above 4G elements. | Rec (d3d12x lane): cap typed and structured `NumElements` at 1<<27. For raw views above 2^27 dwords (512 MiB or more), first run one WARP plus debug-layer probe; the public pages don't say whether raw views are exempt. |
| 9 | Medium, correctness | XTaskQueue terminate | The termination callback "will be invoked from the completion thread at the end of termination". The doc's WndProc example relies on that and calls `DestroyWindow` from it ([XTaskQueueTerminate][tqterm]). Reference impl: the work port terminates first, then the completion port, and the callback is queued on the completion port. | `taskqueue.rs:360` runs it on whichever thread finished the last callback, or on the terminating thread. | Rec: queue the terminated callback on the completion port. This needs `dispatch` to keep a terminated manual port alive until that callback has run (today it returns false as soon as the port is empty, `taskqueue.rs:403-430`). |
| 10 | Medium, robustness | Timestamps | Copy-queue timestamps exist only if `OPTIONS3.CopyQueueTimestampQueriesSupported`. Frequency is per queue ([Timing][timing]). | `uma/src/upload.rs:385` calls `copy_queue.GetTimestampFrequency()?` without checking the flag, so an optional feature becomes a setup failure. `xlist/console/stamps.rs:59-64` checks it correctly. | Rec (uma lane): skip copy timing when the flag is off. |
| 11 | Medium, portability | Agility SDK | An app opts in by exporting `D3D12SDKVersion` and `D3D12SDKPath` from its .exe ([Agility SDK][agility]). | `rebuild.exe` exports neither, so enhanced barriers, layout 31 and OPTIONS19 depend on the OS's D3D12. | Rec: ship the Agility SDK from `rebuild.exe`; `host_layouts` (`list.rs`) could then pass 31 straight through. |
| 12 | Med-low, correctness | XAsync waits | `XAsyncGetStatus(wait)` waits until the call "either completes or is canceled". Using both it and a completion callback is ambiguous: "you should not use both for the same call" ([doc][xstatus]). Reference impl: the wait is satisfied only after the completion routine has returned. | `get_status` (`xasync.rs:282`) returns when the status is out, before the routine runs (a deliberate choice, so a manual completion port nobody dispatches can't hang a waiter). | Rec: keep it for manual completion ports. For thread-pool and immediate ports, wait for the routine as the reference does, so a title that frees a stack block after the wait can't race its routine. |
| 13 | Low-med, correctness | XTaskQueue vs XAsync | Reference impl: every in-flight XAsync call holds `XTaskQueueSuspendTermination` on its queue, so termination waits for calls still in flight (for example on external I/O). | Our termination waits only for queued callbacks. An in-flight call's completion then finds the queue terminated and runs its routine inline (`xasync.rs` `complete`). | Rec: count in-flight calls per queue in `taskqueue::Queue::pending`. |
| 14 | Low | XTaskQueueDispatch | It "can be called for any dispatch mode", and the call is dispatched on the calling thread ([doc][tqdisp]). | `taskqueue.rs:405` returns false for any non-manual port. | Rec: harmless today (pool ports drain themselves). Revisit if a title is seen dispatching a pool port. |
| 15 | Low | XAsync small rules | Reference impl: a null result buffer gives `E_INVALIDARG`; `XAsyncGetResultSize` always writes its out value; the provider block and callback are snapshotted at begin. | Ours gave `E_NOT_SUFFICIENT_BUFFER`, wrote no size on failure, and re-read `block.callback` after publishing the status. | **Fixed** (`xasync.rs` `get_result`, `get_result_size`, `Completion`). |
| 16 | Low | XUser token data | `XUserGetTokenAndSignatureData.signature` is `_Null_terminated_` (GDK headers, `XUser.h:276-282`). The doc sample prints it with `%s` ([doc][xtoken]). | The sandbox placeholder signature had no NUL (`xuser.rs`). | **Fixed** (debug-only sandbox path). |

## What this PR changes

All the code changes are in `crates/gdk` (no other lane owns it), plus one function in `crates/xlist`:

- `crates/gdk/src/xasync.rs`
  - `XAsyncCancel` invokes the provider's Cancel op and does nothing else (#1).
  - The per-call `Gate`: Cleanup runs once, after the DoWork or Cancel ops in flight return (#2).
  - A queue-terminated DoWork sends the Cancel op, then completes with `E_ABORT` if the provider has not (reference).
  - A null result buffer gives `E_INVALIDARG`; the result size is always written; the completion routine is carried with its block instead of being re-read (#15).
  - Tests:
    - `cancel_is_cooperative_and_cleanup_waits_for_do_work` is the docs' cancelable provider, and fails on the old code.
    - `xasyncrun_is_not_cancelable`.
    - `result_size_is_written_on_failure_and_a_null_buffer_is_invalid`.
    - `forever` now completes on Cancel, as documented.
- `crates/gdk/src/areas/xnetworking.rs`: the initial hint notification on the completion port, registrations, `unregister(wait)`, and a required callback (#3). Tests: `registering_sends_one_initial_notification_through_the_queue` and `unregistered_registrations_get_no_callback`.
- `crates/gdk/src/areas/xuser.rs`: the sandbox signature is NUL-terminated (#16); the test reads it as a C string.
- `crates/xlist/src/console/list.rs`: `whole_buffer` on each mapped buffer barrier (#4), with the test `buffer_barriers_cover_the_whole_buffer`. This is a minimal edit in the barrier path; it touches no other xlist logic.

## GDK detail

### XAsync and XAsyncProvider

| Rule | Source | Ours |
|---|---|---|
| A failed Begin op completes the call; `XAsyncBegin` still returns `S_OK` | Reference (`XAsyncBegin`: "we use it to fail the async call, instead of failing XAsyncBegin") | Matches (`xasync.rs:165`). The comment called this "the fork's choice"; the reference confirms it. |
| A block already in flight: `E_INVALIDARG` | Reference (`AllocState`) | Matches. |
| `XAsyncComplete(E_PENDING)` is not a completion; the first result wins | [XAsyncComplete][xcomplete]; reference | Matches. |
| `requiredBufferSize` "is ignored if the callback function returns an error" | [XAsyncComplete][xcomplete] | Matches (failure ends the call with size 0). |
| `XAsyncGetResult` identity mismatch: `E_INVALIDARG` | [XAsyncGetResult][xresult] | Matches. |
| `XAsyncGetResult` on a call with no payload | Doc: "returns the error code `E_NOT_SUPPORTED`". Reference: `S_OK` (the zero-payload call is already cleaned up, so it never reaches that branch). | Ours returns the call's status, like the reference. **Kept**: our `*Result` wrappers for no-payload calls (`XUserResolvePrivilegeWithUiResult`, `XPersistentLocalStoragePromptUserForSpaceResult`) rely on it, and the runtime's own wrappers must too. |
| Results taken twice: `E_ILLEGAL_METHOD_CALL` | Reference | Matches. |
| `XAsyncGetResult` failing with a short buffer | Reference: the call is ended (results lost) | Ours keeps the call so the title can retry. More lenient and harmless; kept. |
| DoWork returning success without completing | Reference: completes with `E_UNEXPECTED` | Ours completes with that success and size 0. Only a buggy provider sees the difference; kept, because our `XAsyncRun` relies on it. |
| `XAsyncSchedule` twice before the work runs | Reference: `E_UNEXPECTED`; on a completed call, its failure code or `E_INVALIDARG` | Ours accepts it. Low; no change. |
| `XAsyncRun` can't be canceled | [Set up cancelability][xcancelex] | Now matches (#1). |
| Null queue: the process queue is duplicated at begin; none set gives `E_NO_TASK_QUEUE` | Reference | Ours resolves the queue at schedule/complete time. Low. |

### XTaskQueue

| Rule | Source | Ours |
|---|---|---|
| Terminate: pending callbacks run with `canceled = true`; new submits fail with `E_ABORT` | [XTaskQueueTerminate][tqterm] | Matches. |
| Termination callback on the completion thread | same page | Differs (#9). |
| `wait = true` from a thread that dispatches the queue may deadlock | same page | Same risk in ours; documented behaviour. |
| Dispatch returns false once the queue is terminated, even with `INFINITE` | [XTaskQueueDispatch][tqdisp] | Matches. |
| Dispatch on non-manual ports | same page | Differs (#14). |
| Submit and `Dispatch(0)` are lock- and wait-free ("needs to be lock and wait free") | same page | Ours takes a short mutex for a push and never waits on title code (`taskqueue.rs` header). Close enough; noted. |
| The default process queue is thread pool / thread pool; set-to-null clears it | GDK headers / reference | Matches. |
| Invalid handle: `E_GAMERUNTIME_INVALID_HANDLE` (`0x8924010C`) | Reference; value from `XGameErr.h` | Ours dereferences the handle and returns `E_INVALIDARG` for null only. Low: titles don't pass bad handles. |

### XUser

| Item | Source | Ours |
|---|---|---|
| `XUserAddOptions` values: Silently 1, AllowGuests 2, AllowingUI 4. `AllowGuests` with `AddDefaultUserSilently` is not allowed. | GDK headers; [XUserAddAsync][xadd] | We return the user for any option set. Low (offline single user). |
| No default user: `E_GAMEUSER_NO_DEFAULT_USER` (`0x89245106`) | [XUserAddAsync][xadd] | Never arises: one signed-in user. |
| Gamertag buffer sizes: Classic 16, Modern 97, ModernSuffix 15, UniqueModern 101. A short buffer is the insufficient-buffer error; a bad component is `E_INVALIDARG`. | GDK headers; [XUserGetGamertag][xgt] | Matches (`put_str`, `E_NOT_SUFFICIENT_BUFFER`). |
| `XUserState`: SignedIn 0; `XUserAgeGroup`: Adult 3 | GDK headers | Matches. |
| Token data: `tokenSize` and `signatureSize` are "The size in bytes of the buffer"; both strings are `_Null_terminated_` | [XUserGetTokenAndSignatureData][xtoken]; `XUser.h` | Signature fixed (#16). Whether the sizes count the NUL is **not settled** by the public sources; ours excludes it. |
| `E_GAMEUSER_*` HRESULTs in `hr.rs` | `XGameErr.h` | All match: USER_NOT_FOUND `0x89245104`, FAILED_TO_GET_TOKEN `0x89245111`. |

### XNetworking

- `XNetworkingConnectivityHint` (16 bytes) has the field order level, cost, iana, networkInitialized, approachingDataLimit, overDataLimit, roaming. The headers and [the struct page][xnhint] match `ConnectivityHint`.
- The level enum is Unknown 0, None 1, Local 2, Internet 3, Constrained 4. The cost enum is Unknown 0, Unrestricted 1, Fixed 2, Variable 3. Both match.
- `Unknown` level and cost "occurs when the network is not initialized". We report initialized with level None, which the docs give as "network medium disconnect". That is consistent with an offline console.
- The initial notification is fixed (#3). Which port the runtime queues it on is not documented. We use the completion port, where XAsync completion routines also go.
- The [initialization page][xninit] shows titles blocking on the change callback until `networkInitialized`. That works with our hint, which says initialized from the first call.

### XPersistentLocalStorage

- **Confirmed**: a buffer shorter than the path fails with "E_FAIL for Xbox devices", and with `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)` on PC ([XPersistentLocalStorageGetPath][plspath]). Ours returns `E_FAIL` (`xlocalstorage.rs:38`). The fork's reading is right; the "not confirmed" note in the code can go.
- **Confirmed**: `XPersistentLocalStoragePromptUserForSpaceAsync` completes with "E_ABORT if the user chose not to free up space", and `requestedBytes` is "The total amount of space" ([doc][plsprompt]). Ours matches.
- `XPersistentLocalStorageSpaceInfo` has the field order availableFree, totalFree, used, total (GDK headers). Matches.

### XGame, XSystem, XPackage, XDisplay

- `XGameRuntimeFeature` values 0-21 match `AVAILABLE` in `xgame.rs` (GDK headers, `XGameRuntimeFeature.h`).
- `XSystemConsoleIdBytes` 39, `XSystemAppSpecificDeviceIdBytes` 45 and `XSystemXboxLiveSandboxIdMaxBytes` 16 match (GDK headers).
- `XSystemAnalyticsInfo` is 0x90 bytes, and `XVersion` is major, minor, build, revision in that order (GDK headers; [XVersion][xversion]). Both match.
- The doc's `family` example is "Xbox" and its `form` example "Xbox One X DevKit" ([doc][xanalytics]). Ours: "Xbox" plus the profile's form.
- `XPACKAGE_IDENTIFIER_MAX_LENGTH` 33, `XPackageInstallationProgress`, and `XPackageChunkAvailability` (Ready 0) match.
- `XDisplayHdrModeResult` (Unknown 0, Enabled 1, Disabled 2) and `XDisplayHdrModePreference` (PreferHdr 0) match. `displayHdrModeInfo` is `_Out_opt_`.

### Slot offsets: the public headers do not predict them

The console's method tables are not the public header's declaration order. Counting `STDAPI`
declarations at 0x18 + 8i matches our evidence-based slots for some areas but not others:

- **Matches:** XNetworking (all nine slots we serve), XPersistentLocalStorage, XPackage up to 0x98.
- **Diverges:** XUser from 0x48 (the header has `XUserAddByIdWithUiAsync` there; ours is `XUserGetLocalId`), XPackage from 0xc8, and XSystem (the header starts with `XSystemGetAnalyticsInfo`).

So header order is no evidence for a slot, and the slot tables stay as they are. A match is weak
supporting evidence at most.

## D3D12 detail

| Topic | Spec says | Ours | Verdict |
|---|---|---|---|
| Legacy and enhanced interop | A legacy resource "must be transitioned to D3D12_RESOURCE_STATE_COMMON before being referenced using an enhanced Barrier", and the reverse holds for layouts. LAYOUT_COMMON equals STATE_COMMON ([EB][eb]). | `resource.rs:218-254` maps a legacy initial state to a layout; legacy barriers pass through (`list.rs:1563`); enhanced barriers are remapped (`list.rs:1631-1716`). | Works in practice, because the runtime translates legacy barriers. A legacy-created resource moved by legacy barriers from a non-COMMON state is outside the stated rule: debug-layer noise, low. |
| `CreateCommittedResource3` initial layout | See #6 | See #6 | Differs (#6). |
| Upload / readback initial states | GENERIC_READ / COPY_DEST | `views.rs:387` | Matches. |
| Opened and cross-adapter heaps | See #5. Committed cross-adapter resources are row-major Texture2D only, and RT or UAV on row-major needs `CrossAdapterRowMajorTextureSupported` ([Shared heaps][shared]). | Buffers only today | Matches for buffers (#5 is about performance). Textures in title memory would need the row-major query. |
| Tiled resources | Tile heaps on FLAG_NONE need resource heap tier 2; tier 1 needs `ALLOW_ONLY_BUFFERS`. Check `MaxGPUVirtualAddressBitsPerResource` for 4 GiB windows. | `uma/src/tiles.rs:89, 113-119` requires tiled tier 2 | Matches on our hosts. Add the VA-bits check before the 4 GiB windows. |
| Buffer view limits | See #8 | See #8 | 2^27 cap missing. |
| Optional render-target formats | See #7 | See #7 | Replace the guess with the query. |
| Descriptor heaps | 1,000,000 on tiers 1 and 2; "1,000,000+" on tier 3; samplers 2048; "maximum number of unique static samplers across live root signatures is 2032" ([Hardware tiers][tiers]). OPTIONS19 reports the real maxima. | `d3d12x/src/heap.rs:66-93` reads OPTIONS19, probes 2^21, then falls back to 1M/2048; `sir/src/lift.rs:78` | Matches. Watch the 2032 static-sampler budget. |
| ExecuteIndirect | The draw/dispatch entry "must come last". The root signature is set "if and only if" root arguments change. Constants are sorted with no overlap. 4-byte strides. The count is min(max, count buffer). Bindings it touched are reset afterwards. `INCREMENTING_CONSTANT` needs OPTIONS21 tier 1.1 ([Indirect drawing][indirect]). | `xlist/console/indirect.rs:98-193`, `d3d12x/src/signature.rs:113`, `list.rs:1392-1397` | Matches. `DISPATCH_RAYS` and `INCREMENTING_CONSTANT` are refused; map the latter on tier 1.1 if a title uses it. |
| Timestamps | Frequency is per queue; copy queues need OPTIONS3; `GetClockCalibration` correlates a queue with QPC ([Timing][timing]). Nothing promises that timestamps from different queues are comparable. | `runtime/src/queue.rs:183-194` passes each queue's frequency through; slot 0x98 (`GetClockCalibration`) traps | Matches, except #10. Write 0x98 only when a run shows the title calls it. |
| DRED | Enable before device creation | `runtime/src/device.rs:213-226` | Matches. |
| CBV | 256-byte placement, at most 4096 x 16 B | `d3d12x/src/heap.rs:360-361` | Matches. |

## Recommended next changes, in order

1. **d3d12x lane**: #6 (`initial_layout`: simultaneous-access textures COMMON, layouts 18-29 passed through, strip `ALLOW_CROSS_ADAPTER`, gate on OPTIONS12) and #8 (the 2^27 cap). Both are small and testable on WARP.
2. **d3d12x lane**: #7, a `FORMAT_SUPPORT` query at device creation replacing the hard-coded B4G4R4A4 / B5G5R5A1 stand-ins.
3. **gdk**: #9, the terminated callback on the completion port, with dispatch kept alive until it has run. Then #13, in-flight XAsync calls holding off termination.
4. **uma lane**: #10 (the copy-timestamp check) and the conditional `ALLOW_CROSS_ADAPTER` from #5.
5. **runtime**: #11, the Agility SDK exports from `rebuild.exe`.
6. **gdk**: #12, waiting for the completion routine on non-manual completion ports.

[xop]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xasyncprovider/enums/xasyncop
[xcancel]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xasync/functions/xasynccancel
[xcancelex]: https://learn.microsoft.com/en-us/gaming/gdk/docs/features/common/async/async-libraries/async-library-xasyncprovider-example-setup-cancelability
[xcomplete]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xasyncprovider/functions/xasynccomplete
[xresult]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xasyncprovider/functions/xasyncgetresult
[xstatus]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xasync/functions/xasyncgetstatus
[tqterm]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xtaskqueue/functions/xtaskqueueterminate
[tqdisp]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xtaskqueue/functions/xtaskqueuedispatch
[xnreg]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/networking/xnetworking/functions/xnetworkingregisterconnectivityhintchanged
[xnhint]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/networking/xnetworking/structs/xnetworkingconnectivityhint
[xninit]: https://learn.microsoft.com/en-us/gaming/gdk/docs/features/console/networking/initialization-connectivity-networking
[xadd]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xuser/functions/xuseraddasync
[xgt]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xuser/functions/xusergetgamertag
[xtoken]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xuser/structs/xusergettokenandsignaturedata
[plspath]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xpersistentlocalstorage/functions/xpersistentlocalstoragegetpath
[plsprompt]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xpersistentlocalstorage/functions/xpersistentlocalstoragepromptuserforspaceasync
[xanalytics]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xsystem/structs/xsystemanalyticsinfo
[xversion]: https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/xgameruntimetypes/structs/xversion
[eb]: https://microsoft.github.io/DirectX-Specs/d3d/D3D12EnhancedBarriers.html
[indirect]: https://microsoft.github.io/DirectX-Specs/d3d/IndirectDrawing.html
[oefa]: https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device3-openexistingheapfromaddress
[shared]: https://learn.microsoft.com/en-us/windows/win32/direct3d12/shared-heaps
[rflags]: https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ne-d3d12-d3d12_resource_flags
[fmt]: https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/hardware-support-for-direct3d-12-0-formats
[limits]: https://learn.microsoft.com/en-us/windows/win32/direct3d11/overviews-direct3d-11-resources-limits
[tiers]: https://learn.microsoft.com/en-us/windows/win32/direct3d12/hardware-support
[timing]: https://learn.microsoft.com/en-us/windows/win32/direct3d12/timing
[agility]: https://devblogs.microsoft.com/directx/gettingstarted-dx12agility/
