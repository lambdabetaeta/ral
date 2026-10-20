#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]

use super::eliminate::await_handle;
use super::*;
use crate::first_order::FOValue;
use crate::io::new_buffer;
use crate::process::{CancelCause, DurableRoot, Scope, ScopeNode, WorkerStop};
use crate::ty::{Grade, Ty};
use crate::types::{
    Break, Brood, BroodOwner, Cradle, DeferredSink, Error, Escape, HandleInner, Map, Owner,
    ReapCause, Sample, Status, ThreadEnd, WatchEvent, WorkerId,
};
use std::sync::{Arc, Mutex, mpsc};

fn status(b: Break) -> i32 {
    match b {
        Break::Error(e) => e.code(),
        other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
    }
}

/// `verb`'s refusal of a handle whose result was already claimed: an ordinary
/// error, not a stop.
fn expect_claimed(b: Break, verb: &str) {
    match b {
        Break::Error(e) => assert_eq!(
            e.message,
            format!("{verb}: this worker's result was already claimed")
        ),
        other @ Break::Escape(_) => panic!("expected the already-claimed error, got {other:?}"),
    }
}

/// A titled spawn's birth.
fn spawned(title: &str) -> Birth {
    Birth::Spawn {
        title: Some(title.into()),
    }
}

/// A handle whose worker dropped its settler unsettled, modelling a panic.
/// The buffers are pre-seeded so a settled outcome can be checked for them.
fn handle_with_panicked_worker(stdout: &[u8], stderr: &[u8]) -> HandleInner {
    let (_, out) = new_buffer();
    let (_, err) = new_buffer();
    out.append(stdout);
    err.append(stderr);
    let (handle, _) = HandleInner::new(
        WorkerId::mint(),
        "<test>",
        DurableRoot::default().worker(),
        out,
        err,
    );
    handle
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

/// A worker whose settler is dropped unsettled settles as a panic, not
/// `pending` — else `poll` reads `pending` forever and `race` spins.
#[test]
fn a_dropped_settler_settles_as_panicked() {
    let handle = handle_with_panicked_worker(b"", b"");
    match handle.observe(|completed| completed.end.clone()) {
        Sample::Settled(ThreadEnd::Panicked(text)) => {
            assert!(text.contains("spawned thread panicked"));
        }
        other => panic!("expected a settled panic, got {other:?}"),
    }
}

/// `poll` re-raises a panicked worker, as `await` does: a defect is not the
/// observer's to survive.
#[test]
fn a_polled_panic_is_reraised() {
    let shell = crate::test_helper::core_shell();
    let handle = handle_with_panicked_worker(b"out", b"err");
    let args = [Value::Handle(Box::new(handle))];
    let raised =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builtin_poll(&args, &shell)));
    assert!(raised.is_err(), "poll must re-raise a worker's panic");
}

/// `await` re-raises a panicked worker.
#[test]
fn an_awaited_panic_is_reraised() {
    let mut shell = crate::test_helper::core_shell();
    let handle = handle_with_panicked_worker(b"", b"");
    let raised = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        await_handle(&handle, "await", &mut shell)
    }));
    assert!(raised.is_err(), "await must re-raise a worker's panic");
}

/// `race` re-raises a panicked winner.
#[test]
fn a_raced_panic_is_reraised() {
    let mut shell = crate::test_helper::core_shell();
    let handle = handle_with_panicked_worker(b"", b"");
    let args = [Value::list(vec![Value::Handle(Box::new(handle))])];
    let raised = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        builtin_race(&args, &Mooring::adrift(), &mut shell)
    }));
    assert!(raised.is_err(), "race must re-raise a worker's panic");
}

/// `cancel` claims the handle: a later `poll` finds nothing left to observe.
#[test]
fn a_cancelled_handle_is_claimed() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), check_loop).unwrap();
    let args = [Value::Handle(Box::new(handle))];
    builtin_cancel(&args).unwrap();
    expect_claimed(
        builtin_poll(&args, &shell).expect_err("cancel claimed the handle"),
        "poll",
    );
}

/// A worker polling its own scope, reporting the
/// status it saw once `cancel_via` fires.  `ready` confirms it is alive
/// first, so a test pins propagation, not a worker that never ran.
fn spawn_polling_worker(shell: &Shell, cancel_via: impl FnOnce(&ScopeNode)) -> (i32, ScopeNode) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let node = shell.session.root.worker();
    let worker_mooring = Mooring::for_worker(&Mooring::adrift(), node.clone(), None);
    let _join = shell
        .spawn_thread(worker_mooring, "test-worker", move |_, child| {
            ready_tx.send(()).unwrap();
            loop {
                if let Err(b) = child.poll() {
                    done_tx.send(b.code()).unwrap();
                    return;
                }
                std::thread::yield_now();
            }
        })
        .expect("spawn_thread");
    ready_rx.recv().unwrap();
    cancel_via(&node);
    (done_rx.recv().unwrap(), node)
}

/// Cancelling a worker's own scope stops it and leaves a sibling from the
/// same shell alone — read off scopes no other test can reach, so no stray
/// cancellation can satisfy it.
#[test]
fn worker_scope_cancel_stops_the_worker() {
    let shell = crate::test_helper::core_shell();
    let sibling = shell.session.root.worker();
    let sibling_mooring = Mooring::for_worker(&Mooring::adrift(), sibling.clone(), None);
    let _idle_join = shell
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

/// A foreground cancel spares a worker: it parents under the
/// durable root, so a run timeout cannot reap work meant to outlive the run.
#[test]
fn foreground_cancel_spares_worker() {
    let shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let worker_scope = shell.session.root.worker();
    let worker_mooring = Mooring::for_worker(&m, worker_scope.clone(), None);
    let _join = shell
        .spawn_thread(worker_mooring, "test-worker", |_, _| ())
        .expect("spawn_thread");
    m.home.cancel(crate::process::CancelCause::Interrupted);
    assert!(
        !worker_scope.is_cancelled(),
        "a foreground cancel must not reach a worker"
    );
}

/// A blocked `await` unwinds on a foreground cancel instead of sleeping on a
/// bare `recv`, yet the root-parented worker it awaited stays awaitable.
#[test]
fn await_unwinds_on_foreground_cancel_sparing_the_worker() {
    let mut shell = crate::test_helper::core_shell();

    // A still-running worker, root-parented as a real `spawn`'s is: its
    // settler stays alive, unsettled.
    let (_sink, stdout_buf) = new_buffer();
    let (_sink2, stderr_buf) = new_buffer();
    let worker_scope = shell.session.root.worker();
    let (handle, _settler) = HandleInner::new(
        WorkerId::mint(),
        "<test>",
        worker_scope.clone(),
        stdout_buf,
        stderr_buf,
    );

    // Cancel the foreground (run deadline / interrupt), not the root.
    let m = Mooring::adrift();
    shell.scope = Scope::clone(&m.home);
    m.home.cancel(crate::process::CancelCause::Interrupted);

    let err = await_handle(&handle, "await", &mut shell)
        .expect_err("await must unwind on a foreground cancel, not block");
    assert!(matches!(err, Break::Escape(Escape::Stopped { .. })));
    assert!(
        !worker_scope.is_cancelled(),
        "the foreground cancel must unblock await without reaping the root-parented worker"
    );
}

/// A body that stays `Running` until cancelled, polling its shell so
/// a reap genuinely unwinds the thread rather than flag a scope nobody reads.
fn check_loop(_mooring: &Mooring, child: &mut Shell) -> Settled<Value> {
    loop {
        child.poll()?;
        std::thread::yield_now();
    }
}

/// Block until `handle`'s worker has settled it at exit.
fn wait_settled(handle: &HandleInner) {
    for _ in 0..500 {
        if handle.sample() == Sample::Settled(()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("worker never settled");
}

struct EchoDesk;
impl crate::types::EnquiryDesk for EchoDesk {
    fn enquire(
        &self,
        req: crate::first_order::FOValue,
        _scope: &crate::process::Scope,
    ) -> Settled<crate::first_order::FOValue> {
        Ok(req)
    }
}

/// Containment: a worker's own `enquire` answers the absence
/// error rather than reach the run's desk.
#[test]
fn spawned_worker_never_receives_the_enquiry_desk() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.desk = Some(Arc::new(EchoDesk) as crate::types::Desk);
    let (tx, rx) = mpsc::channel::<Settled<crate::first_order::FOValue>>();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), move |mooring, child| {
        let outcome = child.enquire(mooring, crate::first_order::FOValue::Unit);
        let _ = tx.send(outcome);
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);
    let outcome = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("worker must send its enquire outcome before settling");
    match outcome {
        Err(Break::Error(e)) => assert_eq!(e.message, crate::types::NO_DESK),
        Err(other) => panic!("expected the absence error, got {other:?}"),
        Ok(_) => panic!("a worker must never reach the spawning run's desk"),
    }
}

/// The nursery's twin of `spawned_worker_never_receives_the_enquiry_desk`:
/// the worker's mooring carries no fork door.
#[test]
fn spawned_worker_never_receives_the_nursery() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.fork = Some(crate::types::Fork::Park(crate::types::Nursery::default()));
    let (tx, rx) = mpsc::channel::<bool>();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), move |mooring, _| {
        let _ = tx.send(mooring.fork().is_none());
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);
    let doorless = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("worker must report its fork door before settling");
    assert!(
        doorless,
        "a worker must never reach the spawning run's nursery"
    );
}

