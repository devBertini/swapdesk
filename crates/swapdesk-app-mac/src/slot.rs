//! The main thread's side of the viewer: the shell's [`VideoSlot`] and the thread-local that owns the [`MainSide`].
//!
//! The renderer's layers may only be edited on the main thread, and the shell calls its slot there, so the `MainSide`
//! lives in a thread-local of the main thread: [`MacVideoSlot::attached`] builds it, [`with_main`] reaches it from the
//! UI backend hooks (also main thread), and [`wake_main`] (called by the decode thread through the `Handoff`) queues
//! a block on the main dispatch queue that drains it. On any other thread `with_main` finds nothing.
//!
//! Apple documentation: `dispatch_async` on `dispatch_get_main_queue` (developer.apple.com/documentation/dispatch/
//! dispatchqueue/main, via `dispatch2::DispatchQueue::main().exec_async`).

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use dispatch2::DispatchQueue;
use swapdesk_app::shared::mailbox::{Cmd, Mailbox};
use swapdesk_input_mac::MacInput;
use swapdesk_platform::{NativeSurface, ScaleMode};
use swapdesk_shell_mac::{VideoArea, VideoSlot, VideoSurface};

use crate::decode::Shared;
use crate::handoff::Handoff;
use crate::layout_state::LayoutState;
use crate::main_side::{MainFrame, MainSide, PresentObserver, pixel_area};
use crate::pipeline::MacViewer;

/// How long the window waits for the decode thread to let go of the decoder before it closes.
const DETACH_WAIT: Duration = Duration::from_secs(2);

thread_local! {
    static MAIN: RefCell<Option<MainSide>> = const { RefCell::new(None) };
}

/// Runs `f` on the main thread's `MainSide`; `None` when there is none (no viewer window, another thread, or called
/// from inside another `with_main`).
pub fn with_main<R>(f: impl FnOnce(&mut MainSide) -> R) -> Option<R> {
    MAIN.with(|m| m.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Replaces the `MainSide` (dropping the old one, which detaches its renderer); main thread.
fn set_main(side: Option<MainSide>) {
    let old = MAIN.with(|m| m.replace(side));
    drop(old);
}

fn drain_main() {
    with_main(MainSide::drain);
}

/// Called by the decode thread (any thread) when a picture or a cursor update is waiting: drains it on the main
/// thread.
pub fn wake_main() {
    DispatchQueue::main().exec_async(drain_main);
}

/// The shell's video slot for this app.
pub struct MacVideoSlot {
    mailbox: Arc<Mailbox>,
    shared: Arc<Shared>,
    layout: Arc<LayoutState>,
    handoff: Arc<Handoff<MainFrame>>,
    observer: Option<PresentObserver>,
    input: MacInput,
}

impl MacVideoSlot {
    pub fn new(viewer: &MacViewer) -> Self {
        Self {
            mailbox: Arc::clone(&viewer.mailbox),
            shared: Arc::clone(&viewer.shared),
            layout: Arc::clone(&viewer.layout),
            handoff: Arc::clone(&viewer.handoff),
            observer: viewer.observer.clone(),
            input: viewer.input(),
        }
    }
}

impl VideoSlot for MacVideoSlot {
    fn attached(&self, surface: &VideoSurface) {
        let area = pixel_area(&surface.area);
        let backing = surface.area.scale;
        let parent_layer = surface.parent_layer_ptr();
        let side = MainSide::attach(
            parent_layer,
            Some(surface.view.clone()),
            area,
            backing,
            ScaleMode::Fit,
            Arc::clone(&self.shared),
            Arc::clone(&self.layout),
            Arc::clone(&self.handoff),
            self.observer.clone(),
        );
        match side {
            Ok(side) => {
                set_main(Some(side));
                self.input.set_backing_scale(backing);
                self.mailbox.push_cmd(Cmd::Attach {
                    surface: NativeSurface::MacOs { parent_layer },
                    area,
                    scale: ScaleMode::Fit,
                });
                // The first area comes before the window was laid out: ask the view again once the run loop turned.
                let input = self.input.clone();
                DispatchQueue::main().exec_async(move || {
                    if let Some(Some(scale)) = with_main(MainSide::relayout_from_view) {
                        input.set_backing_scale(scale);
                    }
                });
            }
            Err(e) => self.shared.error("attach", e),
        }
    }

    fn resized(&self, area: VideoArea) {
        let px = pixel_area(&area);
        with_main(|m| m.resized(px, area.scale));
        self.input.set_backing_scale(area.scale);
        self.mailbox.push_cmd(Cmd::Resize(px));
    }

    fn detached(&self) {
        // The layers go first (the window's tree is about to go), then the decoder is released and acknowledged.
        set_main(None);
        let (tx, rx) = channel();
        self.mailbox.push_cmd(Cmd::Detach(tx));
        let _ = rx.recv_timeout(DETACH_WAIT);
    }
}
