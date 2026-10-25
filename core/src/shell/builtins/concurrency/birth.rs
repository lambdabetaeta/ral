//! A worker's birth, the one door `spawn` and `watch` share, and the exit
//! rule a gather and a worker's base share: join the brood, or end it.

use super::join;
use crate::fact::Worker;
use crate::io::{Sink, new_buffer};
use crate::process::{DurableRoot, Scope, WorkerStop};
use crate::shell::runtime::{
    Break, BroodOwner, Cradle, DeferredSurface, Done, DoneEvent, EnrolClosed, Error, Escape,
    HandleInner, Mooring, Observed, Owner, ScopeOwner, Settled, Shell, SurfaceSink, ThreadEnd,
    Value, WatchEvent, WorkerEntry, WorkerId, final_poll, sig,
};
use std::sync::Arc;
use std::time::Duration;

/// Which birth [`spawn_child`] is serving: the title and the byte wiring.
/// The node's parent, the owner and the surface are the cradle's: a
/// gather's child is registered, surfaces live and emits no `done`; a
/// session's is announced, buffered, and flushed with one `done`.
pub(super) enum Birth {
    /// `spawn [TITLE] BODY`.
    Spawn { title: Option<String> },
    /// `watch LABEL BODY`: as `Spawn`, its lines streamed live.
    Watch { label: String },
}

/// Spawn a child concurrent block on a new OS thread and return its handle.
///
/// Under a frame with a `worker_cap` the seat is reserved before any thread or
/// entry exists and released only into the `register` below, so a sibling birth
/// racing on another thread never sees a seat mid-fill as free.
pub(super) fn spawn_child<F>(
    mooring: &Mooring,
    shell: &mut Shell,
    birth: Birth,
    work: F,
) -> Settled<HandleInner>
where
    F: FnOnce(&Mooring, &mut Shell) -> Settled<Value> + Send + 'static,
{
    let verb = match birth {
        Birth::Spawn { .. } => "spawn",
        Birth::Watch { .. } => "watch",
    };
    let reservation = shell
        .local
        .workers
        .reserve(mooring.worker_cap)
        .map_err(|err| sig(format!("{verb}: {err}")))?;

    let (stdout_sink, stdout_buf) = new_buffer();
    let (stderr_sink, stderr_buf) = new_buffer();
    let cradle = &shell.local.cradle;
    let brood = cradle.brood.clone();
    let node = cradle.under.child();
    let (title, watched) = match birth {
        Birth::Spawn { title } => (
            title.unwrap_or_else(|| {
                shell
                    .call_site()
                    .map_or_else(|| "block".into(), |site| format!("block at {site}"))
            }),
            None,
        ),
        Birth::Watch { label } => (label.clone(), Some(label)),
    };
    // A gather's child surfaces live, as a stage does.  A session worker's
    // surface is taken from the spawning run's mooring, so the destination
    // outlives that run's teardown.
    let (owner, worker_mooring, worker_surface) = if let Some(brood) = &brood {
        let mooring = Mooring::for_thread(mooring, node.clone(), mooring.surface.clone());
        (brood.owner.into(), mooring, None)
    } else {
        let surface = mooring
            .deferred
            .clone()
            .map(|d| Arc::new(DeferredSurface::new(d)));
        let sink = surface.clone().map(|s| s as SurfaceSink);
        let mooring = Mooring::for_thread(mooring, node.clone(), sink);
        (Owner::Session, mooring, surface)
    };
    let (stdout, stderr) = match watched {
        None => (stdout_sink, stderr_sink),
        Some(label) => {
            if mooring.deferred.is_none() {
                let _ = shell.write_stderr(
                    format!(
                        "note: watch '{label}': this host installs no deferred sink, \
                         so the worker's lines are dropped\n"
                    )
                    .as_bytes(),
                );
            }
            let watch = |label: String| {
                let deferred = mooring.deferred.clone();
                Sink::Watch {
                    line: Arc::new(move |line| {
                        if let Some(deferred) = &deferred {
                            let (label, line) =
                                (label.clone(), String::from_utf8_lossy(line).into_owned());
                            deferred.deliver(vec![WatchEvent { label, line }.to_surface()]);
                        }
                    }),
                    pending: Vec::new(),
                }
            };
            let err = format!("{label}:err");
            (watch(label), watch(err))
        }
    };

    let id = WorkerId::mint();
    let (handle, permit) = HandleInner::new(id, title.as_str(), node, stdout_buf, stderr_buf);
    let receipt = worker_surface.map(|surface| (handle.started, title.clone(), surface));
    shell
        .spawn_thread(worker_mooring, "ral spawn worker", move |mooring, child| {
            // A worker's stdout is its handle buffer, drained as it settles.
            child.io.stdout = stdout;
            child.io.stderr = stderr;

            let end = match ThreadEnd::of(|| {
                // A birth inside this worker hangs under it and is settled by
                // it: I4, I5.  Constructed inside the catch so a body or join
                // panic drops the owner and stops accepted children.
                let owner = ScopeOwner::new(BroodOwner::Worker(id));
                child.local.cradle = Cradle::owning(Scope::clone(&mooring.home), owner.brood());
                let body = work(mooring, child);
                exit_brood(&owner, body, mooring.cleanup_grace, child)
            }) {
                // The base's return clause: the closing poll, then the switch.
                ThreadEnd::Settled(result) => ThreadEnd::Settled(convert_own_stop(
                    final_poll(result, &mooring.home),
                    &child.session.root,
                )),
                panicked @ ThreadEnd::Panicked(_) => panicked,
            };
            let end = flush_worker_stream(end, &mut child.io.stdout);
            let end = flush_worker_stream(end, &mut child.io.stderr);
            let receipt = receipt.map(|(started, title, surface)| {
                let outcome = Done::of(&end, child);
                (started, title, outcome, surface)
            });
            let ended = permit.finish(end);
            if let Some((started, title, outcome, surface)) = receipt {
                let elapsed = ended.mono.saturating_duration_since(started.mono);
                let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                surface.flush(DoneEvent {
                    id,
                    title,
                    elapsed_ms,
                    outcome,
                });
            }
        })
        .map_err(|e| sig(format!("could not start a worker thread: {e}")))?;

    shell.local.workers.register(
        reservation,
        WorkerEntry {
            owner,
            settled_epoch: None,
            handle: handle.clone(),
        },
    );
    match &brood {
        Some(brood) => brood.enrol(handle.clone()).map_err(|EnrolClosed| {
            Break::Error(Error::new(format!(
                "{verb}: the enclosing gather has ended; the new worker was cancelled"
            )))
        })?,
        // After the reservation, never before: a cap-refused spawn observes no
        // birth, so no phantom worker reaches an orphan join.
        None => shell.observe(mooring, Observed::Worker(Worker { id, title })),
    }
    Ok(handle)
}

