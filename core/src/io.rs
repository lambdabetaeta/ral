//! Unified stream plumbing.
//!
//! [`Sink`] and [`Source`] carry the destination or origin for a pipeline
//! stage's byte I/O.  [`TerminalState`] caches the shell's entry-time isatty
//! results so downstream code can ask "is the user's terminal in the loop?"
//! without repeating the syscall.  [`Io`] groups these together with the
//! other pipeline-stage IO flags that used to live as loose fields on Shell.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

// ── TerminalState ─────────────────────────────────────────────────────────

/// Operating mode for the interactive frontend, resolved from
/// `RAL_INTERACTIVE_MODE` at shell startup.
///
/// `Auto` is the default: capability bits drive feature gating.
/// `Minimal` forces every terminal round-trip and every ANSI emission off.
/// `Full` forces ANSI on even when capability detection says otherwise
/// (useful when piping ral into a wrapper that understands ANSI).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InteractiveMode {
    #[default]
    Auto,
    Minimal,
    Full,
}

impl InteractiveMode {
    /// Parse the `RAL_INTERACTIVE_MODE` value.  Unknown values fall back to
    /// `Auto` and set `warn` so the caller can emit a one-time diagnostic.
    pub fn parse(raw: Option<&str>) -> (Self, Option<String>) {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("auto") => (Self::Auto, None),
            Some("minimal") | Some("dumb") | Some("plain") => (Self::Minimal, None),
            Some("full") => (Self::Full, None),
            Some(other) => (
                Self::Auto,
                Some(format!(
                    "unknown RAL_INTERACTIVE_MODE '{other}', using auto"
                )),
            ),
        }
    }

    /// True when the mode suppresses all terminal output, round-trips, and ANSI.
    pub fn is_minimal(self) -> bool {
        matches!(self, Self::Minimal)
    }
}

/// Cached terminal capability snapshot, taken once at shell start.
///
/// `stdin_tty` / `stdout_tty` / `stderr_tty` are the raw `isatty(3)` results
/// for fds 0/1/2.  The remaining fields record whether the terminal is known
/// to accept ANSI escape sequences and which "hostile but common" environment
/// we are running inside.  Population happens once via
/// `TerminalState::probe_with_mode`; nothing re-queries the OS mid-session.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalState {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    /// `true` when stdout is a tty *and* TERM/platform checks say ANSI works.
    pub supports_ansi: bool,
    /// `NO_COLOR` is set in the environment.
    pub no_color: bool,
    /// Running inside a tmux session.
    pub is_tmux: bool,
    /// Running under asciinema recording.
    pub is_asciinema: bool,
    /// Running in a CI environment (GitHub Actions, GitLab CI, etc.).
    pub is_ci: bool,
    /// Resolved from `RAL_INTERACTIVE_MODE`.
    pub mode: InteractiveMode,
}

impl TerminalState {
    /// Back-compat entry point: probe with `InteractiveMode::Auto`.
    pub fn probe() -> Self {
        Self::probe_with_mode(InteractiveMode::Auto)
    }

    /// Query the OS and environment for the current terminal state.
    pub fn probe_with_mode(mode: InteractiveMode) -> Self {
        #[cfg(unix)]
        let (stdin_tty, stdout_tty, stderr_tty) = {
            use std::io::IsTerminal;
            (
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
                std::io::stderr().is_terminal(),
            )
        };
        #[cfg(windows)]
        let (stdin_tty, stdout_tty, stderr_tty) = (
            crate::compat::is_console(crate::compat::STD_INPUT_HANDLE),
            crate::compat::is_console(crate::compat::STD_OUTPUT_HANDLE),
            crate::compat::is_console(crate::compat::STD_ERROR_HANDLE),
        );
        #[cfg(not(any(unix, windows)))]
        let (stdin_tty, stdout_tty, stderr_tty) = (false, false, false);

        let no_color = anstyle_query::no_color();
        let is_ci = anstyle_query::is_ci();
        let is_tmux = std::env::var_os("TMUX").is_some();
        let is_asciinema = std::env::var_os("ASCIINEMA_REC").is_some();

        // `Full` forces ANSI even on a piped stdout; `Minimal` forces it off.
        // Otherwise defer to anstyle-query + isatty.
        let supports_ansi = match mode {
            InteractiveMode::Full => true,
            InteractiveMode::Minimal => false,
            InteractiveMode::Auto => stdout_tty && anstyle_query::term_supports_ansi_color(),
        };

        TerminalState {
            stdin_tty,
            stdout_tty,
            stderr_tty,
            supports_ansi,
            no_color,
            is_tmux,
            is_asciinema,
            is_ci,
            mode,
        }
    }

