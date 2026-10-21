//! The eliminators and observations `await`, `poll`, `is-done`,
//! `first-settled`, `race` and `cancel`: how a worker's handle is observed,
//! claimed and stopped; and `workers`, the listing, which is data.
//!
//! `await` behaves as if the block had run where it is awaited: the bytes the
//! worker wrote are written to the awaiter's own streams, then it ends as the
//! block ended, with its value, its failure, its exit or its stop.  `race` is
//! `await` on the first ready handle, the rest stopped and drained.  A handle's
//! result is claimed once: by `await`, by `race` (its winner and every loser),
//! by `cancel`, or by the handle's owner; a second `await` is a failure, a
//! second `cancel` nothing.  `poll`, `is-done` and `first-settled` are the
//! observations that return data instead: they write nothing and claim nothing.
//! `poll` reports `` `running ``, `` `stopping ``, `` `ready `` or
//! `` `consumed `` from one snapshot; in `` `ready ``, an ordinary failure or
//! the worker's own stop surfaces as `` `err ``, while an `exit`, a foreign
//! stop and a panic are re-raised.

use crate::fact::Io;
use crate::types::{
    Break, CompletedHandle, HandleInner, Mooring, Sample, Settled, Shell, ThreadEnd, Value,
    outcome_value, sig, stop_and_drain,
};

/// `cancel <handle>` -- claim a concurrent block and stop it and everything it
/// started; nothing if its result was already claimed.
pub(crate) fn builtin_cancel(args: &[Value]) -> Settled<Value> {
    args[0].expect_handle("cancel")?.claim_and_stop();
    Ok(Value::Unit)
}

/// `workers` -- every registered worker, live or retained, as a row of data:
/// never a handle, so the list crosses to a host whole.
pub(crate) fn builtin_workers(shell: &Shell) -> Value {
    Value::list(
        shell
            .workers()
            .iter()
            .filter_map(|e| shell.worker_row(e).map(|row| row.ral_value()))
            .collect(),
    )
}

/// Replay what the worker wrote, in the awaiter's own sinks: stdout, then
/// stderr.  A broken pipe is the stop it always is.
fn replay(stdout: &[u8], stderr: &[u8], shell: &mut Shell) -> Settled<()> {
    shell.write_stdout(stdout)?;
    shell.write_stderr(stderr)
}

/// `verb`'s refusal of a handle whose result is gone to another claimer.
fn already_claimed(verb: &str) -> Break {
    sig(format!("{verb}: this worker's result was already claimed"))
}

/// End as the block ended: its value, or its failure, exit or stop raised as
/// it stands, or its panic resumed.
fn conclude(end: ThreadEnd) -> Settled<Value> {
    match end {
        ThreadEnd::Settled(settled) => settled,
        ThreadEnd::Panicked(text) => std::panic::resume_unwind(Box::new(text)),
    }
}

/// Exact await of a claimed result: replay, then conclude.
fn exact(settle: CompletedHandle, shell: &mut Shell) -> Settled<Value> {
    replay(&settle.stdout, &settle.stderr, shell)?;
    conclude(settle.end)
}

