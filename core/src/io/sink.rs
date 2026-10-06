//! Byte output for a pipeline stage.
//!
//! [`Sink`] is the single shape every writer routes through; [`ChildStdioPlan`]
//! is its companion for children, pairing the stdio to hand `process::Launch`
//! with the sink the caller must pump after spawn.  The buffer helpers below own
//! the [`ByteBuffer`] idiom for captured bytes.

use super::edge::{DeadEdge, Edge};
use crate::sync::LockExt as _;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Cap on `Sink::Buffer` growth: past it bytes are dropped after a truncation
/// marker, so a high-volume capture has to become an explicit redirect.  Two
/// readings of the one event: a detached worker keeps the marked prefix, and a
/// capture whose bytes are about to become a value refuses it
/// (`runtime::capture::with_capture`, and `machine.rs`'s `Frame::Capture` arm).
pub(crate) const SINK_BUFFER_CAP: usize = 16 * 1024 * 1024;
const SINK_BUFFER_TRUNC_MARKER: &[u8] =
    b"\n[ral: buffer exceeded 16 MiB; remaining output dropped]\n";

/// Captured bytes, and the one thing about them the write path cannot say:
/// that [`SINK_BUFFER_CAP`] cut the stream short.
///
/// Every writer's `Ok(())` is honest about its write, so truncation is
/// recorded here and read out of band, once the buffer is complete.
#[derive(Debug, Default)]
pub struct CapturedBytes {
    bytes: Mutex<Vec<u8>>,
    overflowed: AtomicBool,
}

