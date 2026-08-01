//! Stdio routing for a spawned external child: classify the call-site
//! redirects into a [`RedirectPlan`], then wire stdin, stdout and stderr
//! into the `Launch` per platform conventions.  Pipeline stages take
//! `pipeline::launch::wire_stage_stdio` and never come here.

use crate::syntax::ast::RedirectMode;
use crate::types::{Break, Error, Mooring, Settled, Shell};

use super::io_event;
use super::io_event::WriteOutcome;
#[cfg(windows)]
use super::process::pipe_err;
use super::redirect::{AtomicCommit, EvalRedirect, EvalRedirectV, open_file, stderr_mode};

/// Capability witness that the parent's fd 0 is safe to inherit into a
/// spawned child's stdin, mintable only through the issuers below.
///
/// A child whose pgid will not hold the foreground must get
/// `StdinRoute::Null` instead: hand it the terminal and the kernel SIGTTINs
/// it on its first read, leaving ral's pump waiting forever.
pub struct TtyInputPermit {
    _private: (),
}

impl TtyInputPermit {
    /// The child leads the foreground pgid, or runs non-interactively.
    pub(crate) fn for_standalone_external() -> Self {
        Self { _private: () }
    }

    /// The pipeline's own pgid takes the foreground via
    /// `PipelineGroup::claim_foreground`, so its members may read the tty.
    pub(crate) fn for_pure_external_pipeline() -> Self {
        Self { _private: () }
    }

    /// Nothing to SIGTTIN: the caller has just seen `startup_stdin_tty` false.
    pub(crate) fn for_non_tty_stdin() -> Self {
        Self { _private: () }
    }
}

/// How a child's stdin is wired; `Inherit` costs a [`TtyInputPermit`].
pub enum StdinRoute {
    Inherit(TtyInputPermit),
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
    Null,
}

impl StdinRoute {
    pub fn into_stdio(self) -> crate::process::StdioSpec {
        match self {
            Self::Inherit(_) => crate::process::StdioSpec::inherit(),
            Self::Pipe(r) => crate::process::StdioSpec::from_pipe_reader(r),
            Self::File(f) => crate::process::StdioSpec::from_file(f),
            Self::Null => crate::process::StdioSpec::null(),
        }
    }
}

/// Stdout / stderr routing for a child, derived once from the call-site
/// redirects.  Both `2>file` and `2>&1` claim fd 2, so `stderr_route` holds
/// whichever appears *last* in source order, POSIX applying redirects left
/// to right: `2>&1 2>/dev/null` silences, the reverse does not.
///
/// Stdin is deliberately absent — `<file` is opened upstream into
/// `shell.io.stdin` by `redirect::install_stdin_redirect`, so a native,
/// external and pipeline stage all read through the same `Source`.
pub(crate) struct RedirectPlan {
    pub(crate) stdout_file: Option<(String, RedirectMode)>,
    pub(crate) stderr_route: Option<StderrRoute>,
}

/// The fd-2 redirect left standing once every call-site redirect applies.
#[derive(Clone, Debug)]
pub(crate) enum StderrRoute {
    File(String, RedirectMode),
    Stdout,
}

/// Should the child inherit ral's fd 1 directly, so it can detect a TTY?
///
/// Only when nothing redirects stdout, fd 1 was a tty at startup, and the
/// sink still targets that real fd — `Sink::External` counts, the REPL's
/// printer writing there with no prompt drawn under a foreground child.
/// An `audit` capture installs a `Sink::Tee`, which fails here and so
/// falls through to the pump path unaided.
pub(super) fn inherit_tty(plan: &RedirectPlan, shell: &Shell) -> bool {
    plan.stdout_file.is_none()
        && shell.io.terminal.startup_stdout_tty
        && matches!(
            shell.io.stdout,
            crate::io::Sink::Terminal | crate::io::Sink::External(_)
        )
}

/// Classify the call-site redirects into a stdout/stderr plan.  Every shape
/// either lands a case below or becomes a named error; nothing is dropped in
/// silence, matching `redirect::apply_redirects` on the in-process path.
pub(crate) fn classify_redirects(redirects: &[EvalRedirectV]) -> Settled<RedirectPlan> {
    let mut plan = RedirectPlan {
        stdout_file: None,
        stderr_route: None,
    };
    for EvalRedirectV { fd, mode, target } in redirects {
        match target {
            EvalRedirect::Fd(target_fd) => {
                if *fd == 2 && *target_fd == 1 {
                    plan.stderr_route = Some(StderrRoute::Stdout);
                } else {
                    return Err(unmodeled_redirect(*fd, &format!("&{target_fd}")));
                }
            }
            EvalRedirect::File(filename) => match fd {
                // fd 0 (`< file`, `<< str`) is already parked on
                // `shell.io.stdin` by `install_stdin_redirect`.
                0 => {}
                1 => plan.stdout_file = Some((filename.clone(), *mode)),
                2 => plan.stderr_route = Some(StderrRoute::File(filename.clone(), *mode)),
                other => return Err(unmodeled_redirect(*other, filename)),
            },
        }
    }
    Ok(plan)
}

/// An fd ≥ 3 file target, or a `fd>&fd` dup other than the modeled `2>&1`.
fn unmodeled_redirect(fd: u32, target: &str) -> Break {
    Break::Error(Error::new(
        format!("redirect {fd}>{target} is not supported for external commands"),
        1,
    ))
}

