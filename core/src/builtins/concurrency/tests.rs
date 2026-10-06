#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]

use super::eliminate::await_handle;
use super::*;
use crate::first_order::FOValue;
use crate::io::new_buffer;
use crate::types::{
    Break, CompletedHandle, DeferredSink, EventSink, HandleInner, HandleState, LeaseClass, Map,
    ReapCause, SurfaceBuffer, WorkerLease,
};
use std::io::Write as _;
use std::sync::{Arc, Mutex, mpsc};

fn status(b: Break) -> i32 {
    match b {
        Break::Error(e) => e.code(),
        other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
    }
}

/// A handle whose worker dropped its `Sender` unsent, modelling a panic.
/// The buffers are pre-seeded so a settled outcome can be checked for them
/// — through their sinks, as a real worker fills them.
fn handle_with_disconnected_worker(stdout: &[u8], stderr: &[u8]) -> HandleInner {
    use std::io::Write;
    let (tx, rx) = mpsc::channel::<Settled<Value>>();
    drop(tx);
    let (mut out_sink, stdout_buf) = new_buffer();
    let (mut err_sink, stderr_buf) = new_buffer();
    out_sink
        .write_all(stdout)
        .expect("a buffer sink cannot fail");
    err_sink
        .write_all(stderr)
        .expect("a buffer sink cannot fail");
    HandleInner {
        stdout_buf,
        stderr_buf,
        ..HandleInner::new("<test>", crate::process::CancelScope::default(), Some(rx))
    }
}

fn expect_variant<'a>(v: &'a Value, label: &str) -> &'a Value {
    match v {
        Value::Variant {
            label: l,
            payload: Some(p),
        } if l.as_ref() == label => p,
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

/// A panicked worker's `Disconnected` receiver must settle as a failure,
/// not `None` — else `poll` reads `pending` forever and `race` spins.
#[test]
fn try_settle_reports_disconnected_worker_as_failed() {
    let handle = handle_with_disconnected_worker(b"", b"");
    match handle.try_settle() {
        Some(CompletedHandle {
            outcome: Err(Break::Error(e)),
            ..
        }) => {
            assert_eq!(e.code(), 1);
        }
        other => panic!("expected Some(failed outcome), got {other:?}"),
    }
}

/// `poll` over a panicked worker yields `` `settled `` with the bytes it
/// buffered before panicking and an `` `err `` outcome, never re-raising.
#[test]
fn poll_reports_disconnected_worker_as_settled_err() {
    let shell = crate::test_helper::core_shell();
    let handle = handle_with_disconnected_worker(b"out", b"err");
    let args = [Value::Handle(Box::new(handle))];
    let poll1 = builtin_poll(&args, &shell).expect("poll must not re-raise a panic");

    let settled = expect_variant(&poll1, "settled");
    let fields = expect_map(settled);
    assert_eq!(
        fields.get("stdout").as_deref(),
        Some(&Value::bytes(b"out".to_vec()))
    );
    assert_eq!(
        fields.get("stderr").as_deref(),
        Some(&Value::bytes(b"err".to_vec()))
    );
    let outcome = fields.get("outcome");
    let err = expect_variant(outcome.as_deref().expect("outcome field"), "err");
    let err_fields = expect_map(err);
    assert_eq!(err_fields.get("status").as_deref(), Some(&Value::Int(1)));

    let poll2 = builtin_poll(&args, &shell).expect("repeat poll must not re-raise");
    assert_eq!(poll1, poll2);
}

/// A worker polling `Mooring::check` against its own scope, reporting the
/// status it saw once `cancel_via` fires.  `ready` confirms it is alive
/// first, so a test pins propagation, not a worker that never ran.
fn spawn_polling_worker(
    shell: &Shell,
    cancel_via: impl FnOnce(&crate::process::CancelScope),
) -> (i32, crate::process::CancelScope) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker_mooring = Mooring::for_worker(&Mooring::adrift(), &shell.session.root, Arc::new(()));
    let (_join, worker_cancel) = shell
        .spawn_thread(worker_mooring, "test-worker", move |mooring, _child| {
            ready_tx.send(()).unwrap();
            loop {
                if let Err(b) = mooring.check() {
                    done_tx.send(status(b)).unwrap();
                    return;
                }
                std::thread::yield_now();
            }
        })
        .expect("spawn_thread");
    ready_rx.recv().unwrap();
    cancel_via(&worker_cancel);
    (done_rx.recv().unwrap(), worker_cancel)
}

