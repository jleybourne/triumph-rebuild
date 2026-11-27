fn main() {
    if std::env::args().any(|a| a == "--bare") {
        match remux_uma::windows::prove_bare_dispatch() {
            Ok(msg) => println!("RESULT    {msg}"),
            Err(e) => { println!("FAILED    {e}"); std::process::exit(1); }
        }
        return;
    }
    let vram = std::env::args().any(|a| a == "--vram") || std::env::var_os("REMUX_VRAM").is_some();
    let proof = if vram {
        remux_uma::windows::prove_on_vram()
    } else {
        remux_uma::windows::prove_shared_memory()
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
