//! The OS-free half of the viewer pipeline, shared by `Swapdesk.exe` ([`crate::viewer`]) and the macOS app
//! (`swapdesk-app-mac`): the hand-over between the pipe's reader thread and the decode thread, the keyframe and report
//! scheduling, the remote cursor, the audio player over any `AudioSink`, and the output side (`ViewerOut`, the input
//! thread's batching). Nothing here touches the OS; every platform-specific piece (decoder, renderer, window, input
//! source, audio sink) stays in the platform's own app.

pub mod audio;
pub mod cursor;
pub mod down;
pub mod feedback;
pub mod mailbox;
pub mod out;
