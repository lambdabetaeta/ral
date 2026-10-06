//! The eliminators `await`, `poll`, `race` and `cancel`: how a worker's handle
//! is observed, settled and stopped.

use crate::fact::Io;
use crate::types::{
    Break, CompletedHandle, Error, ErrorRecord, Escape, HandleInner, HandleState, Mooring, Settled,
    Shell, Value, outcome_value, sig, sig_hint,
};

/// Stop a handle: the policy `cancel` and `race`'s loser cleanup share.  An
/// already-completed handle keeps its cached outcome, so a finished worker's
/// value is never destroyed by a losing `race` or a `cancel` that lost the toss.
///
/// The `state` guard spans the test and the transition, so there is no window
/// for a worker to finish in between and have its outcome torn out from under
/// it: every transition holds this lock, and takes `result` and `cached` under
/// it, never the other way round.  The scope fires after the guard is dropped —
/// `CancelScope::cancel` runs every armed watcher on this thread, and nothing
/// that runs arbitrary code belongs inside a handle's transition.
fn stop_handle(handle: &HandleInner) {
    if handle.detach() {
        handle.cancel.cancel(crate::process::CancelCause::Cancelled);
    }
}

/// `cancel <handle>` -- mark a running concurrent block as cancelled.
pub(crate) fn builtin_cancel(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("cancel")?;
    stop_handle(handle);
    shell.local.workers.remove(handle);
    Ok(Value::Unit)
}

/// The pre-check `await` and `poll` share: a cancelled handle has no result to
/// wait for or sample, so observing one is an error.
fn ensure_live(handle: &HandleInner) -> Settled<()> {
    if handle.state() == HandleState::Cancelled {
        return Err(sig_hint(
            "handle is cancelled",
            "use try around await to handle cancellation",
        ));
    }
    Ok(())
}

/// Replay a finished detached worker's buffered surface events through the
/// awaiting run's *current* surface, once.  Only `await`/`race` call this, so a
/// polled-but-never-awaited handle emits nothing and a repeat `await` does not
/// duplicate.
fn replay_deferred_surface(handle: &HandleInner, completed: &CompletedHandle, mooring: &Mooring) {
    if !handle.joined.claim() {
        return;
    }
    if let Some(sink) = mooring.surface.as_ref() {
        for ev in &completed.surface {
            sink.emit(ev);
        }
    }
}

/// Admit the value a worker settled with against the site its handle was
/// reacquired at, if it was: observers of one worker at two sites check it
/// at each.
fn admit_handled(handle: &HandleInner, value: &Value, shell: &Shell) -> Settled<()> {
    match &handle.site {
        Some(site) => site
            .admit_handled(value)
            .map_err(|mismatch| mismatch.refusal("service-handle", shell)),
        None => Ok(()),
    }
}

/// Project a finished block's outcome to the `await`/`race` record, re-raising
/// an `Err` verbatim.
fn project_completed(
    completed: CompletedHandle,
    handle: &HandleInner,
    shell: &Shell,
) -> Settled<Value> {
    let value = completed.outcome?;
    admit_handled(handle, &value, shell)?;
    Ok(awaited(value, Io::of(completed.stdout, completed.stderr)))
}

/// The `{value, stdout, stderr}` record `await` and `race` return; the
/// checker's `await_record` is its type.
fn awaited(value: Value, io: Io) -> Value {
    beside(io, "value", value)
}

/// `io` as a record with one more field, the shape `Ty::extend` gives its type.
fn beside(io: Io, key: &str, v: Value) -> Value {
    let Value::Map(m) = Value::from_datum(io) else {
        unreachable!("an Io encodes as a record");
    };
    let fields = m.iter().map(|(k, v)| (k.to_string(), v.into_owned()));
    Value::map(fields.chain([(key.to_string(), v)]).collect())
}

