//! The runtime's side of our FFX implementation: resources, the host list a console list records
//! into, and the upscaler backends.
//!
//! [`XboxHost`] is the title's: console resources through `rebuild_d3d12x::Resources::host_resource`,
//! console lists through `rebuild_xlist::console::segments`. [`DirectHost`] takes host objects as they
//! are (the synthetic FFX client in the tests, and any PC caller).

use std::sync::Arc;

use rebuild_upscale::{Size, Upscaler};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::backends::Factory;
use crate::Raw;

/// An upscaler backend on host objects.
pub type Backend = Box<dyn Upscaler<Resource = ID3D12Resource, CommandList = ID3D12GraphicsCommandList>>;

/// What a context was created for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub max_render: Size,
    pub max_output: Size,
    pub hdr: bool,
    pub depth_reversed: bool,
}

pub trait Host: Send + Sync {
    /// The host resource behind a resource pointer from a descriptor.
    fn resource(&self, p: Raw) -> Option<ID3D12Resource>;

    /// Run `f` on the host list that `list` records into at this moment, in order with the commands
    /// recorded before and after this call. The upscale binds its own descriptor heaps, root signature
    /// and pipeline: the console list must forget its bound state afterwards
    /// (`rebuild_xlist::console::forget_bound_state`).
    fn record(&self, list: Raw, f: &mut dyn FnMut(&ID3D12GraphicsCommandList) -> Result<(), String>) -> Result<(), String>;

    /// A new upscaler for one context.
    fn upscaler(&self, want: &Wanted) -> Result<Backend, String>;

    /// Whether `list` is a command list this host knows (for the first-dispatch log line).
    fn is_list(&self, _list: Raw) -> Option<bool> {
        None
    }
}

/// Host objects as they are: resource pointers are `ID3D12Resource`s, command lists are
/// `ID3D12GraphicsCommandList`s.
pub struct DirectHost {
    pub factory: Factory,
}

impl Host for DirectHost {
    fn resource(&self, p: Raw) -> Option<ID3D12Resource> {
        crate::borrowed_resource(p)
    }

    fn record(&self, list: Raw, f: &mut dyn FnMut(&ID3D12GraphicsCommandList) -> Result<(), String>) -> Result<(), String> {
        let l = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&list) }.ok_or("null command list")?;
        f(l)
    }

    fn upscaler(&self, want: &Wanted) -> Result<Backend, String> {
        self.factory.make(want)
    }
}

/// The title's objects: console resources (d3d12x) and console command lists (xlist).
pub struct XboxHost {
    pub resources: rebuild_d3d12x::Resources,
    pub factory: Factory,
}

impl Host for XboxHost {
    fn resource(&self, p: Raw) -> Option<ID3D12Resource> {
        let raw = self.resources.host_resource(p)?;
        crate::borrowed_resource(raw)
    }

    /// The last segment of the console list is the host list it records into now (a memory wait cuts
    /// the list and opens a new segment; commands after this call land after ours either way).
    fn record(&self, list: Raw, f: &mut dyn FnMut(&ID3D12GraphicsCommandList) -> Result<(), String>) -> Result<(), String> {
        let segments = rebuild_xlist::console::segments(list).ok_or("the command list is not a console list of ours")?;
        let open = segments.last().ok_or("the console list has no open host list")?;
        let r = f(&open.list.list);
        // the upscale bound its own heaps, root signature and pipeline: the title's re-binds after the
        // dispatch must be applied, not dropped as repeats of what the list last saw
        rebuild_xlist::console::forget_bound_state(list);
        r
    }

    fn upscaler(&self, want: &Wanted) -> Result<Backend, String> {
        self.factory.make(want)
    }

    fn is_list(&self, list: Raw) -> Option<bool> {
        Some(rebuild_xlist::console::segments(list).is_some())
    }
}

/// The runtime's one call, when its device and resources exist: serve the title's FFX calls from them.
/// Backends come from the environment ([`Factory::from_env`]).
pub fn install_xbox(resources: rebuild_d3d12x::Resources, device: &ID3D12Device) {
    let factory = Factory::from_env(device);
    eprintln!("ffx: the title's FidelityFX calls are served by rebuild-ffx ({})", factory.describe());
    crate::install(Arc::new(XboxHost { resources, factory }));
}
