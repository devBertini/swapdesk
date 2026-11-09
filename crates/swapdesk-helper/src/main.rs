//! `swapdesk-helper.exe --role <system|user> --pipe <name> --server-pid <pid>` (CONTRACTS.md C2.1, C2.4).
//!
//! Only wiring: the loop is `swapdesk_host::helper::run_helper`, the sources and sinks are the `*-win` crates. The
//! service names the pipe and its own PID on the command line; the pipe open checks the server PID before a byte
//! is written. Exit codes: 0 after Shutdown/Bye/EOF, 2 bad arguments, 3 pipe refused, 4 the loop failed.

#[cfg(windows)]
fn main() {
    std::process::exit(win::run(std::env::args().skip(1).collect()));
}

#[cfg(not(windows))]
fn main() {
    eprintln!("swapdesk-helper runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
mod win {
    use std::sync::Arc;
    use std::time::Duration;

    use swapdesk_audio_win::{CaptureMode, WasapiLoopbackSource};
    use swapdesk_capture_win::{DxgiScreen, QpcClock, gpu_payload};
    use swapdesk_codec_win::{EncoderBackend, H264Encoder, H264EncoderOptions};
    use swapdesk_files::{FsServer, Roots};
    use swapdesk_host::helper::{HelperDeps, run_helper};
    use swapdesk_ipc::{AudioModeKind, IpcRole, VideoBackend};
    use swapdesk_platform::{EncodedFrame, EncoderConfig, EncoderInfo, Frame, GpuSurface, Pixels, VideoEncoder};

    /// The capture hands out `Pixels::Gpu(GpuSurface(GpuFrame))` (a pooled texture with its device), while the
    /// encoders take `GpuSurface(ID3D11Texture2D)`: this unwraps the texture for each `encode` and keeps the capture's
    /// `GpuFrame` borrowed, so alive and unrecycled, until `encode` returns (readings of stage 35).
    pub struct CaptureEncoder(pub H264Encoder);

    pub fn unwrap_gpu(frame: &Frame) -> Option<Frame> {
        let gpu = gpu_payload(frame)?;
        Some(Frame {
            display: frame.display,
            captured_at: frame.captured_at,
            width: frame.width,
            height: frame.height,
            pixels: Pixels::Gpu(GpuSurface(Box::new(gpu.texture().clone()))),
            dirty: frame.dirty.clone(),
            content_changed: frame.content_changed,
        })
    }

    impl VideoEncoder for CaptureEncoder {
        /// The encoder's name carries why NVENC did not serve an NVIDIA display, for the helper's log note.
        fn configure(&mut self, config: &EncoderConfig) -> swapdesk_platform::Result<EncoderInfo> {
            let mut info = self.0.configure(config)?;
            if let Some(reason) = self.0.fallback_reason() {
                info.name = format!("{} (NVENC did not open: {reason})", info.name);
            }
            Ok(info)
        }
        fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> swapdesk_platform::Result<Vec<EncodedFrame>> {
            match unwrap_gpu(frame) {
                Some(unwrapped) => self.0.encode(&unwrapped, force_keyframe),
                None => self.0.encode(frame, force_keyframe),
            }
        }
        fn set_rate(&mut self, kbps: u32, fps: u16) -> swapdesk_platform::Result<()> {
            self.0.set_rate(kbps, fps)
        }
        fn refine(&mut self, frame: &Frame) -> swapdesk_platform::Result<Vec<EncodedFrame>> {
            match unwrap_gpu(frame) {
                Some(unwrapped) => self.0.refine(&unwrapped),
                None => self.0.refine(frame),
            }
        }
        fn flush(&mut self) -> swapdesk_platform::Result<Vec<EncodedFrame>> {
            self.0.flush()
        }
    }

    pub struct Args {
        pub role: IpcRole,
        pub pipe: String,
        pub server_pid: u32,
    }

    pub fn parse(args: &[String]) -> Result<Args, String> {
        let mut role = None;
        let mut pipe = None;
        let mut server_pid = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut value = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
            match a.as_str() {
                "--role" => {
                    role = Some(match value()?.as_str() {
                        "system" => IpcRole::HelperSystem,
                        "user" => IpcRole::HelperUser,
                        other => return Err(format!("unknown role {other}")),
                    })
                }
                "--pipe" => pipe = Some(value()?),
                "--server-pid" => {
                    server_pid = Some(value()?.parse::<u32>().map_err(|_| "bad --server-pid".to_string())?)
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(Args {
            role: role.ok_or("--role is required")?,
            pipe: pipe.ok_or("--pipe is required")?,
            server_pid: server_pid.ok_or("--server-pid is required")?,
        })
    }

    fn encoder(
        first: &Frame,
        _config: &EncoderConfig,
        backend: VideoBackend,
    ) -> swapdesk_platform::Result<CaptureEncoder> {
        let mut options = H264EncoderOptions {
            backend: match backend {
                VideoBackend::Auto => EncoderBackend::Auto,
                VideoBackend::Nvenc => EncoderBackend::Nvenc,
                VideoBackend::Mf => EncoderBackend::MediaFoundation,
            },
            ..Default::default()
        };
        // The capture's BGRA texture is encoded on the device that owns it (readings of stage 35).
        options.mf.device = gpu_payload(first).map(|g| g.device().device.clone());
        Ok(CaptureEncoder(H264Encoder::new(options)))
    }

    /// The folder of the user this helper runs as.
    fn home_dir() -> Option<std::path::PathBuf> {
        let home =
            std::env::var_os("USERPROFILE").map(std::path::PathBuf::from).filter(|p| p.is_absolute() && p.is_dir());
        home.or_else(|| {
            let (drive, path) = (std::env::var_os("HOMEDRIVE")?, std::env::var_os("HOMEPATH")?);
            let mut p = std::path::PathBuf::from(drive);
            p.push(path);
            (p.is_absolute() && p.is_dir()).then_some(p)
        })
    }

    pub fn run(args: Vec<String>) -> i32 {
        let args = match parse(&args) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("swapdesk-helper: {e}");
                return 2;
            }
        };
        // D12: this process builds with panic = "abort"; the hook leaves the record before it dies.
        if let Some(dir) = swapdesk_sys_win::user_log_dir() {
            swapdesk_sys_win::install_panic_hook(&dir, "swapdesk-helper");
        }
        let conn = match swapdesk_sys_win::pipe::connect(&args.pipe, args.server_pid, Duration::from_secs(5)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("swapdesk-helper: pipe refused: {e:?}");
                return 3;
            }
        };
        let (reader, writer) = match conn.split() {
            Ok(rw) => rw,
            Err(e) => {
                eprintln!("swapdesk-helper: pipe split: {e}");
                return 3;
            }
        };
        let deps = HelperDeps::new(
            |_display| Ok(DxgiScreen::new()),
            encoder,
            || Ok(swapdesk_input_win::open_sink()),
            || Ok(WasapiLoopbackSource::new()),
            Arc::new(QpcClock),
        )
        // The user's clipboard and quick actions, for the service's clipboard sync and `QuickAction` (stage 42).
        .with_clipboard(|| Ok(swapdesk_input_win::clipboard::WinClipboard::new()))
        .with_actions(|| Ok(swapdesk_sys_win::actions::WinActions::user()))
        // Terminals (stage 43): the shell runs as this helper's user. `ConPtySpawner` re-runs the token guard (`check_spawn`)
        // with this process's own token and the console user the service named, so a compromised service cannot make this
        // process start a shell as anyone else, or as a service account.
        .with_pty(|session_user| {
            Arc::new(swapdesk_sys_win::pty::ConPtySpawner::current_user(
                swapdesk_sys_win::pty::Shell::user_default(),
                session_user,
            )) as Arc<dyn swapdesk_platform::PtySpawner>
        })
        .with_audio_mode(|s: &WasapiLoopbackSource| match s.mode() {
            Some(CaptureMode::ExcludeProcessTree(_)) => AudioModeKind::ProcessExclusion,
            Some(CaptureMode::Loopback) => AudioModeKind::Loopback,
            None => AudioModeKind::Off,
        });
        // The user helper serves the user's files to the service (C2.4 "Files"): its root is this user's home, chosen
        // here and never by the service; every path the service sends is checked against it again.
        let deps = match (args.role, home_dir()) {
            (IpcRole::HelperUser, Some(home)) => deps.with_fs(Arc::new(FsServer::new(Roots::Home {
                home,
                receive: Some(swapdesk_files::DEFAULT_RECEIVE.to_string()),
            }))),
            _ => deps,
        };
        match run_helper(reader, writer, deps, args.role, std::process::id()) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("swapdesk-helper: {e:?}");
                4
            }
        }
    }
}
