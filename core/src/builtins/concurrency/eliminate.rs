//! The eliminators `await`, `poll`, `race` and `cancel`: how a worker's handle
//! is observed, settled and stopped; `workers`, the listing, which is data;
//! and `worker`, the door back from it to a handle.
//!
//! `await` behaves as if the block had run where it is awaited: the bytes the
//! worker wrote are written to the awaiter's own streams, then it ends as the
//! block ended, with its value, its failure, its exit or its stop.  `race` is
//! `await` on the first to settle, the rest stopped and drained.  Each of
//! these claims what it settles.  `poll` is the one observation that returns
//! data instead: it writes nothing and raises no ordinary failure.

use crate::fact::Io;
use crate::process::WorkerStop;
use crate::ty::Site;
use crate::types::{
    Break, CompletedHandle, Error, HandleInner, HandleState, Mooring, Settled, Shell, ThreadEnd,
    Value, WorkerId, claim, end_unclaimed, outcome_value, sig,
};
use std::sync::Arc;

/// `cancel <handle>` -- stop a running concurrent block and everything it started.
pub(crate) fn builtin_cancel(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("cancel")?;
    handle.stop();
    claim(handle, shell);
    Ok(Value::Unit)
}

/// `workers` -- every registered worker, running or retained, as a row of
/// data: never a handle, so the list crosses to a host whole.
pub(crate) fn builtin_workers(shell: &Shell) -> Value {
    Value::list(
        shell
            .workers()
            .iter()
            .map(|e| shell.worker_row(e).ral_value())
            .collect(),
    )
}

/// `worker <id>` -- the handle of any worker this session registered, by the
/// id its birth, its listing and its completion notice carry.  A boundary:
/// the value the worker settles with is admitted at this site when awaited.
pub(crate) fn builtin_worker(
    args: &[Value],
    site: &Arc<Site>,
    _: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let id = match args[0].as_int() {
        Some(n) => u64::try_from(n).map_err(|_| n.to_string()),
        None => Err(args[0].type_name().to_string()),
    }
    .map(WorkerId)
    .map_err(|got| sig(format!("worker: expected a non-negative Int id, got {got}")))?;
    let entry = shell.worker_by_id(id).ok_or_else(|| {
        sig(format!(
            "worker: no worker has id {} in this session; it was never born here, \
             or its result was claimed or has expired (see workers)",
            id.0
        ))
    })?;
    let mut handle = entry.handle;
    handle.site = Some(Arc::clone(site));
    Ok(Value::Handle(Box::new(handle)))
}

/// Admit the value a worker settled with against the site its handle was
/// reacquired at, if it was: observers of one worker at two sites check it
/// at each.
fn admit_handled(handle: &HandleInner, value: &Value, shell: &Shell) -> Settled<()> {
    match &handle.site {
        Some(site) => site
            .admit_handled(value, shell.fixings())
            .map_err(|mismatch| mismatch.refusal("worker", shell)),
        None => Ok(()),
    }
}

/// Replay what the worker wrote, in the awaiter's own sinks: stdout, then
/// stderr.  A broken pipe is the stop it always is.
fn replay(stdout: &[u8], stderr: &[u8], shell: &mut Shell) -> Settled<()> {
    shell.write_stdout(stdout)?;
    shell.write_stderr(stderr)
}

/// The settle a handle has now: a cancelled handle's is the bytes written so
/// far and `worker was cancelled`; a completed one's is its cache; a running
/// one's is `None`.
fn settle_of(handle: &HandleInner) -> Option<CompletedHandle> {
    match handle.state() {
        HandleState::Cancelled => Some(CompletedHandle {
            stdout: handle.stdout_buf.peek(),
            stderr: handle.stderr_buf.peek(),
            end: ThreadEnd::Settled(Err(Error::worker_stopped(WorkerStop::Cancelled).into())),
        }),
        HandleState::Running | HandleState::Completed => handle.try_settle(),
    }
}

/// End as the block ended: its value, admitted at the handle's site, or its
/// failure, exit or stop raised as it stands, or its panic resumed.
fn conclude(end: ThreadEnd, handle: &HandleInner, shell: &Shell) -> Settled<Value> {
    let value = match end {
        ThreadEnd::Settled(settled) => settled?,
        ThreadEnd::Panicked(text) => std::panic::resume_unwind(Box::new(text)),
    };
    admit_handled(handle, &value, shell)?;
    Ok(value)
}

