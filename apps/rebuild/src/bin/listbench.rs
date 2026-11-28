//! Build 0 (b): per-draw translation cost, on our own code.
//!
//! cargo run --release --bin listbench [-- --frames N]
//!
//! A synthetic frame in the title's shape (~4,500 draws, ~20,300 list calls, 48 lists, 1,433
//! pipelines, 200,000 descriptor slots) is translated (a) into a counting sink — our cost alone —
//! and (b) into real D3D12 command lists — our cost plus the runtime and driver — on 1..6 threads,
//! then submitted and run, so a broken list shows as a device removal. Workers are a persistent
//! pool released by a barrier: no thread is created inside a timed frame.

use rebuild_xlist::d3d::{run_frames, Lists, Setup};
use rebuild_xlist::frame::{frame, layout, Shape};

fn arg(name: &str, default: usize) -> usize {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let frames = arg("--frames", 30);
    let shape = Shape::default();
    let lay = layout(&shape);
    let (mut setup, ctx) = match Setup::new(&lay, shape.descriptor_slots as u32) {
        Ok(s) => s,
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    };
    let lists = Lists::new(&setup, shape.lists).expect("lists");
    // several different frames, so no run is a replay of the same bytes
    let frames_data: Vec<_> = (0..4).map(|s| frame(&shape, &lay, 100 + s)).collect();
    let calls: usize = frames_data[0].iter().map(|l| l.len()).sum();

    println!("listbench — Build 0 (b): per-draw translation, our own code");
    println!(
        "frame     {} draws · {} list calls · {} lists · {} pipelines (built in {:.0} ms at start) · {} descriptor slots",
        shape.draws, calls, shape.lists, shape.pipelines, setup.build_ms, shape.descriptor_slots
    );
    println!("budget    7.4 µs a draw / 1.6 µs a call if serial (33.3 ms / 4,500 draws)");
    println!("          {frames} timed frames after 3 warm-up, medians");
    println!();
    println!("{:<28} {:>2} | {:>8} {:>9} | {:>9} {:>9} | {:>7} {:>7}", "sink", "T", "wall ms", "thread ms", "µs/draw", "µs/call", "elided", "refused");

    let mut failed = false;
    for record in [false, true] {
        for threads in [1usize, 2, 4, 6] {
            let times = match run_frames(&mut setup, &lists, &ctx, &frames_data, frames + 3, threads, record) {
                Ok(t) => t,
                Err(e) => {
                    println!("FAILED    {e}");
                    std::process::exit(1);
                }
            };
            let mut wall: Vec<f64> = times[3..].iter().map(|t| t.wall_ms).collect();
            let mut thread: Vec<f64> = times[3..].iter().map(|t| t.thread_ms).collect();
            let last = times.last().map(|t| t.stats);
            let st = last.unwrap();
            if st.refused > 0 {
                failed = true;
            }
            let (w, t) = (median(&mut wall), median(&mut thread));
            println!(
                "{:<28} {:>2} | {:>8.2} {:>9.2} | {:>9.3} {:>9.3} | {:>7} {:>7}",
                if record { "D3D12 lists (ours + driver)" } else { "counting (ours alone)" },
                threads,
                w,
                t,
                t * 1000.0 / st.draws as f64,
                t * 1000.0 / st.calls as f64,
                st.elided,
                st.refused
            );
        }
    }

    // decision 2: the descriptors the title wrote this frame, from a dirty list
    let dirty: Vec<u32> = (0..12_027u64).map(|i| ((i.wrapping_mul(0x9E37_79B9) >> 7) % shape.descriptor_slots) as u32).collect();
    let mut d: Vec<f64> = (0..frames + 3).map(|_| setup.write_descriptors(&dirty)).skip(3).collect();
    let dm = median(&mut d);
    println!();
    println!(
        "descriptors  12,027 written a frame from a dirty list (the old layer scanned 203,127): {:.2} ms = {:.3} µs each",
        dm,
        dm * 1000.0 / 12_027.0
    );
    println!();
    println!("columns   wall = first list started -> last list closed; thread ms = summed over workers");
    println!("          µs/draw and µs/call are thread time per draw/call; every recorded frame was executed");

    // Build 6: the title's path. The same frames through the console list objects (crates/xlist/src/console),
    // translated as each call arrives (`call`) or kept as records and translated at Close (`close`), timed inside
    // the list functions as the frame line's `xlate` is, with and without a stand-in for the title's own work
    // between its calls. The two modes alternate round by round, so both see the same machine.
    println!();
    println!("console lists — Build 6: translated as each call arrives (call) vs at Close (close), 1 thread");
    let console = match rebuild_xlist::console::bench::ConsoleLists::new(&setup, &lay, shape.lists) {
        Ok(c) => c,
        Err(e) => {
            println!("FAILED    console lists: {e}");
            std::process::exit(1);
        }
    };
    let rounds = arg("--console-rounds", 40);
    let tpu = rebuild_xlist::console::cost::ticks_per_us();
    let work: [(&str, usize, usize); 3] = [("none (calls back to back)", 0, 0), ("light: 32 KiB code, 64 KiB data", 32, 64), ("heavy: 64 KiB code, 512 KiB data", 64, 512)];
    println!(
        "{:<34} | {:>8} {:>8} {:>8} | {:>8} {:>8} {:>8} | {:>8} | {:>8}",
        "title work between calls", "call ms", "µs/draw", "best", "close ms", "µs/draw", "best", "close", "append"
    );
    let mut buffers: Vec<Vec<rebuild_xlist::Cmd>> = (0..shape.lists).map(|_| Vec::with_capacity(1024)).collect();
    let mut calls_seen = 0u32;
    for (name, code_kb, data_kb) in work {
        let mut pollute = rebuild_xlist::console::bench::Pollute::new(code_kb, data_kb);
        let mut between = || {
            if code_kb + data_kb > 0 {
                pollute.run()
            }
        };
        let (mut call, mut close, mut append) = (Vec::new(), Vec::new(), Vec::new());
        let (mut draws, mut calls) = (0u32, 0u32);
        for r in 0..rounds + 3 {
            let f = &frames_data[r % frames_data.len()];
            for at_close in [false, true] {
                rebuild_xlist::console::mode::set(at_close);
                let (t, d, c, x) = match console.frame(&mut setup, f, &lay, &mut between) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("FAILED    console frame: {e}");
                        std::process::exit(1);
                    }
                };
                if x > 0 {
                    failed = true;
                }
                if r >= 3 {
                    if at_close {
                        close.push(t as f64)
                    } else {
                        call.push(t as f64)
                    }
                }
                (draws, calls) = (d, c);
            }
            let a = rebuild_xlist::console::bench::record_cost(f, &mut buffers, &mut between);
            if r >= 3 {
                append.push(a as f64);
            }
        }
        let best = |v: &[f64]| v.iter().cloned().fold(f64::MAX, f64::min) / tpu;
        let (ab, bb) = (best(&call), best(&close));
        let (a, b, ap) = (median(&mut call) / tpu, median(&mut close) / tpu, median(&mut append) / tpu);
        println!(
            "{:<34} | {:>8.2} {:>8.3} {:>8.3} | {:>8.2} {:>8.3} {:>8.3} | {:>+6.0} % | {:>8.3}",
            name,
            a / 1000.0,
            a / draws as f64,
            ab / draws as f64,
            b / 1000.0,
            b / draws as f64,
            bb / draws as f64,
            100.0 * (b / a - 1.0),
            ap / draws as f64
        );
        calls_seen = calls;
    }
    println!("          ticks inside the list functions (ours + D3D12 + driver), {rounds} rounds after 3 warm-up: medians, and the best");
    println!("          round; every frame executed. close = close against call (medians); append = keeping the frame's calls as bare");
    println!("          records alone, µs a draw. {} calls a frame", calls_seen);
    drop(console);
    if failed {
        std::process::exit(1);
    }
}
