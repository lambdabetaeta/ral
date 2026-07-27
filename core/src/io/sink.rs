//! Byte output for a pipeline stage.
//!
//! [`Sink`] enumerates everywhere a stage's bytes can land — terminal,
//! stderr, kernel pipe, redirect file, in-memory buffer, tee, frontend
//! printer, line-framed adapter — and is the single shape every writer routes
//! through.  [`ChildStdioPlan`] is its companion for child processes: the
//! `Stdio` to hand `Command::stdout`/`stderr` plus an optional pump sink the
//! caller drains after spawn.  The buffer primitives ([`new_buffer`],
//! [`tee_with_buffer`], [`take_buffer`], [`peek_buffer`]) are the sole
//! owners of the `Arc<Mutex<Vec<u8>>>` idiom for captured bytes.  The two
//! strip helpers ([`strip_trailing_newline`], [`str_strip_one_terminator`])
//! are plain byte/string trimmers over that captured output.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

/// Hard cap on in-memory `Sink::Buffer` growth.
///
/// Past this point we append a
/// one-line truncation marker and drop further bytes.  Chosen small relative
/// to disk so high-volume spawn / command-substitution captures push the user
/// toward an explicit redirect (`cmd > log`).  Enforced in `Write::write_all`
/// so both direct shell writes and pump-thread appends observe it.
const SINK_BUFFER_CAP: usize = 16 * 1024 * 1024;
const SINK_BUFFER_TRUNC_MARKER: &[u8] =
    b"\n[ral: buffer exceeded 16 MiB; remaining output dropped]\n";

/// Frontend-provided byte writer.
///
/// The REPL installs one of these at `shell.io.stdout` so every write — from
/// foreground `echo`, from pumped external commands, or from backgrounded
/// watched blocks — goes through rustyline's `ExternalPrinter`.  That keeps
/// output atomic with respect to the line editor: bytes arrive above the
/// active prompt rather than scribbling over it.
///
/// Implementations must be `Send + Sync` because a single `External` sink can
/// be cloned into many threads (each backgrounded watcher, each pump).
pub trait ExternalWrite: Send + Sync {
    /// Write `bytes` through the frontend printer.
    ///
    /// # Errors
    /// Returns `Err` if the underlying frontend write fails.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
}

/// In-memory byte buffer shared between Sink writers and the eventual
/// reader.
///
/// Always wrapped in `Arc<Mutex<…>>`: writers run on background
/// threads (`Sink::pump` workers, parallel pipeline stages), readers run
/// on the main eval thread once writers have joined.
pub type ByteBuffer = Arc<Mutex<Vec<u8>>>;

/// Joint plan for a child process's stdout *or* stderr: the `Stdio` to
/// hand `Command`, plus an optional `Sink` the caller must pump bytes
/// into after spawn.
///
/// `pump = None` means the child writes directly to the destination
/// (`inherit` or a `Pipe` end); the caller does not need to drain the
/// child fd.  `pump = Some(sink)` means the child writes to a piped fd
/// allocated by the kernel; the caller must take that fd after `spawn()`
/// and feed it to [`Sink::pump`] so the bytes reach `sink`.
///
/// Constructed only via [`Sink::child_stdout`] / [`Sink::child_stderr`]
/// or the small inherent constructors here, so the (stdio, pump)
/// invariant is centralised — no caller computes "inherit, pipe, pump,
/// tee" by hand.
pub struct ChildStdioPlan {
    pub stdio: crate::process::StdioSpec,
    pub pump: Option<Sink>,
}

impl ChildStdioPlan {
    /// The child reads/writes the parent's matching fd directly.
    pub fn inherit() -> Self {
        Self {
            stdio: crate::process::StdioSpec::inherit(),
            pump: None,
        }
    }

    /// Build the plan for a non-inherited routing of `sink`.  A `Pipe(w)`
    /// becomes the child's fd directly; everything else is piped and the
    /// pump field carries a clone of the sink.
    fn for_sink(sink: &Sink) -> io::Result<Self> {
        match sink {
            Sink::Pipe(w) => Ok(Self {
                stdio: crate::process::StdioSpec::from_pipe_writer(w.try_clone()?),
                pump: None,
            }),
            other => Ok(Self {
                stdio: crate::process::StdioSpec::piped(),
                pump: Some(other.try_clone()?),
            }),
        }
    }
}