    /// UI may emit styling.  False under NO_COLOR, TERM=dumb, non-tty, or
    /// `RAL_INTERACTIVE_MODE=minimal`.
    pub fn ui_ansi_ok(&self) -> bool {
        !self.mode.is_minimal() && self.supports_ansi && !self.no_color
    }

    /// Terminal round-trip queries (CPR, DA, OSC) are appropriate.  False on
    /// non-tty stdout or in minimal mode.
    pub fn ui_round_trips_ok(&self) -> bool {
        self.stdout_tty && !self.mode.is_minimal()
    }

    /// Terminal title may be set via OSC 0/2 sequences.
    pub fn ui_title_ok(&self) -> bool {
        self.ui_round_trips_ok()
    }

    /// Diagnostics (stderr) may emit ANSI.  Independent of `ui_ansi_ok`
    /// because stderr may be a tty while stdout is piped to a pager; we still
    /// want colored errors in that case.  False under NO_COLOR, TERM=dumb on
    /// Auto, non-tty stderr, or minimal mode.
    pub fn stderr_ansi_ok(&self) -> bool {
        !self.mode.is_minimal()
            && !self.no_color
            && self.stderr_tty
            && (matches!(self.mode, InteractiveMode::Full)
                || anstyle_query::term_supports_ansi_color())
    }
}

// ── Sink ──────────────────────────────────────────────────────────────────

/// Hard cap on in-memory `Sink::Buffer` growth.  Past this point we append a
/// one-line truncation marker and drop further bytes.  Chosen small relative
/// to disk so high-volume spawn / command-substitution captures push the user
/// toward an explicit redirect (`cmd > log`).  Enforced in `Write::write_all`
/// so both direct shell writes and pump-thread appends observe it.
pub const SINK_BUFFER_CAP: usize = 16 * 1024 * 1024;
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
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
}

/// Where a pipeline stage's byte output goes.
pub enum Sink {
    /// The shell's inherited stdout (fd 1 at process start).  Whether it is
    /// actually a terminal is recorded in `TerminalState::stdout_tty`.
    Terminal,
    /// The shell's inherited stderr (fd 2).  Used when --audit reserves
    /// stdout for JSON and the user still wants to see command output.
    Stderr,
    /// Byte pipe into the next pipeline stage or sandbox subprocess.
    Pipe(os_pipe::PipeWriter),
    /// In-memory buffer used by command substitution (`let x = cmd`).
    Buffer(Arc<Mutex<Vec<u8>>>),
    /// Duplicate bytes to both A and B in turn.
    Tee(Box<Sink>, Box<Sink>),
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
        inner: Box<Sink>,
        prefix: String,
        pending: Vec<u8>,
    },
}

