//! Structured concurrency primitives: `spawn`, `watch`, `await`,
//! `race`, `cancel`, and `disown`.
//!
//! Each spawned task runs on its own OS thread with a cloned environment
//! snapshot.  IO is either buffered (returned as a record by `await`) or
//! line-framed (streamed live with a label prefix for `watch`).
//!
//! `await h` resolves a `Handle α` to a record
//! `{ value: α, stdout: Bytes, stderr: Bytes, status: Int }`.  The block's
//! stdout and stderr are not auto-replayed to the caller's terminal; they
//! sit in the record.  If the block raised, `await` re-raises — wrap in
//! `try` to recover.

use crate::evaluator::eval_block_body;
use crate::io::Sink;
use crate::types::*;
use std::sync::{Arc, Mutex};

use super::util::{as_list, check_arity, expect_handle, expect_thunk, sig};

/// How a child task's stdout/stderr are wired.
pub(super) enum ChildIoMode {
    Buffered,
    Watch { label: String },
}

/// Sinks for the child shell, plus the buffers shared with its handle.
/// `flush_pending` flushes any partial line on child exit (watch mode);
/// buffered mode drains on `await`.
struct ChildIo {
    stdout: Sink,
    stderr: Sink,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    flush_pending: bool,
}

impl ChildIo {
    /// Allocate buffers and build the child's sinks.  Buffered mode writes
    /// into the shared byte buffers; watch mode wraps clones of the parent
    /// stdout in `Sink::LineFramed` with a per-task prefix.
    fn prepare(mode: ChildIoMode, shell: &mut Shell) -> Result<Self, EvalSignal> {
        let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (stdout, stderr, flush_pending) = match mode {
            ChildIoMode::Buffered => (
                Sink::Buffer(stdout_buf.clone()),
                Sink::Buffer(stderr_buf.clone()),
                false,
            ),
            ChildIoMode::Watch { label } => {
                let clone_parent = || {
                    shell.io
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
        Ok(Self {
            stdout,
            stderr,
            stdout_buf,
            stderr_buf,
            flush_pending,
        })
    }
}

/// Spawn a child task on a new OS thread.
///
/// The child receives a cloned environment with IO wired according to
/// `io_mode`.  The returned `HandleInner` can be awaited, cancelled, or
/// disowned.
pub(super) fn spawn_child<F>(
    snap: Arc<Env>,
    shell: &mut Shell,
    io_mode: ChildIoMode,
    cmd: &str,
    work: F,
) -> Result<HandleInner, EvalSignal>
where
    F: FnOnce(&mut Shell) -> Result<Value, EvalSignal> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let ChildIo {
        stdout,
        stderr,
        stdout_buf,
        stderr_buf,
        flush_pending,
    } = ChildIo::prepare(io_mode, shell)?;
    let cmd = cmd.to_string();

    shell.spawn_thread(snap, move |child_env| {
        child_env.io.capture_outer = None;
        child_env.io.stdout = stdout;
        child_env.io.stderr = stderr;

        let result = match work(child_env) {
            Err(EvalSignal::TailCall { callee, args }) => {
                crate::evaluator::trampoline(callee, args, child_env)
            }
            other => other,
        };
        if flush_pending {
            let _ = child_env.io.stdout.flush_pending();
            let _ = child_env.io.stderr.flush_pending();
        }
        let _ = tx.send(result);
    });

    Ok(HandleInner {
        result: Arc::new(Mutex::new(Some(rx))),
        cached: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(HandleState::Running)),
        stdout_buf,
        stderr_buf,
        cmd,
    })
}

// ── spawn ────────────────────────────────────────────────────────────────

/// `spawn <thunk>` -- spawn a concurrent task, return a handle.
pub(super) fn builtin_spawn(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "spawn")?;
    let (ast, captured) = expect_thunk(&args[0], "spawn")?;
    spawn_buffered(ast, captured, shell)
}