/// Where a pipeline stage's byte output goes.
pub enum Sink {
    /// The shell's inherited stdout (fd 1 at process start).  Whether it is
    /// actually a terminal is recorded in `TerminalState::startup_stdout_tty`.
    Terminal,
    /// The shell's inherited stderr (fd 2).  Used when --audit reserves
    /// stdout for JSON and the user still wants to see command output.
    Stderr,
    /// Byte pipe into the next pipeline stage or sandbox subprocess.
    Pipe(os_pipe::PipeWriter),
    /// Open file target for a shell-level redirect frame.
    File(std::fs::File),
    /// In-memory buffer used by command substitution (`let x = cmd`).
    Buffer(ByteBuffer),
    /// Duplicate bytes to both A and B in turn.
    Tee(Box<Self>, Box<Self>),
    /// Frontend-provided sink, typically rustyline's external printer.
    /// Used by the interactive REPL so background-thread output does not
    /// clobber the active prompt.
    External(Arc<dyn ExternalWrite>),
    /// Line-framing adapter: buffer bytes up to the next `\n`, then emit
    /// `prefix + line + '\n'` to `inner` as one write.  Used by `watch` so
    /// backgrounded output arrives on the caller's stdout prefixed and
    /// line-atomic without a global multiplexer.  `pending` carries a partial
    /// line across writes; `flush_pending` emits whatever is left at thread
    /// teardown.
    LineFramed {
        inner: Box<Self>,
        prefix: String,
        pending: Vec<u8>,
    },
}

impl Sink {
    /// Flush any buffered partial line.  No-op for every variant except
    /// `LineFramed`, which may hold a tail of bytes without a terminating
    /// newline.  Called at the end of a watched block's lifetime so the last
    /// line is not silently dropped.
    ///
    /// # Errors
    /// Returns `Err` if writing the buffered tail to the inner sink fails, or
    /// if either branch of a `Tee` fails to flush.
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

    /// Plan to route a child process's stdout into this sink.
    ///
    /// `inherit_tty=true` widens the "no pump needed" set to sinks that
    /// ultimately resolve to a real fd (Terminal, External, Stderr): the
    /// child gets the parent's fd 1 directly via `Stdio::inherit()`, which
    /// is the only way it sees a TTY.  `Sink::Terminal` always inherits
    /// regardless of `inherit_tty`.  Under the conservative default
    /// (`inherit_tty=false`) the pump is skipped only for a direct `Pipe`
    /// and for `Terminal`; everything else is pumped.
    ///
    /// Callers MUST consume the returned plan in two steps: assign
    /// `plan.stdio` to `cmd.stdout(...)` before spawn, then — after spawn
    /// — if `plan.pump` is `Some(sink)`, take `child.stdout` and call
    /// `sink.pump(stdout)`.  The two-method asymmetry between stdout and
    /// stderr lives here in [`ChildStdioPlan`] alone, so the rest of the
    /// codebase routes through one shape.
    ///
    /// # Errors
    /// Returns `Err` if duplicating the pipe writer (for a direct `Pipe`) or
    /// the sink to be pumped fails.
    pub fn child_stdout(&self, inherit_tty: bool) -> io::Result<ChildStdioPlan> {
        // Sinks already targeting fd 1 inherit directly when the caller
        // has verified TTY ownership.  `Sink::Stderr` here means "swap fd 1
        // for fd 2" (audit mode); inheriting is correct only when the
        // caller knows the child won't try to re-grab the TTY from fd 1.
        if matches!(self, Self::Terminal)
            || (inherit_tty && matches!(self, Self::Stderr | Self::External(_)))
        {
            return Ok(ChildStdioPlan::inherit());
        }
        ChildStdioPlan::for_sink(self)
    }

    /// Plan to route a child process's stderr into this sink.
    ///
    /// Default-inherit for `Sink::Stderr` (the natural fd 2 target); other
    /// sinks drain via a pump.  No `inherit_tty` parameter: stderr never
    /// owns the TTY in any pipeline shape.
    ///
    /// # Errors
    /// Returns `Err` if duplicating the pipe writer (for a direct `Pipe`) or
    /// the sink to be pumped fails.
    pub fn child_stderr(&self) -> io::Result<ChildStdioPlan> {
        if matches!(self, Self::Stderr) {
            return Ok(ChildStdioPlan::inherit());
        }
        ChildStdioPlan::for_sink(self)
    }

