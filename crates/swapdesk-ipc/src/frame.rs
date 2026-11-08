//! Blocking framing over `std::io`: `FrameReader` and `FrameWriter`.

use std::io::{self, Read, Write};

use swapdesk_proto::Reader;

use crate::error::{IpcError, Result};
use crate::message::{Message, check_declared, decode_body, encode_message};

/// Reads messages from a byte stream.
#[derive(Debug)]
pub struct FrameReader<R: Read> {
    inner: R,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
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
    /// checked against the type's limit before a single body byte is read or reserved, and the body buffer
    /// grows only with bytes that arrive.
    pub fn read_message(&mut self) -> Result<Option<Message>> {
        let Some(ty) = self.varint(true)? else {
            return Ok(None);
        };
        let len = self.varint(false)?.ok_or(IpcError::Closed)?;
        let len = check_declared(ty, len)?;
        let mut body = Vec::with_capacity(len.min(64 * 1024));
        (&mut self.inner).take(len as u64).read_to_end(&mut body)?;
        if body.len() != len {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        decode_body(ty, &body).map(Some)
    }
}

/// Writes messages to a byte stream, flushing after each.
#[derive(Debug)]
pub struct FrameWriter<W: Write> {
    inner: W,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    pub fn write_message(&mut self, msg: &Message) -> Result<()> {
        let bytes = encode_message(msg)?;
        self.inner.write_all(&bytes)?;
        self.inner.flush()?;
        Ok(())
    }
}
