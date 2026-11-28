<h1 align="center">Features</h1>

<p align="center"><b>What triumph-rebuild does today, what is being built, and what comes next.</b><br>
<sub>✅ works on the card &nbsp;·&nbsp; 🔄 being built &nbsp;·&nbsp; ⏳ planned &nbsp;·&nbsp; measured on an RTX 4070 Ti, i7-10700KF, 64 GB</sub></p>

<p align="center">
  <a href="#-running-the-game">Running the game</a> ·
  <a href="#-graphics">Graphics</a> ·
  <a href="#-display">Display</a> ·
  <a href="#-launcher">Launcher</a> ·
  <a href="#-saves-and-data">Saves and data</a> ·
  <a href="#-in-game">In game</a> ·
  <a href="#-audio-and-input">Audio and input</a> ·
  <a href="#-online">Online</a> ·
  <a href="#-for-developers">For developers</a>
</p>

> [!NOTE]
> **Where the game is:** the title boots, plays its intro video, and runs its first-run menus readably at its own
> 30 fps cap (about 260 fps with the cap off). Gameplay is not reached yet. See [progress](progress.md).

---

## 🎮 Running the game

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Runs the title's own code** | the console build is native x86-64: its exe and DLLs load through our loader, 1,924 of 1,972 imports bound |
| ✅ | **Console system services** | user sign-in (offline user), game saves (XGameSave), packages, storage drives, speech (silent), timers and threads |
| ✅ | **Intel CPUs too** | AMD-only SSE4a instructions the title uses are emulated |
| ✅ | **Guarded runs** | one instance at a time, a memory budget scaled to installed RAM, a watchdog, and a crash reporter |
| ✅ | **The title's files stay untouched** | the game folder is read-only; anything the title writes goes to its own data folder |
| 🔄 | **Gameplay** | the menus are done; the next step is driving setup through to the story start |

## 🖼️ Graphics

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Xbox Direct3D 12 on PC Direct3D 12** | devices, queues, command lists, pipelines, descriptors and present |
| ✅ | **Console memory at console addresses** | GPU memory lives in VRAM and is never copied back; only what the CPU changed is uploaded |
| ✅ | **Idle memory not scanned** | memory nobody writes stops costing time: upload pass 12.5 → 2.3 ms a frame |
| ✅ | **The title's 1.2 million descriptor slots** | paged onto the PC's 1 million limit |
| ✅ | **Depth buffers written by compute** | every one gets a twin the shaders can write; 0 unusable views |
| ✅ | **Textures written by the CPU** | video frames, font glyphs and UI gradients reach the GPU |
| ✅ | **Bink 2 video** | the intro video plays |
| 🔄 | **Tiled textures written by the CPU** | 606 textures in the console's tile layout; needed for streamed world textures |
| 🔄 | **Shaders** | RDNA2 shaders translated to DXIL: 12,981 of 25,694 compile, 5,195 checked bit-exact |
| 🔄 | **Shader library built on your PC** | built once from your own copy of the game; nothing translated is shipped |
| ⏳ | **Upscaling** | DLSS on NVIDIA, FSR on everything else; both run standalone, wiring to the title is next |

## 🖥️ Display

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Windowed** | 120p, 240p, 480p, **720p (default)**, 1080p, 1440p or 2160p; never larger than the screen |
| ✅ | **Windowed fullscreen** | a borderless window over the whole screen |
| 🔄 | **Fullscreen** | exclusive fullscreen; today it behaves as windowed fullscreen |
| ✅ | **Frame rate** | the title's own pacing (30), a cap of your choice, or unlimited; vsync on or off |
| ✅ | **One window** | the title's own window never appears; you see one game window |

## 🚀 Launcher

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Library** | add a game folder; each game keeps its own settings, saves and caches |
| ✅ | **Recommended for this PC** | reads your graphics card, video memory, RAM, CPU and screen, and picks resolution and upscaler settings; applied once on first run, never over your own changes |
| ✅ | **Settings** | every option in one registry, validated, with its measured effect; changes that need a restart say so |
| ✅ | **Profiles** | save and switch sets of settings; built-in test profiles (console class, mid and low PCs, stress) |
| ✅ | **Hardware limits for testing** | cap RAM, VRAM, CPU cores or CPU time to see how a weaker machine would run |
| ✅ | **Title settings** | the title's own `settings.xml` is edited as a copy; the original is never touched |
| ✅ | **Mods** | enable, order, and see conflicts; mods overlay the game's files without changing them |
| ✅ | **Saves** | the save editor, below |
| ✅ | **Runs and diagnostics** | every session's log and numbers; a system check with a fix line for each failure; a "This PC" card |
| ✅ | **Storage** | where everything lives, sizes, open folder, clear cache, export and import |

## 💾 Saves and data

| | feature | notes |
|:-:|:--|:--|
| ✅ | **One data folder** | portable: a `data` folder beside the launcher holds everything; falls back to your user folders when the launcher can't write there; caches can move to another drive |
| ✅ | **Saves kept apart from caches** | clearing caches can never touch a save |
| ✅ | **Export and import** | a game's saves and/or settings as one zip, checked file by file; import shows what it will replace and backs up first |
| ✅ | **Save editor** | the title's save format decoded from its own code; edit settings saves field by field (volume, field of view, HUD, controls…) and player saves by value |
| ✅ | **Safe by design** | a backup before every write, undo, never while the game runs, and nothing written the game would reject |
| ✅ | **Import from Triumph** | brings saves from the older Triumph build across, leaving its folder untouched |
| ⏳ | **Story-progress saves** | these are encrypted and signed by the title, so they are shown read-only |

## 🕹️ In game

| | feature | notes |
|:-:|:--|:--|
| ✅ | **In-game menu** | F10 or View+Menu: live settings (vsync, frame rate, cap), stats |
| ✅ | **Performance monitor** | fps, frame and GPU time, draws, CPU, RAM, VRAM and upload on screen; on by default, hidden with one setting |

## 🔊 Audio and input

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Controller** | through our own GameInput |
| ✅ | **Keyboard** | mapped to the controller |
| 🔄 | **Audio** | checking every audio path the title uses (music, effects, video sound) and filling the gaps |

## 🌐 Online

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Offline only** | the layer plays offline; an optional sandbox blocks all network access except the local machine |
| — | **Online play** | not supported and not planned |

## 🛠️ For developers

| | feature | notes |
|:-:|:--|:--|
| ✅ | **Frame line** | one line a second: fps, frame time, GPU time, draws, upload, translation |
| ✅ | **Scripted input and captures** | `REBUILD_INPUT_SCRIPT="40s:A,60s:cap"`: timed presses and full-resolution captures, without the overlays unless asked |
| 🔄 | **Live control** | a pipe to press buttons and take captures while the game runs |
| ✅ | **Tests** | 786 tests; GPU tests run on WARP, so no card is needed |
| ✅ | **Shader checker** | every translated shader compared bit-for-bit against a reference on WARP |
| ✅ | **Traces** | lists, textures, descriptors, Bink, files, saves: each behind its own switch |

---

<p align="center"><sub>Every ✅ above has a test or a measured run behind it: see <a href="runs/README.md">runs</a> and
<a href="tasks.md">tasks</a>. Plans and the reasons behind them: <a href="plan.md">plan</a>.</sub></p>
