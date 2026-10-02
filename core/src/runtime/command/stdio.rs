//! Stdio routing for a spawned external child: wire stdin, stdout and stderr
//! into the `Launch` from sources and sinks.  The call's redirects are
//! already installed on the shell's by `evaluator::redirect`, and a pipeline
//! edge is just another source or sink.

use crate::io::{Io, Sink, Source};
use crate::types::{Break, Error, Settled, Shell};

use super::Pumps;
use super::process::pipe_err;

/// Capability witness that the parent's fd 0 is safe to inherit into a
/// spawned child's stdin, mintable only through the issuers below.
///
/// A child whose pgid will not hold the foreground must get
/// `/dev/null` instead: hand it the terminal and the kernel SIGTTINs
/// it on its first read, leaving ral's pump waiting forever.
pub(crate) struct TtyInputPermit {
    _private: (),
}

impl TtyInputPermit {
    /// The child leads the foreground pgid, or runs non-interactively.
    pub(crate) fn for_standalone_external() -> Self {
        Self { _private: () }
    }

    /// The pipeline's own pgid takes the foreground via
    /// `PipelineGroup::lend`, so its members may read the tty.
    pub(crate) fn for_pure_external_pipeline() -> Self {
        Self { _private: () }
    }

    /// Nothing to SIGTTIN: the caller has just seen `startup_stdin_tty` false.
    pub(crate) fn for_non_tty_stdin() -> Self {
        Self { _private: () }
    }
}

/// How a child's stdin is wired; `Inherit` costs a [`TtyInputPermit`].
enum StdinRoute {
    Inherit(TtyInputPermit),
    Reader(crate::io::SourceReader),
    Null,
}

impl StdinRoute {
    fn into_stdio(self) -> crate::process::StdioSpec {
        match self {
            Self::Inherit(_) => crate::process::StdioSpec::inherit(),
            Self::Reader(r) => r.into(),
            Self::Null => crate::process::StdioSpec::null(),
        }
    }
}

/// Should the child inherit ral's fd 1 directly, so it can detect a TTY?
///
/// Only when fd 1 was a tty at startup and the sink still targets that real
/// fd.  A redirect or an `audit` capture installs another sink, which fails
/// here and so falls through to the pump path unaided.
pub(super) fn inherit_tty(shell: &Shell) -> bool {
    shell.io.terminal.startup_stdout_tty && matches!(shell.io.stdout, Sink::Terminal)
}

/// The streams a child's three fds are wired from.
pub(crate) struct ChildIo<'a> {
    pub(crate) stdin: &'a Source,
    pub(crate) stdout: &'a Sink,
    pub(crate) stderr: &'a Sink,
}

impl<'a> From<&'a Io> for ChildIo<'a> {
    fn from(io: &'a Io) -> Self {
        Self {
            stdin: &io.stdin,
            stdout: &io.stdout,
            stderr: &io.stderr,
        }
    }
}

/// Choose the stdin route: a source with an fd is borrowed, `Empty` is
/// `/dev/null`, and only the fall-through to fd 0 needs a permit.  `grant` is
/// what the caller allows when fd 0 is a tty.
fn wire_stdin(
    stdin: &Source,
    startup_stdin_tty: bool,
    grant: Option<TtyInputPermit>,
) -> Settled<StdinRoute> {
    // An explicit empty source (an exarch tool run) denies byte input in its
    // own right, so it wires `/dev/null` instead of falling through to fd 0.
    if matches!(stdin, Source::Empty) {
        return Ok(StdinRoute::Null);
    }
    if let Some(r) = stdin.reader().map_err(stdin_error)? {
        return Ok(StdinRoute::Reader(r));
    }
    Ok(if startup_stdin_tty {
        grant.map_or(StdinRoute::Null, StdinRoute::Inherit)
    } else {
        StdinRoute::Inherit(TtyInputPermit::for_non_tty_stdin())
    })
}

/// Shared by every door that duplicates a stdin source.
pub(crate) fn stdin_error(e: impl std::fmt::Display) -> Break {
    Break::Error(Error::new(format!("could not duplicate stdin: {e}"), 1))
}

