//! Pipeline resolve: freeze each stage's launch decision and the terminal
//! handoff.  No process is created and no pipe opened; launch reads everything
//! this phase produces.  The form's `PipeYield` comes committed in the checked
//! IR and never passes through here.

use super::super::command::CommandIdentity;
use super::super::command_call;
use crate::evaluator::machine;
use crate::ir::{Comp, CompKind};
use crate::source::Span;
use crate::types::{Env, Mooring, Settled, Shell, TerminalAccess, Value};
use std::sync::Arc;

// ── TerminalPlan ────────────────────────────────────────────────────────

/// Frozen terminal-ownership decision: whether the parent hands the controlling
/// terminal to the pipeline pgid via `tcsetpgrp` once the group is established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalPlan {
    NoTerminal,
    ForegroundExternalGroup,
}

/// Identity and pre-evaluated argv for a directly spawned external stage.  Args
/// stay `Value`s so launch-time `command::vet` applies the same shape rejection
/// single-command exec does.
#[derive(Clone, Debug)]
pub(super) struct ExternalStage {
    pub(super) id: CommandIdentity,
    pub(super) args: Vec<Value>,
}

/// One stage's launch decision, frozen here and read by launch rather than
/// re-derived.  Argv is evaluated only on the `Direct` path, which consumes
/// it: a `Thread` stage re-evaluates its own inside the thread, so doing it
/// here too would run effectful arguments twice.
#[derive(Clone, Debug)]
pub(super) enum StageLaunch {
    Direct(ExternalStage),
    Thread,
}

/// Per-stage analysis.  Edge transport is deliberately absent: the allocator
/// `route::open_stage_routes` derives each edge from the stage's position
/// alone.
#[derive(Clone, Debug)]
pub(super) struct StageSpec {
    pub(super) launch: StageLaunch,
    /// So a parent-side error points at this stage, not the whole pipeline.
    pub(super) span: Option<Span>,
}

/// Freeze one stage's launch decision: a thread unless the head is an external
/// with no redirect and no byte-capturing audit in force — a redirect needs a
/// thread's fd table, a capture its accounting.
///
/// A bundled tool is not distinguished from a host binary, so `ls`, `cat`,
/// `wc` behave alike everywhere.  Admission is `command::vet`'s at launch: a
/// head the grant denies still routes through here and refuses as an ordinary
/// error.
fn resolve_launch(stage: &Comp, env: &Env, shell: &Shell) -> Settled<StageLaunch> {
    let CompKind::Exec(e) = &stage.item else {
        return Ok(StageLaunch::Thread);
    };
    let command_call::Resolution::External(id) =
        command_call::resolve_command_word(&e.head, env, shell)
    else {
        return Ok(StageLaunch::Thread);
    };
    if !e.redirects.is_empty() || shell.local.audit.captures_bytes() {
        return Ok(StageLaunch::Thread);
    }
    Ok(StageLaunch::Direct(ExternalStage {
        id,
        args: machine::close_args(&e.args, env)?,
    }))
}

/// Frozen output of resolve, threaded through launch and collect.
pub(super) struct PipelinePlan {
    pub(super) specs: Vec<StageSpec>,
    pub(super) terminal: TerminalPlan,
}

fn resolve_terminal_plan(mooring: &Mooring, shell: &Shell) -> TerminalPlan {
    // The handoff authority is the session's terminal lease, lent only to a run
    // whose `TerminalAccess` permits it: no reachable lease, never foreground.
    if shell.terminal_lease(mooring).is_none() {
        return TerminalPlan::NoTerminal;
    }
    // A capture (`!{...}`) has a buffer sink and so stays background; the
    // `_ed-tui` loan is captured too, but its body (`fzf`, say) draws on
    // `/dev/tty` and must own the foreground pgid or `tcsetattr` raises
    // SIGTTOU.
    let loan = matches!(mooring.terminal_access, TerminalAccess::ExplicitLoan);
    let terminal_bound = matches!(
        shell.io.stdout,
        crate::io::Sink::Terminal | crate::io::Sink::External(_)
    );
    if terminal_bound || loan {
        TerminalPlan::ForegroundExternalGroup
    } else {
        TerminalPlan::NoTerminal
    }
}

