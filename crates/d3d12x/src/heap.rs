//! Descriptor heaps and views: the title's heap memory, the host heaps that mirror it, and the
//! shadow table.
//!
//! ```text
//! console heap (0x70)     desc {type, count, flags, mask} + placement {address, size}: the
//!                         title's own memory when size == count x increment (the fork's test);
//!                         else ours. Handles are addresses, CPU and GPU alike.
//! shader-visible views    one host heap of Limits::views descriptors:
//!   [0, mirrored)           console index i at host index i. Buffers: a raw SRV of the
//!                           descriptor's bytes; textures: the title's view
//!   [P, P + NULL_BANK)      null views, one per view type translated shaders declare
//!                           (`rebuild_sir::lift::NullKind`): a console index at or past P reads
//!                           the null of its type there (`sir_vi`), and so does an alternate that
//!                           names the bank (`sir_va`). P = `rebuild_sir::lift::CONSOLE_VIEWS`
//!                           on a real device (750,000), so no console index ever reaches:
//!   [P + NULL_BANK, W)      alternate views: a buffer descriptor's own kind (typed SRV, UAV;
//!   and [1,000,000, views)  never a CBV: CBVs are only raw SRVs here), at the index its shadow
//!                           entry names
//!   [W, 1,000,000)          window views (W = `rebuild_sir::lift::WINDOW_HEAP_BASE`): never an
//!                           alternate
//! shadow table            per console index 16 bytes (stride, records, alternate, lead), t0 space101;
//!                         entry P (what a shader past the range reads) is (0, 0, P, 0)
//! shader-visible samplers host heap of min(count, Limits::samplers), index for index
//! CPU-only views, RTV, DSV host heap of count, index for index (the natural view)
//! ```
//!
//! A slot is mirrored when a create call writes it, when CopyDescriptors copies into it, and
//! when [`Resources::sync`] is told its bytes changed (the title copies records as bytes; the
//! lead's write-watch over the heap's pages says which). Each slot keeps a fingerprint of the
//! bytes it was mirrored from, so a sync of an unchanged slot costs a read and a compare.
//! Slots are independent: no lock is taken to mirror one, except the alternate pool's when a
//! heap is too large for fixed alternates.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::format::{self, DIMENSION_BUFFER, DIMENSION_TEXTURE1D, DIMENSION_TEXTURE3D};
use crate::object::{self, Head};
use crate::records::{self, EntryView, Record, RAW};
use crate::resource::{resource, Resource};

/// UAVs written as null views: the host resource has no unordered access.
pub static NO_UAV: AtomicU64 = AtomicU64::new(0);
use crate::slots::{Fp, Object};
use crate::views::{HostHeap, HostView, Shadow, DSV, RTV, SAMPLERS, VIEWS};
use crate::{HeapBinding, Inner, MemoryRange, Raw, Resources};

/// Console descriptor sizes (device 0x78): views, samplers, RTV, DSV.
pub const INCREMENTS: [u32; 4] = [32, 16, 128, 128];

/// The host's descriptor heap limits, and how view indices are split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// descriptors in a shader-visible view heap (`MaxViewDescriptorHeapSize`; 1,000,000 on
    /// resource binding tiers 1-2 and on this NVIDIA card)
    pub views: u32,
    /// descriptors in a shader-visible sampler heap (2,048 unless the device says more)
    pub samplers: u32,
}

impl Limits {
    pub const TIER2: Limits = Limits { views: 1_000_000, samplers: 2048 };

    /// From the device: `D3D12_OPTIONS19` when the runtime has it, else a creation probe at
    /// 2^21 views (tier 3 hardware may take it), else the tier-2 numbers.
    pub fn probe(device: &ID3D12Device) -> Limits {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        let ok = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS19, (&mut o as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS19).cast(), size_of_val(&o) as u32) }.is_ok();
        if ok && o.MaxViewDescriptorHeapSize > 0 {
            // root signatures with static samplers (the fork's lesson 9 keeps them beside the
            // sampler heap) take the smaller sampler limit
            let s = if o.MaxSamplerDescriptorHeapSizeWithStaticSamplers > 0 { o.MaxSamplerDescriptorHeapSizeWithStaticSamplers } else { o.MaxSamplerDescriptorHeapSize };
            return Limits { views: o.MaxViewDescriptorHeapSize, samplers: s.max(2048) };
        }
        let fits = |n: u32| {
            unsafe {
                device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: n,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
            }
            .is_ok()
        };
        let views = if fits(1 << 21) { 1 << 21 } else { Limits::TIER2.views };
        Limits { views, samplers: Limits::TIER2.samplers }
    }

    /// The first null slot of every shader-visible view heap, and the first index a console
    /// index cannot have: `rebuild_sir::lift::CONSOLE_VIEWS` (750,000, above the 720,000 the fork
    /// saw the title use), which translated shaders compare with. A heap smaller than 1,000,000
    /// (test rigs only: every host has at least that) keeps three quarters.
    pub fn null_index(&self) -> u32 {
        if self.views >= 1_000_000 {
            rebuild_sir::lift::CONSOLE_VIEWS
        } else {
            self.views - self.views / 4
        }
    }

    /// The host indices alternates may take: after the null bank, below the window range, and
    /// past 1,000,000 on a larger heap. Disjoint from every index a console index or a window
    /// can reach.
    pub fn alternate_ranges(&self) -> [std::ops::Range<u32>; 2] {
        let first = self.null_index() + rebuild_sir::lift::NULL_BANK;
        if self.views >= 1_000_000 {
            [first..rebuild_sir::lift::WINDOW_HEAP_BASE, 1_000_000..self.views]
        } else {
            [first..self.views, self.views..self.views]
        }
    }
}

/// How a shader-visible view heap places alternate views.
enum Alt {
    /// alternate of console index i at `base + i` (the heap's count fits the alternate range)
    Fixed(u32),
    /// from a free list over the alternate range (the title's 1,245,184-slot heap)
    Pool(Mutex<Vec<u32>>),
    /// not a shader-visible view heap
    None,
}

/// Counters of one heap.
#[derive(Default)]
pub struct HeapStats {
    /// writes to console indices past what the host heap mirrors
    pub beyond: AtomicU64,
    /// the highest console index written
    pub highest: AtomicU32,
    /// alternates wanted while the pool was empty
    pub pool_empty: AtomicU64,
    /// buffer views whose own kind cannot be made at their address (CBV not 256-aligned,
    /// typed not element-aligned): the alternate is null, the raw view is exact
    pub misaligned: AtomicU64,
    /// typed buffer views with more elements than the memory holds from their start: cut to what fits
    pub clamped: AtomicU64,
    /// views of memory the lead's model does not know
    pub unknown_memory: AtomicU64,
}

/// A console descriptor heap and its host mirror.
pub struct HeapState {
    pub kind: i32,
    pub visible: bool,
    /// console handle of descriptor 0 (title memory, or ours)
    pub start: u64,
    pub count: u32,
    /// console bytes per descriptor
    pub stride: u32,
    pub host: HostHeap,
    /// console indices mirrored to the host (count, or less when the host heap is smaller)
    pub mirrored: u32,
    /// the null slot (shader-visible view heaps)
    pub null_index: u32,
    pub shadow: Option<Shadow>,
    alt: Alt,
    seen: Box<[AtomicU64]>,
    alts: Box<[AtomicU32]>,
    pub stats: HeapStats,
}

const NO_ALT: u32 = u32::MAX;

pub(crate) struct Registered {
    start: u64,
    end: u64,
    state: Arc<HeapState>,
}

/// The console descriptor heap object.
#[repr(C)]
pub struct DescriptorHeap {
    head: Head,
    /// `D3D12_DESCRIPTOR_HEAP_DESC` as given
    pub desc: [u32; 4],
    pub state: Arc<HeapState>,
    /// our memory when the title placed none
    _owned: Option<Box<[u64]>>,
}

