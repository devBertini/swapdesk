//! The hand-over between the pipe's reader thread and the decode thread.
//!
//! The reader thread (`ViewerSink`) must return quickly, so it only pushes here. The decode thread takes, in this
//! order: commands (never dropped), cursor shapes, the newest cursor position (older ones are replaced), then video
//! frames. Video is bounded: when the decoder falls behind by more than [`MAX_FRAMES`] frames or [`MAX_BYTES`] bytes
//! the queue is emptied and everything is dropped until the next keyframe (the decoder would have to reset at the hole
//! anyway), and the decode thread is told to ask for one ([`Item::Reset`]). A keyframe also empties the queue: the
//! pictures before it are never shown.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_platform::{CursorShape, NativeSurface, Rect, ScaleMode};
use swapdesk_proto::{CursorPosition, VideoHeader};

/// Most frames waiting for the decoder (about 130 ms at 60 fps).
pub const MAX_FRAMES: usize = 8;
/// Most payload bytes waiting for the decoder.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Cursor shapes kept waiting.
const MAX_SHAPES: usize = 16;

pub enum Cmd {
    /// The viewer window exists: build the renderer and the decoder on its surface.
    Attach {
        surface: NativeSurface,
        area: Rect,
        scale: ScaleMode,
    },
    Resize(Rect),
    Scale(ScaleMode),
    /// Tear the renderer down and acknowledge (the window's composition tree is about to go).
    Detach(Sender<()>),
    /// The viewer started or stopped controlling the host: draw the remote cursor again under the new rule.
    CursorMode,
}

pub struct Frame {
    pub session_id: u64,
    pub header: VideoHeader,
    pub payload: Vec<u8>,
    pub arrived: Instant,
}

pub enum Item {
    Cmd(Cmd),
    Shape(CursorShape),
    Cursor(u64, CursorPosition),
    /// Frames were dropped for lack of time: reset the decoder and ask for a keyframe.
    Reset,
    Frame(Frame),
    Timeout,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Queued,
    /// Dropped: the queue is waiting for a keyframe.
    Skipped,
    /// The queue was full and was emptied (the frame itself is dropped, an oversized keyframe too).
    Overflowed,
}

#[derive(Default)]
struct State {
    cmds: VecDeque<Cmd>,
    shapes: VecDeque<CursorShape>,
    cursor: Option<(u64, CursorPosition)>,
    frames: VecDeque<Frame>,
    bytes: usize,
    /// Everything but a keyframe is dropped until one arrives.
    resync: bool,
    reset: bool,
    /// Frames dropped since the last `take_dropped`.
    dropped: u64,
    closed: bool,
}

#[derive(Default)]
pub struct Mailbox {
    state: Mutex<State>,
    cv: Condvar,
}

