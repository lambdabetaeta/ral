//! The external arm of command dispatch — `command_call` picks it over
//! env and handler: vet the resolved head, wire the shell's streams
//! into child stdio, spawn under the canonical pgid and sandbox, reap.
//!
//! Pipeline stages never reach [`run`] — they take
//! [`super::pipeline::PipeNode::launch`] — but share `vet` and
//! `build_launch` with it, so both paths resolve and confine a call the
//! same way.

use crate::process::Group;
use crate::types::{Break, Error, Mooring, Settled, Shell, Value};

mod child;
#[cfg(unix)]
mod detach;
mod foreground;
mod head;
pub(crate) mod process;
mod redirect;
mod stdio;
mod vet;

pub(crate) use child::{Pumps, RunningChild};
#[cfg(unix)]
pub(crate) use detach::detach;
pub(crate) use head::Head;
pub(crate) use process::{build_launch, spawn_error};
pub(crate) use redirect::{
    PendingWrite, StdinRedirectGuard, atomic_write, atomic_write_error, install_stdin_redirect,
    open_write,
};
pub(crate) use stdio::{ChildIo, TtyInputPermit, stdin_error, wire_stdio};
pub(crate) use vet::vet;

use child::WaitedChild;
use foreground::ForegroundDecision;
use process::spawn;
use stdio::inherit_tty;

/// Runs a standalone external call from vetting to reap.  A bundled
/// coreutils/diffutils/ripgrep head takes the same path as any host
/// executable: its image is `ral --ral-bundled-tool <tool>`.
pub(crate) fn run(
    head: &Head,
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let rc = vet(head, args, shell)?;
    let cmd_name = rc.shown.clone();

    // Confinement can run for minutes on Windows, so the run's scope goes in
    // with it and is read again on the way out: a wall that expired mid-stamp
    // must not be discovered only once the child is already running.
    let mut command = build_launch(
        &rc,
        crate::sandbox::Ownership::Kept,
        shell,
        mooring.cancel.as_scope(),
    )?;
    crate::process::check(mooring)?;

    let inherit_tty = inherit_tty(shell);
    let pumps = wire_stdio(
        &mut command,
        shell,
        &ChildIo::from(&shell.io),
        Some(TtyInputPermit::for_standalone_external()),
        inherit_tty,
    )?;
    let needs_pump = pumps.pumps_stdout();

    let confinement = command.confinement();
    let fg = ForegroundDecision::for_standalone(shell, needs_pump, confinement.is_some(), mooring);
    let program = rc.admitted.program().to_string();
    trace_io_wiring(&cmd_name, &program, inherit_tty, needs_pump, &fg, shell);

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
        pumps,
        group,
        mooring.cancel.as_scope().clone(),
        jail,
    );

    let waited: WaitedChild = running.wait();
    // The terminal returns the instant its tenant is dead, and what the
    // tenant heard is struck on the frame before the next poll.
    let pressed = loan.and_then(|loan| loan.reclaim(waited.outcome.death(false)));
    let (outcome, cause) = (waited.outcome, waited.cause.max(pressed));

    // Only joins the pump threads: under audit the bytes are already
    // captured by the dispatch-level Tee on `shell.io.stdout` / `stderr`.
    waited.settle();
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
