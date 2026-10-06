//! A worker's birth: the one door `spawn`, `watch` and `service` share.

use super::eliminate::break_record;
use super::watch_event;
use crate::fact::Worker;
use crate::io::{Sink, new_buffer};
use crate::types::{
    CapReached, DeferredSurface, Done, FlushGuard, HandleInner, Latch, LeaseChain, LeaseClass,
    Mooring, Observed, Settled, Shell, SurfaceBuffer, Value, WorkerEntry, WorkerId, sig,
};
use std::io::Write as _;
use std::sync::{Arc, Mutex};
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
    let surface_buf: SurfaceBuffer = Arc::new(Mutex::new(Vec::new()));
    // Taken from the spawning run's mooring, so the destination outlives that
    // run's teardown; the `joined` latch is shared with the eliminators.
    let worker_surface = Arc::new(DeferredSurface::new(
        surface_buf.clone(),
        mooring.deferred.clone(),
    ));
    let joined = Latch::default();
    let worker_joined = joined.clone();
    let (stdout, stderr) = match birth {
        Birth::Spawn | Birth::Service => (stdout_sink, stderr_sink),
        Birth::Watch { label } => {
            if mooring.deferred.is_none() {
                let _ = shell.io.stderr.write_all(
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
                            deferred.deliver(vec![watch_event(&label, line)]);
                        }
                    }),
                    pending: Vec::new(),
                }
            };
            (watch(label.clone()), watch(format!("{label}:err")))
        }
    };

    let cmd = cmd.to_string();
    let worker_cmd = cmd.clone();

    let worker_mooring = Mooring::for_worker(mooring, &shell.session.root, worker_surface.clone());
    // Minted before the thread so the worker can hold a clone: its exit mark is
    // what ends the lease chain silently on a finished worker.
    let handle = HandleInner {
        stdout_buf,
        stderr_buf,
        surface_buf,
        joined,
        ..HandleInner::new(
            cmd.clone(),
            worker_mooring.cancel.as_scope().clone(),
            Some(rx),
        )
    };
    let worker = handle.clone();
    shell
        .spawn_thread(worker_mooring, "ral spawn worker", move |mooring, child| {
            // A worker's stdout is its handle buffer: nobody is watching it
            // until `await` drains one.
            child.io.stdout = stdout;
            child.io.stderr = stderr;

            let guard = FlushGuard::new(worker_surface, worker_joined, worker_cmd);

            let result = work(mooring, child);
            child.io.stdout.flush_pending();
            child.io.stderr.flush_pending();
            let outcome = match &result {
                Ok(_) => Done::Ok,
                Err(e) => Done::Err(break_record(e, child)),
            };
            guard.settle(outcome);
            let _ = tx.send(result);
            // Strictly *after* the send, so `Completed` always implies an
            // outcome already in the channel.
            worker.complete();
        })
        .map_err(|e| sig(format!("could not start a worker thread: {e}")))?;

    let id = WorkerId::mint();
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
