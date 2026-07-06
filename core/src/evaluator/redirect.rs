//! Installs and unwinds redirect frames for the evaluator.
//!
//! Two computation forms carry trailing redirects: [`CompKind::Exec`]
//! when its `redirects` field is non-empty, and [`ScopeOp::Redirect`]
//! when an `App` or nested scope is wrapped in `> file` syntax. Both
//! flow through one pair of combinators —
//! [`within_redirect_frame`] (the high-level entry used by
//! [`super::invoke`]) on top of [`with_redirects`] (RAII
//! install/restore) — so sink routing, fd duplication, tail-call
//! absorption, and atomic-write commits all happen in a single
//! place.

use super::absorb_tail;
use super::val::eval_val;
use crate::io::Sink;
use crate::ir::{RedirectV, ValRedirectTarget};
use crate::runtime::command::io_event::{self, WriteOutcome};
use crate::runtime::command::{self, EvalRedirect, EvalRedirectV};
use crate::syntax::ast::RedirectMode;
use crate::types::*;

/// Evaluates redirect targets to their concrete forms — file paths
/// or numeric fds. Used by [`super::call::eval_call_parts`] for
/// `Exec` calls (which fuse their own redirects into the syscall)
/// and by [`within_redirect_frame`] for the redirect-frame scope
/// wrapping non-`Exec` bodies.
pub(crate) fn eval_redirects(
    redirects: &[RedirectV],
    shell: &mut Shell,
) -> Result<Vec<EvalRedirectV>, Error> {
    redirects
        .iter()
        .map(|r| {
            Ok(EvalRedirectV {
                fd: r.fd,
                mode: r.mode,
                target: match &r.target {
                    ValRedirectTarget::File(v) => {
                        EvalRedirect::File(eval_val(v, shell)?.to_string())
                    }
                    ValRedirectTarget::Fd(n) => EvalRedirect::Fd(*n),
                },
            })
        })
        .collect()
}

/// Installs `redirects` for the duration of `body`, restores them
/// on exit, and absorbs any tail call that would otherwise escape
/// past the redirect frame. The tail callee is the redirect's
/// intended consumer and must see the redirected sinks; letting the tail
/// escape past `with_redirects` would land the callee after
/// restoration, with stdout and stderr aimed back at the parent
/// rather than the redirect target. (SPEC §3.1
/// trampoline-absorb-inside-frame, specialised to a redirect-shaped
/// frame.)
///
/// This is the single combinator for the two redirect-bearing sites:
/// [`crate::ir::CompKind::Exec`] when its `redirects` field is
/// non-empty, and [`crate::ir::ScopeOp::Redirect`] when an `App` or
/// nested scope carries trailing I/O redirects.
pub(crate) fn within_redirect_frame<F>(
    redirects: &[RedirectV],
    shell: &mut Shell,
    body: F,
) -> Raw<Value>
where
    F: FnOnce(&mut Shell) -> Raw<Value>,
{
    if redirects.is_empty() {
        return body(shell);
    }
    let evaluated = eval_redirects(redirects, shell)?;
    with_redirects(&evaluated, shell, |shell| {
        absorb_tail(body(shell), shell).map_err(Control::Break)
    })
}

/// One fd-1/2 file write target opened inside the frame, carried until
/// the frame settles so its outcome (`committed` / `aborted` / `failed`)
/// can be surfaced as a write event. `commit` is `Some` only for an
/// atomic `>` to a regular file (fd 1); it is fired in `settle_writes`,
/// where its success decides between `committed` and `failed`.
struct WriteIntent {
    path: String,
    mode: RedirectMode,
    commit: Option<command::AtomicCommit>,
}

