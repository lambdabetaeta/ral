//! Redirect frames: open the targets, route fd 1/2 through the shell's
//! sinks, run a body, restore.  [`RedirectState`] is entered directly by
//! `evaluator::machine`'s `Frame::Redirect`, and by [`with_redirects`] for a
//! base-frame native's synchronous call; the targets themselves are closed
//! in `machine::close_redirects`, over the machine's own `Env`.

use super::audit::observe;
use crate::io::Sink;
use crate::runtime::command::{self, EvalRedirect, EvalRedirectV};
use crate::syntax::ast::RedirectMode;
use crate::types::{Break, Error, Mooring, Observed, Settled, Shell, Value, WriteOutcome};

/// What the body's result means for the writes this frame staged.
#[derive(Clone, Copy)]
pub(crate) enum WriteFate {
    /// The body settled: rename each temp onto its target.
    Commit,
    /// The body broke: leave every target exactly as it was.
    Abort,
}

/// An fd-1/2 file target opened in the frame, held until settle so its
/// outcome can be surfaced. `commit` is `Some` only for an atomic `>`.
struct WriteIntent {
    path: String,
    mode: RedirectMode,
    commit: Option<command::PendingWrite>,
}

/// The installed redirect state, owned — no borrow of `Shell` or
/// `Mooring` survives `enter`. Undone by `tear_down`, called explicitly:
/// on the normal path by every caller below, on a panic by `abandon` — the
/// machine's own unwind walk calls it for a `Frame::Redirect` on its stack;
/// [`with_redirects`] calls it itself via `catch_unwind`, for a base-frame
/// native's call, which pushes no frame of its own.
pub(crate) struct RedirectState {
    stdin_guard: Option<command::StdinRedirectGuard>,
    prev_stdout: Option<Sink>,
    prev_ambient: Option<Sink>,
    prev_stderr: Option<Sink>,
    write_intents: Vec<WriteIntent>,
}

/// The sinks a frame displaced, `Some` only where it installed its own.
struct PriorSinks {
    stdout: Option<Sink>,
    ambient: Option<Sink>,
    stderr: Option<Sink>,
}

