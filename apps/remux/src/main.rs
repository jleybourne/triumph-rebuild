fn main() {
    let windows = 4;
    let uma = remux_uma::Uma::new(windows, 3 << 30);
    println!(
        "remux: {} windows of {} MiB, {} MiB of card memory, {} GiB of addresses",
        windows,
        remux_uma::WINDOW >> 20,
        3 << 10,
        uma.bytes() >> 30
    );
}
