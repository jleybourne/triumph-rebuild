//! `rebuild_xlist::console::ListHost` over [`Resources`]: the list translator's view of
//! resources, descriptor heaps, CPU descriptors and memory. The lead wraps it with its own
//! pipelines (and anything else it overrides), or calls the [`Resources`] methods from its own
//! `ListHost`.

use rebuild_xlist::console::{HostPipeline, HostSignature, LibrarySignature, ListHost, PipelineSource, UserMap};

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
    fn input_layout(&self, console: Raw) -> Option<Vec<(u32, u32, u32, bool)>> {
        self.pipelines.input_layout(console)
    }
}

impl<P: PipelineSource + Send + Sync> ListHost for Host<P> {
    fn device(&self) -> Raw {
        self.resources.inner.device.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[inline]
    fn command_signature(&self, console: Raw) -> Option<HostSignature> {
        self.resources.host_command_signature(console)
    }
    fn library_signature(&self, console: Raw, root: Raw, map: &'static UserMap) -> Option<LibrarySignature> {
        self.resources.library_signature(console, root, map)
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
    fn uav_resource(&self, console: Raw, cpu: usize) -> Option<Raw> {
        self.resources.uav_resource(console, cpu)
    }
    #[inline]
    fn uav_twin(&self, console: Raw) -> Option<rebuild_xlist::console::UavTwin> {
        self.resources.uav_twin(console)
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
    fn cpu_memory(&self, va: u64) -> Option<MemoryRange> {
        self.resources.cpu_memory(va)
    }
    fn sync_descriptors(&self, start: u64, len: u64) -> u32 {
        self.resources.sync(start, len)
    }
    fn first_trap(&self, object: &'static str, offset: usize) {
        self.resources.inner.events.first_trap(object, offset);
    }
}
