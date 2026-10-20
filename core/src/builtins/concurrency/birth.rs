//! A worker's birth: the one door `spawn`, `watch` and the combinators share.

use super::join;
use crate::fact::Worker;
use crate::io::{Sink, new_buffer};
use crate::process::{DurableRoot, Scope, WorkerStop};
use crate::types::{
    Break, Brood, CapReached, Cradle, DeferredSurface, Done, Error, Escape, HandleInner, Mooring,
    Observed, Owner, Settled, Shell, SurfaceSink, ThreadEnd, Value, WatchEvent, WorkerEntry,
    WorkerId, final_poll, sig,
};
use std::sync::Arc;
use std::time::Duration;

/// Which birth [`spawn_child`] is serving.  Everything that varies between
/// them — the title, the node's parent, the owner, how the bytes and the
/// surface are wired — is read off this rather than passed alongside it.
pub(super) enum Birth {
    /// `spawn [TITLE] BODY`: loose, session- or worker-owned, announced,
    /// flushed with a `done`.
    Spawn { title: Option<String> },
    /// `watch LABEL BODY`: as `Spawn`, its lines streamed live.
    Watch { label: String },
    /// A calling expression's child: hangs under `under`, registered for the
    /// cap, not announced, surfaces on the call's live sink, no `done`.
    /// `verb` is the combinator's, for a refusal to name.
    Call {
        under: Scope,
        title: String,
        verb: &'static str,
    },
}

impl Birth {
    /// The verb the user wrote, for an error that must not name another.
    fn verb(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "spawn",
            Self::Watch { .. } => "watch",
            Self::Call { verb, .. } => verb,
        }
    }
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
    let reservation =
        shell
            .local
            .workers
            .reserve(mooring.worker_cap)
            .map_err(|CapReached(cap)| {
                sig(format!(
                    "{}: {cap} workers already live on this agent; `await` or `cancel` one (`workers` lists them)",
                    birth.verb()
                ))
            })?;

    let (stdout_sink, stdout_buf) = new_buffer();
    let (stderr_sink, stderr_buf) = new_buffer();
    let cradle = &shell.local.cradle;
    let (title, node, owner, watched) = match birth {
        Birth::Spawn { title } => (
            title.unwrap_or_else(|| {
                shell
                    .call_site()
                    .map_or_else(|| "block".into(), |site| format!("block at {site}"))
            }),
            cradle.under.child(),
            loose_owner(cradle),
            None,
        ),
        Birth::Watch { label } => (
            label.clone(),
            cradle.under.child(),
            loose_owner(cradle),
            Some(label),
        ),
        Birth::Call { under, title, .. } => (title, under.child(), Owner::Call, None),
    };
    let loose = owner != Owner::Call;
    // Taken from the spawning run's mooring, so the destination outlives that
    // run's teardown.  A call's child surfaces live, as a stage does.
    let worker_surface = mooring
        .deferred
        .clone()
        .filter(|_| loose)
        .map(|d| Arc::new(DeferredSurface::new(d)));
    let worker_mooring = if loose {
        Mooring::for_worker(
            mooring,
            node.clone(),
            worker_surface.clone().map(|s| s as SurfaceSink),
        )
    } else {
        Mooring::for_stage_thread(mooring, node.clone())
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
    let (handle, settler) = HandleInner::new(id, title.as_str(), node, stdout_buf, stderr_buf);
    shell
        .spawn_thread(worker_mooring, "ral spawn worker", move |mooring, child| {
            // A birth inside this worker hangs under it and is settled by it:
            // I4, I5.
            let brood = Brood::new(id);
            child.local.cradle = Cradle::of_worker(&mooring.home, brood.clone());
            // A worker's stdout is its handle buffer, drained as it settles.
            child.io.stdout = stdout;
            child.io.stderr = stderr;
            // Armed in the thread: a thread that never starts flushes nothing.
            let settler = settler.flushing(worker_surface);

            let end = match ThreadEnd::of(|| {
                let settled = work(mooring, child).and_then(|v| settle(&brood, child).map(|()| v));
                if settled.is_err() {
                    brood.end(mooring.cleanup_grace);
                }
                settled
            }) {
                // The base's return clause: the closing poll, then the switch.
                ThreadEnd::Settled(result) => ThreadEnd::Settled(convert_own_stop(
                    final_poll(result, &mooring.home),
                    &child.session.root,
                )),
                panicked @ ThreadEnd::Panicked(_) => {
                    // The panic path: no wait.
                    brood.end(Duration::ZERO);
                    panicked
                }
            };
            child.io.stdout.flush_pending();
            child.io.stderr.flush_pending();
            let done = Done::of(&end, child);
            settler.settle(end, done);
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
    if loose {
        if let Some(brood) = &shell.local.cradle.brood {
            brood.enrol(handle.clone());
        }
        // After the reservation, never before: a cap-refused spawn observes no
        // birth, so no phantom worker reaches an orphan join.
        shell.observe(mooring, Observed::Worker(Worker { id, title }));
    }
    Ok(handle)
}

/// A loose birth's owner: the worker whose cradle it is born in, else the
/// session.
fn loose_owner(cradle: &Cradle) -> Owner {
    cradle
        .brood
        .as_ref()
        .map_or(Owner::Session, |brood| Owner::Worker(brood.worker))
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

/// The return rule (I4): claim each child in birth order and await it
/// exactly, stopping at the first failure.  A child another claimer has is not
/// this worker's to join.  `Err` is that failure or the stop that cut the
/// wait.
fn settle(brood: &Brood, shell: &mut Shell) -> Settled<()> {
    for child in brood.children() {
        join(&child, shell)?;
    }
    Ok(())
}