/// Cancelling a worker's own scope stops it and leaves a sibling from the
/// same shell alone — read off scopes no other test can reach, so no stray
/// cancellation can satisfy it.
#[test]
fn worker_scope_cancel_stops_the_worker() {
    let shell = crate::test_helper::core_shell();
    let sibling_mooring =
        Mooring::for_worker(&Mooring::adrift(), &shell.session.root, Arc::new(()));
    let (_idle_join, sibling) = shell
        .spawn_thread(sibling_mooring, "test-sibling", |_, _| ())
        .expect("spawn_thread");
    let (observed, worker_scope) = spawn_polling_worker(&shell, |c| {
        c.cancel(crate::process::CancelCause::Cancelled);
    });
    assert!(
        worker_scope.is_cancelled(),
        "the worker's own scope must observe its cancel"
    );
    assert!(
        !sibling.is_cancelled(),
        "cancelling one worker's scope must not cancel a sibling"
    );
    assert_eq!(observed, 143);
}

/// A [`RootAbort`](crate::process::CancelCause::Aborted) reaches the
/// worker: its scope descends from the root, so its next poll sees the flag.
#[test]
fn root_cancel_reaches_the_worker() {
    let shell = crate::test_helper::core_shell();
    let root = shell.session.root.clone();
    let (observed, worker_scope) = spawn_polling_worker(&shell, move |_| {
        root.cancel(crate::process::CancelCause::Aborted);
    });
    assert!(
        worker_scope.is_cancelled(),
        "a RootAbort on the durable root must cancel the worker's scope"
    );
    assert_eq!(observed, 131);
}

/// A foreground cancel spares a detached worker: it parents under the
/// durable root, so a run timeout cannot reap work meant to outlive the run.
#[test]
fn foreground_cancel_spares_detached_worker() {
    let shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let worker_mooring = Mooring::for_worker(&m, &shell.session.root, Arc::new(()));
    let (_join, worker_scope) = shell
        .spawn_thread(worker_mooring, "test-worker", |_, _| ())
        .expect("spawn_thread");
    m.cancel.cancel(crate::process::CancelCause::Interrupted);
    assert!(
        !worker_scope.is_cancelled(),
        "a foreground cancel must not reach a detached worker"
    );
}

/// A blocked `await` unwinds on a foreground cancel instead of sleeping on a
/// bare `recv`, yet the root-parented worker it awaited stays awaitable.
#[test]
fn await_unwinds_on_foreground_cancel_sparing_the_worker() {
    let shell = crate::test_helper::core_shell();

    // A still-running worker, root-parented as a real `spawn`'s is.  Its
    // `Sender` stays alive, so the receiver reports `Empty`, not `Disconnected`.
    let (_tx, rx) = mpsc::channel::<Settled<Value>>();
    let (_sink, stdout_buf) = new_buffer();
    let (_sink2, stderr_buf) = new_buffer();
    let worker_scope = shell.session.root.worker().as_scope().clone();
    let handle = HandleInner {
        stdout_buf,
        stderr_buf,
        ..HandleInner::new("<test>", worker_scope.clone(), Some(rx))
    };

    // Cancel the foreground (run deadline / interrupt), not the root.
    let m = Mooring::adrift();
    m.cancel.cancel(crate::process::CancelCause::Interrupted);

    let err = await_handle(&handle, &m, &shell)
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

/// A body that stays `Running` until cancelled, polling `Mooring::check` so
/// a reap genuinely unwinds the thread rather than flag a scope nobody reads.
fn check_loop(mooring: &Mooring, _child: &mut Shell) -> Settled<Value> {
    loop {
        mooring.check()?;
        std::thread::yield_now();
    }
}

/// Block until `handle`'s worker has marked itself `Completed` at exit.
fn wait_settled(handle: &HandleInner) {
    for _ in 0..500 {
        if handle.state() == HandleState::Completed {
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(40, 10_000));
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<abandoned>", check_loop)
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
    assert_eq!(scope.cause(), Some(crate::process::CancelCause::TimedOut));
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<test>", |_, _child| {
        Ok(Value::Unit)
    })
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
        req: crate::first_order::FOValue,
        _cancel: &crate::process::CancelScope,
    ) -> Result<crate::first_order::FOValue, crate::types::Error> {
        Ok(req)
    }
}

