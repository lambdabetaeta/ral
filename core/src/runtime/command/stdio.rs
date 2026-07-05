//! Stdio routing for a spawned external child: classify the call-site
//! redirects, derive a [`RedirectPlan`], and wire each of stdin / stdout /
//! stderr into the `Command` per platform conventions.  Atomic-commit token
//! plumbing for `> file` is delegated to [`super::redirect::open_file`].

use crate::syntax::ast::RedirectMode;
use crate::types::*;

#[cfg(windows)]
use super::process::pipe_err;
use super::redirect::{AtomicCommit, open_file};

/// A redirect whose target has been evaluated to a concrete file
/// path or fd.  The evaluator walks the AST's `ValRedirectTarget`
/// and hands one of these to [`super::run`] and to
/// [`super::redirect::apply_redirects`] (for builtins).
#[derive(Clone, Debug)]
pub(crate) enum EvalRedirect {
    File(String),
    Fd(u32),
}

/// One evaluated redirect: the runtime counterpart of the IR's
/// [`crate::ir::RedirectV`], with `target` already resolved to a
/// concrete file path or fd.
#[derive(Clone, Debug)]
pub(crate) struct EvalRedirectV {
    pub(crate) fd: u32,
    pub(crate) mode: RedirectMode,
    pub(crate) target: EvalRedirect,
}

/// Capability witness that fd 0 of the parent process is safe to inherit
/// into a spawned child's stdin in the current context.
///
/// Constructible only via the named issuers below.  Each issuer documents
/// the discipline that justifies the inheritance: a standalone foreground
/// external owns the tty itself; a pure-external pipeline pgid will own
/// the tty via `claim_foreground`; a non-tty parent fd 0 cannot SIGTTIN
/// anyone.
///
/// Mixed pipelines must NOT mint a permit — handing terminal stdin to an
/// external in a backgrounded pgid would SIGTTIN the child the moment it
/// reads, and ral's pump would then wait forever.  The audit's high #2.
pub struct TtyInputPermit {
    _private: (),
}

impl TtyInputPermit {
    /// Issued for a standalone external job: the spawned process either
    /// becomes the foreground pgid leader (and so owns the tty) or runs
    /// non-interactively where SIGTTIN is moot.
    pub(crate) fn for_standalone_external() -> Self {
        Self { _private: () }
    }

    /// Issued for the first stage of a pure-external pipeline: the
    /// pipeline's own pgid will be foregrounded, so its members can read
    /// from the tty without SIGTTIN.
    pub(crate) fn for_pure_external_pipeline() -> Self {
        Self { _private: () }
    }

    /// Issued when fd 0 of the parent is not a controlling terminal —
    /// inheritance cannot SIGTTIN anyone.  Caller must have just observed
    /// `terminal.startup_stdin_tty == false`.
    pub(crate) fn for_non_tty_stdin() -> Self {
        Self { _private: () }
    }
}

/// How a spawned child's stdin will be wired.
///
/// `Inherit` requires a [`TtyInputPermit`] so that grep'ing for the route
/// makes the discipline visible: the permit is the auditable place where
/// "yes, I checked, this is safe to inherit" lives.  Mixed pipelines
/// produce `Null` instead of `Inherit` for stages with no upstream pipe.
pub enum StdinRoute {
    Inherit(TtyInputPermit),
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
    Null,
}

impl StdinRoute {
    pub fn into_stdio(self) -> crate::process::StdioSpec {
        match self {
            StdinRoute::Inherit(_) => crate::process::StdioSpec::inherit(),
            StdinRoute::Pipe(r) => crate::process::StdioSpec::from_pipe_reader(r),
            StdinRoute::File(f) => crate::process::StdioSpec::from_file(f),
            StdinRoute::Null => crate::process::StdioSpec::null(),
        }
    }
}

/// Routing decisions for a child process's stdout / stderr, derived once
/// from the call-site redirects.  `stderr_to_stdout` captures the `2>&1` fd
/// dup; the file fields carry their `RedirectMode` so `open_file` can pick
/// create-vs-append.
///
/// Stdin is *not* on the plan: `<file` is opened upstream and parked in
/// `shell.turn.io.stdin` by [`super::redirect::install_stdin_redirect`], so every
/// dispatch arm (builtin, external, pipeline stage) reads input through the
/// same `Source` channel.  See the module docstring on the cached-tty bug
/// class.
pub(crate) struct RedirectPlan {
    pub(crate) stdout_file: Option<(String, RedirectMode)>,
    pub(crate) stderr_file: Option<(String, RedirectMode)>,
    pub(crate) stderr_to_stdout: bool,
}

