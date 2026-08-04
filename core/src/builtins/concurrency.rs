//! Concurrency: the births `spawn`, `watch`, `service`, `detach`, and the
//! eliminators `await`, `poll`, `race`, `cancel`.
//!
//! A spawned block runs on its own OS thread with a cloned environment, parked
//! under the durable session root rather than the foreground scope, so a run
//! deadline or interrupt cannot reach it.  Its bytes are buffered per handle
//! (line-framed live, for `watch`) and projected out of one cached
//! [`CompletedHandle`]; its `surface` events are buffered too — the spawning
//! run may be over — and reach a sink exactly once, by an eliminator's replay
//! or by the completion delivery, whichever wins the `joined` latch.

use crate::evaluator::absorb_tail;
use crate::evaluator::comp::{eval_comp, with_scope};
use crate::evaluator::scope::error_record;
use crate::io::{Sink, new_buffer, peek_buffer, take_buffer};
use crate::serial::FOValue;
use crate::types::{
    Break, CapReached, CompletedHandle, DeferredSink, Env, Error, Escape, EventSink, HandleInner,
    HandleState, LeaseClass, Mooring, Raw, ReapCause, Settled, Shell, SurfaceBuffer, Tail, Value,
    WorkerEntry, WorkerId, WorkerLease, WorkerRegistry, sig,
};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use super::util::check_arity;
use super::util::{expect_handle, expect_thunk};
use crate::types::as_list;

/// How a child block's stdout/stderr are wired.
pub(super) enum ChildIoMode {
    Buffered,
    Watch { label: String },
}

/// Cap on a detached worker's deferred surface: past it one `surface-overflow`
/// marker is recorded and further events drop.
const DEFERRED_SURFACE_CAP: usize = 4096;

/// A detached worker's surface: it may outlive the run that spawned it, so it
/// buffers into a bounded [`SurfaceBuffer`] rather than hold that run's live
/// sink.  The buffer leaves by `await`/`race` replay or by [`Self::flush`] to
/// the [`DeferredSink`], whichever wins the handle's `joined` latch.
struct DeferredSurface {
    buf: SurfaceBuffer,
    deferred: Option<Arc<dyn DeferredSink>>,
}

impl EventSink for DeferredSurface {
    fn emit(&self, ev: &FOValue) {
        let mut buf = self.buf.lock().unwrap();
        if buf.len() < DEFERRED_SURFACE_CAP {
            buf.push(ev.clone());
        } else if buf.len() == DEFERRED_SURFACE_CAP {
            buf.push(FOValue::Variant {
                label: "surface-overflow".into(),
                payload: None,
            });
        }
    }
}

/// The event a detached worker appends at completion: a `` `done `` record
/// carrying the handle's `cmd` and an `` `ok ``, `` `err `` (a [`break_record`])
/// or `` `panic `` outcome.  No return value — the model `await`s for that.
fn done_event(cmd: &str, outcome: &Value) -> FOValue {
    // Callers pass only the three statically-built tags, never the block's own
    // return value, so the conversion is provably total.
    let outcome = FOValue::try_from(outcome).expect("spawn outcome tag is statically first-order");
    FOValue::Variant {
        label: "done".into(),
        payload: Some(Box::new(FOValue::Map {
            entries: vec![
                ("cmd".into(), FOValue::String { value: cmd.into() }),
                ("outcome".into(), outcome),
            ],
        })),
    }
}

impl DeferredSurface {
    /// Deliver the buffer plus a final [`done_event`] as one batch, at most once
    /// — the test-and-set lives here, at the sink's sole call site, so no
    /// implementation can forget the discipline.  The batch is a fresh clone,
    /// independent of `complete_handle`'s later `mem::take` of the same buffer.
    fn flush(&self, joined: &Arc<Mutex<bool>>, cmd: &str, outcome: &Value) {
        let Some(deferred) = self.deferred.as_ref() else {
            return;
        };
        let already = std::mem::replace(&mut *joined.lock().unwrap(), true);
        if already {
            return;
        }
        let mut batch = self.buf.lock().unwrap().clone();
        batch.push(done_event(cmd, outcome));
        deferred.deliver(batch);
    }
}

/// Flushes the worker's deferred surface on *every* exit path.  The clean path
/// disarms it through [`Self::settle`]; an unwinding panic leaves it armed, so
/// `drop` flushes a `` `panic `` outcome and the unwind carries on — dropping
/// `tx` unsent, which [`try_settle`] still settles as a panic.
struct FlushGuard {
    surface: Arc<DeferredSurface>,
    joined: Arc<Mutex<bool>>,
    cmd: String,
    armed: bool,
}

impl FlushGuard {
    /// Disarm and flush `outcome`.  The call site runs this before sending the
    /// result, so the boundary's clone predates the eliminators' drain.
    fn settle(mut self, outcome: &Value) {
        self.armed = false;
        self.surface.flush(&self.joined, &self.cmd, outcome);
    }
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        if self.armed {
            let panic = Value::Variant {
                label: "panic".into(),
                payload: Some(Box::new(Value::String("spawned thread panicked".into()))),
            };
            self.surface.flush(&self.joined, &self.cmd, &panic);
        }
    }
}

/// Spawn a child concurrent block on a new OS thread and return its handle.
///
/// `work` returns [`Raw<Value>`] so a terminal tail call surfaces at the
/// worker's trampoline and is absorbed there: a tail call cannot cross a thread
/// boundary, and only a [`Settled`] value may enter the channel.
///
/// Under a frame with a `worker_cap` the seat is reserved before any thread or
/// entry exists and released only into the `register` below, so a sibling birth
/// racing on another thread never sees a seat mid-fill as free.  A
/// [`LeaseClass::Worker`] birth under a frame supplying a [`WorkerLease`] then
/// arms the idle-observation chain ([`lease_fire`]); a [`LeaseClass::Durable`]
/// one arms nothing — the absent chain *is* the durable policy.
pub(super) fn spawn_child<F>(
    snap: Arc<Env>,
    mooring: &Mooring,
    shell: &Shell,
    io_mode: ChildIoMode,
    class: LeaseClass,
    cmd: &str,
    work: F,
) -> Settled<HandleInner>
where
    F: FnOnce(&Mooring, &mut Shell) -> Raw<Value> + Send + 'static,
{
    let reservation = match shell.local.workers.reserve(mooring.worker_cap) {
        Ok(reservation) => reservation,
        Err(CapReached(cap)) => {
            return Err(sig(format!(
                "spawn: {cap} workers already live on this agent; \
                 await or cancel one"
            )));
        }
    };

    let (tx, rx) = std::sync::mpsc::channel();

    let (stdout_sink, stdout_buf) = new_buffer();
    let (stderr_sink, stderr_buf) = new_buffer();
    let surface_buf: SurfaceBuffer = Arc::new(Mutex::new(Vec::new()));
    // Taken from the spawning run's mooring, so the destination outlives that
    // run's teardown; the `joined` latch is shared with the eliminators.
    let worker_surface = Arc::new(DeferredSurface {
        buf: surface_buf.clone(),
        deferred: mooring.deferred.clone(),
    });
    let joined = Arc::new(Mutex::new(false));
    let worker_joined = joined.clone();
    let (stdout, stderr, flush_pending) = match io_mode {
        ChildIoMode::Buffered => (stdout_sink, stderr_sink, false),
        ChildIoMode::Watch { label } => {
            let clone_parent = || {
                shell
                    .io
                    .stdout
                    .try_clone()
                    .map_err(|e| sig(format!("watch: cannot clone parent stdout: {e}")))
            };
            let framed = |inner, prefix| Sink::LineFramed {
                inner: Box::new(inner),
                prefix,
                pending: Vec::new(),
            };
            (
                framed(clone_parent()?, format!("[{label}] ")),
                framed(clone_parent()?, format!("[{label}:err] ")),
                true,
            )
        }
    };

    let cmd = cmd.to_string();
    let worker_cmd = cmd.clone();

    // Minted before the thread so the worker can hold a clone: its exit mark is
    // what ends the lease chain silently on a finished worker.
    let state = Arc::new(Mutex::new(HandleState::Running));
    let worker_state = state.clone();

    let (_join, cancel) = shell.spawn_thread(
        mooring,
        worker_surface.clone(),
        snap,
        move |mooring, child_env| {
            child_env.io.capture_outer = None;
            child_env.io.stdout = stdout;
            child_env.io.stderr = stderr;
            // `spawn_thread` builds the worker from a defaulted `Io`, whose stdin
            // is `Source::Terminal`: without this an external in the body could
            // `tcgetpgrp(stdin)` / `kill(-fg, …)` whoever owns the real terminal.
            child_env.io.stdin = crate::io::Source::Empty;

            let guard = FlushGuard {
                surface: worker_surface,
                joined: worker_joined,
                cmd: worker_cmd,
                armed: true,
            };

            let result = absorb_tail(work(mooring, child_env), mooring, child_env);
            if flush_pending {
                let _ = child_env.io.stdout.flush_pending();
                let _ = child_env.io.stderr.flush_pending();
            }
            let outcome = match &result {
                Ok(_) => Value::Variant {
                    label: "ok".into(),
                    payload: Some(Box::new(Value::Unit)),
                },
                Err(e) => Value::Variant {
                    label: "err".into(),
                    payload: Some(Box::new(break_record(e, child_env))),
                },
            };
            guard.settle(&outcome);
            let _ = tx.send(result);
            // Strictly *after* the send, so `Completed` always implies an
            // outcome already in the channel.  Guarded: an eliminator may have
            // won the transition, and a `cancel`'s `Cancelled` must not be undone.
            let mut settled_state = worker_state.lock().unwrap();
            if *settled_state == HandleState::Running {
                *settled_state = HandleState::Completed;
            }
        },
    );

    let handle = HandleInner {
        result: Arc::new(Mutex::new(Some(rx))),
        cached: Arc::new(Mutex::new(None)),
        state,
        stdout_buf,
        stderr_buf,
        surface_buf,
        joined,
        last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
        cmd: cmd.clone(),
        cancel,
    };
    let id = WorkerId::mint();
    shell.local.workers.register(
        reservation,
        WorkerEntry {
            id,
            cmd,
            started: std::time::SystemTime::now(),
            class,
            settled_epoch: None,
            handle: handle.clone(),
        },
    );

    // Armed after `register`, so the id the chain reaps always names an entry
    // that existed.  `keep()`-ed: the worker outlives this call.
    if class == LeaseClass::Worker
        && let Some(lease) = mooring.deferred_lease
    {
        let chain = LeaseChain {
            scope: handle.cancel.clone(),
            state: handle.state.clone(),
            last_observed: handle.last_observed.clone(),
            started: std::time::Instant::now(),
            lease,
            registry: shell.local.workers.clone(),
            id,
        };
        crate::process::arm_callback(lease.idle, move || lease_fire(&chain)).keep();
    }
    Ok(handle)
}

