//! The external arm of command dispatch — `dispatch` picks it over
//! env and handler: prepare the admitted head, wire the shell's streams
//! into child stdio, start it under the canonical pgid and sandbox, reap.
//!
//! Pipeline stages never reach [`run`] — they take
//! [`super::pipeline::PipeNode::run`] — but a direct stage shares
//! [`prepare`], [`start`] and [`ending`] with it, so both paths resolve,
//! confine, start and end a call the same way.

use crate::authority::sandbox::Ownership;
use crate::process::Spawned;
use crate::shell::runtime::{Mooring, Settled, Shell, Value};

mod child;
#[cfg(unix)]
mod detach;
mod foreground;
mod head;
pub(crate) mod process;
mod stdio;
mod vet;

pub(crate) use child::{Pumps, RunningChild};
#[cfg(unix)]
pub(crate) use detach::detach;
pub(crate) use head::{Admission, Head, Target};
pub(crate) use process::{Prepared, ending, prepare, start};
pub(crate) use stdio::{ChildIo, ChildOut, TtyInputPermit, wire_stdio};

use foreground::ForegroundDecision;
use stdio::inherit_tty;

/// Runs a standalone external call from vetting to reap.  A bundled
/// coreutils/diffutils/ripgrep head takes the same path as any host
/// executable: its image is `ral --ral-bundled-tool <tool>`.
pub(crate) fn run(
    head: Head<Admission>,
    args: &[Value],
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<Value> {
    let mut prepared = prepare(head, args, Ownership::Kept, shell)?;

    let inherit_tty = inherit_tty(shell);
    let pumps = wire_stdio(
        &mut prepared.launch,
        shell,
        ChildIo::from(&shell.io),
        Some(TtyInputPermit::for_standalone_external()),
        inherit_tty,
    )?;
    let needs_pump = pumps.pumps_stdout();

    let fg =
        ForegroundDecision::for_standalone(shell, needs_pump, prepared.launch.enveloped(), mooring);
    #[cfg(debug_assertions)]
    trace_io_wiring(&prepared.shown, inherit_tty, needs_pump, &fg, shell);

    announce_command_title(&prepared.shown, shell);

    // Anchor the denial-log window before the spawn, so a kernel deny the
    // child logs falls inside what `authority::sandbox::denial_hint` reads back.
    let started = std::time::Instant::now();
    // `finish_command` builds the `Command{External}` observation from
    // whatever error reaches it, so a start failure needs no emission of
    // its own here.
    let prepared_loan = fg.prepare(shell, mooring);
    let Spawned {
        child,
        landing,
        jail,
    } = start(&mut prepared, fg.pgid_policy())?;
    let Prepared { shown, .. } = prepared;

    let child_pid = child.id();
    // Held until `reclaim`: its `Drop` restores ral's pgid on every path out
    // of here, sparing the next REPL tty read an EIO from a background
    // pgroup.
    let loan = prepared_loan.and_then(|loan| loan.lend(child_pid.cast_signed()));

    // Nothing fallible may run between `start` and this assembly: until
    // `RunningChild` owns it the bare child leaks on an early return,
    // whereas afterwards its `Drop` SIGKILLs the pgid and reaps.
    let running = RunningChild::assemble_with_owner(
        child,
        shown.clone(),
        pumps,
        landing.group(shell.io.stage.as_ref()),
        shell.scope.clone(),
        jail,
    );

    let waited = running.wait();
    let outcome = waited.outcome;
    // The terminal returns the instant its tenant is dead; a key it heard is
    // struck on the node, which the current scope reads or a shield masks.
    if let Some(loan) = loan {
        loan.reclaim(outcome.death());
    }
    // The current scope, read once after the strike: under a shield a key
    // the tenant heard is not the child's cause, and its death is a failure.
    let cause = waited.cause.max(shell.scope.cause());

    // Only joins the pump threads: under audit the bytes are already
    // captured by the dispatch-level Tee on `shell.io.stdout` / `stderr`.
    waited.settle(&shell.scope);
    // A command inside a pipeline stage cannot take SIGPIPE from an interior
    // edge — the parent holds that edge's read end — so any SIGPIPE it
    // suffers is from a pipe of its own making and is its own failure.
    // Only this child and its descendants may claim a kernel deny line.
    let pids = || {
        let mut pids = crate::process::sample_descendants(child_pid);
        pids.insert(child_pid);
        pids
    };
    ending(&shown, outcome, cause, &shell.scope, shell, pids, started).map(|()| Value::Unit)
}

/// Announce the running command via the terminal title (OSC 0).
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.io.interactive && shell.io.terminal.ui_title_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::io::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// Debug-only single-line trace of the I/O wiring decisions.  The function and
/// its call are cfg-gated, not just `dbg_trace!`, which alone would leave the
/// sink match running in release.
#[cfg(debug_assertions)]
fn trace_io_wiring(
    cmd_name: &str,
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
        "cmd={cmd_name} tty=[in:{} out:{} err:{}] \
         sink={sink} inherit={inherit_tty} pump={needs_pump} \
         interactive={} fg_job={}",
        shell.io.terminal.startup_stdin_tty,
        shell.io.terminal.startup_stdout_tty,
        shell.io.terminal.startup_stderr_tty,
        shell.io.interactive,
        fg.want_fg(),
    );
}
