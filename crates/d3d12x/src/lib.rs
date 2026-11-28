//! The D3D12.X resource, heap, descriptor heap and view objects (Build 5), as a crate the lead's
//! device calls into.
//!
//! ```text
//! device slot                         here                                   what the title gets
//! 0x70 CreateDescriptorHeap           Resources::create_descriptor_heap     a console heap over ITS memory (placed) or ours
//! 0x78 GetDescriptorHandleIncrement   Resources::increment                  32 / 16 / 128 / 128
//! 0x88-0xb0 Create{CBV,SRV,UAV,RTV,DSV,Sampler}   Resources::create_*_view  a 32-byte record written at the handle
//! 0xb8 / 0xc0 CopyDescriptors(Simple) Resources::copy_descriptors*          bytes copied, destinations mirrored
//! 0xc8 / 0x360 GetResourceAllocationInfo(2)   Resources::allocation_info*   console sizes (lane xg's layouts)
//! 0xd0 GetCustomHeapProperties        Resources::custom_heap_properties
//! 0xd8 / 0x3f8 CreateCommittedResource(3)     Resources::create_committed*  resource object, memory at a console address
//! 0xe0 / 0xe8 CreateHeap / CreatePlacedResource
//! 0x130 GetCopyableFootprints         Resources::copyable_footprints        the host's rules (copies run on the host)
//! 0x1e8 CreateSamplerX                Resources::create_sampler_x
//! 0x230 / 0x238 placed SRV / UAV      Resources::create_placed_view
//! 0x250 / 0x258 page pools            Resources::register_page_pool / unregister_page_pool
//! 0x428 resource at an address        Resources::create_at_address
//! ```
//!
//! [`thunks`] has an `extern "system"` function for each of those slots, for the device table.
//! [`Host`] implements `rebuild_xlist::console::ListHost` (resource, descriptor heap, CPU
//! descriptor, memory) over a [`Resources`], wrapping the lead's pipelines.
//!
//! **Memory** is the lead's ([`Memory`]): console addresses are GPU addresses (plan decision 1),
//! committed resources get console memory from it, and every buffer view is made from
//! [`Memory::range`] of its console address. [`memory::HostMemory`] is a stand-in on one upload
//! heap, used by the GPU test.
//!
//! **Descriptors** (`docs/design/d3d12x-resources.md`): the title's heap memory stays the
//! title's; it holds our 32-byte records, which name the view whole (buffers) or by table entry
//! (textures), so a record survives being copied as bytes. A shader-visible host heap mirrors
//! each console index at the same index (SM 6.6 heap indexing, plan decision 5); a buffer
//! descriptor's view at its index is a **raw SRV** (every buffer slot reads as a
//! `ByteAddressBuffer`, agents/shader.md S7-5; a CBV is only that) and its own kind (typed, UAV) sits at an
//! alternate index the shadow table names. The shadow table (plan decision 9) holds, per index,
//! `(stride, records, alternate index, raw-view lead bytes)`.

#![cfg(windows)]

pub mod format;
pub mod heap;
pub mod host;
pub mod memory;
pub mod object;
pub mod records;
pub mod resource;
pub mod root;
pub mod signature;
pub mod sampler;
pub mod slots;
pub mod thunks;
pub mod views;

#[cfg(test)]
mod tests;

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use windows::core::GUID;
use windows::Win32::Graphics::Direct3D12::*;

pub use heap::{HeapState, HeapStats, Limits};
pub use host::Host;
pub use memory::{HostMemory, Memory};
pub use records::Record;
pub use resource::{HeapObject, Resource};
pub use rebuild_xlist::console::{HeapBinding, MemoryRange};
pub use slots::{traps, Object, SlotInfo, SlotKind, Trap};
pub use views::{HostDevice, HostHeap, HostView, Shadow};

/// A console object pointer, borrowed.
pub type Raw = *mut c_void;