/// `cancel` fires a worker's scope and removes its entry, and leaves no
/// reap notice: an explicit cancel is not policy.
#[test]
fn cancel_through_the_handle_ends_a_titled_worker() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle =
        spawn_child(&m, &mut shell, spawned("<serve>"), check_loop).expect("spawn must succeed");

    builtin_cancel(&[Value::Handle(Box::new(handle.clone()))])
        .expect("cancel must succeed on a running worker");

    assert!(
        handle.cancel.is_cancelled(),
        "cancel fires the worker's scope"
    );
    assert_eq!(
        handle.cancel.cause(),
        Some(crate::process::CancelCause::Cancelled)
    );
    assert_eq!(shell.local.workers.count(), 0, "cancel removes the entry");
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "an explicit cancel is not a reap: no notice"
    );
}

// ── `worker <id>` ────────────────────────────────────────────────────

/// The site of a `worker` call whose handle is used at `Handle ty`.
fn worker_site(ty: Ty) -> Arc<crate::ty::Site> {
    Arc::new(crate::ty::Site::snapshot(
        &crate::typecheck::Unifier::new(),
        &Ty::Handle(Grade::Value, Box::new(ty)),
        crate::ty::UnitId::fresh(),
    ))
}

/// `worker <id>` at a site using the handle at `Handle ty`.
fn reacquire(shell: &mut Shell, id: WorkerId, ty: Ty) -> Settled<HandleInner> {
    let id = Value::Int(i64::try_from(id.0).expect("a test's worker id fits an Int"));
    match builtin_worker(&[id], &worker_site(ty), &Mooring::adrift(), shell)? {
        Value::Handle(h) => Ok(*h),
        other => panic!("worker must return a Handle, got {other:?}"),
    }
}

/// A running worker is reacquired by its id, as the very handle registered.
#[test]
fn worker_reacquires_a_running_spawn_by_id() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<gated>"), check_loop).unwrap();
    let again = reacquire(&mut shell, handle.id, Ty::Unit).expect("a running worker resolves");
    assert_eq!(again, handle, "the registered handle comes back");
    handle.cancel.cancel(CancelCause::Cancelled);
}

/// A settled worker nothing has claimed still resolves, and its retained
/// result is what `await` delivers.
#[test]
fn worker_reacquires_a_settled_but_unclaimed_worker() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<answer>"), |_, _| {
        Ok(Value::Int(42))
    })
    .unwrap();
    wait_settled(&handle);
    let again = reacquire(&mut shell, handle.id, Ty::Int).expect("a retained worker resolves");
    let awaited =
        await_handle(&again, "await", &mut shell).expect("the retained result is delivered");
    assert_eq!(awaited, Value::Int(42));
}

/// A settled `poll` claims nothing: the polled worker still resolves by its
/// id, and `await` claims it there.
#[test]
fn worker_reacquires_a_polled_worker() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<answer>"), |_, _| {
        Ok(Value::Int(42))
    })
    .unwrap();
    wait_settled(&handle);
    let polled = builtin_poll(&[Value::Handle(Box::new(handle.clone()))], &shell)
        .expect("a settled poll observes");
    expect_variant(&polled, "settled");
    let again = reacquire(&mut shell, handle.id, Ty::Int).expect("a polled worker resolves");
    let awaited =
        await_handle(&again, "await", &mut shell).expect("the polled result is delivered");
    assert_eq!(awaited, Value::Int(42));
}

#[test]
fn worker_refuses_an_unknown_id() {
    let mut shell = crate::test_helper::core_shell();
    let err = match builtin_worker(
        &[Value::Int(999_999)],
        &worker_site(Ty::Unit),
        &Mooring::adrift(),
        &mut shell,
    ) {
        Err(Break::Error(e)) => e,
        other => panic!("an unknown id must error, got {other:?}"),
    };
    assert!(err.message.contains("no worker has id"), "{}", err.message);
}

/// `workers` lists a running and a retained worker, in birth order, as data.
#[test]
fn workers_lists_each_entry_as_data() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let gated = spawn_child(&m, &mut shell, spawned("gated"), check_loop).unwrap();
    let answer = spawn_child(&m, &mut shell, spawned("answer"), |_, _| Ok(Value::Int(42))).unwrap();
    wait_settled(&answer);
    let Value::List(rows) = builtin_workers(&shell) else {
        panic!("workers is a list");
    };
    let listed: Vec<(Value, String)> = rows
        .iter()
        .map(|row| {
            let Value::Map(r) = &*row else {
                panic!("a row is a record, got {row:?}");
            };
            let Value::Variant { label, .. } = &*r.get("state").expect("a state") else {
                panic!("a state is a variant");
            };
            (
                r.get("title").expect("a title").into_owned(),
                label.to_string(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (Value::String("gated".into()), "running".to_string()),
            (Value::String("answer".into()), "done".to_string()),
        ]
    );
    gated.cancel.cancel(CancelCause::Cancelled);
}

/// Two `worker` calls on one worker are two sites, and each observation
/// admits the worker's value against its own: what one script decides about
/// the answer is not what another decides.
#[test]
fn worker_twice_on_one_worker_admits_at_each_site() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<answer>"), |_, _| {
        Ok(Value::Int(42))
    })
    .unwrap();
    wait_settled(&handle);
    let as_number = reacquire(&mut shell, handle.id, Ty::Int).expect("resolves");
    let as_text = reacquire(&mut shell, handle.id, Ty::String).expect("resolves");
    builtin_poll(&[Value::Handle(Box::new(as_number))], &shell)
        .expect("the answer is a number, as the first site uses it");
    let refused = match await_handle(&as_text, "await", &mut shell) {
        Err(Break::Error(e)) => e,
        other => panic!("the second site uses the answer as text, got {other:?}"),
    };
    assert!(
        refused.message.contains("is a whole number") && refused.message.contains("as text"),
        "{}",
        refused.message
    );
}

/// Run `src` as one capturing top-level run on a shell the caller dressed,
/// under `worker_cap`, with the bytes it wrote to stdout. Panics on a static
/// failure — every source here is expected to compile.
fn run_captured(
    shell: &mut Shell,
    src: &str,
    worker_cap: Option<usize>,
) -> (Settled<Value>, Vec<u8>) {
    run_framed(
        shell,
        crate::protocol::Run {
            worker_cap,
            ..crate::protocol::Run::captured(src, "<test>")
        }
        .into(),
    )
}

