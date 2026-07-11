//! Executes an external command.
//!
//! Where `runtime/command_call` decides
//! which arm — Handler, Builtin, or External — serves a given call,
//! this module owns the External arm: vetting the resolved identity,
//! spawning the OS process, and wiring its stdio per the call's
//! redirects.
//!
//! Submodules:
//!
//!   * [`identity`] — build a [`CommandIdentity`] from the head.
//!   * [`vet`] — admit the identity for spawn (argv shape, grant
//!     verdict).
//!   * [`process`] — turn the vetted identity into a
//!     `std::process::Command` and spawn it under the canonical pgid
//!     and sandbox.
//!   * [`stdio`] — classify call-site redirects and wire fds 0/1/2
//!     into the `Command`.
//!   * [`redirect`] — redirect file opening, the `apply_redirects`
//!     dup machinery for bundled uutils / unsupported fd shapes, and
//!     the `<file → fd 0` handoff.
//!   * [`child`] — typestated [`RunningChild`] / `WaitedChild` handles
//!     that own the spawned process and its drainer threads.
//!   * [`foreground`] — the decision to place a standalone external
//!     in the foreground (`PgidPolicy` + RAII terminal handoff).
//!   * [`uutils`] — coreutils / diffutils / ripgrep dispatch, bundled
//!     into ral.
//!
//! [`run`] orchestrates the standalone case. Pipeline stages reuse
//! the same `build_command` / `spawn` primitives directly through
//! [`super::pipeline::run_pipeline`].

use crate::types::{Break, Error, Raw, Shell, Value};

mod child;
mod foreground;
mod identity;
pub(crate) mod io_event;
pub(crate) mod process;
mod redirect;
mod stdio;
mod uutils;
mod vet;

pub(crate) use child::{ExternalPlumbing, GroupOwner, RunningChild};
pub(crate) use identity::CommandIdentity;
pub(crate) use process::{build_command, spawn_error};
pub(crate) use redirect::{
    AtomicCommit, EvalRedirect, EvalRedirectV, RedirectGuard, StdinRedirectGuard, apply_redirects,
    atomic_write, commit_atomics, install_stdin_redirect, open_file, restore_redirects, stderr_mode,
};
use stdio::classify_redirects;
pub(crate) use stdio::{StdinRoute, TtyInputPermit};
use vet::ExecImage;
pub(crate) use vet::vet;

use child::WaitedChild;
use foreground::ForegroundDecision;
use process::{pipe_err, spawn};
use stdio::{inherit_tty, wire_stderr, wire_stdin, wire_stdout_file};

