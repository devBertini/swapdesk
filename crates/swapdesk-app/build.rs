//! Embeds the application manifest in `Swapdesk.exe`: per-monitor v2 DPI, long paths, common controls v6 (the native
//! inputs' placeholder text) and the supported systems. No resource compiler or extra crate: the linker does it.

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets the manifest dir");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}/app.manifest");
    }
}
