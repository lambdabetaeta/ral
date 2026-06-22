//! Concurrency primitives: `spawn`, `watch`, `await`, `race`, `cancel`.
//!
//! The handle is the evidence of detachment.  `spawn` (and REPL-only
//! `watch`) reify a [`Value::Handle`] and park their worker under the
//! durable session root, so it outlives the turn that launched it; a
//! foreground cancel — a turn deadline or interrupt — cannot reach a
//! worker that is not a child of the foreground scope.  `await`, `race`,
//! and `poll` are the eliminators that observe a handle from the
//! foreground, and `cancel` the explicit reaper.
//!
//! Each spawned concurrent block runs on its own OS thread with a
//! cloned environment snapshot.  IO is either buffered (returned as a
//! record by `await`) or line-framed (streamed live with a label
//! prefix for `watch`).
//!
//! Every finished block has one observable outcome — a [`CompletedHandle`]
//! carrying the bytes it wrote (drained once into the handle's cache) and
//! its raw result.  The eliminators project that one outcome: `await` and
//! `race` to the record `{ value: Value, stdout: Bytes, stderr: Bytes }`
//! (re-raising a failed block), `poll` to a `` `settled `` / `` `pending ``
//! variant (reporting a failed block as data rather than re-raising).  The
//! block's stdout and stderr are not auto-replayed to the caller's
//! terminal; they sit in the projected record.

use crate::evaluator::absorb_tail;
use crate::evaluator::comp::{eval_comp, with_scope};
use crate::evaluator::scope::error_record;
use crate::io::{Sink, new_buffer, take_buffer};
use crate::types::*;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};

use super::util::{as_list, check_arity, expect_handle, expect_thunk};

/// How a child concurrent block's stdout/stderr are wired.
pub(super) enum ChildIoMode {
    Buffered,
    Watch { label: String },
}

/// Cap on a detached worker's deferred surface buffer.  Past it, one overflow
/// marker is recorded and further events drop, so a runaway detached emitter (a
/// `meter` in a server loop) cannot grow memory without bound.  The marker is
/// itself a surface event; a host that does not know its tag drops it.
const DEFERRED_SURFACE_CAP: usize = 4096;

/// The surface a *detached* worker installs.  Unlike a same-thread thunk body,
/// a detached worker may outlive the turn that spawned it, so it must not hold
/// the turn's live sink: it buffers structured events into a bounded
/// [`SurfaceBuffer`].  The buffer has two ways out — `await`/`race` replay it
/// through the awaiting turn's surface (the live-frame pull), and at completion
/// the worker flushes a clone to its [`Boundary`] sink (the un-awaited
/// fallback) — gated to deliver exactly once by the handle's `joined` latch.
struct DeferredSurface {
    buf: SurfaceBuffer,
    boundary: Option<Boundary>,
}

impl EventSink for DeferredSurface {
    fn emit(&self, ev: &Value) {
        let mut buf = self.buf.lock().unwrap();
        if buf.len() < DEFERRED_SURFACE_CAP {
            buf.push(ev.clone());
        } else if buf.len() == DEFERRED_SURFACE_CAP {
            buf.push(Value::Variant {
                label: "surface-overflow".into(),
                payload: None,
            });
        }
        // Past the cap (marker already recorded): drop.
    }
}

/// The final surface event a detached worker appends to its buffer at
/// completion: a `` `done `` record carrying the handle's `cmd` and the
/// worker's `outcome` — `` `ok ``, `` `err `` (the [`break_record`]), or
/// `` `panic `` (the message).  Core names the event; exarch names its
/// appearance.  It carries no return value — the model `await`s for that.
fn done_event(cmd: &str, outcome: Value) -> Value {
    Value::Variant {
        label: "done".into(),
        payload: Some(Box::new(Value::map(vec![
            ("cmd".into(), Value::String(cmd.into())),
            ("outcome".into(), outcome),
        ]))),
    }
}

impl DeferredSurface {
    /// Flush this worker's deferred surface — the buffer plus a final
    /// [`done_event`] — to its [`Boundary`] sink as one batch, gated to deliver
    /// once by the shared `joined` latch.  A no-op when no boundary is installed
    /// (a bare REPL): then the deferred surface reaches a sink only via
    /// `await`/`race`.  The batch is a fresh clone so it is independent of
    /// whatever an eliminator later drains from the same buffer
    /// (`complete_handle`'s `mem::take`).
    fn flush(&self, joined: &Arc<Mutex<bool>>, cmd: &str, outcome: Value) {
        let Some(boundary) = self.boundary.as_ref() else {
            return;
        };
        let mut batch = self.buf.lock().unwrap().clone();
        batch.push(done_event(cmd, outcome));
        boundary.deliver(batch, joined.clone());
    }
}

/// Drop-guard that flushes the worker's deferred surface to its boundary on
/// *every* exit path.  The clean path arms it with the body's `` `ok ``/`` `err ``
/// outcome and disarms it explicitly (the clone is taken before the result is
/// sent on the handle channel, so the boundary's copy is independent of the
/// eliminators' later drain); an unwinding panic leaves it armed, so `drop`
/// fires with a `` `panic `` outcome and the unwind then continues — the worker
/// still drops its `Sender` without sending, so the receiver reports
/// `Disconnected` and `try_settle` settles the handle as a panic exactly as
/// before.
struct FlushGuard {
    surface: Arc<DeferredSurface>,
    joined: Arc<Mutex<bool>>,
    cmd: String,
    armed: bool,
}

