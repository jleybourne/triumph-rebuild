//! `acphal.dll`: the SHAPE context arrays (`AcpHalAllocateShapeContexts`, `AcpHalReleaseShapeContexts`)
//! and the audio coprocessor object (`AcpHalCreate`).
//!
//! The coprocessor takes commands and answers with messages. Here every command is carried out when
//! it is submitted (XMA commands go to `xma::Engine`, whose thread does the decoding), a command with an
//! id is acknowledged by a "command completed" message, and its notification word is set. The title's
//! middleware waits inside its start-up for the first acknowledgement, so it must come.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use crate::heap::{self, ApuHeap};
use crate::xma::{self, DecoderFactory, Engine};
use crate::{lock, BOOL, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, HRESULT, S_OK};

/// Context kinds in `ShapeContexts` order.
pub const KINDS: [&str; 6] = ["rate conversion", "EQ/compressor", "filter/volume", "DMA", "XMA", "PCM"];
/// Bytes per context. XMA (64), rate conversion (32) and DMA (16) are the sizes the title's
/// middleware clears (fork's notes); the other three are not seen in use and get 256.
pub const CONTEXT_BYTES: [usize; 6] = [32, 256, 256, 16, xma::CONTEXT_BYTES, 256];

/// `AcpHalAllocateShapeContexts`'s in/out block: counts in, arrays and their device addresses out.
#[repr(C)]
pub struct ShapeContexts {
    pub counts: [u32; 6],
    pub arrays: [*mut c_void; 6],
    pub device: [u32; 6],
}

struct Shape {
    arrays: [usize; 6],
    xma: Option<Arc<Engine>>,
}

/// The state behind the `acphal.dll` functions: the heap, the context arrays, and the XMA decoder.
pub struct Hal {
    heap: Arc<ApuHeap>,
    shape: RwLock<Option<Shape>>,
    factory: Mutex<DecoderFactory>,
}

impl Hal {
    pub fn new(heap: Arc<ApuHeap>) -> Hal {
        Hal {
            heap,
            shape: RwLock::new(None),
            factory: Mutex::new(xma::silence()),
        }
    }

    pub fn heap(&self) -> &Arc<ApuHeap> {
        &self.heap
    }

    /// The frame decoder for XMA voices enabled from now on (the default writes silence).
    pub fn set_xma_decoder(&self, factory: DecoderFactory) {
        *lock(&self.factory) = factory;
    }

    pub fn xma(&self) -> Option<Arc<Engine>> {
        self.shape.read().unwrap_or_else(|e| e.into_inner()).as_ref()?.xma.clone()
    }

    /// Allocates each non-empty array from the heap, zeroed, 256-byte aligned.
    pub fn allocate_shape(&self, counts: [u32; 6]) -> Result<([*mut u8; 6], [u32; 6]), HRESULT> {
        let mut shape = self.shape.write().unwrap_or_else(|e| e.into_inner());
        if shape.is_some() {
            return Err(E_FAIL);
        }
        let mut arrays = [std::ptr::null_mut(); 6];
        let mut device = [0u32; 6];
        for i in 0..6 {
            if counts[i] == 0 {
                continue;
            }
            match self.heap.alloc(counts[i] as usize * CONTEXT_BYTES[i], 256) {
                Ok((p, d)) => (arrays[i], device[i]) = (p, d),
                Err(_) => {
                    arrays.iter().filter(|p| !p.is_null()).for_each(|&p| {
                        self.heap.free(p);
                    });
                    return Err(E_OUTOFMEMORY);
                }
            }
        }
        let xma = (counts[4] > 0).then(|| {
            Arc::new(Engine::new(
                self.heap.clone(),
                arrays[4],
                counts[4] as usize,
                lock(&self.factory).clone(),
            ))
        });
        *shape = Some(Shape {
            arrays: arrays.map(|p| p as usize),
            xma,
        });
        Ok((arrays, device))
    }

    pub fn release_shape(&self) -> HRESULT {
        let Some(shape) = self.shape.write().unwrap_or_else(|e| e.into_inner()).take() else {
            return E_FAIL;
        };
        // The decoder thread stops before the arrays it reads are freed.
        drop(shape.xma);
        for p in shape.arrays.into_iter().filter(|&p| p != 0) {
            self.heap.free(p as *const u8);
        }
        S_OK
    }

