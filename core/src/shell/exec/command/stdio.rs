//! Stdio routing for a spawned external child: wire stdin, stdout and stderr
//! into the `Launch` from sources and sinks.  The call's redirects are
//! already installed on the shell's by `shell::exec::redirect`, and a pipeline
//! edge is just another source or sink.

use crate::io::{Edge, Io, OutFd, Sink, Source};
use crate::process::StdioSpec;
use crate::shell::runtime::{Error, Settled};
use std::sync::Arc;

use super::Pumps;

/// What a child at the boundary reads of `io`'s stdin.  A joined group's
/// source is its owner's choice and passes through; otherwise a startup tty
/// survives only where `readable`, in ral's own group or one lent the
/// terminal, and any other group of its own reads EOF.
pub(crate) fn boundary_stdin(io: &Io, readable: bool) -> &Source {
    match &io.stdin {
        Source::Terminal if io.terminal.startup_stdin_tty && io.stage.is_none() && !readable => {
            &Source::Empty
        }
        stdin => stdin,
    }
}

/// Where a child's or a stage's stdout goes: a ral sink, or a pipeline edge
/// handed over whole, its write end and its fate.
pub(crate) enum ChildOut {
    Sink(Sink),
    Edge {
        writer: os_pipe::PipeWriter,
        edge: Arc<Edge>,
    },
}

/// The child's stdin is its source: `Empty` is `/dev/null`, a reader a
/// duplicate of it, and `Terminal` ral's own fd 0.
fn stdin_stdio(stdin: &Source) -> Settled<StdioSpec> {
    Ok(match stdin {
        Source::Empty => StdioSpec::null(),
        Source::Terminal => StdioSpec::inherit(),
        Source::Reader(r) => r
            .try_clone()
            .map_err(|e| Error::io("could not duplicate stdin", &e))?
            .into(),
    })
}

/// Wire the child's stdin, stdout and stderr, returning the sinks its piped
/// fds must be pumped into.  A `File` or `Pipe` sink is handed to the child
/// directly; an edge's fate is dropped, the sentinel hearing the child's
/// writes in its place.
///
/// The one platform split: when both output streams share a destination, Unix
/// registers a `pre_exec` `dup2` that runs after the kernel wired fd 1, so
/// fd 2 follows fd 1 wherever it went; Windows, having no `pre_exec`, hands
/// the child a second handle to that destination.
///
/// Audit capture belongs elsewhere: it tees the shell's own sinks at dispatch
/// level in `shell::exec::capture::with_audit_capture`.
pub(crate) fn wire_stdio(
    command: &mut crate::process::Launch,
    stdin: &Source,
    stdout: ChildOut,
    stderr: &Sink,
) -> Settled<Pumps<Sink>> {
    let pipe = |e: std::io::Error| Error::io("pipe", &e);
    command.stdin(stdin_stdio(stdin)?);
    let (out, out_pump, joined) = match stdout {
        ChildOut::Sink(sink) => {
            let (out, pump) = sink.child_stdio(OutFd::Stdout).map_err(pipe)?;
            (out, pump, stderr.same_destination(&sink).then_some(sink))
        }
        ChildOut::Edge { writer, .. } => (StdioSpec::from_pipe_writer(writer), None, None),
    };
    command.stdout(out);
    let err_pump = if let Some(sink) = joined {
        join_stderr(command, &sink)?
    } else {
        let (err, pump) = stderr.child_stdio(OutFd::Stderr).map_err(pipe)?;
        command.stderr(err);
        pump
    };
    Ok(Pumps::new(out_pump, err_pump))
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
            .map_err(|e| Error::io("pipe", &e))?;
        (StdioSpec::from_owned_handle(owned), None)
    } else {
        stdout
            .child_stdio(OutFd::Stderr)
            .map_err(|e| Error::io("pipe", &e))?
    };
    command.stderr(stdio);
    Ok(pump)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{boundary_stdin, stdin_stdio};
    use crate::io::Source;
    use crate::process::{DurableRoot, Membership, Pgid, Scope, StdioSpec};
    use crate::shell::runtime::Shell;

    /// A startup tty on the shell's stdin.
    fn at_a_tty() -> Shell {
        let mut shell = crate::test_helper::core_shell();
        shell.io.terminal.startup_stdin_tty = true;
        shell.io.stdin = Source::Terminal;
        shell
    }

    /// The terminal survives in ral's own group or a lent one, and is EOF to
    /// any other group of its own.
    #[test]
    fn a_startup_tty_survives_only_where_its_group_can_read_it() {
        let shell = at_a_tty();
        assert!(matches!(boundary_stdin(&shell.io, true), Source::Terminal));
        assert!(matches!(boundary_stdin(&shell.io, false), Source::Empty));
    }

    /// A joined group reads what its owner chose, lent or not.
    #[test]
    fn a_joined_group_passes_its_owners_source_through() {
        let mut shell = at_a_tty();
        let group = Pgid::from_raw(1).expect("1 is positive");
        let stage = Scope::clone(&DurableRoot::default().worker());
        shell.io.stage = Some(Membership::new(group, stage));
        assert!(matches!(boundary_stdin(&shell.io, false), Source::Terminal));
        shell.io.stdin = Source::Empty;
        assert!(matches!(boundary_stdin(&shell.io, true), Source::Empty));
    }

    /// Only a startup tty is withheld: a redirected fd 0 is no terminal.
    #[test]
    fn a_non_tty_stdin_reaches_every_group() {
        let mut shell = at_a_tty();
        shell.io.terminal.startup_stdin_tty = false;
        assert!(matches!(boundary_stdin(&shell.io, false), Source::Terminal));
    }

    /// The source is authoritative: `Empty` wires `/dev/null` and `Terminal`
    /// inherits fd 0, whatever fd 0 is.
    #[test]
    fn the_childs_source_decides_its_stdin() {
        assert!(matches!(stdin_stdio(&Source::Empty), Ok(StdioSpec::Null)));
        assert!(matches!(
            stdin_stdio(&Source::Terminal),
            Ok(StdioSpec::Inherit)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_stage_shaped_source_is_borrowed() {
        use crate::io::SourceReader;

        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let stdin = Source::Reader(
            SourceReader::pipe(r).interruptible(Scope::clone(&DurableRoot::default())),
        );

        let stdio = stdin_stdio(&stdin).expect("stdin_stdio");
        assert!(matches!(stdio, StdioSpec::Stdio(_)));

        // The source is borrowed, never taken, so it still has a reader.
        assert!(stdin.reader().expect("reader").is_some());
    }
}
