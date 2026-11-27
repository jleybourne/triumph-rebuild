//! A4's second half: no call the title makes blocks for 1 ms. Hand-run (timings need a quiet
//! machine; check `agents\MEASURING` first):
//! `cargo test --release -p rebuild-audio -- --ignored a4_title_calls --nocapture --test-threads 1`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::acp::{self, Hal, ENABLE_XMA, ADVANCE_XMA_READ, REGISTER_MESSAGES, COMMAND_COMPLETED};
use crate::heap::ApuHeap;
use crate::xapu::{self, Activate, CommandId, Connection, ConnectInput, Process};
use crate::xdsp;
use crate::xma::{self, PACKET_BYTES};

struct Timing {
    name: &'static str,
    samples: Vec<Duration>,
}

impl Timing {
    fn new(name: &'static str) -> Timing {
        Timing { name, samples: Vec::new() }
    }

    fn time<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let r = f();
        self.samples.push(t.elapsed());
        r
    }

    fn report(mut self) -> Duration {
        self.samples.sort();
        let n = self.samples.len();
        let at = |q: f64| self.samples[((n as f64 * q) as usize).min(n - 1)].as_secs_f64() * 1e6;
        let max = self.samples[n - 1];
        eprintln!(
            "{:<28} {:>7} calls  median {:>7.2} us  p99 {:>7.2} us  max {:>8.2} us",
            self.name,
            n,
            at(0.5),
            at(0.99),
            max.as_secs_f64() * 1e6
        );
        max
    }
}

fn id(kind: u32, stream: u32, sequence: u32) -> CommandId {
    CommandId { kind, stream, sequence, options: 0, context: 0 }
}

