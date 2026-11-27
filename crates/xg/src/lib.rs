//! `rebuild-xg`: the console's texture layout calculator (`xg_xs.dll`),
//! for Build 5.
//!
//! - [`layout`]: GFX10 texture layouts (block size, mip placement, mip
//!   tails, element offsets), after AMD's addrlib; vendored from the fork.
//! - [`address`]: the inverse, byte offset to element (X2).
//! - [`computer`]: `XGCreateTextureComputer` / `XGSuggestSwizzleMode` and
//!   the computer's methods, as plain Rust over the console's structures
//!   (X3).
//! - [`detile`]: the CPU detiler, tiled to linear rows and back, and the
//!   per-axis tables a GPU detile would read (X4).
//!
//! The pipe/bank XOR modes (`_X`, `_T`: render targets and depth) are
//! refused, never guessed: their addresses need the console's pipe
//! configuration.

pub mod address;
pub mod computer;
pub mod detile;
pub mod layout;

pub use address::Texel;
pub use computer::{Computer, ResourceDesc};
pub use detile::Tables;
pub use layout::{format, Desc, Dimension, Error, Format, Layout, Swizzle, Texture, TextureDesc};
