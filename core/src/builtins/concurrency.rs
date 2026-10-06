//! Concurrency: the births `spawn`, `watch`, `service`, `detach`, and the
//! eliminators `await`, `poll`, `race`, `cancel`.
//!
//! A spawned block runs on its own OS thread with a cloned environment, parked
//! under the durable session root rather than the foreground scope, so a run
//! deadline or interrupt cannot reach it.  Its bytes are buffered per handle
//! (surfaced live line by line, for `watch`) and projected out of one cached
//! [`CompletedHandle`]; its `surface` events are buffered too — the spawning
//! run may be over — and reach a sink exactly once, by an eliminator's replay
//! or by the completion delivery, whichever wins the `joined` latch.

mod birth;
mod eliminate;
#[cfg(test)]
mod tests;

use crate::evaluator::machine;
use crate::first_order::FOValue;
use crate::types::{Closure, Mooring, Settled, Shell, Value, sig};
use birth::{Birth, spawn_child};
pub(super) use eliminate::{builtin_await, builtin_cancel, builtin_poll, builtin_race};

#[cfg(unix)]
use super::util::check_arity;

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

/// `spawn <thunk>` -- spawn a concurrent block on a worker thread, return a handle.
///
/// Buffered: stdout/stderr accumulate in per-handle buffers and come back as
/// the `stdout`/`stderr` fields of `await`'s record.  The worker's own `Shell` is the only one the
/// body touches, so "blocks discard their state" falls out of the thread's
/// lifecycle with no boundary ceremony.
pub(crate) fn builtin_spawn(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let closure = args[0].expect_thunk("spawn")?;
    let name = shell
        .call_site()
        .map_or_else(|| "block".into(), |site| format!("block at {site}"));
    Ok(Value::Handle(Box::new(spawn_child(
        mooring,
        shell,
        Birth::Spawn,
        &name,
        worker_body(closure),
    )?)))
}

// ── watch ────────────────────────────────────────────────────────────────

/// One line of a watched stream as the `` `watch [label, line] `` surface.
fn watch_event(label: &str, line: &[u8]) -> FOValue {
    let text = |value: String| FOValue::String { value };
    FOValue::Variant {
        label: "watch".into(),
        payload: Some(Box::new(FOValue::Map {
            entries: vec![
                ("label".into(), text(label.to_string())),
                (
                    "line".into(),
                    text(String::from_utf8_lossy(line).into_owned()),
                ),
            ],
        })),
    }
}

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
    let name = label.clone();
    Ok(Value::Handle(Box::new(spawn_child(
        mooring,
        shell,
        Birth::Watch { label },
        &name,
        worker_body(closure),
    )?)))
}

// ── service ──────────────────────────────────────────────────────────────

/// The legibility bound the two births that escape the lease chain carry: a
/// non-empty, single-line description.  For `detach` it is the only thing that
/// later says what a surviving pid was for — there is no handle left to ask.
fn one_line_desc(arg: &Value, verb: &str) -> Settled<String> {
    let Value::String(s) = arg else {
        return Err(sig(format!(
            "{verb}: description must be a String, got {}",
            arg.type_name()
        )));
    };
    let desc = s.trim();
    if desc.is_empty() {
        return Err(sig(format!("{verb}: description must be non-empty")));
    }
    if desc.contains('\n') {
        return Err(sig(format!(
            "{verb}: description must be a single line (no newlines)"
        )));
    }
    Ok(desc.to_string())
}

/// `service <desc> <thunk>` -- birth a durable worker: an ordinary buffered
/// spawn but for its [`LeaseClass::Durable`] registration — no idle reap, no
/// backstop, and `desc` (the registry `cmd`) as the bound standing in for time.
///
/// Host-wise the mirror image of `watch`: only an agent host, whose lease frame
/// would otherwise reap long work, installs
/// [`crate::builtins::SERVICE_BUILTIN`] — grant no lease and a durable class
/// would distinguish nothing.
pub(super) fn builtin_service(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let desc = one_line_desc(&args[0], "service")?;
    Ok(Value::Handle(Box::new(spawn_child(
        mooring,
        shell,
        Birth::Service,
        &desc,
        worker_body(args[1].expect_thunk("service")?),
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
    let desc = one_line_desc(&args[0], "detach")?;
    crate::runtime::command::detach(&desc, &args[1], &args[2..], mooring, shell)
}
