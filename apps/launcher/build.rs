//! Windows version info for the launcher's executable: "Triumph Launcher" in Explorer and Task Manager.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/launcher.rc");
        let _ = embed_resource::compile_for("assets/launcher.rc", ["rebuild-launcher"], embed_resource::NONE);
    }
}
