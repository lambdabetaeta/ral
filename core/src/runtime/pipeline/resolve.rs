//! Pipeline resolve phase: validate channel adjacency, classify launch,
//! and freeze the pipeline-level invariants.
//!
//! Resolve reads the checker's ground wires; it does not infer modes.
//! It may evaluate argv for stages committed to direct external launch.
//! It creates no process and opens no pipe.
//! Everything this module produces is read by [`super::launch`].

use super::super::command::CommandIdentity;
use super::super::command_call;
use crate::evaluator::call;
use crate::ir::{Comp, CompKind};
use crate::types::*;
use std::sync::Arc;

// ── TerminalPlan ────────────────────────────────────────────────────────

/// Frozen terminal-ownership decision for a pipeline.
///
/// `NoTerminal` covers non-interactive launches (scripts, captured
/// stdin); `ForegroundExternalGroup` covers interactive launches where
/// the parent should hand the controlling terminal to the pipeline pgid
/// via `tcsetpgrp` once the group is established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalPlan {
    NoTerminal,
    ForegroundExternalGroup,
}

impl TerminalPlan {
    /// True when the pipeline pgid should own the controlling terminal.
    pub(super) fn owns_tty(self) -> bool {
        matches!(self, Self::ForegroundExternalGroup)
    }
}

// ── PipelineKind ────────────────────────────────────────────────────────

/// Top-level execution class for a resolved pipeline.
///
/// `PureValue` is a pipeline with no byte channel on any edge: it reduces
/// to a data-last fold in the parent evaluator (`x | f = f !{x}`).
/// `ProcessStaged` is a pipeline carrying at least one byte edge.  It
/// launches every stage as a child in one process group; a child is
/// either a direct external command or a ral helper evaluating the stage.
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

/// PATH-resolved identity and pre-evaluated argument values for an
/// external stage.
///
/// The [`CommandIdentity`] is built at staging time and threaded down
/// to launch, so classify and exec see one rendering of the head.
/// Args are kept as `Value`s rather than strings so launch-time
/// `command::vet` can run the same shape rejection that single-command
/// exec runs (lists / maps / lambdas / blocks / handles / bytes
/// rejected with a hint to use `...$xs` or `to-bytes`).  A `Direct`
/// stage carries no redirects — [`direct_spawnable`] gates on
/// `e.redirects.is_empty()` — so none are threaded through here.
#[derive(Clone, Debug)]
pub(super) struct ExternalStage {
    pub(super) id: CommandIdentity,
    pub(super) args: Vec<Value>,
}

/// Head resolution for one process-staged pipeline stage: a ral
/// computation evaluated in a helper, or an external command found on
/// `PATH`.  A bundled tool resolves to `External` like any other head;
/// its `ral --ral-bundled-tool` child is selected later by the command
/// image, not by a separate stage kind.  `External` carries the resolved
/// [`CommandIdentity`] so the launch decision needs no second `PATH` walk.
#[derive(Clone, Debug)]
enum StageKind {
    Ral,
    External(CommandIdentity),
}

/// The frozen launch decision for one process-staged stage, made once in
/// [`resolve_pipeline`] from facts all known at resolve time — the
/// terminal plan, the unified wire modes, the redirects, and whether a
/// `!{…}` audit is capturing bytes.
///
/// `Direct` spawns an external command with no stage helper — a host
/// binary, or, when the head is a bundled tool, the `ral
/// --ral-bundled-tool` child selected by the command image;
/// `HelperEval` evaluates the stage's ral computation in the helper.
/// Launch reads this decision rather than re-deriving it, and argv is
/// evaluated into the carried [`ExternalStage`] only for the `Direct`
/// path that consumes it — a helper-evaluated stage re-evaluates its
/// argv inside the child, so evaluating it here too would be redundant.
#[derive(Clone, Debug)]
pub(super) enum StageLaunch {
    Direct(ExternalStage),
    HelperEval,
}

/// Per-stage analysis result: resolved comp type, frozen dispatch
/// decision, and source position.
///
/// `launch` is the frozen dispatch decision for this stage — see
/// [`StageLaunch`].  Each interior edge's transport is not cached here:
/// the edge allocator [`super::route::open_stage_routes`] derives it
/// directly from the stage's position and `comp_type.output` (the
/// checker unified each producer's output with its consumer's input when
/// it emitted the wires).
#[derive(Clone, Debug)]
pub(super) struct StageSpec {
    pub(super) comp_type: crate::mode::PipeSpec,
    pub(super) launch: StageLaunch,
    pub(super) loc: crate::diagnostic::SourceLoc,
}

