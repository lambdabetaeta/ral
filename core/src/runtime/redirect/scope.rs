//! Redirect scopes: open the targets, route fd 1/2 through the shell's
//! sinks, run a body, restore.  [`RedirectState`] is the one interpreter of a
//! redirect list, entered by `evaluator::machine`'s `Frame::Redirect` and by
//! [`with_redirects`] for a synchronous call; the targets themselves are
//! closed in `machine::close_redirects`, over the machine's own `Env`.

use super::{OpenedWrite, StdinRedirectGuard, install_stdin_redirect, open_write};
use crate::fact::Write;
use crate::io::Sink;
use crate::ir::{Redirects, StderrTarget, WriteMode};
use crate::source::Span;
use crate::types::{Bytes, Mooring, Observed, Settled, Shell, Value, WriteOutcome};

/// What the body's result means for the writes this frame staged.
#[derive(Clone, Copy)]
pub(crate) enum WriteFate {
    /// The body settled: rename each temp onto its target.
    Commit,
    /// The body broke: drop each staged write, leaving its target as it was.
    Abort,
}

/// A write target the frame opened, held until it settles so the write can
/// be reported as what it did: the target before the body ran, and after.
struct WriteIntent {
    path: String,
    mode: WriteMode,
    opened: OpenedWrite,
    /// Taken at the open, before any byte lands, and only for an ear to hear it.
    before: Option<Vec<u8>>,
}

/// The installed redirect state, owned — no borrow of `Shell` or
/// `Mooring` survives `enter`. Undone by `leave`, called explicitly:
/// on the normal path by every caller below, on a panic by `abandon` — the
/// machine's own unwind walk calls it for a `Frame::Redirect` on its stack;
/// [`with_redirects`] calls it itself via `catch_unwind`, for a synchronous
/// call, which pushes no frame of its own.
pub(crate) struct RedirectState {
    stdin_guard: Option<StdinRedirectGuard>,
    prior: PriorSinks,
    write_intents: Vec<WriteIntent>,
    /// The redirect's own site: every entry it makes carries it.
    span: Option<Span>,
}

/// The sinks a frame displaced, `Some` only where it installed its own.
struct PriorSinks {
    stdout: Option<Sink>,
    stderr: Option<Sink>,
}

/// Opens one fd-1/2 write target as an intent the frame settles.  An open
/// that fails is reported here, the one write that never reaches settle.
fn open_redirect_sink(
    path: &str,
    mode: WriteMode,
    mooring: &Mooring,
    shell: &mut Shell,
    intents: &mut Vec<WriteIntent>,
) -> Settled<Sink> {
    let (file, opened) = open_write(path, mode, shell).inspect_err(|_| {
        let what = Observed::Write(Write {
            path: path.to_string(),
            mode,
            outcome: WriteOutcome::Failed,
            new_bytes: None,
            old_bytes: None,
        });
        shell.observe(mooring, what);
    })?;
    if let Some(opened) = opened {
        let before = shell
            .listening(mooring)
            .then(|| opened.before(shell))
            .flatten();
        intents.push(WriteIntent {
            path: path.to_string(),
            mode,
            opened,
            before,
        });
    }
    Ok(Sink::File(std::sync::Arc::new(file)))
}

/// Reports one write per intent, committing or dropping each atomic one by
/// `fate`; dropping is what unlinks its staging file.  Returns the first
/// commit failure.
fn settle(
    intents: Vec<WriteIntent>,
    fate: WriteFate,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    let heard = shell.listening(mooring);
    let mut commit_err: Settled<()> = Ok(());
    for WriteIntent {
        path,
        mode,
        opened,
        before,
    } in intents
    {
        // Read before a commit renames the staged file away.
        let after = heard.then(|| opened.after(shell)).flatten();
        let (outcome, landed) = match (opened, fate) {
            (OpenedWrite::Atomic(_), WriteFate::Abort) => (WriteOutcome::Aborted, false),
            (OpenedWrite::Atomic(commit), WriteFate::Commit) => match commit.commit() {
                Ok(()) => (WriteOutcome::Committed, true),
                Err(e) => {
                    if commit_err.is_ok() {
                        commit_err = Err(crate::types::Error::io("atomic write", &e).into());
                    }
                    (WriteOutcome::Failed, false)
                }
            },
            // A stream committed each byte as it landed: it cannot abort.
            (OpenedWrite::Stream(_), _) => (WriteOutcome::Committed, true),
        };
        // An atomic write that did not land left its target as it was.
        let (old_bytes, new_bytes) = (before.filter(|_| landed), after.filter(|_| landed));
        let what = Observed::Write(Write {
            path,
            mode,
            outcome,
            new_bytes: new_bytes.map(Bytes::from),
            old_bytes: old_bytes.map(Bytes::from),
        });
        shell.observe(mooring, what);
    }
    commit_err
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
        let stdin_guard = install_stdin_redirect(redirects.stdin.as_ref(), mooring, shell)?;
        let mut write_intents = Vec::new();
        let prior = match install_sink_redirects(redirects, mooring, shell, &mut write_intents) {
            Ok(prior) => prior,
            Err(e) => {
                // An abort cannot fail to commit.
                let _ = settle(write_intents, WriteFate::Abort, mooring, shell);
                stdin_guard.restore(shell);
                return Err(e);
            }
        };
        Ok(Self {
            stdin_guard: Some(stdin_guard),
            prior,
            write_intents,
            span,
        })
    }

    /// Restores the sinks and stdin, then settles the writes: with the
    /// redirected handles dropped, each atomic commit fires on `Commit` and is
    /// abandoned on `Abort`, and every write is reported.  Returns the first
    /// commit failure.
    pub(crate) fn leave(
        &mut self,
        fate: WriteFate,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<()> {
        self.tear_down(shell);
        let span = self.span;
        let intents = std::mem::take(&mut self.write_intents);
        shell.at_site(span, |shell| settle(intents, fate, mooring, shell))
    }

    /// Flushes, then restores the sinks and stdin. Idempotent: a second call
    /// sees only emptied slots.
    fn tear_down(&mut self, shell: &mut Shell) {
        use std::io::Write;
        // Flush before swapping the sinks back, or buffered bytes land at
        // the parent.
        let _ = shell.io.stdout.flush();
        let _ = shell.io.stderr.flush();
        if let Some(s) = self.prior.stdout.take() {
            shell.io.stdout = s;
        }
        if let Some(s) = self.prior.stderr.take() {
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