impl Sink {
    /// Flush any buffered partial line.  No-op for every variant except
    /// `LineFramed`, which may hold a tail of bytes without a terminating
    /// newline.  Called at the end of a watched block's lifetime so the last
    /// line is not silently dropped.
    pub fn flush_pending(&mut self) -> io::Result<()> {
        match self {
            Sink::LineFramed {
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
            Sink::Tee(a, b) => {
                a.flush_pending()?;
                b.flush_pending()
            }
            _ => Ok(()),
        }
    }

    /// Produce a `Stdio` for `Command::stdout`.
    ///
    /// For sinks that require the shell to read child bytes (all variants
    /// except Terminal and Pipe), returns `Stdio::piped()`.  The caller must
    /// drain the child's piped stdout via `Sink::pump`.
    pub fn as_stdio(&self) -> io::Result<std::process::Stdio> {
        use std::process::Stdio;
        match self {
            Sink::Terminal => Ok(Stdio::inherit()),
            Sink::Pipe(w) => Ok(Stdio::from(w.try_clone()?)),
            _ => Ok(Stdio::piped()),
        }
    }

    /// True when bytes from a piped child stdout must be forwarded to this
    /// sink via a pump thread (i.e. `as_stdio` returns `Stdio::piped()`).
    pub fn needs_pump(&self) -> bool {
        !matches!(self, Sink::Terminal | Sink::Pipe(_))
    }

    /// Set `command.stdout` for this sink and report whether a pump thread
    /// will be needed after `spawn()`.
    ///
    /// `inherit_tty` should be true when the shell's real TTY fd can be given
    /// to the child directly — typically in non-audit interactive mode with a
    /// Terminal sink.  When true the child inherits fd 1 and no pump is needed.
    ///
    /// Returns `Ok(true)` when a pump is needed; the caller should take
    /// `child.stdout` after `spawn()` and call `sink.pump(stdout, capture)`.
    pub fn wire_command_stdout(
        &self,
        cmd: &mut std::process::Command,
        inherit_tty: bool,
    ) -> io::Result<bool> {
        use std::process::Stdio;
        // Inheriting the TTY is only safe when this sink targets the real
        // fd 1: `Terminal` (the inherited stdout) and `External` (REPL's
        // rustyline printer, which also writes to fd 1) both qualify.
        // `LineFramed` (watched block) must *not* inherit: its bytes need
        // to be prefixed and line-atomic, so the child must stay piped.
        if inherit_tty && matches!(self, Sink::Terminal | Sink::Stderr | Sink::External(_)) {
            cmd.stdout(Stdio::inherit());
            return Ok(false);
        }
        match self {
            Sink::Pipe(w) => {
                cmd.stdout(Stdio::from(w.try_clone()?));
                Ok(false)
            }
            _ => {
                cmd.stdout(Stdio::piped());
                Ok(true)
            }
        }
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
    pub fn flush_to(&self, target: &Sink) -> io::Result<()> {
        if let Sink::Buffer(buf) = self
            && let Ok(mut g) = buf.lock()
            && !g.is_empty()
        {
            let bytes = std::mem::take(&mut *g);
            drop(g);
            let mut t = target.try_clone()?;
            t.write_all(&bytes)?;
        }
        Ok(())
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        match self {
            Sink::Terminal => Ok(Sink::Terminal),
            Sink::Stderr => Ok(Sink::Stderr),
            Sink::Pipe(w) => Ok(Sink::Pipe(w.try_clone()?)),
            Sink::Buffer(b) => Ok(Sink::Buffer(b.clone())),
            Sink::Tee(a, b) => Ok(Sink::Tee(
                Box::new(a.try_clone()?),
                Box::new(b.try_clone()?),
            )),
            Sink::External(w) => Ok(Sink::External(w.clone())),
            Sink::LineFramed { inner, prefix, .. } => Ok(Sink::LineFramed {
                inner: Box::new(inner.try_clone()?),
                prefix: prefix.clone(),
                // Each cloned `LineFramed` owns its own partial-line carry —
                // two threads writing "part of a line" concurrently must not
                // interleave their bytes via shared `pending`.
                pending: Vec::new(),
            }),
        }
    }

    /// Run `f` with fd 1 pointing at this sink, then restore.
    ///
    /// Used for in-process uutils execution, which calls `isatty(1)` to decide
    /// whether to emit colour.  `Terminal` calls `f` directly so that uutils
    /// sees the real terminal fd.  Other variants redirect fd 1 via dup2.
    #[cfg(any(feature = "coreutils", feature = "diffutils"))]
    pub fn with_child_stdout(&mut self, f: impl FnOnce() -> i32) -> i32 {
        // Redirect fd 1 into a pipe, run `f`, drain the pipe.  Falls back to
        // running `f` directly when the pipe cannot be created.
        fn pipe_capture(f: impl FnOnce() -> i32) -> Option<(i32, Vec<u8>)> {
            let (mut reader, writer) = os_pipe::pipe().ok()?;
            let code = crate::compat::with_stdout_redirected(&writer, f);
            drop(writer);
            let mut out = Vec::new();
            let _ = reader.read_to_end(&mut out);
            Some((code, out))
        }

        match self {
            // `External` is installed only when the REPL holds a real TTY,
            // so fd 1 is already a terminal: let uutils see it directly.
            Sink::Terminal | Sink::External(_) => f(),
            Sink::Pipe(w) => crate::compat::with_stdout_redirected(w, f),
            _ => match pipe_capture(f) {
                None => -1,
                Some((code, out)) => {
                    let _ = self.write_all(&out);
                    let _ = self.flush_pending();
                    code
                }
            },
        }
    }
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
            Sink::Terminal => io::stdout().write_all(bytes),
            Sink::Stderr => io::stderr().write_all(bytes),
            Sink::Pipe(w) => w.write_all(bytes),
            Sink::Buffer(b) => {
                write_capped(b, bytes);
                Ok(())
            }
            Sink::Tee(a, b) => {
                a.write_all(bytes)?;
                b.write_all(bytes)
            }
            Sink::External(w) => w.write(bytes),
            Sink::LineFramed {
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

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Terminal => io::stdout().flush(),
            Sink::Stderr => io::stderr().flush(),
            Sink::Pipe(w) => w.flush(),
            Sink::Tee(a, b) => {
                a.flush()?;
                b.flush()
            }
            _ => Ok(()),
        }
    }
}

// ── Source ────────────────────────────────────────────────────────────────

/// Where a pipeline stage's byte input comes from.
pub enum Source {
    /// The shell's inherited stdin (fd 0 at process start).
    Terminal,
    /// Byte pipe from the previous pipeline stage.
    Pipe(os_pipe::PipeReader),
}

impl Source {
    /// Consume the pipe reader, replacing `self` with `Terminal`.
    /// Returns `None` when already `Terminal`.
    pub fn take_pipe(&mut self) -> Option<os_pipe::PipeReader> {
        match std::mem::replace(self, Source::Terminal) {
            Source::Pipe(r) => Some(r),
            Source::Terminal => None,
        }
    }
}

// ── Io ────────────────────────────────────────────────────────────────────

/// Whether the current shell context may hand the controlling terminal
/// to a spawned external child.
///
/// Constructed only via the named methods so the discipline is grep-able:
/// the orchestrator (top-level call, single-command exec) issues
/// `Eligible`; pipeline-internal stage threads issue `Forbidden`.  An
/// internal stage runs inside ral itself, so `tcsetpgrp` from the thread
/// races with the orchestrator and can leave the tty handed to a child
/// that the orchestrator's `claim_foreground` knows nothing about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobControl {
    foreground_eligible: bool,
}