impl Drop for DescriptorHeap {
    fn drop(&mut self) {
        let mut heaps = self.head.common.ctx.heaps.write().unwrap_or_else(|e| e.into_inner());
        heaps.retain(|r| !Arc::ptr_eq(&r.state, &self.state));
    }
}

fn heap_of<'a>(p: Raw) -> Option<&'a DescriptorHeap> {
    // SAFETY: checked to be ours by its table.
    object::is(p, Object::DescriptorHeap).then(|| unsafe { &*p.cast::<DescriptorHeap>() })
}

// ---- descriptor heap methods ----

unsafe extern "system" fn dh_release(this: Raw) -> u32 {
    unsafe { object::release_boxed::<DescriptorHeap>(this) }
}

unsafe extern "system" fn dh_desc(a: Raw, b: Raw) -> Raw {
    let (this, out) = object::this_and_out::<[u32; 4]>(a, b, Object::DescriptorHeap);
    if !out.is_null() {
        unsafe { out.write((*this.cast::<DescriptorHeap>()).desc) };
    }
    out.cast()
}

/// 0x48 / 0x50: the start handle (an address, CPU and GPU alike). An 8-byte struct return: MSVC
/// C++ passes a hidden `out` after `this` and takes it back; a C-style caller passes nothing
/// and reads the value. A second argument within 1 MiB of our own stack frame is the caller's
/// return slot.
unsafe extern "system" fn dh_start(this: Raw, out: *mut u64) -> u64 {
    let start = unsafe { &*this.cast::<DescriptorHeap>() }.state.start;
    let here = &start as *const u64 as u64;
    if !out.is_null() && (out as u64).abs_diff(here) < (1 << 20) {
        unsafe { out.write(start) };
        return out as u64;
    }
    start
}

pub(crate) fn methods() -> Vec<(usize, &'static str, Fp)> {
    let mut m = object::common_slots::<2>(dh_release);
    m.extend([
        (0x40, "GetDesc", Fp(dh_desc as *const ())),
        (0x48, "GetCPUDescriptorHandleForHeapStart", Fp(dh_start as *const ())),
        (0x50, "GetGPUDescriptorHandleForHeapStart", Fp(dh_start as *const ())),
    ]);
    m
}

/// What a view slot becomes on the host.
#[derive(Clone, Copy)]
pub struct Plan {
    /// at the console index
    pub primary: HostView,
    /// the descriptor's own kind, at the alternate index (buffers that are not raw SRVs)
    pub alt: Option<HostView>,
    /// `(stride, records, -, lead)`; the alternate index is filled in when placed
    pub shadow: [u32; 4],
    /// the view for a CPU-only heap and for UAV clears
    pub natural: HostView,
}

impl Plan {
    fn null() -> Plan {
        Plan { primary: HostView::NullBuffer, alt: None, shadow: [0; 4], natural: HostView::NullBuffer }
    }
}

/// CPU-only descriptors for UAV clears of shader-visible slots (ClearUnorderedAccessView needs
/// a CPU handle in a CPU-only heap).
pub(crate) struct Transient {
    host: HostHeap,
    next: AtomicU32,
}

const TRANSIENT: u32 = 4096;

/// Buffer views made recently on this thread, in a CPU-only heap: `(resource, first element,
/// elements, format and flags)` per slot, direct-mapped. One per thread, so no lock; rebuilt if another [`Inner`] uses the
/// thread. Resources named here are the model's ranges, which live for the run.
struct RawCache {
    owner: u64,
    host: HostHeap,
    keys: Box<[(usize, u64, u32, u32)]>,
}

const RAW_SLOTS: u32 = 1 << 16;

thread_local! {
    static RAW_CACHE: std::cell::RefCell<Option<RawCache>> = const { std::cell::RefCell::new(None) };
}

fn dword_range(r: &MemoryRange, va: u64) -> u64 {
    r.resource_offset + (va - r.console)
}

impl Inner {
    /// The heap holding console handle `h`, and the index there.
    #[inline]
    pub(crate) fn heap_at(&self, h: u64) -> Option<(Arc<HeapState>, u32)> {
        let heaps = self.heaps.read().unwrap_or_else(|e| e.into_inner());
        let k = heaps.partition_point(|r| r.start <= h);
        let r = heaps.get(k.checked_sub(1)?)?;
        (h < r.end).then(|| (r.state.clone(), ((h - r.start) / u64::from(r.state.stride)) as u32))
    }

    /// The plan for a 32-byte view record.
    pub(crate) fn plan(&self, w: &[u32; 8], heap: &HeapState) -> Plan {
        match Record::decode(w) {
            None => Plan::null(),
            // a null raw SRV in a shader-visible heap (the slot type stays SRV, as for every buffer)
            Some(Record::NullCbv) => Plan { primary: HostView::NullBuffer, alt: None, shadow: [0; 4], natural: HostView::NullCbv },
            Some(Record::Null { uav, dimension }) => {
                let v = if uav {
                    HostView::NullUav
                } else if dimension <= 1 {
                    HostView::NullBuffer
                } else {
                    HostView::NullTexture(dimension as i32)
                };
                Plan { primary: v, alt: None, shadow: [0; 4], natural: v }
            }
            Some(Record::Entry { index, generation }) => match self.entries.get(index, generation) {
                Some(EntryView::Texture(v)) => Plan { primary: v, alt: None, shadow: [0; 4], natural: v },
                _ => Plan::null(),
            },
            Some(Record::Cbv { va, size }) => self.buffer_plan(heap, va, u64::from(size), None),
            Some(Record::Buffer { uav, va, format, elements, stride, flags }) => {
                let (elem, kind) = if flags & RAW != 0 {
                    (4, BufferKind::Raw { uav })
                } else if stride > 0 {
                    (u64::from(stride), BufferKind::Structured { uav, stride })
                } else {
                    let f = format::host_format(format);
                    match format::element(f) {
                        Some((e, 1)) => (e, BufferKind::Typed { uav, format: f }),
                        _ => {
                            heap.stats.misaligned.fetch_add(1, Ordering::Relaxed);
                            (4, BufferKind::Raw { uav })
                        }
                    }
                };
                self.buffer_plan(heap, va, u64::from(elements) * elem, Some((kind, elements)))
            }
        }
    }

