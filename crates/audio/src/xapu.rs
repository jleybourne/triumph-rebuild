//! A3: `xapu.dll`, the console's Opus decoder: `XApuConnect`, `XApuEnqueueCommand`,
//! `XApuDequeueResult`, `XApuGetResultCount`, `XApuDisconnect`.
//!
//! The title connects once (256 streams, two commands per stream, 1,064,960 bytes of memory: fork run
//! log), puts packets and output buffers in that memory, and queues per-stream commands. Each command
//! gives one result. Here commands go to a worker thread of the connection, which decodes Opus through
//! `opus-pure`; results are dequeued in command order, and `XAPU_PENDING` means "not yet", which the
//! title polls for. Enqueueing never decodes on the title's thread.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use opus_pure::OpusDecoder;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::heap::{self, ApuHeap};
use crate::{lock, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, HRESULT, S_OK};

/// Codes as the fork observed them.
pub const XAPU_QUEUE_FULL: HRESULT = 0x8AC8_1000_u32 as i32;
pub const XAPU_PENDING: HRESULT = 0x8AC8_1001_u32 as i32;
pub const XAPU_INVALID_COMMAND: HRESULT = 0x8AC8_1002_u32 as i32;
pub const XAPU_INVALID_STREAM: HRESULT = 0x8AC8_1003_u32 as i32;
pub const XAPU_ALREADY_ACTIVE: HRESULT = 0x8AC8_1005_u32 as i32;
pub const XAPU_BAD_PACKET: HRESULT = 0x8AC8_1008_u32 as i32;

/// `ConnectInput::process`: plain Opus decoding (the title's). Others (rate conversion) are refused.
pub const PROCESS_OPUS: u32 = 1;
/// Command kinds.
pub const ACTIVATE: u32 = 1;
pub const DEACTIVATE: u32 = 2;
pub const PROCESS: u32 = 3;
pub const NOTIFY: u32 = 4;
pub const RESET: u32 = 5;
/// `CommandId::options`: signal the connection's event when the result is ready.
pub const SIGNAL: u32 = 1;
/// `ConnectInput::options`: planar stereo output; create a result event. (The title passes 1, whose
/// meaning is not known.)
pub const DEINTERLEAVED: u32 = 4;
pub const RESULT_EVENT: u32 = 8;
/// 120 ms at 48 kHz, the longest Opus packet.
const MAX_FRAMES: usize = 5760;

/// 24 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ConnectInput {
    pub process: u32,
    pub streams: u32,
    pub commands_per_stream: u32,
    pub memory_bytes: u32,
    pub frames_per_output: u32,
    pub options: u32,
}

/// 16 bytes: the connection's memory (packets and output live there) and its result event.
#[repr(C)]
pub struct ConnectOutput {
    pub memory: *mut c_void,
    pub result_event: HANDLE,
}

/// 24 bytes, at the start of every command and every result.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct CommandId {
    pub kind: u32,
    pub stream: u32,
    pub sequence: u32,
    pub options: u32,
    pub context: u64,
}

/// `ACTIVATE`, 56 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Activate {
    pub id: CommandId,
    pub channels: u32,
    pub init_length: u32,
    pub init: *const c_void,
    pub start_pitch: f32,
    pub processing_length: u32,
    pub processing: *mut c_void,
}

/// `PROCESS`, 64 bytes: one packet in, float PCM out.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Process {
    pub id: CommandId,
    pub pitch_ramp: f32,
    pub target_pitch: f32,
    pub first_frame: u32,
    pub frame_count: u32,
    pub input_length: u32,
    pub input: *const u8,
    pub output_length: u32,
    pub output: *mut u8,
}

/// One result, 80 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct ApuResult {
    pub id: CommandId,
    pub state: u32,
    pub state_data: u64,
    /// Microseconds the command took.
    pub usec: u64,
    pub error: HRESULT,
    pub frames: u32,
    pub output_length: u32,
    pub output: *mut u8,
    pub pitch: f32,
    pub input_length: u32,
    pub input: *const u8,
}

// SAFETY: title addresses carried back, not dereferenced by the result's holder.
unsafe impl Send for ApuResult {}

enum Job {
    Activate(CommandId, u32),
    Process(Process),
    Other(CommandId),
}

