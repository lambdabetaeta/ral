//! Byte output for a pipeline stage.
//!
//! [`Sink`] is the single shape every writer routes through; [`ChildStdioPlan`]
//! is its companion for children, pairing the stdio to hand `process::Launch`
//! with the sink the caller must pump after spawn.  The buffer helpers below own
//! the [`ByteBuffer`] idiom for captured bytes.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Cap on `Sink::Buffer` growth: past it bytes are dropped after a truncation
/// marker, so a high-volume capture has to become an explicit redirect.  Two
/// readings of the one event: a detached worker keeps the marked prefix, and a
/// capture whose bytes are about to become a value refuses it (`eval_capture`).
pub(crate) const SINK_BUFFER_CAP: usize = 16 * 1024 * 1024;
const SINK_BUFFER_TRUNC_MARKER: &[u8] =
    b"\n[ral: buffer exceeded 16 MiB; remaining output dropped]\n";

/// Frontend-provided byte writer — the REPL installs rustyline's
/// `ExternalPrinter` here so bytes land above the active prompt instead of
/// over it.
///
/// `Send + Sync` because one `External` sink is cloned into every watcher
/// and pump thread.
pub trait ExternalWrite: Send + Sync {
    /// Implementors must serialise whole calls — `LineFramed` leans on that for
    /// line atomicity.
    ///
    /// # Errors
    /// Returns `Err` if the underlying frontend write fails.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
}

/// Captured bytes, and the one thing about them the write path cannot say:
/// that [`SINK_BUFFER_CAP`] cut the stream short.
///
/// `Sink::pump` hands back `()` from a thread and every writer's `Ok(())` is
/// honest about the write, so truncation is recorded here and read out of
/// band, once the buffer is complete.
#[derive(Debug, Default)]
pub struct CapturedBytes {
    bytes: Mutex<Vec<u8>>,
    overflowed: AtomicBool,
}

/// Shared between writers and their eventual reader: writers run on pump and
/// worker threads, the reader on the eval thread after they join.
pub type ByteBuffer = Arc<CapturedBytes>;

/// One child stream's routing, stdout or stderr.
///
/// `pump: None` means the child writes the destination itself; `Some(sink)`
/// means the kernel piped it and the caller must hand the child's fd to
/// [`Sink::pump`] after spawn.
///
/// Only [`Sink::child_stdout`] and [`Sink::child_stderr`] decide which, so no
/// caller reasons about "inherit, pipe, pump, tee" on its own.
pub struct ChildStdioPlan {
    pub stdio: crate::process::StdioSpec,
    pub pump: Option<Sink>,
}

impl ChildStdioPlan {
    /// The child gets the parent's matching fd directly.
    pub fn inherit() -> Self {
        Self {
            stdio: crate::process::StdioSpec::inherit(),
            pump: None,
        }
    }

    /// Route `sink` without inheriting: the kernel pipes the child's fd and the
    /// caller pumps it into `sink`.
    fn for_sink(sink: &Sink) -> io::Result<Self> {
        Ok(Self {
            stdio: crate::process::StdioSpec::piped(),
            pump: Some(sink.clone()),
        })
    }
}

/// Where a pipeline stage's byte output goes.
pub enum Sink {
    /// The inherited fd 1.  Whether it is a terminal is not this variant's
    /// claim but `TerminalState::startup_stdout_tty`'s.
    Terminal,
    /// The inherited fd 2, and the default `Io::stderr`.
    Stderr,
    /// Redirect target, opened by `evaluator::redirect`. `Arc` so a nested
    /// `to_ambient` under a redirect clones the sink rather than `dup`ing
    /// the fd — a `dup` shares the file offset anyway, so nothing about
    /// where bytes land changes.
    File(Arc<std::fs::File>),
    /// In-memory capture, as under `let x = cmd` or a spawned handle.
    Buffer(ByteBuffer),
    /// Both branches in order; a failure on the first skips the second.
    Tee(Box<Self>, Box<Self>),
    /// The frontend's own writer — rustyline's printer under the REPL.
    External(Arc<dyn ExternalWrite>),
    /// Buffers up to each `\n`, then emits `prefix + line + '\n'` to `inner` as
    /// one write.  `watch` frames a background block's output this way, so it
    /// stays line-atomic against the parent's without a global multiplexer;
    /// `pending` holds the partial line that `flush_pending` later emits.
    LineFramed {
        inner: Box<Self>,
        prefix: String,
        pending: Vec<u8>,
    },
}

impl Sink {
    /// Emit a `LineFramed`'s unterminated tail, recursing through `Tee`; a no-op
    /// elsewhere.  End-of-stream only — nothing else ever emits that tail.
    ///
    /// # Errors
    /// Returns `Err` if writing the tail to an inner sink fails.
    pub fn flush_pending(&mut self) -> io::Result<()> {
        match self {
            Self::LineFramed {
                inner,
                prefix,
                pending,
            } => {
                if pending.is_empty() {
                    return Ok(());
                }
                let tail = std::mem::take(pending);
                emit_framed(inner, prefix, &tail)
            }
            Self::Tee(a, b) => {
                a.flush_pending()?;
                b.flush_pending()
            }
            _ => Ok(()),
        }
    }