    /// A coprocessor object, as `AcpHalCreate` returns it.
    pub fn create(&'static self) -> *mut Acp {
        Box::into_raw(Box::new(Acp {
            vtable: &METHODS,
            hal: self,
            state: Mutex::new(State {
                connected: false,
                mask: 0,
                messages: VecDeque::new(),
                capacity: 0,
                dropped: 0,
                since: Instant::now(),
            }),
        }))
    }
}

static HAL: OnceLock<Hal> = OnceLock::new();

/// The state behind the console functions, over `heap::global()`.
pub fn global() -> &'static Hal {
    HAL.get_or_init(|| Hal::new(heap::global().clone()))
}

/// `HRESULT AcpHalAllocateShapeContexts(ShapeContexts* contexts)`: six counts in (rate conversion,
/// EQ/compressor, filter/volume, DMA, XMA, PCM); six host arrays and six device addresses out. Once
/// per process until released.
///
/// # Safety
/// `contexts` is null or a readable and writable `ShapeContexts`.
pub unsafe extern "system" fn AcpHalAllocateShapeContexts(contexts: *mut ShapeContexts) -> HRESULT {
    if contexts.is_null() {
        return E_INVALIDARG;
    }
    let counts = std::ptr::addr_of!((*contexts).counts).read_unaligned();
    match global().allocate_shape(counts) {
        Ok((arrays, device)) => {
            std::ptr::addr_of_mut!((*contexts).arrays).write_unaligned(arrays.map(|p| p.cast()));
            std::ptr::addr_of_mut!((*contexts).device).write_unaligned(device);
            S_OK
        }
        Err(e) => e,
    }
}

/// `HRESULT AcpHalReleaseShapeContexts(void)`
pub extern "system" fn AcpHalReleaseShapeContexts() -> HRESULT {
    global().release_shape()
}

/// `HRESULT AcpHalCreate(IAcpHal** out)`: the coprocessor object. Not COM: its table has six methods
/// in this order (slots 0-5): `Connect`, `Disconnect`, `SubmitCommand`, `PopMessage`,
/// `GetMessageCount`, `Release`.
///
/// # Safety
/// `out` is null or writable.
pub unsafe extern "system" fn AcpHalCreate(out: *mut *mut Acp) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    out.write(global().create());
    S_OK
}

// The coprocessor object.

/// Commands.
pub const LOAD_FLOWGRAPH: u32 = 1;
pub const REGISTER_MESSAGES: u32 = 2;
pub const UNREGISTER_MESSAGES: u32 = 3;
pub const ENABLE_XMA: u32 = 5;
pub const ENABLE_XMAS: u32 = 6;
pub const DISABLE_XMA: u32 = 7;
pub const DISABLE_XMAS: u32 = 8;
pub const ADVANCE_XMA_READ: u32 = 18;

/// Message kinds (also the registration mask bits).
pub const FLOWGRAPH_COMPLETED: u32 = 0x02;
pub const COMMAND_COMPLETED: u32 = 0x10;
pub const DISCONNECTED: u32 = 0x80;

/// The coprocessor's clock: audio frames of 128 samples at 48 kHz.
const FRAMES_PER_SECOND: u128 = 375;

/// One message, 28 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct Message {
    pub kind: u32,
    /// Messages lost to a full queue just before this one.
    pub dropped: u32,
    /// Microseconds since the object was made.
    pub usec: u32,
    /// `COMMAND_COMPLETED`: command (u32), id (u64), audio frame (u32). `FLOWGRAPH_COMPLETED`: the
    /// flowgraph's word.
    pub data: [u8; 16],
}

#[repr(C)]
pub struct Acp {
    vtable: &'static Methods,
    hal: &'static Hal,
    state: Mutex<State>,
}

struct State {
    connected: bool,
    mask: u32,
    messages: VecDeque<Message>,
    capacity: usize,
    dropped: u32,
    since: Instant,
}

impl State {
    fn post(&mut self, kind: u32, data: &[u8]) {
        if self.mask & kind == 0 {
            return;
        }
        if self.messages.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let mut m = Message {
            kind,
            dropped: std::mem::take(&mut self.dropped),
            usec: self.since.elapsed().as_micros() as u32,
            data: [0; 16],
        };
        m.data[..data.len()].copy_from_slice(data);
        self.messages.push_back(m);
    }
}

/// The object's method table (slots 0-5).
#[repr(C)]
pub struct Methods {
    pub connect: unsafe extern "system" fn(*mut Acp, u32, u32) -> HRESULT,
    pub disconnect: unsafe extern "system" fn(*mut Acp) -> HRESULT,
    pub submit: unsafe extern "system" fn(*mut Acp, u32, u64, u32, *const c_void, u32) -> HRESULT,
    pub pop_message: unsafe extern "system" fn(*mut Acp, *mut Message) -> BOOL,
    pub message_count: unsafe extern "system" fn(*mut Acp) -> u32,
    pub release: unsafe extern "system" fn(*mut Acp),
}