impl FlushGuard {
    /// Disarm and flush with `outcome` on a non-panicking exit.  Taking the
    /// clone here, before the clean path sends the result, keeps the boundary's
    /// copy independent of the eliminators' later `mem::take`.
    fn settle(mut self, outcome: Value) {
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
            self.surface.flush(&self.joined, &self.cmd, panic);
        }
    }
}

/// Spawn a child concurrent block on a new OS thread.
///
/// The child receives a cloned environment with IO wired according to
/// `io_mode`.  The returned `HandleInner` can be awaited or cancelled.
///
/// `work` is the worker's body, returning [`Raw<Value>`] so the body's
/// terminal tail call (if any) is absorbed here at the worker's
/// trampoline before the result enters the handle channel.  Settled
/// is the wire shape for `HandleInner.result` — a tail call cannot
/// cross between threads.
pub(super) fn spawn_child<F>(
    snap: Arc<Env>,
    shell: &mut Shell,
    io_mode: ChildIoMode,
    cmd: &str,
    work: F,
) -> Settled<HandleInner>
where
    F: FnOnce(&mut Shell) -> Raw<Value> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();

    // Allocate buffers and build the child's sinks.  Buffered mode writes
    // into the shared byte buffers; watch mode wraps clones of the parent
    // stdout in `Sink::LineFramed` with a per-handle prefix.
    let (stdout_sink, stdout_buf) = new_buffer();
    let (stderr_sink, stderr_buf) = new_buffer();
    // The detached worker buffers `surface` calls here rather than holding the
    // spawning turn's live sink; `await`/`race` replay it once and the worker
    // flushes a clone to the boundary at completion.
    let surface_buf: SurfaceBuffer = Arc::new(Mutex::new(Vec::new()));
    // The session-lived boundary the worker flushes to at completion, captured
    // from the spawning turn so it survives that turn's teardown and a nested
    // `spawn` inside the worker inherits it.  The worker's `DeferredSurface`
    // holds both the buffer and this destination; the `joined` latch is shared
    // with the eliminators so whichever renders first wins the deliver-once test.
    let boundary = shell.turn.boundary.clone();
    let worker_surface = Arc::new(DeferredSurface {
        buf: surface_buf.clone(),
        boundary: boundary.clone(),
    });
    let joined = Arc::new(Mutex::new(false));
    let worker_joined = joined.clone();
    let (stdout, stderr, flush_pending) = match io_mode {
        ChildIoMode::Buffered => (stdout_sink, stderr_sink, false),
        ChildIoMode::Watch { label } => {
            let clone_parent = || {
                shell
                    .turn
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

    let (_join, cancel) = shell.spawn_thread(snap, move |child_env| {
        child_env.turn.io.capture_outer = None;
        child_env.turn.io.stdout = stdout;
        child_env.turn.io.stderr = stderr;
        // The boundary flows onto the worker's turn so a nested `spawn` inside
        // the body installs its own `DeferredSurface` with the same boundary
        // and flushes at *its* own completion.
        child_env.turn.boundary = boundary;
        child_env.turn.surface = Some(worker_surface.clone());

        // Arm the flush guard before the body runs: a panicking body unwinds
        // through it (a `` `panic `` batch, then the unwind continues to drop
        // `tx` unsent), while the clean path disarms it with the body's
        // outcome.  When no boundary is installed it is inert on every path.
        let guard = FlushGuard {
            surface: worker_surface,
            joined: worker_joined,
            cmd: worker_cmd,
            armed: true,
        };

        // Worker absorption point: a tail call cannot cross the thread
        // boundary, so the worker root settles it into the channel
        // result.  `work` returns `Raw<Value>` precisely so a terminal
        // tail call surfaces here rather than collapsing inside.
        let result = absorb_tail(work(child_env), child_env);
        if flush_pending {
            let _ = child_env.turn.io.stdout.flush_pending();
            let _ = child_env.turn.io.stderr.flush_pending();
        }
        // Flush the boundary's clone before sending the result, so its copy is
        // independent of `complete_handle`'s later `mem::take` of the buffer.
        let outcome = match &result {
            Ok(_) => Value::Variant {
                label: "ok".into(),
                payload: Some(Box::new(Value::Unit)),
            },
            Err(e) => Value::Variant {
                label: "err".into(),
                payload: Some(Box::new(break_record(e))),
            },
        };
        guard.settle(outcome);
        let _ = tx.send(result);
    });

    // Under a frame that arms a detached-worker lifetime ceiling (exarch),
    // hand the worker's scope to the shared reaper so an abandoned worker
    // is force-cancelled once the ceiling elapses.  The death-clock is
    // fire-and-forget: the worker outlives this `spawn` call, so the
    // deadline is `keep()`-ed to fire at its ceiling regardless — a late
    // cancel of an already-finished worker is harmless.  The interactive
    // frame (the REPL) arms none, leaving the worker to `cancel`, root
    // abort, or session exit.
    if let Some(ceiling) = shell.turn.detached_ceiling {
        crate::process::arm_lifetime(cancel.clone(), ceiling).keep();
    }

    Ok(HandleInner {
        result: Arc::new(Mutex::new(Some(rx))),
        cached: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(HandleState::Running)),
        stdout_buf,
        stderr_buf,
        surface_buf,
        joined,
        cmd,
        cancel,
    })
}

// ── spawn ────────────────────────────────────────────────────────────────

/// `spawn <thunk>` -- spawn a concurrent block on a worker thread, return a handle.
pub(crate) fn builtin_spawn(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "spawn")?;
    let (body, captured) = expect_thunk(&args[0], "spawn")?;
    spawn_buffered(body, captured, shell)
}