#[test]
#[ignore]
fn a4_title_calls_never_block_1ms() {
    let heap = Arc::new(ApuHeap::reserve(1 << 30).unwrap());
    let mut worst = Vec::new();

    // Apu*: small allocations and frees, and the address maps.
    let (mut alloc, mut free, mut map) = (Timing::new("ApuAlloc 256 B"), Timing::new("ApuFree 256 B"), Timing::new("ApuMap* round trip"));
    for _ in 0..10_000 {
        let (p, d) = alloc.time(|| heap.alloc(256, 16).unwrap());
        map.time(|| assert_eq!(heap.host(heap.device(p)), p));
        let _ = d;
        free.time(|| heap.free(p));
    }
    let mut big = Timing::new("ApuAlloc 401 MB (start-up)");
    let (p, _) = big.time(|| heap.alloc(401 << 20, 4096).unwrap());
    heap.free(p);
    worst.extend([alloc.report(), free.report(), map.report()]);
    big.report();

    // XApu: enqueue and dequeue on the title's side while the worker decodes Opus.
    let input = ConnectInput { process: xapu::PROCESS_OPUS, streams: 256, commands_per_stream: 2, memory_bytes: 1_064_960, frames_per_output: 0, options: 1 };
    let (c, out) = Connection::connect(heap.clone(), &input).unwrap();
    let base = out.memory.cast::<u8>();
    let a = Activate { id: id(xapu::ACTIVATE, 0, 0), channels: 2, init_length: 0, init: std::ptr::null(), start_pitch: 1.0, processing_length: 0, processing: std::ptr::null_mut() };
    // SAFETY: whole commands.
    unsafe { c.enqueue((&raw const a).cast()).unwrap() };
    while c.dequeue().is_none() {}
    let packets = opus_packets(200);
    let (mut enq, mut deq, mut decode) = (Timing::new("XApuEnqueueCommand"), Timing::new("XApuDequeueResult"), Timing::new("  (worker) Opus 20 ms stereo"));
    let started = Instant::now();
    for (i, pk) in packets.iter().enumerate() {
        // SAFETY: inside the connection's memory.
        unsafe { std::ptr::copy_nonoverlapping(pk.as_ptr(), base, pk.len()) };
        let cmd = Process { id: id(xapu::PROCESS, 0, i as u32), pitch_ramp: 0.0, target_pitch: 1.0, first_frame: 0, frame_count: 960, input_length: pk.len() as u32, input: base, output_length: 960 * 8, output: unsafe { base.add(0x1000) } };
        // SAFETY: a whole command.
        enq.time(|| unsafe { c.enqueue((&raw const cmd).cast()) }.unwrap());
        let r = loop {
            if let Some(r) = deq.time(|| c.dequeue()) {
                break r;
            }
            std::thread::yield_now();
        };
        decode.samples.push(Duration::from_micros(r.usec));
    }
    let lat = c.latency_ns.load(std::sync::atomic::Ordering::Relaxed) as f64 / 201.0 / 1e3;
    eprintln!("  enqueue -> result ready, mean {lat:.1} us over 201 commands ({:.0} ms for 200 packets)", started.elapsed().as_secs_f64() * 1e3);
    worst.extend([enq.report(), deq.report()]);
    decode.report();
    drop(c);

    // AcpHal: the coprocessor's submit, with XMA voices decoding (silence decoder) on their thread.
    let hal: &'static Hal = Box::leak(Box::new(Hal::new(heap.clone())));
    let (arrays, _) = hal.allocate_shape([0, 0, 0, 0, 64, 0]).unwrap();
    let frames: Vec<(usize, u16)> = (0..200).map(|i| (3000 + i % 7 * 500, i as u16)).collect();
    let stream = xma::synthetic_stream(&frames);
    let (input, input_dev) = heap.alloc(stream.len(), PACKET_BYTES).unwrap();
    // SAFETY: ours.
    unsafe { std::ptr::copy_nonoverlapping(stream.as_ptr(), input, stream.len()) };
    let acp = hal.create();
    // SAFETY: the object starts with its table.
    let m: &acp::Methods = unsafe { *(acp as *const &acp::Methods) };
    let mut submit = Timing::new("AcpHal SubmitCommand");
    // SAFETY: as title code calls them.
    unsafe {
        (m.connect)(acp, 256, 256);
        (m.submit)(acp, REGISTER_MESSAGES, 0, 0, [COMMAND_COMPLETED].as_ptr().cast(), 0);
        for v in 0..64u32 {
            let (_, ring_dev) = heap.alloc(16 * 256, 256).unwrap();
            let w = std::slice::from_raw_parts_mut(arrays[4].cast::<u32>().add(v as usize * 16), 16);
            w[0] = (stream.len() / PACKET_BYTES) as u32 | 1 << 20 | 16 << 22 | 255 << 12;
            w[1] = 3 << 27 | 1 << 29 | 1 << 31;
            w[3] = 0;
            w[4] = 0;
            w[5] = input_dev;
            w[7] = ring_dev;
            submit.time(|| (m.submit)(acp, ENABLE_XMA, 1, 0, [v].as_ptr().cast(), 0));
        }
        let mut msg = std::mem::zeroed::<acp::Message>();
        for i in 0..20_000u32 {
            let v = i % 64;
            submit.time(|| (m.submit)(acp, ADVANCE_XMA_READ, u64::from(i) + 2, 0, [v, 4].as_ptr().cast(), 0));
            while (m.pop_message)(acp, &mut msg) == 1 {}
            if i % 64 == 0 {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
        (m.release)(acp);
    }
    let x = hal.xma().unwrap();
    let (fr, bad, blocks) = x.counts();
    eprintln!("  (XMA thread) {fr} frames walked, {bad} refused, {blocks} blocks written for 64 stereo voices");
    worst.push(submit.report());
    drop(x);
    hal.release_shape();

    // XDsp: one block per command.
    let mut dsp_base = vec![0u8; 393_216];
    let mut submit = Timing::new("XDspSubmitCommand 1024");
    // SAFETY: as title code calls them.
    unsafe {
        let mut dsp = std::ptr::null_mut();
        xdsp::XDspConnectWithMaximumStreamLimit(dsp_base.as_mut_ptr().cast(), 393_216, 30, 128, 1, &mut dsp);
        let desc = xdsp::StreamDesc { kind: xdsp::CONVOLUTION, options: 0, frames: 1024, channels: 1, ir_floats: 0, ir: std::ptr::null() };
        let (mut status, mut s) = (std::ptr::null(), std::ptr::null_mut());
        xdsp::XDspActivate(dsp, &desc, &mut status, &mut s);
        let mut output = vec![0f32; 1024];
        for _ in 0..10_000 {
            let cmd = xdsp::Command { gains: [1.0; 2], input: output.as_ptr(), output: output.as_mut_ptr(), reserved: 0 };
            submit.time(|| xdsp::XDspSubmitCommand(s, &cmd, std::ptr::null_mut()));
        }
        xdsp::XDspDeactivate(s);
        xdsp::XDspDisconnect(dsp);
    }
    worst.push(submit.report());

    let max = worst.iter().max().unwrap();
    eprintln!("worst title-side call: {:.1} us", max.as_secs_f64() * 1e6);
    assert!(*max < Duration::from_millis(1));
}

fn opus_packets(count: usize) -> Vec<Vec<u8>> {
    use opus_pure::{Application, OpusEncoder};
    let mut enc = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
    (0..count)
        .map(|p| {
            let pcm: Vec<f32> = (0..1920)
                .map(|i| (((p * 960 + i / 2) as f32) * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
                .collect();
            let mut out = [0u8; 1500];
            let n = enc.encode(&pcm, 960, &mut out).unwrap();
            out[..n].to_vec()
        })
        .collect()
}
