//! A worker's birth: the one door `spawn`, `watch` and `service` share.

use crate::fact::Worker;
use crate::io::{Sink, new_buffer};
use crate::process::{Scope, WorkerStop};
use crate::types::{
    Break, CapReached, DeferredSurface, Done, Error, Escape, FlushGuard, HandleInner, LeaseChain,
    LeaseClass, Mooring, Observed, Settled, Shell, SurfaceSink, ThreadEnd, Value, WatchEvent,
    WorkerEntry, WorkerId, final_poll, sig,
};
use std::sync::Arc;
use std::time::Instant;

/// Which birth [`spawn_child`] is serving.  One door carries three surface
/// verbs, and everything that varies between them — the verb a refusal must
/// name, the lease class the worker registers under, how its bytes are wired —
/// is read off this rather than passed alongside it.
pub(super) enum Birth {
    Spawn,
    Watch { label: String },
    Service,
}

impl Birth {
    /// The verb the user wrote, for an error that must not name another.
    fn verb(&self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Watch { .. } => "watch",
            Self::Service => "service",
        }
    }

    /// A durable worker escapes the lease chain; the absent chain *is* the
    /// durable policy.
    fn class(&self) -> LeaseClass {
        match self {
            Self::Spawn | Self::Watch { .. } => LeaseClass::Worker,
            Self::Service => LeaseClass::Durable,
        }
    }
}

/// Spawn a child concurrent block on a new OS thread and return its handle.
///
/// Under a frame with a `worker_cap` the seat is reserved before any thread or
/// entry exists and released only into the `register` below, so a sibling birth
/// racing on another thread never sees a seat mid-fill as free.  A
/// [`LeaseClass::Worker`] birth under a frame supplying a [`WorkerLease`] then
/// arms the idle-observation chain ([`LeaseChain::fire`]); a [`LeaseClass::Durable`]
/// one arms nothing — the absent chain *is* the durable policy.
pub(super) fn spawn_child<F>(
    mooring: &Mooring,
    shell: &mut Shell,
    birth: Birth,
    cmd: &str,
    work: F,
) -> Settled<HandleInner>
where
    F: FnOnce(&Mooring, &mut Shell) -> Settled<Value> + Send + 'static,
{
    let class = birth.class();
    let reservation =
        shell
            .local
            .workers
            .reserve(mooring.worker_cap)
            .map_err(|CapReached(cap)| {
                sig(format!(
                    "{}: {cap} workers already live on this agent; await or cancel one",
                    birth.verb()
                ))
            })?;

    let (tx, rx) = std::sync::mpsc::channel();

    let (stdout_sink, stdout_buf) = new_buffer();
    let (stderr_sink, stderr_buf) = new_buffer();
    // Taken from the spawning run's mooring, so the destination outlives that
    // run's teardown.
    let worker_surface = mooring
        .deferred
        .clone()
        .map(|d| Arc::new(DeferredSurface::new(d)));
    let (stdout, stderr) = match birth {
        Birth::Spawn | Birth::Service => (stdout_sink, stderr_sink),
        Birth::Watch { label } => {
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
            (watch(label.clone()), watch(format!("{label}:err")))
        }
    };

    let id = WorkerId::mint();
    let cmd = cmd.to_string();
    let worker_cmd = cmd.clone();

    let node = shell.session.root.worker();
    let worker_mooring = Mooring::for_worker(
        mooring,
        node.clone(),
        worker_surface.clone().map(|s| s as SurfaceSink),
    );
    // Minted before the thread so the worker can hold a clone: its exit mark is
    // what ends the lease chain silently on a finished worker.
    let handle = HandleInner {
        stdout_buf,
        stderr_buf,
        ..HandleInner::new(cmd.clone(), node, Some(rx))
    };
    let worker = handle.clone();
    shell
        .spawn_thread(worker_mooring, "ral spawn worker", move |mooring, child| {
            // A worker's stdout is its handle buffer: nobody is watching it
            // until `await` drains one.
            child.io.stdout = stdout;
            child.io.stderr = stderr;

            let guard = FlushGuard::new(worker_surface, id, worker_cmd);

            let end = match ThreadEnd::of(|| work(mooring, child)) {
                // The base's return clause: the closing poll, then the switch.
                ThreadEnd::Settled(result) => ThreadEnd::Settled(convert_own_stop(
                    final_poll(result, &mooring.home),
                    &mooring.home,
                )),
                panicked @ ThreadEnd::Panicked(_) => panicked,
            };
            child.io.stdout.flush_pending();
            child.io.stderr.flush_pending();
            guard.settle(Done::of(&end, child));
            let _ = tx.send(end);
            // Strictly *after* the send, so `Completed` always implies an
            // outcome already in the channel.
            worker.complete();
        })
        .map_err(|e| sig(format!("could not start a worker thread: {e}")))?;

    shell.local.workers.register(
        reservation,
        WorkerEntry {
            id,
            cmd: cmd.clone(),
            started: std::time::SystemTime::now(),
            class,
            settled_epoch: None,
            handle: handle.clone(),
        },
    );
    // After the reservation, never before: a cap-refused spawn observes no
    // birth, so no phantom worker reaches an orphan join.
    shell.observe(mooring, Observed::Worker(Worker { id, cmd, class }));

    // Armed after `register`, so the id the chain reaps always names an entry
    // that existed.  `keep()`-ed: the worker outlives this call.
    if class == LeaseClass::Worker
        && let Some(lease) = mooring.deferred_lease
    {
        LeaseChain {
            handle: handle.clone(),
            started: Instant::now(),
            lease,
            registry: shell.local.workers.clone(),
            id,
        }
        .fire();
    }
    Ok(handle)
}

/// The boundary's switch: a stop naming this worker's own node, and only
/// that, becomes the handle's error, carrying the stop's cause.  Any other
/// stop passes through as the escape it is.
pub(super) fn convert_own_stop(result: Settled<Value>, own: &Scope) -> Settled<Value> {
    match result {
        Err(Break::Escape(Escape::Stopped {
            cause,
            owner: Some(owner),
            ..
        })) if owner == own.id() => Err(WorkerStop::of(cause).map_or_else(
            || Break::stopped(cause, Some(owner)),
            |o| Error::worker_stopped(o).into(),
        )),
        other => other,
    }
}
