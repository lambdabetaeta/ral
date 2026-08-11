//! Pipeline resolve: freeze each stage's launch decision and the pipeline's
//! final route.  The route comes from the checker's ground annotation, never
//! re-inferred; no process is created and no pipe opened.  Launch reads
//! everything this phase produces.

use super::super::command::CommandIdentity;
use super::super::command_call;
use crate::evaluator::call;
use crate::ir::{Comp, CompKind};
use crate::source::Span;
use crate::types::{Mooring, Settled, Shell, TerminalAccess, Value};
use std::sync::Arc;

// ── TerminalPlan ────────────────────────────────────────────────────────

/// Frozen terminal-ownership decision: whether the parent hands the controlling
/// terminal to the pipeline pgid via `tcsetpgrp` once the group is established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalPlan {
    NoTerminal,
    ForegroundExternalGroup,
}

impl TerminalPlan {
    pub(super) fn owns_tty(self) -> bool {
        matches!(self, Self::ForegroundExternalGroup)
    }
}

/// Identity and pre-evaluated argv for a directly spawned external stage.  Args
/// stay `Value`s so launch-time `command::vet` applies the same shape rejection
/// single-command exec does.
#[derive(Clone, Debug)]
pub(super) struct ExternalStage {
    pub(super) id: CommandIdentity,
    pub(super) args: Vec<Value>,
}

/// Head resolution for one process-staged stage.  A bundled tool resolves to
/// `External` like any other head, and the carried identity spares the launch
/// decision a second `PATH` walk.
#[derive(Clone, Debug)]
enum StageKind {
    Ral,
    External(CommandIdentity),
}

/// One stage's launch decision, frozen in `resolve_pipeline` and read by launch
/// rather than re-derived.  Argv is evaluated into the carried `ExternalStage`
/// only on the `Direct` path, which consumes it; a `HelperEval` stage
/// re-evaluates its argv inside the child, so doing it here too would run
/// effectful arguments twice.
#[derive(Clone, Debug)]
pub(super) enum StageLaunch {
    Direct(ExternalStage),
    HelperEval,
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

/// A bundled tool is not distinguished from a host binary — both become
/// `External`, the `ral --ral-bundled-tool` child being chosen later by the
/// command image — so `ls`, `cat`, `wc` behave alike everywhere, including on
/// Windows where there is no `.exe` to spawn.  Admission is left to
/// `command::vet` at launch, so a head the grant denies still routes through
/// here and surfaces its refusal as an ordinary error.
fn classify_stage(stage: &Comp, shell: &Shell) -> StageKind {
    let CompKind::Exec(e) = &stage.item else {
        return StageKind::Ral;
    };
    match command_call::resolve_command_word(&e.head, shell) {
        command_call::Resolution::External(id) => StageKind::External(id),
        _ => StageKind::Ral,
    }
}

/// Evaluate a stage's argv, for the `Direct` path alone — a `HelperEval`
/// external re-evaluates it inside the child.  `direct_spawnable` admits only
/// redirect-free stages, so the evaluated redirects are always empty here.
fn eval_external_stage(
    id: CommandIdentity,
    stage: &Comp,
    shell: &mut Shell,
) -> Settled<ExternalStage> {
    let CompKind::Exec(e) = &stage.item else {
        unreachable!("classify_stage yields an identity only for Exec stages")
    };
    let (args, redirects) = call::eval_call_parts(&e.args, &e.redirects, shell)?;
    debug_assert!(
        redirects.is_empty(),
        "direct_spawnable gates on no redirects"
    );
    Ok(ExternalStage { id, args })
}

/// Whether an external stage can be spawned with no helper — every
/// condition a resolve-time fact:
///
/// - the pipeline does not own the controlling terminal (a foreground
///   pipeline parks its stages on stop, which only the helper handles);
/// - the stage has no redirects (the direct path wires only byte ends);
/// - no `!{…}` audit is capturing bytes (that needs the helper's accounting).
fn direct_spawnable(stage: &Comp, terminal: TerminalPlan, shell: &Shell) -> bool {
    let redirects_empty = matches!(&stage.item, CompKind::Exec(e) if e.redirects.is_empty());
    !terminal.owns_tty() && redirects_empty && !shell.local.audit.captures_bytes()
}

/// Freeze one stage's launch decision.  A bundled tool still becomes an
/// external stage, while redirects, foreground ownership, and byte-capturing
/// audits keep the evaluator in a helper.
fn resolve_launch(stage: &Comp, terminal: TerminalPlan, shell: &mut Shell) -> Settled<StageLaunch> {
    Ok(match classify_stage(stage, shell) {
        StageKind::Ral => StageLaunch::HelperEval,
        StageKind::External(id) => {
            if direct_spawnable(stage, terminal, shell) {
                StageLaunch::Direct(eval_external_stage(id, stage, shell)?)
            } else {
                StageLaunch::HelperEval
            }
        }
    })
}

fn analyze_stage(stage: &Comp, terminal: TerminalPlan, shell: &mut Shell) -> Settled<StageSpec> {
    let launch = resolve_launch(stage, terminal, shell)?;
    Ok(StageSpec {
        launch,
        span: stage.span,
    })
}

/// Frozen output of resolve, threaded through launch and collect.
pub(super) struct PipelinePlan {
    pub(super) specs: Vec<StageSpec>,
    pub(super) terminal: TerminalPlan,
    /// What the pipeline form hands back, straight from the IR node.
    pub(super) yields: crate::ir::PipeYield,
}

fn resolve_terminal_plan(mooring: &Mooring, shell: &Shell) -> TerminalPlan {
    // The handoff authority is the session's terminal lease, lent only to a run
    // whose `TerminalAccess` permits it.  No reachable lease → never foreground,
    // and that single question covers a `Denied` run (exarch's tool runs), a
    // backgrounded or tty-less launch (ral held no terminal foreground at
    // startup, so no lease was minted), and every platform without `tcsetpgrp`
    // (none is ever minted off Unix).
    if shell.terminal_lease(mooring).is_none() {
        return TerminalPlan::NoTerminal;
    }
    // With the lease held, foreground iff the final sink is terminal-bound or the
    // run is an explicit tty loan.  A capture (`!{...}`) has a buffer sink, so it
    // stays background; the `_ed-tui` loan is the exception — its stdout is
    // captured too, but the body (e.g. `fzf`) draws on `/dev/tty` and must own the
    // foreground pgid or its first `tcsetattr` raises SIGTTOU.
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

/// Resolve phase: freeze every stage's launch path and carry the form's
/// yield through.  The byte-capturing audit decision is consulted live during
/// classification, not stored on the plan.
pub(super) fn resolve_pipeline(
    stages: &[Arc<Comp>],
    yields: crate::ir::PipeYield,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<PipelinePlan> {
    // Every stage's launch decision depends on it, so the terminal plan is frozen
    // first; it reads only boot/capture state, which argv evaluation cannot touch.
    let terminal = resolve_terminal_plan(mooring, shell);
    let specs = stages
        .iter()
        .map(|stage| analyze_stage(stage, terminal, shell))
        .collect::<Settled<Vec<_>>>()?;
    Ok(PipelinePlan {
        specs,
        terminal,
        yields,
    })
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