impl JobControl {
    /// The orchestrator (top-level eval, single-command exec).  May
    /// foreground a spawned child when other conditions are met
    /// (interactive shell, tty stdin, terminal stdout, no shell pump).
    pub fn top_level() -> Self {
        Self {
            foreground_eligible: true,
        }
    }

    /// Pipeline-internal stage thread.  Must NEVER take foreground —
    /// the orchestrator owns that decision.
    pub fn pipeline_thread() -> Self {
        Self {
            foreground_eligible: false,
        }
    }

    pub fn may_foreground(&self) -> bool {
        self.foreground_eligible
    }
}

impl Default for JobControl {
    fn default() -> Self {
        Self::top_level()
    }
}

/// All pipeline-stage IO state for a single Shell.
pub struct Io {
    /// Byte source for this stage.
    pub stdin: Source,
    /// Byte sink for this stage.
    pub stdout: Sink,
    /// Byte sink for this stage's stderr.  Defaults to `Sink::Stderr`.
    /// Spawned handles install a `Sink::Buffer` here so errors are buffered
    /// in the handle and replayed on `await` (§13.3 replay rule).
    pub stderr: Sink,
    /// Structured value piped from the previous internal pipeline stage.
    pub value_in: Option<crate::types::Value>,
    /// True when the shell is running as an interactive REPL.
    pub interactive: bool,
    /// Cached isatty results from shell startup.
    pub terminal: TerminalState,
    /// Whether this shell context may take terminal foreground.  `top_level`
    /// for orchestrator paths; `pipeline_thread` inside an internal pipeline
    /// stage thread (set by `launch_internal_stage`).  Independent of the
    /// `interactive`/`terminal` checks: those describe the *capability*, this
    /// describes whether *this caller* is permitted to use it.
    pub job_control: JobControl,
    /// The stdout that was active before the current `with_capture` installed
    /// its buffer.  `Comp::Seq` flushes non-final commands' bytes here so
    /// side-effects remain visible rather than being silently discarded.
    /// `None` when not inside a capture context.
    pub capture_outer: Option<Sink>,
}

impl Io {
    /// Clone the Io state for a child thread.
    ///
    /// `stdin` is not propagated: it is a read-once resource consumed by the
    /// child that spawns it.  The caller must set `child.io.stdin` explicitly.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Io {
            stdin: Source::Terminal,
            stdout: self.stdout.try_clone()?,
            stderr: self.stderr.try_clone()?,
            value_in: self.value_in.clone(),
            interactive: self.interactive,
            terminal: self.terminal,
            job_control: self.job_control,
            capture_outer: self
                .capture_outer
                .as_ref()
                .map(Sink::try_clone)
                .transpose()?,
        })
    }

    /// Install Io state from `parent` into `self` for a same-thread child shell
    /// (thunk body, `try`, `_audit`, …).  Bytes sinks are `try_clone`d; the
    /// pipe stdin and structured `value_in` are *moved* out of the parent so
    /// the child consumes them once.  `try_clone` failure collapses to the
    /// default terminal sink — the parent's FDs are already gone by then, and
    /// silent `Inherit` would re-open the sandbox-bypass class.
    pub fn install_from_parent(&mut self, parent: &mut Io) {
        self.stdout = parent.stdout.try_clone().unwrap_or(Sink::Terminal);
        self.stderr = parent.stderr.try_clone().unwrap_or(Sink::Stderr);
        self.capture_outer = parent
            .capture_outer
            .as_ref()
            .and_then(|s| s.try_clone().ok());
        self.terminal = parent.terminal;
        self.interactive = parent.interactive;
        self.job_control = parent.job_control;
        self.stdin = match parent.stdin.take_pipe() {
            Some(r) => Source::Pipe(r),
            None => Source::Terminal,
        };
        self.value_in = parent.value_in.take();
    }

    /// Mirror of `install_from_parent`: return the read-once resources the
    /// child received back to `parent`.  Called from `Shell::return_to` so
    /// subsequent sibling calls in the parent see the unconsumed pipe.
    pub fn return_to_parent(&mut self, child: &mut Io) {
        self.stdin = match child.stdin.take_pipe() {
            Some(r) => Source::Pipe(r),
            None => Source::Terminal,
        };
        self.value_in = child.value_in.take();
    }
}

