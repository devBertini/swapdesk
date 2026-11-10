//! Blocking framing over `std::io`: `FrameReader` and `FrameWriter`.
//!
//! A video frame crosses a pipe sixty times a second, so neither side copies its payload: the writer hands the
//! message's own bytes to the pipe after a small head, and the reader reads them straight into the buffer the message
//! keeps. Every other message goes through one buffer that the reader or the writer reuses.

use std::fmt;
use std::io::{self, Read, Write};

use swapdesk_proto::{CodecError, Reader, VIDEO_HEADER_LEN};

use crate::error::{IpcError, Result};
use crate::message::{
    Message, check_declared, decode_body, frame_parts, video_declared, video_from_parts, video_prefix,
};
use crate::redact::Payload;

/// A reused buffer that grew past this is let go after its message, so one large message (a file part, a clipboard
/// image) does not hold its size for the life of the connection. Also the most a reader reserves for a body before
/// the body's bytes arrive.
const KEEP_BUFFER: usize = 64 * 1024;

/// The most a reader reserves for a video payload before its bytes arrive (a frame of ordinary size is read into a
/// buffer of exactly its length); a larger payload's buffer grows with the bytes that came.
const VIDEO_RESERVE: usize = 1024 * 1024;

/// A video payload up to this size is copied behind its head and written with it in one call; a larger one is written
/// by itself, uncopied.
const WRITE_TOGETHER: usize = 16 * 1024;

/// Reads messages from a byte stream.
pub struct FrameReader<R: Read> {
    inner: R,
    /// The body of the message being read (never a video payload).
    body: Vec<u8>,
}

/// The reused buffer holds the last body read: `Debug` shows the stream only.
impl<R: Read + fmt::Debug> fmt::Debug for FrameReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameReader").field("inner", &self.inner).finish_non_exhaustive()
    }
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, body: Vec::new() }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// One varint from the stream. `Ok(None)` only when the stream ends before its first byte and `eof_ok`.
    fn varint(&mut self, eof_ok: bool) -> Result<Option<u64>> {
        let mut first = [0u8; 1];
        loop {
            match self.inner.read(&mut first) {
                Ok(0) => {
                    return if eof_ok { Ok(None) } else { Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()) };
                }
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let len = 1usize << (first[0] >> 6);
        let mut buf = [0u8; 8];
        buf[0] = first[0];
        if len > 1 {
            self.inner.read_exact(&mut buf[1..len])?;
        }
        Ok(Some(Reader::new(&buf[..len]).varint()?))
    }

    /// The next message; `Ok(None)` when the stream ended cleanly between two messages. The declared length is
    /// checked against the type's limit before a single body byte is read or reserved. What is reserved ahead of the
    /// bytes is bounded whatever the length says: 64 KiB for a body, 1 MiB for a video payload; past that a buffer
    /// grows only with bytes that arrived.
    pub fn read_message(&mut self) -> Result<Option<Message>> {
        let Some(ty) = self.varint(true)? else {
            return Ok(None);
        };
        let len = self.varint(false)?.ok_or(IpcError::Closed)?;
        let len = check_declared(ty, len)?;
        if let Some(prefix) = video_prefix(ty)
            && len >= prefix
        {
            return self.read_video(ty, prefix, len - prefix).map(Some);
        }
        self.body.clear();
        if len <= KEEP_BUFFER {
            self.body.resize(len, 0);
            self.inner.read_exact(&mut self.body)?;
        } else {
            self.body.reserve(KEEP_BUFFER);
            (&mut self.inner).take(len as u64).read_to_end(&mut self.body)?;
            if self.body.len() != len {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
            }
        }
        let msg = decode_body(ty, &self.body);
        if self.body.capacity() > KEEP_BUFFER {
            self.body = Vec::new();
        }
        msg.map(Some)
    }

    /// A video message whose body is `prefix` bytes and a payload of `want`: the payload is read into the buffer the
    /// message keeps, of exactly its length, and is never copied. A header that is wrong, or that declares another
    /// length than the frame has, is refused before the payload is read or reserved.
    fn read_video(&mut self, ty: u64, prefix: usize, want: usize) -> Result<Message> {
        let mut head = [0u8; 8 + VIDEO_HEADER_LEN];
        let head = &mut head[..prefix];
        self.inner.read_exact(head)?;
        if video_declared(ty, head)? != want {
            return Err(CodecError::InvalidValue("video payload length").into());
        }
        let mut payload = vec![0u8; want.min(VIDEO_RESERVE)];
        self.inner.read_exact(&mut payload)?;
        while payload.len() < want {
            // As much again as has arrived, at most.
            let have = payload.len();
            let more = (want - have).min(have);
            // Exactly that much: the last step leaves the buffer at the payload's length, with nothing spare.
            payload.reserve_exact(more);
            payload.resize(have + more, 0);
            self.inner.read_exact(&mut payload[have..])?;
        }
        video_from_parts(ty, head, Payload::from(payload))
    }
}

/// Writes messages to a byte stream, flushing after each.
pub struct FrameWriter<W: Write> {
    inner: W,
    /// The frame being written, without a large video payload.
    frame: Vec<u8>,
}

/// The reused buffer holds the last message written: `Debug` shows the stream only.
impl<W: Write + fmt::Debug> fmt::Debug for FrameWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameWriter").field("inner", &self.inner).finish_non_exhaustive()
    }
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, frame: Vec::new() }
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    pub fn write_message(&mut self, msg: &Message) -> Result<()> {
        let (start, payload) = frame_parts(msg, &mut self.frame)?;
        if payload.len() <= WRITE_TOGETHER {
            self.frame.extend_from_slice(payload);
            self.inner.write_all(&self.frame[start..])?;
        } else {
            self.inner.write_all(&self.frame[start..])?;
            self.inner.write_all(payload)?;
        }
        self.inner.flush()?;
        if self.frame.capacity() > KEEP_BUFFER {
            self.frame = Vec::new();
        }
        Ok(())
    }
}