    /// Spawn a background thread that reads `reader` to EOF and writes all
    /// bytes to this sink.
    ///
    /// To capture output, pass a `Sink::Buffer(buf)` or a
    /// `Sink::Tee(Box::new(Sink::Buffer(buf)), Box::new(other))`.
    /// The caller reads from `buf` after joining the returned handle.
    pub fn pump(self, reader: impl Read + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut sink = self;
            let _ = io::copy(&mut { reader }, &mut sink);
            let _ = sink.flush_pending();
        })
    }

    /// Write this sink's buffered bytes to `target`, then clear the buffer.
    /// No-op when `self` is not a `Buffer`.
    ///
    /// Used by `Comp::Seq` to route non-final commands' byte output to the
    /// outer (visible) stdout when running inside a capture context (§4.3).
    ///
    /// # Errors
    /// Returns `Err` if duplicating `target` fails, or if writing the drained
    /// bytes to it fails (the drained bytes are restored to the buffer first),
    /// or if flushing the clone's pending tail fails.
    pub fn flush_to(&self, target: &Self) -> io::Result<()> {
        if let Self::Buffer(buf) = self
            && let Ok(mut g) = buf.lock()
            && !g.is_empty()
        {
            let mut t = target.try_clone()?;
            let bytes = std::mem::take(&mut *g);
            drop(g);
            if let Err(e) = t.write_all(&bytes) {
                // Restore the drained bytes so a later `take_buffer` still
                // recovers them; the lock is re-acquired only after the
                // failed write, never held across the blocking IO.
                if let Ok(mut g) = buf.lock() {
                    let tail = std::mem::take(&mut *g);
                    *g = bytes;
                    g.extend(tail);
                }
                return Err(e);
            }
            // `t` is a fresh clone about to be dropped; if `target` is
            // `LineFramed`, its own pending tail must be flushed here — a
            // clone starts with empty `pending`, so `Drop` alone would lose
            // a non-`\n`-terminated tail forever.
            t.flush_pending()?;
        }
        Ok(())
    }

    /// Duplicate this sink, sharing the same ultimate destination.
    ///
    /// # Errors
    /// Returns `Err` if duplicating the underlying file descriptor of a
    /// `Pipe`, `File`, `Tee` branch, or `LineFramed` inner sink fails.
    pub fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Terminal => Ok(Self::Terminal),
            Self::Stderr => Ok(Self::Stderr),
            Self::Pipe(w) => Ok(Self::Pipe(w.try_clone()?)),
            Self::File(f) => Ok(Self::File(f.try_clone()?)),
            Self::Buffer(b) => Ok(Self::Buffer(b.clone())),
            Self::Tee(a, b) => Ok(Self::Tee(
                Box::new(a.try_clone()?),
                Box::new(b.try_clone()?),
            )),
            Self::External(w) => Ok(Self::External(w.clone())),
            Self::LineFramed { inner, prefix, .. } => Ok(Self::LineFramed {
                inner: Box::new(inner.try_clone()?),
                prefix: prefix.clone(),
                // Each cloned `LineFramed` owns its own partial-line carry —
                // two threads writing "part of a line" concurrently must not
                // interleave their bytes via shared `pending`.
                pending: Vec::new(),
            }),
        }
    }
}

/// Wrap `base` in a `Sink::Tee` whose other branch is a fresh in-memory
/// buffer.  Bytes written to the returned sink go to *both* `base` and the
/// buffer; the caller holds the buffer arc and drains it after the writers
/// have all closed (typically post-wait, post-pump-join).
///
/// One primitive used at every per-command capture site: pipeline-stage
/// stdout, pipeline-stage stderr, and (for symmetry with the let-binding
/// path) anywhere else that needs "see the bytes AND keep them visible."
/// Standalone externals do *not* call this — their bytes are captured at
/// dispatch level by `evaluator::with_audit_capture`, which tees
/// `shell.io.stdout`/`stderr` themselves.
pub(crate) fn tee_with_buffer(base: Sink) -> (Sink, ByteBuffer) {
    let (buffer_sink, buf) = new_buffer();
    (Sink::Tee(Box::new(buffer_sink), Box::new(base)), buf)
}

/// Allocate a fresh [`ByteBuffer`] paired with a [`Sink::Buffer`] that
/// writes into it.  Two-arity return because every caller wants the sink
/// to wire onto `shell.io.{stdout,stderr}` *and* the buffer arc to drain
/// from later.  Sole owner of the `Arc::new(Mutex::new(Vec::new()))`
/// idiom — every `ByteBuffer` allocation in the crate goes through here.
pub(crate) fn new_buffer() -> (Sink, ByteBuffer) {
    let buf: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
    (Sink::Buffer(buf.clone()), buf)
}