/// Build the source location for a pipeline stage: the active source
/// identity plus the span's byte offset converted into (line, col) via the
/// current source text held on `shell`.  Carried on the stage so a stage
/// error resolves against the right source at render time.
fn stage_loc(stage: &Comp, shell: &Shell) -> crate::diagnostic::SourceLoc {
    let (line, col) = match (stage.span, shell.turn.loc.source.as_ref()) {
        (Some(sp), Some(src)) => src.byte_to_line_col(sp.start as usize),
        (Some(sp), None) => (sp.start as usize, 0),
        (None, _) => (0, 0),
    };
    crate::diagnostic::SourceLoc {
        source: shell.turn.loc.current,
        line,
        col,
        len: 0,
    }
}

/// Decide which launcher kind a pipeline stage needs.
///
/// A byte stage — whether its head is a host binary or a bundled
/// coreutils / diffutils / ripgrep tool — is classified the same:
/// [`StageKind::External`] carrying the resolved [`CommandIdentity`], so
/// the caller threads it into launch without a second PATH walk.  Launch
/// then spawns it directly when [`direct_spawnable`] holds, and otherwise
/// evaluates it in a helper.  A bundled head's direct child is the `ral
/// --ral-bundled-tool` placement chosen later by the command image
/// (`command::build_command` reads `ExecImage::BundledTool`); nothing
/// here distinguishes it, so `ls`, `cat`, `wc`, … behave identically
/// everywhere, including on Windows where no `.exe` exists to spawn.
/// Handler-intercepted and builtin-bound heads route to Ral; admission
/// is left to the launch-time gate inside `command::vet`, so a denied
/// head still routes here and surfaces its denial through ral's
/// audit/error machinery.
fn classify_stage(stage: &Comp, shell: &Shell) -> StageKind {
    let CompKind::Exec(e) = &stage.item else {
        return StageKind::Ral;
    };
    match command_call::resolve_command_word(&e.head, shell) {
        command_call::Resolution::External(id) => StageKind::External(id),
        _ => StageKind::Ral,
    }
}

/// Evaluate a stage's argv into an [`ExternalStage`].
///
/// Run only for the launch path that consumes the result — `Direct`,
/// which [`direct_spawnable`] admits only for a redirect-free stage, so
/// the evaluated redirects are always empty and are dropped here.  A
/// `HelperEval` external re-evaluates its argv inside the child, so
/// evaluating it here as well would be a discarded second evaluation of
/// any effectful argument.
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

/// Whether stage `i` of `n` carries a value edge on either side: the
/// disjunction of [`super::route::value_edge_in`] and
/// [`super::route::value_edge_out`], the per-side predicates the edge
/// allocator [`super::route::open_stage_routes`] realizes.  Only an
/// evaluating helper, which reads the value channel before invoking, can
/// run a stage on either end of such an edge.
fn carries_value_edge(i: usize, n: usize, comp_type: crate::mode::PipeSpec) -> bool {
    super::route::value_edge_in(i, comp_type) || super::route::value_edge_out(i, n, comp_type)
}

/// Whether a pure external stage can be spawned directly, without a stage
/// helper.  Every condition is a resolve-time fact:
///
/// - the pipeline does not own the controlling terminal (a foreground
///   pipeline parks its stages on stop, which only the helper handles);
/// - the stage carries no value edge (see [`carries_value_edge`]);
/// - the stage has no redirects (the direct path wires only byte ends);
/// - no `!{…}` audit is capturing bytes (which needs the helper's byte
///   accounting).
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

/// Freeze the launch decision for one stage from its head resolution and
/// the resolve-time facts (see [`StageLaunch`]).
///
/// An external head — host binary or bundled tool — is spawned directly
/// when [`direct_spawnable`] holds, and otherwise evaluated in a helper.
/// A value-edge bundled stage is `direct_spawnable == false` (the
/// predicate excludes [`carries_value_edge`]), so it routes to
/// `HelperEval`: data-last application (`x | f = f !{x}`) is evaluator
/// work, and the bundled tool's in-process path still fires from command
/// dispatch inside that child, preserving the bundled-first policy.
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