    /// A buffer descriptor: raw SRV at its index (exact bytes, from a 16-byte aligned start;
    /// `lead` bytes before the descriptor's first), its own kind as the alternate.
    fn buffer_plan(&self, heap: &HeapState, va: u64, len: u64, kind: Option<(BufferKind, u32)>) -> Plan {
        let Some(r) = self.memory.range(va) else {
            if heap.stats.unknown_memory.fetch_add(1, Ordering::Relaxed) == 0 {
                self.events.note("view of memory the model does not know", va);
            }
            return Plan::null();
        };
        let off = dword_range(&r, va);
        let room = r.bytes - (va - r.console);
        let len = len.min(room);
        let start = off & !15;
        let lead = (off - start) as u32;
        let dwords = ((u64::from(lead) + len).div_ceil(4)).clamp(1, u64::from(u32::MAX)) as u32;
        let primary = HostView::raw_srv(r.resource, start / 4, dwords);
        let host_va = r.host_va + (va - r.console);
        let (shadow, alt) = match kind {
            // a constant buffer view: only the raw SRV in a shader-visible heap, no alternate. Console descriptors
            // are untyped (the title may read a CBV slot as a buffer or a buffer slot as constants) and the host
            // wants one type per slot, so translated shaders read every heap buffer as raw (`rebuild_sir` lift
            // `buffer_load`) and no CBV is ever in a shader-visible heap. The CBV itself (256-byte aligned on the
            // host, at most 64 KiB) is the natural view, for CPU-only heaps.
            None => {
                let cbv = if host_va % 256 == 0 {
                    HostView::Cbv { va: host_va, size: (len.max(1).next_multiple_of(256)).min(65536) as u32 }
                } else {
                    heap.stats.misaligned.fetch_add(1, Ordering::Relaxed);
                    HostView::NullCbv
                };
                return Plan { primary, alt: None, shadow: [1, len as u32, 0, lead], natural: cbv };
            }
            Some((BufferKind::Raw { uav }, _)) => ([1, len as u32, 0, lead], uav.then(|| self.raw_uav(&r, start, dwords))),
            // records past the memory the model knows are out of range (as the raw view's length is)
            Some((BufferKind::Structured { uav, stride }, n)) => ([stride, n.min((len / u64::from(stride.max(1))) as u32), 0, lead], uav.then(|| self.raw_uav(&r, start, dwords))),
            Some((BufferKind::Typed { uav, format }, n)) => {
                let e = format::element(format).map_or(4, |e| e.0);
                // no more elements than the memory holds from the view's start: a view past its buffer is an
                // invalid call that removes the device (debug #31, a title view of 1,048,576 R32_UINT over
                // 64 KiB the model knows)
                let asked = n;
                let n = n.min((len / e) as u32);
                if n < asked {
                    heap.stats.clamped.fetch_add(1, Ordering::Relaxed);
                }
                let v = if n == 0 {
                    if uav {
                        HostView::NullUav
                    } else {
                        HostView::NullBuffer
                    }
                } else if off % e == 0 {
                    if uav {
                        if resource_uav(r.resource) {
                            HostView::typed_uav(r.resource, format, off / e, n)
                        } else {
                            HostView::NullUav
                        }
                    } else {
                        HostView::typed_srv(r.resource, format, off / e, n)
                    }
                } else {
                    heap.stats.misaligned.fetch_add(1, Ordering::Relaxed);
                    if uav {
                        HostView::NullUav
                    } else {
                        HostView::NullBuffer
                    }
                };
                ([e as u32, n, 0, lead], Some(v))
            }
        };
        let natural = alt.unwrap_or(primary);
        Plan { primary, alt, shadow, natural }
    }

    fn raw_uav(&self, r: &MemoryRange, start: u64, dwords: u32) -> HostView {
        if resource_uav(r.resource) {
            HostView::raw_uav(r.resource, start / 4, dwords)
        } else {
            HostView::NullUav
        }
    }

