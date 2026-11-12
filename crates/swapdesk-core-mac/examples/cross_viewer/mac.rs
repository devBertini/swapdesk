//! Development tool of the stage 23 cross-machine proof (`scripts/mac/cross-session.sh`): a headless Mac viewer.
//!
//! Starts a `ServiceCore` and a `UiHub` in this process on the real LAN (the physical Wi-Fi/Ethernet interfaces of
//! `swapdesk-net-mac`, no dev loopback), the way the Mac app does (D27), and drives the hub through the same C6
//! `ServiceBackend` the shells use: finds the host by address, pairs (prints the 6-digit code), connects with control,
//! decodes the video with `swapdesk-codec-mac`'s `VtH264Decoder`, sends a few small pointer moves, counts the audio
//! datagrams, disconnects and forgets the host.
//!
//!     cross_viewer --host-addr <ipv4> --peer-code <file> --go <file> --anchor-file <file>
//!
//! After the first decoded frame it prints `VIEWER_NEED_ANCHOR` and waits for `--anchor-file`: where the host's pointer
//! is now ("x,y" in 0 to 65535 across the captured display, or `none`), read by the orchestrator from the host's OS.
//! The pointer moves stay within a 50 px box around it; with `none` none are sent.
//!
//! Secrets: a throwaway keychain file in a scratch folder (an SSH session cannot use the login keychain, -25308, and
//! the login keychain would be the owner's real Swapdesk identity). Keychain file, items and folder are removed at exit.
//! Markers go to stdout, the last line is `VIEWER_RESULT PASS` or `VIEWER_RESULT FAIL <why>`.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, VtH264Decoder};
use swapdesk_core_mac::{MacServiceParams, MacServiceSetup, mac_service_config, macos_version};
use swapdesk_crossdrive::{DecoderFactory, Media, ViewerConfig, viewer_drive};
use swapdesk_host::service::ServiceCore;
use swapdesk_host::ui_hub::{ConnInfo, HubParams, NoHooks, NoPresence, UiHub};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake};
use swapdesk_platform::{Clock, Level, MonoMicros, TraceSink, VideoDecoder};
use swapdesk_proto::Os;
use swapdesk_secret_mac::{
    KeychainLocation, MacSecretStore, SecretLocation, create_keychain_file, delete_keychain_file,
};
use swapdesk_session::{ConsentRequest, HostUi, PromptAnswer, PromptRequest};
use swapdesk_trust::{MemoryAudit, SystemWallClock, UserRef};
use swapdesk_ui::UiWaker;
use swapdesk_ui::service_backend::ServiceBackend;
use tokio::sync::oneshot;

static N: AtomicU32 = AtomicU32::new(0);

// ---------- in-memory duplex pipe between the hub's `serve` and the backend ----------

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
    pos: usize,
}

struct PipeWriter {
    tx: Sender<Vec<u8>>,
}

struct End {
    reader: PipeReader,
    writer: PipeWriter,
}

fn duplex() -> (End, End) {
    let (a_tx, a_rx) = channel();
    let (b_tx, b_rx) = channel();
    (
        End { reader: PipeReader { rx: b_rx, left: Vec::new(), pos: 0 }, writer: PipeWriter { tx: a_tx } },
        End { reader: PipeReader { rx: a_rx, left: Vec::new(), pos: 0 }, writer: PipeWriter { tx: b_tx } },
    )
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.left.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.left = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.left.len() - self.pos);
        buf[..n].copy_from_slice(&self.left[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx.send(buf.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------- small pieces ----------

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

/// This machine controls; nobody hosts here, so every request is left unanswered (never asked in practice).
struct ViewerOnlyUi;

impl HostUi for ViewerOnlyUi {
    fn somebody_present(&self) -> bool {
        false
    }
    fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        oneshot::channel().1
    }
    fn consent(&self, _request: ConsentRequest) -> oneshot::Receiver<bool> {
        oneshot::channel().1
    }
}

struct Monotonic(Instant);

impl Clock for Monotonic {
    fn now(&self) -> MonoMicros {
        MonoMicros(u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX))
    }
}

/// Warnings and errors of the core and the hub, on stderr (the orchestrator keeps them in the log).
struct StderrTrace;

impl TraceSink for StderrTrace {
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>) {
        if matches!(level, Level::Error | Level::Warn | Level::Info) {
            eprintln!("TRACE {level:?} {target}: {what}");
        }
    }
}

