//! The GPU→CPU region: labels and fences the title's CPU polls.
//!
//! A console title marks GPU progress with bottom-of-pipe writes into its own memory and spins on
//! them. The best PC equivalent is to let the GPU write those labels **straight into the title's
//! memory**: the memory is opened as a D3D12 heap (`OpenExistingHeapFromAddress`), a buffer is placed
//! over it, and each label becomes a `WriteBufferImmediate` in `MARKER_OUT` mode (written once every
//! command before it has completed). No CPU thread relays it, so the label is exact and as early as
//! the hardware allows. The fallback, for memory that cannot be opened, is a worker that waits on the
//! fence and writes the label itself.

#![cfg(windows)]

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

/// Title memory the GPU can write: the heap opened over it and a buffer covering all of it.
pub struct LabelMemory {
    pub heap: ID3D12Heap,
    pub buffer: ID3D12Resource,
    pub va: u64,
    pub len: usize,
}

impl LabelMemory {
    /// Open `len` bytes of CPU memory at `base` (a whole `VirtualAlloc` allocation, 64 KiB aligned)
    /// for the GPU to write.
    pub fn open(device: &ID3D12Device, base: *mut u8, len: usize) -> Result<LabelMemory, String> {
        unsafe {
            let d3: ID3D12Device3 = device.cast().map_err(|e| format!("ID3D12Device3: {e}"))?;
            let heap: ID3D12Heap = d3.OpenExistingHeapFromAddress(base as *const _).map_err(|e| format!("OpenExistingHeapFromAddress: {e}"))?;
            let mut desc = crate::tiles::buffer_desc(len as u64);
            // a resource in a cross-adapter heap must say so; one that is not takes none
            if heap.GetDesc().Flags.0 & D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER.0 != 0 {
                desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER;
            } else {
                desc.Flags = D3D12_RESOURCE_FLAG_NONE;
            }
            let mut r: Option<ID3D12Resource> = None;
            device
                .CreatePlacedResource(&heap, 0, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r)
                .map_err(|e| format!("placed buffer over title memory: {e}"))?;
            let buffer = r.ok_or("placed buffer: none")?;
            let va = buffer.GetGPUVirtualAddress();
            Ok(LabelMemory { heap, buffer, va, len })
        }
    }

    /// Record a bottom-of-pipe label: `value` lands at `offset` once every earlier command is done.
    pub fn write(&self, list: &ID3D12GraphicsCommandList2, offset: u64, value: u32) {
        let p = D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: self.va + offset, Value: value };
        let mode = D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT;
        unsafe { list.WriteBufferImmediate(1, &p, Some(&mode)) };
    }
}
