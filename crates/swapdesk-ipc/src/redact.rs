//! Wrappers whose `Debug` prints a length and never the content (C2.4: no token, text, input or byte body).

use core::fmt;

use swapdesk_proto::{CodecError, CursorShape, InputEvent, Reader, Wire, Writer};

/// Opaque bytes (video payload, audio datagram, an encoded reply body).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Bytes(pub Vec<u8>);

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes(len {})", self.0.len())
    }
}

/// A batch of input records (key codes and typed text are as private as the text itself).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Events(pub Vec<InputEvent>);

impl fmt::Debug for Events {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Events(count {})", self.0.len())
    }
}

/// A text that may hold a never-logged value (the SAS code, a fault detail, a message argument).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Text(pub String);

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Text(len {})", self.0.len())
    }
}

impl Wire for Text {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, lim: usize) -> Result<(), CodecError> {
        w.put_str(&self.0, lim)
    }

    fn get(r: &mut Reader<'_>, lim: usize) -> Result<Self, CodecError> {
        Ok(Self(r.str(lim)?))
    }
}

/// A cursor image (pixels of whatever is under the pointer).
#[derive(Clone, PartialEq, Eq)]
pub struct Shape(pub CursorShape);

impl fmt::Debug for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Shape({}x{}, id {}, rgba len {})",
            self.0.header.width,
            self.0.header.height,
            self.0.header.shape_id,
            self.0.rgba.len()
        )
    }
}
