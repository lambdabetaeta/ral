//! Redirect frames: open the targets, route fd 1/2 through the shell's
//! sinks, run a body, restore.  [`RedirectState`] is the one interpreter of a
//! redirect list, entered by `evaluator::machine`'s `Frame::Redirect` and by
//! [`with_redirects`] for a synchronous call; the targets themselves are
//! closed in `machine::close_redirects`, over the machine's own `Env`.

use super::audit::observe;
use crate::io::Sink;
use crate::runtime::command;
use crate::source::Span;
use crate::syntax::ast::{Redirects, StderrTarget, WriteMode};
use crate::types::{Mooring, Observed, Settled, Shell, Value, WriteOutcome};

/// What the body's result means for the writes this frame staged.
#[derive(Clone, Copy)]
pub(crate) enum WriteFate {
    /// The body settled: rename each temp onto its target.
    Commit,
    /// The body broke: leave every target exactly as it was.
    Abort,
}

/// An atomic `>` staged in the frame, held until settle so its outcome can
/// be surfaced.  Every other target streams, and is observed at its open.
struct WriteIntent {
    path: String,
    mode: WriteMode,
    commit: command::PendingWrite,
}

/// The installed redirect state, owned — no borrow of `Shell` or
/// `Mooring` survives `enter`. Undone by `leave`, called explicitly:
/// on the normal path by every caller below, on a panic by `abandon` — the
/// machine's own unwind walk calls it for a `Frame::Redirect` on its stack;
/// [`with_redirects`] calls it itself via `catch_unwind`, for a synchronous
/// call, which pushes no frame of its own.
pub(crate) struct RedirectState {
    stdin_guard: Option<command::StdinRedirectGuard>,
    prev_stdout: Option<Sink>,
    prev_stderr: Option<Sink>,
    write_intents: Vec<WriteIntent>,
    /// The redirect's own site: every entry it makes carries it.
    span: Option<Span>,
}

/// The sinks a frame displaced, `Some` only where it installed its own.
struct PriorSinks {
    stdout: Option<Sink>,
    stderr: Option<Sink>,
}

fn observe_write(
    shell: &mut Shell,
    mooring: &Mooring,
    path: &str,
    mode: WriteMode,
    outcome: WriteOutcome,
) {
    observe(
        shell,
        mooring,
        Observed::Write {
            path: path.to_string(),
            mode,
            outcome,
            new_bytes: None,
            old_bytes: None,
        },
    );
}