/// Everything one firing of a worker's lease chain needs, cloned forward into
/// each re-arm.  Shared cells and `Copy` facts, never a `Shell`: cheap to clone
/// and cheap to run on the reaper daemon thread.
#[derive(Clone)]
struct LeaseChain {
    scope: crate::process::CancelScope,
    state: Arc<Mutex<HandleState>>,
    last_observed: Arc<Mutex<std::time::Instant>>,
    /// The backstop's clock; the registry entry's `SystemTime` is display-only.
    started: std::time::Instant,
    lease: WorkerLease,
    registry: WorkerRegistry,
    id: WorkerId,
}

/// One firing of a worker's lease chain, on the reaper daemon thread.
///
/// A worker no longer `Running` ends the chain silently.  A running one is
/// reaped at the backstop (age from spawn), then at the idle bound (since an
/// eliminator last named it), else the chain re-arms for the sooner of the two
/// remaining margins.  A reap does the bookkeeping *before* firing the scope,
/// so the ledger never lags an observable cancellation, and deliberately leaves
/// the handle attached: the body settles as an error, so a later `poll`/`await`
/// still observes the partial output and the failure.
fn lease_fire(chain: &LeaseChain) {
    if *chain.state.lock().unwrap() != HandleState::Running {
        return;
    }
    let age = chain.started.elapsed();
    let idle = chain.last_observed.lock().unwrap().elapsed();
    if age >= chain.lease.backstop {
        chain.registry.reap(chain.id, ReapCause::Backstop);
        chain.scope.cancel(crate::process::CancelCause::Deadline);
    } else if idle >= chain.lease.idle {
        chain.registry.reap(chain.id, ReapCause::Idle);
        chain.scope.cancel(crate::process::CancelCause::Deadline);
    } else {
        let next = std::cmp::min(
            chain.lease.idle.checked_sub(idle).unwrap(),
            chain.lease.backstop.checked_sub(age).unwrap(),
        );
        let rearm = chain.clone();
        crate::process::arm_callback(next, move || lease_fire(&rearm)).keep();
    }
}

// ── spawn ────────────────────────────────────────────────────────────────

/// The worker body handed to [`spawn_child`]: the whole computation of a fresh
/// thread, so it runs at [`Tail::Yes`] and `spawn_child` absorbs its tail call.
fn worker_body(
    body: Arc<crate::ir::Comp>,
) -> impl FnOnce(&Mooring, &mut Shell) -> Raw<Value> + Send + 'static {
    move |mooring, child_env| with_scope(child_env, |s| eval_comp(&body, mooring, s, Tail::Yes))
}

/// `spawn <thunk>` -- spawn a concurrent block on a worker thread, return a handle.
pub(crate) fn builtin_spawn(args: &[Value], mooring: &Mooring, shell: &Shell) -> Settled<Value> {
    let (body, captured) = expect_thunk(&args[0], "spawn")?;
    spawn_buffered(body, captured, mooring, shell)
}

/// Buffered spawn: stdout/stderr accumulate in per-handle
/// buffers and drain to the caller's sinks on `await`.  The worker's own
/// `Shell` is the only one the body touches, so "blocks discard their mobile"
/// falls out of the thread's lifecycle with no boundary ceremony.
fn spawn_buffered(
    body: Arc<crate::ir::Comp>,
    captured: Arc<Env>,
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<Value> {
    Ok(Value::Handle(spawn_child(
        captured,
        mooring,
        shell,
        ChildIoMode::Buffered,
        LeaseClass::Worker,
        "<block>",
        worker_body(body),
    )?))
}

// ── watch ────────────────────────────────────────────────────────────────

/// `watch <label> <thunk>` -- spawn a concurrent block whose output streams
/// live to the caller's stdout, line-framed with the given label.
///
/// A watched worker writes on past the run that spawned it, so only a host with
/// a durable stdout installs [`crate::builtins::WATCH_BUILTIN`]; naming `watch`
/// elsewhere is an unknown-name diagnostic, not a runtime refusal.
pub(super) fn builtin_watch(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let label = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(sig(format!(
                "watch: label must be String, got {}",
                other.type_name()
            )));
        }
    };
    let (body, captured) = expect_thunk(&args[1], "watch")?;
    spawn_labelled(body, captured, label, mooring, shell)
}

/// Line-framed spawn: the child writes through `Sink::LineFramed` over a clone
/// of the caller's stdout, so lines arrive prefixed with no global multiplexer —
/// siblings serialise on the OS stdout lock or the `Sink::External` adapter's
/// mutex.  The byte buffers stay empty, so `await`'s replay drain is a no-op.
fn spawn_labelled(
    body: Arc<crate::ir::Comp>,
    captured: Arc<Env>,
    label: std::string::String,
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<Value> {
    Ok(Value::Handle(spawn_child(
        captured,
        mooring,
        shell,
        ChildIoMode::Watch { label },
        LeaseClass::Worker,
        "<watch>",
        worker_body(body),
    )?))
}

// ── service ──────────────────────────────────────────────────────────────

/// The legibility bound the two births that escape the lease chain carry: a
/// non-empty, single-line description.  For `detach` it is the only thing that
/// later says what a surviving pid was for — there is no handle left to ask.
fn one_line_desc(arg: &Value, verb: &str) -> Settled<String> {
    let Value::String(s) = arg else {
        return Err(sig(format!(
            "{verb}: description must be a String, got {}",
            arg.type_name()
        )));
    };
    let desc = s.trim();
    if desc.is_empty() {
        return Err(sig(format!("{verb}: description must be non-empty")));
    }
    if desc.contains('\n') {
        return Err(sig(format!(
            "{verb}: description must be a single line (no newlines)"
        )));
    }
    Ok(desc.to_string())
}

/// `service <desc> <thunk>` -- birth a durable worker: an ordinary buffered
/// spawn but for its [`LeaseClass::Durable`] registration — no idle reap, no
/// backstop, and `desc` (the registry `cmd`) as the bound standing in for time.
///
/// Host-wise the mirror image of `watch`: only an agent host, whose lease frame
/// would otherwise reap long work, installs
/// [`crate::builtins::SERVICE_BUILTIN`] — grant no lease and a durable class
/// would distinguish nothing.
pub(super) fn builtin_service(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let desc = one_line_desc(&args[0], "service")?;
    let (body, captured) = expect_thunk(&args[1], "service")?;
    Ok(Value::Handle(spawn_child(
        captured,
        mooring,
        shell,
        ChildIoMode::Buffered,
        LeaseClass::Durable,
        &desc,
        worker_body(body),
    )?))
}

// ── detach ───────────────────────────────────────────────────────────────

/// `detach <desc> <cmd> <args…>` -- birth a process this session stops owning,
/// returning a receipt rather than a handle.
///
/// The one concurrency verb whose axis is *ownership*: the others reify a
/// [`Value::Handle`] over work that dies with this process, while a detached
/// program is double-forked away and reparented to init, so no eliminator
/// applies and no teardown here can reach it.  This door is the surface
/// discipline only; resolution, vetting, and the receipt live at the exec
/// boundary (`runtime::command::detach`).  Installed only by a host that arms a
/// detach policy ([`Shell::arm_detach`]).
#[cfg(unix)]
pub(super) fn builtin_detach(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    check_arity(args, 2, "detach")?;
    let desc = one_line_desc(&args[0], "detach")?;
    crate::runtime::command::detach(&desc, &args[1], &args[2..], mooring, shell)
}

/// Stop a handle: the policy `cancel` and `race`'s loser cleanup share.  An
/// already-completed handle keeps its cached outcome, so a finished worker's
/// value is never destroyed by a losing `race` or a `cancel` that lost the toss.
fn stop_handle(handle: &HandleInner) {
    if *handle.state.lock().unwrap() != HandleState::Completed {
        handle.cancel.cancel(crate::process::CancelCause::Explicit);
        detach_handle(handle);
    }
}

/// `cancel <handle>` -- mark a running concurrent block as cancelled.
pub(super) fn builtin_cancel(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let handle = expect_handle(&args[0], "cancel")?;
    stop_handle(handle);
    shell.local.workers.remove(handle);
    shell.mobile.control.last_status = 0;
    Ok(Value::Unit)
}

/// The pre-check `await` and `poll` share: a cancelled handle has no result to
/// wait for or sample, so observing one is an error.
fn ensure_live(handle: &HandleInner, shell: &mut Shell) -> Settled<()> {
    if *handle.state.lock().unwrap() == HandleState::Cancelled {
        shell.mobile.control.last_status = 1;
        return Err(Break::Error(
            Error::new("handle is cancelled", 1)
                .with_hint("use try around await to handle cancellation"),
        ));
    }
    Ok(())
}

/// Non-blocking settle: `Some` if the handle has an outcome to observe now (the
/// cache, or a just-arrived channel message drained into it), `None` while the
/// worker runs.  A `Disconnected` receiver means the worker dropped its `Sender`
/// unsent — it panicked — so it settles as a failure rather than a `None` that
/// `poll` and `race` would read as still-running.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the result guard must span the cached re-check, try_recv, and cache write; releasing early lets a second awaiter observe a bare Disconnected"
)]
fn try_settle(handle: &HandleInner, shell: &mut Shell) -> Option<CompletedHandle> {
    let cached = handle.cached.lock().unwrap().clone();
    if let Some(completed) = cached {
        set_status_from_outcome(&completed.outcome, shell);
        return Some(completed);
    }
    // Settling is once-only: hold `result` across the re-check, the receive,
    // and the cache write, so a second awaiter either sees the first's cached
    // outcome or blocks — never a bare `Disconnected` left by someone's `recv`.
    let mut rx_guard = handle.result.lock().unwrap();
    let cached = handle.cached.lock().unwrap().clone();
    if let Some(completed) = cached {
        set_status_from_outcome(&completed.outcome, shell);
        return Some(completed);
    }
    let rx = rx_guard.as_ref()?;
    let result = match rx.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Disconnected) => Err(sig("await: spawned thread panicked")),
        Err(TryRecvError::Empty) => return None,
    };
    rx_guard.take();
    Some(complete_handle(handle, result, shell))
}