/// [`run_captured`] under a frame, and with loans, the caller dressed.
fn run_framed(shell: &mut Shell, request: crate::run::RunRequest) -> (Settled<Value>, Vec<u8>) {
    use crate::run::RunReport;
    let src = format!("{:?}", request.run.program);
    match shell.run(request) {
        RunReport::Ran {
            ending, captured, ..
        } => (
            ending.into_result(),
            captured.map(|c| c.stdout).unwrap_or_default(),
        ),
        RunReport::Static { .. } => {
            panic!("well-formed source must run, not fail statically: {src}")
        }
        RunReport::Faulted { error } => panic!("the run faulted: {}", error.message),
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
/// fact carrying the spawn's own `title` and the minted `id` — so a later
/// reader can join the trail against the registry.
#[test]
fn audit_over_spawn_carries_the_birth_id_and_all() {
    let mut shell = crate::test_helper::core_shell();
    let report = run_captured(&mut shell, "audit { !{spawn { return 1 }} }", None)
        .0
        .expect("audit over a plain spawn must succeed");
    let birth = only_birth(&trail_of(&report));
    assert_eq!(
        birth.get("title").as_deref(),
        Some(&Value::string("block at <test>, line 1"))
    );
    assert!(
        matches!(birth.get("id").as_deref(), Some(Value::Int(_))),
        "the birth carries the minted worker id: {birth:?}"
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
    assert!(String::from_utf8_lossy(&printed).contains("Source: session"));
    let (awaited, replayed) = run_captured(
        &mut shell,
        "let h = !{spawn { explain sess_name }}\nawait $h",
        None,
    );
    awaited.expect("the worker must run explain");
    assert_eq!(replayed, printed);
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
    assert_eq!(
        expect_map(&awaited).get("v").as_deref(),
        Some(&Value::Int(7))
    );
}

// ── `await` is the block run here ────────────────────────────────────

/// `await` writes what the worker wrote to the awaiter's own streams, then
/// returns its value.
#[test]
fn await_replays_the_workers_bytes_then_returns_its_value() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "let h = spawn { echo out; warn err; return 4 }\n\
         let v = await $h\necho $v",
        None,
    );
    ended.expect("the block returned");
    assert_eq!(String::from_utf8_lossy(&stdout), "out\n4\n");
}

/// A handle's result is claimed once: a second `await` replays nothing and
/// fails, as an ordinary error.
#[test]
fn a_second_await_finds_the_result_claimed() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "let h = spawn { echo out; return 4 }\n\
         let v = await $h\necho $v\n\
         try { let _ = await $h } { |e| echo $e[message] }",
        None,
    );
    ended.expect("the second await's failure is caught");
    assert_eq!(
        String::from_utf8_lossy(&stdout),
        "out\n4\nawait: this worker's result was already claimed\n"
    );
}

/// A settled `poll` claims nothing: the handle stays awaitable, and the one
/// `await` replays its bytes once.
#[test]
fn a_settled_poll_leaves_the_handle_to_await() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "let h = spawn { echo out; return 4 }\n\
         let settle = { |h| case !{poll $h} [\n\
             `settled: { |s| return $s[outcome] },\n\
             `pending: { |_| sleep 0.01; settle $h }\n\
         ] }\n\
         let first = settle $h\n\
         let again = settle $h\n\
         await $h",
        None,
    );
    assert_eq!(ended.expect("the polled handle is awaited"), Value::Int(4));
    assert_eq!(String::from_utf8_lossy(&stdout), "out\n");
}

/// `cancel` after `await` is nothing: the result is claimed, so there is no
/// worker left for it to stop.
#[test]
fn cancel_after_await_is_nothing() {
    let mut shell = crate::test_helper::core_shell();
    let handle = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<answer>"),
        |_, _| Ok(Value::Int(42)),
    )
    .unwrap();
    assert_eq!(
        await_handle(&handle, "await", &mut shell).expect("awaited"),
        Value::Int(42)
    );
    let cancelled = builtin_cancel(&[Value::Handle(Box::new(handle.clone()))])
        .expect("cancel of a claimed handle succeeds");
    assert_eq!(cancelled, Value::Unit);
    assert!(
        !handle.cancel.is_cancelled(),
        "a claimed handle's worker is not stopped"
    );
}

/// A failed block's bytes are replayed before its failure is raised, as
/// they would have been written before it ran here.
#[test]
fn await_raises_a_failed_block_after_replaying() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "let h = spawn { echo partial; fail [status: 7, message: boom] }\nawait $h",
        None,
    );
    assert_eq!(status(ended.expect_err("the block failed")), 7);
    assert_eq!(String::from_utf8_lossy(&stdout), "partial\n");
}

/// `poll` reports the seconds a block has run on both arms: so far while it
/// runs, its whole run once it has settled.
#[test]
fn poll_reports_elapsed_on_both_arms() {
    let mut shell = crate::test_helper::core_shell();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let gated = move |_: &Mooring, _: &mut Shell| {
        gate_rx.recv().unwrap();
        Ok(Value::Unit)
    };
    let handle = spawn_child(&Mooring::adrift(), &mut shell, spawned("<gated>"), gated)
        .expect("spawn must succeed");
    let args = [Value::Handle(Box::new(handle.clone()))];
    let elapsed = |polled: &Value, arm: &str| match expect_map(expect_variant(polled, arm))
        .get("elapsed")
        .as_deref()
    {
        Some(Value::Float(secs)) => secs.get(),
        other => panic!("expected a Float `elapsed`, got {other:?}"),
    };

    let pending = builtin_poll(&args, &shell).expect("poll is total");
    assert!(elapsed(&pending, "pending") >= 0.0);

    gate_tx.send(()).unwrap();
    wait_settled(&handle);
    let settled = builtin_poll(&args, &shell).expect("poll is total");
    let ran = handle.elapsed().as_secs_f64();
    assert!(
        (elapsed(&settled, "settled") - ran).abs() < 0.05,
        "a settled poll reports the handle's run time, {ran}s"
    );
}

/// `race` skips a handle another caller claimed and races the rest.
#[test]
fn race_skips_a_claimed_handle() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let claimed = spawn_child(&m, &mut shell, spawned("<claimed>"), |_, _| {
        Ok(Value::Int(1))
    })
    .unwrap();
    await_handle(&claimed, "await", &mut shell).expect("awaited");
    let live = spawn_child(&m, &mut shell, spawned("<live>"), |_, _| Ok(Value::Int(2))).unwrap();
    let args = [Value::list(vec![
        Value::Handle(Box::new(claimed)),
        Value::Handle(Box::new(live)),
    ])];
    assert_eq!(
        builtin_race(&args, &m, &mut shell).expect("the live handle wins"),
        Value::Int(2)
    );
}

/// `race` over handles every one of which is claimed fails, stopping nothing.
#[test]
fn race_fails_when_every_handle_is_claimed() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let awaited = spawn_child(&m, &mut shell, spawned("<awaited>"), |_, _| {
        Ok(Value::Int(1))
    })
    .unwrap();
    await_handle(&awaited, "await", &mut shell).expect("awaited");
    let cancelled = spawn_child(&m, &mut shell, spawned("<cancelled>"), check_loop).unwrap();
    builtin_cancel(&[Value::Handle(Box::new(cancelled.clone()))]).unwrap();
    let args = [Value::list(vec![
        Value::Handle(Box::new(awaited)),
        Value::Handle(Box::new(cancelled)),
    ])];
    expect_claimed(
        builtin_race(&args, &m, &mut shell).expect_err("every handle is claimed"),
        "race",
    );
}

/// A handle claimed mid-wait is skipped; once the last pending one is
/// claimed too, `race` fails rather than wait on nothing.
#[test]
fn race_fails_when_every_handle_is_claimed_mid_wait() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let a = spawn_child(&m, &mut shell, spawned("<a>"), check_loop).unwrap();
    let b = spawn_child(&m, &mut shell, spawned("<b>"), check_loop).unwrap();
    let (outsider_a, outsider_b) = (a.clone(), b.clone());
    let outsider = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        outsider_a.claim_and_stop();
        std::thread::sleep(std::time::Duration::from_millis(50));
        outsider_b.claim_and_stop();
    });
    let args = [Value::list(vec![
        Value::Handle(Box::new(a)),
        Value::Handle(Box::new(b)),
    ])];
    expect_claimed(
        builtin_race(&args, &m, &mut shell).expect_err("both were claimed mid-wait"),
        "race",
    );
    outsider.join().unwrap();
}

#[test]
fn race_refuses_an_empty_list() {
    let mut shell = crate::test_helper::core_shell();
    match builtin_race(&[Value::list(vec![])], &Mooring::adrift(), &mut shell) {
        Err(Break::Error(e)) => assert!(e.message.contains("race: no handles"), "{}", e.message),
        other => panic!("an empty race must be refused, got {other:?}"),
    }
}

// ── first-settled ────────────────────────────────────────────────────