impl Mailbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push_cmd(&self, cmd: Cmd) {
        self.lock().cmds.push_back(cmd);
        self.cv.notify_one();
    }

    pub fn push_shape(&self, shape: CursorShape) {
        let mut s = self.lock();
        if s.shapes.len() >= MAX_SHAPES {
            s.shapes.pop_front();
        }
        s.shapes.push_back(shape);
        drop(s);
        self.cv.notify_one();
    }

    pub fn set_cursor(&self, session_id: u64, pos: CursorPosition) {
        self.lock().cursor = Some((session_id, pos));
        self.cv.notify_one();
    }

    pub fn push_frame(&self, frame: Frame) -> Pushed {
        let mut s = self.lock();
        let keyframe = frame.header.keyframe;
        if keyframe && frame.payload.len() > MAX_BYTES {
            // A keyframe the queue can never hold: it is not queued, so the stream stays unusable. Keep waiting for the next keyframe (the P-frames after
            // this one are dropped) and have the decode thread ask for one again.
            let n = s.frames.len() as u64 + 1;
            s.dropped += n;
            s.frames.clear();
            s.bytes = 0;
            s.resync = true;
            s.reset = true;
            drop(s);
            self.cv.notify_one();
            return Pushed::Overflowed;
        }
        if keyframe {
            // Nothing before a keyframe is ever shown.
            let n = s.frames.len() as u64;
            s.dropped += n;
            s.frames.clear();
            s.bytes = 0;
            s.resync = false;
        } else if s.resync {
            s.dropped += 1;
            return Pushed::Skipped;
        } else if s.frames.len() >= MAX_FRAMES || s.bytes + frame.payload.len() > MAX_BYTES {
            let n = s.frames.len() as u64 + 1;
            s.dropped += n;
            s.frames.clear();
            s.bytes = 0;
            s.resync = true;
            s.reset = true;
            drop(s);
            self.cv.notify_one();
            return Pushed::Overflowed;
        }
        s.bytes += frame.payload.len();
        s.frames.push_back(frame);
        drop(s);
        self.cv.notify_one();
        Pushed::Queued
    }

    /// Frames dropped since the last call.
    pub fn take_dropped(&self) -> u64 {
        std::mem::take(&mut self.lock().dropped)
    }

    pub fn queued_frames(&self) -> usize {
        self.lock().frames.len()
    }

    /// After this `next` returns [`Item::Closed`] once everything queued before has been taken.
    pub fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    fn take(s: &mut State) -> Option<Item> {
        if let Some(c) = s.cmds.pop_front() {
            return Some(Item::Cmd(c));
        }
        if let Some(sh) = s.shapes.pop_front() {
            return Some(Item::Shape(sh));
        }
        if s.reset {
            s.reset = false;
            return Some(Item::Reset);
        }
        if let Some((id, pos)) = s.cursor.take() {
            return Some(Item::Cursor(id, pos));
        }
        if let Some(f) = s.frames.pop_front() {
            s.bytes = s.bytes.saturating_sub(f.payload.len());
            return Some(Item::Frame(f));
        }
        None
    }

    /// The next thing to do, waiting at most `timeout`.
    pub fn next(&self, timeout: Duration) -> Item {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        loop {
            if let Some(item) = Self::take(&mut s) {
                return item;
            }
            if s.closed {
                return Item::Closed;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Item::Timeout;
            }
            s = self.cv.wait_timeout(s, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::Codec;

    fn frame(id: u32, keyframe: bool, len: usize) -> Frame {
        Frame {
            session_id: 1,
            header: VideoHeader {
                keyframe,
                config: keyframe,
                discontinuity: false,
                display_id: 0,
                frame_id: id,
                capture_us: 0,
                encode_us: 0,
                width: 64,
                height: 64,
                codec: Codec::H264,
                payload_len: len as u32,
            },
            payload: vec![0; len],
            arrived: Instant::now(),
        }
    }

    fn pos(seq: u32) -> CursorPosition {
        CursorPosition { visible: true, display: 0, seq, x: 1, y: 2, shape_id: 0 }
    }

    fn ids(mb: &Mailbox) -> Vec<u32> {
        let mut out = Vec::new();
        while let Item::Frame(f) = mb.next(Duration::ZERO) {
            out.push(f.header.frame_id);
        }
        out
    }

    #[test]
    fn frames_come_out_in_order() {
        let mb = Mailbox::new();
        assert_eq!(mb.push_frame(frame(1, true, 10)), Pushed::Queued);
        assert_eq!(mb.push_frame(frame(2, false, 10)), Pushed::Queued);
        assert_eq!(ids(&mb), vec![1, 2]);
    }

    #[test]
    fn overflow_empties_the_queue_and_skips_until_a_keyframe() {
        let mb = Mailbox::new();
        mb.push_frame(frame(0, true, 10));
        for i in 1..MAX_FRAMES as u32 {
            assert_eq!(mb.push_frame(frame(i, false, 10)), Pushed::Queued);
        }
        assert_eq!(mb.push_frame(frame(99, false, 10)), Pushed::Overflowed);
        assert_eq!(mb.push_frame(frame(100, false, 10)), Pushed::Skipped);
        assert!(matches!(mb.next(Duration::ZERO), Item::Reset));
        assert_eq!(mb.take_dropped(), MAX_FRAMES as u64 + 1 + 1);
        assert_eq!(mb.push_frame(frame(101, true, 10)), Pushed::Queued);
        assert_eq!(mb.push_frame(frame(102, false, 10)), Pushed::Queued);
        assert_eq!(ids(&mb), vec![101, 102]);
    }

    #[test]
    fn a_keyframe_drops_the_pictures_before_it() {
        let mb = Mailbox::new();
        mb.push_frame(frame(1, true, 10));
        mb.push_frame(frame(2, false, 10));
        mb.push_frame(frame(3, true, 10));
        assert_eq!(ids(&mb), vec![3]);
        assert_eq!(mb.take_dropped(), 2);
    }

    #[test]
    fn an_oversized_keyframe_keeps_the_resync_and_the_p_frames_after_it_are_dropped() {
        let mb = Mailbox::new();
        mb.push_frame(frame(1, true, 10));
        assert_eq!(mb.push_frame(frame(2, true, MAX_BYTES + 1)), Pushed::Overflowed);
        assert_eq!(mb.queued_frames(), 0);
        assert!(matches!(mb.next(Duration::ZERO), Item::Reset));
        assert_eq!(mb.push_frame(frame(3, false, 10)), Pushed::Skipped);
        assert_eq!(mb.push_frame(frame(4, false, 10)), Pushed::Skipped);
        assert!(matches!(mb.next(Duration::ZERO), Item::Timeout));
        assert_eq!(mb.take_dropped(), 1 + 1 + 2);
        assert_eq!(mb.push_frame(frame(5, true, 10)), Pushed::Queued);
        assert_eq!(mb.push_frame(frame(6, false, 10)), Pushed::Queued);
        assert_eq!(ids(&mb), vec![5, 6]);
    }

    #[test]
    fn too_many_bytes_overflow_too() {
        let mb = Mailbox::new();
        mb.push_frame(frame(1, true, MAX_BYTES - 5));
        assert_eq!(mb.push_frame(frame(2, false, 100)), Pushed::Overflowed);
        assert_eq!(mb.queued_frames(), 0);
    }

    #[test]
    fn commands_and_shapes_go_first_and_only_the_newest_cursor_position_is_kept() {
        let mb = Mailbox::new();
        mb.push_frame(frame(1, true, 10));
        mb.set_cursor(1, pos(1));
        mb.set_cursor(1, pos(2));
        mb.push_cmd(Cmd::Resize(Rect { x: 0, y: 0, width: 10, height: 10 }));
        assert!(matches!(mb.next(Duration::ZERO), Item::Cmd(Cmd::Resize(_))));
        match mb.next(Duration::ZERO) {
            Item::Cursor(1, p) => assert_eq!(p.seq, 2),
            _ => panic!("expected the newest cursor position"),
        }
        assert!(matches!(mb.next(Duration::ZERO), Item::Frame(_)));
        assert!(matches!(mb.next(Duration::ZERO), Item::Timeout));
    }

    #[test]
    fn close_ends_the_wait_after_the_queue_is_empty() {
        let mb = Mailbox::new();
        mb.push_frame(frame(1, true, 10));
        mb.close();
        assert!(matches!(mb.next(Duration::from_secs(5)), Item::Frame(_)));
        assert!(matches!(mb.next(Duration::from_secs(5)), Item::Closed));
    }
}