/// Buffered spawn (§13.3 replay rule).  The child's stdout/stderr accumulate
/// in per-handle buffers and are drained to the caller's sinks on `await`.
///
/// A concurrent block is a thunk evaluated on a worker thread.  The
/// worker inherits the parent's mobile state via [`Shell::spawn_thread`]
/// (called inside [`spawn_child`]) — including the captured environment
/// installed as the worker's `env` — and runs the body via
/// `with_scope(eval_comp(body))` directly.  No top-level/block boundary
/// ceremony: the worker's `Shell` is the only shell the body interacts
/// with, so "blocks discard their mobile" is satisfied automatically by
/// the thread's natural lifecycle; and the OS sandbox (if any) wraps
/// the worker by virtue of wrapping the parent process, so no confined
/// re-exec is attempted from a worker thread.
fn spawn_buffered(
    body: Arc<crate::ir::Comp>,
    captured: Arc<Env>,
    shell: &mut Shell,
) -> Settled<Value> {
    Ok(Value::Handle(spawn_child(
        captured,
        shell,
        ChildIoMode::Buffered,
        "<block>",
        // The worker body is the sole computation of a fresh thread,
        // under the trivial continuation the thread's join provides;
        // `spawn_child` absorbs any terminal tail call on that thread.
        move |child_env| with_scope(child_env, |s| eval_comp(&body, s, Tail::Yes)),
    )?))
}

// ── watch ────────────────────────────────────────────────────────────────

/// `watch <label> <thunk>` -- spawn a concurrent block whose output
/// streams live to the caller's stdout, line-framed with the given
/// label.
///
/// The watched worker is detached at the root and keeps writing as it
/// runs, so its sink must outlive the turn.  Availability is therefore the
/// host's: a host with a durable stdout sink (the ral REPL, batch scripts)
/// installs it via [`crate::builtins::WATCH_BUILTIN`]; a host whose active
/// streams are per-call capture buffers (exarch) leaves it uninstalled, so
/// naming `watch` there is an unknown-name diagnostic rather than a runtime
/// refusal.
pub(super) fn builtin_watch(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "watch")?;
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
    spawn_labelled(body, captured, label, shell)
}

/// Line-framed spawn.  The child writes to a `Sink::LineFramed` wrapping a
/// clone of the caller's stdout (resp. stderr), so every complete line arrives
/// on the caller's stream prefixed `[label] ` (resp. `[label:err] `) without
/// any global multiplexer.  Sibling watchers serialise through the OS stdout
/// lock or through the `Sink::External` adapter's internal mutex, so each
/// line is emitted atomically.  The `await` replay drain is a no-op because
/// the stdout/stderr buffers stay empty.
///
/// Body dispatch matches [`spawn_buffered`]: the body runs via
/// `with_scope(eval_comp(body))` directly on the worker thread.
fn spawn_labelled(
    body: Arc<crate::ir::Comp>,
    captured: Arc<Env>,
    label: std::string::String,
    shell: &mut Shell,
) -> Settled<Value> {
    Ok(Value::Handle(spawn_child(
        captured,
        shell,
        ChildIoMode::Watch { label },
        "<watch>",
        // The worker body is the sole computation of a fresh thread,
        // under the trivial continuation the thread's join provides;
        // `spawn_child` absorbs any terminal tail call on that thread.
        move |child_env| with_scope(child_env, |s| eval_comp(&body, s, Tail::Yes)),
    )?))
}

/// `cancel <handle>` -- mark a running concurrent block as cancelled.
pub(super) fn builtin_cancel(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "cancel")?;
    let handle = expect_handle(&args[0], "cancel")?;
    let state = *handle.state.lock().unwrap();
    if state != HandleState::Completed {
        handle.cancel.cancel(crate::process::CancelCause::Explicit);
        detach_handle(handle);
    }
    shell.mobile.control.last_status = 0;
    Ok(Value::Unit)
}

/// Error if `handle` was cancelled; otherwise pass.  The
/// pre-check shared by `await` and `poll`: a detached handle has no
/// result to wait for or sample, so observing one is an error (status 1).
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

/// Non-blocking settle: `Some(completed)` if the handle has an outcome to
/// observe now (the cached [`CompletedHandle`], or a just-arrived channel
/// message drained into one), `None` if the worker is still running.  The
/// sampling step shared by `await`'s fast path, `poll`, and `race`.
///
/// A `Disconnected` receiver means the worker dropped its `Sender`
/// without sending — i.e. it panicked before producing a result.  That
/// settles the handle with the same panic error `await`'s blocking path
/// reports, so `poll` and `race` observe a finished (failed) block rather
/// than spinning on a `None` they would read as still-running.
fn try_settle(handle: &HandleInner, shell: &mut Shell) -> Option<CompletedHandle> {
    if let Some(completed) = handle.cached.lock().unwrap().clone() {
        set_status_from_outcome(&completed.outcome, shell);
        return Some(completed);
    }
    let rx_guard = handle.result.lock().unwrap();
    let rx = (*rx_guard).as_ref()?;
    match rx.try_recv() {
        Ok(result) => {
            drop(rx_guard);
            Some(complete_handle(handle, result, shell))
        }
        Err(TryRecvError::Disconnected) => {
            drop(rx_guard);
            let panicked = Err(sig("await: spawned thread panicked"));
            Some(complete_handle(handle, panicked, shell))
        }
        Err(TryRecvError::Empty) => None,
    }
}

