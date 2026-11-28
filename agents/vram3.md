# Lane `vram3` — the world load under the card's budget

Branch `agent/vram3`, on master 620f8fe (vram #78, vram2 #81), then origin/master 5d15d84 merged. Card RTX 4070 Ti
12 GB, budget ~11.05 GiB. Starting point (vram2, run `v2-c`, `REBUILD_MAPPED_TEXTURES=1`): the world load ran with no
GPU hang and reached **10.8 of 11.05 GB**; frames stopped ~122 s, title fatal `exe+0x23cf683` (null write) ~139 s.

## Where v2-c's 10.8 GB went (vram2's census) and what each item can give

| item | MiB | what this lane does | expected |
|:--|--:|:--|--:|
| card pages under streamed textures: pool pages | ~2,640 | (the console's own cost) | 0 |
| ... host tiles with no console page ("private") | ~700 (11,190 tiles) | **derived tiles**: NULL while their mips' pages are unmapped, shared by textures of one layout over the same pool pages | see below |
| title-memory tiles | 1,394 | — | 0 |
| render targets in other memory | 3,005 (console 2,804, covered 2,804) | **aliasing heaps** | ~0: no two live ones overlap on the console |
| render targets in watched memory | 1,160 (console 1,119, covered 957) | aliasing heaps | up to ~160 |
| placed views, twins, rest | 782 + 322 + ~570 driver | — (census now says how much of the placed views lies over targets) | ? |

**The 3.0 GB of targets is not aliasing the host failed to honour**: vram2's census shows their console ranges cover
2,804 MiB, the sum of their sizes, so the console pays 2.8 GB for them too. Aliasing can only give back console
overlap that is live at once: ~160 MiB (watched memory) per the census. The census now prints the overlap across
origins per memory kind, and how many textures the GPU does not write lie over one it does (a view of a target's
memory with a host texture of its own: memory paid twice, and a view that does not see the target's writes).