/// Opens one fd-1/2 write target.  A streaming target is settled at the
/// open; an atomic one is staged as an intent until the frame settles.
fn open_redirect_sink(
    path: &str,
    mode: WriteMode,
    mooring: &Mooring,
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Settled<Sink> {
    let (file, commit) = command::open_write(path, mode, shell).inspect_err(|_| {
        observe_write(shell, mooring, path, mode, WriteOutcome::Failed);
    })?;
    match commit {
        Some(commit) => intents.push(WriteIntent {
            path: path.to_string(),
            mode,
            commit,
        }),
        None => observe_write(shell, mooring, path, mode, WriteOutcome::Committed),
    }
    Ok(Sink::File(std::sync::Arc::new(file)))
}

fn install_sink_redirects(
    redirects: &Redirects<String>,
    mooring: &Mooring,
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Settled<PriorSinks> {
    let stdout = redirects
        .stdout
        .as_ref()
        .map(|(mode, path)| open_redirect_sink(path, *mode, mooring, shell, intents))
        .transpose()?;
    let stderr = match &redirects.stderr {
        Some(StderrTarget::File(mode, path)) => {
            Some(open_redirect_sink(path, *mode, mooring, shell, intents)?)
        }
        Some(StderrTarget::Stdout) => {
            Some(stdout.clone().unwrap_or_else(|| shell.io.stdout.clone()))
        }
        None => None,
    };

    Ok(PriorSinks {
        stdout: stdout.map(|s| std::mem::replace(&mut shell.io.stdout, s)),
        stderr: stderr.map(|s| std::mem::replace(&mut shell.io.stderr, s)),
    })
}

impl RedirectState {
    /// Opens in a fixed order — stdin, stdout, stderr — so the audit trail
    /// reads the same for a block and for an external.
    pub(crate) fn enter(
        redirects: &Redirects<String>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        shell.at_site(span, |shell| Self::open(redirects, span, mooring, shell))
    }

    fn open(
        redirects: &Redirects<String>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        // The stdin guard restores only when told to, and nothing owns it
        // until the state exists, so the error arm below undoes it by hand.
        let stdin_guard =
            command::install_stdin_redirect(redirects.stdin.as_ref(), mooring, shell)?;
        let mut write_intents = Vec::new();
        let prior = match install_sink_redirects(redirects, mooring, shell, &mut write_intents) {
            Ok(prior) => prior,
            Err(e) => {
                for intent in write_intents {
                    observe_write(
                        shell,
                        mooring,
                        &intent.path,
                        intent.mode,
                        WriteOutcome::Failed,
                    );
                }
                stdin_guard.restore(shell);
                return Err(e);
            }
        };
        Ok(Self {
            stdin_guard: Some(stdin_guard),
            prev_stdout: prior.stdout,
            prev_stderr: prior.stderr,
            write_intents,
            span,
        })
    }

    /// Restores the sinks and stdin, then settles the staged writes: with the
    /// redirected handles dropped, each atomic commit fires on `Commit` and is
    /// abandoned on `Abort`.  Returns the first commit failure.
    pub(crate) fn leave(
        &mut self,
        fate: WriteFate,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        self.tear_down(shell);
        let span = self.span;
        shell.at_site(span, |shell| self.settle_writes(fate, mooring, shell))
    }

    /// Surfaces one write observation per intent.  A failed body abandons
    /// every intent's temp — dropped here, which is what unlinks its staging
    /// file.
    fn settle_writes(
        &mut self,
        fate: WriteFate,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        let mut commit_err: Settled<()> = Ok(());
        for WriteIntent { path, mode, commit } in std::mem::take(&mut self.write_intents) {
            let mut old_bytes = None;
            let mut new_bytes = None;
            let outcome = match fate {
                WriteFate::Abort => WriteOutcome::Aborted,
                WriteFate::Commit => {
                    // Both reads must precede the rename, and cost two
                    // whole-file reads: taken only for an ear to hear them.
                    if super::audit::listening(shell, mooring) {
                        old_bytes = commit.old_snapshot_for_diff(shell);
                        new_bytes = commit.new_snapshot_for_diff();
                    }
                    match commit.commit() {
                        Ok(()) => WriteOutcome::Committed,
                        Err(e) => {
                            if commit_err.is_ok() {
                                commit_err = Err(command::atomic_write_error(&e));
                            }
                            WriteOutcome::Failed
                        }
                    }
                }
            };
            observe(
                shell,
                mooring,
                Observed::Write {
                    path,
                    mode,
                    outcome,
                    new_bytes,
                    old_bytes,
                },
            );
        }
        commit_err
    }

    /// Flushes, then restores the sinks and stdin. Idempotent: a second call
    /// sees only emptied slots.
    fn tear_down(&mut self, shell: &mut Shell) {
        use std::io::Write;
        // Flush before swapping the sinks back, or buffered bytes land at
        // the parent.
        let _ = shell.io.stdout.flush();
        let _ = shell.io.stderr.flush();
        if let Some(s) = self.prev_stdout.take() {
            shell.io.stdout = s;
        }
        if let Some(s) = self.prev_stderr.take() {
            shell.io.stderr = s;
        }
        if let Some(g) = self.stdin_guard.take() {
            g.restore(shell);
        }
    }

    /// The panic path: undo the sinks and drop the intents, which unlinks
    /// their staging files. No write is observed: a panic never reaches an
    /// audit trail.
    pub(crate) fn abandon(mut self, shell: &mut Shell) {
        self.tear_down(shell);
    }
}

/// Runs `body` with `redirects` installed, always restoring. Atomic commits
/// fire on success and are dropped on failure, discarding the staging file.
///
/// For a synchronous call: it runs to completion inside one machine step, so
/// the install/teardown pair needs no frame on the machine's own stack — this
/// is the whole of its panic safety.
///
/// fd 1/2 route through the shell's `Sink`s, never `dup2`: libtest, the
/// REPL frontend, and sibling ral threads all share the process-global
/// fds, and the runtime's own descriptors — pipes, pinned binaries — are
/// nobody's redirect target. fd 0 is parked on `shell.io.stdin` by
/// `install_stdin_redirect`, so the cached `startup_stdin_tty` is
/// consulted only when stdin really is the inherited terminal.
pub(crate) fn with_redirects(
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Settled<Value> {
    if redirects.is_empty() {
        return body(shell);
    }
    let mut state = RedirectState::enter(redirects, span, mooring, shell)?;
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(shell))) {
        Ok(result) => result,
        Err(payload) => {
            state.abandon(shell);
            std::panic::resume_unwind(payload);
        }
    };
    let fate = match &result {
        Ok(_) => WriteFate::Commit,
        Err(_) => WriteFate::Abort,
    };
    // Restored before either the commits fire or the error propagates, so
    // both paths get a clean shell to write through.  The body's own break
    // outranks a failed commit.
    let settled = state.leave(fate, mooring, shell);
    let v = result?;
    settled?;
    Ok(v)
}