static METHODS: Methods = Methods {
    connect,
    disconnect,
    submit,
    pop_message,
    message_count,
    release,
};

/// Slot 0: `HRESULT Connect(this, UINT32 commandQueueEntries, UINT32 messageQueueEntries)`.
unsafe extern "system" fn connect(this: *mut Acp, commands: u32, messages: u32) -> HRESULT {
    if commands == 0 {
        return E_INVALIDARG;
    }
    let mut s = lock(&(*this).state);
    s.connected = true;
    s.capacity = messages as usize;
    S_OK
}

/// Slot 1: `HRESULT Disconnect(this)`: posts "disconnected" if registered.
unsafe extern "system" fn disconnect(this: *mut Acp) -> HRESULT {
    let mut s = lock(&(*this).state);
    if !s.connected {
        return E_FAIL;
    }
    s.post(DISCONNECTED, &[]);
    s.connected = false;
    S_OK
}

/// Slot 2: `HRESULT SubmitCommand(this, UINT32 command, UINT64 id, UINT32 frame, const void* data,
/// UINT32 notificationApuAddress)`. `data` starts with the command's u32 arguments: a mask for 2 and 3,
/// (_, flowgraph) for 1, a context index for 5 and 7, (index, blocks) for 18, bit masks for 6 and 8.
/// `frame` (when to run it) is ignored: everything runs now.
unsafe extern "system" fn submit(
    this: *mut Acp,
    command: u32,
    id: u64,
    _frame: u32,
    data: *const c_void,
    notification: u32,
) -> HRESULT {
    let acp = &*this;
    let word = |i: usize| -> u32 {
        if data.is_null() {
            0
        } else {
            data.cast::<u32>().add(i).read_unaligned()
        }
    };
    let mut s = lock(&acp.state);
    match command {
        REGISTER_MESSAGES => s.mask |= word(0),
        UNREGISTER_MESSAGES => s.mask &= !word(0),
        LOAD_FLOWGRAPH => s.post(FLOWGRAPH_COMPLETED, &word(1).to_le_bytes()),
        ENABLE_XMA | DISABLE_XMA | ENABLE_XMAS | DISABLE_XMAS | ADVANCE_XMA_READ => {
            if let Some(x) = acp.hal.xma() {
                match command {
                    ENABLE_XMA => x.enable(word(0)),
                    DISABLE_XMA => x.disable(word(0)),
                    ENABLE_XMAS => x.enable_mask(|i| word(i), true),
                    DISABLE_XMAS => x.enable_mask(|i| word(i), false),
                    _ => x.advance_read(word(0), word(1)),
                }
            }
        }
        // Other SHAPE units' commands are acknowledged and not carried out.
        _ => {}
    }
    if id != 0 {
        let frame = (s.since.elapsed().as_micros() * FRAMES_PER_SECOND / 1_000_000) as u32;
        let mut done = [0u8; 16];
        done[..4].copy_from_slice(&command.to_le_bytes());
        done[4..12].copy_from_slice(&id.to_le_bytes());
        done[12..].copy_from_slice(&frame.to_le_bytes());
        s.post(COMMAND_COMPLETED, &done);
    }
    if notification != 0 {
        if let Some(p) = acp.hal.heap.span(notification, 4) {
            (*p.cast::<std::sync::atomic::AtomicU32>()).store(1, std::sync::atomic::Ordering::Release);
        }
    }
    S_OK
}

/// Slot 3: `BOOL PopMessage(this, Message* out)`: false when there is none.
unsafe extern "system" fn pop_message(this: *mut Acp, out: *mut Message) -> BOOL {
    let Some(m) = lock(&(*this).state).messages.pop_front() else {
        return 0;
    };
    if !out.is_null() {
        out.write_unaligned(m);
    }
    1
}

/// Slot 4: `UINT32 GetMessageCount(this)`.
unsafe extern "system" fn message_count(this: *mut Acp) -> u32 {
    lock(&(*this).state).messages.len() as u32
}