/// `first-settled` names the released one of two gated workers and claims
/// nothing: both stay awaitable.
#[test]
fn first_settled_names_the_first_to_settle() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let gated = |shell: &mut Shell, title: &str, v: i64| {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let handle = spawn_child(&m, shell, spawned(title), move |_, _| {
            gate_rx.recv().unwrap();
            Ok(Value::Int(v))
        })
        .unwrap();
        (handle, gate_tx)
    };
    let (a, open_a) = gated(&mut shell, "<a>", 1);
    let (b, open_b) = gated(&mut shell, "<b>", 2);
    open_b.send(()).unwrap();
    let args = [Value::list(vec![
        Value::Handle(Box::new(a.clone())),
        Value::Handle(Box::new(b.clone())),
    ])];
    assert_eq!(
        builtin_first_settled(&args, &shell).expect("b settles"),
        Value::Int(1)
    );
    open_a.send(()).unwrap();
    assert_eq!(
        await_handle(&a, "await", &mut shell).unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        await_handle(&b, "await", &mut shell).unwrap(),
        Value::Int(2)
    );
}

/// A handle another caller claimed is skipped.
#[test]
fn first_settled_skips_a_claimed_handle() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let claimed = spawn_child(&m, &mut shell, spawned("<claimed>"), |_, _| {
        Ok(Value::Int(1))
    })
    .unwrap();
    await_handle(&claimed, "await", &mut shell).expect("awaited");
    let live = spawn_child(&m, &mut shell, spawned("<live>"), |_, _| Ok(Value::Int(2))).unwrap();
    let args = [Value::list(vec![
        Value::Handle(Box::new(claimed)),
        Value::Handle(Box::new(live)),
    ])];
    assert_eq!(
        builtin_first_settled(&args, &shell).expect("the live handle settles"),
        Value::Int(1)
    );
}

#[test]
fn first_settled_fails_when_every_handle_is_claimed() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let awaited = spawn_child(&m, &mut shell, spawned("<awaited>"), |_, _| {
        Ok(Value::Int(1))
    })
    .unwrap();
    await_handle(&awaited, "await", &mut shell).expect("awaited");
    let cancelled = spawn_child(&m, &mut shell, spawned("<cancelled>"), check_loop).unwrap();
    builtin_cancel(&[Value::Handle(Box::new(cancelled.clone()))]).unwrap();
    let args = [Value::list(vec![
        Value::Handle(Box::new(awaited)),
        Value::Handle(Box::new(cancelled)),
    ])];
    expect_claimed(
        builtin_first_settled(&args, &shell).expect_err("every handle is claimed"),
        "first-settled",
    );
}

/// A foreground cancel unwinds the wait, and the handles stay unclaimed.
#[test]
fn first_settled_is_cancel_aware() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let running = spawn_child(&m, &mut shell, spawned("<running>"), check_loop).unwrap();
    let foreground = Mooring::adrift();
    shell.scope = Scope::clone(&foreground.home);
    foreground.home.cancel(CancelCause::Interrupted);
    let args = [Value::list(vec![Value::Handle(Box::new(running.clone()))])];
    let err =
        builtin_first_settled(&args, &shell).expect_err("a cancelled foreground cuts the wait");
    assert!(matches!(err, Break::Escape(Escape::Stopped { .. })));
    assert!(!running.is_claimed(), "first-settled claims nothing");
    running.claim_and_stop();
}

#[test]
fn first_settled_refuses_an_empty_list() {
    let shell = crate::test_helper::core_shell();
    match builtin_first_settled(&[Value::list(vec![])], &shell) {
        Err(Break::Error(e)) => assert!(
            e.message.contains("first-settled: no handles"),
            "{}",
            e.message
        ),
        other => panic!("an empty first-settled must be refused, got {other:?}"),
    }
}

// ── `detach` ─────────────────────────────────────────────────────────

