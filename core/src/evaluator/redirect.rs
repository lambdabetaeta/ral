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
    /// The body stopped: the writes are unfinished and belong to the job now,
    /// so the frame surrenders them rather than deciding for it.
    Defer,
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
    fd_guard: Option<command::RedirectGuard>,
    prev_stdout: Option<Sink>,
    prev_ambient: Option<Sink>,
    prev_stderr: Option<Sink>,
    write_intents: Vec<WriteIntent>,
}

struct SinkRedirects {
    unhandled: Vec<EvalRedirectV>,
    prev_stdout: Option<Sink>,
    prev_ambient: Option<Sink>,
    prev_stderr: Option<Sink>,
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
) -> Settled<SinkRedirects> {
    let mut stdout = shell.io.stdout.clone();
    let mut stderr = shell.io.stderr.clone();
    let mut stdout_changed = false;
    let mut stderr_changed = false;
    let mut unhandled = Vec::new();

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
            _ => unhandled.push(EvalRedirectV {
                fd: *fd,
                mode: *mode,
                target: target.clone(),
            }),
        }
    }

    // A redirect changes where bytes go, not which conduit they are on, so the
    // ambient sink follows stdout: under `!{ … } > f`, a discarded statement's
    // bytes belong in the file, which is now the visible stream.
    let prev_ambient =
        stdout_changed.then(|| std::mem::replace(&mut shell.io.ambient, stdout.clone()));
    let prev_stdout = stdout_changed.then(|| std::mem::replace(&mut shell.io.stdout, stdout));
    let prev_stderr = stderr_changed.then(|| std::mem::replace(&mut shell.io.stderr, stderr));

    Ok(SinkRedirects {
        unhandled,
        prev_stdout,
        prev_ambient,
        prev_stderr,
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
        let fd_guard = match command::apply_redirects(&sink_redirects.unhandled, shell) {
            Ok(g) => g,
            Err(e) => {
                emit_writes_failed(shell, mooring, write_intents);
                if let Some(s) = sink_redirects.prev_stdout {
                    shell.io.stdout = s;
                }
                if let Some(s) = sink_redirects.prev_ambient {
                    shell.io.ambient = s;
                }
                if let Some(s) = sink_redirects.prev_stderr {
                    shell.io.stderr = s;
                }
                stdin_guard.restore(shell);
                return Err(e);
            }
        };
        Ok(Self {
            stdin_guard: Some(stdin_guard),
            fd_guard: Some(fd_guard),
            prev_stdout: sink_redirects.prev_stdout,
            prev_ambient: sink_redirects.prev_ambient,
            prev_stderr: sink_redirects.prev_stderr,
            write_intents,
        })
    }

    /// Fires each atomic commit once the body result is known and the
    /// sinks are back, surfaces one write observation per intent, and
    /// returns the first commit failure. A failed body abandons every
    /// intent's temp; a stopped one hands the staged writes back for the
    /// caller to put on the escape.
    pub(crate) fn settle_writes(
        &mut self,
        fate: WriteFate,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Vec<command::PendingWrite>> {
        let mut commit_err: Settled<()> = Ok(());
        let mut unfinished = Vec::new();
        for intent in std::mem::take(&mut self.write_intents) {
            let outcome;
            let new_bytes;
            let mut old_bytes = None;
            match fate {
                WriteFate::Abort => {
                    outcome = WriteOutcome::Aborted;
                    new_bytes = None;
                    command::abandon_all(intent.commit);
                }
                WriteFate::Defer => {
                    outcome = WriteOutcome::Deferred;
                    new_bytes = None;
                    unfinished.extend(intent.commit);
                }
                WriteFate::Commit => {
                    if let Some(commit) = intent.commit {
                        old_bytes = commit.old_snapshot_for_diff();
                        new_bytes = commit.new_snapshot_for_diff();
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
        commit_err.map(|()| unfinished)
    }

    /// Flushes, restores the sinks and stdin, and hands back the pending
    /// atomic commits; the caller's `fd_guard` drop, taken here, runs the
    /// kernel-level fd restore. Idempotent: a second call sees only
    /// emptied slots.
    pub(crate) fn tear_down(&mut self, shell: &mut Shell) -> Vec<command::PendingWrite> {
        use std::io::Write;
        // Flush before swapping the sinks back, or buffered bytes land at
        // the parent. The libc flushes catch the fd-level redirect arms.
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
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
        let commits = self
            .fd_guard
            .take()
            .map(command::restore_redirects)
            .unwrap_or_default();
        if let Some(g) = self.stdin_guard.take() {
            g.restore(shell);
        }
        commits
    }

    /// The panic path: undo fds and sinks, and drop the commits `tear_down`
    /// hands back — which unlinks their staging files. No write is
    /// observed: a panic never reaches an audit trail.
    pub(crate) fn abandon(mut self, shell: &mut Shell) {
        let _ = self.tear_down(shell);
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
/// fd 1/2 route through the shell's `Sink`s, not `dup2`: libtest, the
/// REPL frontend, and sibling ral threads all share the process-global
/// fds. The fd-level path survives for redirects no `Sink` models and
/// for bundled uutils, which writes through libc rather than
/// `Shell::write_stdout`. fd 0 stays off `dup2` too — `< file` is parked
/// on `shell.io.stdin` by `install_stdin_redirect` — so the cached
/// `startup_stdin_tty` is consulted only when stdin really is the
/// inherited terminal.
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
        Err(brk) if brk.is_stop() => WriteFate::Defer,
        Err(_) => WriteFate::Abort,
    };
    // Restore before either the commits fire or the error propagates, so
    // both paths get a clean shell to write through.
    let commits = state.tear_down(shell);
    let settled = state.settle_writes(fate, mooring, shell);
    match result {
        Ok(v) => {
            settled?;
            command::commit_atomics(commits)?;
            Ok(v)
        }
        // A stop takes every staged write with it, fd-level and sink-level
        // alike; any other break leaves them to fall out of scope, which is
        // how a `PendingWrite` without a destructor abandons a temp.
        Err(brk) => {
            let unfinished = settled.unwrap_or_default();
            Err(command::defer_to_stop(
                brk,
                commits.into_iter().chain(unfinished),
            ))
        }
    }
}