/// Replay a finished detached worker's deferred surface events through the
/// awaiting turn's *current* surface — once.  Only the foreground eliminators
/// `await`/`race` call this: a `poll`ed-but-not-awaited handle and a handle no
/// turn ever awaits emit no cards, and repeated `await` does not duplicate
/// them.  A detached worker's `surface` calls are buffered (never on the
/// possibly-ended spawning turn's sink), so this is where they finally surface
/// — on whichever turn observes the handle.
fn replay_deferred_surface(handle: &HandleInner, completed: &CompletedHandle, shell: &mut Shell) {
    {
        let mut joined = handle.joined.lock().unwrap();
        if *joined {
            return;
        }
        *joined = true;
    }
    if let Some(sink) = shell.turn.surface.as_ref() {
        for ev in &completed.surface {
            sink.emit(ev);
        }
    }
}

/// Project a finished block's outcome to the `await`/`race` record:
/// `Ok(value)` → `{value, stdout, stderr}`, re-raising `Err(e)` verbatim.
/// `$status` already reflects the block (set when the outcome was cached).
fn project_completed(completed: CompletedHandle) -> Settled<Value> {
    let value = completed.outcome?;
    Ok(Value::map(vec![
        ("value".into(), value),
        ("stdout".into(), Value::Bytes(completed.stdout)),
        ("stderr".into(), Value::Bytes(completed.stderr)),
    ]))
}

