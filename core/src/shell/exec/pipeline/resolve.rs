//! Pipeline resolve: freeze each stage's launch decision.  No process is
//! created and no pipe opened; launch reads everything this phase produces.

use super::super::command::{Command, Request};
use super::super::resolve::Resolution;
use super::StageEval;
use crate::ir::{Comp, CompKind};
use crate::shell::runtime::{Break, Env, Settled, Shell, ValClos};
use std::sync::Arc;

/// One stage's launch decision, frozen here and read by launch rather than
/// re-derived.  Argv is evaluated only on the `Direct` path: a `Thread` stage
/// re-evaluates its own inside the thread, so doing it here too would run
/// effectful arguments twice.  Args stay `ValClos`s so launch-time
/// `command::check_argv` applies the same shape rejection single-command exec does.
#[derive(Debug)]
pub(super) enum StageLaunch {
    Direct {
        command: Command<Request>,
        args: Vec<ValClos>,
    },
    Thread,
}

/// A thread unless the command is an external with no redirect and no
/// byte-capturing audit in force: a redirect swaps sinks on a shell's `Io`, a
/// capture tees them, and a direct stage has no shell.  Admission is
/// `Command::admit`'s at launch, so a command the grant denies still routes through
/// here and refuses as an ordinary error.
fn resolve_launch(stage: &Comp, env: &Env, eval: StageEval, shell: &Shell) -> Settled<StageLaunch> {
    let CompKind::Exec(exec) = &stage.item else {
        return Ok(StageLaunch::Thread);
    };
    let Resolution::External(command) = Resolution::of(&exec.head, env, shell) else {
        return Ok(StageLaunch::Thread);
    };
    if !exec.redirects.is_empty() || shell.local.audit.captures_bytes() {
        return Ok(StageLaunch::Thread);
    }
    Ok(StageLaunch::Direct {
        command,
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
) -> Settled<Vec<StageLaunch>> {
    stages
        .iter()
        .map(|stage| resolve_launch(stage, env, eval, shell))
        .collect()
}
