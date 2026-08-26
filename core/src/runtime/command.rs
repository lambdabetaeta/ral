//! The external arm of command dispatch — `command_call` picks it over
//! env and handler: vet the resolved identity, wire the call's
//! redirects into child stdio, spawn under the canonical pgid and
//! sandbox, reap.
//!
//! Pipeline stages never reach [`run`] — they take
//! [`super::pipeline::PipeNode::launch`] — but share `vet` and
//! `build_command` with it, so both paths resolve and confine a call the
//! same way.

use crate::evaluator::audit::observe;
use crate::types::{Break, Error, Mooring, Observed, Settled, Shell, Value, WriteOutcome};

mod child;
#[cfg(unix)]
mod detach;
mod foreground;
mod identity;
pub(crate) mod process;
mod redirect;
mod stdio;
mod vet;

pub(crate) use child::{ExternalPlumbing, GroupOwner, RunningChild};
#[cfg(unix)]
pub(crate) use detach::detach;
pub(crate) use identity::CommandIdentity;
pub(crate) use process::{build_command, spawn_error};
pub use redirect::PendingWrite;
pub(crate) use redirect::{
    EvalRedirect, EvalRedirectV, RedirectGuard, StdinRedirectGuard, abandon_all, apply_redirects,
    atomic_write, commit_atomics, defer_to_stop, install_stdin_redirect, open_file,
    restore_redirects, stderr_mode,
};
use stdio::classify_redirects;
pub(crate) use stdio::{StdinRoute, TtyInputPermit};
use vet::ExecImage;
pub(crate) use vet::vet;

use child::WaitedChild;
use foreground::ForegroundDecision;
use process::{pipe_err, spawn};
use stdio::{inherit_tty, wire_stderr, wire_stdin, wire_stdout_file};