/// Executes a standalone external call: vet the identity, route
/// bundled uutils invocations through the in-process or
/// pipeline-helper path when applicable, otherwise wire stdio, spawn
/// the child under the canonical pgid and sandbox, and reap.
/// Pipeline stages reuse the same `build_command` / `spawn` primitives
/// via [`super::pipeline::run_pipeline`], so this function and that path
/// share one resolution and one stdio rendering of every call.
pub(crate) fn run(
    id: &CommandIdentity,
    args: &[Value],
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Raw<Value> {
    let rc = vet(id, args, shell)?;
    let cmd_name = rc.shown.clone();

    // A bundled uutils image keeps an inline placement only for the
    // clean-terminal case: direct terminal/stderr sinks, no redirects,
    // no capture/audit tee, no env overrides, no `within [dir: …]`, no
    // logical/process cwd mismatch, and no active sandbox projection.
    // The `redirects.is_empty()` gate means `run_uutils_in_process`
    // wires no `> file` plumbing.  Any other bundled invocation falls
    // through to the ordinary spawn path below as
    // `ral --ral-bundled-tool <tool> …`, an ordinary child whose
    // redirects become child stdio and whose env/cwd come from
    // `apply_env`.
    #[cfg(all(
        any(unix, windows),
        any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
    ))]
    if let ExecImage::BundledTool { tool } = &rc.image
        && redirects.is_empty()
        && uutils::can_run_uutils_in_process(shell)
    {
        return uutils::run_uutils_in_process(tool, &rc.args, shell)
            .map_err(crate::types::Control::from);
    }

    let mut command = build_command(&rc, shell)?;

    let plan = classify_redirects(redirects)?;
    command.stdin(wire_stdin(shell).into_stdio());
    let (mut atomic_commit, stdout_file_dup) = wire_stdout_file(&mut command, &plan, shell)?;
    let inherit_tty = inherit_tty(&plan, shell);

    // Wire stdout before stderr: on Windows, `wire_stderr`'s `2>&1`
    // case reads `shell.turn.io.stdout` to clone a writer; on Unix,
    // the `pre_exec` dup2 needs a real stdout fd in place pre-fork.
    let stdout_plan = if plan.stdout_file.is_none() {
        let p = shell
            .turn
            .io
            .stdout
            .child_stdout(inherit_tty)
            .map_err(|e| pipe_err(&e))?;
        command.stdout(p.stdio);
        p.pump
    } else {
        None
    };
    let needs_pump = stdout_plan.is_some();

    let stderr_piped = wire_stderr(&mut command, &plan, inherit_tty, stdout_file_dup, shell)?;

    // Stderr pump destination: clone `shell.turn.io.stderr` (which
    // may itself be a Tee under dispatch-level audit capture) when
    // stderr was piped; otherwise there is nothing for ral to drain
    // (file / 2>&1 / inherited fd 2). The clone happens pre-spawn so
    // that no fallible work remains between `spawn` and
    // `RunningChild::assemble` — the `?` here cannot leak a child.
    let stderr_pump = if stderr_piped {
        Some(shell.turn.io.stderr.try_clone().map_err(|e| pipe_err(&e))?)
    } else {
        None
    };

    let fg = ForegroundDecision::for_standalone(shell, needs_pump);
    let image_shown = match &rc.image {
        ExecImage::Host(p) => p.clone(),
        ExecImage::BundledTool { tool } => format!("ral --ral-bundled-tool {tool}"),
    };
    trace_io_wiring(&cmd_name, &image_shown, inherit_tty, needs_pump, &fg, shell);

    announce_command_title(&cmd_name, shell);

    // Wall-window start for the sandbox-denial reader: anchored before
    // the spawn so a kernel deny logged by the child falls inside the
    // window the failure arm reads back (see `sandbox::diag`).
    let started = std::time::Instant::now();
    let (child, wait_pgid) = match spawn(&mut command, fg.pgid_policy(), shell) {
        Ok(pair) => pair,
        Err(e) => {
            // Door 3 — EXEC (spawn failure): the synthesized exit code is
            // exactly the status the resulting failure yields (NotFound→127,
            // PermissionDenied→126, else 127), so read it off the very Break
            // we are about to propagate.  Outcome is "bad" (nonzero status).
            let failure = spawn_error(&cmd_name, &e);
            let status = match &failure {
                Break::Error(err) => err.exit_code(),
                Break::Escape(_) => 127,
            };
            shell.emit_io(&io_event::exec(&cmd_name, &rc.args, status));
            return Err(failure.into());
        }
    };

    let child_pid = child.id();
    let _fg_guard = fg.acquire(child_pid, shell);

    // The `_fg_guard` above hands the terminal to the child through
    // an RAII `ForegroundGuard`: its `Drop` restores ral's pgid no
    // matter how this function returns. The shell ignores SIGTTIN,
    // so a missed restore would put the next REPL read into EIO —
    // making the restore RAII-managed is the only reliable way to
    // cover every early-return path between `spawn` and `wait()`.
    //
    // Ownership of the in-flight resources is then handed to a
    // `RunningChild`. No fallible work runs between `spawn` above
    // and this point, so the bare `child` cannot leak: assembly is
    // unconditional from spawn-success. From here on any error
    // triggers `RunningChild`'s `Drop`, which SIGKILLs the pgid and
    // reaps.
    //
    // Standalone foreground externals of an interactive REPL park their
    // pgid as a stopped job when SIGTSTP / SIGSTOP arrives (Ctrl-Z on
    // vim, less, man, …) so the shell can later `fg` them. A child that
    // takes foreground from a non-interactive script (`ral run-claude.ral`)
    // must NOT park: there is no job table to `fg` it, so a stopped
    // child is killed and reaped instead. The same holds for a
    // background external leading its own group for cancel tree-kill.
    let park_on_stop = fg.park_on_stop();
    // A `NewLeader` standalone external on Windows owns its Job
    // Object directly; `Inherit` children have no group at all. The
    // ownership tag drives both the abort-path teardown and the
    // success-path release.
    let group_owner = if wait_pgid.is_some() {
        GroupOwner::Standalone
    } else {
        GroupOwner::None
    };
    let running = RunningChild::assemble_with_owner(
        child,
        wait_pgid,
        cmd_name.clone(),
        ExternalPlumbing {
            stdout_pump: stdout_plan,
            stderr_pump,
        },
        park_on_stop,
        group_owner,
        shell.turn.cancel.as_scope().clone(),
    );

    // `wait` consumes `RunningChild`; the resulting `WaitedChild` is
    // the only handle that lets us read `status` or join the pump
    // threads. All post-wait work below therefore carries a
    // borrow-check proof of "child has been observed dead".
    let waited: WaitedChild = running.wait()?;
    let outcome = waited.outcome;

    // Settle the pending atomic commit (if any) and surface its write
    // event now, but hold the result rather than `?`-propagating it here:
    // the post-run bookkeeping below (drain the pumps, set `$?`, emit the
    // exec event) must run on every exit path, including a commit failure
    // — a straight-line `?` here would skip it for a command that did run.
    let commit_result = if let Some(commit) = atomic_commit.take() {
        let (path, mode) = plan
            .stdout_file
            .as_ref()
            .expect("atomic_commit is only Some when plan.stdout_file is Some");
        if outcome.is_success() {
            let old_bytes = commit.old_snapshot_for_diff();
            let preview = commit.temp_preview();
            match commit.commit() {
                Ok(()) => {
                    shell.emit_io(&io_event::write(
                        path,
                        *mode,
                        io_event::WriteOutcome::Committed,
                        preview.as_deref(),
                        old_bytes.as_deref(),
                    ));
                    Ok(())
                }
                Err(e) => {
                    shell.emit_io(&io_event::write(
                        path,
                        *mode,
                        io_event::WriteOutcome::Failed,
                        None,
                        None,
                    ));
                    Err(Break::Error(Error::new(format!("atomic write: {e}"), 1)))
                }
            }
        } else {
            // The command did not succeed: the write door did not
            // complete. `commit` drops here, discarding the staged temp.
            shell.emit_io(&io_event::write(
                path,
                *mode,
                io_event::WriteOutcome::Aborted,
                None,
                None,
            ));
            Ok(())
        }
    } else {
        Ok(())
    };

    // Join the pump threads. When audit is active, bytes are
    // captured by the dispatch-level `with_audit_capture` Tee on
    // `shell.turn.io.stdout` / `shell.turn.io.stderr`, so there is
    // nothing for this site to drain.
    waited.drain();
    let code = outcome.to_user_exit_code();
    shell.mobile.control.last_status = code;
    // Door 3 — EXEC (external + spawned-bundled completion): the inline
    // bundled fast path returned long before the spawn above, so this door
    // covers only the Host and spawned `BundledTool` images.  `code` is the
    // user-visible exit status; outcome is "ok" iff it is zero.
    shell.emit_io(&io_event::exec(&cmd_name, &rc.args, code));
    commit_result?;
    // A child whose `LaunchRole` is `PipelineStage` (pipeline stages, the
    // pipeline helper subprocess) forgives SIGPIPE — the reader ended the
    // pipe, not a real error.
    let forgive_sigpipe = !shell.turn.io.launch_role.is_top_level();
    match crate::process::CommandFailure::from_outcome(outcome, forgive_sigpipe) {
        None => Ok(Value::Unit),
        Some(failure) => {
            let err = Error::from_command_failure(
                &cmd_name,
                failure,
                shell.turn.loc.source_loc(cmd_name.len()),
                shell,
            );
            // When this child ran under an active OS sandbox, attach the
            // kernel-reported denial (if any) — exactly this child plus
            // its descendants attribute the deny lines.  The augment
            // gate short-circuits on the common non-sandbox failure.
            let mut pids = crate::sandbox::sample_descendants(child_pid);
            pids.insert(child_pid);
            let err = crate::sandbox::augment_failure(err, shell, &pids, started);
            Err(Break::Error(err).into())
        }
    }
}

