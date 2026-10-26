//! Shell byte streams and their terminal presentation.
//!
//! [`Io`] bundles per-`Shell` streams and cached terminal state. Stream
//! plumbing sits over [`edge`] / [`source`] / [`sink`]; [`terminal`] gates
//! terminal output, using the escape vocabulary in [`ansi`].

pub mod ansi;
mod edge;
pub(crate) mod frame;
mod poll;
mod regime;
mod sink;
mod source;
mod state;
pub mod terminal;
pub(crate) use edge::{Edge, WriteStopped};
#[cfg(unix)]
pub(crate) use poll::poll_slice;
#[cfg(windows)]
pub(crate) use poll::{aborted, interrupt};
pub use regime::{RunIo, RunStdin};
pub use sink::{ByteBuffer, Captured, CapturedBytes, LineFn, Sink, terminator_len};
pub(crate) use sink::{ChildStdioPlan, SINK_BUFFER_CAP, new_buffer, tee_with_buffer};
pub use source::{Source, SourceReader};

pub(crate) use state::Io;
