//! The terminal model of the Swapdesk terminal panel (stage 20, D19): `vte` parses the host's output into a cell grid
//! with scrollback, a viewport, a selection and damage tracking; [`encode_key`] and [`encode_paste`] turn the viewer's
//! keys into PTY bytes; [`TermView`] owns the model and drains the bounded output channel. OS-free, no `unsafe`.
//! The shells only draw [`Screen`].

mod cell;
mod channel;
mod keys;
mod perform;
mod screen;
mod selection;
mod term;

pub use cell::{Attrs, Cell, Color, Row, char_width};
pub use channel::{Closed, OUTPUT_CHANNEL_CHUNKS, OutputTx, Pump, TermView, output_channel};
pub use keys::{Key, Mods, encode_key, encode_paste};
pub use screen::{Cursor, Damage, Modes, Screen};
pub use selection::{Point, Selection};
pub use term::Term;

#[cfg(test)]
mod tests;