/// Wire the child's stdin, stdout and stderr from `io`, returning the sinks
/// its piped fds must be pumped into.
///
/// `grant` admits a tty fd 0 and `inherit_tty` lets a `Stderr` stdout sink
/// inherit the real fd 1.  A `File` or `Pipe` sink is handed to the child
/// directly.
///
/// The one platform split: when both output streams share a destination, Unix
/// registers a `pre_exec` `dup2` that runs after the kernel wired fd 1, so
/// fd 2 follows fd 1 wherever it went; Windows, having no `pre_exec`, hands
/// the child a second handle to that destination.
///
/// Audit capture belongs elsewhere: it tees the shell's own sinks at dispatch
/// level in `evaluator::with_audit_capture`.
pub(crate) fn wire_stdio(
    command: &mut crate::process::Launch,
    shell: &Shell,
    io: &ChildIo<'_>,
    grant: Option<TtyInputPermit>,
    inherit_tty: bool,
) -> Settled<Pumps<Sink>> {
    let stdin = wire_stdin(io.stdin, shell.io.terminal.startup_stdin_tty, grant)?;
    command.stdin(stdin.into_stdio());
    let out = io
        .stdout
        .child_stdout(inherit_tty)
        .map_err(|e| pipe_err(&e))?;
    command.stdout(out.stdio);
    let err_pump = if io.stderr.same_destination(io.stdout) {
        join_stderr(command, io.stdout)?
    } else {
        let err = io.stderr.child_stderr().map_err(|e| pipe_err(&e))?;
        command.stderr(err.stdio);
        err.pump
    };
    Ok(Pumps::new(out.pump, err_pump))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps, reason = "the Windows twin can fail")]
fn join_stderr(command: &mut crate::process::Launch, _stdout: &Sink) -> Settled<Option<Sink>> {
    command.dup_stdout_to_stderr();
    Ok(None)
}

#[cfg(windows)]
fn join_stderr(command: &mut crate::process::Launch, stdout: &Sink) -> Settled<Option<Sink>> {
    let (stdio, pump) = if matches!(stdout, Sink::Terminal) {
        // The child inherits our fd 1, so clone fd 1 — not fd 2 — for its
        // stderr.  The bare inherit would hand it fd 2, routing diagnostics
        // straight past `2>&1`.
        use std::os::windows::io::AsHandle;
        let owned = std::io::stdout()
            .as_handle()
            .try_clone_to_owned()
            .map_err(|e| pipe_err(&e))?;
        (crate::process::StdioSpec::from_owned_handle(owned), None)
    } else {
        let plan = stdout.child_stdio_plan().map_err(|e| pipe_err(&e))?;
        (plan.stdio, plan.pump)
    };
    command.stderr(stdio);
    Ok(pump)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::{StdinRoute, wire_stdin};
    use crate::io::Source;
    use crate::types::Shell;

    /// Denial of byte input is its own effect, independent of foreground:
    /// `Empty` wires `/dev/null`, `Terminal` still inherits fd 0.
    #[test]
    fn empty_stdin_wires_null_but_terminal_inherits() {
        let mut shell = Shell::default();

        shell.io.stdin = Source::Empty;
        assert!(
            matches!(
                wire_stdin(&shell.io.stdin, shell.io.terminal.startup_stdin_tty, None),
                Ok(StdinRoute::Null)
            ),
            "Empty stdin must wire to /dev/null"
        );

        shell.io.stdin = Source::Terminal;
        shell.io.terminal.startup_stdin_tty = false;
        assert!(
            matches!(
                wire_stdin(&shell.io.stdin, shell.io.terminal.startup_stdin_tty, None),
                Ok(StdinRoute::Inherit(_))
            ),
            "Terminal stdin still inherits fd 0"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wire_stdin_borrows_a_stage_shaped_source() {
        use crate::io::SourceReader;
        use crate::process::Wake;

        let mut shell = Shell::default();
        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let wake = Wake::new().expect("wake");
        shell.io.stdin = Source::Reader(SourceReader::pipe(r).interruptible(wake));

        let route = wire_stdin(&shell.io.stdin, shell.io.terminal.startup_stdin_tty, None)
            .expect("wire_stdin");
        assert!(matches!(route, StdinRoute::Reader(_)));
        let _stdio = route.into_stdio();

        // The source is borrowed, never taken, so it still has a reader.
        assert!(shell.io.stdin.reader().expect("reader").is_some());
    }
}