/// Replay a finished detached worker's buffered surface events through the
/// awaiting run's *current* surface, once.  Only `await`/`race` call this, so a
/// polled-but-never-awaited handle emits nothing and a repeat `await` does not
/// duplicate.
fn replay_deferred_surface(handle: &HandleInner, completed: &CompletedHandle, mooring: &Mooring) {
    {
        let mut joined = handle.joined.lock().unwrap();
        if *joined {
            return;
        }
        *joined = true;
    }
    if let Some(sink) = mooring.surface.as_ref() {
        for ev in &completed.surface {
            sink.emit(ev);
        }
    }
}

/// Project a finished block's outcome to the `await`/`race` record, re-raising
/// an `Err` verbatim.  `$status` was already set when the outcome was cached.
fn project_completed(completed: CompletedHandle) -> Settled<Value> {
    let value = completed.outcome?;
    Ok(Value::map(vec![
        ("value".into(), value),
        ("stdout".into(), Value::Bytes(completed.stdout)),
        ("stderr".into(), Value::Bytes(completed.stderr)),
    ]))
}

/// One cancel-aware foreground wait, shared by `await` and `race`: sweep until
/// a handle settles, erroring when none remain live.  Between sweeps it polls
/// `process::check`, so a foreground cancel unwinds the wait — but the workers
/// hang at the durable root, so a cut-short wait leaves them observable later.
fn wait_first_settled<'a>(
    handles: &[&'a HandleInner],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<(&'a HandleInner, CompletedHandle)> {
    loop {
        let mut saw_running = false;
        for &handle in handles {
            // A blocked wait is continuous observation: each sweep renews
            // every named handle's idle lease, so none is reaped mid-wait.
            *handle.last_observed.lock().unwrap() = std::time::Instant::now();
            let state = *handle.state.lock().unwrap();
            match state {
                HandleState::Cancelled => continue,
                HandleState::Running => saw_running = true,
                HandleState::Completed => {}
            }
            if let Some(completed) = try_settle(handle, shell) {
                return Ok((handle, completed));
            }
        }
        if !saw_running {
            return Err(sig("no live handles to wait for"));
        }
        crate::process::check(mooring)?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Block in the foreground until `handle` completes, replay its buffered
/// surface, and return its result record, re-raising a failed block.
pub(super) fn await_handle(
    handle: &HandleInner,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    ensure_live(handle, shell)?;
    let (_, completed) = wait_first_settled(&[handle], mooring, shell)?;
    shell.local.workers.remove(handle);
    replay_deferred_surface(handle, &completed, mooring);
    project_completed(completed)
}

/// `await <handle>` -- wait for a concurrent block to complete and return its result record.
pub(super) fn builtin_await(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let handle = expect_handle(&args[0], "await")?;
    await_handle(handle, mooring, shell)
}

/// `poll <handle>` -- non-blocking, total sample of a concurrent block:
/// `` `settled `` `{stdout, stderr, outcome}` once it has finished (returned,
/// raised, or panicked), `` `pending `` `{stdout, stderr}` while it runs.
///
/// The pending bytes are a cumulative, non-destructive snapshot
/// ([`peek_buffer`], not [`take_buffer`]), so a partial poll never steals bytes
/// the one-shot completion drain must still see — and repeated pending polls
/// are therefore non-idempotent.  A watched handle's
/// buffers stay empty, so a pending poll on one reports nothing.
///
/// Errors only on a cancelled handle ([`ensure_live`]).  A failed block is
/// reported as data, not re-raised, so a successful `poll` leaves `$status` at
/// 0 whatever the block's own status — that lives in `outcome.err.status`.
pub(super) fn builtin_poll(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let handle = expect_handle(&args[0], "poll")?;
    ensure_live(handle, shell)?;
    // Both arms are observations, so the touch lands once at entry, before the
    // settle attempt decides which arm it is.
    *handle.last_observed.lock().unwrap() = std::time::Instant::now();
    let variant = |label: &str, payload| Value::Variant {
        label: label.into(),
        payload,
    };
    let result = if let Some(completed) = try_settle(handle, shell) {
        // A settled poll observes as `await` does, so it removes the entry too.
        shell.local.workers.remove(handle);
        let outcome = match completed.outcome {
            Ok(value) => variant("ok", Some(Box::new(value))),
            Err(e) => variant("err", Some(Box::new(break_record(&e, shell)))),
        };
        let settled = Value::map(vec![
            ("stdout".into(), Value::Bytes(completed.stdout)),
            ("stderr".into(), Value::Bytes(completed.stderr)),
            ("outcome".into(), outcome),
        ]);
        variant("settled", Some(Box::new(settled)))
    } else {
        let pending = Value::map(vec![
            (
                "stdout".into(),
                Value::Bytes(peek_buffer(&handle.stdout_buf)),
            ),
            (
                "stderr".into(),
                Value::Bytes(peek_buffer(&handle.stderr_buf)),
            ),
        ]);
        variant("pending", Some(Box::new(pending)))
    };
    shell.mobile.control.last_status = 0;
    Ok(result)
}

/// `race <handles>` -- wait for the first of several blocks to finish, stopping
/// the rest, then project the winner's outcome as `await` does.
pub(super) fn builtin_race(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    if args.is_empty() {
        return Err(sig("race requires 1 argument (list of handles)"));
    }
    let values = as_list(&args[0], "race")?;
    let mut handles: Vec<&HandleInner> = Vec::new();
    for v in &values {
        handles.push(expect_handle(v, "race")?);
    }

    let (winner, completed) = wait_first_settled(&handles, mooring, shell)?;
    shell.local.workers.remove(winner);
    for &h in &handles {
        if !Arc::ptr_eq(&h.result, &winner.result) {
            stop_handle(h);
            shell.local.workers.remove(h);
        }
    }
    replay_deferred_surface(winner, &completed, mooring);
    project_completed(completed)
}

/// The exit code a settled error carries — an `exit 42` carries 42, not a
/// flattened 1.  Shared by [`set_status_from_outcome`] and [`break_record`].
fn error_exit_code(e: &Break) -> i32 {
    match e {
        Break::Error(e) => e.exit_code(),
        Break::Escape(esc) => escape_exit_code(esc),
    }
}

/// An `Escape`'s exit code: `exit code`'s own, or 1 for a job-control stop.
fn escape_exit_code(esc: &Escape) -> i32 {
    match esc {
        Escape::Exit(code) => *code,
        #[cfg(unix)]
        Escape::Stopped { .. } => 1,
    }
}

/// `poll`'s `` `err `` payload — the same `{cmd, status, message, line, col}`
/// record `try` hands its handler thunk, via [`error_record`].  The position
/// resolves the error's span against `shell`'s registry, and is zero when there
/// is none or when an `Escape` carries no located message.
fn break_record(e: &Break, shell: &Shell) -> Value {
    match e {
        Break::Error(err) => {
            let site = shell.site_of(err.span);
            error_record(
                "<runtime>",
                err.exit_code(),
                &err.message,
                site.line,
                site.col,
            )
        }
        Break::Escape(esc) => {
            let message = match esc {
                Escape::Exit(_) => "block exited".to_string(),
                #[cfg(unix)]
                Escape::Stopped { .. } => "block stopped".to_string(),
            };
            error_record("<runtime>", escape_exit_code(esc), &message, 0, 0)
        }
    }
}

fn set_status_from_outcome(outcome: &Settled<Value>, shell: &mut Shell) {
    shell.mobile.control.last_status = match outcome {
        Ok(_) => 0,
        Err(e) => error_exit_code(e),
    };
}

/// Transition a handle to `Completed`, drain both byte buffers exactly once
/// into a cached [`CompletedHandle`], and set `$status`.  Draining on the error
/// path too captures a failed block's bytes; the cache serves every repeat.
fn complete_handle(
    handle: &HandleInner,
    result: Settled<Value>,
    shell: &mut Shell,
) -> CompletedHandle {
    {
        let mut state = handle.state.lock().unwrap();
        if *state == HandleState::Running {
            *state = HandleState::Completed;
        }
    }
    set_status_from_outcome(&result, shell);
    let completed = CompletedHandle {
        stdout: take_buffer(&handle.stdout_buf),
        stderr: take_buffer(&handle.stderr_buf),
        surface: std::mem::take(&mut *handle.surface_buf.lock().unwrap()),
        outcome: result,
    };
    *handle.cached.lock().unwrap() = Some(completed.clone());
    completed
}

/// Release a handle's receiver and clear its cached result.
fn detach_handle(handle: &HandleInner) {
    *handle.state.lock().unwrap() = HandleState::Cancelled;
    let mut rx_guard = handle.result.lock().unwrap();
    let _ = rx_guard.take();
    drop(rx_guard);
    *handle.cached.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Capabilities, Map};
    use std::sync::mpsc;

    fn status(b: Break) -> i32 {
        match b {
            Break::Error(e) => e.exit_code(),
            other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
        }
    }

    /// A handle whose worker dropped its `Sender` unsent, modelling a panic.
    /// The buffers are pre-seeded so a settled outcome can be checked for them.
    fn handle_with_disconnected_worker(stdout: &[u8], stderr: &[u8]) -> HandleInner {
        let (tx, rx) = mpsc::channel::<Settled<Value>>();
        drop(tx);
        let (_sink, stdout_buf) = new_buffer();
        let (_sink, stderr_buf) = new_buffer();
        stdout_buf.lock().unwrap().extend_from_slice(stdout);
        stderr_buf.lock().unwrap().extend_from_slice(stderr);
        HandleInner {
            result: Arc::new(Mutex::new(Some(rx))),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf,
            stderr_buf,
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "<test>".into(),
            cancel: crate::process::CancelScope::default(),
        }
    }

    fn expect_variant<'a>(v: &'a Value, label: &str) -> &'a Value {
        match v {
            Value::Variant {
                label: l,
                payload: Some(p),
            } if l == label => p,
            other => panic!("expected `{label} with payload, got {other:?}"),
        }
    }

    fn expect_map(v: &Value) -> &Map {
        match v {
            Value::Map(m) => m,
            other => panic!("expected Map, got {other:?}"),
        }
    }

    /// The [`FOValue`] dual of [`expect_variant`].
    fn fo_expect_variant<'a>(v: &'a FOValue, label: &str) -> &'a FOValue {
        match v {
            FOValue::Variant {
                label: l,
                payload: Some(p),
            } if l == label => p,
            other => panic!("expected `{label} with payload, got {other:?}"),
        }
    }

    fn fo_map_get<'a>(v: &'a FOValue, key: &str) -> Option<&'a FOValue> {
        match v {
            FOValue::Map { entries } => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            other => panic!("expected Map, got {other:?}"),
        }
    }

    /// A panicked worker's `Disconnected` receiver must settle as a failure,
    /// not `None` — else `poll` reads `pending` forever and `race` spins.
    #[test]
    fn try_settle_reports_disconnected_worker_as_failed() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let handle = handle_with_disconnected_worker(b"", b"");
        match try_settle(&handle, &mut shell) {
            Some(CompletedHandle {
                outcome: Err(Break::Error(e)),
                ..
            }) => {
                assert_eq!(e.exit_code(), 1);
                assert_eq!(shell.mobile.control.last_status, 1);
            }
            other => panic!("expected Some(failed outcome), got {other:?}"),
        }
    }

    /// `poll` over a panicked worker yields `` `settled `` with the bytes it
    /// buffered before panicking and an `` `err `` outcome, never re-raising.
    #[test]
    fn poll_reports_disconnected_worker_as_settled_err() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let handle = handle_with_disconnected_worker(b"out", b"err");
        let args = [Value::Handle(handle)];
        let poll1 = builtin_poll(&args, &mut shell).expect("poll must not re-raise a panic");
        assert_eq!(shell.mobile.control.last_status, 0, "poll itself succeeded");

        let settled = expect_variant(&poll1, "settled");
        let fields = expect_map(settled);
        assert_eq!(fields.get("stdout"), Some(&Value::Bytes(b"out".to_vec())));
        assert_eq!(fields.get("stderr"), Some(&Value::Bytes(b"err".to_vec())));
        let err = expect_variant(fields.get("outcome").expect("outcome field"), "err");
        let err_fields = expect_map(err);
        assert_eq!(err_fields.get("status"), Some(&Value::Int(1)));

        let poll2 = builtin_poll(&args, &mut shell).expect("repeat poll must not re-raise");
        assert_eq!(poll1, poll2);
    }

    /// A worker polling `process::check` against its own scope, reporting the
    /// status it saw once `cancel_via` fires.  `ready` confirms it is alive
    /// first, so a test pins propagation, not a worker that never ran.
    fn spawn_polling_worker(
        shell: &Shell,
        cancel_via: impl FnOnce(&crate::process::CancelScope),
    ) -> (i32, crate::process::CancelScope) {
        let snap = Arc::new(shell.mobile().scope);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (_join, worker_cancel) = shell.spawn_thread(
            &Mooring::adrift(),
            Arc::new(()),
            snap,
            move |mooring, _child| {
                ready_tx.send(()).unwrap();
                loop {
                    if let Err(b) = crate::process::check(mooring) {
                        done_tx.send(status(b)).unwrap();
                        return;
                    }
                    std::thread::yield_now();
                }
            },
        );
        ready_rx.recv().unwrap();
        cancel_via(&worker_cancel);
        (done_rx.recv().unwrap(), worker_cancel)
    }

    /// Cancelling a worker's own scope stops it and leaves a sibling from the
    /// same shell alone — read off scopes no other test can reach, so no stray
    /// cancellation can satisfy it.
    #[test]
    fn worker_scope_cancel_stops_the_worker() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let (_idle_join, sibling) = shell.spawn_thread(
            &Mooring::adrift(),
            Arc::new(()),
            Arc::new(shell.mobile().scope),
            |_, _| (),
        );
        let (observed, worker_scope) = spawn_polling_worker(&shell, |c| {
            c.cancel(crate::process::CancelCause::Explicit);
        });
        assert!(
            worker_scope.is_cancelled(),
            "the worker's own scope must observe its cancel"
        );
        assert!(
            !sibling.is_cancelled(),
            "cancelling one worker's scope must not cancel a sibling"
        );
        assert_eq!(observed, 130);
    }

    /// A [`RootAbort`](crate::process::CancelCause::RootAbort) reaches the
    /// worker: its scope descends from the root, so its next poll sees the flag.
    #[test]
    fn root_cancel_reaches_the_worker() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let root = shell.session.root.clone();
        let (observed, worker_scope) = spawn_polling_worker(&shell, move |_| {
            root.cancel(crate::process::CancelCause::RootAbort);
        });
        assert!(
            worker_scope.is_cancelled(),
            "a RootAbort on the durable root must cancel the worker's scope"
        );
        assert_eq!(observed, 130);
    }

    /// A foreground cancel spares a detached worker: it parents under the
    /// durable root, so a run timeout cannot reap work meant to outlive the run.
    #[test]
    fn foreground_cancel_spares_detached_worker() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let snap = Arc::new(shell.mobile().scope);
        let m = Mooring::adrift();
        let (_join, worker_scope) = shell.spawn_thread(&m, Arc::new(()), snap, |_, _| ());
        m.cancel.cancel(crate::process::CancelCause::Interrupt);
        assert!(
            !worker_scope.is_cancelled(),
            "a foreground cancel must not reach a detached worker"
        );
    }

    /// A blocked `await` unwinds on a foreground cancel instead of sleeping on a
    /// bare `recv`, yet the root-parented worker it awaited stays awaitable.
    #[test]
    fn await_unwinds_on_foreground_cancel_sparing_the_worker() {
        let mut shell = Shell::new(crate::io::TerminalState::default());

        // A still-running worker, root-parented as a real `spawn`'s is.  Its
        // `Sender` stays alive, so the receiver reports `Empty`, not `Disconnected`.
        let (_tx, rx) = mpsc::channel::<Settled<Value>>();
        let (_sink, stdout_buf) = new_buffer();
        let (_sink2, stderr_buf) = new_buffer();
        let worker_scope = shell.session.root.worker().as_scope().clone();
        let handle = HandleInner {
            result: Arc::new(Mutex::new(Some(rx))),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf,
            stderr_buf,
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "<test>".into(),
            cancel: worker_scope.clone(),
        };

        // Cancel the foreground (run deadline / interrupt), not the root.
        let m = Mooring::adrift();
        m.cancel.cancel(crate::process::CancelCause::Interrupt);

        let err = await_handle(&handle, &m, &mut shell)
            .expect_err("await must unwind on a foreground cancel, not block");
        assert!(matches!(err, Break::Error(_)));
        assert!(
            !worker_scope.is_cancelled(),
            "the foreground cancel must unblock await without reaping the root-parented worker"
        );
    }

    fn lease_ms(idle: u64, backstop: u64) -> WorkerLease {
        WorkerLease {
            idle: std::time::Duration::from_millis(idle),
            backstop: std::time::Duration::from_millis(backstop),
        }
    }

    /// A body that stays `Running` until cancelled, polling `process::check` so
    /// a reap genuinely unwinds the thread rather than flag a scope nobody reads.
    fn check_loop(mooring: &Mooring, _child: &mut Shell) -> Raw<Value> {
        loop {
            crate::process::check(mooring)?;
            std::thread::yield_now();
        }
    }

    /// Block until `handle`'s worker has marked itself `Completed` at exit.
    fn wait_settled(handle: &HandleInner) {
        for _ in 0..500 {
            if *handle.state.lock().unwrap() == HandleState::Completed {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("worker never marked itself Completed");
    }

    /// A `spawn` unobserved for its idle bound: the scope is force-cancelled
    /// with `Deadline`, the entry removed, one `Idle` notice left to drain.
    #[test]
    fn unobserved_worker_is_reaped_at_its_idle_lease() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(40, 10_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<abandoned>",
            check_loop,
        )
        .expect("spawn must succeed");
        let entry = shell.local.workers.snapshot().pop().expect("registered");
        let scope = handle.cancel;

        let mut fired = false;
        for _ in 0..200 {
            if scope.is_cancelled() {
                fired = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            fired,
            "an unobserved worker must be reaped at its idle bound"
        );
        assert_eq!(scope.cause(), Some(crate::process::CancelCause::Deadline));
        assert_eq!(shell.local.workers.count(), 0, "the reap removed the entry");

        let notices = shell.take_worker_reap_notices();
        assert_eq!(notices.len(), 1, "exactly one notice per reap");
        assert_eq!(notices[0].id, entry.id);
        assert_eq!(notices[0].cmd, entry.cmd);
        assert_eq!(notices[0].class, entry.class);
        assert_eq!(notices[0].cause, ReapCause::Idle);
        assert!(
            shell.take_worker_reap_notices().is_empty(),
            "the drain empties the ledger"
        );
    }

    /// A `spawn` under the interactive frame arms no lease: never reaped on a
    /// timer, its settled entry unstamped (the REPL never sweeps), no notices.
    #[test]
    fn spawn_under_interactive_frame_arms_no_lease() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test>",
            |_, _child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        let scope = handle.cancel;

        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(
            !scope.is_cancelled(),
            "the interactive frame must arm no lease"
        );
        let snapshot = shell.local.workers.snapshot();
        assert_eq!(
            snapshot.len(),
            1,
            "the entry stays listed: the REPL never reaps"
        );
        assert_eq!(
            snapshot[0].settled_epoch, None,
            "no epoch sweep ever runs on a policy-free host"
        );
        assert!(
            shell.take_worker_reap_notices().is_empty(),
            "no policy, no notices"
        );
    }

    struct EchoDesk;
    impl crate::types::EnquiryDesk for EchoDesk {
        fn enquire(
            &self,
            req: crate::serial::FOValue,
            _cancel: &crate::process::CancelScope,
        ) -> Result<crate::serial::FOValue, crate::types::Error> {
            Ok(req)
        }
    }

    /// Containment: a detached worker's own `enquire` answers the absence
    /// error rather than reach the run's desk.
    #[test]
    fn spawned_worker_never_receives_the_enquiry_desk() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.desk = Some(Arc::new(EchoDesk) as crate::types::Desk);
        let snap = Arc::new(shell.mobile().scope);
        let (tx, rx) = mpsc::channel::<Result<crate::serial::FOValue, crate::types::Error>>();
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test>",
            move |mooring, child| {
                let outcome = child.enquire(mooring, crate::serial::FOValue::Unit);
                let _ = tx.send(outcome);
                Ok(Value::Unit)
            },
        )
        .expect("spawn must succeed");
        wait_settled(&handle);
        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker must send its enquire outcome before settling");
        match outcome {
            Err(e) => assert_eq!(e.message, crate::types::NO_DESK),
            Ok(_) => panic!("a detached worker must never reach the spawning run's desk"),
        }
    }

    /// The nursery's twin of `spawned_worker_never_receives_the_enquiry_desk`:
    /// `fork_into_nursery` answers the absence error rather than reach one.
    #[test]
    fn spawned_worker_never_receives_the_nursery() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.nursery = Some(crate::types::Nursery::default());
        let snap = Arc::new(shell.mobile().scope);
        let (tx, rx) = mpsc::channel::<crate::types::Settled<crate::types::NurseryId>>();
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test>",
            move |mooring, child| {
                let outcome = child.fork_into_nursery(mooring);
                let _ = tx.send(outcome);
                Ok(Value::Unit)
            },
        )
        .expect("spawn must succeed");
        wait_settled(&handle);
        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker must send its fork_into_nursery outcome before settling");
        match outcome {
            Err(Break::Error(e)) => assert_eq!(e.message, "this host adopts no forked sessions"),
            Err(other) => panic!("expected Break::Error, got {other:?}"),
            Ok(_) => panic!("a detached worker must never reach the spawning run's nursery"),
        }
    }

    /// Observation renews the idle lease: a worker polled every ~20 ms under a
    /// 200 ms bound survives to ~3× it, then finishes and awaits normally.
    #[test]
    fn polled_worker_survives_past_its_idle_lease() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(200, 10_000));
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<babysat>",
            move |_, _c| {
                gate_rx.recv().unwrap();
                Ok(Value::Unit)
            },
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        while std::time::Instant::now() < deadline {
            builtin_poll(&[Value::Handle(handle.clone())], &mut shell).expect("poll a live handle");
            assert!(
                !scope.is_cancelled(),
                "a polled worker must never be idle-reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(shell.local.workers.count(), 1, "the babysat entry stays");

        gate_tx.send(()).unwrap();
        await_handle(&handle, &m, &mut shell).expect("await after the gate opens");
        assert!(!scope.is_cancelled(), "the worker finished by itself");
    }

    /// The backstop is absolute: ritual polling renews the idle bound but
    /// cannot carry a worker past `backstop`, and the cause says so.
    #[test]
    fn backstop_reaps_a_ritually_polled_worker() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(150, 400));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<immortal>",
            check_loop,
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

        let budget = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !scope.is_cancelled() {
            assert!(
                std::time::Instant::now() < budget,
                "the backstop must fire within the budget"
            );
            // A poll may race the reap and see the cancelled body settle as
            // an error; only the touch matters here.
            let _ = builtin_poll(&[Value::Handle(handle.clone())], &mut shell);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(scope.cause(), Some(crate::process::CancelCause::Deadline));
        assert_eq!(shell.local.workers.count(), 0);
        let notices = shell.take_worker_reap_notices();
        assert_eq!(notices.len(), 1, "one notice for the backstop reap");
        assert_eq!(notices[0].cause, ReapCause::Backstop);
    }

    /// A worker that completed but was never observed is not reaped: its exit
    /// mark ends the chain, so its entry lingers as an unclaimed result.
    #[test]
    fn completed_unobserved_worker_is_not_reaped() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(100, 10_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<done>",
            |_, _child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

        // Wait for the exit mark (it rides the worker thread), then let ~3 idle
        // bounds elapse so the chain has demonstrably fired and ended.
        let mut completed = false;
        for _ in 0..200 {
            if *handle.state.lock().unwrap() == HandleState::Completed {
                completed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(completed, "the worker marks itself Completed at exit");
        std::thread::sleep(std::time::Duration::from_millis(300));

        assert!(
            !scope.is_cancelled(),
            "a settled worker is never lease-cancelled"
        );
        assert_eq!(shell.local.workers.count(), 1, "the settled entry lingers");
        assert!(
            shell.take_worker_reap_notices().is_empty(),
            "no notice: nothing was reaped"
        );
    }

    /// Enumeration is not observation: `workers()` and `worker_count()` touch
    /// nothing, so a listed-but-unpolled worker is reaped anyway.
    #[test]
    fn listing_does_not_renew_the_lease() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(40, 10_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<listed>",
            check_loop,
        )
        .expect("spawn must succeed");
        let scope = handle.cancel;

        let budget = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !scope.is_cancelled() {
            assert!(
                std::time::Instant::now() < budget,
                "listing must not keep the worker alive"
            );
            let _ = shell.workers();
            let _ = shell.worker_count();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(scope.cause(), Some(crate::process::CancelCause::Deadline));
        assert_eq!(
            shell.take_worker_reap_notices().len(),
            1,
            "reaped despite the listing ritual"
        );
    }

    /// The durable class is the whole difference: under one lease frame a
    /// `Durable` birth outlives both bounds while its sibling is reaped.
    #[test]
    fn durable_worker_outlives_both_lease_bounds_while_its_sibling_reaps() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(40, 150));

        let snap = Arc::new(shell.mobile().scope);
        let durable = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Durable,
            "<service>",
            check_loop,
        )
        .expect("durable spawn must succeed");
        let born = std::time::Instant::now();
        let snap = Arc::new(shell.mobile().scope);
        let sibling = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<sibling>",
            check_loop,
        )
        .expect("ordinary spawn must succeed");

        // The ordinary sibling proves the frame's lease is genuinely armed.
        let budget = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !sibling.cancel.is_cancelled() {
            assert!(
                std::time::Instant::now() < budget,
                "the ordinary sibling must be reaped under this frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let past_both = born + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < past_both {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            !durable.cancel.is_cancelled(),
            "a durable worker is never lease-cancelled: no idle bound, no backstop"
        );
        let entries = shell.workers();
        assert_eq!(entries.len(), 1, "only the durable entry remains listed");
        assert_eq!(entries[0].class, LeaseClass::Durable);
        assert_eq!(entries[0].cmd, "<service>");
        let notices = shell.take_worker_reap_notices();
        assert_eq!(notices.len(), 1, "one notice: the sibling's reap alone");
        assert_eq!(notices[0].cmd, "<sibling>");

        // End the blocked worker so the test leaks no live thread.
        durable.cancel.cancel(crate::process::CancelCause::Explicit);
    }

    /// `cancel` still fires a durable worker's scope and removes its entry:
    /// durability exempts the lease chain, not the eliminators.
    #[test]
    fn cancel_through_the_handle_ends_a_durable_worker() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.deferred_lease = Some(lease_ms(10_000, 20_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Durable,
            "<service>",
            check_loop,
        )
        .expect("durable spawn must succeed");

        builtin_cancel(&[Value::Handle(handle.clone())], &mut shell)
            .expect("cancel must succeed on a durable worker");

        assert!(
            handle.cancel.is_cancelled(),
            "cancel fires the durable worker's scope"
        );
        assert_eq!(
            handle.cancel.cause(),
            Some(crate::process::CancelCause::Explicit)
        );
        assert_eq!(
            shell.local.workers.count(),
            0,
            "cancel removes the durable entry"
        );
        assert!(
            shell.take_worker_reap_notices().is_empty(),
            "an explicit cancel is not a reap: no notice"
        );
    }

    // ── `service`'s mandatory description ────────────────────────────────

    /// Run `src` as one capturing top-level run on a shell the caller dressed.
    /// Panics on a static failure — every source here is expected to compile.
    fn run_source(shell: &mut Shell, src: &str) -> Settled<Value> {
        use crate::transport::{Program, Run};
        use crate::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};
        let req = RunRequest {
            run: Run {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        };
        match shell.run(req) {
            RunReport::Ran { result, .. } => result,
            RunReport::Static { .. } => {
                panic!("well-formed source must run, not fail statically: {src:?}")
            }
        }
    }

    fn service_test_shell() -> Shell {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(crate::builtins::SERVICE_BUILTIN);
        shell
    }

    /// An empty (or whitespace-only) description is refused: it is the whole
    /// legibility bound a durable birth declares, so it cannot be absent.
    #[test]
    fn service_rejects_an_empty_description() {
        let mut shell = service_test_shell();
        let err = run_source(&mut shell, r#"service "   " { 1 }"#)
            .expect_err("an empty description must be refused");
        assert_eq!(status(err), 1);
    }

    /// A multi-line description is refused: a ledger label, not a paragraph.
    #[test]
    fn service_rejects_a_multiline_description() {
        let mut shell = service_test_shell();
        let err = run_source(&mut shell, "service \"one\ntwo\" { 1 }")
            .expect_err("a multiline description must be refused");
        assert_eq!(status(err), 1);
    }

    /// A valid description lands trimmed as the registry entry's `cmd`.
    #[test]
    fn service_description_lands_in_the_registry_entry() {
        let mut shell = service_test_shell();
        let handle = match run_source(&mut shell, r#"service "  watch the thing  " { 1 }"#) {
            Ok(Value::Handle(h)) => h,
            other => panic!("service must return a Handle, got {other:?}"),
        };
        let entries = shell.workers();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].class, LeaseClass::Durable);
        assert_eq!(entries[0].cmd, "watch the thing", "the description trims");
        handle.cancel.cancel(crate::process::CancelCause::Explicit);
    }

    // ── `detach` ─────────────────────────────────────────────────────────

    #[cfg(unix)]
    fn detach_test_shell(budget: u64) -> Shell {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(crate::builtins::DETACH_BUILTIN);
        shell.arm_detach(budget);
        shell
    }

    /// Neither description nor program is optional: a shorter call never
    /// reaches the exec boundary.
    #[cfg(unix)]
    #[test]
    fn detach_requires_a_description_and_a_command() {
        let mut shell = detach_test_shell(4);
        let err = run_source(&mut shell, r#"detach "a server""#)
            .expect_err("a description alone is not a detach");
        assert_eq!(status(err), 1);
    }

    /// The same legibility bound `service` carries, refused before a birth.
    #[cfg(unix)]
    #[test]
    fn detach_rejects_an_illegible_description() {
        let mut shell = detach_test_shell(4);
        let empty = run_source(&mut shell, r#"detach "   " /bin/echo hi"#)
            .expect_err("an empty description must be refused");
        assert_eq!(status(empty), 1);
        let multiline = run_source(&mut shell, "detach \"one\ntwo\" /bin/echo hi")
            .expect_err("a multiline description must be refused");
        assert_eq!(status(multiline), 1);
    }

    /// A head a handler intercepts runs that handler, per name and by catch-all
    /// alike, and its value is the `detach`'s.  The zero budget adds that an
    /// intercepted call spends no birth.
    #[cfg(unix)]
    #[test]
    fn detach_runs_a_handler_that_intercepts_its_head() {
        let mut shell = detach_test_shell(0);
        let by_name = run_source(
            &mut shell,
            r#"within [handlers: [my-server: { |args| $args }]] { detach "a server" my-server up now }"#,
        )
        .expect("a per-name handler runs in place of the birth");
        assert_eq!(
            by_name,
            Value::list(vec![
                Value::String("up".into()),
                Value::String("now".into())
            ]),
            "the handler receives the argv after the head, as an ordinary call would"
        );

        let catch_all = run_source(
            &mut shell,
            r#"within [handler: { |n _a| $n }] { detach "a server" my-server }"#,
        )
        .expect("a catch-all handler intercepts the head too");
        assert_eq!(catch_all, Value::String("my-server".into()));
    }

    /// A base frame's name runs the frame in place of a birth: `cd` moves
    /// the shell's cwd instead of naming a process image.
    #[cfg(unix)]
    #[test]
    fn detach_reaches_cds_base_frame_instead_of_spawning_it() {
        let mut shell = detach_test_shell(0);
        run_source(&mut shell, r#"detach "a server" cd /tmp"#)
            .expect("a base frame's name runs the frame, not a birth");
        assert_eq!(shell.cwd().to_string_lossy(), "/tmp");
    }

    /// Vetting is reused wholesale, so an unresolvable head gives the usual 127.
    #[cfg(unix)]
    #[test]
    fn detach_reports_an_unknown_command_as_127() {
        let mut shell = detach_test_shell(4);
        let err = run_source(
            &mut shell,
            r#"detach "a server" definitely-not-a-real-tool-xyz"#,
        )
        .expect_err("an unknown head must not be born");
        assert_eq!(status(err), 127);
    }

    /// The budget is a hard bound, and its refusal claims no remedy inside ral.
    #[cfg(unix)]
    #[test]
    fn detach_refuses_past_its_budget() {
        let mut shell = detach_test_shell(0);
        let err = run_source(&mut shell, r#"detach "a server" /bin/echo hi"#)
            .expect_err("a spent budget must refuse");
        assert!(format!("{err:?}").contains("budget"));
    }

    /// The whole of what a birth hands back: the caller's description and the
    /// kernel's pid.  Nothing else — three `/dev/null` streams, no file anywhere.
    #[cfg(unix)]
    #[test]
    fn detach_returns_a_receipt_of_a_pid_and_a_desc() {
        let mut shell = detach_test_shell(4);
        let born = run_source(&mut shell, r#"detach "the greeter" /bin/echo hello"#)
            .expect("the birth must succeed");
        let fields = expect_map(&born);
        assert_eq!(
            fields.len(),
            2,
            "the receipt is a pid and a desc, and nothing else: {fields:?}"
        );
        assert_eq!(
            fields.get("desc"),
            Some(&Value::String("the greeter".into()))
        );
        assert!(matches!(fields.get("pid"), Some(Value::Int(p)) if *p > 0));
        assert_eq!(shell.mobile.control.last_status, 0);
    }

    /// A detached worker's `surface` events replay through the *awaiting* run
    /// exactly once: `poll` never, the first `await` yes, a second no.
    #[test]
    fn deferred_surface_replays_once_on_await_not_poll() {
        struct Rec(Arc<Mutex<Vec<FOValue>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.surface = Some(Arc::new(Rec(log.clone())));

        // A settled handle carrying one buffered event; the sender stays alive,
        // so the receiver sees a value rather than a disconnect.
        let (tx, rx) = mpsc::channel::<Settled<Value>>();
        tx.send(Ok(Value::Unit)).unwrap();
        let (_s, stdout_buf) = new_buffer();
        let (_s2, stderr_buf) = new_buffer();
        let surface_buf: SurfaceBuffer = Arc::new(Mutex::new(vec![FOValue::Variant {
            label: "patch".into(),
            payload: None,
        }]));
        let handle = HandleInner {
            result: Arc::new(Mutex::new(Some(rx))),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf,
            stderr_buf,
            surface_buf,
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "<test>".into(),
            cancel: crate::process::CancelScope::default(),
        };

        builtin_poll(&[Value::Handle(handle.clone())], &mut shell).expect("poll ok");
        assert_eq!(log.lock().unwrap().len(), 0, "poll must not replay surface");

        await_handle(&handle, &m, &mut shell).expect("await ok");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "await replays the deferred card"
        );

        await_handle(&handle, &m, &mut shell).expect("await ok");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "repeat await must not duplicate"
        );
    }

    /// A deferred-sink double for the agent host.  The deliver-once test-and-set
    /// lives at `DeferredSurface::flush`, so this just records what it is handed.
    struct RecDeferred(Arc<Mutex<Vec<Vec<FOValue>>>>);

    impl DeferredSink for RecDeferred {
        fn deliver(&self, batch: Vec<FOValue>) {
            self.0.lock().unwrap().push(batch);
        }
    }

    /// Spin until the deferred sink records a batch: a worker flushes on its own
    /// thread, so completion is observed there, not on the result channel.
    fn wait_for_batch(batches: &Arc<Mutex<Vec<Vec<FOValue>>>>) -> Vec<Vec<FOValue>> {
        for _ in 0..500 {
            {
                let got = batches.lock().unwrap();
                if !got.is_empty() {
                    return got.clone();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("worker never flushed its batch to the deferred sink");
    }

    /// The `outcome` label inside a batch's trailing `` `done ``.  Pins the shape
    /// the exarch decoder matches: a `{cmd, outcome}` map over a closed variant.
    fn done_outcome_label(done: &FOValue) -> String {
        let done = fo_expect_variant(done, "done");
        match fo_map_get(done, "outcome").expect("outcome field") {
            FOValue::Variant { label, .. } => label.clone(),
            other => panic!("outcome must be a variant, got {other:?}"),
        }
    }

    /// With a deferred sink installed, a completed worker flushes its buffer plus
    /// a trailing `` `done `` as one batch — a sink reached with no eliminator at
    /// all — and each of the three outcomes stamps its own label.
    #[test]
    fn detached_worker_flushes_done_to_deferred_sink() {
        fn run(
            work: impl FnOnce(&Mooring, &mut Shell) -> Raw<Value> + Send + 'static,
        ) -> Vec<FOValue> {
            let shell = Shell::new(crate::io::TerminalState::default());
            let batches = Arc::new(Mutex::new(Vec::new()));
            let mut m = Mooring::adrift();
            m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
            let snap = Arc::new(shell.mobile().scope);
            // Hold the handle so the channel stays connected until the flush;
            // never observed, so no eliminator competes for the `joined` latch.
            let _handle = spawn_child(
                snap,
                &m,
                &shell,
                ChildIoMode::Buffered,
                LeaseClass::Worker,
                "<block>",
                work,
            )
            .unwrap();
            let mut got = wait_for_batch(&batches);
            assert_eq!(got.len(), 1, "one batch per completed worker");
            got.pop().unwrap()
        }

        let ok = run(|_, _child| Ok(Value::Unit));
        assert_eq!(ok.len(), 1, "an empty body's batch is just the `done event");
        let done = &ok[0];
        assert_eq!(done_outcome_label(done), "ok");
        let fields = fo_expect_variant(done, "done");
        assert_eq!(
            fo_map_get(fields, "cmd"),
            Some(&FOValue::String {
                value: "<block>".into()
            })
        );

        let err = run(|_, _child| Err(sig("boom").into()));
        assert_eq!(done_outcome_label(&err[0]), "err");

        let panicked = run(|_, _child| panic!("worker exploded"));
        assert_eq!(done_outcome_label(&panicked[0]), "panic");
    }

    /// The body's own events precede the trailing `` `done ``: the batch carries
    /// the whole surface.  A panicking worker still settles through `Disconnected`.
    #[test]
    fn deferred_batch_carries_body_surface_before_done() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let snap = Arc::new(shell.mobile().scope);

        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<block>",
            |mooring, _child| {
                if let Some(sink) = mooring.surface.as_ref() {
                    sink.emit(&FOValue::Variant {
                        label: "card".into(),
                        payload: None,
                    });
                }
                panic!("after surfacing");
            },
        )
        .unwrap();

        let batch = wait_for_batch(&batches).pop().unwrap();
        assert_eq!(batch.len(), 2, "the body's card, then the `done event");
        assert_eq!(
            batch[0],
            FOValue::Variant {
                label: "card".into(),
                payload: None,
            },
            "the body's surface precedes the `done"
        );
        assert_eq!(done_outcome_label(&batch[1]), "panic");

        match try_settle(&handle, &mut shell) {
            Some(CompletedHandle {
                outcome: Err(Break::Error(_)),
                ..
            }) => {}
            other => panic!("expected a settled panic outcome, got {other:?}"),
        }
    }

    /// With no deferred sink installed (the bare REPL) a completed worker
    /// flushes nothing, and its surface reaches a sink only via `await`/`race`.
    #[test]
    fn no_deferred_sink_means_no_delivery() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        assert!(m.deferred.is_none(), "a bare REPL installs none");
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<block>",
            |mooring, _child| {
                if let Some(sink) = mooring.surface.as_ref() {
                    sink.emit(&FOValue::Variant {
                        label: "card".into(),
                        payload: None,
                    });
                }
                Ok(Value::Unit)
            },
        )
        .unwrap();

        // Nothing was delivered, so the `joined` latch is unset and the `await`
        // replay still surfaces the body's card.
        let log = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<FOValue>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        m.surface = Some(Arc::new(Rec(log.clone())));
        await_handle(&handle, &m, &mut shell).expect("await ok");
        let replayed = log.lock().unwrap().clone();
        assert_eq!(
            replayed.as_slice(),
            &[FOValue::Variant {
                label: "card".into(),
                payload: None,
            }],
            "no deferred sink appends no `done; await replays only the body's card"
        );
    }

    /// Deliver-once across the two regimes: once the deferred sink has won the
    /// `joined` latch, a later `await` replays nothing yet still returns its record.
    #[test]
    fn deferred_delivery_suppresses_a_later_await_replay() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<block>",
            |mooring, _child| {
                if let Some(sink) = mooring.surface.as_ref() {
                    sink.emit(&FOValue::Variant {
                        label: "card".into(),
                        payload: None,
                    });
                }
                Ok(Value::Unit)
            },
        )
        .unwrap();

        // The deferred sink wins the latch first, recording one batch.
        wait_for_batch(&batches);
        assert!(
            *handle.joined.lock().unwrap(),
            "the deferred flush set `joined"
        );

        // A later `await` finds the latch set, so it replays nothing.
        let log = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<FOValue>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        m.surface = Some(Arc::new(Rec(log.clone())));
        await_handle(&handle, &m, &mut shell).expect("await still returns the result record");
        assert_eq!(
            log.lock().unwrap().len(),
            0,
            "the deferred sink already delivered, so the replay is suppressed"
        );
    }

    // ── worker registry (pure bookkeeping, no policy) ────────────────────

    /// `spawn_child` files exactly one entry with the spawn's own `cmd` and class,
    /// and the registered handle is the returned one — by `Arc::ptr_eq`.
    #[test]
    fn spawn_child_registers_one_entry_with_matching_handle() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test-cmd>",
            |_, _child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");

        assert_eq!(
            shell.local.workers.count(),
            1,
            "spawn_child registers exactly one entry"
        );
        let snapshot = shell.local.workers.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].cmd, "<test-cmd>");
        assert_eq!(snapshot[0].class, LeaseClass::Worker);
        assert_eq!(
            snapshot[0].handle, handle,
            "the registered handle is the returned handle"
        );
    }

    /// Every eliminator that observes a settled worker removes its entry, while a
    /// `` `pending `` `poll` leaves it: only observation may mutate the registry.
    #[test]
    fn eliminators_remove_the_entry_except_a_pending_poll() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();

        // `await` removes.
        let snap = Arc::new(shell.mobile().scope);
        let h1 = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<a>",
            |_, _c| Ok(Value::Unit),
        )
        .unwrap();
        await_handle(&h1, &m, &mut shell).expect("await ok");
        assert_eq!(shell.local.workers.count(), 0, "await removes its entry");

        // `cancel` removes.
        let snap = Arc::new(shell.mobile().scope);
        let h2 = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<b>",
            |_, _c| Ok(Value::Unit),
        )
        .unwrap();
        assert_eq!(shell.local.workers.count(), 1);
        builtin_cancel(&[Value::Handle(h2)], &mut shell).expect("cancel ok");
        assert_eq!(shell.local.workers.count(), 0, "cancel removes its entry");

        // A settled `poll` removes.
        let snap = Arc::new(shell.mobile().scope);
        let h3 = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<c>",
            |_, _c| Ok(Value::Unit),
        )
        .unwrap();
        loop {
            let polled = builtin_poll(&[Value::Handle(h3.clone())], &mut shell).unwrap();
            if matches!(&polled, Value::Variant { label, .. } if label == "settled") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            shell.local.workers.count(),
            0,
            "a settled poll removes its entry"
        );

        // Block the worker on its own channel so the sample is deterministically
        // `` `pending ``, with no timing guess.
        let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let h4 = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<d>",
            move |_, _c| {
                unblock_rx.recv().unwrap();
                Ok(Value::Unit)
            },
        )
        .unwrap();
        assert_eq!(shell.local.workers.count(), 1);
        let pending = builtin_poll(&[Value::Handle(h4.clone())], &mut shell).unwrap();
        assert!(
            matches!(&pending, Value::Variant { label, .. } if label == "pending"),
            "the worker is blocked, so poll must observe it pending: got {pending:?}"
        );
        assert_eq!(
            shell.local.workers.count(),
            1,
            "a pending poll must not touch the registry"
        );
        unblock_tx.send(()).unwrap();
        await_handle(&h4, &m, &mut shell).expect("await ok");
    }

    /// `race` removes the winner and every cancelled loser: nothing lingers in
    /// the registry once it returns.
    #[test]
    fn race_removes_winner_and_cancelled_losers() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let snap = Arc::new(shell.mobile().scope);
        let winner = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<winner>",
            |_, _c| Ok(Value::Unit),
        )
        .unwrap();

        // The losers block on their own channels, so the winner always settles
        // first and these two are cancelled.
        let (l1_tx, l1_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let loser1 = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<loser1>",
            move |_, _c| {
                let _ = l1_rx.recv();
                Ok(Value::Unit)
            },
        )
        .unwrap();
        let (l2_tx, l2_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let loser2 = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<loser2>",
            move |_, _c| {
                let _ = l2_rx.recv();
                Ok(Value::Unit)
            },
        )
        .unwrap();
        assert_eq!(shell.local.workers.count(), 3);

        let args = [Value::list(vec![
            Value::Handle(winner),
            Value::Handle(loser1),
            Value::Handle(loser2),
        ])];
        builtin_race(&args, &m, &mut shell).expect("race must succeed");
        assert_eq!(
            shell.local.workers.count(),
            0,
            "race removes the winner and both cancelled losers"
        );

        // Release the cancelled-but-blocked losers so nothing stays parked.
        let _ = l1_tx.send(());
        let _ = l2_tx.send(());
    }

    /// The flow rule: the registry `Arc` flows into a worker's own shell, so a
    /// `spawn` nested in a worker's body registers into the *same* registry the
    /// owning shell reads.  The `go_rx` gate buys determinism only — a thread
    /// starts before its outer entry is filed, so an ungated body could sample
    /// ahead of it; production order is deliberately unordered.
    #[test]
    fn nested_spawn_registers_into_the_owning_shells_registry() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let snap = Arc::new(shell.mobile().scope);
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let _outer = spawn_child(
            snap,
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<outer>",
            move |mooring, child_shell| {
                go_rx.recv().unwrap();
                let child_snap = Arc::new(child_shell.mobile().scope);
                let _inner = spawn_child(
                    child_snap,
                    mooring,
                    child_shell,
                    ChildIoMode::Buffered,
                    LeaseClass::Worker,
                    "<inner>",
                    |_, _c| Ok(Value::Unit),
                )
                .unwrap();
                // The outer entry is filed (the gate above), so this count is
                // exact.
                ready_tx.send(child_shell.local.workers.count()).unwrap();
                Ok(Value::Unit)
            },
        )
        .unwrap();
        // The outer entry is filed; release the worker to spawn and sample.
        go_tx.send(()).unwrap();

        let observed_from_worker = ready_rx.recv().unwrap();
        assert_eq!(
            observed_from_worker, 2,
            "the nested spawn's own shell sees both entries in the one shared registry"
        );
        assert_eq!(
            shell.local.workers.count(),
            2,
            "the parent's registry observes the nested spawn's entry too — same registry, not a copy"
        );
    }

    // ── settled retention (the epoch sweep) ──────────────────────────────

    /// Tick the registry clock `n` times, as `n` source dispatches would.
    fn tick(shell: &Shell, n: u64) {
        for _ in 0..n {
            shell.local.workers.tick_epoch();
        }
    }

    /// The retention ledger in integers: an unclaimed settled entry is stamped at
    /// the first sweep that sees it settled, kept while `epoch − stamp <
    /// retention`, expired with one `Retention` notice at the bound.
    #[test]
    fn retention_stamps_then_expires_an_unclaimed_settled_entry() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        shell.arm_worker_retention(2);
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<done>",
            |_, _child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        wait_settled(&handle);

        tick(&shell, 5);
        shell.local.workers.sweep_retention();
        let stamped = shell.local.workers.snapshot();
        assert_eq!(stamped.len(), 1, "a swept settled entry lingers");
        assert_eq!(
            stamped[0].settled_epoch,
            Some(5),
            "stamped at the first sweep that observes it settled"
        );

        tick(&shell, 1);
        shell.local.workers.sweep_retention();
        assert_eq!(shell.local.workers.count(), 1, "6 − 5 < 2: retained");
        assert_eq!(
            shell.local.workers.snapshot()[0].settled_epoch,
            Some(5),
            "the stamp is first-observed-settled, never re-stamped"
        );

        tick(&shell, 1);
        shell.local.workers.sweep_retention();
        assert_eq!(shell.local.workers.count(), 0, "7 − 5 ≥ 2: expired");
        let notices = shell.take_worker_reap_notices();
        assert_eq!(notices.len(), 1, "one notice per retention expiry");
        assert_eq!(notices[0].id, stamped[0].id);
        assert_eq!(notices[0].cmd, stamped[0].cmd);
        assert_eq!(notices[0].class, stamped[0].class);
        assert_eq!(notices[0].cause, ReapCause::Retention);
    }

    /// A session's workers die with its shell: the drop cancels every registered
    /// worker's scope through `LocalState`'s teardown, unaided.
    #[test]
    fn dropping_a_shell_cancels_its_workers() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let _handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<gated>",
            move |_, _c| {
                gate_rx.recv().unwrap();
                Ok(Value::Unit)
            },
        )
        .expect("spawn must succeed");

        let entry = shell
            .local
            .workers
            .snapshot()
            .pop()
            .expect("the spawn registered its worker");
        assert!(!entry.handle.cancel.is_cancelled());

        drop(shell);
        assert!(
            entry.handle.cancel.is_cancelled(),
            "the dropped shell must cancel its registered workers"
        );

        // Release the gated thread so nothing stays parked.
        gate_tx.send(()).unwrap();
    }

    /// An unarmed sweep stamps nothing and expires nothing — the REPL's
    /// "retain indefinitely", structural.
    #[test]
    fn unarmed_sweep_retains_settled_entries_indefinitely() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<kept>",
            |_, _child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        wait_settled(&handle);

        tick(&shell, 1_000);
        shell.local.workers.sweep_retention();
        let snapshot = shell.local.workers.snapshot();
        assert_eq!(snapshot.len(), 1, "an unarmed sweep expires nothing");
        assert_eq!(
            snapshot[0].settled_epoch, None,
            "an unarmed sweep does not even stamp"
        );
    }

    /// Observation beats retention: a stamped entry is removed the moment a
    /// settled `poll` claims it, so a later sweep finds nothing to expire.
    #[test]
    fn observation_beats_retention() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<claimed>",
            |_, _child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        wait_settled(&handle);

        shell.arm_worker_retention(256);
        tick(&shell, 1);
        shell.local.workers.sweep_retention();
        assert_eq!(shell.local.workers.snapshot()[0].settled_epoch, Some(1));

        builtin_poll(&[Value::Handle(handle)], &mut shell).expect("poll ok");
        assert_eq!(
            shell.local.workers.count(),
            0,
            "a settled poll claims the entry"
        );

        tick(&shell, 400);
        shell.local.workers.sweep_retention();
        assert!(
            shell.take_worker_reap_notices().is_empty(),
            "a claimed result leaves no retention notice"
        );
    }

    /// Retention is a settled entry's lease, not a second bound on live work: a
    /// running entry is never stamped or expired, at retention 0 or any epoch.
    #[test]
    fn running_entries_are_never_stamped_or_expired() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let m = Mooring::adrift();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<live>",
            move |_, _c| {
                gate_rx.recv().unwrap();
                Ok(Value::Unit)
            },
        )
        .expect("spawn must succeed");

        shell.arm_worker_retention(0);
        tick(&shell, 5);
        shell.local.workers.sweep_retention();
        tick(&shell, 1_000_000);
        shell.local.workers.sweep_retention();
        let snapshot = shell.local.workers.snapshot();
        assert_eq!(snapshot.len(), 1, "live work is never expired");
        assert_eq!(
            snapshot[0].settled_epoch, None,
            "live work is never stamped"
        );
        assert!(shell.take_worker_reap_notices().is_empty());

        gate_tx.send(()).unwrap();
        await_handle(&handle, &m, &mut shell).expect("await after the gate opens");
    }

    // ── the admission cap ────────────────────────────────────────────────

    /// The cap refuses the (cap+1)th birth at the door, naming `await` and
    /// `cancel` as remedies and registering nothing; cancelling one frees a seat.
    #[test]
    fn worker_cap_rejects_at_the_door_and_frees_on_cancel() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.worker_cap = Some(2);

        let mut gates = Vec::new();
        let mut handles = Vec::new();
        for cmd in ["<one>", "<two>"] {
            let (gate_tx, gate_rx) = mpsc::channel::<()>();
            let handle = spawn_child(
                Arc::new(shell.mobile().scope),
                &m,
                &shell,
                ChildIoMode::Buffered,
                LeaseClass::Worker,
                cmd,
                move |_, _c| {
                    gate_rx.recv().unwrap();
                    Ok(Value::Unit)
                },
            )
            .expect("a birth under the cap must be admitted");
            gates.push(gate_tx);
            handles.push(handle);
        }
        assert_eq!(shell.local.workers.count(), 2);

        let refused = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<three>",
            |_, _c| Ok(Value::Unit),
        );
        let err = match refused {
            Err(Break::Error(e)) => e,
            other => panic!("the capped birth must be refused, got {other:?}"),
        };
        for remedy in ["await", "cancel"] {
            assert!(
                err.message.contains(remedy),
                "the refusal must name `{remedy}`: {}",
                err.message
            );
        }
        assert_eq!(
            shell.local.workers.count(),
            2,
            "a refused birth registers nothing"
        );

        builtin_cancel(&[Value::Handle(handles[0].clone())], &mut shell).expect("cancel ok");
        spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<after>",
            |_, _c| Ok(Value::Unit),
        )
        .expect("cancelling one frees a seat");

        // Unblock the parked workers so none outlives the test.
        for gate in gates {
            let _ = gate.send(());
        }
    }

    /// A durable service is live work too: the cap counts running entries of
    /// every class, so one `Durable` and one `Worker` fill a cap of 2.
    #[test]
    fn durable_birth_counts_toward_the_cap() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.worker_cap = Some(2);

        let mut gates = Vec::new();
        for (class, cmd) in [
            (LeaseClass::Durable, "<service>"),
            (LeaseClass::Worker, "<block>"),
        ] {
            let (gate_tx, gate_rx) = mpsc::channel::<()>();
            spawn_child(
                Arc::new(shell.mobile().scope),
                &m,
                &shell,
                ChildIoMode::Buffered,
                class,
                cmd,
                move |_, _c| {
                    gate_rx.recv().unwrap();
                    Ok(Value::Unit)
                },
            )
            .expect("a birth under the cap must be admitted");
            gates.push(gate_tx);
        }

        let refused = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<three>",
            |_, _c| Ok(Value::Unit),
        );
        assert!(
            matches!(refused, Err(Break::Error(_))),
            "a durable service holds a seat like any live worker"
        );

        for gate in gates {
            let _ = gate.send(());
        }
    }

    /// A settled entry lingering under retention holds no seat: the cap counts
    /// running workers, not registry entries.
    #[test]
    fn settled_entries_do_not_block_admission() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut m = Mooring::adrift();
        m.worker_cap = Some(1);
        let first = spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<done>",
            |_, _c| Ok(Value::Unit),
        )
        .expect("the first birth is admitted");
        wait_settled(&first);
        assert_eq!(shell.local.workers.count(), 1, "the settled entry lingers");

        spawn_child(
            Arc::new(shell.mobile().scope),
            &m,
            &shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<next>",
            |_, _c| Ok(Value::Unit),
        )
        .expect("a lingering settled entry must not hold a seat");
    }
}