/// Should the child inherit ral's stdout fd directly so it can detect a
/// TTY (colour, pager selection, etc.)?
///
/// Holds when there is no `>file` redirect on stdout, fd 1 was a tty at
/// startup, and the active sink targets that real fd 1 (`Sink::Terminal`,
/// or `Sink::External` — the REPL's rustyline printer also writes there;
/// for foreground commands the prompt is not drawn, so a direct dup is
/// safe and is the only way `ls` / `grep` / pagers see a TTY).  Under
/// `audit` the dispatch-level capture installs a `Sink::Tee` on
/// `shell.turn.io.stdout`, so this falls through to the pump path
/// automatically — no separate `auditing` predicate needed.
pub(super) fn inherit_tty(plan: &RedirectPlan, shell: &Shell) -> bool {
    plan.stdout_file.is_none()
        && shell.turn.io.terminal.startup_stdout_tty
        && matches!(
            shell.turn.io.stdout,
            crate::io::Sink::Terminal | crate::io::Sink::External(_)
        )
}

/// Coerce `>` to streaming for stderr — atomic semantics make no sense for
/// diagnostic output.  All other modes pass through unchanged.
pub(crate) fn stderr_mode(mode: &RedirectMode) -> RedirectMode {
    match mode {
        RedirectMode::Write => RedirectMode::StreamWrite,
        other => *other,
    }
}

pub(crate) fn classify_redirects(redirects: &[EvalRedirectV]) -> RedirectPlan {
    let mut plan = RedirectPlan {
        stdout_file: None,
        stderr_file: None,
        stderr_to_stdout: false,
    };
    for EvalRedirectV { fd, mode, target } in redirects {
        match target {
            EvalRedirect::Fd(target_fd) => {
                if *fd == 2 && *target_fd == 1 {
                    plan.stderr_to_stdout = true;
                }
            }
            EvalRedirect::File(filename) => match fd {
                // fd 0 is handled by install_stdin_redirect upstream.
                1 => plan.stdout_file = Some((filename.clone(), *mode)),
                2 => plan.stderr_file = Some((filename.clone(), *mode)),
                _ => {}
            },
        }
    }
    plan
}

/// Choose the stdin route for a single-command external job.
///
/// All non-terminal sources (pipeline pipe, `<file` redirect already opened
/// by [`super::redirect::install_stdin_redirect`]) flow through
/// `shell.turn.io.stdin`; this routine just consumes whatever sits there.  The
/// fall-through inherit case is gated on a `TtyInputPermit`: inheriting fd
/// 0 when the parent's fd 0 is the controlling tty is only safe when this
/// child will hold the foreground itself (`for_standalone_external`) or
/// when fd 0 is not actually a tty (`for_non_tty_stdin`).  Both are issued
/// here.
pub(super) fn wire_stdin(shell: &mut Shell) -> StdinRoute {
    // An explicit empty source (an exarch tool turn) wires to `/dev/null`,
    // never inheriting fd 0 — denial of byte input is its own effect, not a
    // consequence of foreground denial.
    if matches!(shell.turn.io.stdin, crate::io::Source::Empty) {
        return StdinRoute::Null;
    }
    match shell.turn.io.stdin.take_reader() {
        Some(crate::io::SourceReader::Pipe(r)) => StdinRoute::Pipe(r),
        Some(crate::io::SourceReader::File(f)) => StdinRoute::File(f),
        None => {
            let permit = if shell.turn.io.terminal.startup_stdin_tty {
                TtyInputPermit::for_standalone_external()
            } else {
                TtyInputPermit::for_non_tty_stdin()
            };
            StdinRoute::Inherit(permit)
        }
    }
}

/// Wire the child's stdout to a redirect file when one is set.
///
/// Returns the atomic-commit token (`Some` for `>` to a regular file) plus an
/// optional `Stdio` handle to assign to the child's stderr — populated only on
/// Windows when the redirect plan has `2>&1`, since Windows lacks `pre_exec`
/// and the dup must be wired pre-spawn from a clone of the same handle.
pub(super) fn wire_stdout_file(
    command: &mut crate::process::Launch,
    plan: &RedirectPlan,
    shell: &mut Shell,
) -> Settled<(Option<AtomicCommit>, Option<crate::process::StdioSpec>)> {
    let Some((path, mode)) = &plan.stdout_file else {
        return Ok((None, None));
    };
    let (file, commit) = open_file(path, mode, shell)?;
    #[cfg(windows)]
    let stderr_dup = if plan.stderr_to_stdout {
        Some(crate::process::StdioSpec::from_file(
            file.try_clone().map_err(pipe_err)?,
        ))
    } else {
        None
    };
    #[cfg(not(windows))]
    let stderr_dup = None;
    command.stdout(crate::process::StdioSpec::from_file(file));
    Ok((commit, stderr_dup))
}

