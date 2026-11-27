# Lane `loader` — Build 3, first half: PE loader and the import census

Branch `agent/loader`, crate `crates/loader` (package `rebuild-loader`), binary `apps/rebuild/src/bin/imports.rs`.
Nothing vendored: the parser is written here from the PE/COFF specification, with no dependencies, so
there is no `CREDITS.md` line. No title code executed, no GPU used, the game folder unchanged (checked by
the tool on every run).

```
L1  PE32+ parse            closed   4,523 / 4,523 PE32+ images in System32 parse, 0 errors, 0 panics
                                    (recursive: 6,382 / 6,382); 13 unit tests on a synthetic DLL
L2  map, relocate, bind    closed   synthetic DLL maps at 4 bases and at a VirtualAlloc'd address: every
                                    pointer checks out; dbghelp.dll mapped beside the Windows loader:
                                    9,912 relocations, 0 wrong
L3  import census          closed   18 images, 93 DLLs, 962 distinct imported functions;
                                    GDK discovery list = 697 functions in 86 DLLs
L4  the title's DLLs       closed   16 DLLs with export counts; the 6 that are not the C++ runtime listed in full
L5  run-time lookups       closed   76 names (high 42 · medium 3 · low 31) in loader-getproc.csv; the 6 names
                                    the fork saw the title look up through GetProcAddress: 6 / 6 found, all high
```

Since the lane closed, the lead added `src/prepare.rs`: a patched copy of a title image that the Windows loader
maps (relocation, static TLS, unwind tables) but does not run; the runtime binds its imports and calls its TLS
callbacks and entry point itself. `runtitle` uses it; the copies go to `%LOCALAPPDATA%\triumph-rebuild\images`.

## Built

- `pe.rs`: headers, sections, base relocations (all types recorded; ABSOLUTE padding skipped), imports by
  name (with hint) and by ordinal, delay-load imports (both the RVA and the old VA form), exports with
  forwarders and ordinal-only entries, the TLS directory and its callback array, `.pdata`. Every read is
  bounds-checked through an RVA view that knows file-backed bytes from zero-fill; every table walk is
  capped. PE32 images are refused by design (`NotPe32Plus`).