/// One cancel-aware foreground wait, shared by `await`, `race`, a gather's
/// join and `first-settled`: poll the shell first so a pending foreground
/// cancel never claims a ready result, then sweep the handles in list order
/// with `sample` until one is ready: the index of the first ready, or `None`
/// once no handle remains running.
pub(super) fn first_ready<R>(
    handles: &[HandleInner],
    sample: impl Fn(&HandleInner) -> Sample<R>,
    shell: &Shell,
) -> Settled<Option<(usize, R)>> {
    loop {
        shell.poll()?;
        let mut running = false;
        for (i, handle) in handles.iter().enumerate() {
            match sample(handle) {
                Sample::Ready(value) => return Ok(Some((i, value))),
                Sample::Running(()) => running = true,
                Sample::Stopping | Sample::Consumed => {}
            }
        }
        if !running {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// [`first_ready`], claiming the first ready result.
fn claim_first(
    handles: &[HandleInner],
    shell: &Shell,
) -> Settled<Option<(usize, CompletedHandle)>> {
    first_ready(handles, HandleInner::claim_ready, shell)
}

/// Block until `handle` finishes, then, if this caller claims it, behave as if
/// its block had run here.  `None` if another claimer has it: the owner's
/// join, which skips such a child.
pub(crate) fn join(handle: &HandleInner, shell: &mut Shell) -> Settled<Option<Value>> {
    claim_first(std::slice::from_ref(handle), shell)?
        .map(|(_, settle)| exact(settle, shell))
        .transpose()
}

/// [`join`], for a caller to whom another claimer is `verb`'s failure.
pub(crate) fn await_handle(handle: &HandleInner, verb: &str, shell: &mut Shell) -> Settled<Value> {
    join(handle, shell)?.ok_or_else(|| already_claimed(verb))
}

/// `await <handle>` -- wait for a concurrent block, claim it, and behave as if
/// it had run here.
pub(crate) fn builtin_await(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("await")?;
    await_handle(handle, "await", shell)
}

/// `is-done <handle>` -- whether the worker's execution has ended (`ready` or
/// `consumed`), without copying buffers, claiming a result, or re-raising a
/// panic.
pub(crate) fn builtin_is_done(args: &[Value]) -> Settled<Value> {
    let handle = args[0].expect_handle("is-done")?;
    Ok(Value::Bool(handle.sample().has_ended()))
}

/// `poll <handle>` -- non-blocking sample of a concurrent block:
/// `` `running `` `{stdout, stderr, elapsed}` while it runs,
/// `` `stopping `` `{elapsed}` while a cancelled worker finishes teardown,
/// `` `ready `` `{stdout, stderr, elapsed, outcome}` once it has finished with
/// an unclaimed result, or `` `consumed `` `{elapsed}` once its result has been
/// claimed and execution has ended.  A pure observation: a `ready` handle stays
/// to be claimed.
///
/// The running bytes are a cumulative, non-destructive snapshot
/// ([`crate::io::ByteBuffer::peek`]), so a partial poll never steals bytes the
/// worker's drain as it finishes must still see.  A watched handle's buffers
/// stay empty, so a poll on one reports no bytes.
///
/// In `ready`, an ordinary failure or the worker's own stop surfaces as
/// `` `err ``; an `exit`, a foreign stop and a panic are re-raised, as `await`
/// raises them.
pub(crate) fn builtin_poll(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("poll")?;
    let snap = handle.snapshot_io();
    let elapsed = snap.elapsed;
    match snap.state {
        Sample::Running(io) => Ok(Value::variant(
            "running",
            Some(beside(io, [("elapsed", Value::Float(elapsed.into()))])),
        )),
        Sample::Stopping => Ok(Value::variant("stopping", Some(elapsed_record(elapsed)))),
        Sample::Ready(settle) => {
            let outcome = match settle.end {
                ThreadEnd::Settled(Err(Break::Error(e))) => Err(e.record(shell)),
                end => Ok(conclude(end)?),
            };
            let io = Io::of(settle.stdout, settle.stderr);
            let fields = [
                ("elapsed", Value::Float(elapsed.into())),
                ("outcome", outcome_value(outcome)),
            ];
            Ok(Value::variant("ready", Some(beside(io, fields))))
        }
        Sample::Consumed => Ok(Value::variant("consumed", Some(elapsed_record(elapsed)))),
    }
}

/// `race <handles>` -- wait for the first of several blocks to become ready,
/// claim and stop the rest and drain them within the grace, then replay and
/// conclude the winner as `await` does: when `race` returns, nothing it was
/// given still runs on (within the grace).  A handle claimed by another is
/// skipped for the result; with every one claimed, `race` fails.
pub(crate) fn builtin_race(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let handles = handles_of(&args[0], "race")?;
    let first = claim_first(&handles, shell);
    // Whatever ends the wait, nothing given to `race` runs on past it.
    stop_and_drain(&handles, mooring.cleanup_grace);
    let (_, settle) = first?.ok_or_else(|| already_claimed("race"))?;
    exact(settle, shell)
}

/// `first-settled <handles>` -- the 0-based index of the first handle to
/// become ready: cancel-aware, writing and claiming nothing, so the caller may
/// `await` the one it names.  A handle another caller claimed (`stopping` or
/// `consumed`) is skipped; with every handle claimed, it fails as `await`
/// does.  An empty list is an error.
pub(crate) fn builtin_first_settled(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handles = handles_of(&args[0], "first-settled")?;
    let (index, ()) = first_ready(&handles, HandleInner::sample, shell)?
        .ok_or_else(|| already_claimed("first-settled"))?;
    Ok(Value::Int(index.try_into().expect("an index fits an Int")))
}

/// The handles in `verb`'s list argument; an empty list is `verb`'s failure.
fn handles_of(list: &Value, verb: &str) -> Settled<Vec<HandleInner>> {
    let handles = list
        .as_list(verb)?
        .iter()
        .map(|v| v.expect_handle(verb).cloned())
        .collect::<Result<Vec<_>, _>>()?;
    if handles.is_empty() {
        return Err(sig(format!("{verb}: no handles to wait for")));
    }
    Ok(handles)
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

/// The closed `[elapsed: Float]` record `poll` carries in its `stopping` and
/// `consumed` arms.
fn elapsed_record(elapsed: std::time::Duration) -> Value {
    Value::map(vec![("elapsed".to_string(), Value::Float(elapsed.into()))])
}
