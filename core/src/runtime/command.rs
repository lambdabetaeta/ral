//! The external arm of command dispatch — `command_call` picks it over
//! env and handler: vet the resolved identity, wire the call's
//! redirects into child stdio, spawn under the canonical pgid and
//! sandbox, reap.
//!
//! Pipeline stages never reach [`run`] — they take
//! [`super::pipeline::PipeNode::launch`] — but share `vet` and
//! `build_command` with it, so both paths resolve and confine a call the
//! same way.

use crate::evaluator::audit::{listening, observe};
use crate::process::Group;
use crate::syntax::ast::RedirectMode;
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

pub(crate) use child::{Pumps, RunningChild};
#[cfg(unix)]
pub(crate) use detach::detach;
pub(crate) use identity::CommandIdentity;
pub(crate) use process::{build_command, spawn_error};
pub(crate) use redirect::{
    EvalRedirect, EvalRedirectV, PendingWrite, StdinRedirectGuard, atomic_write,
    atomic_write_error, install_stdin_redirect, open_file, stderr_mode,
};
use stdio::classify_redirects;
pub(crate) use stdio::{StdinRoute, TtyInputPermit, stdin_error};
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
    command.stdin(wire_stdin(shell)?.into_stdio());
    let (atomic_commit, stdout_file_dup) = wire_stdout_file(&mut command, &plan, mooring, shell)?;
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

    let confinement = command.confinement();
    let fg = ForegroundDecision::for_standalone(shell, needs_pump, confinement.is_some(), mooring);
    let image_shown = match &rc.image {
        ExecImage::Host(p) => p.clone(),
        ExecImage::BundledTool { tool } => format!("ral --ral-bundled-tool {tool}"),
    };
    trace_io_wiring(&cmd_name, &image_shown, inherit_tty, needs_pump, &fg, shell);

    announce_command_title(&cmd_name, shell);

    // Anchor the denial-log window before the spawn, so a kernel deny the
    // child logs falls inside what `sandbox::augment_failure` reads back.
    let started = std::time::Instant::now();
    let (child, led, jail) = match spawn(&mut command, fg.pgid_policy(), shell) {
        Ok(pair) => pair,
        // `finish_command` builds the `Command{External}` observation from
        // whatever error reaches it, so a spawn failure needs no emission of
        // its own here.
        Err(e) => return Err(spawn_error(confinement, &cmd_name, &e)),
    };

    let child_pid = child.id();
    // Held until `reclaim`: its `Drop` restores ral's pgid on every path out
    // of here, sparing the next REPL tty read an EIO from a background
    // pgroup.
    let loan = fg.acquire(child_pid, shell, mooring);

    let group = landed_in(led, &shell.io.launch_role);
    // Nothing fallible may run between `spawn` and this assembly: until
    // `RunningChild` owns it the bare child leaks on an early return,
    // whereas afterwards its `Drop` SIGKILLs the pgid and reaps.
    let running = RunningChild::assemble_with_owner(
        child,
        cmd_name.clone(),
        Pumps::new(stdout_plan, stderr_pump),
        group,
        mooring.cancel.as_scope().clone(),
        jail,
    );

    let waited: WaitedChild = running.wait();
    // The terminal returns the instant its tenant is dead, and what the
    // tenant heard is struck on the frame before the next poll.
    let pressed = loan.and_then(|loan| loan.reclaim(waited.outcome.death(false)));
    let (outcome, cause) = (waited.outcome, waited.cause.max(pressed));

    // Held rather than `?`-propagated: the drain below must still run for a
    // command that did run, even when its commit failed.
    let commit_result = settle_atomic_write(
        atomic_commit,
        plan.stdout_file.as_ref(),
        outcome.is_success(),
        shell,
        mooring,
    );

    // Only joins the pump threads: under audit the bytes are already
    // captured by the dispatch-level Tee on `shell.io.stdout` / `stderr`.
    waited.settle();
    commit_result?;
    // A command inside a pipeline stage cannot take SIGPIPE from an interior
    // edge — the parent holds that edge's read end — so any SIGPIPE it
    // suffers is from a pipe of its own making and is its own failure.
    match outcome.classify(cause, confinement.is_some()) {
        None => Ok(Value::Unit),
        Some(end) => {
            let err = Error::of_child(&cmd_name, end, shell);
            // Only this child and its descendants may claim a kernel deny
            // line; `augment_failure` short-circuits when no sandbox ran.
            let mut pids = crate::sandbox::sample_descendants(child_pid);
            pids.insert(child_pid);
            let err = crate::sandbox::augment_failure(err, shell, &pids, started);
            Err(Break::Error(err))
        }
    }
}

/// Who signals a child spawned under `role`: the group it `led`, else the
/// pipeline's it joined, else nobody but its own pid.
fn landed_in(led: Option<crate::process::Pgid>, role: &crate::io::LaunchRole) -> Option<Group> {
    led.map(Group::Owns)
        .or_else(|| role.membership().cloned().map(Group::Joins))
}

/// Settle a `>` staged by [`stdio::wire_stdout_file`], if the call staged
/// one: commit it on a successful run, abandon it otherwise, and post the
/// write observation either way.
fn settle_atomic_write(
    atomic_commit: Option<PendingWrite>,
    stdout_file: Option<&(String, RedirectMode)>,
    succeeded: bool,
    shell: &mut Shell,
    mooring: &Mooring,
) -> Settled<()> {
    let Some(commit) = atomic_commit else {
        return Ok(());
    };
    let (path, mode) =
        stdout_file.expect("atomic_commit is only Some when plan.stdout_file is Some");

    if !succeeded {
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
        return Ok(());
    }

    // Both reads must precede the rename, and cost two whole-file reads:
    // taken only for an ear to hear them.
    let (old_bytes, preview) = if listening(shell, mooring) {
        (
            commit.old_snapshot_for_diff(shell),
            commit.new_snapshot_for_diff(),
        )
    } else {
        (None, None)
    };
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
            Err(atomic_write_error(&e))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::LaunchRole;
    use crate::process::{CancelScope, Membership, Pgid};

    fn pgid(raw: i32) -> Pgid {
        Pgid::from_raw(raw).expect("positive")
    }

    /// An envelope inside a stage leads its payload's group, and is its
    /// owner; a plain member joins; a top-level `Inherit` child has none.
    #[test]
    fn a_child_owns_what_it_leads_and_joins_what_it_only_entered() {
        let stage = LaunchRole::PipelineStage(Membership::new(pgid(7), CancelScope::root()));
        assert!(matches!(
            landed_in(Some(pgid(9)), &stage),
            Some(Group::Owns(g)) if g == pgid(9)
        ));
        assert!(matches!(
            landed_in(None, &stage),
            Some(Group::Joins(m)) if m.group() == pgid(7)
        ));
        assert!(landed_in(None, &LaunchRole::TopLevel).is_none());
    }
}