/// Opens one fd-1/2 write target. The intent is recorded *before* the
/// open, so a failure still surfaces a `failed` write for that path.
fn open_redirect_sink(
    path: &str,
    mode: RedirectMode,
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Settled<Sink> {
    intents.push(WriteIntent {
        path: path.to_string(),
        mode,
        commit: None,
    });
    let (file, commit) = command::open_file(path, mode, shell)?;
    intents.last_mut().expect("intent pushed above").commit = commit;
    Ok(Sink::File(std::sync::Arc::new(file)))
}

fn install_sink_redirects(
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Settled<PriorSinks> {
    let mut stdout = shell.io.stdout.clone();
    let mut stderr = shell.io.stderr.clone();
    let mut stdout_changed = false;
    let mut stderr_changed = false;

    for EvalRedirectV { fd, mode, target } in redirects {
        match (*fd, target) {
            (0, EvalRedirect::File(_))
                if matches!(mode, RedirectMode::Read | RedirectMode::HereString) => {}
            (1, EvalRedirect::File(path)) => {
                stdout = open_redirect_sink(path, *mode, shell, intents)?;
                stdout_changed = true;
            }
            (2, EvalRedirect::File(path)) => {
                let mode = command::stderr_mode(*mode);
                stderr = open_redirect_sink(path, mode, shell, intents)?;
                stderr_changed = true;
            }
            (1, EvalRedirect::Fd(1)) => {
                stdout = stdout.clone();
                stdout_changed = true;
            }
            // Only 2→1: `1>&2` is not in the surface, so nothing can build it.
            (2, EvalRedirect::Fd(1)) => {
                stderr = stdout.clone();
                stderr_changed = true;
            }
            (2, EvalRedirect::Fd(2)) => {
                stderr = stderr.clone();
                stderr_changed = true;
            }
            (1 | 2, EvalRedirect::Fd(other)) => {
                return Err(Break::Error(Error::new(
                    format!(
                        "redirect: fd {fd} cannot be routed to fd {other} \
                         inside an in-process ral frame"
                    ),
                    1,
                )));
            }
            // The lexer admits no fd past 2, so what is left is fd 0 written
            // to or duplicated onto — standard input has no such shape.
            _ => {
                return Err(Break::Error(Error::new(
                    "redirect: standard input can only be read — \
                     `< file` opens a file on it, `<< 'text'` feeds it a string"
                        .to_string(),
                    1,
                )));
            }
        }
    }

    // A redirect changes where bytes go, not which conduit they are on, so the
    // ambient sink follows stdout: under `!{ … } > f`, a discarded statement's
    // bytes belong in the file, which is now the visible stream.
    let prev_ambient =
        stdout_changed.then(|| std::mem::replace(&mut shell.io.ambient, stdout.clone()));
    let prev_stdout = stdout_changed.then(|| std::mem::replace(&mut shell.io.stdout, stdout));
    let prev_stderr = stderr_changed.then(|| std::mem::replace(&mut shell.io.stderr, stderr));

    Ok(PriorSinks {
        stdout: prev_stdout,
        ambient: prev_ambient,
        stderr: prev_stderr,
    })
}

/// Surface every recorded intent as a `failed` write — the frame-entry
/// error paths, where no body runs and any atomic temp is discarded.
fn emit_writes_failed(shell: &mut Shell, mooring: &Mooring, intents: Vec<WriteIntent>) {
    for intent in intents {
        observe(
            shell,
            mooring,
            Observed::Write {
                path: intent.path,
                mode: intent.mode,
                outcome: WriteOutcome::Failed,
                new_bytes: None,
                old_bytes: None,
            },
        );
    }
}

impl RedirectState {
    pub(crate) fn enter(
        redirects: &[EvalRedirectV],
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        // The stdin guard restores only when told to, and nothing owns it
        // until the state exists, so the error arms below undo it by hand.
        let stdin_guard = command::install_stdin_redirect(redirects, mooring, shell)?;
        let mut write_intents = Vec::new();
        let sink_redirects = match install_sink_redirects(redirects, shell, &mut write_intents) {
            Ok(r) => r,
            Err(e) => {
                emit_writes_failed(shell, mooring, write_intents);
                stdin_guard.restore(shell);
                return Err(e);
            }
        };
        Ok(Self {
            stdin_guard: Some(stdin_guard),
            prev_stdout: sink_redirects.stdout,
            prev_ambient: sink_redirects.ambient,
            prev_stderr: sink_redirects.stderr,
            write_intents,
        })
    }

    /// Fires each atomic commit once the body result is known and the
    /// sinks are back, surfaces one write observation per intent, and
    /// returns the first commit failure. A failed body abandons every
    /// intent's temp — dropped here, which is what unlinks its staging
    /// file.
    pub(crate) fn settle_writes(
        &mut self,
        fate: WriteFate,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        let mut commit_err: Settled<()> = Ok(());
        for intent in std::mem::take(&mut self.write_intents) {
            let outcome;
            let new_bytes;
            let mut old_bytes = None;
            match fate {
                WriteFate::Abort => {
                    outcome = WriteOutcome::Aborted;
                    new_bytes = None;
                    // `intent.commit` drops at the end of this iteration,
                    // which is what unlinks the staged temp.
                }
                WriteFate::Commit => {
                    if let Some(commit) = intent.commit {
                        // Both reads must precede the rename, and cost two
                        // whole-file reads: taken only for an ear to hear them.
                        if super::audit::listening(shell, mooring) {
                            old_bytes = commit.old_snapshot_for_diff(shell);
                            new_bytes = commit.new_snapshot_for_diff();
                        } else {
                            new_bytes = None;
                        }
                        match commit.commit() {
                            Ok(()) => outcome = WriteOutcome::Committed,
                            Err(e) => {
                                if commit_err.is_ok() {
                                    commit_err = Err(Break::Error(Error::new(
                                        format!("atomic write: {e}"),
                                        1,
                                    )));
                                }
                                outcome = WriteOutcome::Failed;
                            }
                        }
                    } else {
                        outcome = WriteOutcome::Committed;
                        new_bytes = None;
                    }
                }
            }
            observe(
                shell,
                mooring,
                Observed::Write {
                    path: intent.path,
                    mode: intent.mode,
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
    pub(crate) fn tear_down(&mut self, shell: &mut Shell) {
        use std::io::Write;
        // Flush before swapping the sinks back, or buffered bytes land at
        // the parent.
        let _ = shell.io.stdout.flush();
        let _ = shell.io.stderr.flush();
        if let Some(s) = self.prev_stdout.take() {
            shell.io.stdout = s;
        }
        if let Some(s) = self.prev_ambient.take() {
            shell.io.ambient = s;
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
/// For a base-frame native's call: unlike the `Exec` rule's own arms, that
/// call runs synchronously to completion inside one machine step, so the
/// install/teardown pair needs no frame on the machine's own stack — this
/// is the whole of its panic safety.
///
/// fd 1/2 route through the shell's `Sink`s, never `dup2`: libtest, the
/// REPL frontend, and sibling ral threads all share the process-global
/// fds, and the runtime's own descriptors — pipes, pinned binaries — are
/// nobody's redirect target. fd 0 is parked on `shell.io.stdin` by
/// `install_stdin_redirect`, so the cached `startup_stdin_tty` is
/// consulted only when stdin really is the inherited terminal.
pub(crate) fn with_redirects<F>(
    redirects: &[EvalRedirectV],
    mooring: &Mooring,
    shell: &mut Shell,
    body: F,
) -> Settled<Value>
where
    F: FnOnce(&mut Shell) -> Settled<Value>,
{
    if redirects.is_empty() {
        return body(shell);
    }
    let mut state = RedirectState::enter(redirects, mooring, shell)?;
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
    // Restore before either the commits fire or the error propagates, so
    // both paths get a clean shell to write through.
    state.tear_down(shell);
    let settled = state.settle_writes(fate, mooring, shell);
    match result {
        Ok(v) => {
            settled?;
            Ok(v)
        }
        Err(brk) => Err(brk),
    }
}
