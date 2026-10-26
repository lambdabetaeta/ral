//! Byte output for a pipeline stage.
//!
//! [`Sink`] is the single shape every writer routes through; [`ChildStdioPlan`]
//! is its companion for children, pairing the stdio to hand `process::Launch`
//! with the sink the caller must pump after spawn.  The buffer helpers below own
//! the [`ByteBuffer`] idiom for captured bytes.

use super::edge::{Edge, WriteStopped};
use crate::process::{CancelCause, Scope, ScopeId};
use crate::sync::LockExt as _;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

/// Cap on `Sink::Buffer` growth: past it bytes are dropped after a truncation
/// marker, so a high-volume capture has to become an explicit redirect.  Two
/// readings of the one event: a worker keeps the marked prefix, and a
/// capture whose bytes are about to become a value refuses it
/// (`shell::exec::capture::with_capture`, and `machine.rs`'s `Frame::Capture` arm).
pub(crate) const SINK_BUFFER_CAP: usize = 16 * 1024 * 1024;
const SINK_BUFFER_TRUNC_MARKER: &[u8] =
    b"\n[ral: buffer exceeded 16 MiB; remaining output dropped]\n";

/// A bounded prefix and the byte count before retention.
///
/// Every writer's `Ok(())` is honest about its write, so truncation is
/// recorded here and read out of band, once the buffer is complete.
#[derive(Debug)]
pub struct CapturedBytes {
    bytes: Mutex<(Vec<u8>, u64)>,
    cap: usize,
    marker: &'static [u8],
}

impl Default for CapturedBytes {
    fn default() -> Self {
        Self::limited(SINK_BUFFER_CAP, SINK_BUFFER_TRUNC_MARKER)
    }
}

impl CapturedBytes {
    fn limited(cap: usize, marker: &'static [u8]) -> Self {
        Self {
            bytes: Mutex::default(),
            cap,
            marker,
        }
    }

    /// Drain the buffer.
    pub(crate) fn take(&self) -> Vec<u8> {
        self.take_counted().0
    }

    pub(crate) fn take_counted(&self) -> (Vec<u8>, u64) {
        let mut g = self.bytes.lock_ignore_poison();
        (std::mem::take(&mut g.0), g.1)
    }

    /// Whether retention truncated what [`Self::take`] hands back,
    /// so a caller for whom the bytes *are* a value can refuse them instead of
    /// binding a prefix. Read after every writer has joined for the final verdict.
    pub(crate) fn overflowed(&self) -> bool {
        self.total() > self.cap as u64
    }

    pub(crate) fn total(&self) -> u64 {
        self.bytes.lock_ignore_poison().1
    }

    /// Copy the buffer without draining it, so `poll` can sample a worker
    /// still running and the eventual [`Self::take`] still sees the whole
    /// output.  The price is that successive peeks overlap: each is a snapshot
    /// of everything so far, not a delta.
    pub(crate) fn peek(&self) -> Vec<u8> {
        self.bytes.lock_ignore_poison().0.clone()
    }

