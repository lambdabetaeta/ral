//! Concurrency primitives: `spawn`, `watch`, `service`, `await`, `race`,
//! `cancel`.
//!
//! The handle is the evidence of detachment.  `spawn` (with its
//! host-installed siblings — the REPL's `watch`, the agent host's durable
//! `service`) reifies a [`Value::Handle`] and parks its worker under the
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
use crate::io::{Sink, new_buffer, peek_buffer, take_buffer};
use crate::serial::FOValue;
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

/// The surface a *deferred* worker installs.  Unlike a same-thread thunk body,
/// a deferred worker may outlive the turn that spawned it, so it must not hold
/// the turn's live sink: it buffers structured events into a bounded
/// [`SurfaceBuffer`].  The buffer has two ways out — `await`/`race` replay it
/// through the awaiting turn's surface (the live-frame pull), and at completion
/// the worker delivers a clone to its [`DeferredSink`] (the un-awaited
/// fallback) — gated to deliver exactly once by the handle's `joined` latch.
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
        // Past the cap (marker already recorded): drop.
    }
}

/// The final surface event a detached worker appends to its buffer at
/// completion: a `` `done `` record carrying the handle's `cmd` and the
/// worker's `outcome` — `` `ok ``, `` `err `` (the [`break_record`]), or
/// `` `panic `` (the message).  Core names the event; exarch names its
/// appearance.  It carries no return value — the model `await`s for that.
fn done_event(cmd: &str, outcome: Value) -> FOValue {
    // `outcome` is always one of the three statically-built variants below
    // (`` `ok `` over `Unit`, `` `err `` over `break_record`'s all-scalar
    // map, `` `panic `` over a `String`) — never the block's actual return
    // value, so it is provably first-order.
    let outcome = FOValue::try_from(&outcome)
        .expect("spawn outcome tag is statically first-order");
    FOValue::Variant {
        label: "done".into(),
        payload: Some(Box::new(FOValue::Map {
            entries: vec![
                (
                    "cmd".into(),
                    FOValue::String {
                        value: cmd.into(),
                    },
                ),
                ("outcome".into(), outcome),
            ],
        })),
    }
}

