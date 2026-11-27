fn print(r: Result<Vec<String>, String>) {
    match r {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
        }
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    if has("--bare") {
        match rebuild_uma::windows::prove_bare_dispatch() {
            Ok(msg) => println!("RESULT    {msg}"),
            Err(e) => { println!("FAILED    {e}"); std::process::exit(1); }
        }
        return;
    }
    if has("--tiled") {
        return print(rebuild_uma::tileproof::prove_tiles());
    }
    if has("--ring") {
        let n = |f: &str, d: usize| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d);
        return print(rebuild_uma::tileproof::prove_ring(n("--frames", 60), n("--workers", 4)));
    }
    if has("--labels") {
        return print(rebuild_uma::tileproof::prove_labels(200));
    }
    if has("--arena") {
        return print(rebuild_uma::tileproof::prove_arena());
    }
    if let Some(i) = args.iter().position(|a| a == "--scale") {
        let gb = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(10);
        return print(rebuild_uma::tileproof::prove_scale(gb));
    }
    let vram = has("--vram") || std::env::var_os("REMUX_VRAM").is_some();
    let proof = if vram {
        rebuild_uma::windows::prove_on_vram()
    } else {
        rebuild_uma::windows::prove_shared_memory()
    };
    match proof {
        Ok(report) => {
            println!("backend   {}", report.backend);
            println!("shader wrote 0x{:08X} and 42", report.want);
            println!("cpu read     {:02X?}", &report.read[..12]);
            if report.zero_copy {
                println!("RESULT    the GPU wrote into memory the CPU reads, with no copy");
            } else {
                println!("RESULT    the bytes are not what the shader wrote");
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    }
}