// SAFETY: the pointers in a `Process` are the title's buffers, which it leaves alone until the result.
unsafe impl Send for Job {}

/// The title's side of the queue. Only title threads take this lock; the worker never does.
struct Order {
    enqueued: u32,
    jobs: Option<Sender<(Job, Instant)>>,
}

pub struct Connection {
    heap: Arc<ApuHeap>,
    memory: *mut u8,
    event: HANDLE,
    per_stream: u32,
    /// Per stream: commands queued and not yet dequeued.
    outstanding: Box<[AtomicU32]>,
    order: Mutex<Order>,
    /// Finished results, oldest first. The worker holds this lock only to push one result; the title
    /// only tries it, so it never waits on the worker.
    ready: Arc<Mutex<VecDeque<ApuResult>>>,
    ready_count: Arc<AtomicU32>,
    dequeued: AtomicU32,
    worker: Option<JoinHandle<()>>,
    /// Nanoseconds from enqueue to result, summed, for the measured latency.
    pub latency_ns: Arc<AtomicU64>,
}

impl Connection {
    pub fn connect(heap: Arc<ApuHeap>, input: &ConnectInput) -> Result<(Box<Connection>, ConnectOutput), HRESULT> {
        if !(1..=500).contains(&input.streams)
            || !(1..=16).contains(&input.commands_per_stream)
            || input.memory_bytes == 0
            || input.frames_per_output != 0
        {
            return Err(E_INVALIDARG);
        }
        if input.process != PROCESS_OPUS {
            return Err(E_NOTIMPL);
        }
        let (memory, _) = heap.alloc(input.memory_bytes as usize, 256)?;
        let event = if input.options & RESULT_EVENT != 0 {
            // SAFETY: an unnamed auto-reset event.
            unsafe { CreateEventW(None, false, false, None) }.map_err(|_| E_OUTOFMEMORY)?
        } else {
            HANDLE::default()
        };
        let streams = input.streams as usize;
        let capacity = streams * input.commands_per_stream as usize;
        let ready = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let ready_count = Arc::new(AtomicU32::new(0));
        let (jobs, rx) = channel::<(Job, Instant)>();
        let latency_ns = Arc::new(AtomicU64::new(0));
        let (q, n, lat, deinterleave, ev) = (
            ready.clone(),
            ready_count.clone(),
            latency_ns.clone(),
            input.options & DEINTERLEAVED != 0,
            event.0 as usize,
        );
        let worker = std::thread::Builder::new()
            .name("rebuild Opus".into())
            .spawn(move || {
                crate::raise_priority();
                let mut decoders: Vec<Option<OpusDecoder>> = (0..streams).map(|_| None).collect();
                let mut pcm = vec![0f32; MAX_FRAMES * 2];
                for (job, queued) in rx {
                    let started = Instant::now();
                    let mut r = run(job, &mut decoders, &mut pcm, deinterleave);
                    r.usec = started.elapsed().as_micros() as u64;
                    let signal = r.id.options & SIGNAL != 0;
                    lock(&q).push_back(r);
                    n.fetch_add(1, Ordering::Release);
                    lat.fetch_add(queued.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    if signal && ev != 0 {
                        // SAFETY: the connection's event, closed only after this thread ends.
                        let _ = unsafe { SetEvent(HANDLE(ev as *mut c_void)) };
                    }
                }
            })
            .map_err(|_| E_OUTOFMEMORY)?;
        let c = Box::new(Connection {
            heap,
            memory,
            event,
            per_stream: input.commands_per_stream,
            outstanding: (0..streams).map(|_| AtomicU32::new(0)).collect(),
            order: Mutex::new(Order { enqueued: 0, jobs: Some(jobs) }),
            ready,
            ready_count,
            dequeued: AtomicU32::new(0),
            worker: Some(worker),
            latency_ns,
        });
        let out = ConnectOutput {
            memory: memory.cast(),
            result_event: event,
        };
        Ok((c, out))
    }

    /// Queues one command; returns how many have ever been queued.
    ///
    /// # Safety
    /// `command` points to a whole command of its kind.
    pub unsafe fn enqueue(&self, command: *const CommandId) -> Result<u32, HRESULT> {
        let id = command.read_unaligned();
        let job = match id.kind {
            ACTIVATE => Job::Activate(id, command.cast::<Activate>().read_unaligned().channels),
            PROCESS => Job::Process(command.cast::<Process>().read_unaligned()),
            DEACTIVATE | NOTIFY | RESET => Job::Other(id),
            _ => return Err(XAPU_INVALID_COMMAND),
        };
        let Some(slot) = self.outstanding.get(id.stream as usize) else {
            return Err(XAPU_INVALID_STREAM);
        };
        let mut o = lock(&self.order);
        let held = slot.load(Ordering::Acquire);
        if held >= self.per_stream {
            return Err(XAPU_QUEUE_FULL);
        }
        slot.fetch_add(1, Ordering::AcqRel);
        o.enqueued = o.enqueued.wrapping_add(1);
        // Sent under the title's lock, so the worker sees commands in the order they were counted.
        if let Some(jobs) = &o.jobs {
            let _ = jobs.send((job, Instant::now()));
        }
        Ok(o.enqueued)
    }

    /// The oldest result, if it is ready (and the worker is not pushing one this instant).
    pub fn dequeue(&self) -> Option<ApuResult> {
        let mut q = match self.ready.try_lock() {
            Ok(q) => q,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let r = q.pop_front()?;
        drop(q);
        self.ready_count.fetch_sub(1, Ordering::AcqRel);
        self.outstanding[r.id.stream as usize].fetch_sub(1, Ordering::AcqRel);
        self.dequeued.fetch_add(1, Ordering::AcqRel);
        Some(r)
    }

    /// Results ready, and results ever dequeued.
    pub fn counts(&self) -> (u32, u32) {
        (self.ready_count.load(Ordering::Acquire), self.dequeued.load(Ordering::Acquire))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Closing the queue ends the worker once queued work is done.
        lock(&self.order).jobs.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if !self.event.is_invalid() && !self.event.0.is_null() {
            // SAFETY: our event.
            let _ = unsafe { CloseHandle(self.event) };
        }
        self.heap.free(self.memory);
    }
}

fn run(job: Job, decoders: &mut [Option<OpusDecoder>], pcm: &mut [f32], deinterleave: bool) -> ApuResult {
    let id = match &job {
        Job::Activate(id, _) | Job::Other(id) => *id,
        Job::Process(p) => p.id,
    };
    let mut r = ApuResult {
        id,
        state: 0,
        state_data: 0,
        usec: 0,
        error: S_OK,
        frames: 0,
        output_length: 0,
        output: std::ptr::null_mut(),
        pitch: 1.0,
        input_length: 0,
        input: std::ptr::null(),
    };
    let slot = &mut decoders[id.stream as usize];
    match job {
        Job::Activate(_, channels) => {
            r.error = match (slot.is_some(), channels) {
                (true, _) => XAPU_ALREADY_ACTIVE,
                (false, 1 | 2) => match OpusDecoder::new(48_000, channels as usize) {
                    Ok(d) => {
                        *slot = Some(d);
                        S_OK
                    }
                    Err(_) => E_INVALIDARG,
                },
                _ => E_INVALIDARG,
            }
        }
        Job::Other(id) => match id.kind {
            DEACTIVATE => *slot = None,
            RESET => {
                if let Some(d) = slot {
                    let _ = d.reset_state();
                }
            }
            _ => {}
        },
        Job::Process(p) => {
            r.input = p.input;
            r.input_length = p.input_length;
            r.output = p.output;
            let Some(d) = slot else {
                r.error = XAPU_INVALID_STREAM;
                return r;
            };
            if p.input.is_null() || p.output.is_null() || p.input_length == 0 {
                r.error = E_INVALIDARG;
                return r;
            }
            let ch = d.channels();
            // SAFETY: the title's packet.
            let packet = unsafe { std::slice::from_raw_parts(p.input, p.input_length as usize) };
            let Ok(decoded) = d.decode(packet, MAX_FRAMES, &mut pcm[..MAX_FRAMES * ch]) else {
                r.error = XAPU_BAD_PACKET;
                return r;
            };
            let first = (p.first_frame as usize).min(decoded);
            let frames = (decoded - first)
                .min(p.frame_count as usize)
                .min(p.output_length as usize / (4 * ch));
            let kept = &pcm[first * ch..(first + frames) * ch];
            // SAFETY: the title's output buffer, `output_length` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(p.output.cast::<f32>(), frames * ch) };
            if deinterleave && ch == 2 {
                for (i, f) in kept.chunks_exact(2).enumerate() {
                    out[i] = f[0];
                    out[frames + i] = f[1];
                }
            } else {
                out.copy_from_slice(kept);
            }
            r.frames = frames as u32;
            r.output_length = (frames * ch * 4) as u32;
        }
    }
    r
}

/// `HRESULT XApuConnect(const ConnectInput* input, ConnectOutput* output, XApuHandle* handle)`
///
/// # Safety
/// Pointers are null or valid for their types.
pub unsafe extern "system" fn XApuConnect(
    input: *const ConnectInput,
    output: *mut ConnectOutput,
    handle: *mut *mut Connection,
) -> HRESULT {
    if input.is_null() || output.is_null() || handle.is_null() {
        return E_INVALIDARG;
    }
    match Connection::connect(heap::global().clone(), &*input) {
        Ok((c, out)) => {
            output.write(out);
            handle.write(Box::into_raw(c));
            S_OK
        }
        Err(e) => e,
    }
}

/// `HRESULT XApuDisconnect(XApuHandle handle)`: waits for queued commands, frees the memory.
///
/// # Safety
/// A handle from `XApuConnect`, disconnected once.
pub unsafe extern "system" fn XApuDisconnect(handle: *mut Connection) -> HRESULT {
    if handle.is_null() {
        return E_INVALIDARG;
    }
    drop(Box::from_raw(handle));
    S_OK
}

/// `HRESULT XApuEnqueueCommand(XApuHandle handle, const CommandId* command, UINT32* totalEnqueued /*optional*/)`:
/// `command` is an `Activate`, a `Process`, or a bare `CommandId` (deactivate, notify, reset).
///
/// # Safety
/// A live handle; `command` a whole command; `total` null or writable.
pub unsafe extern "system" fn XApuEnqueueCommand(
    handle: *mut Connection,
    command: *const CommandId,
    total: *mut u32,
) -> HRESULT {
    if handle.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    match (*handle).enqueue(command) {
        Ok(n) => {
            if !total.is_null() {
                total.write(n);
            }
            S_OK
        }
        Err(e) => e,
    }
}

/// `HRESULT XApuDequeueResult(XApuHandle handle, ApuResult* result)`: `XAPU_PENDING` while the oldest
/// command is not done.
///
/// # Safety
/// A live handle; `out` writable.
pub unsafe extern "system" fn XApuDequeueResult(handle: *mut Connection, out: *mut ApuResult) -> HRESULT {
    if handle.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    match (*handle).dequeue() {
        Some(r) => {
            out.write_unaligned(r);
            S_OK
        }
        None => XAPU_PENDING,
    }
}

/// `UINT32 XApuGetResultCount(XApuHandle handle, UINT32* totalDequeued /*optional*/)`: results ready.
///
/// # Safety
/// A live handle; `total` null or writable.
pub unsafe extern "system" fn XApuGetResultCount(handle: *mut Connection, total: *mut u32) -> u32 {
    if handle.is_null() {
        return 0;
    }
    let (ready, dequeued) = (*handle).counts();
    if !total.is_null() {
        total.write(dequeued);
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, OpusEncoder};

    pub(crate) fn id(kind: u32, stream: u32, sequence: u32) -> CommandId {
        CommandId {
            kind,
            stream,
            sequence,
            options: SIGNAL,
            context: 0xC0DE_0000 + u64::from(sequence),
        }
    }

    fn wait(c: &Connection) -> ApuResult {
        let t = Instant::now();
        loop {
            if let Some(r) = c.dequeue() {
                return r;
            }
            assert!(t.elapsed().as_secs() < 5, "result never came");
            std::thread::yield_now();
        }
    }

    pub(crate) fn tone_packets(channels: usize, count: usize) -> Vec<Vec<u8>> {
        let mut enc = OpusEncoder::new(48_000, channels, Application::Audio).unwrap();
        (0..count)
            .map(|p| {
                let pcm: Vec<f32> = (0..960 * channels)
                    .map(|i| {
                        let t = (p * 960 + i / channels) as f32 / 48_000.0;
                        (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
                    })
                    .collect();
                let mut out = [0u8; 1500];
                let n = enc.encode(&pcm, 960, &mut out).unwrap();
                out[..n].to_vec()
            })
            .collect()
    }

    #[test]
    fn struct_layouts() {
        use std::mem::size_of;
        assert_eq!(
            [size_of::<ConnectInput>(), size_of::<ConnectOutput>(), size_of::<CommandId>()],
            [24, 16, 24]
        );
        assert_eq!([size_of::<Activate>(), size_of::<Process>(), size_of::<ApuResult>()], [56, 64, 80]);
    }

    #[test]
    fn a3_opus_decodes_on_the_worker_and_results_come_in_order() {
        let heap = Arc::new(ApuHeap::reserve(16 << 20).unwrap());
        let input = ConnectInput {
            process: PROCESS_OPUS,
            streams: 256,
            commands_per_stream: 2,
            memory_bytes: 1_064_960,
            frames_per_output: 0,
            options: 1 | RESULT_EVENT,
        };
        let (c, out) = Connection::connect(heap.clone(), &input).unwrap();
        assert!(!out.result_event.0.is_null());
        let base = out.memory.cast::<u8>();
        // Four streams, alternating mono and stereo, ten 20 ms packets each.
        let mut seq = 0;
        for s in 0..4u32 {
            let a = Activate {
                id: id(ACTIVATE, s, seq),
                channels: 1 + s % 2,
                init_length: 0,
                init: std::ptr::null(),
                start_pitch: 1.0,
                processing_length: 0,
                processing: std::ptr::null_mut(),
            };
            seq += 1;
            // SAFETY: a whole command.
            unsafe { c.enqueue((&raw const a).cast()) }.unwrap();
        }
        for _ in 0..4 {
            wait(&c);
        }
        let packets: Vec<Vec<Vec<u8>>> = (0..4).map(|s| tone_packets(1 + s % 2, 10)).collect();
        let mut order = Vec::new();
        for p in 0..10 {
            for s in 0..4u32 {
                let at = (s as usize * 0x4_0000) + p * 0x6000;
                let pk = &packets[s as usize][p];
                // SAFETY: inside the connection's memory.
                unsafe { std::ptr::copy_nonoverlapping(pk.as_ptr(), base.add(at), pk.len()) };
                let cmd = Process {
                    id: id(PROCESS, s, seq),
                    pitch_ramp: 0.0,
                    target_pitch: 1.0,
                    first_frame: 0,
                    frame_count: 960,
                    input_length: pk.len() as u32,
                    // SAFETY: inside the connection's memory.
                    input: unsafe { base.add(at) },
                    output_length: 960 * 8,
                    output: unsafe { base.add(at + 0x1000) },
                };
                order.push(seq);
                seq += 1;
                // SAFETY: a whole command.
                unsafe { c.enqueue((&raw const cmd).cast()) }.unwrap();
            }
            for _ in 0..4 {
                let r = wait(&c);
                let (sq, err, frames) = (r.id.sequence, r.error, r.frames);
                assert_eq!((sq, err, frames), (order.remove(0), S_OK, 960));
                let ch = 1 + r.id.stream as usize % 2;
                // SAFETY: the decoder wrote there.
                let pcm = unsafe { std::slice::from_raw_parts(r.output.cast::<f32>(), 960 * ch) };
                let energy = pcm.iter().map(|v| v * v).sum::<f32>() / pcm.len() as f32;
                if p > 0 {
                    assert!(energy > 0.05, "a 0.5 sine has energy 0.125, got {energy}");
                }
            }
        }
        // Per-stream limit: two outstanding.
        let bare = id(NOTIFY, 9, 0);
        // SAFETY: a whole command.
        unsafe {
            c.enqueue(&bare).unwrap();
            c.enqueue(&bare).unwrap();
            assert_eq!(c.enqueue(&bare), Err(XAPU_QUEUE_FULL));
            assert_eq!(c.enqueue(&id(NOTIFY, 256, 0)), Err(XAPU_INVALID_STREAM));
            assert_eq!(c.enqueue(&id(99, 0, 0)), Err(XAPU_INVALID_COMMAND));
        }
        wait(&c);
        wait(&c);
        assert!(c.dequeue().is_none());
        drop(c);
        assert!(heap.is_empty(), "disconnect frees the memory");
    }
}
