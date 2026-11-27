//! Prints the G4 table, `(interface, slot) -> function`, as CSV (names only).
//! `cargo run --release -p rebuild-gdk --bin gdkslots > agents/gdk-slots.csv`

fn main() {
    println!("area,provider,iids,offset,api,args");
    for r in rebuild_gdk::table::rows() {
        let quoted: Vec<String> = r.iter().map(|f| format!("\"{}\"", f.replace('"', "'"))).collect();
        println!("{}", quoted.join(","));
    }
}