/// One cancel-aware foreground wait, shared by `await` and `race`: sweep until
/// a handle settles, erroring when none remain live.  Between sweeps it polls
/// `Mooring::check`, so a foreground cancel unwinds the wait — but the workers
/// hang at the durable root, so a cut-short wait leaves them observable later.
fn wait_first_settled<'a>(
    handles: &[&'a HandleInner],
    mooring: &Mooring,
) -> Settled<(&'a HandleInner, CompletedHandle)> {
    loop {
        let mut saw_running = false;
        for &handle in handles {
            // A blocked wait is continuous observation: each sweep renews
            // every named handle's idle lease, so none is reaped mid-wait.
            handle.renew();
            match handle.state() {
                HandleState::Cancelled => continue,
                HandleState::Running => saw_running = true,
                HandleState::Completed => {}
            }
            if let Some(completed) = handle.try_settle() {
                return Ok((handle, completed));
            }
        }
        if !saw_running {
            return Err(sig("no live handles to wait for"));
        }
        mooring.check()?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Block in the foreground until `handle` completes, replay its buffered
/// surface, and return its result record, re-raising a failed block.
pub(super) fn await_handle(
    handle: &HandleInner,
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<Value> {
    ensure_live(handle)?;
    let (_, completed) = wait_first_settled(&[handle], mooring)?;
    shell.local.workers.remove(handle);
    replay_deferred_surface(handle, &completed, mooring);
    project_completed(completed, handle, shell)
}

/// `await <handle>` -- wait for a concurrent block to complete and return its result record.
pub(crate) fn builtin_await(args: &[Value], mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("await")?;
    await_handle(handle, mooring, shell)
}

/// `poll <handle>` -- non-blocking, total sample of a concurrent block:
/// `` `settled `` `{stdout, stderr, outcome}` once it has finished (returned,
/// raised, or panicked), `` `pending `` `{stdout, stderr}` while it runs.
///
/// The pending bytes are a cumulative, non-destructive snapshot
/// ([`CapturedBytes::peek`], not [`CapturedBytes::take`]), so a partial poll never steals bytes
/// the one-shot completion drain must still see — and repeated pending polls
/// are therefore non-idempotent.  A watched handle's
/// buffers stay empty, so a pending poll on one reports nothing.
///
/// Errors only on a cancelled handle ([`ensure_live`]).  A failed block is
/// reported as data, not re-raised: `poll` never re-raises, whatever the
/// block's own status — that lives in `outcome.err.status`.
pub(crate) fn builtin_poll(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("poll")?;
    ensure_live(handle)?;
    // Both arms are observations, so the touch lands once at entry, before the
    // settle attempt decides which arm it is.
    handle.renew();
    let result = if let Some(completed) = handle.try_settle() {
        // A settled poll observes as `await` does, so it removes the entry too.
        shell.local.workers.remove(handle);
        let outcome = match completed.outcome {
            Ok(value) => {
                admit_handled(handle, &value, shell)?;
                Ok(value)
            }
            Err(e) => Err(break_record(&e, shell)),
        };
        Value::variant(
            "settled",
            Some(settled(Io::of(completed.stdout, completed.stderr), outcome)),
        )
    } else {
        let pending = Io::of(handle.stdout_buf.peek(), handle.stderr_buf.peek());
        Value::variant("pending", Some(Value::from_datum(pending)))
    };
    Ok(result)
}

/// `race <handles>` -- wait for the first of several blocks to finish, stopping
/// the rest, then project the winner's outcome as `await` does.
pub(crate) fn builtin_race(args: &[Value], mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    let values: Vec<Value> = args[0]
        .as_list("race")?
        .iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
    let handles = values
        .iter()
        .map(|v| v.expect_handle("race"))
        .collect::<Result<Vec<_>, _>>()?;

    let (winner, completed) = wait_first_settled(&handles, mooring)?;
    shell.local.workers.remove(winner);
    for &h in &handles {
        if h != winner {
            stop_handle(h);
            shell.local.workers.remove(h);
        }
    }
    replay_deferred_surface(winner, &completed, mooring);
    project_completed(completed, winner, shell)
}

/// `poll`'s `` `settled `` payload: the streams and how the block ended; the
/// checker's `settle_record` is its type.
fn settled(io: Io, outcome: Result<Value, ErrorRecord>) -> Value {
    beside(io, "outcome", outcome_value(outcome))
}

/// `poll`'s `` `err `` payload and the `` `done `` event's: the same record
/// `try` hands its handler thunk.  An `Escape` carries no located message and
/// names no command, so it has no site.
pub(super) fn break_record(e: &Break, shell: &Shell) -> ErrorRecord {
    match e {
        Break::Error(err) => err.record(shell),
        Break::Escape(Escape::Exit(code)) => Error::raised("block exited", *code).record_at(None),
    }
}