/// Buffered spawn (§13.3 replay rule).  The child's stdout/stderr accumulate
/// in per-handle buffers and are drained to the caller's sinks on `await`.
fn spawn_buffered(
    ast: &crate::ir::Comp,
    captured: &Arc<Env>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let body = std::sync::Arc::new(ast.clone());
    Ok(Value::Handle(spawn_child(
        captured.clone(),
        shell,
        ChildIoMode::Buffered,
        "<block>",
        move |child_env| eval_block_body(&body, child_env),
    )?))
}

// ── watch ────────────────────────────────────────────────────────────────

/// `watch <label> <thunk>` -- spawn a task whose output streams live
/// to the caller's stdout, line-framed with the given label.
pub(super) fn builtin_watch(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
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
    let (ast, captured) = expect_thunk(&args[1], "watch")?;
    spawn_labelled(ast, captured, label, shell)
}

/// Line-framed spawn.  The child writes to a `Sink::LineFramed` wrapping a
/// clone of the caller's stdout (resp. stderr), so every complete line arrives
/// on the caller's stream prefixed `[label] ` (resp. `[label:err] `) without
/// any global multiplexer.  Sibling watchers serialise through the OS stdout
/// lock or through the `Sink::External` adapter's internal mutex, so each
/// line is emitted atomically.  The `await` replay drain is a no-op because
/// the stdout/stderr buffers stay empty.
fn spawn_labelled(
    ast: &crate::ir::Comp,
    captured: &Arc<Env>,
    label: std::string::String,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let body = std::sync::Arc::new(ast.clone());
    Ok(Value::Handle(spawn_child(
        captured.clone(),
        shell,
        ChildIoMode::Watch { label },
        "<watch>",
        move |child_env| eval_block_body(&body, child_env),
    )?))
}

/// `cancel <handle>` -- mark a running task as cancelled.
pub(super) fn builtin_cancel(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "cancel")?;
    let handle = expect_handle(&args[0], "cancel")?;
    let state = *handle.state.lock().unwrap();
    if state != HandleState::Completed {
        detach_handle(handle, HandleState::Cancelled);
    }
    shell.control.last_status = 0;
    Ok(Value::Unit)
}

/// `disown <handle>` -- detach a task, letting it run in the background.
pub(super) fn builtin_disown(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "disown")?;
    let handle = expect_handle(&args[0], "disown")?;
    detach_handle(handle, HandleState::Disowned);
    shell.control.last_status = 0;
    Ok(Value::Unit)
}

/// Block until `handle` completes, replay its buffered output, and
/// return its result.  Errors if the handle was cancelled or disowned.
pub(super) fn await_handle(handle: &HandleInner, shell: &mut Shell) -> Result<Value, EvalSignal> {
    match *handle.state.lock().unwrap() {
        HandleState::Cancelled => {
            shell.control.last_status = 1;
            return Err(handle_state_error(HandleState::Cancelled));
        }
        HandleState::Disowned => {
            shell.control.last_status = 1;
            return Err(handle_state_error(HandleState::Disowned));
        }
        _ => {}
    }

    if let Some(result) = handle.cached.lock().unwrap().clone() {
        set_status_from_result(&result, shell);
        return result;
    }

    let mut rx_guard = handle.result.lock().unwrap();
    let rx = rx_guard
        .take()
        .ok_or_else(|| sig("await: handle in invalid state"))?;
    drop(rx_guard);

    let result = rx
        .recv()
        .unwrap_or_else(|_| Err(sig("await: spawned thread panicked")));
    complete_handle(handle, result, shell)
}

/// `await <handle>` -- wait for a task to complete and return its result record.
pub(super) fn builtin_await(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "await")?;
    let handle = expect_handle(&args[0], "await")?;
    await_handle(handle, shell)
}

/// `_par`-internal: wait for `handle`, then return only the block's value
/// (drop stdout/stderr/status from the surface result).  `par` is a parallel
/// `map` — its output list is values, not full records — so it strips the
/// envelope rather than burdening every caller with `[value]` access.
pub(super) fn await_value(handle: &HandleInner, shell: &mut Shell) -> Result<Value, EvalSignal> {
    let record = await_handle(handle, shell)?;
    match record {
        Value::Map(fields) => fields
            .into_iter()
            .find_map(|(k, v)| (k == "value").then_some(v))
            .ok_or_else(|| sig("await: result record missing `value` field")),
        other => Ok(other),
    }
}

