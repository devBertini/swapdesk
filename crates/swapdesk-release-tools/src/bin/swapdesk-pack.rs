//! `swapdesk-pack <folder> <out.sdpkg>`: every file under the folder into an update package (`docs/UPDATES.md` 3).
//! `swapdesk-pack --list <file.sdpkg>`: reads a package back and lists its entries.

use std::path::Path;
use std::process::ExitCode;

use swapdesk_release_tools::pack;
use swapdesk_update::Package;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [flag, file] if flag == "--list" => list(Path::new(file)),
        [folder, out] => make(Path::new(folder), Path::new(out)),
        _ => Err("usage: swapdesk-pack <folder> <out.sdpkg> | swapdesk-pack --list <file.sdpkg>".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("swapdesk-pack: {e}");
            ExitCode::FAILURE
        }
    }
}

fn make(folder: &Path, out: &Path) -> Result<(), String> {
    let package = pack(folder).map_err(|e| e.to_string())?;
    let bytes = package.write().map_err(|e| e.to_string())?;
    std::fs::write(out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{}: {} files, {} bytes", out.display(), package.entries.len(), bytes.len());
    Ok(())
}

fn list(file: &Path) -> Result<(), String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let package = Package::read(&bytes).map_err(|e| e.to_string())?;
    for e in &package.entries {
        println!("{:o} {:>10} {}", e.mode, e.bytes.len(), e.path);
    }
    Ok(())
}
