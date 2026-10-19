//! The eliminators `await`, `poll`, `race` and `cancel`: how a worker's handle
//! is observed, settled and stopped.

use crate::fact::Io;
use crate::process::WorkerStop;
use crate::types::{
    Break, CompletedHandle, Error, ErrorRecord, HandleInner, HandleState, Settled, Shell,
    ThreadEnd, Value, outcome_value, sig,
};

/// Stop a handle: the policy `cancel` and `race`'s loser cleanup share.  An
/// already-completed handle keeps its cached outcome, so a finished worker's
/// value is never destroyed by a losing `race` or a `cancel` that lost the toss.
///
/// The `state` guard spans the test and the transition, so there is no window
/// for a worker to finish in between and have its outcome torn out from under
/// it: every transition holds this lock, and takes `result` and `cached` under
/// it, never the other way round.
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

/// Admit the value a worker settled with against the site its handle was
/// reacquired at, if it was: observers of one worker at two sites check it
/// at each.
fn admit_handled(handle: &HandleInner, value: &Value, shell: &Shell) -> Settled<()> {
    match &handle.site {
        Some(site) => site
            .admit_handled(value, shell.fixings())
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
    let value = match completed.end {
        ThreadEnd::Settled(settled) => settled?,
        ThreadEnd::Panicked(text) => std::panic::resume_unwind(Box::new(text)),
    };
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
/// a handle settles.  Between sweeps it polls the shell, so a foreground
/// cancel unwinds the wait — but the workers hang at the durable root, so a
/// cut-short wait leaves them observable later.  A set of cancelled handles
/// is the awaiter's own error: the cancel this run made.
fn wait_first_settled<'a>(
    handles: &[&'a HandleInner],
    shell: &Shell,
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
            return Err(if handles.is_empty() {
                sig("no live handles to wait for")
            } else {
                Error::worker_stopped(WorkerStop::Cancelled).into()
            });
        }
        shell.poll()?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Block in the foreground until `handle` completes and return its result
/// record, re-raising a failed block.
pub(super) fn await_handle(handle: &HandleInner, shell: &Shell) -> Settled<Value> {
    let (_, completed) = wait_first_settled(&[handle], shell)?;
    shell.local.workers.remove(handle);
    project_completed(completed, handle, shell)
}

/// `await <handle>` -- wait for a concurrent block to complete and return its result record.
pub(crate) fn builtin_await(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("await")?;
    await_handle(handle, shell)
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
/// Total over every handle: a cancelled one settles with the owner's record;
/// a failed one as data; an `exit`, a foreign stop and a panic are re-raised,
/// as `await` re-raises them.
pub(crate) fn builtin_poll(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("poll")?;
    handle.renew();
    let result = if handle.state() == HandleState::Cancelled {
        // `cancel` abandoned the outcome; the owner's return clause is the record.
        let io = Io::of(handle.stdout_buf.peek(), handle.stderr_buf.peek());
        Value::variant(
            "settled",
            Some(settled(
                io,
                Err(Error::worker_stopped(WorkerStop::Cancelled).record(shell)),
            )),
        )
    } else if let Some(completed) = handle.try_settle() {
        // A settled poll observes as `await` does, so it removes the entry too.
        shell.local.workers.remove(handle);
        let outcome = match completed.end {
            ThreadEnd::Settled(Ok(value)) => {
                admit_handled(handle, &value, shell)?;
                Ok(value)
            }
            ThreadEnd::Settled(Err(Break::Error(e))) => Err(e.record(shell)),
            ThreadEnd::Settled(Err(escape)) => return Err(escape),
            ThreadEnd::Panicked(text) => std::panic::resume_unwind(Box::new(text)),
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
pub(crate) fn builtin_race(args: &[Value], shell: &Shell) -> Settled<Value> {
    let values: Vec<Value> = args[0]
        .as_list("race")?
        .iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
    let handles = values
        .iter()
        .map(|v| v.expect_handle("race"))
        .collect::<Result<Vec<_>, _>>()?;

    let (winner, completed) = wait_first_settled(&handles, shell)?;
    shell.local.workers.remove(winner);
    for &h in &handles {
        if h != winner {
            stop_handle(h);
            shell.local.workers.remove(h);
        }
    }
    project_completed(completed, winner, shell)
}

/// `poll`'s `` `settled `` payload: the streams and how the block ended; the
/// checker's `settle_record` is its type.
fn settled(io: Io, outcome: Result<Value, ErrorRecord>) -> Value {
    beside(io, "outcome", outcome_value(outcome))
}