/// Choose the stdin route for a single-command external job.
///
/// Every non-terminal source — a pipeline pipe, a `<file` opened by
/// `redirect::install_stdin_redirect` — already sits on `shell.io.stdin`;
/// only the fall-through needs a permit to inherit fd 0.
pub(super) fn wire_stdin(shell: &mut Shell) -> StdinRoute {
    // An explicit empty source (an exarch tool run) denies byte input in its
    // own right, so it wires `/dev/null` instead of falling through to fd 0.
    if matches!(shell.io.stdin, crate::io::Source::Empty) {
        return StdinRoute::Null;
    }
    match shell.io.stdin.take_reader() {
        Some(crate::io::SourceReader::Pipe(r)) => StdinRoute::Pipe(r),
        Some(crate::io::SourceReader::File(f)) => StdinRoute::File(f),
        None => {
            let permit = if shell.io.terminal.startup_stdin_tty {
                TtyInputPermit::for_standalone_external()
            } else {
                TtyInputPermit::for_non_tty_stdin()
            };
            StdinRoute::Inherit(permit)
        }
    }
}

/// Wire the child's stdout to the plan's redirect file, if any.
///
/// Returns the atomic-commit token (`Some` only for `>` onto a regular file)
/// plus, on Windows, a stderr handle: lacking `pre_exec` there, `2>&1` must
/// be dup'd pre-spawn from a clone of this same file.
///
/// A non-atomic target (`>>`, `>~`, or a `>` the atomic recipe declined) has
/// no later commit step, so its write door closes at the open and the event
/// fires here.  An atomic one waits on the rename in [`super::run`]'s
/// post-wait commit, which emits it there.
pub(super) fn wire_stdout_file(
    command: &mut crate::process::Launch,
    plan: &RedirectPlan,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<(Option<AtomicCommit>, Option<crate::process::StdioSpec>)> {
    let Some((path, mode)) = &plan.stdout_file else {
        return Ok((None, None));
    };
    let (file, commit) = open_file(path, *mode, shell)?;
    if commit.is_none() {
        mooring.emit_io(&io_event::write(
            path,
            *mode,
            WriteOutcome::Committed,
            None,
            None,
        ));
    }
    #[cfg(windows)]
    let stderr_dup = if matches!(plan.stderr_route, Some(StderrRoute::Stdout)) {
        Some(crate::process::StdioSpec::from_file(
            file.try_clone().map_err(|e| pipe_err(&e))?,
        ))
    } else {
        None
    };
    #[cfg(not(windows))]
    let stderr_dup = None;
    command.stdout(crate::process::StdioSpec::from_file(file));
    Ok((commit, stderr_dup))
}

/// Set up the child's stderr from the plan and `shell.io.stderr`, returning
/// `true` when fd 2 was piped and the caller must drain it into the sink.
///
/// `2>&1` splits by platform: Unix registers a `pre_exec` `dup2` that runs
/// after the kernel wired fd 1, so stderr follows stdout wherever it went;
/// Windows, having no `pre_exec`, dups pre-spawn from a clone of the handle
/// about to become stdout — `stdout_file_dup`, a re-clone of
/// `shell.io.stdout`, or the parent's own stdout handle under inherit-tty.
///
/// Audit capture belongs elsewhere: standalone exec tees at dispatch level
/// in `evaluator::with_audit_capture`, pipeline stages in
/// `pipeline::launch::wire_stage_stdio`.
pub(super) fn wire_stderr(
    command: &mut crate::process::Launch,
    plan: &RedirectPlan,
    inherit_tty: bool,
    stdout_file_dup: Option<crate::process::StdioSpec>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<bool> {
    match &plan.stderr_route {
        Some(StderrRoute::Stdout) => {
            #[cfg(unix)]
            {
                let _ = (inherit_tty, stdout_file_dup);
                command.dup_stdout_to_stderr();
            }
            #[cfg(windows)]
            {
                let stdio = if let Some(dup) = stdout_file_dup {
                    dup
                } else if inherit_tty || matches!(shell.io.stdout, crate::io::Sink::Terminal) {
                    // The child inherits our fd 1, so clone fd 1 — not fd 2 —
                    // for its stderr.  The bare inherit below would hand it
                    // fd 2, routing diagnostics straight past `2>&1`.
                    use std::os::windows::io::AsHandle;
                    let owned = std::io::stdout()
                        .as_handle()
                        .try_clone_to_owned()
                        .map_err(|e| pipe_err(&e))?;
                    crate::process::StdioSpec::from_owned_handle(owned)
                } else {
                    // Every remaining sink pumps child.stdout through a pipe
                    // that `Stdio::piped()` allocates only at spawn, so stderr
                    // cannot share it.  Inherit, so diagnostics surface.
                    crate::process::StdioSpec::inherit()
                };
                command.stderr(stdio);
            }
            Ok(false)
        }
        Some(StderrRoute::File(path, mode)) => {
            let effective_mode = stderr_mode(*mode);
            let (file, _) = open_file(path, effective_mode, shell)?;
            command.stderr(crate::process::StdioSpec::from_file(file));
            // `stderr_mode` coerces `>` to streaming, so a stderr redirect is
            // never atomic and its write door closes at the open.
            mooring.emit_io(&io_event::write(
                path,
                effective_mode,
                WriteOutcome::Committed,
                None,
                None,
            ));
            Ok(false)
        }
        None if !matches!(shell.io.stderr, crate::io::Sink::Stderr) => {
            // A non-default sink (audit tee, replay buffer, watch line-framer)
            // wants the bytes in-process, so pipe fd 2 for the caller to pump.
            command.stderr(crate::process::StdioSpec::piped());
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
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
            matches!(wire_stdin(&mut shell), StdinRoute::Null),
            "Empty stdin must wire to /dev/null"
        );

        shell.io.stdin = Source::Terminal;
        shell.io.terminal.startup_stdin_tty = false;
        assert!(
            matches!(wire_stdin(&mut shell), StdinRoute::Inherit(_)),
            "Terminal stdin still inherits fd 0"
        );
    }
}
