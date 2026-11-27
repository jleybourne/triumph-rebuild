fn main() {
    let windows = 4;
    let uma = rebuild_uma::Uma::new(windows, 3 << 30);
    println!(
        "rebuild: {} windows of {} MiB, {} MiB of card memory, {} GiB of addresses",
        windows,
        rebuild_uma::WINDOW >> 20,
        3 << 10,
        uma.bytes() >> 30
    );
}