impl DeferredSurface {
    /// Flush this worker's deferred surface — the buffer plus a final
    /// [`done_event`] — to its [`DeferredSink`] as one batch, gated to
    /// deliver once by the shared `joined` latch.  The test-and-set lives
    /// here, at the sink's sole call site, not in the sink itself: an
    /// implementation only says where the batch goes and cannot forget the
    /// deliver-once discipline.  A no-op when no sink is installed (a bare
    /// REPL): then the deferred surface reaches a sink only via
    /// `await`/`race`.  The batch is a fresh clone so it is independent of
    /// whatever an eliminator later drains from the same buffer
    /// (`complete_handle`'s `mem::take`).
    fn flush(&self, joined: &Arc<Mutex<bool>>, cmd: &str, outcome: Value) {
        let Some(deferred) = self.deferred.as_ref() else {
            return;
        };
        // Test-and-set: only the first of delivery and replay wins the latch.
        let already = std::mem::replace(&mut *joined.lock().unwrap(), true);
        if already {
            return;
        }
        let mut batch = self.buf.lock().unwrap().clone();
        batch.push(done_event(cmd, outcome));
        deferred.deliver(batch);
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
///
/// Before returning, the handle is filed on `shell.local.workers` — every
/// spawn registers, unconditionally and with no policy attached (see
/// `types::shell::workers`).  `await`, `race`, and a settled `poll` remove
/// the entry on the shell that observes it; an explicit `cancel` does too.
/// For a [`LeaseClass::Worker`] birth under a frame that supplies a
/// [`WorkerLease`], registration is followed by arming the idle-observation
/// lease chain ([`lease_fire`]) on the fresh entry's id; a
/// [`LeaseClass::Durable`] birth registers and arms nothing — the absent
/// chain *is* the durable policy, not an exemption the chain checks.
///
/// Under a frame that supplies a `worker_cap`, admission is reserved
/// first: a birth of any class is refused while `cap` workers are already
/// running or reserved, with an error naming the remedies (`await`,
/// `cancel`). The reservation is held across the thread spawn
/// and handle construction below and only released — into the registered
/// entry it was reserved for — at the `register` call, so a sibling birth
/// racing this one on another thread can never observe the seat this one
/// is still filling as free.
pub(super) fn spawn_child<F>(
    snap: Arc<Env>,
    shell: &mut Shell,
    io_mode: ChildIoMode,
    class: LeaseClass,
    cmd: &str,
    work: F,
) -> Settled<HandleInner>
where
    F: FnOnce(&mut Shell) -> Raw<Value> + Send + 'static,
{
    // Admission: under a frame that caps live workers, the birth reserves
    // its seat at the door — before any thread exists or any entry
    // registers, so a rejected spawn leaves no trace, and a granted
    // reservation counts toward the cap the instant it is granted, closing
    // the gap a plain check-then-register would leave for a sibling spawn
    // racing this one on another thread.  Only still-running entries (and
    // other reservations in flight) count, durable services included (live
    // work is live work); settled entries lingering under retention never
    // block admission.  The reservation travels with this call from here
    // to the `register` call near the end; every early return in between —
    // the `Watch` arm's clone failure included — releases it through
    // `Reservation`'s own drop.
    let reservation = match shell.local.workers.reserve(shell.turn.worker_cap) {
        Ok(reservation) => reservation,
        Err(CapReached(cap)) => {
            return Err(sig(format!(
                "spawn: {cap} workers already live on this agent; \
                 await or cancel one"
            )));
        }
    };

    let (tx, rx) = std::sync::mpsc::channel();

    // Allocate buffers and build the child's sinks.  Buffered mode writes
    // into the shared byte buffers; watch mode wraps clones of the parent
    // stdout in `Sink::LineFramed` with a per-handle prefix.
    let (stdout_sink, stdout_buf) = new_buffer();
    let (stderr_sink, stderr_buf) = new_buffer();
    // The deferred worker buffers `surface` calls here rather than holding the
    // spawning turn's live sink; `await`/`race` replay it once and the worker
    // delivers a clone to the deferred sink at completion.
    let surface_buf: SurfaceBuffer = Arc::new(Mutex::new(Vec::new()));
    // The session-lived deferred sink the worker delivers to at completion,
    // captured from the spawning turn so it survives that turn's teardown and
    // a nested `spawn` inside the worker inherits it.  The worker's
    // `DeferredSurface` holds both the buffer and this destination; the
    // `joined` latch is shared with the eliminators so whichever renders
    // first wins the deliver-once test.
    let deferred = shell.turn.deferred.clone();
    let worker_surface = Arc::new(DeferredSurface {
        buf: surface_buf.clone(),
        deferred: deferred.clone(),
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

    // Minted before the thread so the worker can hold a clone: at exit it
    // marks itself `Completed`, the fact the lease chain reads to end
    // silently on a finished worker instead of reaping it.
    let state = Arc::new(Mutex::new(HandleState::Running));
    let worker_state = state.clone();

    let (_join, cancel) = shell.spawn_thread(snap, move |child_env| {
        child_env.turn.io.capture_outer = None;
        child_env.turn.io.stdout = stdout;
        child_env.turn.io.stderr = stderr;
        // A detached worker is a background computation with no terminal: its
        // stdout/stderr are buffers, and its stdin must not fall through to
        // fd 0.  `spawn_thread` builds the worker from a defaulted `Io`
        // (`Source::Terminal`), so without this an external in the body — a
        // `cargo test` exercising signal code, say — would inherit the real
        // terminal and could `tcgetpgrp(stdin)` / `kill(-fg, …)` whoever owns
        // it.  `Empty` wires fd 0 to `/dev/null`.
        child_env.turn.io.stdin = crate::io::Source::Empty;
        // The deferred sink flows onto the worker's turn so a nested `spawn`
        // inside the body installs its own `DeferredSurface` with the same
        // sink and delivers at *its* own completion.
        child_env.turn.deferred = deferred;
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
        // The worker's own settle mark, strictly *after* the send so
        // `Completed` always implies an observable outcome in the channel
        // — an eliminator that reads the state mid-transition can still
        // settle.  Guarded: an eliminator's `complete_handle` may have won
        // the transition already, and a `cancel`'s `Cancelled` must never
        // be overwritten.  A panicking body never reaches here; its state
        // stays `Running` until an observer settles the disconnect as a
        // panic, or the lease chain reaps the dead thread's scope.
        let mut settled_state = worker_state.lock().unwrap();
        if *settled_state == HandleState::Running {
            *settled_state = HandleState::Completed;
        }
    });

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
    // Every admitted spawn registers, unconditionally — the mechanism is
    // universal and attaches no further policy; affordances (listing,
    // reaping) are the host's and the lease layer's concern, not this
    // door's.  Consuming `reservation` here is what turns the seat
    // `reserve` held into the entry it was held for.
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

    // For an ordinary worker under a frame that grants a lease (exarch),
    // arm the idle-observation chain on the freshly registered entry.  The
    // chain is fire-and-forget (`keep()`-ed): the worker outlives this
    // `spawn` call, and every firing either ends the chain or re-arms
    // exactly one successor.  Registered-then-armed, so the id the chain
    // reaps always names an entry that existed.  The interactive frame
    // (the REPL) supplies no lease and never reaps; a durable birth arms
    // no chain at all — no reaper entry ever exists for it, which is the
    // whole durable policy.
    if class == LeaseClass::Worker
        && let Some(lease) = shell.turn.deferred_lease
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
        crate::process::arm_callback(lease.idle, move || lease_fire(chain)).keep();
    }
    Ok(handle)
}

/// Everything one firing of the idle-observation lease chain needs, cloned
/// forward into each re-arm.  Deliberately a bundle of shared cells and
/// `Copy` facts — never a `Shell` — so the chain stays cheap to clone and
/// cheap to run on the reaper daemon thread.
#[derive(Clone)]
struct LeaseChain {
    /// The worker's own cancel scope: what a reap fires.
    scope: crate::process::CancelScope,
    /// The handle's lifecycle cell: a non-`Running` worker ends the chain.
    state: Arc<Mutex<HandleState>>,
    /// The shared last-observation cell the eliminators renew.
    last_observed: Arc<Mutex<std::time::Instant>>,
    /// The worker's spawn instant — the backstop's clock.  The registry
    /// entry's `SystemTime` field stays display-only.
    started: std::time::Instant,
    lease: WorkerLease,
    /// The owning shell's registry, for the reap bookkeeping.
    registry: WorkerRegistry,
    id: WorkerId,
}

/// One firing of a worker's lease chain, on the reaper daemon thread.
///
/// A worker no longer `Running` ends the chain silently — a settled entry
/// lingers in the registry as an unclaimed result, a cancelled one is
/// already gone; no cancel, no notice, no re-arm.  A running worker is
/// reaped at the backstop (age from spawn) first, then at the idle bound
/// (time since an eliminator last named the handle); otherwise the chain
/// re-arms itself for exactly the sooner of the two remaining margins.
///
/// A reap does the registry bookkeeping *before* firing the scope — the
/// cancel of an already-settled scope is a harmless monotone `fetch_max`,
/// so ordering the ledger first keeps the entry-and-notice state ahead of
/// any observable cancellation.  It deliberately does not detach the
/// handle: the body unwinds with status 130 and settles as an error, so a
/// later `poll`/`await` still observes the partial output and the failure
/// — only the registry entry (plus its notice) records the reap.  Cheap
/// and non-blocking per the reaper's contract: it takes only the handle
/// cells and, briefly, the registry lock.
fn lease_fire(chain: LeaseChain) {
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
        let next = std::cmp::min(chain.lease.idle - idle, chain.lease.backstop - age);
        let rearm = chain.clone();
        crate::process::arm_callback(next, move || lease_fire(rearm)).keep();
    }
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
        LeaseClass::Worker,
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
        LeaseClass::Worker,
        "<watch>",
        // The worker body is the sole computation of a fresh thread,
        // under the trivial continuation the thread's join provides;
        // `spawn_child` absorbs any terminal tail call on that thread.
        move |child_env| with_scope(child_env, |s| eval_comp(&body, s, Tail::Yes)),
    )?))
}