    /// Mirror slot `i` of `heap` from its bytes. `force`: even if the bytes are unchanged.
    pub(crate) fn mirror(&self, heap: &HeapState, i: u32, force: bool) {
        if i >= heap.count {
            return;
        }
        let at = heap.start + u64::from(i) * u64::from(heap.stride);
        let (w, fp) = if heap.kind == SAMPLERS {
            // SAFETY: a slot of the heap's memory (the title's or ours).
            let w = unsafe { (at as *const [u32; 4]).read_volatile() };
            let mut w8 = [0u32; 8];
            w8[..4].copy_from_slice(&w);
            (w8, records::fingerprint(&w))
        } else {
            // SAFETY: as above; RTV and DSV slots use their first 32 of 128 bytes.
            let w = unsafe { (at as *const [u32; 8]).read_volatile() };
            (w, records::fingerprint(&w))
        };
        // a statistic: a racy max is enough, and costs no locked instruction
        if i > heap.stats.highest.load(Ordering::Relaxed) {
            heap.stats.highest.store(i, Ordering::Relaxed);
        }
        if i >= heap.mirrored {
            if heap.stats.beyond.fetch_add(1, Ordering::Relaxed) == 0 {
                self.events.note("descriptor index past the host heap", u64::from(i));
            }
            return;
        }
        let seen = &heap.seen[i as usize];
        if !force && seen.load(Ordering::Relaxed) == fp {
            self.counters.views_skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        seen.store(fp, Ordering::Relaxed);
        self.counters.views_mirrored.fetch_add(1, Ordering::Relaxed);
        match heap.kind {
            VIEWS if heap.visible => {
                let plan = self.plan(&w, heap);
                self.write_view(heap.host.cpu_at(i), &plan.primary);
                let alt = self.place_alt(heap, i, plan.alt);
                if let Some(s) = &heap.shadow {
                    let mut e = plan.shadow;
                    e[2] = alt;
                    s.write(i, e);
                }
            }
            VIEWS => {
                let plan = self.plan(&w, heap);
                self.host.write(heap.host.cpu_at(i), &plan.natural);
            }
            SAMPLERS => {
                let mut w4 = [0u32; 4];
                w4.copy_from_slice(&w[..4]);
                let desc = match Record::decode16(&w4).and_then(|r| match r {
                    Record::Entry { index, generation } => self.entries.get(index, generation),
                    _ => None,
                }) {
                    Some(EntryView::Sampler(d)) => d,
                    _ => crate::sampler::default_desc(),
                };
                self.host.write(heap.host.cpu_at(i), &HostView::Sampler(desc));
            }
            RTV | DSV => {
                if let Some(Record::Entry { index, generation }) = Record::decode(&w) {
                    if let Some(EntryView::Rtv(v) | EntryView::Dsv(v)) = self.entries.get(index, generation) {
                        self.host.write(heap.host.cpu_at(i), &v);
                    }
                }
            }
            _ => {}
        }
    }

    /// Write a buffer view (a raw SRV at every buffer descriptor's index; typed and UAV
    /// alternates) from this thread's cache of recent ones when it is there: creating one costs
    /// ~120 ns on the card, copying one ~5-11 ns, and the title makes its views at ring
    /// addresses that recur. Other views are written as they are.
    pub(crate) fn write_view(&self, at: usize, v: &HostView) {
        // SAFETY: buffer members of buffer views, selected by the dimension.
        let key = match v {
            HostView::Srv { resource, desc } if !resource.is_null() && desc.ViewDimension == D3D12_SRV_DIMENSION_BUFFER => {
                let b = unsafe { desc.Anonymous.Buffer };
                (*resource as usize, b.FirstElement, b.NumElements, desc.Format.0 as u32 | (b.Flags.0 as u32) << 24)
            }
            HostView::Uav { resource, desc } if !resource.is_null() && desc.ViewDimension == D3D12_UAV_DIMENSION_BUFFER => {
                let b = unsafe { desc.Anonymous.Buffer };
                (*resource as usize, b.FirstElement, b.NumElements, desc.Format.0 as u32 | (b.Flags.0 as u32) << 24 | 1 << 31)
            }
            _ => return self.host.write(at, v),
        };
        RAW_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.as_ref().is_none_or(|c| c.owner != self.id) {
                *c = self.host.heap(VIEWS, RAW_SLOTS, false).ok().map(|host| RawCache { owner: self.id, host, keys: vec![(0, 0, 0, 0); RAW_SLOTS as usize].into_boxed_slice() });
            }
            let Some(c) = c.as_mut() else { return self.host.write(at, v) };
            let h = (key.0 as u64 >> 6) ^ key.1.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(key.2).rotate_left(17) ^ u64::from(key.3).rotate_left(41);
            let slot = ((h ^ (h >> 29)) % u64::from(RAW_SLOTS)) as u32;
            let src = c.host.cpu_at(slot);
            if c.keys[slot as usize] != key {
                self.host.write(src, v);
                c.keys[slot as usize] = key;
            }
            self.host.copy(at, src, VIEWS);
        })
    }

    /// Write slot `i`'s alternate view; returns its host index (the null slot when none).
    fn place_alt(&self, heap: &HeapState, i: u32, alt: Option<HostView>) -> u32 {
        match &heap.alt {
            // a slot with no alternate leaves its fixed one as it was: nothing names it
            Alt::Fixed(base) => match alt {
                Some(v) => {
                    self.write_view(heap.host.cpu_at(base + i), &v);
                    base + i
                }
                None => heap.null_index,
            },
            Alt::Pool(free) => {
                let cell = &heap.alts[i as usize];
                let had = cell.load(Ordering::Relaxed);
                match alt {
                    Some(v) => {
                        let at = if had != NO_ALT {
                            had
                        } else {
                            match free.lock().unwrap_or_else(|e| e.into_inner()).pop() {
                                Some(a) => a,
                                None => {
                                    if heap.stats.pool_empty.fetch_add(1, Ordering::Relaxed) == 0 {
                                        self.events.note("alternate view pool empty", u64::from(i));
                                    }
                                    return heap.null_index;
                                }
                            }
                        };
                        cell.store(at, Ordering::Relaxed);
                        self.write_view(heap.host.cpu_at(at), &v);
                        at
                    }
                    None => {
                        // the title overwrote the slot, so nothing in flight reads its old views
                        if had != NO_ALT {
                            cell.store(NO_ALT, Ordering::Relaxed);
                            free.lock().unwrap_or_else(|e| e.into_inner()).push(had);
                        }
                        heap.null_index
                    }
                }
            }
            Alt::None => heap.null_index,
        }
    }

    /// Mirror the slot at console handle `h`, if it is in a heap.
    fn mirror_at(&self, h: u64) {
        match self.heap_at(h) {
            Some((heap, i)) => self.mirror(&heap, i, true),
            None => {
                self.counters.unheaped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn transient(&self) -> Option<&Transient> {
        self.transient
            .get_or_init(|| self.host.heap(VIEWS, TRANSIENT, false).ok().map(|host| Transient { host, next: AtomicU32::new(0) }))
            .as_ref()
    }
}

#[derive(Clone, Copy)]
enum BufferKind {
    Raw { uav: bool },
    Structured { uav: bool, stride: u32 },
    Typed { uav: bool, format: u32 },
}

/// The host resource takes unordered access (resources from the lead's model, or a null
/// resource in the recording stand-in, which takes everything).
/// `REBUILD_TRACE_ENTRY=<index>[,<index>...]`: where texture view entry `index` comes from (the console
/// address its texture lives at, its description and console tiling words), and how many of its first 64 KiB of
/// console memory are non-zero now. [`traced_entries`] gives them for later checks (`present` prints the same at
/// captured frames: whether the title's texel data ever reaches that memory).
fn trace_entry(rec: &Record, what: &str, address: u64, d: &D3D12_RESOURCE_DESC, words: Option<&[u32; 14]>) {
    static WANT: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let want = WANT.get_or_init(|| std::env::var("REBUILD_TRACE_ENTRY").map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default());
    let Record::Entry { index, .. } = *rec else { return };
    if !want.contains(&index) {
        return;
    }
    let bytes = d.Width * u64::from(d.Height) * u64::from(d.DepthOrArraySize);
    eprintln!(
        "entry     {index} ({what}): texture at {address:#x}, {}x{}x{} format {} mips {} dimension {} layout {} flags {:#x} (about {bytes:#x} bytes at 1 byte a texel); console words {words:x?}; {}",
        d.Width, d.Height, d.DepthOrArraySize, d.Format.0, d.MipLevels, d.Dimension.0, d.Layout.0, d.Flags.0, nonzero(address, 64 << 10)
    );
    TRACED.lock().unwrap_or_else(|e| e.into_inner()).push((index, address));
}

static TRACED: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

/// The texture view entries `REBUILD_TRACE_ENTRY` named, and their console addresses.
pub fn traced_entries() -> Vec<(u32, u64)> {
    TRACED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// "n of m bytes non-zero" for console memory at `address` (when it is committed memory the CPU can read).
pub fn nonzero(address: u64, bytes: usize) -> String {
    use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};
    let mut m = MEMORY_BASIC_INFORMATION::default();
    if address == 0 || unsafe { VirtualQuery(Some(address as *const std::ffi::c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 || m.State != MEM_COMMIT {
        return format!("{address:#x} is not committed memory");
    }
    let end = (m.BaseAddress as u64 + m.RegionSize as u64).min(address + bytes as u64);
    // SAFETY: committed memory of this process, inside the region VirtualQuery named
    let b = unsafe { std::slice::from_raw_parts(address as *const u8, (end - address) as usize) };
    format!("{} of {} bytes non-zero (protect {:#x})", b.iter().filter(|x| **x != 0).count(), b.len(), m.Protect.0)
}

/// The null view of one null-bank kind: the type translated shaders declare for it, so a read past the
/// mirrored range (or of a missing alternate) is a typed null read, never a type mismatch.
pub(crate) fn null_view(k: rebuild_sir::lift::NullKind) -> HostView {
    use rebuild_sir::lift::NullKind;
    // the console's dim (0 1D, 1 2D, 2 3D, 3 cube, 4 1D array, 5 2D array) as a host SRV / UAV dimension
    let srv_dim = |d: u8| [D3D12_SRV_DIMENSION_TEXTURE1D, D3D12_SRV_DIMENSION_TEXTURE2D, D3D12_SRV_DIMENSION_TEXTURE3D, D3D12_SRV_DIMENSION_TEXTURECUBE, D3D12_SRV_DIMENSION_TEXTURE1DARRAY, D3D12_SRV_DIMENSION_TEXTURE2DARRAY][usize::from(d.min(5))];
    // a cube has no UAV: the translator declares none (RW cubes read as 2D arrays)
    let uav_dim = |d: u8| [D3D12_UAV_DIMENSION_TEXTURE1D, D3D12_UAV_DIMENSION_TEXTURE2D, D3D12_UAV_DIMENSION_TEXTURE3D, D3D12_UAV_DIMENSION_TEXTURE2DARRAY, D3D12_UAV_DIMENSION_TEXTURE1DARRAY, D3D12_UAV_DIMENSION_TEXTURE2DARRAY][usize::from(d.min(5))];
    let uav_tex = |d: u8, format: DXGI_FORMAT| {
        let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: format, ViewDimension: uav_dim(d), ..Default::default() };
        if matches!(desc.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY | D3D12_UAV_DIMENSION_TEXTURE1DARRAY) {
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV { ArraySize: 1, ..Default::default() };
        } else if desc.ViewDimension == D3D12_UAV_DIMENSION_TEXTURE3D {
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV { WSize: 1, ..Default::default() };
        }
        HostView::Uav { resource: std::ptr::null_mut(), desc }
    };
    match k {
        NullKind::Raw => HostView::NullBuffer,
        NullKind::RwRaw => HostView::NullUav,
        NullKind::Typed => HostView::typed_srv(std::ptr::null_mut(), DXGI_FORMAT_R32G32B32A32_FLOAT.0 as u32, 0, 1),
        NullKind::RwTyped => HostView::typed_uav(std::ptr::null_mut(), DXGI_FORMAT_R32G32B32A32_FLOAT.0 as u32, 0, 1),
        NullKind::Tex(d) => HostView::NullTexture(srv_dim(d).0),
        NullKind::RwTex(d) => uav_tex(d, DXGI_FORMAT_R32G32B32A32_FLOAT),
        NullKind::RwTexUint(d) => uav_tex(d, DXGI_FORMAT_R32_UINT),
    }
}

fn resource_uav(r: Raw) -> bool {
    match unsafe { ID3D12Resource::from_raw_borrowed(&r) } {
        None => true,
        Some(res) => unsafe { res.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0,
    }
}

/// A key of a view description: its header words and the words of the union member its
/// dimension selects (padding and the rest of the union are left out: they hold no value).
fn key_of<T>(head: &[u32], union: &T, words: usize) -> u64 {
    let mut w = head.to_vec();
    let p = (union as *const T).cast::<u32>();
    // SAFETY: `words` is within the union member the dimension selects.
    w.extend((0..words.min(size_of::<T>() / 4)).map(|i| unsafe { p.add(i).read_unaligned() }));
    records::fingerprint(&w)
}

fn srv_key(v: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    let n = [0, 6, 3, 5, 4, 6, 1, 2, 3, 3, 5, 2].get(v.ViewDimension.0 as usize).copied().unwrap_or(6);
    key_of(&[v.Format.0 as u32, v.ViewDimension.0 as u32, v.Shader4ComponentMapping, 1], &v.Anonymous, n)
}

fn uav_key(v: &D3D12_UNORDERED_ACCESS_VIEW_DESC) -> u64 {
    let n = [0, 6, 1, 3, 2, 4, 1, 2, 3].get(v.ViewDimension.0 as usize).copied().unwrap_or(6);
    key_of(&[v.Format.0 as u32, v.ViewDimension.0 as u32, 2], &v.Anonymous, n)
}

fn rtv_key(v: &D3D12_RENDER_TARGET_VIEW_DESC) -> u64 {
    let n = [0, 3, 1, 3, 2, 4, 1, 2, 3].get(v.ViewDimension.0 as usize).copied().unwrap_or(4);
    key_of(&[v.Format.0 as u32, v.ViewDimension.0 as u32, 3], &v.Anonymous, n)
}

fn dsv_key(v: &D3D12_DEPTH_STENCIL_VIEW_DESC) -> u64 {
    let n = [0, 1, 3, 1, 3, 1, 2].get(v.ViewDimension.0 as usize).copied().unwrap_or(3);
    key_of(&[v.Format.0 as u32, v.ViewDimension.0 as u32, v.Flags.0 as u32, 4], &v.Anonymous, n)
}

/// A default SRV of a texture (the title passed no description).
fn default_srv(r: &Resource) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let d = &r.desc;
    let format = DXGI_FORMAT(format::default_view_format(r.view_format, false) as i32);
    let mut v = D3D12_SHADER_RESOURCE_VIEW_DESC { Format: format, Shader4ComponentMapping: crate::views::MAPPING, ..Default::default() };
    let arrays = u32::from(d.DepthOrArraySize.max(1));
    match d.Dimension.0 {
        DIMENSION_TEXTURE1D => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1DARRAY;
            v.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_SRV { MipLevels: u32::MAX, ArraySize: arrays, ..Default::default() };
        }
        DIMENSION_TEXTURE3D => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE3D;
            v.Anonymous.Texture3D = D3D12_TEX3D_SRV { MipLevels: u32::MAX, ..Default::default() };
        }
        _ if d.SampleDesc.Count > 1 => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY;
            v.Anonymous.Texture2DMSArray = D3D12_TEX2DMS_ARRAY_SRV { FirstArraySlice: 0, ArraySize: arrays };
        }
        _ => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DARRAY;
            v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_SRV { MipLevels: u32::MAX, ArraySize: arrays, ..Default::default() };
        }
    }
    v
}