/// Slot 5: `void Release(this)`: frees the object.
unsafe extern "system" fn release(this: *mut Acp) {
    drop(Box::from_raw(this));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xma::{synthetic_stream, TagDecoder, PACKET_BYTES};

    /// The object's methods, called through its table as title code does.
    fn methods(acp: *mut Acp) -> &'static Methods {
        // SAFETY: the object starts with its table.
        unsafe { *(acp as *const &'static Methods) }
    }

    fn hal() -> &'static Hal {
        Box::leak(Box::new(Hal::new(Arc::new(ApuHeap::reserve(64 << 20).unwrap()))))
    }

    #[test]
    fn struct_layouts() {
        assert_eq!(std::mem::size_of::<ShapeContexts>(), 96);
        assert_eq!(std::mem::size_of::<Message>(), 28);
        assert_eq!(std::mem::size_of::<Methods>(), 48);
    }

    #[test]
    fn a3_coprocessor_acknowledges_commands_in_order() {
        let hal = hal();
        let acp = hal.create();
        let m = methods(acp);
        let (note, note_dev) = hal.heap.alloc(4, 4).unwrap();
        // SAFETY: calls through the table on a live object, as title code makes them.
        unsafe {
            assert_eq!((m.connect)(acp, 0, 4), E_INVALIDARG);
            assert_eq!((m.connect)(acp, 64, 4), S_OK);
            let mask = [COMMAND_COMPLETED | FLOWGRAPH_COMPLETED | DISCONNECTED];
            assert_eq!((m.submit)(acp, REGISTER_MESSAGES, 1, 0, mask.as_ptr().cast(), note_dev), S_OK);
            assert_eq!(note.cast::<u32>().read(), 1, "notification word set");
            let flowgraph = [0u32, 0xF00D];
            (m.submit)(acp, LOAD_FLOWGRAPH, 0, 0, flowgraph.as_ptr().cast(), 0);
            for id in 2..=5u64 {
                (m.submit)(acp, 40 + id as u32, id, 0, std::ptr::null(), 0);
            }
            // Capacity 4: register-ack, flowgraph, ids 2 and 3; ids 4 and 5 dropped.
            assert_eq!((m.message_count)(acp), 4);
            let mut msg = std::mem::zeroed::<Message>();
            let mut kinds = Vec::new();
            while (m.pop_message)(acp, &mut msg) == 1 {
                let id = u64::from_le_bytes(msg.data[4..12].try_into().unwrap());
                kinds.push((msg.kind, id, msg.dropped));
            }
            assert_eq!(
                kinds,
                vec![(COMMAND_COMPLETED, 1, 0), (FLOWGRAPH_COMPLETED, 0, 0), (COMMAND_COMPLETED, 2, 0), (COMMAND_COMPLETED, 3, 0)]
            );
            // The next message reports the two lost ones.
            assert_eq!((m.disconnect)(acp), S_OK);
            assert_eq!((m.pop_message)(acp, &mut msg), 1);
            let (kind, dropped) = (msg.kind, msg.dropped);
            assert_eq!((kind, dropped), (DISCONNECTED, 2));
            assert_eq!((m.pop_message)(acp, &mut msg), 0);
            (m.release)(acp);
        }
    }

    /// Fills one XMA context the way the title does.
    fn context(words: &mut [u32], input: u32, packets: u32, output: u32, blocks: u32, stereo: bool) {
        words.fill(0);
        words[0] = packets | 0b01 << 20 | blocks << 22;
        words[1] = 3 << 27 | u32::from(stereo) << 29 | 1 << 31;
        words[5] = input;
        words[7] = output;
    }

    #[test]
    fn a3_xma_voice_fills_its_ring_in_order_and_ends() {
        let hal = hal();
        hal.set_xma_decoder(Arc::new(|ch, _| Box::new(TagDecoder(ch))));
        let (arrays, _) = hal.allocate_shape([0, 0, 0, 0, 4, 0]).unwrap();
        // 12 mono frames, tags 1..=12: 12 × 512 samples = 48 blocks.
        let frames: Vec<(usize, u16)> = (1..=12).map(|t| (4000 + t as usize * 77, t)).collect();
        let stream = synthetic_stream(&frames);
        let (input, input_dev) = hal.heap.alloc(stream.len(), PACKET_BYTES).unwrap();
        // SAFETY: ours, the right size.
        unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input, stream.len()) };
        let blocks = 8u32;
        let (ring, ring_dev) = hal.heap.alloc(blocks as usize * 256, 256).unwrap();
        // SAFETY: context 2 of the XMA array.
        let words = unsafe { std::slice::from_raw_parts_mut(arrays[4].cast::<u32>().add(2 * 16), 16) };
        context(words, input_dev, (stream.len() / PACKET_BYTES) as u32, ring_dev, blocks, false);
        let acp = hal.create();
        let m = methods(acp);
        let x = hal.xma().unwrap();
        let mut got: Vec<i16> = Vec::new();
        // SAFETY: as title code.
        unsafe {
            (m.connect)(acp, 64, 64);
            (m.submit)(acp, ENABLE_XMA, 0, 0, [2u32].as_ptr().cast(), 0);
            let mut read = 0u32;
            for _ in 0..200 {
                x.pump_now();
                let w = std::ptr::read_volatile(&words[0]);
                let write = w >> 27;
                let writable = std::ptr::read_volatile(&words[1]) >> 31;
                // Take what lies between read and write (all of it when the ring is full).
                let mut n = (write + blocks - read) % blocks;
                if n == 0 && writable == 0 {
                    n = blocks;
                }
                for b in 0..n {
                    let at = ring.add(((read + b) % blocks) as usize * 256).cast::<i16>();
                    got.extend(std::slice::from_raw_parts(at, 128));
                }
                read = (read + n) % blocks;
                (m.submit)(acp, ADVANCE_XMA_READ, 0, 0, [2u32, n].as_ptr().cast(), 0);
                if words[2] >> 31 == 1 && n == 0 {
                    break;
                }
            }
            (m.release)(acp);
        }
        let expect: Vec<i16> = (1..=12).flat_map(|t| std::iter::repeat_n(t as i16, 512)).collect();
        assert_eq!(got, expect, "every frame, in order, nothing extra");
        assert_eq!(words[2] >> 26 & 0x1f, xma::STATUS_INPUT_EXHAUSTED);
        assert_eq!(x.counts(), (12, 0, 48));
        drop(x);
        assert_eq!(hal.release_shape(), S_OK);
        assert_eq!(hal.release_shape(), E_FAIL);
    }

    #[test]
    fn a3_xma_loops_and_switches_inputs() {
        let hal = hal();
        hal.set_xma_decoder(Arc::new(|ch, _| Box::new(TagDecoder(ch))));
        let (arrays, _) = hal.allocate_shape([0, 0, 0, 0, 1, 0]).unwrap();
        let a = synthetic_stream(&[(5000, 1), (5000, 2), (5000, 3)]);
        let b = synthetic_stream(&[(5000, 7)]);
        let (ia, da) = hal.heap.alloc(a.len(), PACKET_BYTES).unwrap();
        let (ib, db) = hal.heap.alloc(b.len(), PACKET_BYTES).unwrap();
        // SAFETY: ours.
        unsafe {
            std::ptr::copy_nonoverlapping(a.as_ptr(), ia, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), ib, b.len());
        }
        let (ring, ring_dev) = hal.heap.alloc(31 * 256, 256).unwrap();
        // SAFETY: context 0.
        let words = unsafe { std::slice::from_raw_parts_mut(arrays[4].cast::<u32>(), 16) };
        context(words, da, 1, ring_dev, 31, false);
        // Both inputs valid; loop once over frame 2 (loop start and end both at frame 2), keeping 2
        // subframes of the pass that jumps back.
        let s = xma::Stream::new(&a);
        let f1 = s.frame(0).unwrap();
        let f2 = f1.next.unwrap();
        words[0] |= 0b11 << 20 | 1 << 12;
        words[1] |= 1 << 12 | (b.len() / PACKET_BYTES) as u32;
        words[3] = f2 as u32;
        words[4] = f2 as u32;
        words[6] = db;
        let x = hal.xma().unwrap();
        x.enable(0);
        x.pump_now();
        x.pump_now();
        let n = (words[0] >> 27) as usize;
        // SAFETY: the blocks written.
        let got = unsafe { std::slice::from_raw_parts(ring.cast::<i16>(), n * 128) };
        // Frame 1; frame 2 cut to 2 subframes (loop end); frame 2 again in full (loop count now 0);
        // frame 3; then input 1's frame 7.
        let mut expect = vec![1i16; 512];
        expect.extend([2; 256]);
        expect.extend([2; 512]);
        expect.extend([3; 512]);
        expect.extend([7; 512]);
        let len = expect.len().div_ceil(128) * 128;
        expect.resize(len, 0);
        // Compared as runs (value, length), which say what went wrong.
        let runs = |v: &[i16]| {
            v.chunk_by(|a, b| a == b).map(|r| (r[0], r.len())).collect::<Vec<_>>()
        };
        assert_eq!(runs(got), runs(&expect));
        assert_eq!(words[4] >> 31, 1, "on input 1");
        drop(x);
        hal.release_shape();
    }
}
