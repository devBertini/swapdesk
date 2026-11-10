//! B7 and B8: the private working set of an idle program 10 s after its start and the CPU time it uses over the next
//! 30 s (the method of the UI comparison the budgets come from). The program is started with the given arguments and
//! ended when the reading is done.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::proc;
use crate::stats::Fields;

pub struct IdleConfig {
    pub exe: PathBuf,
    pub args: Vec<String>,
    /// Seconds after the start before the memory is read.
    pub settle: u64,
    /// Seconds the CPU time is read over.
    pub seconds: u64,
}

pub fn run(cfg: &IdleConfig) -> Result<Fields, String> {
    let mut child =
        Command::new(&cfg.exe).args(&cfg.args).spawn().map_err(|e| format!("start {}: {e}", cfg.exe.display()))?;
    let pid = child.id();
    let mut read = || {
        std::thread::sleep(Duration::from_secs(cfg.settle));
        // A program that already ended still answers with zeros through its handle.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the program ended by itself ({status}) before the memory was read"));
        }
        let memory = proc::private_working_set_mb(pid).ok_or("the program ended before the memory was read")?;
        let cpu0 = proc::cpu_seconds(pid).ok_or("the program ended before the CPU was read")?;
        std::thread::sleep(Duration::from_secs(cfg.seconds));
        let cpu1 = proc::cpu_seconds(pid).ok_or("the program ended during the CPU reading")?;
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the program ended by itself ({status}) during the CPU reading"));
        }
        Ok::<_, String>((memory, (cpu1 - cpu0) * 1000.0))
    };
    let read = read();
    let _ = child.kill();
    let _ = child.wait();
    let (memory, cpu_ms) = read?;
    let mut f = Fields::new();
    f.text("config", format!("idle, {} s after the start, CPU over {} s", cfg.settle, cfg.seconds));
    f.num("b7_private_mb", memory);
    f.num("b8_cpu_ms", cpu_ms);
    f.num("b8_seconds", cfg.seconds as f64);
    f.text("valid", "yes");
    Ok(f)
}