    /// Count every byte; retain the prefix and at most one marker.
    pub(crate) fn append(&self, bytes: &[u8]) {
        let mut g = self.bytes.lock_ignore_poison();
        g.1 += bytes.len() as u64;
        let cur = g.0.len();
        if cur <= self.cap {
            let keep = bytes.len().min(self.cap - cur);
            g.0.extend_from_slice(&bytes[..keep]);
            if keep < bytes.len() {
                g.0.extend_from_slice(self.marker);
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

    /// A pipeline edge handed to the child whole: nothing of ral's writes it.
    pub(crate) fn edge(writer: os_pipe::PipeWriter) -> Self {
        Self {
            stdio: crate::process::StdioSpec::from_pipe_writer(writer),
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
    /// Redirect target, opened by `shell::exec::redirect`.  `Arc` so a nested frame
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
    /// A stage thread's interior edge: the write end, the edge's fate, and
    /// the writing stage's name, for its reader-gone stop.  The parent holds
    /// the read end, so no `EPIPE` can reach a thread; a write to a dead edge
    /// stops the stage instead.  The scope a write polls is the writer's own,
    /// handed in at the write.
    Pipe {
        writer: Arc<os_pipe::PipeWriter>,
        edge: Arc<Edge>,
        stage: ScopeId,
    },
}

/// Write `bytes` in `PIPE_BUF` chunks.  Before each chunk the writer's scope
/// is read, so an always-ready pipe still polls once per chunk; a chunk that
/// would block is waited for a slice at a time.  A chunk that lands on a dead
/// edge is the stage's reader-gone stop, judged after landing, not refused
/// before, so the sentinel hears the write; a chunk that cannot land on one
/// (the pipe full, nobody draining) is the reader-gone stop too, judged at the
/// slice.
#[cfg(unix)]
fn write_sliced(
    w: &os_pipe::PipeWriter,
    edge: &Edge,
    stage: &ScopeId,
    scope: &Scope,
    mut bytes: &[u8],
) -> io::Result<()> {
    use super::poll_slice;
    use rustix::event::PollFlags;
    let stopped = |cause, owner| io::Error::other(WriteStopped { cause, owner });
    let reader_gone = || stopped(CancelCause::ReaderGone, Some(stage.clone()));
    while !bytes.is_empty() {
        if let Some((cause, owner)) = scope.in_force() {
            return Err(stopped(cause, Some(owner)));
        }
        let Some(revents) = poll_slice(w, PollFlags::OUT)? else {
            if edge.is_dead() {
                return Err(reader_gone());
            }
            continue;
        };
        if revents.intersects(PollFlags::ERR | PollFlags::HUP) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = (&*w).write(&bytes[..bytes.len().min(libc::PIPE_BUF)])?;
        bytes = &bytes[n..];
        if edge.is_dead() {
            return Err(reader_gone());
        }
    }
    Ok(())
}

/// `CreatePipe`'s default buffer: a chunk fits a drained pipe whole, so the
/// sentinel's one read lands it.
#[cfg(windows)]
const PIPE_CHUNK: usize = 4096;

/// The same loop, a chunk per `write`; a blocked chunk is aborted by an
/// `interrupt`, which re-reads the scope and the edge.  A cause in force
/// stops; none (shielded) retries the chunk.
#[cfg(windows)]
fn write_sliced(
    w: &os_pipe::PipeWriter,
    edge: &Edge,
    stage: &ScopeId,
    scope: &Scope,
    mut bytes: &[u8],
) -> io::Result<()> {
    let stopped = |cause, owner| io::Error::other(WriteStopped { cause, owner });
    let reader_gone = || stopped(CancelCause::ReaderGone, Some(stage.clone()));
    while !bytes.is_empty() {
        if let Some((cause, owner)) = scope.in_force() {
            return Err(stopped(cause, Some(owner)));
        }
        match (&*w).write(&bytes[..bytes.len().min(PIPE_CHUNK)]) {
            Ok(n) => bytes = &bytes[n..],
            Err(e) if super::aborted(&e) => {}
            Err(e) => return Err(e),
        }
        if edge.is_dead() {
            return Err(reader_gone());
        }
    }
    Ok(())
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

    /// Write all of `bytes`, or error; no variant reports a short write.  A
    /// `Pipe` blocked on a full edge reads `scope` between slices, through
    /// `Tee`.
    pub(crate) fn write_under(&mut self, scope: &Scope, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Terminal => io::stdout().write_all(bytes),
            Self::Stderr => io::stderr().write_all(bytes),
            Self::File(f) => (&**f).write_all(bytes),
            Self::Buffer(b) => {
                b.append(bytes);
                Ok(())
            }
            Self::Tee(a, b) => {
                a.write_under(scope, bytes)?;
                b.write_under(scope, bytes)
            }
            Self::Pipe {
                writer,
                edge,
                stage,
            } => write_sliced(writer, edge, stage, scope, bytes),
            Self::Watch { line, pending } => {
                for segment in bytes.split_inclusive(|&b| b == b'\n') {
                    pending.extend_from_slice(segment);
                    if segment.last() == Some(&b'\n') {
                        let whole = std::mem::take(pending);
                        line(&whole[..whole.len() - terminator_len(&whole)]);
                    }
                }
                Ok(())
            }
        }
    }

    /// `Watch` deliberately keeps its tail here: flushing a partial line
    /// would terminate it and frame the rest as a new one.  That belongs to
    /// [`Sink::flush_pending`], at end of stream.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
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

    /// `Stderr`, or a `Tee` either of whose branches is: where a broken pipe
    /// is dropped, as every Unix program drops it.
    pub(crate) fn is_stderr(&self) -> bool {
        match self {
            Self::Stderr => true,
            Self::Tee(a, b) => a.is_stderr() || b.is_stderr(),
            _ => false,
        }
    }

    /// Spawn a thread draining `reader` into this sink, flushing its tail at
    /// EOF; a capture buffer is complete only once the handle is joined.  The
    /// drain outlives any write failure or stop — a child must never find the
    /// pipe it writes into closed under it, and a dead edge still needs the
    /// bytes to land for the sentinel to hear them.  Writes read `scope`, the
    /// launching computation's current scope, so a pump blocked on a full edge
    /// whose reader a teardown killed ends within a slice (on Windows, of
    /// `Pumps::join_under`'s interrupt).
    pub(crate) fn pump(
        self,
        mut reader: impl Read + Send + 'static,
        scope: Scope,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut sink = self;
            let mut buf = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = sink.write_under(&scope, &buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    #[cfg(windows)]
                    Err(e) if super::aborted(&e) => {}
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
            Self::Pipe {
                writer,
                edge,
                stage,
            } => Self::Pipe {
                writer: writer.clone(),
                edge: edge.clone(),
                stage: stage.clone(),
            },
        }
    }
}

/// Wrap `base` in a `Sink::Tee` whose other branch is a fresh buffer, so bytes
/// are recorded and still seen.  `with_audit_capture` in `shell::exec::capture` is
/// the caller; it drains the buffer once every writer has closed.
pub(crate) fn tee_with_buffer(base: Sink, cap: usize) -> (Sink, ByteBuffer) {
    let buf = Arc::new(CapturedBytes::limited(cap, &[]));
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

/// Length of one trailing CRLF (2 bytes) or LF (1 byte), otherwise zero.
/// A lone CR is text. Safe before UTF-8 decoding.
pub fn terminator_len(bytes: &[u8]) -> usize {
    match bytes {
        [.., b'\r', b'\n'] => 2,
        [.., b'\n'] => 1,
        _ => 0,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    #[cfg(unix)]
    use super::WriteStopped;
    use super::{CapturedBytes, Edge, Sink, terminator_len};
    #[cfg(unix)]
    use crate::process::CancelCause;
    use crate::process::DurableRoot;
    #[cfg(unix)]
    use crate::process::Scope;
    #[cfg(unix)]
    use std::io::Read as _;
    #[cfg(unix)]
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[test]
    fn an_overflow_marker_is_appended_once_and_the_count_keeps_growing() {
        let buf = CapturedBytes::limited(4, b"[cut]");
        buf.append(b"abcd");
        assert!(!buf.overflowed());
        buf.append(b"ef");
        buf.append(b"gh");
        assert_eq!(buf.total(), 8);
        assert!(buf.overflowed());
        assert_eq!(buf.take(), b"abcd[cut]");
        assert!(buf.overflowed());
    }

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
        use std::io::Read;
        use std::sync::Arc;

        let (mut reader, writer) = crate::process::cloexec_pipe().expect("pipe");
        let mut sink = Sink::Pipe {
            writer: Arc::new(writer),
            edge: Edge::new(),
            stage: DurableRoot::default().worker().id(),
        };
        sink.write_under(&DurableRoot::default().worker(), b"hello")
            .expect("write");
        drop(sink);

        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).expect("read");
        assert_eq!(buf, b"hello");
    }

    /// A pipe sink over a fresh stage node, the read end, the edge and the
    /// writer `fill` can top up.
    #[cfg(unix)]
    fn pipe_sink(
        node: &Scope,
    ) -> (
        os_pipe::PipeReader,
        Sink,
        Arc<Edge>,
        Arc<os_pipe::PipeWriter>,
    ) {
        let (reader, writer) = crate::process::cloexec_pipe().expect("pipe");
        let (writer, edge) = (Arc::new(writer), Edge::new());
        let sink = Sink::Pipe {
            writer: writer.clone(),
            edge: edge.clone(),
            stage: node.id(),
        };
        (reader, sink, edge, writer)
    }

    /// Fill the pipe with raw writes, each of which poll promised would not block.
    #[cfg(unix)]
    fn fill(writer: &os_pipe::PipeWriter) {
        use rustix::event::PollFlags;
        while crate::io::poll_slice(writer, PollFlags::OUT)
            .expect("poll")
            .is_some()
        {
            (&*writer).write_all(&[0; libc::PIPE_BUF]).expect("fill");
        }
    }

    #[cfg(unix)]
    fn write_stopped(e: std::io::Error) -> WriteStopped {
        e.downcast::<WriteStopped>().expect("a WriteStopped")
    }

    #[cfg(unix)]
    #[test]
    fn a_write_past_a_dead_edge_is_the_stages_reader_gone_stop() {
        let node = DurableRoot::default().worker();
        let (_reader, mut sink, edge, _writer) = pipe_sink(&node);
        edge.mark_dead();
        let stop = write_stopped(sink.write_under(&node, b"x").expect_err("a dead edge"));
        assert_eq!(stop.cause, CancelCause::ReaderGone);
        assert_eq!(stop.owner, Some(node.id()));
    }

    #[cfg(unix)]
    #[test]
    fn a_write_blocked_under_a_cause_is_stopped_within_a_slice() {
        let (node, scope) = (
            DurableRoot::default().worker(),
            DurableRoot::default().worker(),
        );
        let (_reader, mut sink, _edge, writer) = pipe_sink(&node);
        fill(&writer);
        let canceller = scope.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            canceller.cancel(CancelCause::Interrupted);
        });
        let start = Instant::now();
        let stop = write_stopped(sink.write_under(&scope, b"x").expect_err("stopped"));
        assert_eq!(stop.cause, CancelCause::Interrupted);
        assert!(start.elapsed() < Duration::from_millis(120));
    }

    #[cfg(unix)]
    #[test]
    fn a_pump_blocked_on_a_full_edge_ends_within_a_slice() {
        let (node, scope) = (
            DurableRoot::default().worker(),
            DurableRoot::default().worker(),
        );
        let (_edge_reader, sink, _edge, writer) = pipe_sink(&node);
        fill(&writer);
        let (source, mut feed) = crate::process::cloexec_pipe().expect("source");
        let feeder =
            std::thread::spawn(move || feed.write_all(&vec![b'x'; 1 << 20]).expect("feed"));
        let pump = sink.pump(source, Scope::clone(&scope));
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            !pump.is_finished(),
            "the pump should be blocked on the full edge"
        );
        scope.cancel(CancelCause::Interrupted);
        let start = Instant::now();
        while !pump.is_finished() {
            assert!(
                start.elapsed() < Duration::from_millis(100),
                "the pump outlived its scope"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        pump.join().expect("pump");
        feeder.join().expect("the source drained to EOF");
    }

    #[cfg(unix)]
    #[test]
    fn a_write_under_a_shield_waits() {
        let node = DurableRoot::default().worker();
        let (mut reader, mut sink, _edge, writer) = pipe_sink(&node);
        node.cancel(CancelCause::Interrupted);
        let shield = node.shielded(Duration::from_secs(1));
        fill(&writer);
        let blocked = std::thread::spawn(move || sink.write_under(&shield, b"x"));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!blocked.is_finished(), "a shielded write should wait");
        reader.read_exact(&mut [0; libc::PIPE_BUF]).expect("drain");
        blocked.join().expect("writer").expect("ok once drained");
    }

    #[cfg(unix)]
    #[test]
    fn an_always_ready_pipe_still_polls_per_chunk() {
        let (node, scope) = (
            DurableRoot::default().worker(),
            DurableRoot::default().worker(),
        );
        let (mut reader, mut sink, ..) = pipe_sink(&node);
        let drain = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while reader.read(&mut buf).expect("read") > 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let writer_scope = scope.clone();
        let writer = std::thread::spawn(move || sink.write_under(&writer_scope, &vec![0; 1 << 20]));
        std::thread::sleep(Duration::from_millis(30));
        scope.cancel(CancelCause::Interrupted);
        let start = Instant::now();
        let stop = write_stopped(writer.join().expect("writer").expect_err("stopped"));
        assert_eq!(stop.cause, CancelCause::Interrupted);
        assert!(start.elapsed() < Duration::from_millis(100));
        drain.join().expect("drain");
    }

    #[cfg(unix)]
    #[test]
    fn a_full_dead_edge_stops_a_blocked_write_within_a_slice() {
        let node = DurableRoot::default().worker();
        let (_reader, mut sink, edge, writer) = pipe_sink(&node);
        node.cancel(CancelCause::Interrupted);
        let shield = node.shielded(Duration::from_secs(1));
        fill(&writer);
        edge.mark_dead();
        let start = Instant::now();
        let stop = write_stopped(sink.write_under(&shield, b"x").expect_err("stopped"));
        assert_eq!(
            (stop.cause, stop.owner),
            (CancelCause::ReaderGone, Some(node.id()))
        );
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[cfg(unix)]
    #[test]
    fn pipe_child_stdout_reaches_reader() {
        use std::io::Read;
        use std::sync::Arc;

        let (mut reader, writer) = crate::process::cloexec_pipe().expect("pipe");
        let sink = Sink::Pipe {
            writer: Arc::new(writer),
            edge: Edge::new(),
            stage: DurableRoot::default().worker().id(),
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
            edge: Edge::new(),
            stage: DurableRoot::default().worker().id(),
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
        // Empty flush before any bytes is a no-op.
        sink.flush_pending();
        sink.write_under(&DurableRoot::default().worker(), b"one\r\ntw")
            .unwrap();
        sink.write_under(&DurableRoot::default().worker(), b"o\nthree\nfour\r\nfive")
            .unwrap();
        sink.flush_pending();
        // Second flush after tail was already taken is a no-op.
        sink.flush_pending();
        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            [
                b"one".to_vec(),
                b"two".to_vec(),
                b"three".to_vec(),
                b"four".to_vec(),
                b"five".to_vec(),
            ]
        );
    }

    #[test]
    fn a_watch_callback_panic_discards_remaining_segments_and_does_not_retry() {
        let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let log = seen.clone();
        let mut sink = Sink::Watch {
            line: Arc::new(move |line| {
                log.lock().unwrap().push(line.to_vec());
                assert!(line != b"a", "watch line panic");
            }),
            pending: Vec::new(),
        };
        let node = DurableRoot::default().worker();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.write_under(&node, b"a\nb\nc")
        }));
        assert!(res.is_err(), "the first line's callback must panic");
        // Final flushing must neither retry `a` nor deliver `b\nc` as a tail.
        sink.flush_pending();
        assert_eq!(*seen.lock().unwrap(), [b"a".to_vec()]);
    }

    #[test]
    fn a_watch_partial_tail_panic_does_not_retry_on_second_flush() {
        let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let log = seen.clone();
        let mut sink = Sink::Watch {
            line: Arc::new(move |line| {
                log.lock().unwrap().push(line.to_vec());
                panic!("watch tail panic");
            }),
            pending: Vec::new(),
        };
        let node = DurableRoot::default().worker();
        sink.write_under(&node, b"tail").unwrap();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.flush_pending();
        }));
        assert!(res.is_err(), "tail flush must panic");
        // The pending buffer was taken before the callback, so a second flush is a no-op.
        sink.flush_pending();
        assert_eq!(*seen.lock().unwrap(), [b"tail".to_vec()]);
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