/// Analyze a single pipeline stage given its resolved channel signature
/// and position.
///
/// `comp_type` is the stage's ground spec, read off the checker's
/// [`crate::mode::Wire`]; the mode is supplied rather than re-inferred
/// here.
fn analyze_stage(
    i: usize,
    n: usize,
    stage: &Comp,
    comp_type: crate::mode::PipeSpec,
    terminal: TerminalPlan,
    shell: &mut Shell,
) -> Settled<StageSpec> {
    let loc = stage_loc(stage, shell);
    let launch = resolve_launch(i, n, stage, comp_type, terminal, shell)?;
    Ok(StageSpec {
        comp_type,
        launch,
        loc,
    })
}

/// Frozen output of the resolve phase: per-stage analysis plus the
/// pipeline-level invariants derived from it.  Built once at the start of
/// `run_pipeline` and threaded through launch + collect.
pub(super) struct PipelinePlan {
    pub(super) kind: PipelineKind,
    pub(super) specs: Vec<StageSpec>,
    pub(super) terminal: TerminalPlan,
    pub(super) last_output: crate::mode::PipeMode,
}

fn resolve_terminal_plan(shell: &Shell) -> TerminalPlan {
    // The handoff authority is the session's terminal lease, lent only to a
    // turn whose `TerminalAccess` permits it.  No reachable lease → never
    // foreground: a `Denied` turn (an exarch tool turn), a backgrounded or
    // tty-less launch (the session minted no lease), and every platform with
    // no `tcsetpgrp` (the lease is never minted off Unix) all land here — the
    // old `startup_foreground` gate and the `cfg!(windows)` short-circuit both
    // collapse into this one question.
    if shell.terminal_lease().is_none() {
        return TerminalPlan::NoTerminal;
    }
    // With the lease held, foreground iff the final sink is terminal-bound, or
    // the turn is an explicit tty loan.  A captured pipeline (`!{...}`) has a
    // buffer sink, so it does not foreground — the let-binding bytes are bound
    // for memory, not the terminal.  The `_ed-tui` loan is the positive
    // exception: its stdout is captured so the plugin can read the body's
    // selection, but the body (e.g. `fzf`) draws on `/dev/tty` and *must* own
    // the foreground pgid or its first `tcsetattr` raises SIGTTOU; the host
    // suspended its own surface and raised the turn to `ExplicitLoan` for
    // exactly this window.  This replaces both the old `capture_depth` gate
    // and its `tui_active` exception with a single final-sink/loan rule.
    let loan = matches!(shell.turn.terminal_access, TerminalAccess::ExplicitLoan);
    let terminal_bound = matches!(
        shell.turn.io.stdout,
        crate::io::Sink::Terminal | crate::io::Sink::External(_)
    );
    if terminal_bound || loan {
        TerminalPlan::ForegroundExternalGroup
    } else {
        TerminalPlan::NoTerminal
    }
}

/// Build each stage's [`StageSpec`] from the checker's ground wires.
///
/// Each stage's channel signature is the wire's [`crate::mode::PipeSpec`];
/// adjacency holds by construction (the checker unified the wires it
/// emitted), so a debug build only asserts it.
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