fn default_uav(r: &Resource) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let d = &r.desc;
    let mut v = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: DXGI_FORMAT(r.view_format as i32), ..Default::default() };
    match d.Dimension.0 {
        DIMENSION_TEXTURE1D => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            v.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV { ArraySize: u32::from(d.DepthOrArraySize.max(1)), ..Default::default() };
        }
        DIMENSION_TEXTURE3D => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            v.Anonymous.Texture3D = D3D12_TEX3D_UAV { WSize: u32::MAX, ..Default::default() };
        }
        _ => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV { ArraySize: u32::from(d.DepthOrArraySize.max(1)), ..Default::default() };
        }
    }
    v
}

/// A view format the title names, as the host has it (0: the resource's own).
fn view_format(f: i32, r: &Resource, depth: bool) -> DXGI_FORMAT {
    if f == 0 {
        return DXGI_FORMAT(format::default_view_format(r.view_format, depth) as i32);
    }
    let f = format::host_format(f as u32);
    let f = if f == format::STAND_IN.0 && r.view_format == format::STAND_IN.1 { format::STAND_IN.1 } else { f };
    DXGI_FORMAT(f as i32)
}

/// Bytes per element of a buffer view: raw 4, structured its stride, typed its format's.
fn elem_bytes(format: i32, stride: u32, raw: bool) -> u64 {
    if raw {
        4
    } else if stride > 0 {
        u64::from(stride)
    } else {
        format::element(format::host_format(format as u32)).map_or(4, |e| e.0)
    }
}

fn write_record(handle: u64, r: &Record) {
    // SAFETY: the handle is a descriptor slot in memory the title owns (or ours).
    unsafe { (handle as *mut [u32; 8]).write_unaligned(r.encode()) };
}

impl Resources {
    /// 0x78 GetDescriptorHandleIncrementSize.
    pub fn increment(&self, kind: u32) -> u32 {
        INCREMENTS.get(kind as usize).copied().unwrap_or(0)
    }

    /// 0x70 CreateDescriptorHeap `(*desc{type, count, flags, mask}, iid, *out)`; newer titles put
    /// `{address u64, size u64}` (the title's own memory) right after the description.
    ///
    /// # Safety
    /// `desc` as the title passes it (the 16 bytes after it are read as a hint).
    pub unsafe fn create_descriptor_heap(&self, desc: *const [u32; 4], iid: *const GUID, out: *mut Raw) -> HRESULT {
        let ctx = &self.inner;
        let Some(&d) = (unsafe { desc.as_ref() }) else { return E_INVALIDARG };
        let (kind, count, flags) = (d[0] as i32, d[1], d[2]);
        let Some(&stride) = INCREMENTS.get(kind as usize) else { return E_INVALIDARG };
        let bytes = u64::from(stride) * u64::from(count.max(1));
        // SAFETY: read as a hint; a title passing the plain description has its own data there.
        let (address, size) = unsafe { desc.add(1).cast::<(u64, u64)>().read_unaligned() };
        let placed = address != 0 && size == bytes;
        let mut owned = None;
        let start = if placed {
            address
        } else {
            let mem = vec![0u64; bytes.div_ceil(8) as usize].into_boxed_slice();
            let a = mem.as_ptr() as u64;
            owned = Some(mem);
            a
        };
        let visible = flags & 1 != 0 && matches!(kind, VIEWS | SAMPLERS);
        let limits = ctx.limits;
        let (host_count, mirrored, alt, null_index) = match kind {
            VIEWS if visible => {
                let p = limits.null_index();
                let [low, high] = limits.alternate_ranges();
                let alt = if count.min(p) <= low.end - low.start {
                    Alt::Fixed(low.start)
                } else {
                    Alt::Pool(Mutex::new(high.rev().chain(low.rev()).collect()))
                };
                (limits.views, count.min(p), alt, p)
            }
            SAMPLERS if visible => (count.min(limits.samplers), count.min(limits.samplers), Alt::None, 0),
            _ => (count, count, Alt::None, 0),
        };
        let host = match ctx.host.heap(kind, host_count, visible) {
            Ok(h) => h,
            Err(hr) => {
                ctx.counters.refused.fetch_add(1, Ordering::Relaxed);
                ctx.events.note("host descriptor heap refused", u64::from(host_count));
                return hr;
            }
        };
        let shadow = if kind == VIEWS && visible {
            match ctx.host.shadow(null_index + 1) {
                Ok(s) => Some(s),
                Err(hr) => return hr,
            }
        } else {
            None
        };
        if mirrored < count {
            ctx.events.note("console heap larger than the host mirrors", u64::from(count));
        }
        let pool = matches!(alt, Alt::Pool(_));
        let zero_fp = if kind == SAMPLERS { records::fingerprint(&[0u32; 4]) } else { records::fingerprint(&[0u32; 8]) };
        let state = Arc::new(HeapState {
            kind,
            visible,
            start,
            count,
            stride,
            host,
            mirrored,
            null_index,
            shadow,
            alt,
            // every host slot starts as what zero bytes mirror to (null): zeroed slots need no work
            seen: (0..mirrored).map(|_| AtomicU64::new(zero_fp)).collect(),
            alts: (0..if pool { mirrored } else { 0 }).map(|_| AtomicU32::new(NO_ALT)).collect(),
            stats: HeapStats::default(),
        });
        // the null bank, and the shadow entry a console index past the range reads
        if kind == VIEWS && visible {
            for k in rebuild_sir::lift::NullKind::all() {
                ctx.write_view(state.host.cpu_at(null_index + k.slot()), &null_view(k));
            }
            if let Some(s) = &state.shadow {
                s.write(null_index, [0, 0, null_index, 0]);
            }
        }
        {
            let mut heaps = ctx.heaps.write().unwrap_or_else(|e| e.into_inner());
            let k = heaps.partition_point(|r| r.start < start);
            heaps.insert(k, Registered { start, end: start + bytes, state: state.clone() });
        }
        // what the title's memory already holds (a heap placed over records)
        if placed {
            for i in 0..mirrored {
                ctx.mirror(&state, i, false);
            }
        }
        let h = Box::new(DescriptorHeap { head: Head::new(Object::DescriptorHeap, ctx), desc: d, state, _owned: owned });
        object::hand_out(Box::into_raw(h).cast(), Object::DescriptorHeap, iid, out, dh_release)
    }

