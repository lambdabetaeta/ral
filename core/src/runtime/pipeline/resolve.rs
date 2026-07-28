//! Pipeline resolve: validate channel adjacency, freeze each stage's launch
//! decision, and classify the pipeline.  Modes come from the checker's ground
//! wires, never re-inferred; no process is created and no pipe opened.  Launch
//! reads everything this phase produces.

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

// ── PipelineKind ────────────────────────────────────────────────────────

/// `PureValue` — no byte edge anywhere, so the pipeline reduces to a data-last
/// fold in the parent evaluator (`x | f = f !{x}`) and enters no job control.
/// `ProcessStaged` — at least one byte edge, so every stage becomes a child in
/// one process group, either a direct external or a ral helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PipelineKind {
    PureValue,
    ProcessStaged,
}

impl PipelineKind {
    fn from_specs(specs: &[StageSpec]) -> Self {
        let pure_value = specs.iter().all(|spec| {
            spec.comp_type.input != crate::mode::PipeMode::Bytes
                && spec.comp_type.output != crate::mode::PipeMode::Bytes
        });
        if pure_value {
            Self::PureValue
        } else {
            Self::ProcessStaged
        }
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
/// `route::open_stage_routes` derives each edge from the stage's position and
/// `comp_type.output`, which the checker already unified across the edge.
#[derive(Clone, Debug)]
pub(super) struct StageSpec {
    pub(super) comp_type: crate::mode::PipeSpec,
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

/// A value edge on either side of stage `i` of `n`: only an evaluating helper,
/// which reads the value channel before invoking, can run such a stage.
fn carries_value_edge(i: usize, n: usize, comp_type: crate::mode::PipeSpec) -> bool {
    super::route::value_edge_in(i, comp_type) || super::route::value_edge_out(i, n, comp_type)
}

/// Whether a pure external stage can be spawned with no helper — every
/// condition a resolve-time fact:
///
/// - the pipeline does not own the controlling terminal (a foreground
///   pipeline parks its stages on stop, which only the helper handles);
/// - the stage carries no value edge;
/// - the stage has no redirects (the direct path wires only byte ends);
/// - no `!{…}` audit is capturing bytes (that needs the helper's accounting).
fn direct_spawnable(
    i: usize,
    n: usize,
    stage: &Comp,
    comp_type: crate::mode::PipeSpec,
    terminal: TerminalPlan,
    shell: &Shell,
) -> bool {
    let redirects_empty = matches!(&stage.item, CompKind::Exec(e) if e.redirects.is_empty());
    !terminal.owns_tty()
        && redirects_empty
        && !carries_value_edge(i, n, comp_type)
        && !shell.local.audit.captures_bytes()
}

/// Freeze one stage's launch decision.  A value-edge bundled stage fails
/// `direct_spawnable` and so runs in a helper: data-last application
/// (`x | f = f !{x}`) is evaluator work, and the bundled tool's in-process path
/// still fires from command dispatch inside that child, so bundled-first holds.
fn resolve_launch(
    i: usize,
    n: usize,
    stage: &Comp,
    comp_type: crate::mode::PipeSpec,
    terminal: TerminalPlan,
    shell: &mut Shell,
) -> Settled<StageLaunch> {
    Ok(match classify_stage(stage, shell) {
        StageKind::Ral => StageLaunch::HelperEval,
        StageKind::External(id) => {
            if direct_spawnable(i, n, stage, comp_type, terminal, shell) {
                StageLaunch::Direct(eval_external_stage(id, stage, shell)?)
            } else {
                StageLaunch::HelperEval
            }
        }
    })
}

fn analyze_stage(
    i: usize,
    n: usize,
    stage: &Comp,
    comp_type: crate::mode::PipeSpec,
    terminal: TerminalPlan,
    shell: &mut Shell,
) -> Settled<StageSpec> {
    let launch = resolve_launch(i, n, stage, comp_type, terminal, shell)?;
    Ok(StageSpec {
        comp_type,
        launch,
        span: stage.span,
    })
}

/// Frozen output of resolve, threaded through launch and collect.
pub(super) struct PipelinePlan {
    pub(super) kind: PipelineKind,
    pub(super) specs: Vec<StageSpec>,
    pub(super) terminal: TerminalPlan,
    pub(super) last_output: crate::mode::PipeMode,
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

/// Adjacency holds by construction — the checker unified the wires it emitted —
/// so a debug build only asserts it.
fn specs_from_wires(
    stages: &[Arc<Comp>],
    wires: &[crate::mode::Wire],
    terminal: TerminalPlan,
    shell: &mut Shell,
) -> Settled<Vec<StageSpec>> {
    debug_assert_eq!(wires.len(), stages.len());
    for i in 1..wires.len() {
        debug_assert_eq!(
            wires[i - 1].output,
            wires[i].input,
            "adjacent pipeline wires disagree at stage {i}"
        );
    }

    let n = stages.len();
    stages
        .iter()
        .zip(wires)
        .enumerate()
        .map(|(i, (stage, wire))| analyze_stage(i, n, stage, wire.spec(), terminal, shell))
        .collect()
}

/// Resolve phase: validate adjacency, freeze every stage's launch path, and
/// classify the pipeline.  The byte-capturing audit decision is consulted live
/// during classification, not stored on the plan.
pub(super) fn resolve_pipeline(
    stages: &[Arc<Comp>],
    wires: &[crate::mode::Wire],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<PipelinePlan> {
    // Every stage's launch decision depends on it, so the terminal plan is frozen
    // first; it reads only boot/capture state, which argv evaluation cannot touch.
    let terminal = resolve_terminal_plan(mooring, shell);
    let specs = specs_from_wires(stages, wires, terminal, shell)?;

    // `eval_pipeline` in `core/src/evaluator/comp.rs` sends a lone stage straight
    // to `eval_comp`, so this is reached only with ≥2.  Pin the invariant rather
    // than fabricate a `Mode::None` fallback that lies about an empty pipeline.
    let last = specs.last().expect("pipeline has at least one stage");
    let last_output = last.comp_type.output;
    let kind = PipelineKind::from_specs(&specs);

    Ok(PipelinePlan {
        kind,
        specs,
        terminal,
        last_output,
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
