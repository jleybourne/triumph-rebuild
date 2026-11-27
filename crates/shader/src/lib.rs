//! The shader front end: the title's shader archive, its containers, and the GFX10
//! (RDNA2) machine code inside them, decoded into a bit-complete field model that
//! re-encodes to the same bytes.
//!
//! Vendored from the fork (GPL-3.0-or-later, see CREDITS.md): the DXBC container
//! reader ([`dxbc`]), the native-program reader ([`native`]), the SGD5 record walk
//! ([`sgd5`]), and the GFX10 instruction boundaries and opcode tables ([`gfx10`]).
//! Ours: the field model and encoder ([`gfx10::enc`]), the census ([`census`]) and
//! the per-program feature summary ([`features`]).
//!
//! No IR, lowering or DXIL emission lives here: that is the lead's shader core
//! (`crates/sir`).

pub mod bitcode;
pub mod census;
pub mod dxbc;
pub mod features;
pub mod gfx10;
pub mod native;
pub mod oraclediff;
pub mod sgd5;

pub use blake3;
pub use dxbc::{Container, FourCC, Part};
pub use native::{HwStage, NativeProgram, native_programs};

#[derive(Debug, PartialEq, Eq)]
pub enum ShaderError {
    NotDxbc,
    Malformed(&'static str),
    Isa { offset: usize, reason: &'static str },
}

impl std::fmt::Display for ShaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShaderError::NotDxbc => write!(f, "not a DXBC container"),
            ShaderError::Malformed(m) => write!(f, "malformed shader: {m}"),
            ShaderError::Isa { offset, reason } => write!(f, "GFX10 code at {offset:#x}: {reason}"),
        }
    }
}

impl std::error::Error for ShaderError {}