/// Announce the running command via the terminal title (OSC 0).
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.turn.io.interactive && shell.turn.io.terminal.ui_title_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// Debug-only single-line trace of the I/O wiring decisions for an
/// external command. Compiled away in release builds — the body's
/// `dbg_trace!` is itself a no-op there, but the surrounding match
/// would still run, so the entire function is cfg-gated.
#[cfg(debug_assertions)]
fn trace_io_wiring(
    cmd_name: &str,
    resolved: &str,
    inherit_tty: bool,
    needs_pump: bool,
    fg: &ForegroundDecision,
    shell: &Shell,
) {
    let sink = match &shell.turn.io.stdout {
        crate::io::Sink::Terminal => "Terminal",
        crate::io::Sink::External(_) => "External",
        crate::io::Sink::Pipe(_) => "Pipe",
        _ => "Other",
    };
    crate::dbg_trace!(
        "exec",
        "cmd={cmd_name} resolved={resolved} tty=[in:{} out:{} err:{}] \
         sink={sink} inherit={inherit_tty} pump={needs_pump} \
         interactive={} fg_job={}",
        shell.turn.io.terminal.startup_stdin_tty,
        shell.turn.io.terminal.startup_stdout_tty,
        shell.turn.io.terminal.startup_stderr_tty,
        shell.turn.io.interactive,
        fg.want_fg(),
    );
}

#[cfg(not(debug_assertions))]
fn trace_io_wiring(
    _cmd_name: &str,
    _resolved: &str,
    _inherit_tty: bool,
    _needs_pump: bool,
    _fg: &ForegroundDecision,
    _shell: &Shell,
) {
}