/// What the crate tells the lead: first trap calls, interface misses, and one-time notes
/// (an index past the host heap, an unaligned constant buffer). Defaults do nothing.
pub trait Events: Send + Sync {
    fn first_trap(&self, _object: &'static str, _offset: usize) {}
    fn unknown_iid(&self, _object: &'static str, _iid: &GUID) {}
    /// A first occurrence of something worth a log line; `what` is a stable key, `value` its
    /// datum (an index, an address).
    fn note(&self, _what: &'static str, _value: u64) {}
}

/// No events.
pub struct Quiet;
impl Events for Quiet {}

/// Counters over the whole run (relaxed atomics; read with [`Resources::stats`]).
#[derive(Default)]
pub(crate) struct Counters {
    pub views_created: AtomicU64,
    pub views_mirrored: AtomicU64,
    pub views_skipped: AtomicU64,
    pub unheaped: AtomicU64,
    pub resources: AtomicU64,
    pub root_signatures: AtomicU64,
    pub query_heaps: AtomicU64,
    pub command_signatures: AtomicU64,
    /// descriptor copies refused: a handle into unmapped memory
    pub bad_copies: AtomicU64,
    pub refused: AtomicU64,
}

/// A copy of the counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// view create calls (0x88-0xb0, 0x1e8, 0x230/0x238)
    pub views_created: u64,
    /// console slots mirrored into host heaps (create calls, copies, syncs)
    pub views_mirrored: u64,
    /// slots a sync found unchanged
    pub views_skipped: u64,
    /// views written at handles in no known heap (records written, nothing mirrored)
    pub unheaped: u64,
    pub resources: u64,
    /// creations refused (bad arguments, no memory)
    pub refused: u64,
}

/// Everything behind the objects. One per device.
pub struct Inner {
    /// unique per instance (thread caches are keyed by it)
    pub(crate) id: u64,
    pub(crate) host: Arc<dyn HostDevice>,
    pub(crate) memory: Arc<dyn Memory>,
    pub(crate) events: Arc<dyn Events>,
    /// the title's device object (for `GetDevice`), set by the lead
    pub(crate) device: AtomicPtr<c_void>,
    pub(crate) limits: Limits,
    pub(crate) heaps: RwLock<Vec<heap::Registered>>,
    pub(crate) entries: records::Entries,
    pub(crate) pools: Mutex<Vec<Option<(u64, u32)>>>,
    /// host textures for textures the title placed itself (0x230/0x238), by address and
    /// description, kept for the run (bounded by the title's textures)
    pub(crate) placed: Mutex<std::collections::HashMap<(u64, [u32; 14]), Option<ID3D12Resource>>>,
    /// placed texture views already in the entry table
    pub(crate) placed_views: Mutex<std::collections::HashMap<(bool, u64, [u32; 14], [u32; 10]), Record>>,
    /// CPU-only descriptors for UAV clears of shader-visible slots (made on first use)
    pub(crate) transient: std::sync::OnceLock<Option<heap::Transient>>,
    /// window views placed ([`Resources::place_window`]): sub-window index and its buffer (held), written into
    /// every shader-visible view heap, those made later included
    pub(crate) windows: Mutex<Vec<(u32, ID3D12Resource)>>,
    pub(crate) counters: Counters,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The lead's handle on this crate: create functions for the device slots and the lookups the
/// list translator needs.
#[derive(Clone)]
pub struct Resources {
    pub(crate) inner: Arc<Inner>,
}

impl Resources {
    /// Over a host D3D12 device: host heaps sized by the device's limits ([`Limits::probe`]).
    pub fn new(device: &ID3D12Device, memory: Arc<dyn Memory>, events: Arc<dyn Events>) -> windows::core::Result<Resources> {
        let host = views::DeviceHost::new(device)?;
        let limits = Limits::probe(device);
        Ok(Self::with_host(Arc::new(host), memory, events, limits))
    }

    /// Over any [`HostDevice`] (the unit tests use a recording stand-in).
    pub fn with_host(host: Arc<dyn HostDevice>, memory: Arc<dyn Memory>, events: Arc<dyn Events>, limits: Limits) -> Resources {
        slots::start_clock();
        Resources {
            inner: Arc::new(Inner {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                host,
                memory,
                events,
                device: AtomicPtr::new(std::ptr::null_mut()),
                limits,
                heaps: RwLock::new(Vec::new()),
                entries: records::Entries::default(),
                pools: Mutex::new(Vec::new()),
                placed: Mutex::default(),
                placed_views: Mutex::default(),
                transient: std::sync::OnceLock::new(),
                windows: Mutex::new(Vec::new()),
                counters: Counters::default(),
            }),
        }
    }

    /// The title's device object: `GetDevice` (0x38) on our objects answers through it.
    pub fn set_device(&self, device: Raw) {
        self.inner.device.store(device, Ordering::Relaxed);
    }

    pub fn limits(&self) -> Limits {
        self.inner.limits
    }

    /// The highest view index a translated shader may use: indices past it must be clamped to
    /// it (it is a null view in every shader-visible host view heap). The fork's lesson: a
    /// compute program read slot 16,777,088 of a 1,507,328-slot heap and removed the device.
    pub fn view_index_limit(&self) -> u32 {
        self.inner.limits.null_index()
    }

    pub fn stats(&self) -> Stats {
        let c = &self.inner.counters;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Stats {
            views_created: l(&c.views_created),
            views_mirrored: l(&c.views_mirrored),
            views_skipped: l(&c.views_skipped),
            unheaped: l(&c.unheaped),
            resources: l(&c.resources),
            refused: l(&c.refused),
        }
    }
}

/// The method table entries we implement for `object` (the rest are traps).
pub(crate) fn methods(object: Object) -> Vec<(usize, &'static str, slots::Fp)> {
    match object {
        Object::Resource => resource::resource_methods(),
        Object::Heap => resource::heap_methods(),
        Object::DescriptorHeap => heap::methods(),
        Object::RootSignature => root::methods(),
        Object::QueryHeap => signature::query_methods(),
        Object::CommandSignature => signature::methods(),
    }
}
