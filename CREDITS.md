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
| `crates/xg/src/computer.rs` | VIctory, `victory-hle/src/modules/xg.rs` | GPL-3.0-or-later, used under GPL-3.0-only | the `xg_xs.dll` structure layouts, slot meanings and suggestion rule, rewritten as plain Rust methods |