    /// 0x88 CreateConstantBufferView `(*desc{va u64, size u32}, handle)`.
    ///
    /// # Safety
    /// `handle` is a descriptor slot of the title's; `desc` null or a description.
    pub unsafe fn create_cbv(&self, desc: *const D3D12_CONSTANT_BUFFER_VIEW_DESC, handle: u64) {
        let r = match unsafe { desc.as_ref() } {
            Some(d) if d.BufferLocation != 0 => Record::Cbv { va: d.BufferLocation, size: d.SizeInBytes },
            _ => Record::NullCbv,
        };
        self.put(handle, &r);
    }

    /// 0x90 CreateShaderResourceView `(resource, *desc, handle)`.
    ///
    /// # Safety
    /// As [`Resources::create_cbv`]; `res` one of ours or null.
    pub unsafe fn create_srv(&self, res: Raw, desc: *const D3D12_SHADER_RESOURCE_VIEW_DESC, handle: u64) {
        let desc = unsafe { desc.as_ref() };
        let rec = match resource(res) {
            None => match desc {
                Some(d) => Record::Null { uav: false, dimension: d.ViewDimension.0 as u32 },
                None => Record::Null { uav: false, dimension: 1 },
            },
            Some(r) if r.is_buffer() => {
                let (first, n, stride, raw, f) = match desc {
                    Some(d) if d.ViewDimension == D3D12_SRV_DIMENSION_BUFFER => {
                        // SAFETY: the buffer member, selected by the dimension.
                        let b = unsafe { d.Anonymous.Buffer };
                        (b.FirstElement, b.NumElements, b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, d.Format.0)
                    }
                    _ => (0, (r.desc.Width / 4) as u32, 0, true, 0),
                };
                let e = elem_bytes(f, stride, raw);
                Record::Buffer { uav: false, va: r.address + first * e, format: f as u32, elements: n, stride, flags: if raw { RAW } else { 0 } }
            }
            Some(r) => {
                let mut v = match desc {
                    Some(d) => *d,
                    None => default_srv(r),
                };
                v.Format = view_format(v.Format.0, r, false);
                let k = srv_key(&v);
                self.texture_entry(r, k, HostView::Srv { resource: r.host_raw(), desc: v })
            }
        };
        self.put(handle, &rec);
    }

