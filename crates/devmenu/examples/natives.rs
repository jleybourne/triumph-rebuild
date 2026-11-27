//! Lists every native handler in an image copy: `natives <image.dll>` prints `<va> <hash>` per line.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: natives <image.dll>");
        std::process::exit(2);
    };
    let data = std::fs::read(&path).expect("read the image copy");
    let pe = rebuild_devmenu::pe::Pe::parse(&data, false).expect("a PE32+ image");
    for n in rebuild_devmenu::native::scan(&pe) {
        println!("{:x} {:016x}", pe.image_base + n.rva as u64, n.hash);
    }
}
