//! Idle cost of the mDNS daemon (SPEC B6): starts discovery on this PC's approved interfaces with the standard mDNS
//! port, announces, browses and then does nothing for the given number of seconds (default 30). Read the process CPU
//! counters:
//!
//! `cargo run --release --example idle -p swapdesk-lan -- 30`
//!
//! After a 5 s settling pause it reads the process CPU time (`GetProcessTimes`, all threads), holds still for the given
//! seconds, reads it again and prints the difference as a share of one core and of all cores.

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use std::time::Duration;

    use swapdesk_lan::discovery::{Config, Discovery};
    use swapdesk_lan::txt::Txt;
    use swapdesk_net_win::{WindowsNetInterfaces, process_cpu_time};
    use swapdesk_proto::Os;

    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let own = Txt { name: "idle-probe".into(), os: Os::Windows, fingerprint: [0x1D; 16], pairing: false };
    let d = Discovery::start(Arc::new(WindowsNetInterfaces), own, Config::default()).expect("start discovery");
    println!("pid {} applied {:?}", std::process::id(), d.applied().iter().map(|a| a.addr).collect::<Vec<_>>());
    // Let the probing and the first announcements finish, then hold still.
    std::thread::sleep(Duration::from_secs(5));
    let (cpu0, t0) = (process_cpu_time(), std::time::Instant::now());
    println!(
        "CPU time since process start (start-up, probing, first announcements): {:.1} ms",
        cpu0.as_secs_f64() * 1000.0
    );
    std::thread::sleep(Duration::from_secs(secs));
    let (cpu1, wall) = (process_cpu_time(), t0.elapsed());
    let cpu = cpu1.saturating_sub(cpu0);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let one_core = cpu.as_secs_f64() / wall.as_secs_f64() * 100.0;
    println!(
        "idle over {:.1} s: {:.1} ms CPU = {:.4}% of one core = {:.5}% of all {} cores",
        wall.as_secs_f64(),
        cpu.as_secs_f64() * 1000.0,
        one_core,
        one_core / cores as f64,
        cores
    );
    d.shutdown();
}

#[cfg(not(windows))]
fn main() {
    println!("the idle measurement runs on the Windows PC");
}