/// Drain a [`ByteBuffer`] into an owned `Vec<u8>`.  Returns an empty vector
/// if the mutex is poisoned — callers reading captured bytes after writers
/// have joined cannot meaningfully recover from a poisoned lock, and an
/// empty payload is strictly more useful than a panic propagating through
/// `await` / `audit` result construction.
pub(crate) fn take_buffer(buf: &ByteBuffer) -> Vec<u8> {
    buf.lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// Clone a [`ByteBuffer`]'s current contents without draining it — the
/// non-destructive peer of [`take_buffer`].  Returns an empty vector on a
/// poisoned lock, for the same reason `take_buffer` does.
///
/// Used by `poll`'s `` `pending `` arm to sample a *still-running* worker's
/// accumulated stdout/stderr.  Because the bytes stay in the buffer, the
/// one-shot completion drain (`take_buffer` in `complete_handle`) still
/// observes the full output — "bytes leave the buffer exactly once" is
/// preserved.  The flip side is that repeated peeks of a running worker
/// grow monotonically and are not idempotent; see
/// `decisions/260702_partial-poll-pending-output`.
pub(crate) fn peek_buffer(buf: &ByteBuffer) -> Vec<u8> {
    buf.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Strip a single trailing line terminator from `buf`, mirroring POSIX
/// `$()` semantics: command substitution drops one terminating newline
/// so `let x = echo hi` binds `"hi"` rather than `"hi\n"`.  Handles
/// CRLF (`\r\n`) as well as LF (`\n`) so Windows tools like
/// `hostname.EXE`, which emit CRLF, don't leave a bare `\r` behind that
/// would corrupt downstream rendering (the `\r` returns the cursor to
/// column 0 when the captured value is interpolated into a prompt).
/// A lone trailing `\r` is preserved — it isn't a line terminator on
/// its own on any platform we support, and discarding it would damage
/// intentional content.  No-op when the buffer is empty or doesn't end
/// in a recognised terminator.  Safe to apply to bytes before UTF-8
/// decoding — neither `\n` nor `\r` appears mid-codepoint.
pub(crate) fn strip_trailing_newline(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
}

/// String peer of [`strip_trailing_newline`]: `s` without a single trailing
/// line terminator (`\r\n` or `\n`).  A lone `\r` is preserved, exactly as in
/// the byte version — the two share one CRLF/LF/lone-CR rule so a future
/// change can't drift between the byte path and the string callers
/// (`from-line`, `ask`).  No-op when `s` ends in no recognised terminator.
pub(crate) fn str_strip_one_terminator(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

/// Append `bytes` to `buf`, enforcing `SINK_BUFFER_CAP`.
///
/// Once the cap is reached, a one-line truncation marker is appended and
/// further bytes are dropped.  Called only from `Sink::Buffer`'s
/// `Write::write_all` arm so the policy lives in one place.
fn write_capped(buf: &Mutex<Vec<u8>>, bytes: &[u8]) {
    if let Ok(mut g) = buf.lock() {
        let cur = g.len();
        if cur < SINK_BUFFER_CAP + SINK_BUFFER_TRUNC_MARKER.len() {
            if cur + bytes.len() <= SINK_BUFFER_CAP {
                g.extend_from_slice(bytes);
            } else {
                g.extend_from_slice(&bytes[..SINK_BUFFER_CAP.saturating_sub(cur)]);
                g.extend_from_slice(SINK_BUFFER_TRUNC_MARKER);
            }
        }
    }
}

/// Emit `prefix + line + '\n'` as one write to `inner`.
///
/// Shared by `Write::write_all` (mid-stream lines) and `flush_pending`
/// (the unterminated tail).
fn emit_framed(inner: &mut Sink, prefix: &str, line: &[u8]) -> io::Result<()> {
    inner.write_all(&[prefix.as_bytes(), line, b"\n"].concat())
}

impl Write for Sink {
    /// Write `buf` to this sink.  Always consumes the full slice or returns
    /// an error — partial writes do not occur on any in-memory variant.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Terminal => io::stdout().write_all(bytes),
            Self::Stderr => io::stderr().write_all(bytes),
            Self::Pipe(w) => w.write_all(bytes),
            Self::File(f) => f.write_all(bytes),
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
                // Buffer until newline, then emit `prefix + line + '\n'` as a
                // single write to `inner`.  Multiple writes to the same
                // underlying fd are serialised by the OS stdout lock (for
                // Terminal) or by the External adapter's internal mutex, so
                // each line appears atomically regardless of sibling
                // watchers or concurrent parent-thread output.
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

    /// Mid-stream sync to the underlying fd.  `Buffer` and `External` have
    /// nothing to sync; `LineFramed` deliberately does *not* drain its
    /// pending tail here — that's an end-of-stream concern handled by
    /// [`Sink::flush_pending`].  Flushing a partial line as if it were
    /// complete would emit `prefix + half_line + '\n'` and frame the next
    /// chunk as a new line, breaking the line-atomicity guarantee.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Terminal => io::stdout().flush(),
            Self::Stderr => io::stderr().flush(),
            Self::Pipe(w) => w.flush(),
            Self::File(f) => f.flush(),
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

    /// The string peer must agree with the byte version on every shape, so
    /// the CRLF/LF/lone-CR rule stays single-sourced.
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
        // A bare trailing `\r` is not a line terminator we recognise —
        // strip would damage intentional content.
        assert_eq!(strip(b"hi\r"), b"hi\r");
    }

    #[test]
    fn strips_exactly_one_lf() {
        // Matches existing semantics: only the final terminator is dropped.
        assert_eq!(strip(b"hi\n\n"), b"hi\n");
    }

    #[test]
    fn strips_exactly_one_crlf() {
        assert_eq!(strip(b"hi\r\n\r\n"), b"hi\r\n");
    }
}