    /// 0x98 CreateUnorderedAccessView `(resource, counter, *desc, handle)`. Counters are not
    /// carried (noted once).
    ///
    /// # Safety
    /// As [`Resources::create_srv`].
    pub unsafe fn create_uav(&self, res: Raw, counter: Raw, desc: *const D3D12_UNORDERED_ACCESS_VIEW_DESC, handle: u64) {
        if !counter.is_null() {
            self.inner.events.note("UAV counter not carried", counter as u64);
        }
        let desc = unsafe { desc.as_ref() };
        let rec = match resource(res) {
            None => Record::Null { uav: true, dimension: desc.map_or(1, |d| d.ViewDimension.0 as u32) },
            Some(r) if r.is_buffer() => {
                let (first, n, stride, raw, f) = match desc {
                    Some(d) if d.ViewDimension == D3D12_UAV_DIMENSION_BUFFER => {
                        // SAFETY: the buffer member, selected by the dimension.
                        let b = unsafe { d.Anonymous.Buffer };
                        (b.FirstElement, b.NumElements, b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, d.Format.0)
                    }
                    _ => (0, (r.desc.Width / 4) as u32, 0, true, 0),
                };
                let e = elem_bytes(f, stride, raw);
                Record::Buffer { uav: true, va: r.address + first * e, format: f as u32, elements: n, stride, flags: if raw { RAW } else { 0 } }
            }
            // the host resource has no unordered access (the console allows it on any resource; the host strips it
            // from depth targets): a null view, never an invalid descriptor (WARP faults on one, then removes)
            Some(r) if crate::format::host_desc(&r.desc).0.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 => {
                if NO_UAV.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                    self.inner.events.note("UAV of a resource without unordered access: a null view (counted)", r.desc.Flags.0 as u64);
                }
                let v = match desc {
                    Some(d) => *d,
                    None => default_uav(r),
                };
                Record::Null { uav: true, dimension: v.ViewDimension.0 as u32 }
            }
            Some(r) => {
                let mut v = match desc {
                    Some(d) => *d,
                    None => default_uav(r),
                };
                v.Format = view_format(v.Format.0, r, false);
                let k = uav_key(&v);
                self.texture_entry(r, k, HostView::Uav { resource: r.host_raw(), desc: v })
            }
        };
        self.put(handle, &rec);
    }

    /// 0xa0 CreateRenderTargetView `(resource, *desc, handle)`.
    ///
    /// # Safety
    /// As [`Resources::create_srv`].
    pub unsafe fn create_rtv(&self, res: Raw, desc: *const D3D12_RENDER_TARGET_VIEW_DESC, handle: u64) {
        let Some(r) = resource(res) else { return };
        let mut v = match unsafe { desc.as_ref() } {
            Some(d) => *d,
            None => {
                let mut v = D3D12_RENDER_TARGET_VIEW_DESC::default();
                if r.is_buffer() {
                    v.ViewDimension = D3D12_RTV_DIMENSION_BUFFER;
                } else if r.desc.DepthOrArraySize > 1 || r.desc.Dimension.0 == DIMENSION_TEXTURE3D {
                    v.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2DARRAY;
                    v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_RTV { ArraySize: u32::from(r.desc.DepthOrArraySize), ..Default::default() };
                } else if r.desc.SampleDesc.Count > 1 {
                    v.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2DMS;
                } else {
                    v.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2D;
                }
                v
            }
        };
        v.Format = view_format(v.Format.0, r, false);
        let k = rtv_key(&v);
        let rec = self.entry_for(r, k, || EntryView::Rtv(HostView::Rtv { resource: r.host_raw(), desc: v }));
        self.put(handle, &rec);
    }

    /// 0xa8 CreateDepthStencilView `(resource, *desc, handle)`.
    ///
    /// # Safety
    /// As [`Resources::create_srv`].
    pub unsafe fn create_dsv(&self, res: Raw, desc: *const D3D12_DEPTH_STENCIL_VIEW_DESC, handle: u64) {
        let Some(r) = resource(res) else { return };
        let mut v = match unsafe { desc.as_ref() } {
            Some(d) => *d,
            None => {
                let mut v = D3D12_DEPTH_STENCIL_VIEW_DESC::default();
                if r.desc.DepthOrArraySize > 1 {
                    v.ViewDimension = D3D12_DSV_DIMENSION_TEXTURE2DARRAY;
                    v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_DSV { ArraySize: u32::from(r.desc.DepthOrArraySize), ..Default::default() };
                } else if r.desc.SampleDesc.Count > 1 {
                    v.ViewDimension = D3D12_DSV_DIMENSION_TEXTURE2DMS;
                } else {
                    v.ViewDimension = D3D12_DSV_DIMENSION_TEXTURE2D;
                }
                v
            }
        };
        v.Format = view_format(v.Format.0, r, true);
        let k = dsv_key(&v);
        let rec = self.entry_for(r, k, || EntryView::Dsv(HostView::Dsv { resource: r.host_raw(), desc: v }));
        self.put(handle, &rec);
    }

    /// 0xb0 CreateSampler `(*desc (13 words), handle)`.
    ///
    /// # Safety
    /// `handle` a sampler slot of the title's; `desc` a description or null.
    pub unsafe fn create_sampler(&self, desc: *const D3D12_SAMPLER_DESC, handle: u64) {
        let Some(d) = (unsafe { desc.as_ref() }) else { return };
        let r = self.inner.entries.sampler(&crate::sampler::sanitize(d));
        self.put16(handle, &r);
    }

    /// 0x1e8 CreateSamplerX `(*desc (27 console words), handle)`.
    ///
    /// # Safety
    /// As [`Resources::create_sampler`].
    pub unsafe fn create_sampler_x(&self, desc: *const [u32; 27], handle: u64) {
        let Some(w) = (unsafe { desc.as_ref() }) else { return };
        let r = self.inner.entries.sampler(&crate::sampler::console(w));
        self.put16(handle, &r);
    }

    /// 0x230 / 0x238: a view of memory the title placed itself `(*resource desc (14 words),
    /// *view desc (16 words: the PC description's 10, then the address in 10-11), handle)`.
    ///
    /// # Safety
    /// As [`Resources::create_cbv`].
    pub unsafe fn create_placed_view(&self, uav: bool, res: *const [u32; 14], view: *const [u32; 16], handle: u64) {
        let (Some(rw), Some(vw)) = (unsafe { res.as_ref() }, unsafe { view.as_ref() }) else { return };
        self.inner.counters.views_created.fetch_add(1, Ordering::Relaxed);
        let address = u64::from(vw[10]) | u64::from(vw[11]) << 32;
        // SAFETY: 14 words are a D3D12_RESOURCE_DESC.
        let rd: D3D12_RESOURCE_DESC = unsafe { std::mem::transmute_copy(rw) };
        let mut pc = [0u32; 10];
        pc.copy_from_slice(&vw[..10]);
        if rd.Dimension.0 == DIMENSION_BUFFER {
            // the PC description's buffer member: words 4-5 first element, 6 count, 7 stride, 8 flags
            let first = u64::from(pc[4]) | u64::from(pc[5]) << 32;
            let raw = pc[8] & 1 != 0;
            let e = elem_bytes(pc[0] as i32, pc[7], raw);
            let r = Record::Buffer { uav, va: address + first * e, format: pc[0], elements: pc[6], stride: pc[7], flags: if raw { RAW } else { 0 } };
            write_record(handle, &r);
            self.inner.mirror_at(handle);
            return;
        }
        let k = (uav, address, *rw, pc);
        let known = self.inner.placed_views.lock().unwrap_or_else(|e| e.into_inner()).get(&k).copied();
        let rec = match known {
            Some(r) => r,
            None => {
                let tex = self.placed_texture(address, rw, &rd);
                let rec = match tex {
                    Some(t) => {
                        let raw = t.as_raw();
                        let (_, vf) = format::host_desc(&rd);
                        let fmt = |f: u32| DXGI_FORMAT(if f == 0 { format::default_view_format(vf, false) } else { format::host_format(f) } as i32);
                        let view = if uav {
                            // SAFETY: 10 words are a D3D12_UNORDERED_ACCESS_VIEW_DESC.
                            let mut d: D3D12_UNORDERED_ACCESS_VIEW_DESC = unsafe { std::mem::transmute_copy(&pc) };
                            d.Format = fmt(pc[0]);
                            HostView::Uav { resource: raw, desc: d }
                        } else {
                            // SAFETY: 10 words are a D3D12_SHADER_RESOURCE_VIEW_DESC.
                            let mut d: D3D12_SHADER_RESOURCE_VIEW_DESC = unsafe { std::mem::transmute_copy(&pc) };
                            d.Format = fmt(pc[0]);
                            HostView::Srv { resource: raw, desc: d }
                        };
                        let rec = self.inner.entries.add(EntryView::Texture(view), 0);
                        trace_entry(&rec, &format!("placed view, host {raw:p}"), address, &rd, Some(rw));
                        rec
                    }
                    None => Record::Null { uav, dimension: pc[1] },
                };
                self.inner.placed_views.lock().unwrap_or_else(|e| e.into_inner()).insert(k, rec);
                rec
            }
        };
        write_record(handle, &rec);
        self.inner.mirror_at(handle);
    }

    fn placed_texture(&self, address: u64, rw: &[u32; 14], rd: &D3D12_RESOURCE_DESC) -> Option<ID3D12Resource> {
        let ctx = &self.inner;
        let mut placed = ctx.placed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = placed.get(&(address, *rw)) {
            return t.clone();
        }
        let (hd, _) = format::host_desc(rd);
        let t = ctx.memory.texture(address, &hd).or_else(|| ctx.host.resource(&hd, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_NONE, None).ok().flatten());
        placed.insert((address, *rw), t.clone());
        t
    }

    /// The entry for a view of texture `r` with key `k`: one per distinct view.
    fn texture_entry(&self, r: &Resource, k: u64, v: HostView) -> Record {
        self.entry_for(r, k, || EntryView::Texture(v))
    }

    fn entry_for(&self, r: &Resource, k: u64, make: impl FnOnce() -> EntryView) -> Record {
        let mut list = r.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&(_, i)) = list.iter().find(|(kk, _)| *kk == k) {
            let generation = self.inner.entries.generation(i);
            if self.inner.entries.get(i, generation).is_some() {
                return Record::Entry { index: i, generation };
            }
        }
        let rec = self.inner.entries.add(make(), r as *const Resource as usize);
        if let Record::Entry { index, .. } = rec {
            list.push((k, index));
        }
        trace_entry(&rec, &format!("view of console resource {:p}, host {:p}", r as *const Resource, r.host_raw()), r.address, &r.desc, None);
        rec
    }

    fn put(&self, handle: u64, r: &Record) {
        self.inner.counters.views_created.fetch_add(1, Ordering::Relaxed);
        if handle == 0 {
            return;
        }
        write_record(handle, r);
        self.inner.mirror_at(handle);
    }

    fn put16(&self, handle: u64, r: &Record) {
        self.inner.counters.views_created.fetch_add(1, Ordering::Relaxed);
        if handle == 0 {
            return;
        }
        // SAFETY: a sampler slot of the title's.
        unsafe { (handle as *mut [u32; 4]).write_unaligned(r.encode16()) };
        self.inner.mirror_at(handle);
    }

    /// 0xc0 CopyDescriptorsSimple `(count, dst, src, type)`: the bytes, then the destinations.
    ///
    /// # Safety
    /// Both ranges are descriptor slots the title owns.
    pub unsafe fn copy_descriptors_simple(&self, count: u32, dst: u64, src: u64, kind: u32) {
        let step = u64::from(self.increment(kind));
        if dst == 0 || src == 0 || step == 0 || count == 0 {
            return;
        }
        let len = u64::from(count) * step;
        if !(self.descriptor_memory(src, len, false) && self.descriptor_memory(dst, len, true)) {
            // a handle into memory nobody mapped: never write there; name the first ones
            let n = self.inner.counters.bad_copies.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 4 {
                self.inner.events.note("descriptor copy: destination handle outside mapped memory", dst);
                self.inner.events.note("descriptor copy: its source handle", src);
                self.inner.events.note("descriptor copy: descriptors x increment", (u64::from(count) << 32) | step);
            }
            return;
        }
        unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len as usize) };
        self.sync(dst, len);
    }

    /// `len` bytes of descriptors at CPU handle `at` are memory we may read (or write): inside a console heap
    /// (no system call), or committed title memory with that access.
    fn descriptor_memory(&self, at: u64, len: u64, write: bool) -> bool {
        let end = at.saturating_add(len);
        if self.inner.heaps.read().unwrap_or_else(|e| e.into_inner()).iter().any(|r| r.start <= at && end <= r.end) {
            return true;
        }
        use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
        let mut m = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: a query, no access
        if unsafe { VirtualQuery(Some(at as *const _), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            return false;
        }
        let writable = m.Protect.0 & 0xCC != 0; // READWRITE, WRITECOPY, EXECUTE_READWRITE, EXECUTE_WRITECOPY
        let readable = writable || m.Protect.0 & 0x22 != 0; // READONLY, EXECUTE_READ
        m.State.0 == 0x1000 && (if write { writable } else { readable }) && end <= m.BaseAddress as u64 + m.RegionSize as u64
    }

    /// 0xb8 CopyDescriptors `(ndst, *dst starts, *dst sizes, nsrc, *src starts, *src sizes,
    /// type)`: ranges gathered from the sources and scattered to the destinations, in order (a
    /// null size array: one descriptor per range).
    ///
    /// # Safety
    /// As the D3D12 method.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn copy_descriptors(&self, ndst: u32, dst_starts: *const u64, dst_sizes: *const u32, nsrc: u32, src_starts: *const u64, src_sizes: *const u32, kind: u32) {
        let step = u64::from(self.increment(kind));
        if step == 0 || dst_starts.is_null() || src_starts.is_null() {
            return;
        }
        let ranges = |n: u32, starts: *const u64, sizes: *const u32| -> Vec<(u64, u32)> {
            (0..n as usize).map(|i| unsafe { (*starts.add(i), if sizes.is_null() { 1 } else { *sizes.add(i) }) }).collect()
        };
        let src = ranges(nsrc, src_starts, src_sizes);
        let mut dst = ranges(ndst, dst_starts, dst_sizes).into_iter();
        let (mut to, mut room) = dst.next().unwrap_or((0, 0));
        for (mut from, mut left) in src {
            while left > 0 {
                while room == 0 {
                    let Some(next) = dst.next() else { return };
                    (to, room) = next;
                }
                let n = left.min(room);
                unsafe { self.copy_descriptors_simple(n, to, from, kind) };
                let bytes = u64::from(n) * step;
                (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
            }
        }
    }

    /// The title changed bytes `start..start + len` of descriptor memory (copied records itself;
    /// the lead's write-watch over a heap's pages): every slot they touch whose bytes differ
    /// from what it was mirrored from is mirrored again. Returns the slots looked at.
    pub fn sync(&self, start: u64, len: u64) -> u32 {
        let end = start.saturating_add(len);
        let heaps: Vec<(u64, u64, Arc<HeapState>)> = {
            let h = self.inner.heaps.read().unwrap_or_else(|e| e.into_inner());
            h.iter().filter(|r| r.start < end && start < r.end).map(|r| (r.start, r.end, r.state.clone())).collect()
        };
        let mut looked = 0;
        for (hs, he, state) in heaps {
            let s = u64::from(state.stride);
            let first = ((start.max(hs) - hs) / s) as u32;
            let last = ((end.min(he) - hs).div_ceil(s)) as u32;
            for i in first..last {
                self.inner.mirror(&state, i, false);
            }
            looked += last - first;
        }
        looked
    }

    /// Every slot of a console heap (a heap without write-watch, once).
    pub fn sync_heap(&self, heap: Raw) -> u32 {
        match heap_of(heap) {
            Some(h) => self.sync(h.state.start, u64::from(h.state.count) * u64::from(h.state.stride)),
            None => 0,
        }
    }

    /// The host side of a console descriptor heap.
    pub fn heap_state(&self, heap: Raw) -> Option<Arc<HeapState>> {
        heap_of(heap).map(|h| h.state.clone())
    }

    /// `ListHost::descriptor_heap`: a shader-visible console heap as the host heap bound for it.
    #[inline]
    pub fn binding(&self, heap: Raw) -> Option<HeapBinding> {
        let h = &heap_of(heap)?.state;
        h.visible.then(|| HeapBinding {
            heap: h.host.raw(),
            console_base: h.start,
            console_bytes: u64::from(h.mirrored) * u64::from(h.stride),
            console_stride: h.stride,
            host_base: h.host.gpu,
            host_stride: h.host.step,
            shadow_va: h.shadow.as_ref().map_or(0, |s| s.va),
        })
    }

    /// The shadow table's GPU address for a shader-visible view heap (bind at t0 space101).
    pub fn shadow_va(&self, heap: Raw) -> Option<u64> {
        heap_of(heap)?.state.shadow.as_ref().map(|s| s.va)
    }

    /// `ListHost::cpu_descriptor`: a console CPU handle (RTV, DSV, a CPU-only view; or a
    /// shader-visible view for a UAV clear) as a host CPU handle in a CPU-only heap, brought up
    /// to date with the slot's bytes first.
    pub fn cpu_descriptor(&self, handle: usize) -> Option<usize> {
        let (h, i) = self.inner.heap_at(handle as u64)?;
        if i >= h.mirrored {
            return None;
        }
        if h.kind == VIEWS && h.visible {
            let at = handle as u64;
            // SAFETY: a slot of the heap.
            let w = unsafe { (at as *const [u32; 8]).read_volatile() };
            let plan = self.inner.plan(&w, &h);
            let t = self.inner.transient()?;
            let k = t.next.fetch_add(1, Ordering::Relaxed) % TRANSIENT;
            let cpu = t.host.cpu_at(k);
            self.inner.host.write(cpu, &plan.natural);
            return Some(cpu);
        }
        self.inner.mirror(&h, i, false);
        Some(h.host.cpu_at(i))
    }

    /// The host GPU handle of a console shader-visible view handle's own-kind view (the
    /// alternate of a buffer, the view itself for a texture): the GPU half of a UAV clear.
    pub fn own_kind_gpu(&self, handle: u64) -> Option<u64> {
        let (h, i) = self.inner.heap_at(handle)?;
        if !(h.kind == VIEWS && h.visible) || i >= h.mirrored {
            return None;
        }
        let alt = h.shadow.as_ref().map_or(h.null_index, |s| s.read(i)[2]);
        Some(h.host.gpu_at(if alt != h.null_index { alt } else { i }))
    }

    /// `ListHost::memory`.
    #[inline]
    pub fn memory(&self, va: u64) -> Option<MemoryRange> {
        self.inner.memory.range(va)
    }
}