/// Resolve phase: read each checked wire, validate channel adjacency,
/// classify each stage's launch path, and freeze the pipeline-level kind
/// plus final output mode.  The byte-capturing audit decision is
/// consulted live during launch classification rather than stored on the
/// plan.
///
/// Each stage's channel signature is read off its ground wire — the
/// checker annotates every evaluated pipeline.  The only shell effect
/// here is argv evaluation for directly-spawned external stages; no
/// process or pipe is created here.
pub(super) fn resolve_pipeline(
    stages: &[Arc<Comp>],
    wires: &[crate::mode::Wire],
    shell: &mut Shell,
) -> Settled<PipelinePlan> {
    // The launch decision for each stage depends on whether the pipeline
    // owns the controlling terminal, so the terminal plan is frozen
    // before any stage is analyzed.  It reads only boot/capture state,
    // which stage argv evaluation does not mutate.
    let terminal = resolve_terminal_plan(shell);
    let specs = specs_from_wires(stages, wires, terminal, shell)?;

    // `run_pipeline` is only ever called with ≥1 stage — single-stage
    // via `command/uutils.rs`'s `slice::from_ref` wrapper, ≥2 stages
    // via the elaborator's `Ast::Pipeline` lowering.  An empty `specs`
    // would mean a caller bypassed both routes; pin the invariant
    // rather than fabricating a `Mode::None` fallback that lies about
    // an impossible empty pipeline.
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

    /// A shell whose session owns a terminal lease and whose installed turn
    /// holds `Leased` access — the interactive REPL and a terminal-launched
    /// script.  Stdout defaults to `Sink::Terminal` (terminal-bound).
    fn leased_shell() -> Shell {
        let mut shell = Shell::default();
        shell.turn.io.interactive = true;
        shell.turn.io.terminal.startup_stdin_tty = true;
        shell.turn.io.terminal.startup_stdout_tty = true;
        // Hand the session a lease and the turn the authority to borrow it.
        shell.session.terminal_lease = crate::process::TerminalLease::mint_at_startup(true);
        shell.turn.terminal_access = TerminalAccess::Leased;
        shell
    }

    /// A Leased turn with a terminal-bound sink foregrounds its pipeline.
    #[test]
    #[cfg(unix)]
    fn leased_terminal_bound_pipeline_foregrounds() {
        let shell = leased_shell();
        assert_eq!(
            resolve_terminal_plan(&shell),
            TerminalPlan::ForegroundExternalGroup,
        );
    }

    /// A captured pipeline (`!{...}`) has a buffer sink, not a terminal one,
    /// so it must not steal foreground — even in a Leased turn.
    #[test]
    fn leased_captured_pipeline_skips_foreground() {
        let mut shell = leased_shell();
        let (sink, _buf) = crate::io::new_buffer();
        shell.turn.io.stdout = sink;
        assert_eq!(resolve_terminal_plan(&shell), TerminalPlan::NoTerminal);
    }

    /// `_ed-tui` captures stdout to read the body's selection, but the body
    /// (e.g. `fzf`) draws on `/dev/tty` and must own the foreground pgid.  The
    /// explicit loan foregrounds despite the buffer sink.  Regression for the
    /// CTRL-R / fzf-history SIGTTOU failure, re-pinned without `tui_active`.
    #[test]
    #[cfg(unix)]
    fn ed_tui_loan_foregrounds_captured_pipeline() {
        let mut shell = leased_shell();
        let (sink, _buf) = crate::io::new_buffer();
        shell.turn.io.stdout = sink;
        shell.turn.terminal_access = TerminalAccess::ExplicitLoan;
        assert_eq!(
            resolve_terminal_plan(&shell),
            TerminalPlan::ForegroundExternalGroup,
        );
    }

    /// A non-interactive script launched at a terminal holds a `Leased` turn
    /// exactly like the REPL, so its pipeline foregrounds — otherwise an
    /// interactive child (`claude`, `fzf`) raises SIGTTOU on its first
    /// `tcsetattr` from a background pgroup.  Regression for the
    /// `run-claude.ral` SIGTTOU teardown.
    #[test]
    #[cfg(unix)]
    fn terminal_script_leased_pipeline_foregrounds() {
        let mut shell = leased_shell();
        shell.turn.io.interactive = false;
        assert_eq!(
            resolve_terminal_plan(&shell),
            TerminalPlan::ForegroundExternalGroup,
        );
    }

    /// A `Denied` turn never foregrounds, *even though the session owns a
    /// lease* — the exarch tool-turn case.  The lease borrow is unreachable
    /// from a `Denied` turn, so the SIGTTIN handoff is unrepresentable.
    #[test]
    #[cfg(unix)]
    fn denied_turn_skips_foreground() {
        let mut shell = leased_shell();
        shell.turn.terminal_access = TerminalAccess::Denied;
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );
        assert_eq!(resolve_terminal_plan(&shell), TerminalPlan::NoTerminal);
    }

    /// A launch that never owned the terminal foreground (backgrounded
    /// `ral … &`, a piped or tty-less eval) minted no lease, so even a
    /// `Leased` turn cannot borrow one and never foregrounds.
    #[test]
    fn no_lease_skips_foreground() {
        let mut shell = leased_shell();
        shell.session.terminal_lease = None;
        assert_eq!(resolve_terminal_plan(&shell), TerminalPlan::NoTerminal);
    }
}
