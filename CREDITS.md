# Credits

Code in this repository derived from other projects, with its licence and what was changed. Every
such file carries a header saying the same. "VIctory" is VIctory (Marvisak and contributors) via the owner's
fork jleybourne/Triumph, GPL-3.0-or-later, used here under GPL-3.0-only.

| file | from | licence | changes |
|:--|:--|:--|:--|
| `crates/shader/src/dxbc.rs` | VIctory, `victory-shader/src/dxbc.rs` | GPL-3.0-or-later, used under GPL-3.0-only | none |
| `crates/shader/src/native.rs` | VIctory, `victory-shader/src/native.rs` | GPL-3.0-or-later, used under GPL-3.0-only | also keeps the record's SGPR count and LDS bytes |
| `crates/shader/src/sgd5.rs` | VIctory, `victory-game/src/sgd5.rs` | GPL-3.0-or-later, used under GPL-3.0-only | none (moved crate) |
| `crates/shader/src/gfx10/mod.rs` | VIctory, `victory-shader/src/gfx10/mod.rs` | GPL-3.0-or-later, used under GPL-3.0-only | operand-field and SP3 printer modules not vendored; `name()` public |
| `crates/shader/src/gfx10/opcodes.rs` | VIctory, generated from Mesa's `aco_opcodes.py` (Valve Corporation, MIT notice kept in the file) | GPL-3.0-or-later / MIT, used under GPL-3.0-only | tables `pub(crate)`; the test no longer builds the fork's IR |
| `crates/xg/src/layout.rs` | VIctory, `victory-formats/src/texture.rs` (itself following AMD's addrlib, MIT notice kept in the file) | GPL-3.0-or-later / MIT, used under GPL-3.0-only | the GFX10 layout maths: block dimensions, mip placement and tails, micro tile patterns, element offsets, the DXGI element table |
| `crates/audio/src/wma/tables.rs` | VIctory, `victory-formats/src/xma/tables.rs` (itself converted from FFmpeg's `libavcodec/wmaprodata.h`, LGPL-2.1-or-later) | GPL-3.0-or-later / LGPL-2.1-or-later, used under GPL-3.0-only | none: data only (the WMA Pro code and level tables); the decoder in `crates/audio/src/wma` is ours |
| `crates/dstorage/src/bcpack/consts.rs` | VIctory, `victory-formats/src/bcpack.rs` | GPL-3.0-or-later, used under GPL-3.0-only | constant data only (lane widths, BC6H/BC7 layout tables, the console's BC6H run order); the BCPack decoder in `crates/dstorage/src/bcpack` is ours |
| `crates/xg/src/computer.rs` | VIctory, `victory-hle/src/modules/xg.rs` | GPL-3.0-or-later, used under GPL-3.0-only | the `xg_xs.dll` structure layouts, slot meanings and suggestion rule, rewritten as plain Rust methods |

## Fonts and libraries

Third-party work this repository includes or embeds, and its licence.

| what | where | licence |
|:--|:--|:--|
| Inter 3.019 (Regular, Medium, SemiBold, Bold), The Inter Project Authors, https://github.com/rsms/inter | `apps/launcher/assets/fonts/` (licence text: `Inter-OFL.txt`) | SIL Open Font License 1.1 |
| Phosphor Icons, via the `egui-phosphor` crate | linked by `apps/launcher` | MIT |
| Hack, Source Foundry, bundled with egui | linked by `apps/launcher` | MIT and Bitstream Vera licence |
| egui and eframe, Emil Ernerfeldt and contributors | linked by `apps/launcher` | MIT or Apache-2.0 |
| egui's default fonts (`epaint_default_fonts`: Hack, Ubuntu Light, Noto Emoji, emoji-icon-font) | linked by `apps/launcher` | (MIT or Apache-2.0) and OFL-1.1 and Ubuntu Font Licence 1.0 |
| every other crate the two exes link (116 at v0.1.0; the full list and each crate's licence files are generated into the zip's `licenses/THIRD-PARTY-LICENSES.txt`) | linked by `apps/launcher`, `apps/rebuild` | MIT, Apache-2.0, BSD, Zlib, ISC, BSL-1.0, Unicode-3.0, CC0 and similar permissive licences, all GPL-3.0 compatible |

## The release zip

What `scripts/package.ps1` puts in `triumph-rebuild-<version>-windows-x64.zip`, file by file. The script
refuses to package anything else, and refuses outright if the repository or the zip would hold game data or a
shader container (see the script's header). The zip's `FILES.txt` repeats this list with every file's SHA-256.

| file | what | licence |
|:--|:--|:--|
| `rebuild-launcher.exe` | the launcher (`apps/launcher`) | GPL-3.0-only, with the crates above |
| `rebuild.exe`, `rebuild.pdb` | the layer Play starts (`apps/rebuild`), and its line tables for crash reports | GPL-3.0-only, with the crates above |
| `dxc.exe`, `dxcompiler.dll` | Microsoft's DirectX Shader Compiler v1.8.2502, unmodified, from its GitHub release (`dxc_2025_02_20.zip`, SHA-256 pinned in the script): the layer runs `dxc.exe` as a separate process | University of Illinois/NCSA (LLVM) and MIT: `licenses/DXC-LICENSE-LLVM.txt`, `licenses/DXC-LICENSE-MIT.txt` |
| `dxil.dll` | DXC's validator and signer, from the same release, loaded only by `dxcompiler.dll` | Microsoft's DirectX Shader Compiler licence terms, which allow it to be distributed with an application that adds significant function: `licenses/DXC-LICENSE-MS.txt`. Microsoft also lists `dxil.dll` and `dxcompiler.dll` as redistributable in the Windows SDK's REDIST list (learn.microsoft.com/legal/windows-sdk/redist) |
| `LICENSE.txt`, `CREDITS.md`, `README.txt`, `FILES.txt`, `licenses/` | this project's licence, this file, a short readme, the file list, and every licence text above (`Inter-OFL.txt`, `DXC-*.txt`, `THIRD-PARTY-LICENSES.txt`) | |

DXC runs in its own process, so the GPL-3.0 program and Microsoft's `dxil.dll` are an aggregate on one medium,
not a combined work.

**Not bundled, by decision:**

| what | why |
|:--|:--|
| NVIDIA Streamline (`sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll`) and DLSS (`nvngx_dlss.dll`) | DLSS is NVIDIA's proprietary runtime under its own SDK licence, with terms this GPL project should not pass on for you; the upscaler is also not wired into the title yet. You can point the layer at your own copies (`upscaler.streamline_dir`, `upscaler.dlss_dll`) |
| AMD FidelityFX runtime DLLs (`amd_fidelityfx_*_dx12.dll`) | the SDK is MIT, but the signed runtime DLLs ship from AMD's SDK releases and the upscaler is not wired into the title yet; optional, `upscaler.fidelityfx_dir` |
| anything from a game: its files, saves, shader archive, translated shaders, pipeline library, BCPack tables | never in the repository, a CI artifact or a release |
