//! Groundwork for an offline single-player developer menu: what the title's own code offers for teleporting
//! the player, spawning things and similar, found by static reverse engineering of the mapped image
//! (`game_scarlett_master`, preferred base `0x140000000`). Findings, evidence and confidence levels are in
//! `docs/research/devmenu.md`.
//!
//! Nothing here runs by default and nothing hooks the title. The crate holds:
//! - [`addr`]: typed addresses (preferred VAs) with the confidence of each;
//! - [`native`]: the script VM's native-call ABI and a scanner that lists every native handler with its hash
//!   straight from the image bytes;
//! - [`world`]: the packed world position the title stores in entities, and its inverse;
//! - [`hash`]: the title's string hash (`joaat`, lower-cased), used for class ids and model names.

pub mod addr;
pub mod hash;
pub mod native;
pub mod pe;
pub mod world;
