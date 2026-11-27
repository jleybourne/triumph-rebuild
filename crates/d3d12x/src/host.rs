//! `rebuild_xlist::console::ListHost` over [`Resources`]: the list translator's view of
//! resources, descriptor heaps, CPU descriptors and memory. The lead wraps it with its own
//! pipelines (and anything else it overrides), or calls the [`Resources`] methods from its own
//! `ListHost`.

use rebuild_xlist::console::{HostPipeline, ListHost, PipelineSource};

use crate::{HeapBinding, MemoryRange, Raw, Resources};

/// Resources and descriptors from a [`Resources`], pipelines from `P`.
pub struct Host<P> {
    pub resources: Resources,
    pub pipelines: P,
}

impl<P: PipelineSource> PipelineSource for Host<P> {
    fn pipeline(&self, console: Raw) -> Option<HostPipeline> {
        self.pipelines.pipeline(console)
    }
}

impl<P: PipelineSource + Send + Sync> ListHost for Host<P> {
    fn device(&self) -> Raw {
        self.resources.inner.device.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[inline]
    fn command_signature(&self, console: Raw) -> Option<Raw> {
        self.resources.host_command_signature(console)
    }
    #[inline]
    fn root_signature(&self, console: Raw) -> Option<Raw> {
        self.resources.host_root_signature(console)
    }
    #[inline]
    fn resource(&self, console: Raw) -> Option<Raw> {
        self.resources.host_resource(console)
    }
    #[inline]
    fn descriptor_heap(&self, console: Raw) -> Option<HeapBinding> {
        self.resources.binding(console)
    }
    #[inline]
    fn cpu_descriptor(&self, console: usize) -> Option<usize> {
        self.resources.cpu_descriptor(console)
    }
    #[inline]
    fn memory(&self, va: u64) -> Option<MemoryRange> {
        self.resources.memory(va)
    }
    fn first_trap(&self, object: &'static str, offset: usize) {
        self.resources.inner.events.first_trap(object, offset);
    }
}
