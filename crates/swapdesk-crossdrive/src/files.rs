//! The files proof across machines (F-XFILE-01, F-FILE-03): through a live session, a file each way and a cancel.
//!
//! The viewer side sends and fetches through its UI backend (`FilesSend`, `FilesGet`, `TransferCancel`) exactly as
//! the files panel does; the orchestrator checks and prepares the host machine's disk with that OS's tools, and they
//! meet through files in the run folder:
//!
//! 1. To the host: this side writes a 2 MiB file named `relatório-<nonce>.bin` (precomposed, as Windows writes it)
//!    and sends it to the host's receive folder, where the orchestrator first put a different file whose name is the
//!    same in decomposed form (as macOS writes it). When the transfer is done it writes `files-sent` with
//!    `<name>\t<sha256>`; the orchestrator finds the received copy under a new name (`relatório-<nonce> (2).bin`, the
//!    first one untouched) and writes its hash into `files-host-got`.
//! 2. From the host: the orchestrator writes `files-host-file` with `<path relative to the host's home>\t<sha256>`;
//!    this side fetches it into `<dir>/recv` and compares the hash.
//! 3. Cancel: this side sends a 64 MiB file and cancels it at its first progress; the transfer must end failed, and
//!    the orchestrator checks that no file of that name was left on the host (`files-cancel-left` holds `none`).
//!
//! Markers: `FILES_TO_HOST_OK`, `FILES_FROM_HOST_OK`, `FILES_CANCEL_OK`. Resuming after a dropped link is the
//! reconnect proof's (`--reconnect`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use swapdesk_ui::{Command, TransferState, UiEvent};

use crate::pump::{Pump, wait_file};

const REPLY: Duration = Duration::from_secs(10);

/// The name of the file sent in step 1 (precomposed `ó`).
pub fn sent_name(nonce: u128) -> String {
    format!("relat\u{f3}rio-{nonce}.bin")
}

/// Bytes that a compressor cannot shrink and that differ per file: an xorshift stream from `seed`.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The SHA-256 of `data`, as lowercase hex (what `shasum -a 256` and `sha256sum` print).
pub fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Waits for the transfer of `name` in the direction `outgoing` to end; `Ok(Done)` or the failure.
fn wait_end(pump: &mut Pump<'_>, name: &str, outgoing: bool, timeout: Duration) -> Result<(u32, bool), String> {
    pump.wait(&format!("the transfer of {name} to end"), timeout, |e| match e {
        UiEvent::Transfer { transfer_id, name: n, outgoing: o, state: TransferState::Done, .. }
            if n == name && *o == outgoing =>
        {
            Some((*transfer_id, true))
        }
        UiEvent::Transfer { transfer_id, name: n, outgoing: o, state: TransferState::Failed { .. }, .. }
            if n == name && *o == outgoing =>
        {
            Some((*transfer_id, false))
        }
        _ => None,
    })
}

/// How long a cancelled transfer is watched for an end it must not reach.
const CANCEL_QUIET: Duration = Duration::from_secs(3);

/// Runs the three steps; `Err` names the first that failed.
pub fn files_round(
    pump: &mut Pump<'_>,
    session_id: u64,
    dir: &Path,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let send_dir = dir.join("send");
    let recv_dir = dir.join("recv");
    std::fs::create_dir_all(&send_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&recv_dir).map_err(|e| e.to_string())?;

    // 1. A file to the host, whose receive folder holds the same name in decomposed form.
    let name = sent_name(nonce);
    let data = noise(2 << 20, nonce as u64);
    let sha = sha256_hex(&data);
    let path = send_dir.join(&name);
    std::fs::write(&path, &data).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("files-name"), &name).map_err(|e| e.to_string())?;
    wait_file(&dir.join("files-host-ready"), "the host's receive folder to be prepared", timeout)?;
    pump.ask(Command::FilesSend { session_id, local_paths: vec![path], remote_dir: None }, REPLY)?;
    let (_, done) = wait_end(pump, &name, true, timeout)?;
    if !done {
        return Err(format!("sending {name} failed"));
    }
    std::fs::write(dir.join("files-sent"), format!("{name}\t{sha}")).map_err(|e| e.to_string())?;
    let got = wait_file(&dir.join("files-host-got"), "the host's hash of the received file", timeout)?;
    if got != sha {
        return Err(format!("the host holds {got:?} under the new name, not {sha}"));
    }
    log("FILES_TO_HOST_OK");

    // 2. A file from the host.
    let offered = wait_file(&dir.join("files-host-file"), "the host's file to fetch", timeout)?;
    let (remote_path, want) = offered.split_once('\t').ok_or("files-host-file is not <path>\\t<sha>")?;
    let remote_name = remote_path.rsplit('/').next().unwrap_or(remote_path).to_string();
    pump.ask(
        Command::FilesGet { session_id, remote_path: remote_path.to_string(), local_dir: recv_dir.clone() },
        REPLY,
    )?;
    let (_, done) = wait_end(pump, &remote_name, false, timeout)?;
    if !done {
        return Err(format!("fetching {remote_path} failed"));
    }
    let local = std::fs::read(recv_dir.join(&remote_name)).map_err(|e| format!("read the fetched file: {e}"))?;
    if sha256_hex(&local) != want.trim() {
        return Err(format!("the fetched {remote_name} differs from the host's ({} bytes)", local.len()));
    }
    log("FILES_FROM_HOST_OK");

    // 3. A large file, cancelled at its first progress.
    let big = format!("cancel-{nonce}.bin");
    let big_path: PathBuf = send_dir.join(&big);
    std::fs::write(&big_path, noise(64 << 20, nonce as u64 ^ 0x5a5a)).map_err(|e| e.to_string())?;
    pump.ask(Command::FilesSend { session_id, local_paths: vec![big_path], remote_dir: None }, REPLY)?;
    let transfer_id = pump.wait("the large transfer to start", timeout, |e| match e {
        UiEvent::Transfer { transfer_id, name, outgoing: true, state: TransferState::Running { .. }, .. }
            if *name == big =>
        {
            Some(*transfer_id)
        }
        _ => None,
    })?;
    pump.ask(Command::TransferCancel { transfer_id }, REPLY)?;
    // The person's own cancel ends the transfer with no event (the interface drops its row at the click); what must
    // not come is its end as done.
    let ended = pump.wait("a cancelled transfer to end anyway", CANCEL_QUIET, |e| match e {
        UiEvent::Transfer { transfer_id: t, state: TransferState::Done, .. } if *t == transfer_id => Some(()),
        _ => None,
    });
    if ended.is_ok() {
        return Err(format!("{big} finished although it was cancelled"));
    }
    std::fs::write(dir.join("files-cancelled"), &big).map_err(|e| e.to_string())?;
    let left = wait_file(&dir.join("files-cancel-left"), "the host's check after the cancel", timeout)?;
    if left != "none" {
        return Err(format!("the cancelled transfer left {left:?} on the host"));
    }
    log("FILES_CANCEL_OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_the_one_shasum_prints_and_the_noise_differs_per_seed() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_ne!(noise(64, 1), noise(64, 2));
        assert_eq!(noise(64, 3), noise(64, 3));
        assert!(sent_name(7).contains('\u{f3}'), "precomposed, as Windows writes it");
    }
}
