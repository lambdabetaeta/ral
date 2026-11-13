//! Concurrency: the births `spawn`, `watch`, `detach`, the listing `workers`,
//! and the eliminators and observations `await`, `poll`, `is-done`,
//! `first-settled`, `race`, `cancel`.
//!
//! Every worker has one owner, fixed at birth: the gather it was born inside
//! (a `gather { … }`, or a worker's body), which joins it at its exit; the
//! session; or nobody (`detach`).  A worker's bytes are buffered per handle
//! and replayed by `await`.

mod birth;
mod eliminate;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

use crate::fact::{InvalidCause, Multiline};
use crate::ir::Comp;
use crate::shell::evaluator::machine;
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Env, Mooring, Settled, Shell, TitlePolicy, ValClos};
pub(crate) use birth::exit_brood;
use birth::{Birth, spawn_child};
use eliminate::join;
pub(crate) use eliminate::outlive;
pub(super) use eliminate::{
    builtin_await, builtin_cancel, builtin_first_settled, builtin_is_done, builtin_poll,
    builtin_race, builtin_workers,
};
use std::sync::Arc;

// ── spawn ────────────────────────────────────────────────────────────────

/// The worker body handed to [`spawn_child`]: the whole computation of a fresh
/// thread, run as its own closed machine.  Every thread a shell spawns starts
/// from that shell's session; a worker forces the thunk it was handed, whose
/// capture travels in its closure.
fn worker_body(
    comp: Arc<Comp>,
    env: Env,
) -> impl FnOnce(&Mooring, &mut Shell) -> Settled<ValClos> + Send + 'static {
    move |mooring, child| machine::evaluate(comp, env, mooring, child)
}

/// `spawn [TITLE] BODY` -- run `body` on a worker of its own and return its
/// handle; the title names it to people.
///
/// Buffered: stdout/stderr accumulate in per-handle buffers, which `await`
/// replays to the awaiter's own streams.  The worker's own `Shell` is the only
/// one the body touches, so "blocks discard their state" falls out of the
/// thread's lifecycle with no boundary ceremony.
pub(crate) fn spawn_form(
    title: Option<String>,
    body: &ValClos,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    if title.is_none() && mooring.worker_title == TitlePolicy::Required {
        return Err(invalid("spawn", InvalidCause::TitleRequired).into());
    }
    let (comp, env) = body.expect_thunk("spawn")?;
    Ok(ValClos::handle(spawn_child(
        mooring,
        shell,
        Birth::Spawn { title },
        worker_body(comp, env),
    )?))
}

/// A worker's title or a detached process's description: non-empty and one
/// line, since it is all a person reading a listing or a `ps` has to go on.
pub(crate) fn legible_title(arg: &ValClos, verb: &str) -> Settled<String> {
    let title = arg.as_str(verb)?.trim();
    if title.is_empty() {
        return Err(invalid(verb, InvalidCause::Empty).into());
    }
    if title.contains('\n') {
        return Err(invalid(
            verb,
            InvalidCause::Multiline(Multiline {
                lines: title.lines().count().into(),
            }),
        )
        .into());
    }
    Ok(title.to_string())
}

// ── watch ────────────────────────────────────────────────────────────────

/// `watch <label> <thunk>` -- spawn a concurrent block whose output lines
/// surface live to the host, each labelled.
///
/// A watched worker surfaces on past the run that spawned it, so only a host
/// that installs a deferred sink installs [`crate::shell::builtins::WATCH_BUILTIN`].
/// Without that entry or another binding, a bare `watch` head falls back to an
/// external command, whose arguments cannot include a thunk.
///
/// The child writes through `Sink::Watch`, so each whole line is its own
/// `` `watch `` surface on the session's deferred sink, stderr's under
/// `label:err`.  The byte buffers stay empty, so `await`'s replay drain is a
/// no-op.
pub(super) fn builtin_watch(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let label = args[0].as_str("watch")?.to_owned();
    let (comp, env) = args[1].expect_thunk("watch")?;
    Ok(ValClos::handle(spawn_child(
        mooring,
        shell,
        Birth::Watch { label },
        worker_body(comp, env),
    )?))
}

// ── detach ───────────────────────────────────────────────────────────────

/// `detach <desc> <program> <args>` -- birth a process this session stops owning,
/// returning a receipt rather than a handle.
///
/// The one concurrency verb whose axis is *ownership*: the others reify a
/// handle over work that dies with this process, while a detached
/// program is double-forked away and reparented to init, so no eliminator
/// applies and no teardown here can reach it.  This builtin is the surface
/// discipline only; resolution, vetting, and the receipt live at the exec
/// boundary (`shell::exec::detach`).
#[cfg(unix)]
pub(super) fn builtin_detach(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let desc = legible_title(&args[0], "detach")?;
    let program = args[1].as_str("detach")?;
    if program.is_empty() {
        return Err(invalid("detach", InvalidCause::Empty).into());
    }
    let argv = args[2]
        .as_list("detach")?
        .iter()
        .map(std::borrow::Cow::into_owned)
        .collect::<Vec<_>>();
    crate::shell::exec::detach(&desc, program, &argv, mooring, shell)
}
