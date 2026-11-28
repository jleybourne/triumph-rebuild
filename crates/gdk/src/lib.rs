//! Build 3 piece: the GDK runtime behind `xgameruntime.dll`.
//!
//! The title (and the GDK libraries it ships, `libHttpClient.GDK.dll` and `XCurl.dll`) loads
//! `xgameruntime.dll` with `LoadLibraryExA`, takes six entry points by name (`GetProcAddress`), and
//! reaches every runtime area through objects `QueryApiImpl` hands out for a (provider, interface id)
//! pair. Each object is COM-shaped: its first word points at a method table; slots 0-2 are
//! `QueryInterface`, `AddRef`, `Release`; the area's methods start at byte offset 0x18 and take the
//! object first, then the public API's arguments.
//!
//! ```text
//! runtime     the six entry points (InitializeApiImpl*, UninitializeApiImpl, QueryApiImpl, XErrorReport)
//! object      objects, method tables, the discovery handler behind every slot we don't implement
//! taskqueue   XTaskQueue: work and completion ports, manual / thread pool / serialized / immediate
//! xasync      XAsync: XAsyncBlock lifetime, providers, completion, cancel, waits
//! areas       XUser, XGame (title, features, invites, events), XSystem, XPackage, XStore, XNetworking,
//!             XPersistentLocalStorage, XDisplay (HDR), XGameSave, XSpeechSynthesizer
//! table       every (interface, slot) -> Rust function, with the console argument layout
//! ```
//!
//! An offline retail console with one signed-in local user (gamertag from [`profile`]), the title
//! owned and fully installed, and no network: calls that need the service fail with the area's
//! documented error, at once. No call blocks its caller beyond what the GDK documents
//! (`XAsyncGetStatus` with `wait`, `XTaskQueueDispatch` with a timeout, `XTaskQueueTerminate` with
//! `wait`). No GPU, no title code: this crate is pure host Rust.
//!
//! The fork's `victory-hle` GDK modules and its run logs were the reference for slot offsets and
//! what the title asks for (read, not copied); see `agents/gdk.md` for the evidence per row.

pub mod areas;
pub mod discovery;
pub mod guid;
pub mod hr;
pub mod known;
pub mod object;
pub mod profile;
pub mod runtime;
pub mod sandbox;
pub mod table;
pub mod taskqueue;
pub mod xasync;

pub use guid::Guid;
pub use hr::HRESULT;