// ── service ──────────────────────────────────────────────────────────────

/// `service <desc> <thunk>` -- birth a durable worker: an ordinary buffered
/// spawn except for its [`LeaseClass::Durable`] registration — no idle reap,
/// no backstop.  Its bound is legibility, not time: `desc` is mandatory,
/// becomes the worker's `cmd` in the registry, and is what a host's own
/// ledger shows for as long as the service lives.  Cancellable through its
/// handle, dead with `/clear` or the process (`decisions/260705_leases-and-budgets`).
///
/// Availability is the host's, the mirror image of `watch`: an agent host
/// (exarch), whose lease frame would otherwise reap long work, installs it
/// via [`crate::builtins::SERVICE_BUILTIN`]; the interactive/batch ral
/// hosts leave it uninstalled — they grant no lease, so every one of their
/// spawns already lives until cancel or exit and a durable class would
/// distinguish nothing.
pub(super) fn builtin_service(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "service")?;
    let desc = match &args[0] {
        Value::String(s) => s.trim().to_string(),
        other => {
            return Err(sig(format!(
                "service: description must be a String, got {}",
                other.type_name()
            )));
        }
    };
    if desc.is_empty() {
        return Err(sig("service: description must be non-empty".to_string()));
    }
    if desc.contains('\n') {
        return Err(sig(
            "service: description must be a single line (no newlines)".to_string(),
        ));
    }
    let (body, captured) = expect_thunk(&args[1], "service")?;
    Ok(Value::Handle(spawn_child(
        captured,
        shell,
        ChildIoMode::Buffered,
        LeaseClass::Durable,
        &desc,
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
    shell.local.workers.remove(handle);
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
            // A blocked `await`/`race` is continuous observation: each
            // sweep renews every named handle's idle lease, so a worker
            // being waited on is never idle-reaped mid-wait.
            *handle.last_observed.lock().unwrap() = std::time::Instant::now();
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
    shell.local.workers.remove(handle);
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
/// - `` `pending `` carrying `{stdout, stderr}` — the bytes the block has
///   written *so far* — while it is still running.  These are a cumulative,
///   non-destructive snapshot ([`peek_buffer`], not [`take_buffer`]): the
///   buffers are left intact for the one-shot completion drain, so a partial
///   poll never steals bytes a later `await`/`poll` must still see.  A watched
///   handle's buffers stay empty (bytes flow live through `Sink::LineFramed`),
///   so a pending `poll` on one reports empty.  Because the snapshot grows as
///   the worker writes, repeated pending polls are non-idempotent — see
///   `decisions/260702_partial-poll-pending-output`.
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
    // Both samples are observations — a pending poll and a settled one
    // each name the handle — so the touch lands once at entry, before the
    // settle attempt decides which arm it is.
    *handle.last_observed.lock().unwrap() = std::time::Instant::now();
    let variant = |label: &str, payload| Value::Variant {
        label: label.into(),
        payload,
    };
    let result = match try_settle(handle, shell) {
        Some(completed) => {
            // A settled poll is an observation like `await`'s, so it
            // removes the entry too; a `pending` sample below must not.
            shell.local.workers.remove(handle);
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
        None => {
            // A cumulative, non-destructive snapshot of what the running
            // worker has written so far: `peek_buffer` clones, leaving the
            // buffers for `complete_handle`'s one-shot `take_buffer` drain.
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
        }
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
    shell.local.workers.remove(winner);
    for &h in &handles {
        if !Arc::ptr_eq(&h.result, &winner.result) {
            h.cancel.cancel(crate::process::CancelCause::Explicit);
            detach_handle(h);
            shell.local.workers.remove(h);
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
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
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

    /// Destructure an `` `label payload `` [`FOValue`] variant, asserting the
    /// label — the [`FOValue`] dual of [`expect_variant`], for the boundary's
    /// deferred-surface batches.
    fn fo_expect_variant<'a>(v: &'a FOValue, label: &str) -> &'a FOValue {
        match v {
            FOValue::Variant {
                label: l,
                payload: Some(p),
            } if l == label => p,
            other => panic!("expected `{label} with payload, got {other:?}"),
        }
    }

    /// Destructure an [`FOValue::Map`]'s entries and look one up by key.
    fn fo_map_get<'a>(v: &'a FOValue, key: &str) -> Option<&'a FOValue> {
        match v {
            FOValue::Map { entries } => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
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
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
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

    /// A millisecond-scale [`WorkerLease`] for the timing tests.
    fn lease_ms(idle: u64, backstop: u64) -> WorkerLease {
        WorkerLease {
            idle: std::time::Duration::from_millis(idle),
            backstop: std::time::Duration::from_millis(backstop),
        }
    }

    /// A worker body that stays `Running` until cancelled: it polls
    /// `process::check` so a lease reap genuinely unwinds the thread (with
    /// the cancel's 130), not merely flags a scope nobody reads.
    fn check_loop(child: &mut Shell) -> Raw<Value> {
        loop {
            crate::process::check(child)?;
            std::thread::yield_now();
        }
    }

    /// Block until `handle`'s worker has marked itself `Completed` at exit
    /// — the precondition a retention test needs before an epoch sweep can
    /// observe the entry settled.
    fn wait_settled(handle: &HandleInner) {
        for _ in 0..500 {
            if *handle.state.lock().unwrap() == HandleState::Completed {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("worker never marked itself Completed");
    }

    /// A `spawn` under an agent frame's lease is reaped once *unobserved*
    /// for the idle bound: the blocked, never-polled worker's own scope is
    /// force-cancelled with `Deadline`, its registry entry is removed, and
    /// exactly one `Idle` notice — carrying the entry's id, cmd, and class
    /// — awaits the host's drain, which empties the ledger.  The body is a
    /// `process::check` loop, so the reap actually unwinds the thread.
    #[test]
    fn unobserved_worker_is_reaped_at_its_idle_lease() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(40, 10_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<abandoned>",
            check_loop,
        )
        .expect("spawn must succeed");
        let entry = shell.local.workers.snapshot().pop().expect("registered");
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

    /// A `spawn` under the interactive frame arms no lease and admits
    /// freely: the worker's scope is never reaped on a timer, its settled
    /// registry entry lingers unstamped (the REPL never calls the epoch
    /// sweep), and no notice of any cause is ever recorded.
    #[test]
    fn spawn_under_interactive_frame_arms_no_lease() {
        let mut shell = Shell::new(Default::default());
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

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

    /// A stub desk that always answers `Ok` with its request unchanged —
    /// enough to prove a worker either reaches it or does not.
    struct EchoDesk;
    impl crate::types::EnquiryDesk for EchoDesk {
        fn enquire(
            &self,
            req: crate::serial::FOValue,
        ) -> Result<crate::serial::FOValue, crate::types::Error> {
            Ok(req)
        }
    }

    /// Containment (§3 of the enquiry-channel ADR): a detached worker never
    /// receives the spawning turn's enquiry desk, even though one is
    /// installed on the spawning turn. The worker's own `enquire` call must
    /// answer the honest absence error, never reach `EchoDesk`, and never
    /// park.
    #[test]
    fn spawned_worker_never_receives_the_enquiry_desk() {
        let mut shell = Shell::new(Default::default());
        shell.turn.desk = Some(Arc::new(EchoDesk) as crate::types::Desk);
        let snap = Arc::new(shell.mobile().scope);
        let (tx, rx) = mpsc::channel::<Result<crate::serial::FOValue, crate::types::Error>>();
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test>",
            move |child| {
                let outcome = child.enquire(crate::serial::FOValue::Unit);
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
            Err(e) => assert_eq!(e.message, "this host answers no enquiries"),
            Ok(_) => panic!("a detached worker must never reach the spawning turn's desk"),
        }
    }

    /// Observation renews the idle lease: a worker polled every ~20 ms
    /// under a 200 ms idle bound survives to ~3× that bound — each `poll`
    /// touches `last_observed`, so the chain keeps re-arming instead of
    /// reaping — and is then gated to completion and awaited normally.
    #[test]
    fn polled_worker_survives_past_its_idle_lease() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(200, 10_000));
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<babysat>",
            move |_c| {
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
        await_handle(&handle, &mut shell).expect("await after the gate opens");
        assert!(!scope.is_cancelled(), "the worker finished by itself");
    }

    /// The backstop is absolute: ritual polling renews the idle bound but
    /// cannot extend a worker past `backstop`, so a worker polled every
    /// ~20 ms under idle 150 ms / backstop 400 ms is reaped anyway — with
    /// the `Backstop` cause — once its age crosses the line.
    #[test]
    fn backstop_reaps_a_ritually_polled_worker() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(150, 400));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
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
            // A poll may race the reap and observe the cancelled body
            // settling as an error; the sample's outcome is irrelevant
            // here — only the touch is the point.
            let _ = builtin_poll(&[Value::Handle(handle.clone())], &mut shell);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(scope.cause(), Some(crate::process::CancelCause::Deadline));
        assert_eq!(shell.local.workers.count(), 0);
        let notices = shell.take_worker_reap_notices();
        assert_eq!(notices.len(), 1, "one notice for the backstop reap");
        assert_eq!(notices[0].cause, ReapCause::Backstop);
    }

    /// A worker that completed but was never observed is not reaped: its
    /// exit mark ends the chain at the state check, so its scope is never
    /// cancelled, its entry lingers in the registry as an unclaimed
    /// result, and no notice is recorded.
    #[test]
    fn completed_unobserved_worker_is_not_reaped() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(100, 10_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<done>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

        // The instantly-returning body marks itself `Completed` at exit;
        // wait for that mark (it rides the worker thread), then let ~3
        // idle bounds elapse so the chain has demonstrably fired and ended.
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

    /// Enumeration is not observation: a worker listed every ~10 ms under
    /// a 40 ms idle bound is reaped anyway — `workers()` / `worker_count()`
    /// touch nothing — so the lease is renewed only by the eliminators.
    #[test]
    fn listing_does_not_renew_the_lease() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(40, 10_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<listed>",
            check_loop,
        )
        .expect("spawn must succeed");
        let scope = handle.cancel.clone();

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

    /// The durable class is the whole difference: under one
    /// millisecond-scale lease frame, a `Durable` birth arms no chain and
    /// outlives both bounds — unpolled past the idle bound, older than the
    /// backstop — while its ordinary-class sibling, spawned under the very
    /// same frame, is reaped.  Exactly one notice results, and it names the
    /// sibling, never the durable worker.
    #[test]
    fn durable_worker_outlives_both_lease_bounds_while_its_sibling_reaps() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(40, 150));

        let snap = Arc::new(shell.mobile().scope);
        let durable = spawn_child(
            snap,
            &mut shell,
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
            &mut shell,
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
        // Let the durable worker age well past both bounds, unobserved.
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

        // End the blocked worker so the test does not leak a live thread.
        durable.cancel.cancel(crate::process::CancelCause::Explicit);
    }

    /// Explicit destruction still reaches a durable worker: `cancel`
    /// through the handle fires its scope and removes its registry entry,
    /// exactly as for an ordinary worker — durability exempts the lease
    /// chain, never the eliminators.
    #[test]
    fn cancel_through_the_handle_ends_a_durable_worker() {
        let mut shell = Shell::new(Default::default());
        shell.turn.deferred_lease = Some(lease_ms(10_000, 20_000));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
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

    /// Run `src` as one capturing top-level turn with `SERVICE_BUILTIN`
    /// installed, returning the runtime result. Panics on a parse/type
    /// failure — every source these tests run is expected to compile.
    fn run_service_source(shell: &mut Shell, src: &str) -> Settled<Value> {
        use crate::transport::{Program, Turn};
        use crate::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};
        let req = TurnRequest {
            turn: Turn {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: TurnStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            lifecycle: Box::new(()),
        };
        match shell.run_turn(req) {
            TurnReport::Ran { result, .. } => result,
            TurnReport::Static { .. } => {
                panic!("well-formed source must run, not fail statically: {src:?}")
            }
        }
    }

    fn service_test_shell() -> Shell {
        let mut shell = Shell::new(Default::default());
        crate::builtins::register_builtins(crate::builtins::SERVICE_BUILTIN);
        shell.install_builtins(crate::builtins::SERVICE_BUILTIN);
        shell
    }

    /// An empty (or whitespace-only, after trim) description is refused:
    /// it is the whole legibility bound a durable birth declares, so it
    /// cannot be absent.
    #[test]
    fn service_rejects_an_empty_description() {
        let mut shell = service_test_shell();
        let err = run_service_source(&mut shell, r#"service "   " { 1 }"#)
            .expect_err("an empty description must be refused");
        assert_eq!(status(err), 1);
    }

    /// A multi-line description is refused: it is a one-line ledger label,
    /// not a paragraph.
    #[test]
    fn service_rejects_a_multiline_description() {
        let mut shell = service_test_shell();
        let err = run_service_source(&mut shell, "service \"one\ntwo\" { 1 }")
            .expect_err("a multiline description must be refused");
        assert_eq!(status(err), 1);
    }

    /// A valid description lands verbatim (trimmed) as the registry
    /// entry's `cmd` — what a host's own ledger shows for the service.
    #[test]
    fn service_description_lands_in_the_registry_entry() {
        let mut shell = service_test_shell();
        let handle = match run_service_source(&mut shell, r#"service "  watch the thing  " { 1 }"#)
        {
            Ok(Value::Handle(h)) => h,
            other => panic!("service must return a Handle, got {other:?}"),
        };
        let entries = shell.workers();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].class, LeaseClass::Durable);
        assert_eq!(entries[0].cmd, "watch the thing", "the description trims");
        handle.cancel.cancel(crate::process::CancelCause::Explicit);
    }

    /// A detached worker's `surface` events are buffered, not emitted live,
    /// and replay through the *awaiting* turn's surface exactly once: `poll`
    /// never replays, the first `await` replays, and a second `await` does
    /// not duplicate.  Models `spawn { surface … }` then observing the handle.
    #[test]
    fn deferred_surface_replays_once_on_await_not_poll() {
        struct Rec(Arc<Mutex<Vec<FOValue>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &FOValue) {
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

    /// A deferred-sink test double standing in for the agent host: it records
    /// every delivered batch.  The deliver-once test-and-set now lives at
    /// `DeferredSurface::flush`'s call site, so the sink itself just records
    /// whatever it is handed.
    struct RecDeferred(Arc<Mutex<Vec<Vec<FOValue>>>>);

    impl DeferredSink for RecDeferred {
        fn deliver(&self, batch: Vec<FOValue>) {
            self.0.lock().unwrap().push(batch);
        }
    }

    /// Spin until the deferred sink has recorded a batch (the worker flushed) or
    /// the budget elapses, returning the recorded batches.  A `spawn_child`
    /// worker runs on its own thread, so its completion flush is observed by
    /// waiting on the destination rather than on the result channel.
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

    /// The `outcome` label inside a batch's trailing `` `done `` event.  Pins
    /// the structural shape the exarch decoder matches: `` `done `` carries a
    /// `{cmd, outcome}` map, and `outcome` is a closed `` `ok ``/`` `err ``/
    /// `` `panic `` variant.
    fn done_outcome_label(done: &FOValue) -> String {
        let done = fo_expect_variant(done, "done");
        match fo_map_get(done, "outcome").expect("outcome field") {
            FOValue::Variant { label, .. } => label.to_string(),
            other => panic!("outcome must be a variant, got {other:?}"),
        }
    }

    /// Install a deferred sink and `spawn_child` a worker; on completion the
    /// worker flushes its buffer plus a trailing `` `done `` event to the sink
    /// as one batch.  This is the un-awaited delivery the ADR adds: a fire-and-forget
    /// worker reaches a sink with no eliminator at all.  Each outcome — clean
    /// return, raised `Err`, panic — stamps the matching `done` label.
    #[test]
    fn detached_worker_flushes_done_to_deferred_sink() {
        fn run(work: impl FnOnce(&mut Shell) -> Raw<Value> + Send + 'static) -> Vec<FOValue> {
            let mut shell = Shell::new(Default::default());
            let batches = Arc::new(Mutex::new(Vec::new()));
            shell.turn.deferred = Some(Arc::new(RecDeferred(batches.clone())));
            let snap = Arc::new(shell.mobile().scope);
            // Hold the handle (and its receiver) so the channel stays connected
            // until the worker has flushed; never observed, so no eliminator
            // competes for the `joined` latch.
            let _handle = spawn_child(
                snap,
                &mut shell,
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

        // Clean return: a `` `done `` whose outcome is `` `ok ``, carrying the
        // handle's cmd.
        let ok = run(|_child| Ok(Value::Unit));
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

        // Raised `Err`: a `` `done `` whose outcome is `` `err ``.
        let err = run(|_child| Err(sig("boom").into()));
        assert_eq!(done_outcome_label(&err[0]), "err");

        // Panic: the guard fires on the unwind with a `` `panic `` outcome.
        let panicked = run(|_child| panic!("worker exploded"));
        assert_eq!(done_outcome_label(&panicked[0]), "panic");
    }

    /// The body's own `surface`/`io` values appear in the batch *before* the
    /// trailing `` `done ``: the deferred batch carries the full deferred
    /// surface, not only the completion notice.  Also pins that a panicking
    /// worker still settles its handle as a panic through the existing
    /// `Disconnected` path — the flush guard preserves that semantics.
    #[test]
    fn deferred_batch_carries_body_surface_before_done() {
        let mut shell = Shell::new(Default::default());
        let batches = Arc::new(Mutex::new(Vec::new()));
        shell.turn.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let snap = Arc::new(shell.mobile().scope);

        // A worker that surfaces one card, then panics: the buffered card must
        // precede the `` `panic `` `done`, and the handle must still settle as a
        // panic when observed.
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<block>",
            |child| {
                if let Some(sink) = child.turn.surface.as_ref() {
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

    /// With no deferred sink installed (the bare REPL), a completed worker
    /// flushes nothing: REPL behaviour is byte-for-byte unchanged, and the
    /// deferred surface reaches a sink only via `await`/`race`.
    #[test]
    fn no_deferred_sink_means_no_delivery() {
        let mut shell = Shell::new(Default::default());
        assert!(shell.turn.deferred.is_none(), "a bare REPL installs none");
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<block>",
            |child| {
                if let Some(sink) = child.turn.surface.as_ref() {
                    sink.emit(&FOValue::Variant {
                        label: "card".into(),
                        payload: None,
                    });
                }
                Ok(Value::Unit)
            },
        )
        .unwrap();

        // Join through `await`: the worker has run and (with no deferred sink)
        // flushed nothing.  The `joined` latch was never set by a delivery, so
        // the replay still surfaces the body's card through the awaiting turn
        // — the existing pull-forward path.
        let log = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<FOValue>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        shell.turn.surface = Some(Arc::new(Rec(log.clone())));
        await_handle(&handle, &mut shell).expect("await ok");
        let replayed = log.lock().unwrap();
        assert_eq!(
            replayed.as_slice(),
            &[FOValue::Variant {
                label: "card".into(),
                payload: None,
            }],
            "no deferred sink appends no `done; await replays only the body's card"
        );
    }

    /// Deliver-once across the two regimes: once the deferred sink has
    /// delivered a batch (winning the `joined` test-and-set), a later `await`
    /// replays nothing — the shared latch suppresses the duplicate render —
    /// yet `await` still returns its cached result record.
    #[test]
    fn deferred_delivery_suppresses_a_later_await_replay() {
        let mut shell = Shell::new(Default::default());
        let batches = Arc::new(Mutex::new(Vec::new()));
        shell.turn.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<block>",
            |child| {
                if let Some(sink) = child.turn.surface.as_ref() {
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
        assert!(*handle.joined.lock().unwrap(), "the deferred flush set `joined");

        // A later `await` reads the result but finds the latch set, so it
        // replays no card into the live turn.
        let log = Arc::new(Mutex::new(Vec::new()));
        struct Rec(Arc<Mutex<Vec<FOValue>>>);
        impl EventSink for Rec {
            fn emit(&self, ev: &FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        shell.turn.surface = Some(Arc::new(Rec(log.clone())));
        await_handle(&handle, &mut shell).expect("await still returns the result record");
        assert_eq!(
            log.lock().unwrap().len(),
            0,
            "the deferred sink already delivered, so the replay is suppressed"
        );
    }

    // ── worker registry (pure bookkeeping, no policy) ────────────────────

    /// `spawn_child` files exactly one entry, carrying the spawn's own `cmd`
    /// and the (only) `Worker` lease class, and the registered handle is
    /// the same handle the call returns — `HandleInner`'s own `PartialEq`
    /// (`Arc::ptr_eq` on `result`) proves it, not a field-by-field copy.
    #[test]
    fn spawn_child_registers_one_entry_with_matching_handle() {
        let mut shell = Shell::new(Default::default());
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<test-cmd>",
            |_child| Ok(Value::Unit),
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

    /// The mechanism attaches no policy and is host-independent: a bare
    /// `Shell::new(Default::default())` with no `deferred_lease` granted
    /// (the REPL/interactive shape) registers exactly as the agent-framed
    /// case above does.
    #[test]
    fn spawn_child_registers_with_no_deferred_lease_granted() {
        let mut shell = Shell::new(Default::default());
        assert!(
            shell.turn.deferred_lease.is_none(),
            "precondition: no lease granted"
        );
        let snap = Arc::new(shell.mobile().scope);
        let handle = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<repl>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        assert_eq!(shell.local.workers.count(), 1);
        assert_eq!(shell.local.workers.snapshot()[0].handle, handle);
    }

    /// Every foreground eliminator that observes a settled worker removes
    /// its registry entry — `await`, `cancel`, and a `` `settled `` `poll` —
    /// while a `` `pending `` `poll` leaves the entry alone: listing and
    /// sampling must never mutate the registry, only an actual observation
    /// of a finished (or explicitly cancelled) worker may.
    #[test]
    fn eliminators_remove_the_entry_except_a_pending_poll() {
        let mut shell = Shell::new(Default::default());

        // `await` removes.
        let snap = Arc::new(shell.mobile().scope);
        let h1 = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<a>",
            |_c| Ok(Value::Unit),
        )
        .unwrap();
        await_handle(&h1, &mut shell).expect("await ok");
        assert_eq!(shell.local.workers.count(), 0, "await removes its entry");

        // `cancel` removes.
        let snap = Arc::new(shell.mobile().scope);
        let h2 = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<b>",
            |_c| Ok(Value::Unit),
        )
        .unwrap();
        assert_eq!(shell.local.workers.count(), 1);
        builtin_cancel(&[Value::Handle(h2)], &mut shell).expect("cancel ok");
        assert_eq!(shell.local.workers.count(), 0, "cancel removes its entry");

        // A settled `poll` removes.
        let snap = Arc::new(shell.mobile().scope);
        let h3 = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<c>",
            |_c| Ok(Value::Unit),
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

        // A pending `poll` does not touch the registry: block the worker on
        // its own channel so the sample is deterministically `` `pending ``,
        // no timing guess needed.
        let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let h4 = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<d>",
            move |_c| {
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
        await_handle(&h4, &mut shell).expect("await ok");
    }

    /// `race` removes both the winner (a settled observation) and every
    /// cancelled loser (the same cancel-and-detach the loop already
    /// performs for each) — nothing lingers in the registry once `race`
    /// returns.
    #[test]
    fn race_removes_winner_and_cancelled_losers() {
        let mut shell = Shell::new(Default::default());
        let snap = Arc::new(shell.mobile().scope);
        let winner = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<winner>",
            |_c| Ok(Value::Unit),
        )
        .unwrap();

        // The losers block on their own channels, so `race` always finds
        // the winner settled first and cancels these two.
        let (l1_tx, l1_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let loser1 = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<loser1>",
            move |_c| {
                let _ = l1_rx.recv();
                Ok(Value::Unit)
            },
        )
        .unwrap();
        let (l2_tx, l2_rx) = mpsc::channel::<()>();
        let snap = Arc::new(shell.mobile().scope);
        let loser2 = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<loser2>",
            move |_c| {
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
        builtin_race(&args, &mut shell).expect("race must succeed");
        assert_eq!(
            shell.local.workers.count(),
            0,
            "race removes the winner and both cancelled losers"
        );

        // Let the cancelled-but-still-blocked loser threads finish so the
        // test doesn't leave them parked past its own end.
        let _ = l1_tx.send(());
        let _ = l2_tx.send(());
    }

    /// The flow rule: the registry `Arc` flows into a spawned worker's own
    /// shell (`Shell::spawn_thread`), so a `spawn` nested inside a worker's
    /// body registers into the *same* registry the outer, owning shell
    /// reads — not a fresh, private one of its own.
    ///
    /// The worker gates on `go_rx` before doing anything: `spawn_child`
    /// starts the thread *before* it files the outer entry on the spawning
    /// shell, so an ungated body could sample the registry ahead of that
    /// registration and observe one entry instead of two.  The parent
    /// opens the gate only after its `spawn_child` call has returned —
    /// the outer entry is then guaranteed filed — making the worker's
    /// sample deterministic.  Production order needs no such promise:
    /// parent and nested registrations are deliberately unordered.
    #[test]
    fn nested_spawn_registers_into_the_owning_shells_registry() {
        let mut shell = Shell::new(Default::default());
        let snap = Arc::new(shell.mobile().scope);
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let _outer = spawn_child(
            snap,
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<outer>",
            move |child_shell| {
                go_rx.recv().unwrap();
                let child_snap = Arc::new(child_shell.mobile().scope);
                let _inner = spawn_child(
                    child_snap,
                    child_shell,
                    ChildIoMode::Buffered,
                    LeaseClass::Worker,
                    "<inner>",
                    |_c| Ok(Value::Unit),
                )
                .unwrap();
                // Sent right after the nested spawn registers, with the
                // outer entry already filed (the gate above), so the count
                // sampled on the worker's own (child) shell is exact.
                ready_tx.send(child_shell.local.workers.count()).unwrap();
                Ok(Value::Unit)
            },
        )
        .unwrap();
        // The outer `spawn_child` has returned, so its entry is filed;
        // release the worker to spawn its nested child and sample.
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

    /// The retention ledger in integers: a settled, unclaimed entry is
    /// stamped at the first sweep that observes it settled, kept while
    /// `epoch − stamp < retention`, and expired — one `Retention` notice
    /// carrying its facts — at the call the bound is met.
    #[test]
    fn retention_stamps_then_expires_an_unclaimed_settled_entry() {
        let mut shell = Shell::new(Default::default());
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<done>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        wait_settled(&handle);

        shell.advance_worker_epoch(5, 256);
        let stamped = shell.local.workers.snapshot();
        assert_eq!(stamped.len(), 1, "a swept settled entry lingers");
        assert_eq!(
            stamped[0].settled_epoch,
            Some(5),
            "stamped at the first sweep that observes it settled"
        );

        shell.advance_worker_epoch(260, 256);
        assert_eq!(shell.local.workers.count(), 1, "260 − 5 < 256: retained");
        assert_eq!(
            shell.local.workers.snapshot()[0].settled_epoch,
            Some(5),
            "the stamp is first-observed-settled, never re-stamped"
        );

        shell.advance_worker_epoch(261, 256);
        assert_eq!(shell.local.workers.count(), 0, "261 − 5 ≥ 256: expired");
        let notices = shell.take_worker_reap_notices();
        assert_eq!(notices.len(), 1, "one notice per retention expiry");
        assert_eq!(notices[0].id, stamped[0].id);
        assert_eq!(notices[0].cmd, stamped[0].cmd);
        assert_eq!(notices[0].class, stamped[0].class);
        assert_eq!(notices[0].cause, ReapCause::Retention);
    }

    /// Observation beats retention: an entry the sweep has stamped is
    /// removed the moment a settled `poll` claims it, so a later sweep past
    /// the bound finds nothing to expire and records no notice.
    #[test]
    fn observation_beats_retention() {
        let mut shell = Shell::new(Default::default());
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<claimed>",
            |_child| Ok(Value::Unit),
        )
        .expect("spawn must succeed");
        wait_settled(&handle);

        shell.advance_worker_epoch(1, 256);
        assert_eq!(shell.local.workers.snapshot()[0].settled_epoch, Some(1));

        builtin_poll(&[Value::Handle(handle.clone())], &mut shell).expect("poll ok");
        assert_eq!(
            shell.local.workers.count(),
            0,
            "a settled poll claims the entry"
        );

        shell.advance_worker_epoch(400, 256);
        assert!(
            shell.take_worker_reap_notices().is_empty(),
            "a claimed result leaves no retention notice"
        );
    }

    /// A still-running entry is never stamped or expired: the sweep leaves
    /// live work alone even at retention 0 and any epoch distance —
    /// retention is a settled entry's lease, not a second bound on running
    /// workers.
    #[test]
    fn running_entries_are_never_stamped_or_expired() {
        let mut shell = Shell::new(Default::default());
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let handle = spawn_child(
            Arc::new(shell.mobile().scope),
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<live>",
            move |_c| {
                gate_rx.recv().unwrap();
                Ok(Value::Unit)
            },
        )
        .expect("spawn must succeed");

        shell.advance_worker_epoch(5, 0);
        shell.advance_worker_epoch(1_000_000, 0);
        let snapshot = shell.local.workers.snapshot();
        assert_eq!(snapshot.len(), 1, "live work is never expired");
        assert_eq!(
            snapshot[0].settled_epoch, None,
            "live work is never stamped"
        );
        assert!(shell.take_worker_reap_notices().is_empty());

        gate_tx.send(()).unwrap();
        await_handle(&handle, &mut shell).expect("await after the gate opens");
    }

    // ── the admission cap ────────────────────────────────────────────────

    /// The cap refuses the (cap+1)th birth at the door: with two gated
    /// workers running under `worker_cap: Some(2)`, a third spawn errors —
    /// naming `await` and `cancel` as the remedies — and registers
    /// nothing; cancelling one frees a seat, and the next birth is
    /// admitted.
    #[test]
    fn worker_cap_rejects_at_the_door_and_frees_on_cancel() {
        let mut shell = Shell::new(Default::default());
        shell.turn.worker_cap = Some(2);

        let mut gates = Vec::new();
        let mut handles = Vec::new();
        for cmd in ["<one>", "<two>"] {
            let (gate_tx, gate_rx) = mpsc::channel::<()>();
            let handle = spawn_child(
                Arc::new(shell.mobile().scope),
                &mut shell,
                ChildIoMode::Buffered,
                LeaseClass::Worker,
                cmd,
                move |_c| {
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
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<three>",
            |_c| Ok(Value::Unit),
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
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<after>",
            |_c| Ok(Value::Unit),
        )
        .expect("cancelling one frees a seat");

        // Unblock the parked workers so no thread outlives the test.
        for gate in gates {
            let _ = gate.send(());
        }
    }

    /// A durable service is live work too: one `Durable` and one `Worker`
    /// running under cap 2 refuse a third birth — the cap counts running
    /// entries of every class.
    #[test]
    fn durable_birth_counts_toward_the_cap() {
        let mut shell = Shell::new(Default::default());
        shell.turn.worker_cap = Some(2);

        let mut gates = Vec::new();
        for (class, cmd) in [
            (LeaseClass::Durable, "<service>"),
            (LeaseClass::Worker, "<block>"),
        ] {
            let (gate_tx, gate_rx) = mpsc::channel::<()>();
            spawn_child(
                Arc::new(shell.mobile().scope),
                &mut shell,
                ChildIoMode::Buffered,
                class,
                cmd,
                move |_c| {
                    gate_rx.recv().unwrap();
                    Ok(Value::Unit)
                },
            )
            .expect("a birth under the cap must be admitted");
            gates.push(gate_tx);
        }

        let refused = spawn_child(
            Arc::new(shell.mobile().scope),
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<three>",
            |_c| Ok(Value::Unit),
        );
        assert!(
            matches!(refused, Err(Break::Error(_))),
            "a durable service holds a seat like any live worker"
        );

        for gate in gates {
            let _ = gate.send(());
        }
    }

    /// A settled entry lingering under retention holds no seat: with cap 1
    /// and one finished-but-unclaimed worker still listed, the next birth
    /// is admitted — the cap counts running workers, not registry entries.
    #[test]
    fn settled_entries_do_not_block_admission() {
        let mut shell = Shell::new(Default::default());
        shell.turn.worker_cap = Some(1);
        let first = spawn_child(
            Arc::new(shell.mobile().scope),
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<done>",
            |_c| Ok(Value::Unit),
        )
        .expect("the first birth is admitted");
        wait_settled(&first);
        assert_eq!(shell.local.workers.count(), 1, "the settled entry lingers");

        spawn_child(
            Arc::new(shell.mobile().scope),
            &mut shell,
            ChildIoMode::Buffered,
            LeaseClass::Worker,
            "<next>",
            |_c| Ok(Value::Unit),
        )
        .expect("a lingering settled entry must not hold a seat");
    }
}