    /// Plan a child's stdout into this sink.
    ///
    /// `Terminal` always inherits; with `inherit_tty` — the caller's assertion
    /// that fd 1 really is this shell's terminal — so do `Stderr` and
    /// `External`, since a direct dup is the only way the child sees a TTY.
    /// Both halves of the plan bind the caller: `plan.stdio` before spawn, then
    /// the child's fd into [`Sink::pump`] if `plan.pump` is `Some`.
    ///
    /// # Errors
    /// Returns `Err` if cloning the sink to pump fails.
    pub fn child_stdout(&self, inherit_tty: bool) -> io::Result<ChildStdioPlan> {
        if matches!(self, Self::Terminal)
            || (inherit_tty && matches!(self, Self::Stderr | Self::External(_)))
        {
            return Ok(ChildStdioPlan::inherit());
        }
        ChildStdioPlan::for_sink(self)
    }

    /// Plan a child's stderr into this sink: `Stderr` inherits fd 2, everything
    /// else is pumped.  No `inherit_tty` twin — stderr never owns the TTY.
    ///
    /// # Errors
    /// Returns `Err` if cloning the sink to pump fails.
    pub fn child_stderr(&self) -> io::Result<ChildStdioPlan> {
        if matches!(self, Self::Stderr) {
            return Ok(ChildStdioPlan::inherit());
        }
        ChildStdioPlan::for_sink(self)
    }

    /// Spawn a thread draining `reader` into this sink, flushing its tail at
    /// EOF.  A capture buffer is only complete once the handle is joined.
    pub fn pump(self, reader: impl Read + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut sink = self;
            let _ = io::copy(&mut { reader }, &mut sink);
            let _ = sink.flush_pending();
        })
    }

}

impl Clone for Sink {
    /// Share the same ultimate destination — cheap and infallible, since
    /// every variant's handle is itself shared (`Arc`) or trivially copied.
    fn clone(&self) -> Self {
        match self {
            Self::Terminal => Self::Terminal,
            Self::Stderr => Self::Stderr,
            Self::File(f) => Self::File(f.clone()),
            Self::Buffer(b) => Self::Buffer(b.clone()),
            Self::Tee(a, b) => Self::Tee(Box::new((**a).clone()), Box::new((**b).clone())),
            Self::External(w) => Self::External(w.clone()),
            Self::LineFramed { inner, prefix, .. } => Self::LineFramed {
                inner: Box::new((**inner).clone()),
                prefix: prefix.clone(),
                // Each clone carries its own partial line: sharing `pending`
                // would let two threads interleave halves of one.
                pending: Vec::new(),
            },
        }
    }
}

/// Wrap `base` in a `Sink::Tee` whose other branch is a fresh buffer, so bytes
/// are recorded and still seen.  `with_audit_capture` in `evaluator::capture` is
/// the caller; it drains the buffer once every writer has closed.
pub(crate) fn tee_with_buffer(base: Sink) -> (Sink, ByteBuffer) {
    let buf = ByteBuffer::default();
    let sink = tee_into(base, &buf);
    (sink, buf)
}

/// The same tee, into a [`ByteBuffer`] that already exists: two conduits
/// recorded as one stream, which is what `audit` wants of a command's stdout
/// and the ambient sink beside it.
pub(crate) fn tee_into(base: Sink, buf: &ByteBuffer) -> Sink {
    Sink::Tee(Box::new(Sink::Buffer(buf.clone())), Box::new(base))
}

/// A fresh [`ByteBuffer`] and the sink that writes into it: callers wire the
/// sink onto `shell.io` and keep the arc to drain later.  Every `Sink::Buffer`
/// is minted here or in [`tee_into`], so nothing writes into a capture buffer
/// past [`write_capped`].
pub(crate) fn new_buffer() -> (Sink, ByteBuffer) {
    let buf = ByteBuffer::default();
    (Sink::Buffer(buf.clone()), buf)
}