/// Set up the child's stderr according to the redirect plan and the
/// caller's `shell.turn.io.stderr` sink.  Four branches, in priority order:
///
///   1. `2>&1` — stderr follows stdout (see platform note below).
///   2. `2>file` — open the file, hand fd 2 to it.
///   3. Non-default `shell.turn.io.stderr` (capture buffer, replay tee, watch
///      line-framer, etc.) — pipe fd 2 and signal the caller to pump.
///   4. Default `Sink::Stderr` — inherit fd 2 directly.
///
/// `2>&1` is realised differently per platform:
///   * Unix: a `pre_exec` `dup2(STDOUT, STDERR)` runs after the kernel has
///     wired the child's fd 1, so stderr inherits whatever stdout was pointed
///     at — pipe, file, or inherited fd.
///   * Windows: there is no `pre_exec`, so the dup must happen pre-spawn by
///     cloning the same handle that's about to be assigned as stdout.
///     `stdout_file_dup` carries that clone for the file-redirect case;
///     for sink-driven stdout we re-clone the writer from `shell.turn.io.stdout`,
///     and for the inherit-tty case we duplicate the parent's `STDOUT_HANDLE`.
///
/// Returns `true` when stderr is piped and a pump thread or reader must
/// drain `child.stderr` after spawn (callers handle the sink connection).
/// Audit-mode capture is *not* this function's concern: standalone exec
/// captures at dispatch level via `with_audit_capture`, and pipeline
/// stages route stderr inline in `pipeline::stages` without coming here.
pub(super) fn wire_stderr(
    command: &mut crate::process::Launch,
    plan: &RedirectPlan,
    inherit_tty: bool,
    stdout_file_dup: Option<crate::process::StdioSpec>,
    shell: &mut Shell,
) -> Settled<bool> {
    if plan.stderr_to_stdout {
        #[cfg(unix)]
        {
            let _ = (inherit_tty, stdout_file_dup);
            command.dup_stdout_to_stderr();
        }
        #[cfg(windows)]
        {
            let stdio = if let Some(dup) = stdout_file_dup {
                dup
            } else if inherit_tty || matches!(shell.turn.io.stdout, crate::io::Sink::Terminal) {
                // The child inherits the helper's fd 1.  Duplicate fd 1 for
                // stderr so `2>&1` routes diagnostics to whatever fd 1 points
                // at — terminal, parent pipe, or a downstream pipeline stage
                // (the helper's own stdout was preset to a pipe writer by
                // `wire_ral_stdio` in that last case).  Without this, the
                // bare `Stdio::inherit()` fallback below would dup fd 2,
                // which silently routes diagnostics past `2>&1`.
                use std::os::windows::io::AsHandle;
                let owned = std::io::stdout()
                    .as_handle()
                    .try_clone_to_owned()
                    .map_err(pipe_err)?;
                crate::process::StdioSpec::from_owned_handle(owned)
            } else {
                match &shell.turn.io.stdout {
                    crate::io::Sink::Pipe(w) => crate::process::StdioSpec::from_pipe_writer(
                        w.try_clone().map_err(pipe_err)?,
                    ),
                    // Buffer / Tee / LineFramed sinks pump child.stdout via an
                    // anonymous pipe allocated by `Stdio::piped()`; we cannot
                    // reach into that pipe pre-spawn to share it with stderr.
                    // Fall back to inherit so diagnostics still surface.
                    _ => crate::process::StdioSpec::inherit(),
                }
            };
            command.stderr(stdio);
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (inherit_tty, stdout_file_dup);
            command.stderr(crate::process::StdioSpec::inherit());
        }
        Ok(false)
    } else if let Some((path, mode)) = &plan.stderr_file {
        let (file, _) = open_file(path, &stderr_mode(mode), shell)?;
        command.stderr(crate::process::StdioSpec::from_file(file));
        Ok(false)
    } else if !matches!(shell.turn.io.stderr, crate::io::Sink::Stderr) {
        // Non-default stderr sink: dispatch-level audit Tee, §13.3 replay
        // buffer, watch line-framer, etc.  Pipe the child's stderr; the
        // caller pumps it into the sink.
        command.stderr(crate::process::StdioSpec::piped());
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Announce the running command via the terminal title (OSC 0).
pub(super) fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.turn.io.interactive && shell.turn.io.terminal.ui_title_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::{StdinRoute, wire_stdin};
    use crate::io::Source;
    use crate::types::Shell;

    /// The foreground/input split, at the wiring level: an explicit empty
    /// stdin source wires a child to `/dev/null` (no fd-0 fall-through), while
    /// the ambient terminal source still inherits fd 0. Denial of byte input
    /// (`Empty`) is its own effect — a piped `ral -c` is denied foreground yet
    /// keeps reading its inherited pipe through the `Terminal` (inherit) route.
    #[test]
    fn empty_stdin_wires_null_but_terminal_inherits() {
        let mut shell = Shell::default();

        shell.turn.io.stdin = Source::Empty;
        assert!(
            matches!(wire_stdin(&mut shell), StdinRoute::Null),
            "Empty stdin must wire to /dev/null"
        );

        // The ambient terminal/pipe source falls through to fd 0 (inherit),
        // independent of any foreground decision.
        shell.turn.io.stdin = Source::Terminal;
        shell.turn.io.terminal.startup_stdin_tty = false;
        assert!(
            matches!(wire_stdin(&mut shell), StdinRoute::Inherit(_)),
            "Terminal stdin still inherits fd 0"
        );
    }
}
