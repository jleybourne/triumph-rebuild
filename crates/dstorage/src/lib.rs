//! The console's DirectStorage (`xcrdapi.dll!DStorageGetFactory`), served on the host (Build 3).
//!
//! ```text
//!   title ── DStorageGetFactory ──► factory (COM-shaped, slots in agents/dstorage.md)
//!                                     ├─ OpenFile(console path)   ─► file object  ─► vfs handle
//!                                     ├─ CreateStatusArray(n)     ─► status array (title polls it)
//!                                     ├─ SetStagingBufferSize(n)  ─► in-flight compressed-byte cap
//!                                     └─ CreateQueue(desc)        ─► queue
//!   queue.EnqueueRequest(req) / EnqueueStatus(array, i) / Submit / Close
//!        │ submitted commands get one global sequence number each
//!        ▼
//!   engine: worker threads take reads by priority class (weighted by bytes), read the source
//!   (vfs file or memory), inflate zlib, write the destination (an address, or 64 KiB pages);
//!   a status entry is written once every read enqueued before it on its queue has finished,
//!   with the first failure since the previous entry (or S_OK).
//! ```
//!
//! What is ours and what is the lead's: the layouts, the queue semantics and the decode are here;
//! the binding of `DStorageGetFactory` into `crates/runtime` and the physical-page writer
//! ([`Pages`], over `runtime::physical`) are the lead's. BCPack (request bits 1-4) is decoded by
//! [`bcpack::Decoder`] (D5: our decoder, the format tables from a file outside git, an on-disk cache
//! of decoded output) once the engine is given one (`Config::bcpack`, e.g. `Decoder::from_config`);
//! without one (no tables) such requests fail with `E_NOTIMPL` and are counted.

pub mod bcpack;
pub mod capture;
pub mod cache;
pub mod com;
pub mod engine;
pub mod zlib;

#[cfg(test)]
mod tests;

use std::ffi::{c_char, c_void};

pub use bcpack::Decoder;
pub use capture::Capture;
pub use cache::Cache;
pub use com::{install, DStorageGetFactory, OwnedFactory};
pub use engine::{BcPack, Config, Engine, Pages, Stats, StatusArray};

/// `HRESULT`.
pub type Hresult = i32;

pub const S_OK: Hresult = 0;
pub const E_PENDING: Hresult = 0x8000_000A_u32 as i32;
pub const E_NOTIMPL: Hresult = 0x8000_4001_u32 as i32;
pub const E_NOINTERFACE: Hresult = 0x8000_4002_u32 as i32;
pub const E_INVALIDARG: Hresult = 0x8007_0057_u32 as i32;
pub const E_OUTOFMEMORY: Hresult = 0x8007_000E_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`.
pub const FILE_NOT_FOUND: Hresult = 0x8007_0002_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)`.
pub const PATH_NOT_FOUND: Hresult = 0x8007_0003_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_INVALID_DATA)`: a corrupt compressed stream.
pub const INVALID_DATA: Hresult = 0x8007_000D_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_HANDLE_EOF)`: the source ends before the request does.
pub const END_OF_FILE: Hresult = 0x8007_0026_u32 as i32;

/// A COM interface id.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self { data1, data2, data3, data4 }
    }
}

/// `IUnknown`.
pub const IID_UNKNOWN: Guid = Guid::new(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
/// The factory interface the title asks `DStorageGetFactory` for.
pub const IID_FACTORY: Guid = Guid::new(0x6295_9d11, 0xd45f, 0x4884, [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae]);
/// The file interface the title passes to `OpenFile`.
pub const IID_FILE: Guid = Guid::new(0x4370_4f9f, 0x27c6, 0x4a9f, [0x88, 0x15, 0xd1, 0x65, 0x27, 0x89, 0x4d, 0xf4]);
/// The queue interface the title passes to `CreateQueue`.
pub const IID_QUEUE: Guid = Guid::new(0x3c02_cb0c, 0xc050, 0x4195, [0x99, 0xa1, 0xa1, 0xce, 0x94, 0xba, 0x33, 0xc5]);
/// The status-array interface the title passes to `CreateStatusArray`.
pub const IID_STATUS: Guid = Guid::new(0xcd66_d752, 0x2786, 0x44f7, [0x90, 0x70, 0xe2, 0xa6, 0xfd, 0xad, 0x3f, 0x1b]);

/// A read request exactly as the title builds it (0x48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// [`ZLIB`], [`BCPACK_MASK`], [`PAGE_DESTINATION`], [`MEMORY_SOURCE`].
    pub options: u64,
    /// An address; with [`PAGE_DESTINATION`], an array of 64 KiB physical page numbers.
    pub destination: *mut c_void,
    /// With [`PAGE_DESTINATION`]: the low 16 bits are the start inside the first page.
    pub page_offset: u64,
    pub destination_size: u32,
    /// A file object from `OpenFile`; with [`MEMORY_SOURCE`], an address.
    pub source: *const c_void,
    /// Byte offset in the file.
    pub offset: u64,
    pub source_size: u32,
    /// Size between the zlib and BCPack stages when both are set.
    pub intermediate_size: u32,
    /// For cancellation by tag; the title leaves it 0.
    pub tag: u64,
    pub name: *const c_char,
}

const _: () = assert!(std::mem::size_of::<Request>() == 0x48);

impl Request {
    /// The BC format number BCPack data decodes to (0: no BCPack).
    pub fn bcpack_format(&self) -> u8 {
        ((self.options & BCPACK_MASK) >> 1) as u8
    }
}

/// Option bit 0: the source is a zlib stream.
pub const ZLIB: u64 = 1;
/// Option bits 1-4: the source (after zlib, if set) is BCPack data of this BC format.
pub const BCPACK_MASK: u64 = 0xf << 1;
/// Option bit 21: the destination is a list of 64 KiB physical pages.
pub const PAGE_DESTINATION: u64 = 1 << 21;
/// Option bit 22: the source is an address, not a file.
pub const MEMORY_SOURCE: u64 = 1 << 22;
/// Every option bit this crate knows; others (bits 5-20, 23 and up) fail with `E_NOTIMPL`.
pub const KNOWN_OPTIONS: u64 = ZLIB | BCPACK_MASK | PAGE_DESTINATION | MEMORY_SOURCE;

/// A physical page, as the page lists count them.
pub const PAGE: usize = 64 << 10;

/// The queue description passed to `CreateQueue` (0x20 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct QueueDesc {
    /// 0: files (every queue the title makes).
    pub source_type: u64,
    /// Entries (requests and status entries) the queue holds.
    pub capacity: u16,
    /// -1 low, 0 normal, 1 high, 2 real-time.
    pub priority: i8,
    pub name: *const c_char,
    /// The title sets it to 1.
    pub unknown: u64,
}

const _: () = assert!(std::mem::size_of::<QueueDesc>() == 0x20);