impl CapturedBytes {
    /// Drain the buffer.
    pub(crate) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.bytes.lock_ignore_poison())
    }

    /// Whether [`SINK_BUFFER_CAP`] truncated what [`Self::take`] hands back,
    /// so a caller for whom the bytes *are* a value can refuse them instead of
    /// binding a prefix.  Only meaningful once every writer has joined; `join`
    /// is what orders their stores against this load.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed)
    }

    /// Copy the buffer without draining it, so `poll` can sample a worker
    /// still running and the eventual [`Self::take`] still sees the whole
    /// output.  The price is that successive peeks overlap: each is a snapshot
    /// of everything so far, not a delta.
    pub(crate) fn peek(&self) -> Vec<u8> {
        self.bytes.lock_ignore_poison().clone()
    }

    /// Append under `SINK_BUFFER_CAP`, emitting the truncation marker once at
    /// the boundary and raising `overflowed` with it.  Sole enforcement point,
    /// so shell writes and pump-thread appends cannot disagree about the cap.
    fn append(&self, bytes: &[u8]) {
        let mut g = self.bytes.lock_ignore_poison();
        let cur = g.len();
        if cur < SINK_BUFFER_CAP + SINK_BUFFER_TRUNC_MARKER.len() {
            if cur + bytes.len() <= SINK_BUFFER_CAP {
                g.extend_from_slice(bytes);
            } else {
                g.extend_from_slice(&bytes[..SINK_BUFFER_CAP.saturating_sub(cur)]);
                g.extend_from_slice(SINK_BUFFER_TRUNC_MARKER);
                drop(g);
                self.overflowed.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Shared between writers and their eventual reader: writers run on pump and
/// worker threads, the reader on the eval thread after they join.
pub type ByteBuffer = Arc<CapturedBytes>;

/// What a `Sink::Watch` hands each whole line of its stream.
pub type LineFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// One child stream's routing, stdout or stderr.
///
/// `pump: None` means the child writes the destination itself; `Some(sink)`
/// means the kernel piped it and the caller must hand the child's fd to
/// [`Sink::pump`] after spawn.
///
/// Only [`Sink::child_stdout`] and [`Sink::child_stderr`] decide which, so no
/// caller reasons about "inherit, pipe, pump, tee" on its own.
pub(crate) struct ChildStdioPlan {
    pub(crate) stdio: crate::process::StdioSpec,
    pub(crate) pump: Option<Sink>,
}

impl ChildStdioPlan {
    /// The child gets the parent's matching fd directly.
    pub(crate) fn inherit() -> Self {
        Self {
            stdio: crate::process::StdioSpec::inherit(),
            pump: None,
        }
    }

    /// Route `sink` without inheriting: the kernel pipes the child's fd and the
    /// caller pumps it into `sink`.
    fn for_sink(sink: &Sink) -> Self {
        Self {
            stdio: crate::process::StdioSpec::piped(),
            pump: Some(sink.clone()),
        }
    }
}

/// Where a pipeline stage's byte output goes.
pub enum Sink {
    /// The inherited fd 1.  Whether it is a terminal is not this variant's
    /// claim but `TerminalState::startup_stdout_tty`'s.
    Terminal,
    /// The inherited fd 2, and the default `Io::stderr`.
    Stderr,
    /// Redirect target, opened by `runtime::redirect`.  `Arc` so a nested frame
    /// under a redirect clones the sink rather than `dup`ing the fd — a `dup`
    /// shares the file offset anyway, so nothing about where bytes land changes.
    File(Arc<std::fs::File>),
    /// In-memory capture, as under `let x = cmd` or a spawned handle.
    Buffer(ByteBuffer),
    /// Both branches in order; a failure on the first skips the second.
    Tee(Box<Self>, Box<Self>),
    /// A watched worker's stream: each whole line, terminator cut, is handed
    /// to `line`. `pending` holds the partial line `flush_pending` later emits.
    Watch { line: LineFn, pending: Vec<u8> },
    /// A stage thread's interior edge: the write end, the stage's wake, and
    /// the edge's fate.  The parent holds the read end, so no `EPIPE` — and
    /// no `SIGPIPE` to the whole shell — can reach a thread; a write to a
    /// dead edge ends the stage instead.
    Pipe {
        writer: Arc<os_pipe::PipeWriter>,
        wake: Arc<crate::io::Wake>,
        edge: Arc<Edge>,
    },
}

/// Write `bytes` in `PIPE_BUF` chunks, each after `poll` says it will not
/// block, so the wake and the edge are consulted between chunks.
///
/// A chunk that lands on a dead edge is the stage's reader-gone break, judged
/// after landing rather than refused before: it is the sentinel that hears the
/// write.  A fired wake ends the write as success — the stage is being
/// cancelled, and its next `check` says why.
#[cfg(unix)]
fn write_interruptible(
    w: &os_pipe::PipeWriter,
    wake: &crate::io::Wake,
    edge: &Edge,
    mut bytes: &[u8],
) -> io::Result<()> {
    use super::Readiness;
    use rustix::event::PollFlags;
    while !bytes.is_empty() {
        match wake.poll_beside(w, PollFlags::OUT)? {
            Readiness::Fired => return Ok(()),
            Readiness::Ready(revents) if revents.intersects(PollFlags::ERR | PollFlags::HUP) => {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            Readiness::Ready(_) => {
                let n = (&*w).write(&bytes[..bytes.len().min(libc::PIPE_BUF)])?;
                bytes = &bytes[n..];
            }
        }
        if edge.is_dead() {
            return Err(io::Error::other(DeadEdge));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_interruptible(
    w: &os_pipe::PipeWriter,
    wake: &crate::io::Wake,
    edge: &Edge,
    bytes: &[u8],
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;
    if wake.is_fired() {
        wake.acknowledge();
        return Ok(());
    }
    match (&*w).write_all(bytes) {
        // `CancelSynchronousIo` (from `ThreadStage::interrupt`) aborts the
        // `WriteFile` in flight; only a wake-caused abort is success.
        Err(e)
            if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed())
                && wake.is_fired() =>
        {
            wake.acknowledge();
            Ok(())
        }
        Ok(()) if edge.is_dead() => Err(io::Error::other(DeadEdge)),
        r => r,
    }
}

impl Sink {
    /// Emit a `Watch`'s unterminated tail, recursing through `Tee`; a no-op
    /// elsewhere.  End-of-stream only — nothing else ever emits that tail.
    pub(crate) fn flush_pending(&mut self) {
        match self {
            Self::Watch { line, pending } if !pending.is_empty() => {
                line(&std::mem::take(pending));
            }
            Self::Tee(a, b) => {
                a.flush_pending();
                b.flush_pending();
            }
            _ => {}
        }
    }

    /// Plan a child's stdout into this sink.
    ///
    /// `Terminal` always inherits; with `inherit_tty` — the caller's assertion
    /// that fd 1 really is this shell's terminal — so does `Stderr`, since a
    /// direct dup is the only way the child sees a TTY.
    /// Both halves of the plan bind the caller: `plan.stdio` before spawn, then
    /// the child's fd into [`Sink::pump`] if `plan.pump` is `Some`.
    ///
    /// # Errors
    /// Returns `Err` if cloning the sink to pump fails.
    pub(crate) fn child_stdout(&self, inherit_tty: bool) -> io::Result<ChildStdioPlan> {
        if matches!(self, Self::Terminal) || (inherit_tty && matches!(self, Self::Stderr)) {
            return Ok(ChildStdioPlan::inherit());
        }
        self.child_stdio_plan()
    }

    /// Plan a child's stderr into this sink: `Stderr` inherits fd 2, everything
    /// else is pumped.  No `inherit_tty` twin — stderr never owns the TTY.
    ///
    /// # Errors
    /// Returns `Err` if cloning the sink to pump fails.
    pub(crate) fn child_stderr(&self) -> io::Result<ChildStdioPlan> {
        if matches!(self, Self::Stderr) {
            return Ok(ChildStdioPlan::inherit());
        }
        self.child_stdio_plan()
    }

    /// The shared tail of `child_stdout`/`child_stderr` once inheriting is
    /// ruled out: a `File` or `Pipe` hands the child the fd directly,
    /// everything else is pumped.
    pub(crate) fn child_stdio_plan(&self) -> io::Result<ChildStdioPlan> {
        let stdio = match self {
            Self::File(f) => crate::process::StdioSpec::from_file(f.try_clone()?),
            Self::Pipe { writer, .. } => {
                crate::process::StdioSpec::from_pipe_writer(writer.try_clone()?)
            }
            _ => return Ok(ChildStdioPlan::for_sink(self)),
        };
        Ok(ChildStdioPlan { stdio, pump: None })
    }

    /// Do both sinks deliver to one destination?  Decides whether a child's
    /// two streams may share a single descriptor.
    pub(crate) fn same_destination(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Terminal, Self::Terminal) | (Self::Stderr, Self::Stderr) => true,
            (Self::File(a), Self::File(b)) => Arc::ptr_eq(a, b),
            (Self::Buffer(a), Self::Buffer(b)) => Arc::ptr_eq(a, b),
            (Self::Pipe { writer: a, .. }, Self::Pipe { writer: b, .. }) => Arc::ptr_eq(a, b),
            (Self::Watch { line: a, .. }, Self::Watch { line: b, .. }) => Arc::ptr_eq(a, b),
            (Self::Tee(a1, a2), Self::Tee(b1, b2)) => {
                a1.same_destination(b1) && a2.same_destination(b2)
            }
            (
                Self::Terminal
                | Self::Stderr
                | Self::File(_)
                | Self::Buffer(_)
                | Self::Tee(..)
                | Self::Watch { .. }
                | Self::Pipe { .. },
                _,
            ) => false,
        }
    }

    /// Spawn a thread draining `reader` into this sink, flushing its tail at
    /// EOF; a capture buffer is complete only once the handle is joined.  The
    /// drain outlives any write failure — a child must never find the pipe it
    /// writes into closed under it, and a dead edge still needs the bytes to
    /// land for the sentinel to hear them.
    pub(crate) fn pump(
        self,
        mut reader: impl Read + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut sink = self;
            let mut buf = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = sink.write_all(&buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            sink.flush_pending();
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
            Self::Watch { line, .. } => Self::Watch {
                line: line.clone(),
                // Each clone carries its own partial line: sharing `pending`
                // would let two threads interleave halves of one.
                pending: Vec::new(),
            },
            Self::Pipe { writer, wake, edge } => Self::Pipe {
                writer: writer.clone(),
                wake: wake.clone(),
                edge: edge.clone(),
            },
        }
    }
}

/// Wrap `base` in a `Sink::Tee` whose other branch is a fresh buffer, so bytes
/// are recorded and still seen.  `with_audit_capture` in `runtime::capture` is
/// the caller; it drains the buffer once every writer has closed.
pub(crate) fn tee_with_buffer(base: Sink) -> (Sink, ByteBuffer) {
    let buf = ByteBuffer::default();
    let sink = Sink::Tee(Box::new(Sink::Buffer(buf.clone())), Box::new(base));
    (sink, buf)
}

/// A fresh [`ByteBuffer`] and the sink that writes into it: callers wire the
/// sink onto `shell.io` and keep the arc to drain later.  Every `Sink::Buffer`
/// is minted here or in [`tee_with_buffer`], so nothing writes into a capture buffer
/// past [`CapturedBytes::append`].
pub(crate) fn new_buffer() -> (Sink, ByteBuffer) {
    let buf = ByteBuffer::default();
    (Sink::Buffer(buf.clone()), buf)
}

/// Length of the line terminator ending `bytes`: 2 for `\r\n`, 1 for `\n`,
/// else 0.  The one line rule, cut by every line reader and by capture,
/// `from-line` and `ask`: CRLF is one terminator, since a surviving `\r` would
/// send the cursor to column 0 wherever the text is interpolated, and a lone
/// `\r` is text.  Safe on undecoded bytes — neither appears mid-codepoint.
pub(crate) fn terminator_len(bytes: &[u8]) -> usize {
    match bytes {
        [.., b'\r', b'\n'] => 2,
        [.., b'\n'] => 1,
        _ => 0,
    }
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
                b.append(bytes);
                Ok(())
            }
            Self::Tee(a, b) => {
                a.write_all(bytes)?;
                b.write_all(bytes)
            }
            Self::Pipe { writer, wake, edge } => write_interruptible(writer, wake, edge, bytes),
            Self::Watch { line, pending } => {
                pending.extend_from_slice(bytes);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let whole = &pending[..=pos];
                    line(&whole[..whole.len() - terminator_len(whole)]);
                    pending.drain(..=pos);
                }
                Ok(())
            }
        }
    }

    /// `Watch` deliberately keeps its tail here: flushing a partial line
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
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::{Edge, Sink, terminator_len};
    use crate::io::Wake;
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    #[test]
    fn terminator_len_is_the_one_line_rule() {
        let cases: [(&[u8], usize); 11] = [
            (b"", 0),
            (b"hi", 0),
            (b"hi\n", 1),
            (b"\n", 1),
            (b"hi\r\n", 2),
            (b"\r\n", 2),
            (b"hi\r", 0),
            (b"\r", 0),
            (b"hi\r\r\n", 2),
            (b"hi\n\n", 1),
            (b"hi\r\n\r\n", 2),
        ];
        for (input, len) in cases {
            assert_eq!(
                terminator_len(input),
                len,
                "on b\"{}\"",
                input.escape_ascii()
            );
        }
    }

    #[test]
    fn pipe_write_is_read_by_pair() {
        use std::io::{Read, Write};
        use std::sync::Arc;

        let (mut reader, writer) = crate::process::cloexec_pipe().expect("pipe");
        let mut sink = Sink::Pipe {
            writer: Arc::new(writer),
            wake: Wake::new().expect("wake"),
            edge: Edge::new(),
        };
        sink.write_all(b"hello").expect("write");
        drop(sink);

        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).expect("read");
        assert_eq!(buf, b"hello");
    }

    #[cfg(unix)]
    #[test]
    fn pipe_child_stdout_reaches_reader() {
        use std::io::Read;
        use std::sync::Arc;

        let (mut reader, writer) = crate::process::cloexec_pipe().expect("pipe");
        let sink = Sink::Pipe {
            writer: Arc::new(writer),
            wake: Wake::new().expect("wake"),
            edge: Edge::new(),
        };
        let plan = sink.child_stdout(false).expect("plan");
        assert!(plan.pump.is_none());
        drop(sink);

        let mut child = crate::process::Launch::new("/bin/echo")
            .arg("hi")
            .stdout(plan.stdio)
            .spawn(crate::process::PgidPolicy::Inherit)
            .expect("spawn")
            .0;

        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).expect("read");
        assert_eq!(buf, b"hi\n");
        child.reap().expect("reap");
    }

    #[cfg(unix)]
    #[test]
    fn pipe_reader_eof_waits_for_sink_and_child() {
        use std::io::Read;
        use std::time::Duration;

        let (mut reader, writer) = crate::process::cloexec_pipe().expect("pipe");
        let writer = Arc::new(writer);
        let sink = Sink::Pipe {
            writer: writer.clone(),
            wake: Wake::new().expect("wake"),
            edge: Edge::new(),
        };
        let plan = sink.child_stdout(false).expect("plan");

        let mut child = crate::process::Launch::new("/bin/echo")
            .arg("hi")
            .stdout(plan.stdio)
            .spawn(crate::process::PgidPolicy::Inherit)
            .expect("spawn")
            .0;
        child.reap().expect("reap");

        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).expect("read");
            buf
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !handle.is_finished(),
            "reader saw EOF while the sink still held its edge"
        );

        drop(sink);
        drop(writer);
        assert_eq!(handle.join().expect("join"), b"hi\n");
    }

    fn file_sink() -> Sink {
        Sink::File(Arc::new(tempfile::tempfile().expect("anonymous file")))
    }

    #[test]
    fn same_destination_is_identity_of_the_target() {
        let f = file_sink();
        assert!(f.same_destination(&f.clone()));
        assert!(!f.same_destination(&file_sink()));

        let b = Sink::Buffer(Arc::default());
        assert!(b.same_destination(&b.clone()));
        assert!(!b.same_destination(&Sink::Buffer(Arc::default())));

        assert!(Sink::Terminal.same_destination(&Sink::Terminal));
        assert!(Sink::Stderr.same_destination(&Sink::Stderr));
        assert!(!Sink::Terminal.same_destination(&Sink::Stderr));
        assert!(!f.same_destination(&Sink::Terminal));
    }

    #[test]
    fn same_destination_compares_a_tee_branchwise() {
        let f = file_sink();
        let tee = |a: &Sink, b: &Sink| Sink::Tee(Box::new(a.clone()), Box::new(b.clone()));
        assert!(tee(&f, &Sink::Stderr).same_destination(&tee(&f, &Sink::Stderr)));
        assert!(!tee(&f, &Sink::Stderr).same_destination(&tee(&file_sink(), &Sink::Stderr)));
        assert!(!tee(&f, &Sink::Stderr).same_destination(&f));
    }

    #[test]
    fn same_destination_of_watch_needs_the_same_line_function() {
        let watch = || Sink::Watch {
            line: Arc::new(|_| {}),
            pending: Vec::new(),
        };
        let a = watch();
        assert!(a.same_destination(&a.clone()));
        assert!(!a.same_destination(&watch()));
    }

    #[test]
    fn a_watch_hands_over_each_whole_line_and_then_the_tail() {
        let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let log = seen.clone();
        let mut sink = Sink::Watch {
            line: Arc::new(move |line| log.lock().unwrap().push(line.to_vec())),
            pending: Vec::new(),
        };
        sink.write_all(b"one\r\ntw").unwrap();
        sink.write_all(b"o\nthree").unwrap();
        sink.flush_pending();
        let seen = seen.lock().unwrap();
        assert_eq!(*seen, [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    }

    #[test]
    fn a_file_plan_hands_over_the_fd_with_no_pump() {
        let plan = file_sink().child_stdout(false).expect("plan");
        assert!(plan.pump.is_none());
    }
}

/// The byte streams captured under [`RunIo::Capture`](super::RunIo::Capture), carried verbatim onto
/// the protocol [`Report`](crate::protocol::Report).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