/// RAII frame holding the redirect state [`with_redirects`] installs:
/// the stdin guard, any residual fd guard, the prior `stdout` /
/// `stderr` sinks, and the fd-1/2 write intents whose outcome is
/// surfaced at settle. Restoration runs through `tear_down`, which is
/// also called by `Drop` on the panic path so a body that unwinds
/// still leaves the shell clean; the pending atomic commits it returns
/// are a local, collected fresh from `fd_guard`'s restore rather than
/// accumulated on the frame.
struct RedirectFrame<'a> {
    shell: &'a mut Shell,
    stdin_guard: Option<command::StdinRedirectGuard>,
    fd_guard: Option<command::RedirectGuard>,
    prev_stdout: Option<Sink>,
    prev_stderr: Option<Sink>,
    write_intents: Vec<WriteIntent>,
}

struct SinkRedirects {
    unhandled: Vec<EvalRedirectV>,
    prev_stdout: Option<Sink>,
    prev_stderr: Option<Sink>,
}

fn clone_redirect_sink(sink: &Sink, context: &str) -> Raw<Sink> {
    sink.try_clone()
        .map_err(|e| Break::Error(Error::new(format!("{context}: {e}"), 1)).into())
}

/// Open one fd-1/2 file write target and record its write intent. The
/// intent is pushed *before* the open is consulted for a failure so the
/// open-error path (handled by the caller) can surface a `failed` write
/// event for the same path; on success the intent picks up the atomic
/// commit (if any) so `settle_writes` can fire it and decide the outcome.
fn open_redirect_sink(
    path: &str,
    mode: RedirectMode,
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Raw<Sink> {
    intents.push(WriteIntent {
        path: path.to_string(),
        mode,
        commit: None,
    });
    let (file, commit) = command::open_file(path, &mode, shell)?;
    intents.last_mut().expect("intent pushed above").commit = commit;
    Ok(Sink::File(file))
}

fn install_sink_redirects(
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Raw<SinkRedirects> {
    let mut stdout = clone_redirect_sink(&shell.turn.io.stdout, "redirect: duplicate stdout")?;
    let mut stderr = clone_redirect_sink(&shell.turn.io.stderr, "redirect: duplicate stderr")?;
    let mut stdout_changed = false;
    let mut stderr_changed = false;
    let mut unhandled = Vec::new();

    for EvalRedirectV { fd, mode, target } in redirects {
        match (*fd, target) {
            (0, EvalRedirect::File(_)) if matches!(mode, RedirectMode::Read) => {}
            (1, EvalRedirect::File(path)) => {
                stdout = open_redirect_sink(path, *mode, shell, intents)?;
                stdout_changed = true;
            }
            (2, EvalRedirect::File(path)) => {
                let mode = command::stderr_mode(mode);
                stderr = open_redirect_sink(path, mode, shell, intents)?;
                stderr_changed = true;
            }
            (1, EvalRedirect::Fd(1)) => {
                stdout = clone_redirect_sink(&stdout, "redirect: duplicate stdout")?;
                stdout_changed = true;
            }
            (1, EvalRedirect::Fd(2)) => {
                stdout = clone_redirect_sink(&stderr, "redirect: duplicate stderr as stdout")?;
                stdout_changed = true;
            }
            (2, EvalRedirect::Fd(1)) => {
                stderr = clone_redirect_sink(&stdout, "redirect: duplicate stdout as stderr")?;
                stderr_changed = true;
            }
            (2, EvalRedirect::Fd(2)) => {
                stderr = clone_redirect_sink(&stderr, "redirect: duplicate stderr")?;
                stderr_changed = true;
            }
            (1 | 2, EvalRedirect::Fd(other)) => {
                return Err(Break::Error(Error::new(
                    format!(
                        "redirect: fd {fd} cannot be routed to fd {other} \
                         inside an in-process ral frame"
                    ),
                    1,
                ))
                .into());
            }
            _ => unhandled.push(EvalRedirectV {
                fd: *fd,
                mode: *mode,
                target: target.clone(),
            }),
        }
    }

    let prev_stdout = stdout_changed.then(|| std::mem::replace(&mut shell.turn.io.stdout, stdout));
    let prev_stderr = stderr_changed.then(|| std::mem::replace(&mut shell.turn.io.stderr, stderr));

    Ok(SinkRedirects {
        unhandled,
        prev_stdout,
        prev_stderr,
    })
}

/// Surface every recorded write intent as a `failed` write event and
/// drop it — the door for the open-error and frame-entry-error paths,
/// where no body runs and any already-opened atomic temp is discarded.
fn emit_writes_failed(shell: &Shell, intents: Vec<WriteIntent>) {
    for intent in intents {
        shell.emit_io(io_event::write(
            &intent.path,
            intent.mode,
            WriteOutcome::Failed,
            None,
        ));
    }
}

impl<'a> RedirectFrame<'a> {
    fn enter(redirects: &[EvalRedirectV], shell: &'a mut Shell) -> Raw<Self> {
        // `install_stdin_redirect` owns its own RAII unwind through
        // the explicit `restore` API; we hold its guard alongside
        // the rest.
        let stdin_guard = command::install_stdin_redirect(redirects, shell)?;
        // The stdin guard is not yet attached to a `RedirectFrame`,
        // so an error from `apply_redirects` will not trigger its
        // Drop. Restore it manually before propagating.
        let mut write_intents = Vec::new();
        let sink_redirects = match install_sink_redirects(redirects, shell, &mut write_intents) {
            Ok(r) => r,
            Err(e) => {
                // A write target failed to open (or a later one did,
                // after this one opened): no body runs, so every
                // recorded intent is a failed write.
                emit_writes_failed(shell, write_intents);
                stdin_guard.restore(shell);
                return Err(e);
            }
        };
        let fd_guard = match command::apply_redirects(&sink_redirects.unhandled, shell) {
            Ok(g) => g,
            Err(e) => {
                emit_writes_failed(shell, write_intents);
                if let Some(s) = sink_redirects.prev_stdout {
                    shell.turn.io.stdout = s;
                }
                if let Some(s) = sink_redirects.prev_stderr {
                    shell.turn.io.stderr = s;
                }
                stdin_guard.restore(shell);
                return Err(e.into());
            }
        };
        Ok(Self {
            shell,
            stdin_guard: Some(stdin_guard),
            fd_guard: Some(fd_guard),
            prev_stdout: sink_redirects.prev_stdout,
            prev_stderr: sink_redirects.prev_stderr,
            write_intents,
        })
    }

    /// Settle the fd-1/2 write intents after the body has finished and
    /// the sinks are restored: fire each atomic commit, surface one
    /// write event per intent, and return the first commit failure (if
    /// any) so the caller can propagate it.
    ///
    /// - body errored → every intent is `aborted` (its atomic temp drops
    ///   here, discarded);
    /// - body ok, non-atomic target → `committed`;
    /// - body ok, atomic target → fire the commit: `committed` on
    ///   success, `failed` on a rename/fsync error (the first such error
    ///   is returned for propagation).
    fn settle_writes(&mut self, body_ok: bool) -> Settled<()> {
        let mut commit_err: Settled<()> = Ok(());
        for intent in std::mem::take(&mut self.write_intents) {
            let outcome;
            let new_bytes;
            if !body_ok {
                outcome = WriteOutcome::Aborted;
                new_bytes = None;
            } else if let Some(commit) = intent.commit {
                new_bytes = commit.temp_preview();
                match commit.commit() {
                    Ok(()) => outcome = WriteOutcome::Committed,
                    Err(e) => {
                        if commit_err.is_ok() {
                            commit_err =
                                Err(Break::Error(Error::new(format!("atomic write: {e}"), 1)));
                        }
                        outcome = WriteOutcome::Failed;
                    }
                }
            } else {
                outcome = WriteOutcome::Committed;
                new_bytes = None;
            }
            self.shell.emit_io(io_event::write(
                &intent.path,
                intent.mode,
                outcome,
                new_bytes.as_deref(),
            ));
        }
        commit_err
    }

    /// Unwinds the frame: flush, restore sinks, take `fd_guard` (its
    /// Drop runs any remaining kernel-level fd restore), restore
    /// stdin.  Returns any pending atomic-write commits the caller
    /// may want to commit on the success path. Idempotent — `Drop`
    /// re-runs it and skips already-torn-down state.
    fn tear_down(&mut self) -> Vec<command::AtomicCommit> {
        use std::io::Write;
        // Flush *before* swapping sinks back so buffered bytes drain
        // into the redirect target rather than land at the parent
        // destination after restoration. The libc-level stdout/stderr
        // flushes catch any unhandled fd-level redirect arm.
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        let _ = self.shell.turn.io.stdout.flush();
        let _ = self.shell.turn.io.stderr.flush();
        if let Some(s) = self.prev_stdout.take() {
            self.shell.turn.io.stdout = s;
        }
        if let Some(s) = self.prev_stderr.take() {
            self.shell.turn.io.stderr = s;
        }
        let commits = self
            .fd_guard
            .take()
            .map(command::restore_redirects)
            .unwrap_or_default();
        if let Some(g) = self.stdin_guard.take() {
            g.restore(self.shell);
        }
        commits
    }
}

impl Drop for RedirectFrame<'_> {
    fn drop(&mut self) {
        // Discard any commits returned on the panic / early-return
        // path: their `tempfile::NamedTempFile`s remove the staging
        // file on drop, so a half-written file never replaces the
        // target.
        let _ = self.tear_down();
    }
}

/// Runs `body` with `redirects` applied: open the targets, route fd
/// 1/2 through shell sinks, park `<file` input on
/// `shell.turn.io.stdin`, run, then always restore. Atomic-write
/// commits fire on success and are dropped on failure, so the staging
/// file is removed. When redirects are non-empty, stdout and stderr
/// are flushed before restoring sinks/fds, so buffered bytes land at
/// the redirect target rather than back at the parent.
///
/// Ordinary ral frames avoid duping process-global stdout/stderr:
/// libtest, the REPL frontend, and sibling ral threads all share those
/// fds.  The fd-level path remains for redirects we cannot model as
/// `Sink`s and for bundled uutils, whose upstream code writes through
/// libc rather than `Shell::write_stdout`.
///
/// `<file` redirects to fd 0 do *not* go through `dup2` — they are
/// opened by `install_stdin_redirect` and parked on
/// `shell.turn.io.stdin`. This keeps the cached `startup_stdin_tty`
/// honest: it is consulted only when `shell.turn.io.stdin` is
/// `Terminal`, which truly means "fall through to the inherited fd 0".
pub(crate) fn with_redirects<F>(
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
    body: F,
) -> Raw<Value>
where
    F: FnOnce(&mut Shell) -> Raw<Value>,
{
    if redirects.is_empty() {
        return body(shell);
    }
    let mut frame = RedirectFrame::enter(redirects, shell)?;
    let result = body(frame.shell);
    // Tear down explicitly so the pending atomic commits are
    // captured; `frame` then drops as a no-op because `tear_down`
    // zeroed its slots. Restoration completes here — before either
    // error propagation (`result?`) or commits fire — so
    // `commit_atomics` has a clean shell to write through on the
    // success path and `?` has a clean shell on the error path.
    let commits = frame.tear_down();
    // Settle the fd-1/2 write intents once the body result is known and
    // sinks are restored: this fires the per-intent atomic commits and
    // surfaces one write event per target. `frame` then drops as a
    // no-op (its slots are zeroed). The body error takes priority over
    // a commit failure, which in turn precedes the unhandled-fd commits.
    let write_result = frame.settle_writes(result.is_ok());
    drop(frame);
    let v = result?;
    write_result?;
    command::commit_atomics(commits)?;
    Ok(v)
}