/// `race <handles>` -- wait for the first of several tasks to finish.
/// Cancels remaining handles once a winner is found.
pub(super) fn builtin_race(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.is_empty() {
        return Err(sig("race requires 1 argument (list of handles)"));
    }
    let handles = as_list(&args[0], "race")?;

    loop {
        let mut saw_running = false;
        for h in &handles {
            if let Value::Handle(handle) = h {
                let state = *handle.state.lock().unwrap();
                match state {
                    HandleState::Completed => {
                        if let Some(result) = handle.cached.lock().unwrap().clone() {
                            set_status_from_result(&result, shell);
                            return result;
                        }
                        continue;
                    }
                    HandleState::Cancelled | HandleState::Disowned => continue,
                    HandleState::Running => saw_running = true,
                }

                let rx_guard = handle.result.lock().unwrap();
                if let Some(ref rx) = *rx_guard
                    && let Ok(result) = rx.try_recv()
                {
                    drop(rx_guard);
                    let result = complete_handle(handle, result, shell);
                    for h2 in &handles {
                        if let Value::Handle(h2) = h2
                            && !Arc::ptr_eq(&h2.result, &handle.result)
                        {
                            detach_handle(h2, HandleState::Cancelled);
                        }
                    }
                    return result;
                }
            }
        }
        if !saw_running {
            return Err(sig("race: no active handles"));
        }
        crate::signal::check(shell)?;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Set `shell.control.last_status` from a task result: 0 on success, the error's
/// status code on failure, 1 otherwise.
fn set_status_from_result(result: &Result<Value, EvalSignal>, shell: &mut Shell) {
    shell.control.last_status = match result {
        Ok(_) => 0,
        Err(EvalSignal::Error(e)) => e.status,
        _ => 1,
    };
}

/// Transition a handle to `Completed`, cache the await-record (or error),
/// and return it.  Buffers drain on first observation; the cached record
/// stays valid for any repeat awaits.
fn complete_handle(
    handle: &HandleInner,
    result: Result<Value, EvalSignal>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    {
        let mut state = handle.state.lock().unwrap();
        if *state == HandleState::Running {
            *state = HandleState::Completed;
        }
    }
    set_status_from_result(&result, shell);
    let cached = result.map(|value| buildawait_record(handle, value, shell.control.last_status));
    *handle.cached.lock().unwrap() = Some(cached.clone());
    cached
}

/// Drain `handle`'s stdout and stderr buffers into a fresh `Map` carrying
/// the block's return value and exit status.
fn buildawait_record(handle: &HandleInner, value: Value, status: i32) -> Value {
    let stdout = drain_buffer(&handle.stdout_buf);
    let stderr = drain_buffer(&handle.stderr_buf);
    Value::Map(vec![
        ("value".into(), value),
        ("stdout".into(), Value::Bytes(stdout)),
        ("stderr".into(), Value::Bytes(stderr)),
        ("status".into(), Value::Int(status as i64)),
    ])
}

fn drain_buffer(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    let mut guard = buffer.lock().unwrap();
    std::mem::take(&mut *guard)
}

/// Release a handle's receiver and clear its cached result.
fn detach_handle(handle: &HandleInner, new_state: HandleState) {
    *handle.state.lock().unwrap() = new_state;
    let mut rx_guard = handle.result.lock().unwrap();
    let _ = rx_guard.take();
    drop(rx_guard);
    *handle.cached.lock().unwrap() = None;
}

fn handle_state_error(state: HandleState) -> EvalSignal {
    match state {
        HandleState::Cancelled => EvalSignal::Error(
            Error::new("handle is cancelled", 1)
                .with_hint("use try around await to handle cancellation"),
        ),
        HandleState::Disowned => EvalSignal::Error(
            Error::new("handle is disowned", 1)
                .with_hint("disown detaches the handle from await tracking"),
        ),
        _ => sig("invalid handle state"),
    }
}