impl Default for Io {
    fn default() -> Self {
        Io {
            stdin: Source::Terminal,
            stdout: Sink::Terminal,
            stderr: Sink::Stderr,
            value_in: None,
            interactive: false,
            terminal: TerminalState::default(),
            job_control: JobControl::default(),
            capture_outer: None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_mode_parse() {
        let parse = |s| InteractiveMode::parse(s).0;
        assert_eq!(parse(None), InteractiveMode::Auto);
        assert_eq!(parse(Some("")), InteractiveMode::Auto);
        assert_eq!(parse(Some("auto")), InteractiveMode::Auto);
        assert_eq!(parse(Some("AUTO")), InteractiveMode::Auto);
        assert_eq!(parse(Some("  full ")), InteractiveMode::Full);
        assert_eq!(parse(Some("minimal")), InteractiveMode::Minimal);
        assert_eq!(parse(Some("dumb")), InteractiveMode::Minimal);
        let (mode, warn) = InteractiveMode::parse(Some("bogus"));
        assert_eq!(mode, InteractiveMode::Auto);
        assert!(warn.is_some());
    }

    fn with_state(
        mode: InteractiveMode,
        supports_ansi: bool,
        no_color: bool,
        stdout_tty: bool,
    ) -> TerminalState {
        TerminalState {
            stdin_tty: stdout_tty,
            stdout_tty,
            stderr_tty: stdout_tty,
            supports_ansi,
            no_color,
            is_tmux: false,
            is_asciinema: false,
            is_ci: false,
            mode,
        }
    }

    #[test]
    fn ui_ansi_ok_gates() {
        // Auto mode, everything good → ok.
        assert!(with_state(InteractiveMode::Auto, true, false, true).ui_ansi_ok());
        // NO_COLOR blocks it.
        assert!(!with_state(InteractiveMode::Auto, true, true, true).ui_ansi_ok());
        // No ANSI support blocks it.
        assert!(!with_state(InteractiveMode::Auto, false, false, true).ui_ansi_ok());
        // Minimal mode blocks everything.
        assert!(!with_state(InteractiveMode::Minimal, true, false, true).ui_ansi_ok());
        // Full mode still respects NO_COLOR (user intent overrides force).
        assert!(!with_state(InteractiveMode::Full, true, true, true).ui_ansi_ok());
    }

    #[test]
    fn ui_round_trips_ok_gates() {
        assert!(with_state(InteractiveMode::Auto, true, false, true).ui_round_trips_ok());
        // Non-tty blocks CPR.
        assert!(!with_state(InteractiveMode::Auto, true, false, false).ui_round_trips_ok());
        // Minimal mode blocks CPR even on a tty.
        assert!(!with_state(InteractiveMode::Minimal, true, false, true).ui_round_trips_ok());
    }

    #[test]
    fn stderr_ansi_ok_gates() {
        // Non-tty stderr blocks ANSI.
        assert!(!with_state(InteractiveMode::Auto, true, false, false).stderr_ansi_ok());
        // NO_COLOR blocks it even on a tty.
        assert!(!with_state(InteractiveMode::Auto, true, true, true).stderr_ansi_ok());
        // Minimal mode blocks it.
        assert!(!with_state(InteractiveMode::Minimal, true, false, true).stderr_ansi_ok());
        // Full on a tty with no NO_COLOR → on, regardless of TERM checks.
        assert!(with_state(InteractiveMode::Full, false, false, true).stderr_ansi_ok());
    }
}