/// Drain a [`ByteBuffer`].  Empty on a poisoned lock: the writers have already
/// joined, so there is nothing to recover, and no capture is worth a panic torn
/// through `await` result construction.
pub(crate) fn take_buffer(buf: &ByteBuffer) -> Vec<u8> {
    buf.bytes
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// Whether [`SINK_BUFFER_CAP`] truncated what [`take_buffer`] hands back — so
/// a caller for whom the bytes *are* a value can refuse them instead of binding
/// a prefix.  Only meaningful once every writer has joined; `join` is what
/// orders their stores against this load.
pub(crate) fn buffer_overflowed(buf: &ByteBuffer) -> bool {
    buf.overflowed.load(Ordering::Relaxed)
}

/// Copy a [`ByteBuffer`] without draining it, so `poll` can sample a worker
/// still running and the eventual [`take_buffer`] still sees the whole output.
/// The price is that successive peeks overlap: each is a snapshot of everything
/// so far, not a delta.  Empty on a poisoned lock, as [`take_buffer`] is.
pub(crate) fn peek_buffer(buf: &ByteBuffer) -> Vec<u8> {
    buf.bytes.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Drop one trailing line terminator, as POSIX `$()` does, so `let x = echo hi`
/// binds `"hi"`.  `\r\n` counts as one: Windows tools emit CRLF, and a surviving
/// `\r` would send the cursor to column 0 wherever the value is interpolated.  A
/// lone `\r` is content and stays.  Safe on undecoded bytes — neither appears
/// mid-codepoint.
pub(crate) fn strip_trailing_newline(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
}

/// String peer of [`strip_trailing_newline`], for `from-line` and `ask`.  The
/// test below pins the two to one CRLF/LF/lone-CR rule so they cannot drift.
pub(crate) fn str_strip_one_terminator(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

/// Append under `SINK_BUFFER_CAP`, emitting the truncation marker once at the
/// boundary and raising `overflowed` with it.  Sole enforcement point, so shell
/// writes and pump-thread appends cannot disagree about the cap.
fn write_capped(buf: &CapturedBytes, bytes: &[u8]) {
    if let Ok(mut g) = buf.bytes.lock() {
        let cur = g.len();
        if cur < SINK_BUFFER_CAP + SINK_BUFFER_TRUNC_MARKER.len() {
            if cur + bytes.len() <= SINK_BUFFER_CAP {
                g.extend_from_slice(bytes);
            } else {
                g.extend_from_slice(&bytes[..SINK_BUFFER_CAP.saturating_sub(cur)]);
                g.extend_from_slice(SINK_BUFFER_TRUNC_MARKER);
                buf.overflowed.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// One write, so the line cannot be split: shared by the mid-stream and tail
/// paths of `LineFramed`.
fn emit_framed(inner: &mut Sink, prefix: &str, line: &[u8]) -> io::Result<()> {
    inner.write_all(&[prefix.as_bytes(), line, b"\n"].concat())
}

impl Write for Sink {
    /// Consumes the whole slice or errors; no variant reports a short write.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Terminal => io::stdout().write_all(bytes),
            Self::Stderr => io::stderr().write_all(bytes),
            Self::File(f) => (&**f).write_all(bytes),
            Self::Buffer(b) => {
                write_capped(b, bytes);
                Ok(())
            }
            Self::Tee(a, b) => {
                a.write_all(bytes)?;
                b.write_all(bytes)
            }
            Self::External(w) => w.write(bytes),
            Self::LineFramed {
                inner,
                prefix,
                pending,
            } => {
                // One write per line is what buys atomicity: the OS stdout lock
                // or the `External` adapter's mutex serialises whole writes, so
                // sibling watchers interleave lines rather than halves of one.
                pending.extend_from_slice(bytes);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let line = &pending[..pos];
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    emit_framed(inner, prefix, line)?;
                    pending.drain(..=pos);
                }
                Ok(())
            }
        }
    }

    /// `LineFramed` deliberately keeps its tail here: flushing a partial line
    /// would terminate it and frame the rest as a new one.  That belongs to
    /// [`Sink::flush_pending`], at end of stream.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Terminal => io::stdout().flush(),
            Self::Stderr => io::stderr().flush(),
            Self::File(f) => (&**f).flush(),
            Self::Tee(a, b) => {
                a.flush()?;
                b.flush()
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{str_strip_one_terminator, strip_trailing_newline};

    fn strip(input: &[u8]) -> Vec<u8> {
        let mut buf = input.to_vec();
        strip_trailing_newline(&mut buf);
        buf
    }

    /// Pins the two implementations of the one terminator rule together.
    #[test]
    fn str_peer_agrees_with_byte_version() {
        for case in ["hi\n", "hi\r\n", "hi", "", "hi\r", "hi\n\n", "hi\r\n\r\n"] {
            assert_eq!(
                str_strip_one_terminator(case).as_bytes(),
                strip(case.as_bytes()).as_slice(),
                "disagreement on {case:?}",
            );
        }
    }

    #[test]
    fn strips_lf() {
        assert_eq!(strip(b"hi\n"), b"hi");
    }

    #[test]
    fn strips_crlf() {
        assert_eq!(strip(b"hi\r\n"), b"hi");
    }

    #[test]
    fn no_terminator_is_noop() {
        assert_eq!(strip(b"hi"), b"hi");
    }

    #[test]
    fn empty_is_noop() {
        assert_eq!(strip(b""), b"");
    }

    #[test]
    fn lone_cr_preserved() {
        assert_eq!(strip(b"hi\r"), b"hi\r");
    }

    #[test]
    fn strips_exactly_one_lf() {
        assert_eq!(strip(b"hi\n\n"), b"hi\n");
    }

    #[test]
    fn strips_exactly_one_crlf() {
        assert_eq!(strip(b"hi\r\n\r\n"), b"hi\r\n");
    }
}