/// The throwaway keychain file and folder of this run, removed when dropped.
struct Scratch {
    dir: PathBuf,
    keychain_file: PathBuf,
    store: Arc<MacSecretStore>,
}

impl Scratch {
    fn new() -> Result<Self, String> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tag = format!("swapdesk-cross-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
        let dir = std::env::temp_dir().join(&tag);
        std::fs::create_dir_all(&dir).map_err(|e| format!("scratch folder: {e}"))?;
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("$HOME is not set")?);
        let keychain_file = dir.join(format!("{tag}.keychain-db"));
        create_keychain_file(&keychain_file, "swapdesk-cross-password")
            .map_err(|e| format!("throwaway keychain: {e}"))?;
        let mut location = SecretLocation::login(&home).with(tag, dir.join("folder"));
        location.keychain = KeychainLocation::File(keychain_file.clone());
        let store = Arc::new(MacSecretStore::open(&location).map_err(|e| format!("secret store: {e}"))?);
        Ok(Self { dir, keychain_file, store })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.store.delete_all();
        let _ = delete_keychain_file(&self.keychain_file);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(host_addr), Some(peer_code), Some(go)) =
        (value(&args, "--host-addr"), value(&args, "--peer-code"), value(&args, "--go"))
    else {
        eprintln!(
            "usage: cross_viewer --host-addr <ipv4> --peer-code <file> --go <file> --anchor-file <file> [--clip <dir>] [--files <dir>]"
        );
        return ExitCode::from(2);
    };
    let Some(anchor) = value(&args, "--anchor-file") else {
        eprintln!(
            "usage: cross_viewer --host-addr <ipv4> --peer-code <file> --go <file> --anchor-file <file> [--clip <dir>] [--files <dir>]"
        );
        return ExitCode::from(2);
    };
    let type_after = value(&args, "--type").map(PathBuf::from);
    let files_dir = value(&args, "--files").map(PathBuf::from);
    match run(
        &host_addr,
        PathBuf::from(peer_code),
        PathBuf::from(go),
        PathBuf::from(anchor),
        value(&args, "--clip"),
        type_after,
        files_dir,
    ) {
        Ok(()) => {
            println!("VIEWER_RESULT PASS");
            ExitCode::SUCCESS
        }
        Err(why) => {
            println!("VIEWER_RESULT FAIL {why}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    host_addr: &str,
    peer_code: PathBuf,
    go: PathBuf,
    anchor: PathBuf,
    clip_dir: Option<String>,
    type_after: Option<PathBuf>,
    files_dir: Option<PathBuf>,
) -> Result<(), String> {
    let scratch = Scratch::new()?;
    let trace: Arc<dyn TraceSink> = Arc::new(StderrTrace);
    let clock: Arc<dyn Clock> = Arc::new(Monotonic(Instant::now()));
    // Real LAN: the physical interface list of swapdesk-net-mac, no loopback, the system picks the ports (a Mac that
    // runs its own Swapdesk keeps 47820/47821).
    let MacServiceSetup { mut config, identity, trust, settings } = mac_service_config(MacServiceParams {
        default_name: "Mac viewer (cross-session)".into(),
        os_version: macos_version(),
        secrets: scratch.store.clone(),
        interfaces: Arc::new(swapdesk_net_mac::MacNetInterfaces),
        clock: Arc::clone(&clock),
        trace: Arc::clone(&trace),
        ui: Arc::new(ViewerOnlyUi),
        dev_loopback: None,
        ports: Some((0, 0)),
    })?;
    let hub = UiHub::new(HubParams {
        os: Os::MacOs,
        os_version: macos_version(),
        identity,
        trust,
        secrets: scratch.store.clone(),
        settings,
        interfaces: Arc::new(swapdesk_net_mac::MacNetInterfaces),
        presence: Arc::new(NoPresence),
        audit: Arc::new(MemoryAudit::new()),
        clock,
        wall: Arc::new(SystemWallClock),
        trace,
        hooks: Arc::new(NoHooks),
        dev: None,
    });
    config.ui = hub.host_ui();
    let core = Arc::new(ServiceCore::start(config).map_err(|e| format!("the core did not start: {e:?}"))?);
    hub.bind_core(&core);
    // The one UI below runs in session 1, the console's: only the console's UI may connect.
    hub.set_console_session(Some(1));
    println!("VIEWER_CORE_UP endpoints={:?}", core.local_addrs());

    // The UI pipe: the hub serves one end, the backend of the shells' kind drives the other.
    let (client_end, server_end) = duplex();
    let serving = hub.clone();
    let serve = std::thread::spawn(move || {
        let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
        Handshake::server(&mut reader, &mut writer, 1)?;
        serving.serve(
            reader,
            writer,
            ConnInfo {
                session_id: 1,
                user: UserRef { id: "cross-viewer".into(), name: "cross-viewer".into() },
                admin: false,
                can_elevate: false,
            },
        )
    });
    let factory: DecoderFactory = Box::new(|| -> Box<dyn VideoDecoder> {
        Box::new(VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra }))
    });
    let media = Media::new(factory);
    let backend = ServiceBackend::start_with_sink(
        client_end.reader,
        client_end.writer,
        Arc::new(NoWake),
        std::process::id(),
        Some(media.sink()),
    )
    .map_err(|e| format!("hub handshake: {e}"))?;

    let mut cfg = ViewerConfig::new(host_addr, peer_code, go, anchor);
    // `--type <file>`: the keyboard proof toward the Windows host. Once the orchestrator wrote `<file>` (its text box
    // has the focus there), the word is typed with physical keys; the chord after it is Ctrl+S.
    // The orchestrator's text box needs a click of the person at the host before it has the focus: a longer wait.
    if type_after.is_some() {
        cfg.anchor_timeout = std::time::Duration::from_secs(75);
    }
    cfg.type_after = type_after;
    cfg.save_with_ctrl = true;
    // `--clip <dir>`: the copy and paste proof through this user's pasteboard; the orchestrator works the host's
    // clipboard and meets this side through the files in `<dir>`.
    if let Some(dir) = clip_dir {
        backend.set_clipboard(swapdesk_input_mac::MacClipboard::new());
        cfg.clip = Some(swapdesk_crossdrive::ClipStep {
            dir: std::path::PathBuf::from(dir),
            provider: std::sync::Mutex::new(Box::new(swapdesk_input_mac::MacClipboard::new())),
        });
    }
    // `--files <dir>`: the files proof; the orchestrator works the host's disk and meets this side in `<dir>`.
    cfg.files = files_dir;
    let log = |line: &str| println!("{line}");
    let (report, result) = viewer_drive(&backend, &media, &cfg, &log);
    println!("{}", report.counts_line());

    // Teardown: the pipe, the hub, the core, the decoder; the scratch folder goes with `scratch`.
    drop(backend);
    let _ = serve.join();
    hub.shutdown();
    let end = Instant::now() + Duration::from_secs(3);
    let mut core = Some(core);
    while let Some(c) = core.take() {
        match Arc::try_unwrap(c) {
            Ok(c) => c.shutdown(),
            Err(c) if Instant::now() < end => {
                core = Some(c);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(c) => drop(c),
        }
    }
    media.finish();
    drop(scratch);
    println!("VIEWER_CLEANED keychain file, items and folder removed");

    result?;
    if let Some(why) = report.clip_failed {
        return Err(format!("copy and paste: {why}"));
    }
    if let Some(why) = report.files_failed {
        return Err(format!("files: {why}"));
    }
    if report.frames_decoded == 0 {
        return Err("no frame decoded".into());
    }
    Ok(())
}