- `map.rs`: `map_into(image, file, base, out, resolver, opts)`: headers and sections copied (tails zero),
  DIR64 / HIGHLOW / HIGH / LOW relocations applied, imports bound through `trait Resolver`, delay-load
  imports bound eagerly when asked (otherwise their slots keep the image's own, relocated, thunks). An
  unresolved import is returned by name in `MapReport::missing`, never filled with anything. Refuses: a
  destination smaller than `SizeOfImage`, a base not 64 KiB aligned, a move of an image with stripped
  relocations, a relocation type it does not apply.
- `region.rs` (Windows): read-write memory reserved and committed at an exact address. Never executable.
- `synth.rs` (tests): a PE32+ DLL built byte by byte: five sections, 9 DIR64 relocations, imports by name
  and ordinal from two DLLs, a delay-load import, three exports (one forwarded), a TLS callback, two
  `.pdata` entries.
- `imports` binary: `--scan <dir> [--recursive]` (L1 sweep, and L2 on every image moved away from its
  preferred base), and the census over `$REBUILD_GAME_DIR` (L3, L4; `--exports` prints every export).

## Measured

### L1 — every PE in `C:\Windows\System32`

`imports --scan C:\Windows\System32` (this machine, Windows 11 Pro 10.0.26300):

| scope | files | MZ files | PE32+ parsed | PE32 (refused) | errors | panics | mapped away from base | time |
|:--|--:|--:|--:|--:|--:|--:|--:|--:|
| top level | 4,880 | 4,561 | **4,523** | 38 | **0** | 0 | 4,522 (1 has stripped relocations) | 20.1 s, 2.75 GB |
| recursive | 21,048 | 10,440 | **6,382** | 4,058 | **0** | 0 | 6,380 (2 stripped) | 112 s, 6.38 GB |

Top level totals: 9,563,586 relocations · 673,649 imports · 87,770 delay-load imports · 317,670 exports ·
176 TLS callbacks · 5,341,503 `.pdata` entries. The 4,058 PE32 files in the recursive run are mostly
`.mui` resource images. Unit test `garbage_is_an_error_never_a_panic` also parses every truncation of the
synthetic DLL and 200,000 random two-byte corruptions of it.

### L2 — map at a chosen base

- `relocated_pointers_check_out`: the synthetic DLL at 4 bases (including its own, where 0 relocations
  apply): pointer to a string reaches the string, pointer to a function reaches its `ret`, a self pointer
  equals its own address, the TLS callback array moved, the `.data` zero-fill tail is zero, the
  unresolved delay-load slot points at the relocated thunk, and all 4 unresolved imports are named.
- `mapped_at_a_chosen_base_the_pointers_dereference`: mapped into `VirtualAlloc` memory at a chosen
  address; the relocated pointers are dereferenced as real pointers.
- `matches_the_windows_loader_on_a_system_dll`: the Windows loader maps `dbghelp.dll`
  (`DONT_RESOLVE_DLL_REFERENCES`: no DllMain, nothing bound) at an ASLR base (0x7ff8_3026_0000,
  preferred 0x1_8000_0000). We map the same file at the same base: **9,912 relocations, 0 wrong**. Of
  2,277,530 section bytes, 10 differ: 4 in section `fothk` and 6 in `.data`. Those are the loader's own
  run-time patches (CFG / dynamic value relocations from the load config), not base relocations, and are
  left for the runtime half.
- `imports_bind_through_the_resolver`, `refusals`.

### L3 — the import census

`REBUILD_GAME_DIR=<game_dir> imports` (1.6 s). Every image in the title's folder is parsed and mapped at its
preferred base, and bound twice: through a resolver that knows nothing yet, then through one that knows
only the exports of the DLLs the title ships.

```
images                         18 (game_scarlett_master.exe, 16 DLLs, and Catalog000.bin, which is an MZ image with no imports)
imported DLLs                  93
distinct imported functions    962    (1,924 import slots), all missing from the empty resolver
  from the title's own DLLs    265    (msvcp140 162, libHttpClient 38, vcruntime140 30, Bink 23, BvhBuilder 7, FidelityFX loader 4, vcruntime140_1 1)
GDK discovery list             697 functions in 86 DLLs   ← Build 3's list to implement or stub
by ordinal                     27     (ws2_32 23, oleaut32 2, gameinput #1, xaudio2_9 #1)
delay-load imports             0      (no image in the folder uses delay-load)
TLS callbacks                  2      (the exe)
the exe alone                  68 DLLs, 587 imports, 598,282 relocations; 209 bound by the title's DLLs, 378 missing
game folder                    110 files identical before and after (path, size, mtime); 18/18 images rehashed equal
```

The full table, one row per (DLL, function) with its kind, importers and resolver, is
[`loader-imports.csv`](loader-imports.csv) (names only). The console-only DLLs the discovery list names
(beyond `api-ms-win-*`, the CRT and Win32 networking):

| DLL | functions | |
|:--|--:|:--|
| `xmem.dll` | 13 | `XMemAlloc`, `XMemVirtualAlloc`, `XMemAllocatePhysicalPages`, `XMemMapPhysicalPages(Scatter)`, … — the memory model's front door |
| `d3d12_xs.dll` | 4 | `D3D12XboxCreateDevice`, `D3D12ConfigureVirtualMemoryX`, root-signature (de)serialise |
| `acphal.dll` | 9 | APU / audio shape contexts |
| `xapu.dll` | 5 | APU command queue |
| `chad.dll` | 5 | `XDsp*` |
| `xg_xs.dll` | 2 | `XGCreateTextureComputer`, `XGSuggestSwizzleMode` |
| `xcrdapi.dll` | 1 | `DStorageGetFactory` |
| `gameinput.dll` | 1 | ordinal #1 |
| `mfplat.dll`, `mfreadwrite.dll` | 6 + 1 | Media Foundation |

### L4 — the title's own DLLs

| DLL | exports | imported statically by the title's images | what |
|:--|--:|--:|:--|
| `Bink2Scarlett.dll` | 74 | 23 (exe) | Bink 2 video |
| `amd_fidelityfx_loader_xbox.dll` | 5 | 4 (exe: `ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`, `ffxQuery`) | FidelityFX loader |
| `amd_fidelityfx_upscaler_xbox.dll` | 5 | 0 (loaded at run time by the loader, if at all) | FidelityFX upscaler |
| `libHttpClient.GDK.dll` | 129 | 38 (exe) | GDK HTTP |
| `XCurl.dll` | 61 | 0 | curl for GDK |
| `XboxBvhBuilder.dll` | 12 | 7 (exe) | ray-tracing BVH builder |
| `msvcp140.dll` | 1,493 | 162 | C++ runtime |
| `vcruntime140.dll` | 71 | 30 | |
| `vcruntime140_1.dll` | 3 | 1 | |
| `concrt140.dll` | 291 | 0 | |
| `vccorlib140.dll` | 256 | 0 | |
| `vcomp140.dll` | 113 | 0 | OpenMP |
| `msvcp140_2.dll` | 44 | 0 | |
| `msvcp140_atomic_wait.dll` | 32 | 0 | |
| `msvcp140_1.dll` | 7 | 0 | |
| `msvcp140_codecvt_ids.dll` | 4 | 0 | |

All exports are named; none forwarded, none ordinal-only. The six that are not the C++ runtime, in full
(**imported** = some image in the folder imports it):

<details><summary><code>Bink2Scarlett.dll</code></summary>

- `BinkAllocateFrameBuffers`
- `BinkClose` **(imported)**
- `BinkCloseTrack`
- `BinkControlBackgroundIO`
- `BinkCopyToBuffer`
- `BinkCopyToBufferRect`
- `BinkCurrentSubtitle`
- `BinkDoFrame` **(imported)**
- `BinkDoFrameAsync`
- `BinkDoFrameAsyncMulti` **(imported)**
- `BinkDoFrameAsyncWait` **(imported)**
- `BinkDoFramePlane`
- `BinkFindXAudio2WinDevice`
- `BinkFreeGlobals` **(imported)**
- `BinkGetError`
- `BinkGetFrameBuffersInfo` **(imported)**
- `BinkGetGPUDataBuffersInfo`
- `BinkGetKeyFrame` **(imported)**
- `BinkGetPlatformInfo` **(imported)**
- `BinkGetRealtime`
- `BinkGetRects`
- `BinkGetSubtitleByIndex`
- `BinkGetSummary`
- `BinkGetTrackData`
- `BinkGetTrackID`
- `BinkGetTrackMaxSize`
- `BinkGetTrackType`
- `BinkGoto` **(imported)**
- `BinkLoadSubtitles`
- `BinkLogoAddress`
- `BinkNextFrame` **(imported)**
- `BinkOpen`
- `BinkOpenTrack`
- `BinkOpenWithOptions` **(imported)**
- `BinkOpenXAudio2`
- `BinkPause` **(imported)**
- `BinkRegisterFrameBuffers` **(imported)**
- `BinkRegisterGPUDataBuffers`
- `BinkRequestStopAsyncThread`
- `BinkRequestStopAsyncThreadsMulti` **(imported)**
- `BinkService`
- `BinkSetError`
- `BinkSetFileOffset`
- `BinkSetFrameRate`
- `BinkSetIO`
- `BinkSetIOSize`
- `BinkSetMemory` **(imported)**
- `BinkSetOSFileCallbacks` **(imported)**
- `BinkSetPan`
- `BinkSetSimulate`
- `BinkSetSoundOnOff`
- `BinkSetSoundSystem` **(imported)**
- `BinkSetSoundSystem2`
- `BinkSetSoundTrack`
- `BinkSetSpeakerVolumes`
- `BinkSetVideoOnOff`
- `BinkSetVolume` **(imported)**
- `BinkSetWillLoop` **(imported)**
- `BinkShouldSkip` **(imported)**
- `BinkStartAsyncThread` **(imported)**
- `BinkUtilCPUs`
- `BinkUtilFree`
- `BinkUtilMalloc`
- `BinkUtilMutexCreate`
- `BinkUtilMutexDestroy`
- `BinkUtilMutexLock`
- `BinkUtilMutexLockTimeOut`
- `BinkUtilMutexUnlock`
- `BinkUtilSoundGlobalLock`
- `BinkUtilSoundGlobalUnlock`
- `BinkWait` **(imported)**
- `BinkWaitStopAsyncThread`
- `BinkWaitStopAsyncThreadsMulti` **(imported)**
- `RADTimerRead`

</details>

<details><summary><code>XCurl.dll</code></summary>

- `curl_easy_cleanup`
- `curl_easy_duphandle`
- `curl_easy_escape`
- `curl_easy_getinfo`
- `curl_easy_init`
- `curl_easy_perform`
- `curl_easy_reset`
- `curl_easy_setopt`
- `curl_easy_strerror`
- `curl_easy_unescape`
- `curl_escape`
- `curl_formadd`
- `curl_formfree`
- `curl_formget`
- `curl_free`
- `curl_getdate`
- `curl_global_cleanup`
- `curl_global_init`
- `curl_global_init_mem`
- `curl_global_sslset`
- `curl_mime_addpart`
- `curl_mime_data`
- `curl_mime_data_cb`
- `curl_mime_encoder`
- `curl_mime_filedata`
- `curl_mime_filename`
- `curl_mime_free`
- `curl_mime_headers`
- `curl_mime_init`
- `curl_mime_name`
- `curl_mime_subparts`
- `curl_mime_type`
- `curl_multi_add_handle`
- `curl_multi_cleanup`
- `curl_multi_info_read`
- `curl_multi_init`
- `curl_multi_perform`
- `curl_multi_poll`
- `curl_multi_remove_handle`
- `curl_multi_setopt`
- `curl_multi_strerror`
- `curl_multi_wait`
- `curl_multi_wakeup`
- `curl_share_cleanup`
- `curl_share_init`
- `curl_share_setopt`
- `curl_share_strerror`
- `curl_slist_append`
- `curl_slist_free_all`
- `curl_unescape`
- `curl_url`
- `curl_url_cleanup`
- `curl_url_dup`
- `curl_url_get`
- `curl_url_set`
- `curl_version`
- `curl_version_info`
- `xcurl_global_init_mem`
- `xcurl_global_resume`
- `xcurl_global_set_request_limit`
- `xcurl_global_suspend`

</details>

<details><summary><code>XboxBvhBuilder.dll</code></summary>

- `??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z` **(imported)**
- `??1XboxBvhBuilder@@QEAA@XZ` **(imported)**
- `?BuildRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@IPEBUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC@@@Z`
- `?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z` **(imported)**
- `?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z` **(imported)**
- `?CpuValidateResultX@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z`
- `?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z` **(imported)**
- `?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z` **(imported)**
- `?RehydrateRaytracingAccelerationStructureBatchedX@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z`
- `?RehydrateRaytracingAccelerationStructureX@XboxBvhBuilder@@QEAAJPEAUID3D12Device11@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z`
- `?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z` **(imported)**
- `?ValidateRehydrateAccelerationStructureBuildDesc@XboxBvhBuilder@@QEAAJPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z`

</details>

<details><summary><code>amd_fidelityfx_loader_xbox.dll</code></summary>

- `ffxConfigure`
- `ffxCreateContext` **(imported)**
- `ffxDestroyContext` **(imported)**
- `ffxDispatch` **(imported)**
- `ffxQuery` **(imported)**

</details>

<details><summary><code>amd_fidelityfx_upscaler_xbox.dll</code></summary>

- `ffxConfigure`
- `ffxCreateContext`
- `ffxDestroyContext`
- `ffxDispatch`
- `ffxQuery`

</details>

<details><summary><code>libHttpClient.GDK.dll</code></summary>

- `HCAddCallRoutedHandler`
- `HCAddWebSocketRoutedHandler`
- `HCCleanup`
- `HCCleanupAsync` **(imported)**
- `HCGetHttpCallPerformFunction`
- `HCGetLibVersion`
- `HCGetWebSocketConnectResult`
- `HCGetWebSocketFunctions`
- `HCGetWebSocketSendMessageResult`
- `HCHttpCallCloseHandle` **(imported)**
- `HCHttpCallCreate` **(imported)**
- `HCHttpCallDuplicateHandle` **(imported)**
- `HCHttpCallGetContext`
- `HCHttpCallGetId`
- `HCHttpCallGetPerformCount`
- `HCHttpCallGetRequestUrl` **(imported)**
- `HCHttpCallPerformAsync` **(imported)**
- `HCHttpCallRequestAddDynamicBytesWritten`
- `HCHttpCallRequestEnableGzipCompression`
- `HCHttpCallRequestGetDynamicBytesWritten`
- `HCHttpCallRequestGetHeader`
- `HCHttpCallRequestGetHeaderAtIndex` **(imported)**
- `HCHttpCallRequestGetMaxReceiveBufferSize`
- `HCHttpCallRequestGetNumHeaders` **(imported)**
- `HCHttpCallRequestGetRequestBodyBytes` **(imported)**
- `HCHttpCallRequestGetRequestBodyReadFunction`
- `HCHttpCallRequestGetRequestBodyString`
- `HCHttpCallRequestGetRetryAllowed` **(imported)**
- `HCHttpCallRequestGetRetryCacheId` **(imported)**
- `HCHttpCallRequestGetRetryDelay` **(imported)**
- `HCHttpCallRequestGetTimeout` **(imported)**
- `HCHttpCallRequestGetTimeoutWindow` **(imported)**
- `HCHttpCallRequestGetUrl` **(imported)**
- `HCHttpCallRequestSetDynamicSize`
- `HCHttpCallRequestSetHeader` **(imported)**
- `HCHttpCallRequestSetMaxReceiveBufferSize`
- `HCHttpCallRequestSetProgressReportFunction`
- `HCHttpCallRequestSetRequestBodyBytes` **(imported)**
- `HCHttpCallRequestSetRequestBodyReadFunction`
- `HCHttpCallRequestSetRequestBodyString` **(imported)**
- `HCHttpCallRequestSetRetryAllowed` **(imported)**
- `HCHttpCallRequestSetRetryCacheId` **(imported)**
- `HCHttpCallRequestSetRetryDelay` **(imported)**
- `HCHttpCallRequestSetSSLValidation`
- `HCHttpCallRequestSetTimeout` **(imported)**
- `HCHttpCallRequestSetTimeoutWindow` **(imported)**
- `HCHttpCallRequestSetUrl` **(imported)**
- `HCHttpCallResponseAddDynamicBytesWritten`
- `HCHttpCallResponseAppendResponseBodyBytes`
- `HCHttpCallResponseGetDynamicBytesWritten`
- `HCHttpCallResponseGetHeader` **(imported)**
- `HCHttpCallResponseGetHeaderAtIndex` **(imported)**
- `HCHttpCallResponseGetNetworkErrorCode` **(imported)**
- `HCHttpCallResponseGetNumHeaders` **(imported)**
- `HCHttpCallResponseGetPlatformNetworkErrorMessage` **(imported)**
- `HCHttpCallResponseGetResponseBodyBytes` **(imported)**
- `HCHttpCallResponseGetResponseBodyBytesSize` **(imported)**
- `HCHttpCallResponseGetResponseBodyWriteFunction`
- `HCHttpCallResponseGetResponseString` **(imported)**
- `HCHttpCallResponseGetStatusCode` **(imported)**
- `HCHttpCallResponseSetDynamicSize`
- `HCHttpCallResponseSetGzipCompressed`
- `HCHttpCallResponseSetHeader`
- `HCHttpCallResponseSetHeaderWithLength`
- `HCHttpCallResponseSetNetworkErrorCode`
- `HCHttpCallResponseSetPlatformNetworkErrorMessage`
- `HCHttpCallResponseSetResponseBodyBytes`
- `HCHttpCallResponseSetResponseBodyWriteFunction`
- `HCHttpCallResponseSetStatusCode`
- `HCHttpCallSetContext`
- `HCHttpCallSetTracing` **(imported)**
- `HCHttpDisableAssertsForSSLValidationInDevSandboxes`
- `HCInitialize` **(imported)**
- `HCIsInitialized`
- `HCMemGetFunctions`
- `HCMemSetFunctions`
- `HCMockAddMock`
- `HCMockCallCloseHandle`
- `HCMockCallCreate`
- `HCMockCallDuplicateHandle`
- `HCMockClearMocks`
- `HCMockRemoveMock`
- `HCMockResponseSetHeader`
- `HCMockResponseSetNetworkErrorCode`
- `HCMockResponseSetResponseBodyBytes`
- `HCMockResponseSetStatusCode`
- `HCMockSetMockMatchedCallback`
- `HCRemoveCallRoutedHandler`
- `HCRemoveWebSocketRoutedHandler`
- `HCSetGlobalProxy`
- `HCSetHttpCallPerformFunction`
- `HCSetWebSocketFunctions`
- `HCSettingsGetTraceLevel` **(imported)**
- `HCSettingsSetTraceLevel`
- `HCTraceCleanup`
- `HCTraceImplMessage` **(imported)**
- `HCTraceImplMessage_v`
- `HCTraceImplScopeId`
- `HCTraceInit`
- `HCTraceSetClientCallback`
- `HCTraceSetEtwEnabled` **(imported)**
- `HCTraceSetPlatformCallbacks`
- `HCTraceSetTraceToDebugger`
- `HCWebSocketCloseHandle`
- `HCWebSocketConnectAsync`
- `HCWebSocketCreate`
- `HCWebSocketDisconnect`
- `HCWebSocketDisconnectWithStatus`
- `HCWebSocketDuplicateHandle`
- `HCWebSocketGetBinaryMessageFragmentEventFunction`
- `HCWebSocketGetEventFunctions`
- `HCWebSocketGetHeader`
- `HCWebSocketGetHeaderAtIndex`
- `HCWebSocketGetNumHeaders`
- `HCWebSocketGetNumResponseHeaders`
- `HCWebSocketGetPingInterval`
- `HCWebSocketGetProxyUri`
- `HCWebSocketGetResponseHeader`
- `HCWebSocketGetResponseHeaderAtIndex`
- `HCWebSocketSendBinaryMessageAsync`
- `HCWebSocketSendMessageAsync`
- `HCWebSocketSetBinaryMessageFragmentEventFunction`
- `HCWebSocketSetHeader`
- `HCWebSocketSetMaxReceiveBufferSize`
- `HCWebSocketSetOptions`
- `HCWebSocketSetPingInterval`
- `HCWebSocketSetProxyUri`
- `HCWinHttpResume`
- `HCWinHttpSuspend`

</details>

### L5 — names the title looks up at run time

`REBUILD_GAME_DIR=<game_dir> imports --getproc` (0.9 s; `crates/loader/src/strings.rs`, 3 tests, plus
`Image::table_strings` with 1 test). Every ASCII and UTF-16LE string of 4+ characters in the 18 images
(15,066 strings, after removing the strings the import/export tables point at and the resource section;
2,812 look like API names). A DLL-name string within 512 bytes of an API-like name is the evidence of a
lookup table or a `LoadLibrary` + `GetProcAddress` pair. Exports are checked against this PC's System32
(API sets as `kernelbase`, `gcombase` as `combase`; `xgameruntime.dll` and `gameplatformservices.dll`
are on this PC) and the title's own DLLs. A DLL never confirms strings in its own image.

| confidence | rule | names |
|:--|:--|--:|
| high | next to a DLL name, and a DLL this image names exports it | **42** |
| medium | next to a DLL name we cannot check, or exported by a DLL this image names with no DLL name near | **3** |
| low | a console API family name (`XStore*`, `XUser*`, `Xbl*`, `HC*`, …) with nothing confirming: most likely log or assert strings | **31** |

What the high names are:

| from | names | looked up by |
|:--|:--|:--|
| `xgameruntime.dll` | `InitializeApiImpl`, `InitializeApiImplEx`, `InitializeApiImplEx2`, `UninitializeApiImpl`, `QueryApiImpl`, `XErrorReport` | the exe, libHttpClient, XCurl (the GDK runtime stub each links) |
| `gameplatformservices.dll` | `GetApiVersionImpl`, `RestartApiImpl`, `RundownApiImpl` | XCurl |
| `amd_fidelityfx_upscaler_xbox.dll` | `ffxConfigure`, `ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`, `ffxQuery` | the FidelityFX loader |
| `winhttp.dll` | 6 × `WinHttpWebSocket*` | libHttpClient |
| `icu.dll` | 14 × `ucal_*` / `uenum_*` (time zones) | `msvcp140_atomic_wait.dll` |
| Win32 | `GetPackagesByPackageFamily`, `GetCurrentPackageId`, `Get/SetThreadPreferredUILanguages`, `RegGetValueW`, `MiniDumpWriteDump`, `RtlDllShutdownInProgress`, `RoOriginateLanguageException` | various |

So the GDK families (`XGame*`, `XUser*`, `XAsync*`, `XTaskQueue*`, `XStore*`) are **not** looked up by name.
The title takes the 6 `xgameruntime` entry points by name, then reaches every area through `QueryApiImpl`
objects (the fork's design notes say the same). The `XStore*` / `XUser*` / `Xbl*` names in the exe sit
with no DLL nearby (low): they tell which areas the title uses, not names it resolves.

**Check against the fork.** The 133 names the fork's runtime exports (`hle_export!` across
`victory-hle`, names only, read through git) include 44 that no image imports statically. All 6 that the
fork documents as `GetProcAddress` lookups are found here, all high. The other 38 are not strings in
the title (most are `ws2_32` functions the exe imports **by ordinal**, plus Win32 file and HTTP-proxy calls
the fork implemented in general), so they are not misses of this scan. 45 high + medium names here; 6 are
in the fork's list. The other 39 are names the fork's runtime never had to answer by name (WinHttp
WebSocket, ICU, the FidelityFX upscaler, gameplatformservices) or does not export yet.

DLL names the title's images carry as strings, beyond those it imports: `xgameruntime.dll` (3),
`gameplatformservices.dll`, `icu.dll`, `dbghelp.dll`, `rpcrt4.dll`, `appmodel.dll`, `gcombase.dll`,
`vcomp120ui.dll`, `amd_fidelityfx_framegeneration_xbox.dll` (named by the FidelityFX loader, **not
shipped**), `amd_fidelityfx_upscaler_xbox.dll`.

[`loader-getproc.csv`](loader-getproc.csv): name, confidence, nearest DLL name and distance, exporters
found, whether it is also imported statically, encoding, occurrences, images. Names only. The game
folder was identical before and after (110 files; 18/18 images rehashed).

## Decided

- No vendoring: the fork's `victory-pe` wraps the `object` crate; a dependency-free parser was smaller than
  vendoring both, and System32 shows it holds.
- Delay-load imports are bound eagerly when the runtime asks (`MapOptions::bind_delay`), so the title's
  delay-load helper never has to run. The title itself uses none.
- Unresolved imports are reported, never patched with anything here; the stub that names a call at run
  time belongs to the runtime half.

## Next (Build 3 second half, the lead's)

- Run the 2 TLS callbacks and the entry point on the lead's memory model; apply the load-config dynamic
  relocations / CFG pointers the Windows comparison showed (10 bytes in `dbghelp.dll`).
- Resolvers in order: the title's DLLs (265 functions, done here), then our runtime for the 697.
- Register `.pdata` (`RtlAddFunctionTable`) for the mapped images so exceptions unwind.

## For the lead

1. **`plan.md` line 120 (decision 5)**: "Run as a Series X, the title never loads the FidelityFX loader."
   The census shows `game_scarlett_master.exe` imports `amd_fidelityfx_loader_xbox.dll` **statically**
   (`ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch`, `ffxQuery`) and the exe has no delay-load
   imports, so the loader DLL is mapped at process start. It may never be *called* as a Series X, but it
   is loaded, and Build 3 must resolve it. The upscaler DLL is imported by nothing (0 static importers).
2. **GDK runtime functions are not in the static imports.** No `xgameruntime.dll` / `XGame*` / `XUser*`
   import appears in any image; the title must reach them through `GetProcAddress` / `LoadLibraryExW`
   (both in its imports). The discovery loop (tasks.md Build 3, "name every unimplemented call") needs
   to catch `GetProcAddress` misses too, or the 697 undercounts what the title calls.
3. `README.md` badge and table say 35 tests; with this branch it is **48** (`crates/loader` adds 13).
4. Two warnings in the lead's crate on every build: `crates/uma/src/tileproof.rs:572` (unused `mut`) and
   `crates/uma/src/upload.rs:560` (field `len` never read).
5. **L5, for the run-time resolver**: `GetProcAddress` must answer by name for `xgameruntime.dll` (6),
   `gameplatformservices.dll` (3), `winhttp.dll` (6 WebSocket), `icu.dll` (14) and, if the FidelityFX
   loader is ever called, `amd_fidelityfx_upscaler_xbox.dll` (5, which ships) and
   `amd_fidelityfx_framegeneration_xbox.dll` (named by the loader, not shipped: its `LoadLibrary` must fail
   cleanly). Item 2 above is answered: GDK areas come through `QueryApiImpl`, not through names.
6. `apps/rebuild/Cargo.toml` gained `rebuild-loader` as a dependency (for the `imports` binary), and the
   root `Cargo.toml` `members` gained `crates/loader`.
