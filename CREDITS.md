# Credits

Code derived from other projects, with what was taken and where it lives.

- **VIctory** (Marvisak and contributors), via the owner's fork jleybourne/Triumph, GPL-3.0-or-later, used
  here under GPL-3.0-only.
  - `crates/xg/src/layout.rs`: the GFX10 texture layout maths from `victory-formats/src/texture.rs` (block
    dimensions, mip placement and tails, micro tile patterns, element offsets, the DXGI element table).
    That file in turn follows AMD's addrlib (MIT, notice kept in the file).
  - `crates/xg/src/computer.rs`: the `xg_xs.dll` structure layouts, vtable slot meanings and suggestion
    rule from `victory-hle/src/modules/xg.rs`, rewritten as plain Rust methods.