/// Containment: a detached worker's own `enquire` answers the absence
/// error rather than reach the run's desk.
#[test]
fn spawned_worker_never_receives_the_enquiry_desk() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.desk = Some(Arc::new(EchoDesk) as crate::types::Desk);
    let (tx, rx) = mpsc::channel::<Result<crate::first_order::FOValue, crate::types::Error>>();
    let handle = spawn_child(
        &m,
        &mut shell,
        Birth::Spawn,
        "<test>",
        move |mooring, child| {
            let outcome = child.enquire(mooring, crate::first_order::FOValue::Unit);
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.fork = Some(crate::types::Fork::Park(crate::types::Nursery::default()));
    let (tx, rx) = mpsc::channel::<crate::types::Settled<crate::types::NurseryId>>();
    let handle = spawn_child(
        &m,
        &mut shell,
        Birth::Spawn,
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(200, 10_000));
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<babysat>", move |_, _c| {
        gate_rx.recv().unwrap();
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    let scope = handle.cancel.clone();

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
    while std::time::Instant::now() < deadline {
        builtin_poll(&[Value::Handle(Box::new(handle.clone()))], &shell)
            .expect("poll a live handle");
        assert!(
            !scope.is_cancelled(),
            "a polled worker must never be idle-reaped"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(shell.local.workers.count(), 1, "the babysat entry stays");

    gate_tx.send(()).unwrap();
    await_handle(&handle, &m, &shell).expect("await after the gate opens");
    assert!(!scope.is_cancelled(), "the worker finished by itself");
}

/// The backstop is absolute: ritual polling renews the idle bound but
/// cannot carry a worker past `backstop`, and the cause says so.
#[test]
fn backstop_reaps_a_ritually_polled_worker() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(150, 400));
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<immortal>", check_loop)
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
        let _ = builtin_poll(&[Value::Handle(Box::new(handle.clone()))], &shell);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(scope.cause(), Some(crate::process::CancelCause::TimedOut));
    assert_eq!(shell.local.workers.count(), 0);
    let notices = shell.take_worker_reap_notices();
    assert_eq!(notices.len(), 1, "one notice for the backstop reap");
    assert_eq!(notices[0].cause, ReapCause::Backstop);
}

/// A worker that completed but was never observed is not reaped: its exit
/// mark ends the chain, so its entry lingers as an unclaimed result.
#[test]
fn completed_unobserved_worker_is_not_reaped() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(100, 10_000));
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<done>", |_, _child| {
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    let scope = handle.cancel.clone();

    // Wait for the exit mark (it rides the worker thread), then let ~3 idle
    // bounds elapse so the chain has demonstrably fired and ended.
    let mut completed = false;
    for _ in 0..200 {
        if handle.state() == HandleState::Completed {
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(40, 10_000));
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<listed>", check_loop)
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
    assert_eq!(scope.cause(), Some(crate::process::CancelCause::TimedOut));
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(40, 150));

    let durable = spawn_child(&m, &mut shell, Birth::Service, "<service>", check_loop)
        .expect("durable spawn must succeed");
    let born = std::time::Instant::now();
    let sibling = spawn_child(&m, &mut shell, Birth::Spawn, "<sibling>", check_loop)
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
    durable
        .cancel
        .cancel(crate::process::CancelCause::Cancelled);
}

