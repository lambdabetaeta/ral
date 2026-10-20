//! The eliminators `await`, `poll`, `first-settled`, `race` and `cancel`: how
//! a worker's handle is observed, claimed and stopped; `workers`, the
//! listing, which is data; and `worker`, the door back from it to a handle.
//!
//! `await` behaves as if the block had run where it is awaited: the bytes the
//! worker wrote are written to the awaiter's own streams, then it ends as the
//! block ended, with its value, its failure, its exit or its stop.  `race` is
//! `await` on the first to settle, the rest stopped and drained.  A handle's
//! result is claimed once: by `await`, by `race` (its winner and every loser),
//! by `cancel`, or by the handle's owner; a second `await` is a failure, a
//! second `cancel` nothing.  `poll` and `first-settled` are the observations
//! that return data instead: they write nothing and claim nothing.  `poll` is
//! total on an unclaimed handle: a block stopped by its scope settles as
//! `` `err ``, and a handle `cancel` claimed fails.

use crate::fact::Io;
use crate::ty::Site;
use crate::types::{
    Break, CompletedHandle, HandleInner, Mooring, Sample, Settled, Shell, ThreadEnd, Value,
    WorkerId, end_unclaimed, outcome_value, sig,
};
use std::sync::Arc;

/// `cancel <handle>` -- claim a concurrent block and stop it and everything it
/// started; nothing if its result was already claimed.
pub(crate) fn builtin_cancel(args: &[Value]) -> Settled<Value> {
    args[0].expect_handle("cancel")?.claim_and_stop();
    Ok(Value::Unit)
}

/// `workers` -- every registered worker, running or retained, as a row of
/// data: never a handle, so the list crosses to a host whole.
pub(crate) fn builtin_workers(shell: &Shell) -> Value {
    Value::list(
        shell
            .workers()
            .iter()
            .filter_map(|e| shell.worker_row(e).map(|row| row.ral_value()))
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

/// `verb`'s refusal of a handle whose result is gone to another claimer.
fn already_claimed(verb: &str) -> Break {
    sig(format!("{verb}: this worker's result was already claimed"))
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

/// Exact await of a result claimed from `handle`: replay, then conclude.
fn exact(settle: CompletedHandle, handle: &HandleInner, shell: &mut Shell) -> Settled<Value> {
    replay(&settle.stdout, &settle.stderr, shell)?;
    conclude(settle.end, handle, shell)
}

/// One cancel-aware foreground wait, shared by `await`, `race`, a gather's
/// join and `first-settled`: sweep the handles in list order with `sample`
/// until one is ready, polling the shell between sweeps so a foreground
/// cancel unwinds the wait and a loose worker, which does not hang under
/// the waiter's scope, stays to be claimed later: the index of the first
/// ready, or `None` once every handle is claimed by another.
fn first_ready<R>(
    handles: &[HandleInner],
    sample: impl Fn(&HandleInner) -> Sample<R>,
    shell: &Shell,
) -> Settled<Option<(usize, R)>> {
    loop {
        let mut pending = false;
        for (i, handle) in handles.iter().enumerate() {
            match sample(handle) {
                Sample::Settled(r) => return Ok(Some((i, r))),
                Sample::Pending => pending = true,
                Sample::Claimed => {}
            }
        }
        if !pending {
            return Ok(None);
        }
        shell.poll()?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// [`first_ready`], claiming the first settled.
fn claim_first(
    handles: &[HandleInner],
    shell: &Shell,
) -> Settled<Option<(usize, CompletedHandle)>> {
    first_ready(handles, HandleInner::claim_settled, shell)
}

/// Block until `handle` settles, then, if this caller claims it, behave as if
/// its block had run here.  `None` if another claimer has it: the owner's
/// join, which skips such a child.
pub(crate) fn join(handle: &HandleInner, shell: &mut Shell) -> Settled<Option<Value>> {
    claim_first(std::slice::from_ref(handle), shell)?
        .map(|(_, settle)| exact(settle, handle, shell))
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

/// `poll <handle>` -- non-blocking sample of a concurrent block, total on an
/// unclaimed handle: `` `settled `` `{stdout, stderr, elapsed, outcome}` once
/// it has finished (returned, raised or panicked), `` `pending ``
/// `{stdout, stderr, elapsed}` while it runs.  A pure observation: the handle
/// stays to be claimed.
///
/// The pending bytes are a cumulative, non-destructive snapshot
/// ([`crate::io::ByteBuffer::peek`]), so a partial poll never steals bytes the
/// worker's drain as it settles must still see.  A watched handle's buffers
/// stay empty, so a poll on one reports no bytes.
///
/// A failure, and the worker's own stop, settle as `` `err ``; an `exit`, a
/// foreign stop and a panic are re-raised, as `await` raises them.  A handle
/// whose result was claimed has nothing left to observe.
pub(crate) fn builtin_poll(args: &[Value], shell: &Shell) -> Settled<Value> {
    let handle = args[0].expect_handle("poll")?;
    let elapsed = ("elapsed", Value::Float(handle.elapsed().into()));
    // Peeked first: a worker drains its buffers as it settles.
    let (stdout, stderr) = handle.written();
    let settle = match handle.observe(CompletedHandle::clone) {
        Sample::Pending => {
            let io = Io::of(stdout, stderr);
            return Ok(Value::variant("pending", Some(beside(io, [elapsed]))));
        }
        Sample::Settled(settle) => settle,
        Sample::Claimed => return Err(already_claimed("poll")),
    };
    let outcome = match settle.end {
        ThreadEnd::Settled(Err(Break::Error(e))) => Err(e.record(shell)),
        end => Ok(conclude(end, handle, shell)?),
    };
    let io = Io::of(settle.stdout, settle.stderr);
    let fields = [elapsed, ("outcome", outcome_value(outcome))];
    Ok(Value::variant("settled", Some(beside(io, fields))))
}

/// `race <handles>` -- wait for the first of several blocks to settle, claim
/// and stop the rest and drain them within the grace, then claim the winner as
/// `await` does: when `race` returns, nothing it was given still runs on.  A
/// handle claimed by another is skipped; with every one claimed, `race`
/// fails.
pub(crate) fn builtin_race(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let handles = handles_of(&args[0], "race")?;
    let first = claim_first(&handles, shell);
    // Whatever ends the wait, nothing given to `race` runs on past it.
    end_unclaimed(&handles, mooring.cleanup_grace);
    let (winner, settle) = first?.ok_or_else(|| already_claimed("race"))?;
    exact(settle, &handles[winner], shell)
}

/// `first-settled <handles>` -- the 0-based index of the first handle to
/// settle: cancel-aware, writing and claiming nothing, so the caller may
/// `await` the one it names.  A handle another caller claimed is skipped;
/// with every handle claimed, it fails as `await` does.  An empty list is
/// an error.
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