/// Runs a standalone external call from vetting to reap.  A bundled
/// coreutils/diffutils/ripgrep head takes the same path as any host
/// executable: its image is `ral --ral-bundled-tool <tool>`.
pub(crate) fn run(
    id: &CommandIdentity,
    args: &[Value],
    redirects: &[EvalRedirectV],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let rc = vet(id, args, shell)?;
    let cmd_name = rc.shown.clone();

    // Confinement can run for minutes on Windows, so the run's scope goes in
    // with it and is read again on the way out: a wall that expired mid-stamp
    // must not be discovered only once the child is already running.
    let mut command = build_command(
        &rc,
        crate::sandbox::Ownership::Kept,
        shell,
        mooring.cancel.as_scope(),
    )?;
    crate::process::check(mooring)?;

    let plan = classify_redirects(redirects)?;
    command.stdin(wire_stdin(shell).into_stdio());
    let (mut atomic_commit, stdout_file_dup) =
        wire_stdout_file(&mut command, &plan, mooring, shell)?;
    let inherit_tty = inherit_tty(&plan, shell);

    // Stdout before stderr: `wire_stderr`'s `2>&1` case clones a writer
    // from `shell.io.stdout` on Windows, and its Unix `pre_exec` dup2
    // needs a real stdout fd already in place pre-fork.
    let stdout_plan = if plan.stdout_file.is_none() {
        let p = shell
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

    let stderr_piped = wire_stderr(
        &mut command,
        &plan,
        inherit_tty,
        stdout_file_dup,
        mooring,
        shell,
    )?;

    let stderr_pump = if stderr_piped {
        Some(shell.io.stderr.clone())
    } else {
        None
    };

    let fg = ForegroundDecision::for_standalone(shell, needs_pump, mooring);
    let image_shown = match &rc.image {
        ExecImage::Host(p) => p.clone(),
        ExecImage::BundledTool { tool } => format!("ral --ral-bundled-tool {tool}"),
    };
    trace_io_wiring(&cmd_name, &image_shown, inherit_tty, needs_pump, &fg, shell);

    announce_command_title(&cmd_name, shell);

    // Anchor the denial-log window before the spawn, so a kernel deny the
    // child logs falls inside what `sandbox::augment_failure` reads back.
    let started = std::time::Instant::now();
    let confinement = command.confinement();
    let (child, wait_pgid, jail) = match spawn(&mut command, fg.pgid_policy(), shell) {
        Ok(pair) => pair,
        // `finish_command` builds the `Command{External}` observation from
        // whatever error reaches it, so a spawn failure needs no emission of
        // its own here.
        Err(e) => return Err(spawn_error(confinement, &cmd_name, &e)),
    };

    let child_pid = child.id();
    // Bound, not dropped: its `Drop` restores ral's pgid on every path out
    // of here, sparing the next REPL tty read an EIO from a background
    // pgroup.
    let _fg_guard = fg.acquire(child_pid, shell, mooring);

    let park_on_stop = fg.park_on_stop();
    // A tracked leader pgid is exactly what there is to release on
    // Windows; an `Inherit` child has no group at all.
    let group_owner = if wait_pgid.is_some() {
        GroupOwner::Standalone
    } else {
        GroupOwner::None
    };
    // Nothing fallible may run between `spawn` and this assembly: until
    // `RunningChild` owns it the bare child leaks on an early return,
    // whereas afterwards its `Drop` SIGKILLs the pgid and reaps.
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
        mooring.cancel.as_scope().clone(),
        jail,
    );

    let waited: WaitedChild = match running.wait() {
        Ok(waited) => waited,
        // A stop parks the child alive with the staged temp still open, so
        // the uncommitted write leaves with the escape instead of dying here,
        // and the card says `deferred` rather than claiming either end.
        Err(brk) => {
            if brk.is_stop()
                && let Some((path, mode)) = plan.stdout_file.as_ref()
            {
                observe(
                    shell,
                    mooring,
                    Observed::Write {
                        path: path.clone(),
                        mode: *mode,
                        outcome: WriteOutcome::Deferred,
                        new_bytes: None,
                        old_bytes: None,
                    },
                );
            }
            return Err(defer_to_stop(brk, atomic_commit.take()));
        }
    };
    let outcome = waited.outcome;

    // Held rather than `?`-propagated: the bookkeeping below (drain, set
    // `$?`) must still run for a command that did run, even when its
    // commit failed.
    let commit_result = if let Some(commit) = atomic_commit.take() {
        let (path, mode) = plan
            .stdout_file
            .as_ref()
            .expect("atomic_commit is only Some when plan.stdout_file is Some");
        if outcome.is_success() {
            let old_bytes = commit.old_snapshot_for_diff();
            let preview = commit.new_snapshot_for_diff();
            match commit.commit() {
                Ok(()) => {
                    observe(
                        shell,
                        mooring,
                        Observed::Write {
                            path: path.clone(),
                            mode: *mode,
                            outcome: WriteOutcome::Committed,
                            new_bytes: preview,
                            old_bytes,
                        },
                    );
                    Ok(())
                }
                Err(e) => {
                    observe(
                        shell,
                        mooring,
                        Observed::Write {
                            path: path.clone(),
                            mode: *mode,
                            outcome: WriteOutcome::Failed,
                            new_bytes: None,
                            old_bytes: None,
                        },
                    );
                    Err(Break::Error(Error::new(format!("atomic write: {e}"), 1)))
                }
            }
        } else {
            commit.abandon();
            observe(
                shell,
                mooring,
                Observed::Write {
                    path: path.clone(),
                    mode: *mode,
                    outcome: WriteOutcome::Aborted,
                    new_bytes: None,
                    old_bytes: None,
                },
            );
            Ok(())
        }
    } else {
        Ok(())
    };

    // Only joins the pump threads: under audit the bytes are already
    // captured by the dispatch-level Tee on `shell.io.stdout` / `stderr`.
    waited.drain();
    let code = outcome.to_user_exit_code();
    shell.last_status = code;
    commit_result?;
    // A command inside a helper stage can no longer take SIGPIPE from an
    // interior edge — the parent holds that edge's read end — so any SIGPIPE
    // it suffers is from a pipe of its own making and is its own failure.
    match crate::process::CommandFailure::from_outcome(outcome, crate::process::StageKill::NotSent)
    {
        None => Ok(Value::Unit),
        Some(failure) => {
            let err = Error::from_command_failure(&cmd_name, failure, shell);
            // Only this child and its descendants may claim a kernel deny
            // line; `augment_failure` short-circuits when no sandbox ran.
            let mut pids = crate::sandbox::sample_descendants(child_pid);
            pids.insert(child_pid);
            let err = crate::sandbox::augment_failure(err, shell, &pids, started);
            Err(Break::Error(err))
        }
    }
}

/// Announce the running command via the terminal title (OSC 0).
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.io.interactive && shell.io.terminal.ui_title_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// Debug-only single-line trace of the I/O wiring decisions.  The whole
/// function is cfg-gated, not just `dbg_trace!`, which alone would leave
/// the sink match running in release.
#[cfg(debug_assertions)]
fn trace_io_wiring(
    cmd_name: &str,
    resolved: &str,
    inherit_tty: bool,
    needs_pump: bool,
    fg: &ForegroundDecision,
    shell: &Shell,
) {
    let sink = match &shell.io.stdout {
        crate::io::Sink::Terminal => "Terminal",
        crate::io::Sink::External(_) => "External",
        _ => "Other",
    };
    crate::dbg_trace!(
        "exec",
        "cmd={cmd_name} resolved={resolved} tty=[in:{} out:{} err:{}] \
         sink={sink} inherit={inherit_tty} pump={needs_pump} \
         interactive={} fg_job={}",
        shell.io.terminal.startup_stdin_tty,
        shell.io.terminal.startup_stdout_tty,
        shell.io.terminal.startup_stderr_tty,
        shell.io.interactive,
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
