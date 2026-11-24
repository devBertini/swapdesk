//! One window of the phone: its text cache, the passes of a frame, and what the last frame registered.
//!
//! The Java view asks for a frame ([`PhoneWindow::paint`]) and gets either the frame or a list of text to measure;
//! it measures, hands the answer back ([`PhoneWindow::measured`]) and asks again. A frame always comes: after
//! [`PASSES_MAX`] lists in a row, or after an answer that was refused, the next paint returns the frame as it is,
//! with the labels nobody measured drawn on one line at a rough size.

use swapdesk_paint::{Frame, Interaction, Output};
use swapdesk_ui::{UiState, WindowKind};

use crate::canvas_ops::FRAME;
use crate::recorder::{Metrics, Recorder};
use crate::text_measure::{TextCache, TextKey, encode_needs};

/// Most lists of needs in a row before a frame is given as it is.
pub const PASSES_MAX: usize = 6;

/// Where and how a window is painted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Surface {
    pub kind: WindowKind,
    /// The window's size in DIPs.
    pub size: (f32, f32),
    /// Device pixels per DIP (Android's display density).
    pub scale: f32,
    /// An offscreen capture draws the values of text inputs itself; a live window shows native fields there.
    pub offscreen: bool,
}

/// What the passes of the last frame cost, for the trace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameCost {
    /// Lists of needs answered before the frame.
    pub lists: usize,
    /// Labels measured for it.
    pub labels: usize,
    /// The frame was given with labels still unmeasured.
    pub forced: bool,
}

#[derive(Default)]
pub struct PhoneWindow {
    texts: TextCache,
    /// The needs of the list Java is measuring.
    pending: Vec<TextKey>,
    lists: usize,
    labels: usize,
    refused: bool,
    last: Option<Output>,
    cost: FrameCost,
}

impl PhoneWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Paints `state`. The buffer starts with `canvas_ops::FRAME` (a frame to replay) or `canvas_ops::NEEDS` (text
    /// to measure and hand to [`Self::measured`] before painting again).
    pub fn paint(&mut self, metrics: Metrics, state: &UiState, ix: &Interaction, on: Surface) -> Vec<u8> {
        let recorder = Recorder::new(on.scale, metrics, &self.texts);
        let output = swapdesk_paint::paint(Frame::new(&recorder, state, ix, on.size, on.offscreen), on.kind);
        let needs = self.texts.take_needs();
        let forced = !needs.is_empty() && (self.refused || self.lists >= PASSES_MAX);
        if !needs.is_empty() && !forced {
            self.lists += 1;
            self.labels += needs.len();
            let bytes = encode_needs(&needs);
            self.pending = needs;
            return bytes;
        }
        self.cost = FrameCost { lists: self.lists, labels: self.labels, forced };
        (self.lists, self.labels, self.refused) = (0, 0, false);
        self.pending.clear();
        self.last = Some(output);
        let bytes = recorder.finish();
        debug_assert_eq!(bytes.first(), Some(&FRAME));
        bytes
    }

    /// Takes Java's answer to the last list. A refused answer (it does not read back, or does not match the list)
    /// teaches nothing, and the next paint gives its frame as it is.
    pub fn measured(&mut self, answer: &[u8]) -> Result<usize, &'static str> {
        let keys = std::mem::take(&mut self.pending);
        let taken = self.texts.measured(&keys, answer);
        self.refused = taken.is_err();
        taken
    }

    /// What the last frame registered: controls, text inputs, scroll.
    pub fn output(&self) -> Option<&Output> {
        self.last.as_ref()
    }

    /// What the last frame cost.
    pub fn cost(&self) -> FrameCost {
        self.cost
    }

    /// How many labels the window knows the measure of.
    pub fn known_labels(&self) -> usize {
        self.texts.len()
    }
}