fn flush_worker_stream(end: ThreadEnd, sink: &mut Sink) -> ThreadEnd {
    let flushed = ThreadEnd::of(|| {
        sink.flush_pending();
        Ok(Value::Unit)
    });
    match (end, flushed) {
        (panic @ ThreadEnd::Panicked(_), _) | (_, panic @ ThreadEnd::Panicked(_)) => panic,
        (end, ThreadEnd::Settled(_)) => end,
    }
}

/// The boundary's switch (I6): a cancel or a timeout raised anywhere inside
/// the session becomes the handle's error, carrying the stop's cause.  A stop
/// the host owns (a root cause, an interrupt) passes as the escape it is.
pub(super) fn convert_own_stop(result: Settled<Value>, root: &DurableRoot) -> Settled<Value> {
    let stop = match &result {
        Err(Break::Escape(Escape::Stopped {
            cause,
            owner: Some(owner),
            ..
        })) if *owner != root.id() => WorkerStop::of(*cause),
        _ => None,
    };
    stop.map_or(result, |stop| Err(Error::worker_stopped(stop).into()))
}

/// The exit rule a worker's base and a `gather` share (I4, I4′): a body
/// that returned joins each unclaimed child in birth order, then waits for
/// every accepted child to reach a terminal state; a body that halted, or a
/// child that failed under the join, stops and drains them all.  The `Err` is
/// the body's, the child's, or the stop that cut the join.
pub(crate) fn exit_brood(
    owner: &ScopeOwner,
    body: Settled<Value>,
    grace: Duration,
    shell: &mut Shell,
) -> Settled<Value> {
    owner.close();
    let result = body.and_then(|value| {
        for child in owner.children() {
            join(&child, shell)?;
        }
        Ok(value)
    });
    if result.is_err() {
        owner.end(grace);
    } else {
        owner.drain(grace);
    }
    result
}