/// `cancel` still fires a durable worker's scope and removes its entry:
/// durability exempts the lease chain, not the eliminators.
#[test]
fn cancel_through_the_handle_ends_a_durable_worker() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred_lease = Some(lease_ms(10_000, 20_000));
    let handle = spawn_child(&m, &mut shell, Birth::Service, "<service>", check_loop)
        .expect("durable spawn must succeed");

    builtin_cancel(&[Value::Handle(Box::new(handle.clone()))], &shell)
        .expect("cancel must succeed on a durable worker");

    assert!(
        handle.cancel.is_cancelled(),
        "cancel fires the durable worker's scope"
    );
    assert_eq!(
        handle.cancel.cause(),
        Some(crate::process::CancelCause::Cancelled)
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

/// The one `RunRequest` a capturing top-level test run needs, dressed
/// only by its source and worker cap.
fn request(src: &str, worker_cap: Option<usize>) -> crate::run::RunRequest {
    use crate::protocol::Run;

    use crate::run::RunRequest;
    RunRequest::from(Run {
        worker_cap,
        ..Run::captured(src, "<test>")
    })
}

/// Run `src` as one capturing top-level run on a shell the caller dressed,
/// under `worker_cap`, with the bytes it wrote to stdout. Panics on a static
/// failure — every source here is expected to compile.
fn run_captured(
    shell: &mut Shell,
    src: &str,
    worker_cap: Option<usize>,
) -> (Settled<Value>, Vec<u8>) {
    use crate::run::RunReport;
    match shell.run(request(src, worker_cap)) {
        RunReport::Ran {
            ending, captured, ..
        } => (
            ending.into_result(),
            captured.map(|c| c.stdout).unwrap_or_default(),
        ),
        RunReport::Static { .. } => {
            panic!("well-formed source must run, not fail statically: {src:?}")
        }
    }
}

/// The observations an `audit { }` report collected.
fn trail_of(report: &Value) -> crate::types::List {
    match expect_map(report)
        .get("trail")
        .map(std::borrow::Cow::into_owned)
    {
        Some(Value::List(trail)) => trail,
        other => panic!("expected a trail List, got {other:?}"),
    }
}

/// The `` `worker `` facts of a trail — a birth is read by its tag.
fn births(trail: &crate::types::List) -> Vec<Map> {
    trail
        .iter()
        .filter_map(|o| match o.into_owned() {
            Value::Map(o) => match o.get("what").map(std::borrow::Cow::into_owned) {
                Some(Value::Variant {
                    label,
                    payload: Some(fact),
                }) if label.as_ref() == "worker" => match *fact {
                    Value::Map(fact) => Some(fact),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The one `` `worker `` fact of an `audit { }` report's trail, panicking
/// if there is not exactly one.
fn only_birth(trail: &crate::types::List) -> Map {
    match births(trail).as_mut_slice() {
        [birth] => std::mem::take(birth),
        other => panic!("expected exactly one worker birth, got {other:?}"),
    }
}

/// `audit { spawn … }` records the birth in its trail as a `` `worker ``
/// fact carrying the spawn's own `cmd` and `class` and the minted `id` —
/// so a later reader can join the trail against the registry.
#[test]
fn audit_over_spawn_carries_the_birth_id_and_all() {
    let mut shell = crate::test_helper::core_shell();
    let report = run_captured(&mut shell, "audit { !{spawn { return 1 }} }", None)
        .0
        .expect("audit over a plain spawn must succeed");
    let birth = only_birth(&trail_of(&report));
    assert_eq!(
        birth.get("cmd").as_deref(),
        Some(&Value::string("block at <test>, line 1"))
    );
    assert_eq!(
        birth.get("class").as_deref(),
        Some(&Value::variant("worker", None))
    );
    assert!(
        matches!(birth.get("id").as_deref(), Some(Value::Int(_))),
        "the birth carries the minted worker id: {birth:?}"
    );
}

/// `defer` is a prelude function around `spawn`: its worker is named at
/// the line that applied `defer`, not at the command before it.
#[test]
fn a_deferred_worker_is_named_by_the_line_that_wrote_defer() {
    let mut shell = crate::boot::boot_shell(
        crate::terminal::TerminalState::default(),
        crate::boot::BakedPrelude::runtime(),
        &crate::boot::HostSurface::default(),
    );
    let report = run_captured(
        &mut shell,
        "audit {\n  echo x\n  let h = defer { return 1 }\n}",
        None,
    )
    .0
    .expect("audit over a defer must succeed");
    let birth = only_birth(&trail_of(&report));
    assert_eq!(
        birth.get("cmd").as_deref(),
        Some(&Value::string("block at <test>, line 3"))
    );
}

/// A spawn refused at the cap observes no birth: no phantom worker
/// reaches an orphan join, though the refused dispatch itself still
/// records its own (failing) command entry.
#[test]
fn cap_refused_spawn_observes_no_birth() {
    let mut shell = crate::test_helper::core_shell();
    let report = run_captured(&mut shell, "audit { !{spawn { return 1 }} }", Some(0))
        .0
        .expect("audit swallows the refusal as data, not an error");
    let outcome_field = expect_map(&report).get("outcome");
    let outcome = outcome_field
        .as_deref()
        .expect("a report carries its outcome");
    assert_eq!(
        expect_map(expect_variant(outcome, "err"))
            .get("status")
            .as_deref(),
        Some(&Value::Int(1)),
        "the refused spawn's exit code"
    );
    let trail = trail_of(&report);
    assert!(
        births(&trail).is_empty(),
        "a spawn refused at the cap must observe no birth: {trail:?}"
    );
}

/// A worker's `explain` answers for its creator's session, as `!{ … }` does.
#[test]
fn a_worker_explains_against_its_creators_session() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_var("sess_name".into(), Value::Int(1));
    let (direct, printed) = run_captured(&mut shell, "!{ explain sess_name }", None);
    direct.expect("explain must run");
    assert!(String::from_utf8_lossy(&printed).contains("sess_name: session"));
    let awaited = run_captured(
        &mut shell,
        "let h = !{spawn { explain sess_name }}\nawait $h",
        None,
    )
    .0
    .expect("the worker must run explain");
    assert_eq!(
        expect_map(&awaited).get("stdout").as_deref(),
        Some(&Value::bytes(printed))
    );
}

/// A worker's `use` runs the module under its creator's session.
#[test]
fn a_worker_uses_a_module_against_its_creators_session() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let module = dir.path().join("m.ral");
    std::fs::write(&module, "let v = $sess_cfg\n").expect("write the module");
    let mut shell = crate::test_helper::core_shell();
    let src = format!(
        "let sess_cfg = 7\nlet h = !{{spawn {{ use '{}' }}}}\nawait $h",
        module.display()
    );
    let awaited = run_captured(&mut shell, &src, None)
        .0
        .expect("the worker must load the module");
    let value = expect_map(&awaited).get("value").expect("await's value");
    assert_eq!(expect_map(&value).get("v").as_deref(), Some(&Value::Int(7)));
}

fn service_test_shell() -> Shell {
    let mut shell = crate::test_helper::core_shell();
    shell.install_builtins(crate::builtins::SERVICE_BUILTIN);
    shell
}

/// An empty (or whitespace-only) description is refused: it is the whole
/// legibility bound a durable birth declares, so it cannot be absent.
#[test]
fn service_rejects_an_empty_description() {
    let mut shell = service_test_shell();
    let err = run_captured(&mut shell, r#"service "   " { 1 }"#, None)
        .0
        .expect_err("an empty description must be refused");
    assert_eq!(status(err), 1);
}

/// A multi-line description is refused: a ledger label, not a paragraph.
#[test]
fn service_rejects_a_multiline_description() {
    let mut shell = service_test_shell();
    let err = run_captured(&mut shell, "service \"one\ntwo\" { 1 }", None)
        .0
        .expect_err("a multiline description must be refused");
    assert_eq!(status(err), 1);
}

/// A valid description lands trimmed as the registry entry's `cmd`.
#[test]
fn service_description_lands_in_the_registry_entry() {
    let mut shell = service_test_shell();
    let handle = match run_captured(&mut shell, r#"service "  watch the thing  " { 1 }"#, None).0 {
        Ok(Value::Handle(h)) => h,
        other => panic!("service must return a Handle, got {other:?}"),
    };
    let entries = shell.workers();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].class, LeaseClass::Durable);
    assert_eq!(entries[0].cmd, "watch the thing", "the description trims");
    handle.cancel.cancel(crate::process::CancelCause::Cancelled);
}

// ── `detach` ─────────────────────────────────────────────────────────

#[cfg(unix)]
fn detach_test_shell(budget: u64) -> Shell {
    let mut shell = crate::test_helper::core_shell();
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
    let err = run_captured(&mut shell, r#"detach "a server""#, None)
        .0
        .expect_err("a description alone is not a detach");
    assert_eq!(status(err), 1);
}

/// The same legibility bound `service` carries, refused before a birth.
#[cfg(unix)]
#[test]
fn detach_rejects_an_illegible_description() {
    let mut shell = detach_test_shell(4);
    let empty = run_captured(&mut shell, r#"detach "   " /bin/echo hi"#, None)
        .0
        .expect_err("an empty description must be refused");
    assert_eq!(status(empty), 1);
    let multiline = run_captured(&mut shell, "detach \"one\ntwo\" /bin/echo hi", None)
        .0
        .expect_err("a multiline description must be refused");
    assert_eq!(status(multiline), 1);
}

/// A head a handler intercepts is refused, per name and by catch-all alike:
/// a handler runs inside this session, so nothing could be detached.  A stub
/// stands in for `detach` itself.
#[cfg(unix)]
#[test]
fn detach_refuses_a_head_a_handler_intercepts() {
    let mut shell = detach_test_shell(4);
    for src in [
        r#"within [handlers: [my-server: { |args| echo ...$args }]] { detach "a server" my-server up now }"#,
        r#"within [handler: { |n _a| echo $n }] { detach "a server" my-server }"#,
    ] {
        let err = run_captured(&mut shell, src, None)
            .0
            .expect_err("a handled head is no program to detach");
        let Break::Error(e) = err else {
            panic!("an error expected, got {err:?}");
        };
        assert!(
            e.message.contains("`my-server` is handled here"),
            "{}",
            e.message
        );
    }
}

/// A base frame's name is ral's own, and runs inside this session: refused,
/// the budget untouched.
#[cfg(unix)]
#[test]
fn detach_refuses_a_base_frames_name() {
    let mut shell = detach_test_shell(4);
    let err = run_captured(&mut shell, r#"detach "a server" echo hi"#, None)
        .0
        .expect_err("a base frame is no program to detach");
    let Break::Error(e) = err else {
        panic!("an error expected, got {err:?}");
    };
    assert!(e.message.contains("`echo` is ral's own"), "{}", e.message);
}

/// Vetting is reused wholesale, so an unresolvable head gives the usual 127.
#[cfg(unix)]
#[test]
fn detach_reports_an_unknown_command_as_127() {
    let mut shell = detach_test_shell(4);
    let err = run_captured(
        &mut shell,
        r#"detach "a server" definitely-not-a-real-tool-xyz"#,
        None,
    )
    .0
    .expect_err("an unknown head must not be born");
    assert_eq!(status(err), 127);
}

/// The budget is a hard bound, and its refusal claims no remedy inside ral.
#[cfg(unix)]
#[test]
fn detach_refuses_past_its_budget() {
    let mut shell = detach_test_shell(0);
    let err = run_captured(&mut shell, r#"detach "a server" /bin/echo hi"#, None)
        .0
        .expect_err("a spent budget must refuse");
    assert!(format!("{err:?}").contains("budget"));
}

/// The whole of what a birth hands back: the caller's description and the
/// kernel's pid.  Nothing else — three `/dev/null` streams, no file anywhere.
#[cfg(unix)]
#[test]
fn detach_returns_a_receipt_of_a_pid_and_a_desc() {
    let mut shell = detach_test_shell(4);
    let born = run_captured(&mut shell, r#"detach "the greeter" /bin/echo hello"#, None)
        .0
        .expect("the birth must succeed");
    let fields = expect_map(&born);
    assert_eq!(
        fields.len(),
        2,
        "the receipt is a pid and a desc, and nothing else: {fields:?}"
    );
    assert_eq!(
        fields.get("desc").as_deref(),
        Some(&Value::string("the greeter"))
    );
    assert!(matches!(fields.get("pid").as_deref(), Some(Value::Int(p)) if *p > 0));
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

    let shell = crate::test_helper::core_shell();
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
        stdout_buf,
        stderr_buf,
        surface_buf,
        ..HandleInner::new("<test>", crate::process::CancelScope::default(), Some(rx))
    };

    builtin_poll(&[Value::Handle(Box::new(handle.clone()))], &shell).expect("poll ok");
    assert_eq!(log.lock().unwrap().len(), 0, "poll must not replay surface");

    await_handle(&handle, &m, &shell).expect("await ok");
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "await replays the deferred card"
    );

    await_handle(&handle, &m, &shell).expect("await ok");
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
    match done.field("outcome").expect("outcome field") {
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
        work: impl FnOnce(&Mooring, &mut Shell) -> Settled<Value> + Send + 'static,
    ) -> Vec<FOValue> {
        let mut shell = crate::test_helper::core_shell();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        // Hold the handle so the channel stays connected until the flush;
        // never observed, so no eliminator competes for the `joined` latch.
        let _handle = spawn_child(&m, &mut shell, Birth::Spawn, "<block>", work).unwrap();
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
        fields.field("cmd").and_then(FOValue::as_str),
        Some("<block>")
    );

    let err = run(|_, _child| Err(sig("boom")));
    assert_eq!(done_outcome_label(&err[0]), "err");

    let panicked = run(|_, _child| panic!("worker exploded"));
    assert_eq!(done_outcome_label(&panicked[0]), "panic");
}

/// A watched worker's lines each leave as a `` `watch `` batch of one on
/// the deferred sink, stderr's under `label:err`, a partial last line
/// flushed at the end.
#[test]
fn watched_lines_surface_one_by_one() {
    let mut shell = crate::test_helper::core_shell();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let mut m = Mooring::adrift();
    m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
    let birth = Birth::Watch {
        label: "job".into(),
    };
    let handle = spawn_child(&m, &mut shell, birth, "job", |_, child| {
        child
            .io
            .stdout
            .write_all(b"one\ntwo")
            .map_err(|e| sig(e.to_string()))?;
        child
            .io
            .stderr
            .write_all(b"bad\r\n")
            .map_err(|e| sig(e.to_string()))?;
        Ok(Value::Unit)
    })
    .unwrap();
    await_handle(&handle, &m, &shell).expect("await ok");
    let lines: Vec<(String, String)> = batches
        .lock()
        .unwrap()
        .iter()
        .filter_map(|batch| match batch.as_slice() {
            [
                FOValue::Variant {
                    label,
                    payload: Some(p),
                },
            ] if label == "watch" => Some((
                p.field("label")?.as_str()?.to_string(),
                p.field("line")?.as_str()?.to_string(),
            )),
            _ => None,
        })
        .collect();
    let pair = |l: &str, t: &str| (l.to_string(), t.to_string());
    assert_eq!(
        lines,
        [
            pair("job", "one"),
            pair("job:err", "bad"),
            pair("job", "two")
        ]
    );
}

/// The body's own events precede the trailing `` `done ``: the batch carries
/// the whole surface.  A panicking worker still settles through `Disconnected`.
#[test]
fn deferred_batch_carries_body_surface_before_done() {
    let mut shell = crate::test_helper::core_shell();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let mut m = Mooring::adrift();
    m.deferred = Some(Arc::new(RecDeferred(batches.clone())));

    let handle = spawn_child(
        &m,
        &mut shell,
        Birth::Spawn,
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

    match handle.try_settle() {
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    assert!(m.deferred.is_none(), "a bare REPL installs none");
    let handle = spawn_child(
        &m,
        &mut shell,
        Birth::Spawn,
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
    await_handle(&handle, &m, &shell).expect("await ok");
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
    let mut shell = crate::test_helper::core_shell();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let mut m = Mooring::adrift();
    m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
    let handle = spawn_child(
        &m,
        &mut shell,
        Birth::Spawn,
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
    assert!(!handle.joined.claim(), "the deferred flush set `joined");

    // A later `await` finds the latch set, so it replays nothing.
    let log = Arc::new(Mutex::new(Vec::new()));
    struct Rec(Arc<Mutex<Vec<FOValue>>>);
    impl EventSink for Rec {
        fn emit(&self, ev: &FOValue) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }
    m.surface = Some(Arc::new(Rec(log.clone())));
    await_handle(&handle, &m, &shell).expect("await still returns the result record");
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<test-cmd>", |_, _child| {
        Ok(Value::Unit)
    })
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();

    // `await` removes.
    let h1 = spawn_child(&m, &mut shell, Birth::Spawn, "<a>", |_, _c| Ok(Value::Unit)).unwrap();
    await_handle(&h1, &m, &shell).expect("await ok");
    assert_eq!(shell.local.workers.count(), 0, "await removes its entry");

    // `cancel` removes.
    let h2 = spawn_child(&m, &mut shell, Birth::Spawn, "<b>", |_, _c| Ok(Value::Unit)).unwrap();
    assert_eq!(shell.local.workers.count(), 1);
    builtin_cancel(&[Value::Handle(Box::new(h2))], &shell).expect("cancel ok");
    assert_eq!(shell.local.workers.count(), 0, "cancel removes its entry");

    // A settled `poll` removes.
    let h3 = spawn_child(&m, &mut shell, Birth::Spawn, "<c>", |_, _c| Ok(Value::Unit)).unwrap();
    loop {
        let polled = builtin_poll(&[Value::Handle(Box::new(h3.clone()))], &shell).unwrap();
        if matches!(&polled, Value::Variant { label, .. } if label.as_ref() == "settled") {
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
    let h4 = spawn_child(&m, &mut shell, Birth::Spawn, "<d>", move |_, _c| {
        unblock_rx.recv().unwrap();
        Ok(Value::Unit)
    })
    .unwrap();
    assert_eq!(shell.local.workers.count(), 1);
    let pending = builtin_poll(&[Value::Handle(Box::new(h4.clone()))], &shell).unwrap();
    assert!(
        matches!(&pending, Value::Variant { label, .. } if label.as_ref() == "pending"),
        "the worker is blocked, so poll must observe it pending: got {pending:?}"
    );
    assert_eq!(
        shell.local.workers.count(),
        1,
        "a pending poll must not touch the registry"
    );
    unblock_tx.send(()).unwrap();
    await_handle(&h4, &m, &shell).expect("await ok");
}

/// `race` removes the winner and every cancelled loser: nothing lingers in
/// the registry once it returns.
#[test]
fn race_removes_winner_and_cancelled_losers() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let winner = spawn_child(&m, &mut shell, Birth::Spawn, "<winner>", |_, _c| {
        Ok(Value::Unit)
    })
    .unwrap();

    // The losers block on their own channels, so the winner always settles
    // first and these two are cancelled.
    let (l1_tx, l1_rx) = mpsc::channel::<()>();
    let loser1 = spawn_child(&m, &mut shell, Birth::Spawn, "<loser1>", move |_, _c| {
        let _ = l1_rx.recv();
        Ok(Value::Unit)
    })
    .unwrap();
    let (l2_tx, l2_rx) = mpsc::channel::<()>();
    let loser2 = spawn_child(&m, &mut shell, Birth::Spawn, "<loser2>", move |_, _c| {
        let _ = l2_rx.recv();
        Ok(Value::Unit)
    })
    .unwrap();
    assert_eq!(shell.local.workers.count(), 3);

    let args = [Value::list(vec![
        Value::Handle(Box::new(winner)),
        Value::Handle(Box::new(loser1)),
        Value::Handle(Box::new(loser2)),
    ])];
    builtin_race(&args, &m, &shell).expect("race must succeed");
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<usize>();
    let _outer = spawn_child(
        &m,
        &mut shell,
        Birth::Spawn,
        "<outer>",
        move |mooring, child_shell| {
            go_rx.recv().unwrap();
            let _inner = spawn_child(mooring, child_shell, Birth::Spawn, "<inner>", |_, _c| {
                Ok(Value::Unit)
            })
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
        "the parent's registry observes the nested spawn's entry too: same registry, not a copy"
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    shell.arm_worker_retention(2);
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<done>", |_, _child| {
        Ok(Value::Unit)
    })
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let _handle = spawn_child(&m, &mut shell, Birth::Spawn, "<gated>", move |_, _c| {
        gate_rx.recv().unwrap();
        Ok(Value::Unit)
    })
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<kept>", |_, _child| {
        Ok(Value::Unit)
    })
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<claimed>", |_, _child| {
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);

    shell.arm_worker_retention(256);
    tick(&shell, 1);
    shell.local.workers.sweep_retention();
    assert_eq!(shell.local.workers.snapshot()[0].settled_epoch, Some(1));

    builtin_poll(&[Value::Handle(Box::new(handle))], &shell).expect("poll ok");
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
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let handle = spawn_child(&m, &mut shell, Birth::Spawn, "<live>", move |_, _c| {
        gate_rx.recv().unwrap();
        Ok(Value::Unit)
    })
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
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "live work leaves no reap notice"
    );

    gate_tx.send(()).unwrap();
    await_handle(&handle, &m, &shell).expect("await after the gate opens");
}

// ── the admission cap ────────────────────────────────────────────────

/// The cap refuses the (cap+1)th birth at the door, naming `await` and
/// `cancel` as remedies and registering nothing; cancelling one frees a seat.
#[test]
fn worker_cap_rejects_at_the_door_and_frees_on_cancel() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(2);

    let mut gates = Vec::new();
    let mut handles = Vec::new();
    for cmd in ["<one>", "<two>"] {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let handle = spawn_child(&m, &mut shell, Birth::Spawn, cmd, move |_, _c| {
            gate_rx.recv().unwrap();
            Ok(Value::Unit)
        })
        .expect("a birth under the cap must be admitted");
        gates.push(gate_tx);
        handles.push(handle);
    }
    assert_eq!(shell.local.workers.count(), 2);

    let refused = spawn_child(&m, &mut shell, Birth::Spawn, "<three>", |_, _c| {
        Ok(Value::Unit)
    });
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

    builtin_cancel(&[Value::Handle(Box::new(handles[0].clone()))], &shell).expect("cancel ok");
    spawn_child(&m, &mut shell, Birth::Spawn, "<after>", |_, _c| {
        Ok(Value::Unit)
    })
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(2);

    let mut gates = Vec::new();
    for (birth, cmd) in [(Birth::Service, "<service>"), (Birth::Spawn, "<block>")] {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        spawn_child(&m, &mut shell, birth, cmd, move |_, _c| {
            gate_rx.recv().unwrap();
            Ok(Value::Unit)
        })
        .expect("a birth under the cap must be admitted");
        gates.push(gate_tx);
    }

    let refused = spawn_child(&m, &mut shell, Birth::Spawn, "<three>", |_, _c| {
        Ok(Value::Unit)
    });
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
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(1);
    let first = spawn_child(&m, &mut shell, Birth::Spawn, "<done>", |_, _c| {
        Ok(Value::Unit)
    })
    .expect("the first birth is admitted");
    wait_settled(&first);
    assert_eq!(shell.local.workers.count(), 1, "the settled entry lingers");

    spawn_child(&m, &mut shell, Birth::Spawn, "<next>", |_, _c| {
        Ok(Value::Unit)
    })
    .expect("a lingering settled entry must not hold a seat");
}
