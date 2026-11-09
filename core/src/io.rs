//! Shell byte streams and their terminal presentation.
//!
//! [`Io`] bundles a `Shell`'s three streams. Stream plumbing sits over [`edge`] / [`source`] / [`sink`]; [`terminal`] gates
//! terminal output, using the escape vocabulary in [`ansi`].

pub mod ansi;
mod bounded;
pub(crate) mod child;
mod edge;
pub(crate) mod frame;
mod poll;
mod regime;
mod sink;
mod source;
mod state;
mod streams;
pub mod terminal;
pub use bounded::read_bounded;
pub(crate) use edge::Edge;
#[cfg(unix)]
pub(crate) use poll::poll_slice;
#[cfg(windows)]
pub(crate) use poll::{aborted, interrupt};
pub use regime::{RunIo, RunStdin};
pub use sink::{ByteBuffer, Captured, CapturedBytes, LineFn, Retained, Sink, without_terminator};
pub(crate) use sink::{SINK_BUFFER_CAP, WriteFault, new_buffer, tee_with_buffer};
pub use source::{Source, SourceReader};
pub use streams::Streams;

pub(crate) use state::{Io, IoPatch};
