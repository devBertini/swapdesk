//! The host side of the app (stage 24, slice a): the helper loop of `swapdesk_host::helper` over the Mac platform
//! crates, run by the service's supervisor ([`crate::service::HostHelper`]) in this process. It is what the Windows
//! user helper (`swapdesk-helper`) is, with ScreenCaptureKit capture and audio (`swapdesk-capture-mac`), the
//! VideoToolbox encoder (`swapdesk-codec-mac`), `CGEventPost` input and the general pasteboard (`swapdesk-input-mac`),
//! and the user's home as the files root.
//!
//! Grants (TCC): capture and audio need Screen Recording, input needs Accessibility. Without them the helper still
//! runs: the sources report `PermissionDenied` to the service, which tells the viewer. [`ask_for_grants`] shows the
//! system prompts once at start.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use swapdesk_capture_mac::audio::MacAudio;
use swapdesk_capture_mac::screen::{DisplayRegistry, MacScreen};
use swapdesk_codec_mac::VtH264Encoder;
use swapdesk_files::{Fs, FsServer, LocalFs, Roots};
use swapdesk_host::helper::{HelperDeps, run_helper};
use swapdesk_input_mac::{MacClipboard, MacInputSink};
use swapdesk_ipc::IpcRole;
use swapdesk_platform::{Clock, Level, PtySpawner, TraceSink};
use swapdesk_sys_mac::actions::MacActions;
use swapdesk_sys_mac::pty::MacPtySpawner;
use swapdesk_ui::OsPermission;

use crate::service::HostHelper;

/// The disk of this user's files, kept so the app can ask which folder macOS refused (F-FILE-03).
static USER_DISK: OnceLock<Arc<LocalFs>> = OnceLock::new();

/// The folder macOS last refused this app (the "Files and Folders" grant is missing), until a read or create there
/// works again. The UI says so and offers [`swapdesk_sys_mac::FILES_AND_FOLDERS_PANE`].
pub fn receive_folder_denied() -> Option<PathBuf> {
    USER_DISK.get().and_then(|d| d.access_denied())
}

/// The helper loop the supervisor runs, again after each end.
pub fn host_helper(clock: Arc<dyn Clock>) -> HostHelper {
    Arc::new(move |reader, writer| {
        let (screen_clock, encoder_clock, audio_clock) = (Arc::clone(&clock), Arc::clone(&clock), Arc::clone(&clock));
        // One display numbering for every screen this helper makes (the listing and each stream), so switching
        // displays live and plugging one in never changes what an id means.
        let registry = DisplayRegistry::default();
        let deps = HelperDeps::new(
            move |_display| MacScreen::with_registry(Arc::clone(&screen_clock), Arc::clone(&registry)),
            move |_first, _config, _backend| Ok(VtH264Encoder::new(Arc::clone(&encoder_clock))),
            || Ok(MacInputSink::new()),
            move || MacAudio::new(Arc::clone(&audio_clock)),
            Arc::clone(&clock),
        )
        .with_clipboard(|| Ok(MacClipboard::new()))
        // The user's quick actions and keep-awake (F-ACT-01).
        .with_actions(|| Ok(MacActions::user()))
        // Terminals: the shell runs as this process's user. The service's session user is not an input: the spawner never
        // changes uid, refuses root and refuses `admin`, so nothing the service sends widens that.
        .with_pty(|_session_user| Arc::new(MacPtySpawner::current_user()) as Arc<dyn PtySpawner>);
        // The user's files (C2.4 "Files"): the root is this user's home, chosen here, never by the service.
        let deps =
            match std::env::var_os("HOME").map(std::path::PathBuf::from).filter(|p| p.is_absolute() && p.is_dir()) {
                Some(home) => {
                    // The free space of the receiving volume lets the receiver refuse a file that does not fit
                    // before writing anything.
                    let disk = Arc::clone(
                        USER_DISK.get_or_init(|| Arc::new(LocalFs::with_free_space(swapdesk_sys_mac::free_bytes))),
                    );
                    // Its roots are the home folder and the published volumes (FILES-UI.md 8.1).
                    deps.with_fs(Arc::new(
                        FsServer::with_fs(
                            disk as Arc<dyn Fs>,
                            Roots::Home { home, receive: Some(swapdesk_files::DEFAULT_RECEIVE.to_string()) },
                        )
                        .with_system(crate::system_disk::system_disk()),
                    ))
                }
                None => deps,
            };
        run_helper(reader, writer, deps, IpcRole::HelperUser, std::process::id())
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })
}

/// Shows the system prompts for the grants hosting needs, once at start and only for those missing: Screen Recording
/// (capture and audio), then Accessibility and Input Monitoring (`swapdesk_input_mac::request_access`, which the
/// viewer's shortcut capture shares). macOS shows each prompt only the first time; afterwards the user grants it in
/// System Settings > Privacy & Security.
pub fn ask_for_grants(trace: &dyn TraceSink) {
    let screen = swapdesk_capture_mac::screen_access() || swapdesk_capture_mac::request_screen_access();
    let input = swapdesk_input_mac::can_post() || swapdesk_input_mac::request_access().complete();
    trace.log(Level::Info, "app", format_args!("hosting grants: screen recording {screen}, input {input}"));
}

/// Which requests "Conceder permissões" makes for the permissions the notice lists: Screen Recording, and the one call
/// of Accessibility and Input Monitoring (`request_access` asks for each of the two that is missing).
pub fn grant_requests(missing: &[OsPermission]) -> (bool, bool) {
    let screen = missing.contains(&OsPermission::ScreenRecording);
    let input = missing.iter().any(|p| matches!(p, OsPermission::Accessibility | OsPermission::InputMonitoring));
    (screen, input)
}

/// The "Conceder permissões" button: asks macOS for what the notice lists, in this process (the app owns the grants,
/// D32: the capture, the event posting and the shortcut grab all run in it). The system's prompt appears only the
/// first time; the shell then opens the pane of the first one, where the person switches the app on.
pub fn request_grants(missing: &[OsPermission], trace: &dyn TraceSink) {
    let (screen, input) = grant_requests(missing);
    let screen_now = screen.then(swapdesk_capture_mac::request_screen_access);
    let input_now = input.then(|| swapdesk_input_mac::request_access().complete());
    trace.log(
        Level::Info,
        "app",
        format_args!("grants asked for: screen recording {screen_now:?}, input {input_now:?} (answers come later)"),
    );
}