/// Exact await of a handle that has settled: replay, then conclude.
pub(crate) fn take_settled(
    settle: CompletedHandle,
    handle: &HandleInner,
    shell: &mut Shell,
) -> Settled<Value> {
    replay(&settle.stdout, &settle.stderr, shell)?;
    conclude(settle.end, handle, shell)
}

/// One cancel-aware foreground wait, shared by `await` and `race`: sweep until
/// a handle settles, a cancelled one at once.  Between sweeps it polls the
/// shell, so a foreground cancel unwinds the wait, and a loose worker, which
/// does not hang under the awaiter's scope, stays observable later.
fn wait_first_settled<'a>(
    handles: &[&'a HandleInner],
    shell: &Shell,
) -> Settled<(&'a HandleInner, CompletedHandle)> {
    loop {
        if let Some(settled) = handles
            .iter()
            .find_map(|&handle| settle_of(handle).map(|settle| (handle, settle)))
        {
            return Ok(settled);
        }
        shell.poll()?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Block until `handle` settles, then take it exactly.  Claims it.
pub(crate) fn await_handle(handle: &HandleInner, shell: &mut Shell) -> Settled<Value> {
    let (_, settle) = wait_first_settled(&[handle], shell)?;
    claim(handle, shell);
    take_settled(settle, handle, shell)
}

/// `await <handle>` -- wait for a concurrent block and behave as if it had run here.
pub(crate) fn builtin_await(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("await")?;
    await_handle(handle, shell)
}

/// `poll <handle>` -- non-blocking, total sample of a concurrent block:
/// `` `settled `` `{stdout, stderr, elapsed, outcome}` once it has finished
/// (returned, raised, panicked or been cancelled), `` `pending ``
/// `{stdout, stderr, elapsed}` while it runs.
///
/// The pending bytes are a cumulative, non-destructive snapshot
/// ([`crate::io::ByteBuffer::peek`]), so a partial poll never steals bytes the
/// one-shot completion drain must still see.  A watched handle's buffers stay
/// empty, so a poll on one reports no bytes.
///
/// A failure, and the worker's own stop, settle as `` `err ``; an `exit`, a
/// foreign stop and a panic are re-raised, as `await` raises them.
pub(crate) fn builtin_poll(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("poll")?;
    let elapsed = ("elapsed", Value::Float(handle.elapsed().into()));
    let Some(settle) = settle_of(handle) else {
        let io = Io::of(handle.stdout_buf.peek(), handle.stderr_buf.peek());
        return Ok(Value::variant("pending", Some(beside(io, [elapsed]))));
    };
    // A settled poll observes as `await` does, so it claims the handle too.
    claim(handle, shell);
    let outcome = match settle.end {
        ThreadEnd::Settled(Err(Break::Error(e))) => Err(e.record(shell)),
        end => Ok(conclude(end, handle, shell)?),
    };
    let io = Io::of(settle.stdout, settle.stderr);
    let fields = [elapsed, ("outcome", outcome_value(outcome))];
    Ok(Value::variant("settled", Some(beside(io, fields))))
}

/// `race <handles>` -- wait for the first of several blocks to settle, stop
/// the rest and drain them within the grace, then take the winner as `await`
/// does: when `race` returns, nothing it was given still runs on.
pub(crate) fn builtin_race(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let values: Vec<Value> = args[0]
        .as_list("race")?
        .iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
    let handles = values
        .iter()
        .map(|v| v.expect_handle("race"))
        .collect::<Result<Vec<_>, _>>()?;
    if handles.is_empty() {
        return Err(sig("race: no handles to wait for"));
    }

    let first = wait_first_settled(&handles, shell);
    if let Ok((winner, _)) = &first {
        claim(winner, shell);
    }
    // Whatever ends the wait, nothing given to `race` runs on past it.
    let given: Vec<HandleInner> = handles.iter().map(|&h| h.clone()).collect();
    end_unclaimed(&given, shell, mooring.cleanup_grace);
    let (winner, settle) = first?;
    take_settled(settle, winner, shell)
}

/// `io` as a record with more fields, the shape `Ty::extend` gives `poll`'s
/// records: the checker's `poll_variant` is their type.
fn beside<const N: usize>(io: Io, more: [(&str, Value); N]) -> Value {
    let Value::Map(m) = Value::from_datum(io) else {
        unreachable!("an Io encodes as a record");
    };
    let fields = m.iter().map(|(k, v)| (k.to_string(), v.into_owned()));
    let more = more.into_iter().map(|(k, v)| (k.to_string(), v));
    Value::map(fields.chain(more).collect())
}
