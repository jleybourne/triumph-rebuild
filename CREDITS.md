# Credits

Code in this repository derived from other projects, with its licence and what was changed. Every
such file carries a header saying the same.

| file | from | licence | changes |
|:--|:--|:--|:--|
| `crates/shader/src/dxbc.rs` | VIctory (Marvisak and contributors) via jleybourne/Triumph, `victory-shader/src/dxbc.rs` | GPL-3.0-or-later, used under GPL-3.0-only | none |
| `crates/shader/src/native.rs` | the same, `victory-shader/src/native.rs` | GPL-3.0-or-later, used under GPL-3.0-only | also keeps the record's SGPR count and LDS bytes |
| `crates/shader/src/sgd5.rs` | the same, `victory-game/src/sgd5.rs` | GPL-3.0-or-later, used under GPL-3.0-only | none (moved crate) |
| `crates/shader/src/gfx10/mod.rs` | the same, `victory-shader/src/gfx10/mod.rs` | GPL-3.0-or-later, used under GPL-3.0-only | operand-field and SP3 printer modules not vendored; `name()` public |
| `crates/shader/src/gfx10/opcodes.rs` | the same, generated from Mesa's `aco_opcodes.py` (Valve Corporation, MIT notice kept in the file) | GPL-3.0-or-later / MIT, used under GPL-3.0-only | tables `pub(crate)`; the test no longer builds the fork's IR |