/// Resolve phase: freeze every stage's launch path and the terminal handoff.
pub(super) fn resolve_pipeline(
    stages: &[Arc<Comp>],
    env: &Env,
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<PipelinePlan> {
    let terminal = resolve_terminal_plan(mooring, shell);
    let specs = stages
        .iter()
        .map(|stage| {
            Ok(StageSpec {
                launch: resolve_launch(stage, env, shell)?,
                span: stage.span,
            })
        })
        .collect::<Settled<Vec<_>>>()?;
    Ok(PipelinePlan { specs, terminal })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session owning a terminal lease plus a `Leased` mooring — the REPL, or
    /// a terminal-launched script.  Stdout defaults to `Sink::Terminal`.
    fn leased_shell() -> (Shell, Mooring) {
        let mut shell = Shell::default();
        shell.io.interactive = true;
        shell.io.terminal.startup_stdin_tty = true;
        shell.io.terminal.startup_stdout_tty = true;
        shell.session.terminal_lease = crate::process::TerminalLease::mint_at_startup(true);
        let mooring = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        (shell, mooring)
    }

    #[test]
    #[cfg(unix)]
    fn leased_terminal_bound_pipeline_foregrounds() {
        let (shell, mooring) = leased_shell();
        assert_eq!(
            resolve_terminal_plan(&mooring, &shell),
            TerminalPlan::ForegroundExternalGroup,
        );
    }

    /// A capture's sink is a buffer, not the terminal, so it must not steal the
    /// foreground even under a `Leased` mooring.
    #[test]
    fn leased_captured_pipeline_skips_foreground() {
        let (mut shell, mooring) = leased_shell();
        let (sink, _buf) = crate::io::new_buffer();
        shell.io.stdout = sink;
        assert_eq!(
            resolve_terminal_plan(&mooring, &shell),
            TerminalPlan::NoTerminal
        );
    }

    /// The loan foregrounds despite the buffer sink — otherwise `fzf`, drawing on
    /// `/dev/tty` from a background pgroup, raises SIGTTOU (the CTRL-R failure).
    #[test]
    #[cfg(unix)]
    fn ed_tui_loan_foregrounds_captured_pipeline() {
        let (mut shell, mut mooring) = leased_shell();
        let (sink, _buf) = crate::io::new_buffer();
        shell.io.stdout = sink;
        mooring.terminal_access = TerminalAccess::ExplicitLoan;
        assert_eq!(
            resolve_terminal_plan(&mooring, &shell),
            TerminalPlan::ForegroundExternalGroup,
        );
    }

    /// A terminal-launched script holds `Leased` exactly like the REPL: gating on
    /// interactivity would strand `claude` or `fzf` in the background on SIGTTOU.
    #[test]
    #[cfg(unix)]
    fn terminal_script_leased_pipeline_foregrounds() {
        let (mut shell, mooring) = leased_shell();
        shell.io.interactive = false;
        assert_eq!(
            resolve_terminal_plan(&mooring, &shell),
            TerminalPlan::ForegroundExternalGroup,
        );
    }

    /// A `Denied` mooring never foregrounds even though the session owns a lease
    /// — exarch's tool runs: the borrow is unreachable, the handoff unbuildable.
    #[test]
    #[cfg(unix)]
    fn denied_run_skips_foreground() {
        let (shell, mut mooring) = leased_shell();
        mooring.terminal_access = TerminalAccess::Denied;
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );
        assert_eq!(
            resolve_terminal_plan(&mooring, &shell),
            TerminalPlan::NoTerminal
        );
    }

    /// A launch that never owned the terminal foreground (backgrounded `ral … &`,
    /// a piped or tty-less eval) minted no lease, so there is nothing to borrow.
    #[test]
    fn no_lease_skips_foreground() {
        let (mut shell, mooring) = leased_shell();
        shell.session.terminal_lease = None;
        assert_eq!(
            resolve_terminal_plan(&mooring, &shell),
            TerminalPlan::NoTerminal
        );
    }
}
