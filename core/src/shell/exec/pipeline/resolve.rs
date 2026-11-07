//! Pipeline resolve: freeze each stage's launch decision.  No process is
//! created and no pipe opened; launch reads everything this phase produces.

use super::super::command::Head;
use super::super::dispatch;
use super::StageEval;
use crate::ir::{Comp, CompKind};
use crate::shell::runtime::{Break, Env, Settled, Shell, Value};
use crate::source::Span;
use std::sync::Arc;

/// One stage's launch decision, frozen here and read by launch rather than
/// re-derived.  Argv is evaluated only on the `Direct` path: a `Thread` stage
/// re-evaluates its own inside the thread, so doing it here too would run
/// effectful arguments twice.  Args stay `Value`s so launch-time
/// `command::vet` applies the same shape rejection single-command exec does.
#[derive(Debug)]
pub(super) enum StageLaunch {
    Direct { head: Head, args: Vec<Value> },
    Thread,
}

/// Per-stage analysis.  Edge transport is deliberately absent: the allocator
/// `route::open_stage_routes` derives each edge from the stage's position
/// alone.
#[derive(Debug)]
pub(super) struct StageSpec {
    pub(super) launch: StageLaunch,
    /// So a parent-side error points at this stage, not the whole pipeline.
    pub(super) span: Option<Span>,
}

/// A thread unless the head is an external with no redirect and no
/// byte-capturing audit in force — a redirect needs a thread's fd table, a
/// capture its accounting.  Admission is `Head::admit`'s at launch, so a head
/// the grant denies still routes through here and refuses as an ordinary error.
fn resolve_launch(stage: &Comp, env: &Env, eval: StageEval, shell: &Shell) -> Settled<StageLaunch> {
    let CompKind::Exec(exec) = &stage.item else {
        return Ok(StageLaunch::Thread);
    };
    let dispatch::Resolution::External(head) =
        dispatch::resolve_command_word(&exec.head, env, shell)
    else {
        return Ok(StageLaunch::Thread);
    };
    if !exec.redirects.is_empty() || shell.local.audit.captures_bytes() {
        return Ok(StageLaunch::Thread);
    }
    Ok(StageLaunch::Direct {
        head,
        args: (eval.close_argv)(&exec.argv, env, &shell.sig).map_err(|error| {
            Break::from(error).locate(&exec.head.name().written(), stage.span, shell)
        })?,
    })
}

/// Resolve phase: freeze every stage's launch path.
pub(super) fn resolve_pipeline(
    stages: &[Arc<Comp>],
    env: &Env,
    eval: StageEval,
    shell: &Shell,
) -> Settled<Vec<StageSpec>> {
    stages
        .iter()
        .map(|stage| {
            Ok(StageSpec {
                launch: resolve_launch(stage, env, eval, shell)?,
                span: stage.span,
            })
        })
        .collect()
}