#[cfg(unix)]
fn detach_test_shell(budget: u64) -> Shell {
    let mut shell = crate::HostSurface {
        statics: vec![crate::builtins::DETACH_BUILTIN],
        ..crate::HostSurface::default()
    }
    .shell(crate::terminal::TerminalState::default());
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

/// A description is legible or refused, before any birth.
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

/// A deferred-sink double for the agent host: records what it is handed.
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
/// the exarch decoder matches: an `{id, title, elapsed-ms, outcome}` map over a
/// closed variant.
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
fn worker_flushes_done_to_deferred_sink() {
    fn run(
        work: impl FnOnce(&Mooring, &mut Shell) -> Settled<Value> + Send + 'static,
    ) -> Vec<FOValue> {
        let mut shell = crate::test_helper::core_shell();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        // Hold the handle so the channel stays connected until the flush.
        let _handle = spawn_child(&m, &mut shell, spawned("<block>"), work).unwrap();
        let mut got = wait_for_batch(&batches);
        assert_eq!(got.len(), 1, "one batch per completed worker");
        let batch = got.pop().unwrap();
        let done = crate::types::DoneEvent::from_surface(batch.last().unwrap()).unwrap();
        assert_eq!(done.id, shell.local.workers.snapshot()[0].handle.id);
        batch
    }

    let ok = run(|_, _child| Ok(Value::Unit));
    assert_eq!(ok.len(), 1, "an empty body's batch is just the `done event");
    let done = &ok[0];
    assert_eq!(done_outcome_label(done), "ok");
    let fields = fo_expect_variant(done, "done");
    assert_eq!(
        fields.field("title").and_then(FOValue::as_str),
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
    let handle = spawn_child(&m, &mut shell, birth, |_, child| {
        child
            .write_stdout(b"one\ntwo")
            .map_err(|e| sig(e.to_string()))?;
        child
            .write_stderr(b"bad\r\n")
            .map_err(|e| sig(e.to_string()))?;
        Ok(Value::Unit)
    })
    .unwrap();
    await_handle(&handle, "await", &mut shell).expect("await ok");
    let lines: Vec<(String, String)> = batches
        .lock()
        .unwrap()
        .iter()
        .filter_map(|batch| match batch.as_slice() {
            [ev] => WatchEvent::from_surface(ev).map(|w| (w.label, w.line)),
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

    let handle = spawn_child(&m, &mut shell, spawned("<block>"), |mooring, _child| {
        if let Some(sink) = mooring.surface.as_ref() {
            sink.emit(&FOValue::Variant {
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
        FOValue::Variant {
            label: "card".into(),
            payload: None,
        },
        "the body's surface precedes the `done"
    );
    assert_eq!(done_outcome_label(&batch[1]), "panic");

    match handle.observe(|completed| completed.end.clone()) {
        Sample::Settled(ThreadEnd::Panicked(_)) => {}
        other => panic!("expected a panicked end, got {other:?}"),
    }
}

/// With no deferred sink installed a worker has no surface at all.
#[test]
fn no_deferred_sink_means_no_delivery() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    assert!(m.deferred.is_none(), "no deferred sink installed");
    let handle = spawn_child(&m, &mut shell, spawned("<block>"), |mooring, _child| {
        assert!(mooring.surface.is_none(), "a worker has no surface");
        Ok(Value::Unit)
    })
    .unwrap();

    await_handle(&handle, "await", &mut shell).expect("await ok");
}

// ── worker registry (pure bookkeeping, no policy) ────────────────────

/// `spawn_child` files exactly one session-owned entry with the spawn's own
/// title, and the registered handle is the returned one — by `Arc::ptr_eq`.
#[test]
fn spawn_child_registers_one_entry_with_matching_handle() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<test-cmd>"), |_, _child| {
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
    assert_eq!(snapshot[0].handle.title, "<test-cmd>");
    assert_eq!(snapshot[0].owner, Owner::Session);
    assert_eq!(
        snapshot[0].handle, handle,
        "the registered handle is the returned handle"
    );
}

/// Every claimer removes the worker's entry, while `poll`, on either arm,
/// leaves it: an entry leaves the registry when its result is claimed.
#[test]
fn claimers_remove_the_entry_and_poll_never_does() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();

    // `await` removes.
    let h1 = spawn_child(&m, &mut shell, spawned("<a>"), |_, _c| Ok(Value::Unit)).unwrap();
    await_handle(&h1, "await", &mut shell).expect("await ok");
    assert_eq!(shell.local.workers.count(), 0, "await removes its entry");

    // `cancel` removes.
    let h2 = spawn_child(&m, &mut shell, spawned("<b>"), |_, _c| Ok(Value::Unit)).unwrap();
    assert_eq!(shell.local.workers.count(), 1);
    builtin_cancel(&[Value::Handle(Box::new(h2))]).expect("cancel ok");
    assert_eq!(shell.local.workers.count(), 0, "cancel removes its entry");

    // A settled `poll` leaves it; the `await` after it removes.
    let h3 = spawn_child(&m, &mut shell, spawned("<c>"), |_, _c| Ok(Value::Unit)).unwrap();
    wait_settled(&h3);
    let polled = builtin_poll(&[Value::Handle(Box::new(h3.clone()))], &shell).unwrap();
    expect_variant(&polled, "settled");
    assert_eq!(
        shell.local.workers.count(),
        1,
        "a settled poll leaves its entry"
    );
    await_handle(&h3, "await", &mut shell).expect("await ok");
    assert_eq!(shell.local.workers.count(), 0, "the await after it removes");

    // Block the worker on its own channel so the sample is deterministically
    // `` `pending ``, with no timing guess.
    let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
    let h4 = spawn_child(&m, &mut shell, spawned("<d>"), move |_, _c| {
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
    await_handle(&h4, "await", &mut shell).expect("await ok");
}

/// `race` removes the winner and every cancelled loser: nothing lingers in
/// the registry once it returns.
#[test]
fn race_removes_winner_and_cancelled_losers() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let winner = spawn_child(&m, &mut shell, spawned("<winner>"), |_, _c| Ok(Value::Unit)).unwrap();

    // The losers block on their own channels, so the winner always settles
    // first and these two are cancelled.
    let (l1_tx, l1_rx) = mpsc::channel::<()>();
    let loser1 = spawn_child(&m, &mut shell, spawned("<loser1>"), move |_, _c| {
        let _ = l1_rx.recv();
        Ok(Value::Unit)
    })
    .unwrap();
    let (l2_tx, l2_rx) = mpsc::channel::<()>();
    let loser2 = spawn_child(&m, &mut shell, spawned("<loser2>"), move |_, _c| {
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
    builtin_race(&args, &Mooring::adrift(), &mut shell).expect("race must succeed");
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
        spawned("<outer>"),
        move |mooring, child_shell| {
            go_rx.recv().unwrap();
            let _inner = spawn_child(mooring, child_shell, spawned("<inner>"), |_, _c| {
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
    let handle = spawn_child(&m, &mut shell, spawned("<done>"), |_, _child| {
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
    assert_eq!(notices[0].id, stamped[0].handle.id);
    assert_eq!(notices[0].title, stamped[0].handle.title);
    assert_eq!(notices[0].cause, ReapCause::Retention);
}

/// A session's workers die with its shell: the drop cancels every registered
/// worker's scope through `LocalState`'s teardown, unaided.
#[test]
fn dropping_a_shell_cancels_its_workers() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let _handle = spawn_child(&m, &mut shell, spawned("<gated>"), move |_, _c| {
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
    let handle = spawn_child(&m, &mut shell, spawned("<kept>"), |_, _child| {
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

/// Claiming beats retention: a stamped entry is removed the moment `await`
/// claims it, so a later sweep finds nothing to expire.
#[test]
fn claiming_beats_retention() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<claimed>"), |_, _child| {
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);

    shell.arm_worker_retention(256);
    tick(&shell, 1);
    shell.local.workers.sweep_retention();
    assert_eq!(shell.local.workers.snapshot()[0].settled_epoch, Some(1));

    await_handle(&handle, "await", &mut shell).expect("await ok");
    assert_eq!(shell.local.workers.count(), 0, "await claims the entry");

    tick(&shell, 400);
    shell.local.workers.sweep_retention();
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "a claimed result leaves no retention notice"
    );
}

/// A claim made from another shell, whose registry never held the entry,
/// still keeps retention from reaping it: the sweep drops claimed entries
/// under its own lock.
#[test]
fn a_claimed_entry_is_never_reaped() {
    let mut shell = crate::test_helper::core_shell();
    let mut elsewhere = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<claimed>"), |_, _child| {
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);

    shell.arm_worker_retention(2);
    tick(&shell, 1);
    shell.local.workers.sweep_retention();
    await_handle(&handle, "await", &mut elsewhere).expect("await ok");

    tick(&shell, 2);
    shell.local.workers.sweep_retention();
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "a claimed result leaves no retention notice"
    );
    assert_eq!(shell.local.workers.count(), 0, "the claimed entry is gone");
}

/// A settled `poll` claims nothing, so a polled entry is retention's as any
/// unclaimed settled entry is.
#[test]
fn retention_reaps_a_polled_entry() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<polled>"), |_, _child| {
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);

    shell.arm_worker_retention(2);
    tick(&shell, 1);
    shell.local.workers.sweep_retention();
    builtin_poll(&[Value::Handle(Box::new(handle))], &shell).expect("poll ok");
    assert_eq!(shell.local.workers.count(), 1, "a settled poll leaves it");

    tick(&shell, 2);
    shell.local.workers.sweep_retention();
    assert_eq!(shell.local.workers.count(), 0, "retention reaps it");
    assert_eq!(shell.take_worker_reap_notices().len(), 1);
}

/// Retention bounds a settled entry, never live work: a running entry is never stamped or expired, at retention 0 or any epoch.
#[test]
fn running_entries_are_never_stamped_or_expired() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let handle = spawn_child(&m, &mut shell, spawned("<live>"), move |_, _c| {
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
    await_handle(&handle, "await", &mut shell).expect("await after the gate opens");
}

// ── the admission cap ────────────────────────────────────────────────

/// The cap refuses the (cap+1)th birth at the door, naming `await`, `cancel`
/// and `workers` and registering nothing; cancelling one frees a seat.
#[test]
fn worker_cap_rejects_at_the_door_and_frees_on_cancel() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(2);

    let mut gates = Vec::new();
    let mut handles = Vec::new();
    for cmd in ["<one>", "<two>"] {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let handle = spawn_child(&m, &mut shell, spawned(cmd), move |_, _c| {
            gate_rx.recv().unwrap();
            Ok(Value::Unit)
        })
        .expect("a birth under the cap must be admitted");
        gates.push(gate_tx);
        handles.push(handle);
    }
    assert_eq!(shell.local.workers.count(), 2);

    let refused = spawn_child(&m, &mut shell, spawned("<three>"), |_, _c| Ok(Value::Unit));
    let err = match refused {
        Err(Break::Error(e)) => e,
        other => panic!("the capped birth must be refused, got {other:?}"),
    };
    for remedy in ["`await`", "`cancel`", "`workers`"] {
        assert!(
            err.message.contains(remedy),
            "the refusal must name {remedy}: {}",
            err.message
        );
    }
    assert_eq!(
        shell.local.workers.count(),
        2,
        "a refused birth registers nothing"
    );

    builtin_cancel(&[Value::Handle(Box::new(handles[0].clone()))]).expect("cancel ok");
    spawn_child(&m, &mut shell, spawned("<after>"), |_, _c| Ok(Value::Unit))
        .expect("cancelling one frees a seat");

    // Unblock the parked workers so none outlives the test.
    for gate in gates {
        let _ = gate.send(());
    }
}

/// A titled worker holds a seat like any other: the cap counts running
/// entries, whatever their titles.
#[test]
fn a_titled_birth_counts_toward_the_cap() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(2);

    let mut gates = Vec::new();
    for title in ["<serve>", "<block>"] {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        spawn_child(&m, &mut shell, spawned(title), move |_, _c| {
            gate_rx.recv().unwrap();
            Ok(Value::Unit)
        })
        .expect("a birth under the cap must be admitted");
        gates.push(gate_tx);
    }

    let refused = spawn_child(&m, &mut shell, spawned("<three>"), |_, _c| Ok(Value::Unit));
    assert!(
        matches!(refused, Err(Break::Error(_))),
        "two running workers fill a cap of 2"
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
    let first = spawn_child(&m, &mut shell, spawned("<done>"), |_, _c| Ok(Value::Unit))
        .expect("the first birth is admitted");
    wait_settled(&first);
    assert_eq!(shell.local.workers.count(), 1, "the settled entry lingers");

    spawn_child(&m, &mut shell, spawned("<next>"), |_, _c| Ok(Value::Unit))
        .expect("a lingering settled entry must not hold a seat");
}

fn stopped_error(b: Break) -> Error {
    match b {
        Break::Error(e) => e,
        other @ Break::Escape(_) => panic!("expected an error, got {other:?}"),
    }
}

fn expect_worker_stop(e: &Error, stop: WorkerStop) {
    assert_eq!(e.status, Status::WorkerStopped(stop));
    assert_eq!(e.code(), i32::from(stop.cause().code()));
    assert_eq!(e.message, stop.message());
    let reason = crate::fact::Reason::from(&e.status);
    let want = match stop {
        WorkerStop::Cancelled => crate::fact::Reason::Cancelled,
        WorkerStop::TimedOut => crate::fact::Reason::TimedOut,
    };
    assert_eq!(reason, want);
}

/// `cancel` then `await`: `cancel` claimed the handle, so `await` finds it claimed.
#[test]
fn awaiting_a_cancelled_worker_finds_it_claimed() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), check_loop).unwrap();
    builtin_cancel(&[Value::Handle(Box::new(handle.clone()))]).unwrap();
    let err = await_handle(&handle, "await", &mut shell).expect_err("cancel claimed it");
    expect_claimed(err, "await");
}

/// A `cancel` from another thread while `await` is blocked claims the handle
/// from under the awaiter, which stops waiting and finds it claimed.
#[test]
fn a_cancel_during_await_claims_it_from_the_awaiter() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), check_loop).unwrap();
    let canceller = handle.clone();
    let join = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        canceller.claim_and_stop();
    });
    let err = await_handle(&handle, "await", &mut shell).expect_err("cancelled while awaited");
    join.join().unwrap();
    expect_claimed(err, "await");
}

/// A worker whose node is struck from elsewhere, with no claimer, ends with
/// its own `worker was cancelled`, and the `await` that claims it raises it.
#[test]
fn a_worker_stopped_unclaimed_is_its_awaiters_error() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), check_loop).unwrap();
    handle.cancel.cancel(CancelCause::Cancelled);
    let err = await_handle(&handle, "await", &mut shell).expect_err("stopped from elsewhere");
    expect_worker_stop(&stopped_error(err), WorkerStop::Cancelled);
}

