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
mod tests;

#[cfg(unix)]
use super::util::check_arity;
use crate::evaluator::machine;
use crate::types::{Closure, Mooring, Settled, Shell, TitlePolicy, Value, sig, sig_hint};
pub(crate) use birth::exit_brood;
use birth::{Birth, spawn_child};
use eliminate::join;
pub(super) use eliminate::{
    builtin_await, builtin_cancel, builtin_first_settled, builtin_is_done, builtin_poll,
    builtin_race, builtin_workers,
};

// ── spawn ────────────────────────────────────────────────────────────────

/// The worker body handed to [`spawn_child`]: the whole computation of a fresh
/// thread, run as its own closed machine.  Every thread a shell spawns starts
/// from that shell's session; a worker forces the thunk it was handed, whose
/// capture travels in its closure.
fn worker_body(
    closure: Closure,
) -> impl FnOnce(&Mooring, &mut Shell) -> Settled<Value> + Send + 'static {
    move |mooring, child| machine::force(Value::Thunk(closure), mooring, child)
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
    body: &Value,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    if title.is_none() && mooring.worker_title == TitlePolicy::Required {
        return Err(sig(
            "spawn: this host requires a title: spawn \"what it does\" { … }",
        ));
    }
    let closure = body.expect_thunk("spawn")?;
    Ok(Value::Handle(Box::new(spawn_child(
        mooring,
        shell,
        Birth::Spawn { title },
        worker_body(closure),
    )?)))
}

/// A worker's title or a detached process's description: non-empty and one
/// line, since it is all a person reading a listing or a `ps` has to go on.
pub(crate) fn legible_title(arg: &Value, verb: &str) -> Settled<String> {
    let Value::String(s) = arg else {
        return Err(sig(format!(
            "{verb}: the title must be a String, got {}",
            arg.type_name()
        )));
    };
    let title = s.trim();
    let hint = "say what the work is in a few words, on one line: \"build the docs\"";
    if title.is_empty() {
        return Err(sig_hint(
            format!("{verb}: the title must be a non-empty single line, got an empty one"),
            hint,
        ));
    }
    if title.contains('\n') {
        return Err(sig_hint(
            format!(
                "{verb}: the title must be a non-empty single line, got {} lines",
                title.lines().count()
            ),
            hint,
        ));
    }
    Ok(title.to_string())
}

// ── watch ────────────────────────────────────────────────────────────────

/// `watch <label> <thunk>` -- spawn a concurrent block whose output lines
/// surface live to the host, each labelled.
///
/// A watched worker surfaces on past the run that spawned it, so only a host
/// that installs a deferred sink installs [`crate::builtins::WATCH_BUILTIN`];
/// naming `watch` elsewhere is an unknown-name diagnostic, not a runtime
/// refusal.
///
/// The child writes through `Sink::Watch`, so each whole line is its own
/// `` `watch `` surface on the session's deferred sink, stderr's under
/// `label:err`.  The byte buffers stay empty, so `await`'s replay drain is a
/// no-op.
pub(super) fn builtin_watch(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let label = args[0].as_str("watch")?.to_owned();
    let closure = args[1].expect_thunk("watch")?;
    Ok(Value::Handle(Box::new(spawn_child(
        mooring,
        shell,
        Birth::Watch { label },
        worker_body(closure),
    )?)))
}

// ── detach ───────────────────────────────────────────────────────────────

/// `detach <desc> <cmd> <args…>` -- birth a process this session stops owning,
/// returning a receipt rather than a handle.
///
/// The one concurrency verb whose axis is *ownership*: the others reify a
/// [`Value::Handle`] over work that dies with this process, while a detached
/// program is double-forked away and reparented to init, so no eliminator
/// applies and no teardown here can reach it.  This door is the surface
/// discipline only; resolution, vetting, and the receipt live at the exec
/// boundary (`runtime::command::detach`).  Installed only by a host that arms a
/// detach policy ([`Shell::arm_detach`]).
#[cfg(unix)]
pub(super) fn builtin_detach(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    check_arity(args, 2, "detach")?;
    let desc = legible_title(&args[0], "detach")?;
    crate::runtime::command::detach(&desc, &args[1], &args[2..], mooring, shell)
}
