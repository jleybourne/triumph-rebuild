//! Build 3, first half: the PE loader (lane `loader`, `secondary-agent.md`).
//!
//! - [`pe`]: a PE32+ parser written for this project, with no dependencies: headers, sections, base
//!   relocations, imports by name and ordinal, delay-load imports, exports, TLS callbacks and the
//!   exception table (`.pdata`). Every read is bounds-checked; a malformed image is an error, never a
//!   panic.
//! - [`map`]: lay an image out in memory at a chosen base, apply its relocations, and bind its imports
//!   through a [`map::Resolver`]. Nothing in the image is executed.
//! - [`region`] (Windows): reserve and commit read-write memory at a chosen address to map into.
//!
//! Title code is never run here, no thread is made for it and no kernel call is implemented: that is
//! Build 3's second half.

pub mod map;
pub mod pe;
#[cfg(windows)]
pub mod region;

#[cfg(test)]
mod synth;

pub use map::{map_into, MapError, MapReport, Missing, NullResolver, Resolver};
pub use pe::{Image, ImportModule, Symbol, PeError};
