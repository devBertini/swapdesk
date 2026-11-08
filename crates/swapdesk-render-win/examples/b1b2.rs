//! Stage 15 measurement: SyntheticScreen -> encoder -> decoder -> renderer in a window of its own, at 1080p60, reading the
//! barcode back from the presented frame (B1 latency, B2 fps and loss) and sampling the viewer process's memory.
//!
//! `cargo run --release -p swapdesk-render-win --example b1b2 -- [--seconds 600] [--shell min|high] [--render shell|min|high]
//!  [--child-encoder] [--display N]`
//!
//! `--shell`: the adapter of the stand-in shell's device (`min` = what `Gfx::new` picks). `--render`: `shell` = the renderer
//! and decoder share the shell's device (zero copy); `min`/`high` = a device of its own on that adapter (the swap chain then
//! composes across adapters when it differs from the shell's). `--child-encoder`: the encoder runs in a child process of
//! this executable, so the memory numbers are the viewer's alone.
#![cfg_attr(not(windows), allow(unused))]

#[cfg(windows)]
#[path = "../tests/common/harness.rs"]
mod harness;

#[cfg(windows)]
fn main() {
    use harness::*;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--encode-child") {
        let p = |i: usize| args[i].parse::<f64>().expect("number");
        let s = EncodeSettings {
            width: p(1) as u32,
            height: p(2) as u32,
            fps: p(3) as u16,
            kbps: p(4) as u32,
            seconds: p(5),
            adapter: args[6].parse().expect("luid"),
            pool: p(7) as usize,
        };
        if let Err(e) = encode_child_main(&s) {
            eprintln!("encode child: {e}");
            std::process::exit(1);
        }
        return;
    }
    let get = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let mut cfg = Config::b1b2(get("--seconds").map_or(60.0, |s| s.parse().expect("seconds")));
    cfg.shell = if get("--shell").as_deref() == Some("high") { Pref::High } else { Pref::Min };
    cfg.render_on = match get("--render").as_deref() {
        Some("min") => RenderOn::Adapter(Pref::Min),
        Some("high") => RenderOn::Adapter(Pref::High),
        _ => RenderOn::Shell,
    };
    cfg.buffers = get("--buffers").map_or(2, |b| b.parse().expect("buffers"));
    cfg.display = get("--display").map(|d| d.parse().expect("display number"));
    cfg.encoder_child = args.iter().any(|a| a == "--child-encoder");
    let exe = cfg.encoder_child.then(|| std::env::current_exe().expect("current exe"));
    println!("config: {cfg:?}");
    match run(&cfg, exe.as_deref()) {
        Ok(r) => print!("{}", r.text()),
        Err(e) => {
            eprintln!("run failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn main() {}
