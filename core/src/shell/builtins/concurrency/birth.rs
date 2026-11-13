//! A worker's birth, the one door `spawn` and `watch` share, and the exit
//! rule a gather and a worker's base share: join the brood, or end it.

use super::join;
use crate::error::Error;
use crate::fact::{Reason, StateFailure, Worker, WorkerCap};
use crate::handle::HandleInner;
use crate::handle::brood::{BroodGuard, BroodOwner, Cradle, EnrolClosed};
use crate::handle::surface::DeferredSurface;
use crate::io::{Sink, Streams, new_buffer};
use crate::ir::ValClos;
use crate::process::{DurableRoot, Scope};
use crate::shell::runtime::{
    DoneEvent, End, Mooring, Observed, Owner, Settled, Shell, SurfaceSink, WatchEvent, WorkerEntry,
    WorkerId,
};
use std::sync::Arc;
use std::time::Duration;

/// Which birth [`spawn_child`] is serving: the title and the byte wiring.
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
    F: FnOnce(&Mooring, &mut Shell) -> Settled<ValClos> + Send + 'static,
{
    let reservation = shell
        .local
        .workers
        .reserve(mooring.worker_cap)
        .map_err(|err| {
            Error::new(Reason::State(Box::new(StateFailure::WorkerCap(
                WorkerCap {
                    limit: err.0.into(),
                },
            ))))
        })?;

    let Cradle { under, brood } = &shell.local.cradle;
    let node = under.child();
    // A gather's brood enrols the worker, joins or drains it and hears its
    // surface live; the session is told of it and gets its surface buffered,
    // flushed with one `done`, taken from the spawning run's mooring so the
    // destination outlives that run's teardown.
    let gather = brood.clone();
    let (owner, surface, flushed) = if let Some(brood) = &gather {
        (brood.owner.into(), mooring.surface.clone(), None)
    } else {
        let deferred = (mooring.deferred.clone()).map(|d| Arc::new(DeferredSurface::new(d)));
        (
            Owner::Session,
            deferred.clone().map(|s| s as SurfaceSink),
            deferred,
        )
    };
    let worker_mooring = Mooring::for_thread(mooring, node.clone(), surface);
    let (title, watched) = match birth {
        Birth::Spawn { title } => (
            title.unwrap_or_else(|| format!("block at {}", shell.call_site())),
            None,
        ),
        Birth::Watch { label } => (label.clone(), Some(label)),
    };
    let (sinks, buffers) = match watched {
        None => Streams::both(new_buffer).unzip(),
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
            let sinks = Streams {
                stdout: watch(label),
                stderr: watch(err),
            };
            (sinks, Streams::default())
        }
    };

    let id = WorkerId::mint();
    let (handle, permit) = HandleInner::new(id, title.as_str(), node, buffers);
    let receipt = flushed.map(|surface| (handle.started, title.clone(), surface));
    shell.spawn_worker(worker_mooring, sinks, move |mooring, child| {
        let end = child.base(&mooring.home, |child| {
            // A panic drops the owner and stops accepted children (I4, I5).
            let owner = BroodGuard::new(BroodOwner::Worker(id));
            child.local.cradle = Cradle::owning(Scope::clone(&mooring.home), owner.brood());
            let body = work(mooring, child);
            exit_brood(&owner, body, mooring.cleanup_grace, child)
        });
        let end = end.flush_streams(&mut child.io);
        let end = convert_own_stop(end, &child.session.root).leaving(&child.session.sources);
        let receipt = receipt.map(|(started, title, surface)| {
            let outcome = end.done(child);
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
    })?;

    shell.local.workers.register(
        reservation,
        WorkerEntry {
            owner,
            settled_epoch: None,
            handle: handle.clone(),
        },
    );
    match gather {
        Some(brood) => brood.enrol(handle.clone()).map_err(|EnrolClosed| {
            Error::new(Reason::State(Box::new(StateFailure::GatherClosed)))
        })?,
        // After the reservation, never before: a cap-refused spawn observes no
        // birth, so no phantom worker reaches an orphan join.
        None => shell.observe(mooring, Observed::Worker(Worker { id, title })),
    }
    Ok(handle)
}

/// The worker's switch (I6), applied to its base's ending: a cancel or a
/// timeout raised anywhere inside the session becomes the handle's error,
/// carrying the stop's cause and caret.  A stop the host owns (a root cause,
/// an interrupt) passes as the stop it is.
pub(super) fn convert_own_stop(end: End, root: &DurableRoot) -> End {
    match end {
        End::Stopped(stop) if stop.owner.as_ref().is_some_and(|owner| *owner != root.id()) => {
            match Reason::stopped(stop.cause) {
                Some(reason) => {
                    let mut error = Error::new(reason);
                    error.prov = stop.at;
                    End::Err(error)
                }
                None => End::Stopped(stop),
            }
        }
        end => end,
    }
}

/// The exit rule a worker's base and a `gather` share (I4, I4′): a body
/// that returned joins each unclaimed child in birth order, then waits for
/// every accepted child to reach a terminal state; a body that halted, or a
/// child that failed under the join, stops and drains them all.  The `Err` is
/// the body's, the child's, or the stop that cut the join.
pub(crate) fn exit_brood(
    owner: &BroodGuard,
    body: Settled<ValClos>,
    grace: Duration,
    shell: &mut Shell,
) -> Settled<ValClos> {
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