fn exiting_worker(shell: &mut Shell) -> HandleInner {
    let handle = spawn_child(&Mooring::adrift(), shell, spawned("<test>"), |_, _| {
        Err(Break::Escape(Escape::Exit(3)))
    })
    .unwrap();
    wait_settled(&handle);
    handle
}

#[test]
fn awaiting_an_exited_worker_exits() {
    let mut shell = crate::test_helper::core_shell();
    let handle = exiting_worker(&mut shell);
    let err = await_handle(&handle, "await", &mut shell).expect_err("exit passes");
    assert!(matches!(err, Break::Escape(Escape::Exit(3))), "{err:?}");
}

#[test]
fn polling_an_exited_worker_exits() {
    let mut shell = crate::test_helper::core_shell();
    let handle = exiting_worker(&mut shell);
    let err = builtin_poll(&[Value::Handle(Box::new(handle))], &shell).expect_err("exit passes");
    assert!(matches!(err, Break::Escape(Escape::Exit(3))), "{err:?}");
}

/// A stop naming a node that is not the worker's passes the boundary, so
/// `await` and `poll` re-raise it.
#[test]
fn a_foreign_stop_is_reraised_by_await_and_poll() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let foreign = shell.session.root.id();
    let body = move |_: &Mooring, _: &mut Shell| {
        Err(Break::stopped(
            CancelCause::Terminated,
            Some(foreign.clone()),
        ))
    };
    let awaited = spawn_child(&m, &mut shell, spawned("<a>"), body.clone()).unwrap();
    let err = await_handle(&awaited, "await", &mut shell).expect_err("a foreign stop passes");
    assert!(
        matches!(
            err,
            Break::Escape(Escape::Stopped {
                cause: CancelCause::Terminated,
                ..
            })
        ),
        "{err:?}"
    );

    let polled = spawn_child(&m, &mut shell, spawned("<p>"), body).unwrap();
    wait_settled(&polled);
    let err = builtin_poll(&[Value::Handle(Box::new(polled))], &shell)
        .expect_err("a foreign stop passes");
    assert!(
        matches!(
            err,
            Break::Escape(Escape::Stopped {
                cause: CancelCause::Terminated,
                ..
            })
        ),
        "{err:?}"
    );
}

/// I6: a cancel struck anywhere inside the session, a parent worker's node
/// included, is the worker's own error; a stop the host owns passes.
#[test]
fn a_worker_converts_a_cancel_from_inside_the_session_and_passes_a_root_stop() {
    use super::birth::convert_own_stop;
    let root = DurableRoot::default();
    let parent = root.worker();
    let converted = convert_own_stop(
        Err(Break::stopped(CancelCause::Cancelled, Some(parent.id()))),
        &root,
    );
    match converted {
        Err(Break::Error(e)) => assert_eq!(e.status, Status::WorkerStopped(WorkerStop::Cancelled)),
        other => panic!("expected the worker's error, got {other:?}"),
    }
    let terminated = convert_own_stop(
        Err(Break::stopped(CancelCause::Terminated, Some(root.id()))),
        &root,
    );
    assert!(matches!(
        terminated,
        Err(Break::Escape(Escape::Stopped { .. }))
    ));
    let unnamed = convert_own_stop(Err(Break::stopped(CancelCause::Cancelled, None)), &root);
    assert!(matches!(
        unnamed,
        Err(Break::Escape(Escape::Stopped { .. }))
    ));
}

/// A stage's node under a worker's: the join names the outermost node
/// attaining it, the worker's, so its switch converts a stop minted below.
#[test]
fn a_stage_inside_a_worker_names_the_worker() {
    use super::birth::convert_own_stop;
    let root = DurableRoot::default();
    let node = root.worker();
    node.cancel(CancelCause::TimedOut);
    let inner = node.child();
    let (cause, owner) = inner.in_force().expect("the chain carries the cause");
    assert_eq!(owner, node.id());
    match convert_own_stop(Err(Break::stopped(cause, Some(owner))), &root) {
        Err(Break::Error(e)) => assert_eq!(e.status, Status::WorkerStopped(WorkerStop::TimedOut)),
        other => panic!("expected the worker's error, got {other:?}"),
    }
}

// ── ownership: the cradle and the brood ──────────────────────────────

/// Spin until `done` holds, for at most `secs` seconds.
fn within(secs: u64, done: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !done() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

fn nothing_runs(shell: &Shell) -> bool {
    shell
        .local
        .workers
        .snapshot()
        .iter()
        .all(|entry| !entry.handle.is_running())
}

/// I4: a worker's return awaits the children it left, so their bytes follow
/// its own and the parent settles only after them.
#[test]
fn a_birth_inside_a_worker_hangs_under_it_and_is_settled_by_it() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "let h = spawn \"outer\" { spawn \"inner\" { echo child; return 2 }; echo parent; return 1 }\n\
         await $h",
        None,
    );
    assert_eq!(ended.expect("the outer block returned"), Value::Int(1));
    assert_eq!(String::from_utf8_lossy(&stdout), "parent\nchild\n");
    assert_eq!(shell.local.workers.count(), 0, "both are claimed");
}

#[test]
fn a_childs_failure_is_its_parents() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, _) = run_captured(
        &mut shell,
        "let h = spawn \"outer\" { spawn \"inner\" { fail [status: 7, message: inner] }; return 1 }\n\
         await $h",
        None,
    );
    assert_eq!(status(ended.expect_err("the child failed")), 7);
}

/// I5: a child's node hangs under its parent's, so `cancel` on the parent
/// reaches it, and the parent's halt claims it.
#[test]
fn cancelling_a_worker_cancels_its_children() {
    let mut shell = crate::test_helper::core_shell();
    let (inner_tx, inner_rx) = mpsc::channel();
    let outer = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<outer>"),
        move |mooring, child| {
            let inner = spawn_child(mooring, child, spawned("<inner>"), check_loop)?;
            inner_tx.send(inner).unwrap();
            check_loop(mooring, child)
        },
    )
    .unwrap();
    let inner = inner_rx.recv().unwrap();
    let entry = shell.local.workers.lookup(inner.id).expect("registered");
    assert_eq!(entry.owner, Owner::Worker(outer.id));
    builtin_cancel(&[Value::Handle(Box::new(outer))]).unwrap();
    assert!(within(2, || inner.cancel.cause()
        == Some(CancelCause::Cancelled)
        && shell.local.workers.count() == 0));
}

/// A worker that polled its failing child still fails with it: `poll` claims
/// nothing, so the return rule joins the child and raises its failure.
#[test]
fn a_polled_childs_failure_is_still_its_parents() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, _) = run_captured(
        &mut shell,
        "let h = spawn \"outer\" {\n\
             let c = spawn \"inner\" { fail [status: 7, message: inner] }\n\
             let settle = { |h| case !{poll $h} [\n\
                 `settled: { |_| return () },\n\
                 `pending: { |_| sleep 0.01; settle $h }\n\
             ] }\n\
             settle $c\n\
             return 1\n\
         }\n\
         await $h",
        None,
    );
    assert_eq!(status(ended.expect_err("the polled child failed")), 7);
}

