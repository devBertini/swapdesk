// No dependencies: PROBE_NONCE rebuild tracking and the optional __CGPreLoginApp section (probe P6, variant B).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROBE_NONCE");
    let macos = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|v| v == "macos");
    if macos && std::env::var_os("CARGO_FEATURE_PRELOGIN_SECTION").is_some() {
        // GUESS (community claim, not Apple): an empty __CGPreLoginApp section lets a pre-login binary post events.
        println!("cargo:rustc-link-arg=-Wl,-sectcreate,__CGPreLoginApp,__cgpreloginapp,/dev/null");
    }
}
