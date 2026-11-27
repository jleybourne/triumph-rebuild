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
    if failed {
        std::process::exit(1);
    }
}