/// An outside `await` of a child, through `worker <id>`, and its parent's
/// join are mutually exclusive: exactly one of them claims the child, so its
/// bytes are replayed exactly once.  Which of them wins is left to the race,
/// not forced: the test asserts the invariant over whichever outcome occurs.
#[test]
fn an_outside_claim_and_the_parents_join_replay_once() {
    let mut shell = crate::test_helper::core_shell();
    let (sink, out) = new_buffer();
    shell.io.stdout = sink;
    let (inner_tx, inner_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let outer = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<outer>"),
        move |mooring, child| {
            let inner = spawn_child(mooring, child, spawned("<inner>"), move |_, inner| {
                gate_rx.recv().unwrap();
                inner.write_stdout(b"c")?;
                Ok(Value::Unit)
            })?;
            inner_tx.send(inner.id).unwrap();
            Ok(Value::Unit)
        },
    )
    .unwrap();
    let inner = reacquire(&mut shell, inner_rx.recv().unwrap(), Ty::Unit)
        .expect("the waiting child resolves");
    let opener = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        gate_tx.send(()).unwrap();
    });
    let outside = await_handle(&inner, "await", &mut shell);
    opener.join().unwrap();
    let by_outside = out.take();
    await_handle(&outer, "await", &mut shell).expect("the parent returns");
    let by_parent = out.take();
    match outside {
        Ok(_) => assert_eq!((&by_outside[..], &by_parent[..]), (&b"c"[..], &b""[..])),
        Err(e) => {
            expect_claimed(e, "await");
            assert_eq!((&by_outside[..], &by_parent[..]), (&b""[..], &b"c"[..]));
        }
    }
}

#[test]
fn a_halting_worker_ends_its_children_within_the_grace() {
    let mut shell = crate::test_helper::core_shell();
    let started = std::time::Instant::now();
    let (ended, _) = run_captured(
        &mut shell,
        "let h = spawn \"outer\" { spawn \"nap\" { sleep 30 }; fail [status: 3, message: halt] }\n\
         await $h",
        None,
    );
    assert_eq!(status(ended.expect_err("the outer block failed")), 3);
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(nothing_runs(&shell));
}

/// A run has no brood: its loose births hang at the session root, owned by
/// the session.
#[test]
fn a_top_level_birth_hangs_at_the_root() {
    let mut shell = crate::test_helper::core_shell();
    assert!(shell.local.cradle.brood.is_none());
    let handle = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<loose>"),
        check_loop,
    )
    .unwrap();
    assert_eq!(shell.local.workers.snapshot()[0].owner, Owner::Session);
    shell.session.root.cancel(CancelCause::Terminated);
    assert!(handle.cancel.is_cancelled(), "the root reaches it");
}

/// A worker's title is the one written, else the line that spawned it.
#[test]
fn a_spawn_is_named_by_its_title_or_its_line() {
    let mut shell = crate::test_helper::core_shell();
    let title = |shell: &mut Shell, src: &str| {
        let report = run_captured(shell, src, None)
            .0
            .expect("audit over a spawn must succeed");
        only_birth(&trail_of(&report))
            .get("title")
            .map(std::borrow::Cow::into_owned)
    };
    assert_eq!(
        title(&mut shell, "audit { spawn \"named\" { return 1 } }"),
        Some(Value::string("named"))
    );
    assert_eq!(
        title(&mut shell, "audit {\n echo x\n spawn { return 1 }\n}"),
        Some(Value::string("block at <test>, line 3"))
    );
}

#[test]
fn an_untitled_spawn_is_refused_where_titles_are_required() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_title = TitlePolicy::Required;
    let body = crate::types::block_over(&shell.env);
    match spawn_form(None, &body, &m, &mut shell) {
        Err(Break::Error(e)) => assert!(e.message.contains("requires a title"), "{}", e.message),
        other => panic!("an untitled spawn must be refused, got {other:?}"),
    }
    assert_eq!(shell.local.workers.count(), 0, "refused before any birth");
}

#[test]
fn a_title_must_be_legible() {
    let mut shell = crate::test_helper::core_shell();
    for src in ["spawn \"\" { return 1 }", "spawn \"a\nb\" { return 1 }"] {
        let Break::Error(e) = run_captured(&mut shell, src, None)
            .0
            .expect_err("an illegible title must be refused")
        else {
            panic!("an error expected");
        };
        assert!(
            e.message
                .contains("spawn: the title must be a non-empty single line"),
            "{}",
            e.message
        );
    }
    assert_eq!(shell.local.workers.count(), 0);
}

// ── race ─────────────────────────────────────────────────────────────

#[test]
fn race_drains_its_losers() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let winner = spawn_child(&m, &mut shell, spawned("<winner>"), |_, _| Ok(Value::Unit)).unwrap();
    let losers: Vec<HandleInner> = ["<a>", "<b>"]
        .map(|title| spawn_child(&m, &mut shell, spawned(title), check_loop).unwrap())
        .into();
    let mut handles = vec![Value::Handle(Box::new(winner))];
    handles.extend(losers.iter().map(|h| Value::Handle(Box::new(h.clone()))));
    builtin_race(&[Value::list(handles)], &m, &mut shell).expect("race must succeed");
    assert!(
        losers.iter().all(HandleInner::has_ended),
        "drained before race returns"
    );
}

/// A wait cut by a stop still ends everything `race` was given.
#[test]
fn a_cut_race_ends_what_it_was_given() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let given: Vec<HandleInner> = ["<a>", "<b>"]
        .map(|title| spawn_child(&m, &mut shell, spawned(title), check_loop).unwrap())
        .into();
    let m2 = Mooring::adrift();
    shell.scope = Scope::clone(&m2.home);
    m2.home.cancel(crate::process::CancelCause::Interrupted);
    let handles = given
        .iter()
        .map(|h| Value::Handle(Box::new(h.clone())))
        .collect();
    let err = builtin_race(&[Value::list(handles)], &m, &mut shell)
        .expect_err("a cancelled foreground cuts the wait");
    assert!(matches!(err, Break::Escape(Escape::Stopped { .. })));
    assert!(
        given.iter().all(HandleInner::has_ended),
        "drained before race returns"
    );
    assert_eq!(shell.local.workers.count(), 0);
}

// ── a gather's ownership ─────────────────────────────────────────────

/// A gather's child is registered, so the cap bounds it, but its birth is
/// the gather's business, not announced.
#[test]
fn gather_owned_children_are_registered_but_not_announced() {
    let mut shell = crate::test_helper::core_shell();
    let report = run_captured(
        &mut shell,
        "audit { gather { let h = spawn \"g\" { return 1 } } }",
        None,
    )
    .0
    .expect("audit over gather must succeed");
    assert!(
        births(&trail_of(&report)).is_empty(),
        "a gather's child is not announced"
    );

    run_captured(
        &mut shell,
        "let h = spawn \"gate\" { gather { spawn \"g\" { sleep 30 } } }",
        None,
    )
    .0
    .expect("the spawn returns at once");
    let owners = |shell: &Shell| -> Vec<Owner> {
        shell
            .local
            .workers
            .snapshot()
            .iter()
            .map(|e| e.owner)
            .collect()
    };
    assert!(within(2, || owners(&shell).contains(&Owner::Gather)));
    let gate = shell
        .local
        .workers
        .snapshot()
        .into_iter()
        .find(|e| e.owner == Owner::Session)
        .expect("the spawn is registered")
        .handle;
    builtin_cancel(&[Value::Handle(Box::new(gate))]).unwrap();
    assert!(within(2, || shell.local.workers.count() == 0));
}

