//! The release key tool (`docs/UPDATES.md` 4.4). Runs on the owner's PC only.
//!
//! ```text
//! swapdesk-release-sign keygen <key-a.dpapi> <key-a-offline.pk8> <key-b-offline.pk8>
//! swapdesk-release-sign sign <key-a.dpapi> <version> <auto|manual> <out-folder> <target>=<package> ...
//! swapdesk-release-sign verify <swapdesk-update.txt> <swapdesk-update.txt.sig>
//! ```
//!
//! * `keygen` makes the signing key A and the rotation key B once. A is written under DPAPI in the user scope (only
//!   this Windows account on this PC opens it) and once in the clear for the offline copy; B only in the clear, for
//!   offline keeping. It refuses to overwrite any file, and prints only the two public halves, as the lines
//!   `swapdesk-update/src/keys.rs` takes. The files in the clear go to offline media and leave this PC.
//! * `sign` hashes the packages it is given from their bytes, writes `swapdesk-update.txt` and
//!   `swapdesk-update.txt.sig` into the folder, and checks the signature with A's public half first.
//! * `verify` checks a manifest against the keys compiled into this build.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("swapdesk-release-sign: {e}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage: swapdesk-release-sign keygen <key-a.dpapi> <key-a-offline.pk8> <key-b-offline.pk8>\n       \
                     swapdesk-release-sign sign <key-a.dpapi> <version> <auto|manual> <out-folder> <target>=<package> ...\n       \
                     swapdesk-release-sign verify <manifest> <signature>";

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("verify") => verify(&args[1..]),
        Some("keygen") => keys::keygen(&args[1..]),
        Some("sign") => keys::sign(&args[1..]),
        _ => Err(USAGE.into()),
    }
}

fn verify(args: &[String]) -> Result<(), String> {
    let [manifest, signature] = args else { return Err(USAGE.into()) };
    let m = std::fs::read(manifest).map_err(|e| format!("{manifest}: {e}"))?;
    let s = std::fs::read(signature).map_err(|e| format!("{signature}: {e}"))?;
    let read = swapdesk_update::manifest::verify(&m, &s).map_err(|e| e.to_string())?;
    println!("valid: version {}, {} packages", read.version, read.files.len());
    Ok(())
}

#[cfg(windows)]
mod keys {
    use std::path::{Path, PathBuf};

    use ring::rand::SystemRandom;
    use ring::signature::Ed25519KeyPair;
    use swapdesk_release_tools::{key_line, manifest_for, sign_release};
    use swapdesk_update::Apply;
    use swapdesk_update::manifest::public_key;

    use super::USAGE;

    /// Binds the protected key to its purpose: a DPAPI blob made for something else does not open as this.
    const ENTROPY: &[u8] = b"swapdesk-release-key-a";

    fn fresh() -> Result<Vec<u8>, String> {
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map(|d| d.as_ref().to_vec())
            .map_err(|_| "no key could be made".to_string())
    }

    fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("{}: {e} (keygen never overwrites a file)", path.display()))?;
        f.write_all(bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn keygen(args: &[String]) -> Result<(), String> {
        let [a_path, a_offline, b_offline] = args else { return Err(USAGE.into()) };
        let paths = [Path::new(a_path), Path::new(a_offline), Path::new(b_offline)];
        if let Some(p) = paths.iter().find(|p| p.exists()) {
            return Err(format!("{} exists: keygen never overwrites a file", p.display()));
        }
        let (a, b) = (fresh()?, fresh()?);
        let blob = swapdesk_secret_win::protect_user(&a, ENTROPY).map_err(|e| e.to_string())?;
        write_new(paths[0], &blob)?;
        write_new(paths[1], &a)?;
        write_new(paths[2], &b)?;
        let (pa, pb) = (public_key(&a).map_err(|e| e.to_string())?, public_key(&b).map_err(|e| e.to_string())?);
        println!("The two public keys, for crates/swapdesk-update/src/keys.rs (RELEASE_KEYS):");
        println!("{}", key_line(&pa, "A, the signing key"));
        println!("{}", key_line(&pb, "B, the rotation key"));
        println!("Move {} and {} to offline media and delete them from this PC.", a_offline, b_offline);
        Ok(())
    }

    pub fn sign(args: &[String]) -> Result<(), String> {
        let [key, version, apply, out, files @ ..] = args else { return Err(USAGE.into()) };
        if files.is_empty() {
            return Err(USAGE.into());
        }
        let apply = match apply.as_str() {
            "auto" => Apply::Auto,
            "manual" => Apply::Manual,
            _ => return Err(USAGE.into()),
        };
        let mut packages = Vec::new();
        for f in files {
            let (target, path) = f.split_once('=').ok_or_else(|| USAGE.to_string())?;
            packages.push((target.to_string(), PathBuf::from(path)));
        }
        let blob = std::fs::read(key).map_err(|e| format!("{key}: {e}"))?;
        let pkcs8 = swapdesk_secret_win::unprotect_user(&blob, ENTROPY).map_err(|e| e.to_string())?;
        let manifest = manifest_for(version, apply, &packages).map_err(|e| e.to_string())?;
        let (text, sig) = sign_release(&manifest, &pkcs8).map_err(|e| e.to_string())?;
        let out = Path::new(out);
        std::fs::write(out.join("swapdesk-update.txt"), &text).map_err(|e| format!("{}: {e}", out.display()))?;
        std::fs::write(out.join("swapdesk-update.txt.sig"), &sig).map_err(|e| format!("{}: {e}", out.display()))?;
        print!("{text}");
        println!("signed and checked: {}", out.join("swapdesk-update.txt.sig").display());
        Ok(())
    }
}

#[cfg(not(windows))]
mod keys {
    pub fn keygen(_: &[String]) -> Result<(), String> {
        Err("the release key lives on the owner's Windows PC (UPDATES.md 4.4)".into())
    }
    pub fn sign(_: &[String]) -> Result<(), String> {
        keygen(&[])
    }
}
