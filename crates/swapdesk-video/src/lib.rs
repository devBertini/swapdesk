//! Video transport of Swapdesk (PROTOCOL.md 4 and 7.1): one unidirectional QUIC stream per encoded frame.
//!
//! * [`sender::VideoSender`] (host): opens a stream per frame at priority 1, resets what went stale (a newer
//!   keyframe, a backlog over 100 ms), holds P-frames back until a wanted keyframe is sent, and turns the
//!   receiver's reports and the path's RTT and loss into a target bitrate ([`rate::RateController`]).
//! * [`receiver::VideoReceiver`] (viewer): reads the streams, orders them ([`sequencer::Sequencer`]), stops what is
//!   stale, hands the decoder only a decodable sequence and asks for a keyframe on a gap or a decode error.
//!
//! Frames are opaque payloads here: no codec, no capture, no `unsafe`. The encoder hears the bitrate through
//! [`rate::BitrateSink`] and asks the sender whether to emit a keyframe with `take_keyframe_request`.

pub mod rate;
pub mod receiver;
pub mod sender;
pub mod sequencer;
mod util;

pub use rate::{BitrateSink, NoBitrateSink, RateConfig, RateController, RateReason};
pub use receiver::{
    FeedbackSink, ReceivedFrame, ReceiverConfig, ReceiverStats, STOP_BUDGET, STOP_STALE, VideoReceiver,
};
pub use sender::{Frame, RESET_STALE, SendOutcome, SenderConfig, SenderStats, VIDEO_PRIORITY, VideoSender};
pub use sequencer::{Action, Completed, MAX_DISPLAYS, Sequencer, SequencerConfig, SequencerStats};