/// Bug 21: retention leaves a running worker's settled child to the parent
/// that will join it.
#[test]
fn retention_spares_a_running_workers_settled_child() {
    let mut shell = crate::test_helper::core_shell();
    shell.arm_worker_retention(1);
    let (inner_tx, inner_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let outer = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<outer>"),
        move |mooring, child| {
            let inner = spawn_child(mooring, child, spawned("<inner>"), |_, _| Ok(Value::Int(2)))?;
            inner_tx.send(inner).unwrap();
            gate_rx.recv().unwrap();
            Ok(Value::Unit)
        },
    )
    .unwrap();
    let inner = inner_rx.recv().unwrap();
    wait_settled(&inner);
    for _ in 0..3 {
        tick(&shell, 5);
        shell.local.workers.sweep_retention();
    }
    assert_eq!(shell.local.workers.count(), 2, "the settled child is kept");
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "nothing is reaped"
    );

    gate_tx.send(()).unwrap();
    await_handle(&outer, "await", &mut shell).expect("the parent joins its child");
    assert_eq!(shell.local.workers.count(), 0);
}

/// Retention leaves a gather's settled child to the gather that will join it.
#[test]
fn retention_spares_a_gathers_settled_child() {
    let mut shell = crate::test_helper::core_shell();
    shell.arm_worker_retention(1);
    let brood = Brood::new(BroodOwner::Gather);
    shell.local.cradle = Cradle::owning(shell.scope.clone(), brood.clone());
    let child = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<gathered>"),
        |_, _| Ok(Value::Int(2)),
    )
    .unwrap();
    wait_settled(&child);
    for _ in 0..2 {
        tick(&shell, 1);
        shell.local.workers.sweep_retention();
    }
    assert_eq!(shell.local.workers.count(), 1, "the settled child is kept");
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "nothing is reaped"
    );

    exit_brood(
        &brood,
        Ok(Value::Unit),
        std::time::Duration::ZERO,
        &mut shell,
    )
    .expect("the gather joins its child");
    assert!(child.is_claimed());
    assert_eq!(shell.local.workers.count(), 0);
}

/// I4 for a gather: its return awaits the children it left, so their bytes
/// follow its own.  Were the child the session's, the gather would return
/// before it.
#[test]
fn a_gather_joins_its_children_on_return() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "gather { spawn \"c\" { echo child; return 2 }; echo parent; return 1 }",
        None,
    );
    assert_eq!(ended.expect("the gather returned"), Value::Int(1));
    assert_eq!(String::from_utf8_lossy(&stdout), "parent\nchild\n");
    assert_eq!(shell.local.workers.count(), 0, "the child is claimed");
}

/// The halt rule under a stop: a wall striking the body parked in `sleep`
/// cancels and drains the gather's child before the run ends.
#[test]
fn a_walled_gather_drains_its_children_within_the_grace() {
    let mut shell = crate::test_helper::core_shell();
    let started = std::time::Instant::now();
    let (ended, _) = run_framed(
        &mut shell,
        crate::protocol::Run {
            wall: Some(std::time::Duration::from_millis(50)),
            ..crate::protocol::Run::captured(
                "gather { spawn \"c\" { sleep 30 }; sleep 30 }",
                "<test>",
            )
        }
        .into(),
    );
    assert!(matches!(
        ended.expect_err("the wall stops the run"),
        Break::Escape(Escape::Stopped {
            cause: CancelCause::TimedOut,
            ..
        })
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert_eq!(shell.local.workers.count(), 0, "the child is claimed");
}

#[test]
fn a_gather_childs_failure_is_the_gathers() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, _) = run_captured(
        &mut shell,
        "gather { spawn \"c\" { fail [status: 7, message: c] }; return 1 }",
        None,
    );
    assert_eq!(status(ended.expect_err("the child failed")), 7);
}

#[test]
fn a_halting_gather_drains_its_children_within_the_grace() {
    let mut shell = crate::test_helper::core_shell();
    let started = std::time::Instant::now();
    let (ended, _) = run_captured(
        &mut shell,
        "gather { spawn \"nap\" { sleep 30 }; fail [status: 3, message: halt] }",
        None,
    );
    assert_eq!(status(ended.expect_err("the gather failed")), 3);
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(nothing_runs(&shell));
}

/// I5 for a gather: its child hangs under the run's scope, so a stop striking
/// the run while the body is parked on the child reaches the child itself.
#[test]
fn a_gather_cancels_its_children_on_an_interrupt() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    shell.scope = Scope::clone(&m.home);
    let brood = Brood::new(BroodOwner::Gather);
    shell.local.cradle = Cradle::owning(shell.scope.clone(), brood.clone());
    let child = spawn_child(&m, &mut shell, spawned("<parked>"), check_loop).unwrap();
    let run = m.home.clone();
    let striker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        run.cancel(CancelCause::Interrupted);
    });
    let err = await_handle(&child, "await", &mut shell).expect_err("the stop cuts the wait");
    striker.join().unwrap();
    assert!(matches!(err, Break::Escape(Escape::Stopped { .. })));
    assert!(
        child.cancel.is_cancelled(),
        "the run's stop reaches the child"
    );
    assert!(within(6, || child.has_ended()));
    assert!(nothing_runs(&shell));
    brood.end(std::time::Duration::ZERO);
}

/// I4′: a gather's exit waits for a child the body cancelled, though the
/// `cancel` claimed it.  The child outlives its cancel by a beat, as an
/// external's teardown does, so a gather that did not wait returns first.
#[test]
fn a_cancelled_child_is_drained_at_the_gathers_exit() {
    let mut shell = crate::test_helper::core_shell();
    let brood = Brood::new(BroodOwner::Gather);
    shell.local.cradle = Cradle::owning(shell.scope.clone(), brood.clone());
    let child = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<slow>"),
        |_, child| loop {
            if let Err(stop) = child.poll() {
                std::thread::sleep(std::time::Duration::from_millis(100));
                return Err(stop);
            }
            std::thread::yield_now();
        },
    )
    .unwrap();
    builtin_cancel(&[Value::Handle(Box::new(child.clone()))]).unwrap();
    let started = std::time::Instant::now();
    exit_brood(
        &brood,
        Ok(Value::Unit),
        std::time::Duration::from_secs(5),
        &mut shell,
    )
    .expect("the gather returns");
    assert!(child.has_ended(), "the exit waited for the cancelled child");
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
}

/// D-3: a worker's own child is its owner's to deliver, so one unit makes one
/// birth in the trail and one `` `done `` on the deferred sink.
#[test]
fn a_spawn_inside_a_worker_is_not_announced() {
    let mut shell = crate::test_helper::core_shell();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let report = run_framed(
        &mut shell,
        crate::run::RunRequest {
            deferred: Some(Arc::new(RecDeferred(batches.clone()))),
            ..crate::protocol::Run::captured(
                "audit { let h = spawn \"outer\" { spawn \"inner\" { return 1 } }; await $h }",
                "<test>",
            )
            .into()
        },
    )
    .0
    .expect("audit over the spawn must succeed");
    assert_eq!(
        only_birth(&trail_of(&report)).get("title").as_deref(),
        Some(&Value::string("outer"))
    );
    let dones = batches
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .filter(|ev| crate::types::DoneEvent::from_surface(ev).is_some())
        .count();
    assert_eq!(dones, 1, "only the outer worker reports its completion");
}

/// A claimed, running child stays enrolled for I4′; a claimed, ended one is
/// pruned at the next `enrol`.
#[test]
fn a_brood_prunes_only_a_claimed_and_ended_child() {
    let brood = Brood::new(BroodOwner::Gather);
    let (running, _settler) = {
        let (_, out) = new_buffer();
        let (_, err) = new_buffer();
        HandleInner::new(
            WorkerId::mint(),
            "<running>",
            DurableRoot::default().worker(),
            out,
            err,
        )
    };
    let ended = handle_with_panicked_worker(b"", b"");
    for child in [&running, &ended] {
        child.claim_and_stop();
        brood.enrol(child.clone());
    }
    let next = handle_with_panicked_worker(b"", b"");
    brood.enrol(next.clone());
    assert_eq!(brood.children(), [running, next]);
}

// ── prelude combinators over gather ──────────────────────────────────

/// The liveness half; `tests/builtins/combinators.ral` pins the value.
#[test]
fn first_of_takes_the_winner_and_drains_losers() {
    let mut shell = crate::Shell::root(
        crate::boot::boot_runtime(
            crate::terminal::TerminalState::default(),
            crate::boot::BakedPrelude::runtime(),
            &crate::HostSurface::default(),
        )
        .into(),
    );
    let started = std::time::Instant::now();
    let (ended, _) = run_captured(
        &mut shell,
        "first-of [{ sleep 30; return 2 }, { return 1 }]",
        None,
    );
    assert_eq!(ended.expect("the winner returned"), Value::Int(1));
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(nothing_runs(&shell));
}