/// One cancel-aware foreground wait, shared by `await` and `race`.  Sweep
/// `handles` repeatedly, returning the first that has settled together
/// with the handle it settled — skipping any already cancelled, erroring
/// if none remain live.  Between sweeps it polls `process::check`, so a
/// foreground-scope cancel (a deadline or interrupt) unwinds the wait;
/// but the workers hang at the durable root, not the foreground, so a
/// cut-short wait leaves them alive to be observed on a later turn.
fn wait_first_settled<'a>(
    handles: &[&'a HandleInner],
    shell: &mut Shell,
) -> Settled<(&'a HandleInner, CompletedHandle)> {
    loop {
        let mut saw_running = false;
        for &handle in handles {
            match *handle.state.lock().unwrap() {
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
        crate::process::check(shell)?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Block in the foreground until `handle` completes, replay its buffered
/// output, and return its result record.  Errors if the handle was
/// cancelled, and re-raises a failed block.  The wait shares `race`'s
/// cancel-aware loop rather than a bare `recv`: a foreground deadline or
/// interrupt unwinds it, but the root-scoped worker survives the turn.
pub(super) fn await_handle(handle: &HandleInner, shell: &mut Shell) -> Settled<Value> {
    ensure_live(handle, shell)?;
    let (_, completed) = wait_first_settled(&[handle], shell)?;
    replay_deferred_surface(handle, &completed, shell);
    project_completed(completed)
}

/// `await <handle>` -- wait for a concurrent block to complete and return its result record.
pub(super) fn builtin_await(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "await")?;
    let handle = expect_handle(&args[0], "await")?;
    await_handle(handle, shell)
}

/// `poll <handle>` -- non-blocking, total sample of a concurrent block.
/// Yields a two-arm variant and never re-raises a finished block:
///
/// - `` `settled `` carrying `{stdout, stderr, outcome}`, where `outcome`
///   is `` `ok `` with the block's value or `` `err `` with the error
///   record, when the block finished (returned, raised, or panicked).
/// - `` `pending `` (Unit payload) while the block is still running.
///
/// Errors only on a cancelled handle (via [`ensure_live`]):
/// a detached handle has no outcome to sample.  Unlike `await`/`race`, a
/// raised or panicked block is reported as data, not re-raised — so a
/// successful `poll` leaves `$status` at 0 regardless of the block's own
/// status, which is data inside `outcome.err.status`.
pub(super) fn builtin_poll(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "poll")?;
    let handle = expect_handle(&args[0], "poll")?;
    ensure_live(handle, shell)?;
    let variant = |label: &str, payload| Value::Variant {
        label: label.into(),
        payload,
    };
    let result = match try_settle(handle, shell) {
        Some(completed) => {
            let outcome = match completed.outcome {
                Ok(value) => variant("ok", Some(Box::new(value))),
                Err(e) => variant("err", Some(Box::new(break_record(&e)))),
            };
            let settled = Value::map(vec![
                ("stdout".into(), Value::Bytes(completed.stdout)),
                ("stderr".into(), Value::Bytes(completed.stderr)),
                ("outcome".into(), outcome),
            ]);
            variant("settled", Some(Box::new(settled)))
        }
        None => variant("pending", Some(Box::new(Value::Unit))),
    };
    // `poll` itself succeeded: the block's status (if any) is data inside
    // `outcome.err.status`, never a failure of `poll`.
    shell.mobile.control.last_status = 0;
    Ok(result)
}

/// `race <handles>` -- wait for the first of several tasks to finish.
/// Cancels remaining handles once a winner is found, then projects the
/// winner's outcome to the `await` record (re-raising a failed winner).
pub(super) fn builtin_race(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    if args.is_empty() {
        return Err(sig("race requires 1 argument (list of handles)"));
    }
    let values = as_list(&args[0], "race")?;
    let mut handles: Vec<&HandleInner> = Vec::new();
    for v in &values {
        if let Value::Handle(h) = v {
            handles.push(h);
        }
    }

    let (winner, completed) = wait_first_settled(&handles, shell)?;
    for &h in &handles {
        if !Arc::ptr_eq(&h.result, &winner.result) {
            h.cancel.cancel(crate::process::CancelCause::Explicit);
            detach_handle(h);
        }
    }
    replay_deferred_surface(winner, &completed, shell);
    project_completed(completed)
}

/// The exit code carried by a settled error: the runtime error's own
/// code for `Break::Error`, else 1.  The basis of `$status` propagation.
fn error_exit_code(e: &Break) -> i32 {
    match e {
        Break::Error(e) => e.exit_code(),
        _ => 1,
    }
}

/// `poll`'s `` `err `` payload — the same `{cmd, status, message, line,
/// col}` record `try` hands its handler thunk, built from the block's
/// `Break` via the shared [`error_record`] constructor.  An `Escape`
/// (a block that `exit`ed or stopped) carries no located message, so it
/// reports the escape's exit code with a `<runtime>` source position.
fn break_record(e: &Break) -> Value {
    match e {
        Break::Error(err) => {
            let (line, col) = err.loc.as_ref().map(|l| (l.line, l.col)).unwrap_or((0, 0));
            error_record("<runtime>", err.exit_code(), &err.message, line, col)
        }
        Break::Escape(esc) => {
            let (status, message) = match esc {
                Escape::Exit(code) => (*code, "block exited".to_string()),
                #[cfg(unix)]
                Escape::Stopped { .. } => (1, "block stopped".to_string()),
            };
            error_record("<runtime>", status, &message, 0, 0)
        }
    }
}

/// Set `shell.mobile.control.last_status` from a finished block's outcome:
/// 0 on success, the error's exit code on failure.
fn set_status_from_outcome(outcome: &Settled<Value>, shell: &mut Shell) {
    shell.mobile.control.last_status = match outcome {
        Ok(_) => 0,
        Err(e) => error_exit_code(e),
    };
}

/// Transition a handle to `Completed`, drain both byte buffers exactly
/// once into a cached [`CompletedHandle`], set `$status` from the
/// outcome, and return the completed outcome.  Draining on both the ok and
/// err paths captures a failed block's bytes; the cache stays valid for
/// any repeat observation.
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
    use std::sync::mpsc;

    fn status(b: Break) -> i32 {
        match b {
            Break::Error(e) => e.exit_code(),
            other => panic!("expected Break::Error, got {other:?}"),
        }
    }

    /// A handle whose worker never sent a result and dropped its `Sender`,
    /// modelling a panicked worker.  The stdout/stderr buffers are
    /// pre-seeded so a `` `settled `` outcome can be checked for the bytes a
    /// real block would have buffered before panicking.
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
            cmd: "<test>".into(),
            cancel: crate::process::CancelScope::default(),
        }
    }

    /// Destructure a `` `label payload `` variant, asserting the label.
    fn expect_variant<'a>(v: &'a Value, label: &str) -> &'a Value {
        match v {
            Value::Variant {
                label: l,
                payload: Some(p),
            } if l == label => p,
            other => panic!("expected `{label} with payload, got {other:?}"),
        }
    }

    /// Destructure a `Value::Map`, panicking on any other shape.
    fn expect_map(v: &Value) -> &Map {
        match v {
            Value::Map(m) => m,
            other => panic!("expected Map, got {other:?}"),
        }
    }

    /// A panicked worker drops its `Sender`, so the receiver reports
    /// `Disconnected`.  `try_settle` must report a settled (failed)
    /// outcome rather than `None` — otherwise `poll` reads `pending`
    /// forever and `race` spins.  The error matches `await`'s
    /// blocking-path panic text.
    #[test]
    fn try_settle_reports_disconnected_worker_as_failed() {
        let mut shell = Shell::new(Default::default());
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

    /// `poll` over a panicked worker yields `` `settled `` (never
    /// `` `pending ``, never re-raising), carrying the bytes the worker
    /// buffered before panicking and an `` `err `` outcome with the error's
    /// exit code.  A successful `poll` leaves `$status` at 0.
    #[test]
    fn poll_reports_disconnected_worker_as_settled_err() {
        let mut shell = Shell::new(Default::default());
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

        // A second poll reads the cached outcome and the same bytes —
        // repeated polls stay consistent.
        let poll2 = builtin_poll(&args, &mut shell).expect("repeat poll must not re-raise");
        assert_eq!(poll1, poll2);
    }

    /// A worker spawned by `spawn_thread` polls `process::check` against
    /// its own scope.  `ready` confirms the worker is alive before the
    /// cancel, so the test pins propagation rather than a worker that
    /// happened to never start; the worker then reports the status its
    /// poll observed once `cancel` fires the scope it stored.  The
    /// returned [`CancelScope`](crate::process::CancelScope) is the
    /// worker's own scope, so a caller can assert cancellation at the
    /// scope level rather than through the conflated 130 status.
    fn spawn_polling_worker(
        shell: &mut Shell,
        cancel_via: impl FnOnce(&crate::process::CancelScope),
    ) -> (i32, crate::process::CancelScope) {
        let snap = Arc::new(shell.mobile().scope);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (_join, worker_cancel) = shell.spawn_thread(snap, move |child| {
            ready_tx.send(()).unwrap();
            loop {
                if let Err(b) = crate::process::check(child) {
                    done_tx.send(status(b)).unwrap();
                    return;
                }
                std::thread::yield_now();
            }
        });
        ready_rx.recv().unwrap();
        cancel_via(&worker_cancel);
        (done_rx.recv().unwrap(), worker_cancel)
    }

    /// Cancelling the worker's own scope (what `cancel` / `race`-of-losers
    /// fire) stops the worker.  The proof is at the scope level: the
    /// worker's scope observes `is_cancelled()`, while an uncancelled
    /// sibling spawned from the same shell does not.  These are direct
    /// `CancelScope` reads, immune to the process-global `SIGNAL_COUNT`
    /// that `check` also folds into status 130 — so the test pins scope
    /// propagation rather than passing on a transient signal another
    /// test set.  The worker's reported 130 confirms it actually unwound.
    #[test]
    fn worker_scope_cancel_stops_the_worker() {
        let mut shell = Shell::new(Default::default());
        let (_idle_join, sibling) = shell.spawn_thread(Arc::new(shell.mobile().scope), |_| ());
        let (observed, worker_scope) = spawn_polling_worker(&mut shell, |c| {
            c.cancel(crate::process::CancelCause::Explicit)
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

    /// A [`RootAbort`](crate::process::CancelCause::RootAbort) on the
    /// durable root reaches the worker: the worker's scope is a
    /// descendant of the root, so the root flag is visible at the
    /// worker's next poll.
    #[test]
    fn root_cancel_reaches_the_worker() {
        let mut shell = Shell::new(Default::default());
        let root = shell.session.root.clone();
        let (observed, worker_scope) = spawn_polling_worker(&mut shell, move |_| {
            root.cancel(crate::process::CancelCause::RootAbort)
        });
        assert!(
            worker_scope.is_cancelled(),
            "a RootAbort on the durable root must cancel the worker's scope"
        );
        assert_eq!(observed, 130);
    }

    /// A foreground cancel spares a detached worker: the worker parents
    /// under the durable root, not the swappable foreground scope, so
    /// cancelling `turn.cancel` does not reach it.  This is the
    /// collateral-kill fix made executable — a turn timeout on the
    /// foreground must not reap a `spawn`/`watch` worker meant to outlive
    /// the turn.
    #[test]
    fn foreground_cancel_spares_detached_worker() {
        let shell = Shell::new(Default::default());
        let snap = Arc::new(shell.mobile().scope);
        let (_join, worker_scope) = shell.spawn_thread(snap, |_| ());
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);
        assert!(
            !worker_scope.is_cancelled(),
            "a foreground cancel must not reach a detached worker"
        );
    }

    /// A blocked `await` unwinds when the foreground scope is cancelled (a
    /// turn deadline or interrupt) instead of sleeping forever on a bare
    /// `recv`, yet the worker it was awaiting — root-parented, not
    /// foreground — is left alive to be awaited again on a later turn.
    /// One test for the two failure modes the old bare-`recv` path
    /// produced: the past-the-wall hang and the collateral kill, both gone.
    #[test]
    fn await_unwinds_on_foreground_cancel_sparing_the_worker() {
        let mut shell = Shell::new(Default::default());

        // A still-running worker: its result channel never receives, and
        // its cancel scope is a child of the durable root, exactly as a
        // real `spawn` worker's is.  The `Sender` stays alive so the
        // receiver reports `Empty` (pending), not `Disconnected` (panicked).
        let (_tx, rx) = mpsc::channel::<Settled<Value>>();
        let (_sink, stdout_buf) = new_buffer();
        let (_sink2, stderr_buf) = new_buffer();
        let worker_scope = shell.session.root.child().as_scope().clone();
        let handle = HandleInner {
            result: Arc::new(Mutex::new(Some(rx))),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(HandleState::Running)),
            stdout_buf,
            stderr_buf,
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            cmd: "<test>".into(),
            cancel: worker_scope.clone(),
        };

        // Cancel the foreground (turn deadline / interrupt), not the root.
        shell
            .turn
            .cancel
            .cancel(crate::process::CancelCause::Interrupt);

        let err = await_handle(&handle, &mut shell)
            .expect_err("await must unwind on a foreground cancel, not block");
        assert!(matches!(err, Break::Error(_)));
        assert!(
            !worker_scope.is_cancelled(),
            "the foreground cancel must unblock await without reaping the root-parented worker"
        );
    }

    /// A `spawn` under an agent ceiling arms the worker's lifetime with the
    /// shared reaper: the worker's own scope is force-cancelled with
    /// `Deadline` once the (tiny, here) ceiling elapses, even after the
    /// worker itself has finished.
    #[test]
    fn spawn_under_agent_frame_arms_the_ceiling() {
        let mut shell = Shell::new(Default::default());
        shell.turn.detached_ceiling = Some(std::time::Duration::from_millis(20));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            "<test>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

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
            "an agent frame must arm the worker's lifetime ceiling"
        );
        assert_eq!(scope.cause(), Some(crate::process::CancelCause::Deadline));
    }

    /// A `spawn` under the interactive frame arms no ceiling: the worker's
    /// scope is never reaped on a timer.
    #[test]
    fn spawn_under_interactive_frame_arms_no_ceiling() {
        let mut shell = Shell::new(Default::default());
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            "<test>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(
            !scope.is_cancelled(),
            "the interactive frame must arm no lifetime ceiling"
        );
    }

    /// A detached worker's `surface` events are buffered, not emitted live,
    /// and replay through the *awaiting* turn's surface exactly once: `poll`
    /// never replays, the first `await` replays, and a second `await` does
    /// not duplicate.  Models `spawn { surface … }` then observing the handle.
    #[test]
    fn deferred_surface_replays_once_on_await_not_poll() {
        struct Rec(Arc<Mutex<Vec<Value>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &Value) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }

        let mut shell = Shell::new(Default::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        shell.turn.surface = Some(Arc::new(Rec(log.clone())));

        // A settled handle carrying one buffered surface event, modelling a
        // detached worker that called `surface` once and returned.  The
        // sender stays alive so the receiver sees the value, not a disconnect.
        let (tx, rx) = mpsc::channel::<Settled<Value>>();
        tx.send(Ok(Value::Unit)).unwrap();
        let (_s, stdout_buf) = new_buffer();
        let (_s2, stderr_buf) = new_buffer();
        let surface_buf: SurfaceBuffer = Arc::new(Mutex::new(vec![Value::Variant {
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
            cmd: "<test>".into(),
            cancel: crate::process::CancelScope::default(),
        };

        // `poll` settles the handle (draining the buffer into the cache) but
        // never replays: no cards yet.
        builtin_poll(&[Value::Handle(handle.clone())], &mut shell).expect("poll ok");
        assert_eq!(log.lock().unwrap().len(), 0, "poll must not replay surface");

        // The first `await` replays the buffered card exactly once.
        await_handle(&handle, &mut shell).expect("await ok");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "await replays the deferred card"
        );

        // A second `await` reads the cache and must not duplicate it.
        await_handle(&handle, &mut shell).expect("await ok");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "repeat await must not duplicate"
        );
    }

    /// A boundary test double standing in for the agent host: it records each
    /// delivered batch, but only the one that wins the shared `joined`
    /// test-and-set — exactly as the host renders only the batch that beats the
    /// eliminators' replay.
    struct RecBoundary(Arc<Mutex<Vec<Vec<Value>>>>);

    impl BoundarySink for RecBoundary {
        fn deliver(&self, batch: Vec<Value>, joined: Arc<Mutex<bool>>) {
            let mut won = joined.lock().unwrap();
            if *won {
                return;
            }
            *won = true;
            self.0.lock().unwrap().push(batch);
        }
    }

    /// Spin until the boundary has recorded a batch (the worker flushed) or the
    /// budget elapses, returning the recorded batches.  A `spawn_child` worker
    /// runs on its own thread, so its completion flush is observed by waiting on
    /// the destination rather than on the result channel.
    fn wait_for_batch(batches: &Arc<Mutex<Vec<Vec<Value>>>>) -> Vec<Vec<Value>> {
        for _ in 0..500 {
            {
                let got = batches.lock().unwrap();
                if !got.is_empty() {
                    return got.clone();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("worker never flushed its batch to the boundary");
    }

    /// The `outcome` label inside a batch's trailing `` `done `` event.  Pins
    /// the structural shape the exarch decoder matches: `` `done `` carries a
    /// `{cmd, outcome}` map, and `outcome` is a closed `` `ok ``/`` `err ``/
    /// `` `panic `` variant.
    fn done_outcome_label(done: &Value) -> String {
        let fields = expect_map(expect_variant(done, "done"));
        match fields.get("outcome").expect("outcome field") {
            Value::Variant { label, .. } => label.to_string(),
            other => panic!("outcome must be a variant, got {other:?}"),
        }
    }

    /// Install a boundary and `spawn_child` a worker; on completion the worker
    /// flushes its buffer plus a trailing `` `done `` event to the boundary as
    /// one batch.  This is the un-awaited delivery the ADR adds: a fire-and-forget
    /// worker reaches a sink with no eliminator at all.  Each outcome — clean
    /// return, raised `Err`, panic — stamps the matching `done` label.
    #[test]
    fn detached_worker_flushes_done_to_boundary() {
        fn run(work: impl FnOnce(&mut Shell) -> Raw<Value> + Send + 'static) -> Vec<Value> {
            let mut shell = Shell::new(Default::default());
            let batches = Arc::new(Mutex::new(Vec::new()));
            shell.turn.boundary = Some(Arc::new(RecBoundary(batches.clone())));
            let snap = Arc::new(shell.mobile().scope);
            // Hold the handle (and its receiver) so the channel stays connected
            // until the worker has flushed; never observed, so no eliminator
            // competes for the `joined` latch.
            let _handle =
                spawn_child(snap, &mut shell, ChildIoMode::Buffered, "<block>", work).unwrap();
            let mut got = wait_for_batch(&batches);
            assert_eq!(got.len(), 1, "one batch per completed worker");
            got.pop().unwrap()
        }

        // Clean return: a `` `done `` whose outcome is `` `ok ``, carrying the
        // handle's cmd.
        let ok = run(|_child| Ok(Value::Unit));
        assert_eq!(ok.len(), 1, "an empty body's batch is just the `done event");
        let done = &ok[0];
        assert_eq!(done_outcome_label(done), "ok");
        let fields = expect_map(expect_variant(done, "done"));
        assert_eq!(fields.get("cmd"), Some(&Value::String("<block>".into())));

        // Raised `Err`: a `` `done `` whose outcome is `` `err ``.
        let err = run(|_child| Err(sig("boom").into()));
        assert_eq!(done_outcome_label(&err[0]), "err");

        // Panic: the guard fires on the unwind with a `` `panic `` outcome.
        let panicked = run(|_child| panic!("worker exploded"));
        assert_eq!(done_outcome_label(&panicked[0]), "panic");
    }

    /// The body's own `surface`/`io` values appear in the batch *before* the
    /// trailing `` `done ``: the boundary carries the full deferred surface, not
    /// only the completion notice.  Also pins that a panicking worker still
    /// settles its handle as a panic through the existing `Disconnected` path —
    /// the flush guard preserves that semantics.
    #[test]
    fn boundary_batch_carries_body_surface_before_done() {
        let mut shell = Shell::new(Default::default());
        let batches = Arc::new(Mutex::new(Vec::new()));
        shell.turn.boundary = Some(Arc::new(RecBoundary(batches.clone())));
        let snap = Arc::new(shell.mobile().scope);

        // A worker that surfaces one card, then panics: the buffered card must
        // precede the `` `panic `` `done`, and the handle must still settle as a
        // panic when observed.
        let handle = spawn_child(snap, &mut shell, ChildIoMode::Buffered, "<block>", |child| {
            if let Some(sink) = child.turn.surface.as_ref() {
                sink.emit(&Value::Variant {
                    label: "card".into(),
                    payload: None,
                });
            }
            panic!("after surfacing");
        })
        .unwrap();

        let batch = wait_for_batch(&batches).pop().unwrap();
        assert_eq!(batch.len(), 2, "the body's card, then the `done event");
        assert_eq!(
            batch[0],
            Value::Variant {
                label: "card".into(),
                payload: None,
            },
            "the body's surface precedes the `done"
        );
        assert_eq!(done_outcome_label(&batch[1]), "panic");

        // The panicked worker dropped its `Sender` without sending, so the
        // handle settles as a failed (panic) outcome through `try_settle`'s
        // `Disconnected` arm — unchanged by the flush guard.
        match try_settle(&handle, &mut shell) {
            Some(CompletedHandle {
                outcome: Err(Break::Error(_)),
                ..
            }) => {}
            other => panic!("expected a settled panic outcome, got {other:?}"),
        }
    }

    /// With no boundary installed (the bare REPL), a completed worker flushes
    /// nothing: REPL behaviour is byte-for-byte unchanged, and the deferred
    /// surface reaches a sink only via `await`/`race`.
    #[test]
    fn no_boundary_means_no_delivery() {
        let mut shell = Shell::new(Default::default());
        assert!(shell.turn.boundary.is_none(), "a bare REPL installs none");
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(snap, &mut shell, ChildIoMode::Buffered, "<block>", |child| {
            if let Some(sink) = child.turn.surface.as_ref() {
                sink.emit(&Value::Variant {
                    label: "card".into(),
                    payload: None,
                });
            }
            Ok(Value::Unit)
        })
        .unwrap();

        // Join through `await`: the worker has run and (with no boundary) flushed
        // nothing.  The `joined` latch was never set by a boundary, so the replay
        // still surfaces the body's card through the awaiting turn — the existing
        // pull-forward path.
        let log = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<Value>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &Value) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        shell.turn.surface = Some(Arc::new(Rec(log.clone())));
        await_handle(&handle, &mut shell).expect("await ok");
        let replayed = log.lock().unwrap();
        assert_eq!(
            replayed.as_slice(),
            &[Value::Variant {
                label: "card".into(),
                payload: None,
            }],
            "no boundary appends no `done; await replays only the body's card"
        );
    }

    /// Deliver-once across the two regimes: once the boundary has delivered a
    /// batch (winning the `joined` test-and-set), a later `await` replays
    /// nothing — the shared latch suppresses the duplicate render — yet `await`
    /// still returns its cached result record.
    #[test]
    fn boundary_delivery_suppresses_a_later_await_replay() {
        let mut shell = Shell::new(Default::default());
        let batches = Arc::new(Mutex::new(Vec::new()));
        shell.turn.boundary = Some(Arc::new(RecBoundary(batches.clone())));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(snap, &mut shell, ChildIoMode::Buffered, "<block>", |child| {
            if let Some(sink) = child.turn.surface.as_ref() {
                sink.emit(&Value::Variant {
                    label: "card".into(),
                    payload: None,
                });
            }
            Ok(Value::Unit)
        })
        .unwrap();

        // The boundary wins the latch first, recording one batch.
        wait_for_batch(&batches);
        assert!(*handle.joined.lock().unwrap(), "the boundary set `joined");

        // A later `await` reads the result but finds the latch set, so it
        // replays no card into the live turn.
        let log = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<Value>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &Value) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        shell.turn.surface = Some(Arc::new(Rec(log.clone())));
        await_handle(&handle, &mut shell).expect("await still returns the result record");
        assert_eq!(
            log.lock().unwrap().len(),
            0,
            "the boundary already delivered, so the replay is suppressed"
        );
    }
}
