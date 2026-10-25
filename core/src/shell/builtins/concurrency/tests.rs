use super::eliminate::await_handle;
use super::*;
use crate::data::FOValue;
use crate::io::new_buffer;
use crate::process::{CancelCause, DurableRoot, Scope, ScopeNode, WorkerStop};
use crate::shell::runtime::{
    Break, BroodOwner, BuiltinBody, BuiltinEntry, Cradle, DeferredSink, Error, Escape, HandleInner,
    Map, Owner, ReapCause, Sample, ScopeOwner, Status, ThreadEnd, WatchEvent, WorkerId,
};
use std::sync::{Arc, Mutex, mpsc};

/// Release-on-drop gate guard so a panicking assertion never parks a worker
/// thread for the rest of the test binary.
struct GateGuard(Option<mpsc::Sender<()>>);

impl GateGuard {
    fn new() -> (Self, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        (Self(Some(tx)), rx)
    }

    fn release(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.release();
    }
}

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

/// A handle whose worker dropped its completion permit unfinished, modelling a
/// panic. The buffers are pre-seeded so a ready outcome can be checked for them.
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

/// A worker whose completion permit is dropped unfinished publishes a panic,
/// not `running` — else `poll` reads `running` forever and `race` spins.
#[test]
fn a_dropped_settler_settles_as_panicked() {
    let handle = handle_with_panicked_worker(b"", b"");
    match handle.observe(|completed| completed.end.clone()) {
        Sample::Ready(ThreadEnd::Panicked(text)) => {
            assert!(text.contains("spawned thread panicked"));
        }
        other => panic!("expected a ready panic, got {other:?}"),
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

/// `cancel` claims the handle: `poll` observes `` `stopping `` while the worker
/// is still running and `` `consumed `` once it ends (each with `[elapsed]`),
/// while `await` fails because the result was already claimed.
#[test]
fn a_cancelled_handle_is_claimed() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (mut gate, gate_rx) = GateGuard::new();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), move |_, _| {
        let _ = gate_rx.recv();
        Ok(Value::Int(7))
    })
    .unwrap();
    let args = [Value::Handle(Box::new(handle.clone()))];
    builtin_cancel(&args).unwrap();
    assert_eq!(handle.sample(), Sample::Stopping);

    let stopping = builtin_poll(&args, &shell).expect("poll on a stopping handle is total");
    let stopping_map = expect_map(expect_variant(&stopping, "stopping"));
    assert_eq!(stopping_map.len(), 1);
    assert!(matches!(
        stopping_map.get("elapsed").as_deref(),
        Some(Value::Float(secs)) if secs.get() >= 0.0
    ));
    expect_claimed(
        await_handle(&handle, "await", &mut shell).expect_err("cancel claimed the handle"),
        "await",
    );

    gate.release();
    wait_ended(&handle);
    assert_eq!(handle.sample(), Sample::Consumed);

    let consumed = builtin_poll(&args, &shell).expect("poll on a consumed handle is total");
    let consumed_map = expect_map(expect_variant(&consumed, "consumed"));
    assert_eq!(consumed_map.len(), 1);
    assert!(matches!(
        consumed_map.get("elapsed").as_deref(),
        Some(Value::Float(secs)) if secs.get() >= 0.0
    ));
    expect_claimed(
        await_handle(&handle, "await", &mut shell).expect_err("cancel claimed the handle"),
        "await",
    );
}

/// A worker polling its own scope, reporting the
/// status it saw once `cancel_via` fires.  `ready` confirms it is alive
/// first, so a test pins propagation, not a worker that never ran.
fn spawn_polling_worker(shell: &Shell, cancel_via: impl FnOnce(&ScopeNode)) -> (i32, ScopeNode) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let node = shell.session.root.worker();
    let worker_mooring = Mooring::for_thread(&Mooring::adrift(), node.clone(), None);
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
    let sibling_mooring = Mooring::for_thread(&Mooring::adrift(), sibling.clone(), None);
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
    let worker_mooring = Mooring::for_thread(&m, worker_scope.clone(), None);
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
    // permit stays alive, unfinished.
    let (_sink, stdout_buf) = new_buffer();
    let (_sink2, stderr_buf) = new_buffer();
    let worker_scope = shell.session.root.worker();
    let (handle, _permit) = HandleInner::new(
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

/// Block until `handle`'s worker has published a `Ready` completion.
fn wait_settled(handle: &HandleInner) {
    for _ in 0..500 {
        if handle.sample() == Sample::Ready(()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("worker never settled to Ready: {:?}", handle.sample());
}

/// Block until `handle`'s worker has ended (`Ready` or `Consumed`).
fn wait_ended(handle: &HandleInner) {
    for _ in 0..500 {
        if handle.sample().has_ended() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("worker never ended: {:?}", handle.sample());
}

struct EchoDesk;
impl crate::shell::runtime::EnquiryDesk for EchoDesk {
    fn enquire(
        &self,
        req: crate::data::FOValue,
        _scope: &crate::process::Scope,
    ) -> Settled<crate::data::FOValue> {
        Ok(req)
    }
}

/// Containment: a worker's own `enquire` answers the absence
/// error rather than reach the run's desk.
#[test]
fn spawned_worker_never_receives_the_enquiry_desk() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.desk = Some(Arc::new(EchoDesk) as crate::shell::runtime::Desk);
    let (tx, rx) = mpsc::channel::<Settled<crate::data::FOValue>>();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), move |mooring, child| {
        let outcome = child.enquire(mooring, crate::data::FOValue::Unit);
        let _ = tx.send(outcome);
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);
    let outcome = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("worker must send its enquire outcome before settling");
    match outcome {
        Err(Break::Error(e)) => assert_eq!(e.message, crate::shell::runtime::NO_DESK),
        Err(other) => panic!("expected the absence error, got {other:?}"),
        Ok(_) => panic!("a worker must never reach the spawning run's desk"),
    }
}

/// The nursery's twin of `spawned_worker_never_receives_the_enquiry_desk`:
/// the worker's mooring carries no fork handle.
#[test]
fn spawned_worker_never_receives_the_nursery() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.fork = Some(crate::shell::runtime::Fork::Park(
        crate::shell::runtime::Nursery::default(),
    ));
    let (tx, rx) = mpsc::channel::<bool>();
    let handle = spawn_child(&m, &mut shell, spawned("<test>"), move |mooring, _| {
        let _ = tx.send(mooring.fork().is_none());
        Ok(Value::Unit)
    })
    .expect("spawn must succeed");
    wait_settled(&handle);
    let forkless = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("worker must report its fork before settling");
    assert!(
        forkless,
        "a worker must never reach the spawning run's nursery"
    );
}

/// `cancel` fires a worker's scope, transitions it through `stopping` to
/// `consumed` once it ends, removes its entry, and leaves no reap notice: an
/// explicit cancel is not policy.
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
    wait_ended(&handle);
    assert_eq!(handle.sample(), Sample::Consumed);
    assert_eq!(
        shell.local.workers.count(),
        0,
        "once ended, a cancelled worker is consumed and pruned"
    );
    assert!(
        shell.take_worker_reap_notices().is_empty(),
        "an explicit cancel is not a reap: no notice"
    );
}

// ── `workers` listing ────────────────────────────────────────────────

/// `workers` lists running, stopping, and ready workers in id order as data,
/// and omits a cancelled worker once its execution ends (`consumed`).
#[test]
fn workers_lists_each_entry_as_data() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (mut gate, gate_rx) = GateGuard::new();
    let gated = spawn_child(&m, &mut shell, spawned("gated"), move |_, _| {
        let _ = gate_rx.recv();
        Ok(Value::Unit)
    })
    .unwrap();
    let answer = spawn_child(&m, &mut shell, spawned("answer"), |_, _| Ok(Value::Int(42))).unwrap();
    wait_settled(&answer);

    let list_states = |shell: &Shell| -> Vec<(Value, String)> {
        let Value::List(rows) = builtin_workers(shell) else {
            panic!("workers is a list");
        };
        rows.iter()
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
            .collect()
    };

    assert_eq!(
        list_states(&shell),
        [
            (Value::String("gated".into()), "running".to_string()),
            (Value::String("answer".into()), "ready".to_string()),
        ]
    );

    builtin_cancel(&[Value::Handle(Box::new(gated.clone()))]).unwrap();
    assert_eq!(gated.sample(), Sample::Stopping);
    assert_eq!(
        list_states(&shell),
        [
            (Value::String("gated".into()), "stopping".to_string()),
            (Value::String("answer".into()), "ready".to_string()),
        ]
    );

    gate.release();
    wait_ended(&gated);
    assert_eq!(gated.sample(), Sample::Consumed);
    assert_eq!(
        list_states(&shell),
        [(Value::String("answer".into()), "ready".to_string())]
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
            ..crate::protocol::Run::captured(src, crate::source::SourceOrigin::named("<test>"))
        }
        .into(),
    )
}

/// [`run_captured`] under a frame, and with loans, the caller dressed.
fn run_framed(
    shell: &mut Shell,
    request: crate::shell::run::RunRequest,
) -> (Settled<Value>, Vec<u8>) {
    use crate::shell::run::RunReport;
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
fn trail_of(report: &Value) -> crate::shell::runtime::List {
    match expect_map(report)
        .get("trail")
        .map(std::borrow::Cow::into_owned)
    {
        Some(Value::List(trail)) => trail,
        other => panic!("expected a trail List, got {other:?}"),
    }
}

/// The `` `worker `` facts of a trail — a birth is read by its tag.
fn births(trail: &crate::shell::runtime::List) -> Vec<Map> {
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
fn only_birth(trail: &crate::shell::runtime::List) -> Map {
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

/// A ready `poll` claims nothing: the handle stays awaitable, and the one
/// `await` replays its bytes once.
#[test]
fn a_settled_poll_leaves_the_handle_to_await() {
    let mut shell = crate::test_helper::core_shell();
    let (ended, stdout) = run_captured(
        &mut shell,
        "let h = spawn { echo out; return 4 }\n\
         let settle = { |h| case !{poll $h} [\n\
             `ready: { |s| return $s[outcome] },\n\
             `running: { |_| sleep 0.01; settle $h },\n\
             `stopping: { |_| fail [status: 1, message: \"unexpected stopping\"] },\n\
             `consumed: { |_| fail [status: 1, message: \"unexpected consumed\"] }\n\
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

/// `poll` reports the seconds a block has run on every arm: so far while it
/// runs or stops, its whole run once it is ready or consumed.
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

    let running = builtin_poll(&args, &shell).expect("poll is total");
    assert!(elapsed(&running, "running") >= 0.0);

    gate_tx.send(()).unwrap();
    wait_settled(&handle);
    let ready = builtin_poll(&args, &shell).expect("poll is total");
    let ran = handle.elapsed().as_secs_f64();
    assert!(
        (elapsed(&ready, "ready") - ran).abs() < 0.05,
        "a ready poll reports the handle's run time, {ran}s"
    );

    await_handle(&handle, "await", &mut shell).expect("await ok");
    let consumed = builtin_poll(&args, &shell).expect("poll is total");
    assert!(
        (elapsed(&consumed, "consumed") - ran).abs() < 0.05,
        "a consumed poll reports the handle's frozen run time, {ran}s"
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
        statics: vec![crate::shell::builtins::DETACH_BUILTIN],
        ..crate::HostSurface::default()
    }
    .shell(
        crate::io::terminal::TerminalState::default(),
        crate::test_helper::process_cwd(),
    )
    .expect("a test host resolves its baseline");
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
        let done = crate::shell::runtime::DoneEvent::from_surface(batch.last().unwrap()).unwrap();
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
        Sample::Ready(ThreadEnd::Panicked(_)) => {}
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

/// Every claimer removes the worker's entry once its execution ends, while
/// `poll`, on every non-consumed arm, leaves it.
#[test]
fn claimers_remove_the_entry_and_poll_never_does() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();

    // `await` removes.
    let h1 = spawn_child(&m, &mut shell, spawned("<a>"), |_, _c| Ok(Value::Unit)).unwrap();
    await_handle(&h1, "await", &mut shell).expect("await ok");
    assert_eq!(shell.local.workers.count(), 0, "await removes its entry");

    // `cancel` removes once the worker has ended.
    let h2 = spawn_child(&m, &mut shell, spawned("<b>"), |_, _c| Ok(Value::Unit)).unwrap();
    assert_eq!(shell.local.workers.count(), 1);
    builtin_cancel(&[Value::Handle(Box::new(h2.clone()))]).expect("cancel ok");
    wait_ended(&h2);
    assert_eq!(shell.local.workers.count(), 0, "cancel removes its entry");

    // A ready `poll` leaves it; the `await` after it removes.
    let h3 = spawn_child(&m, &mut shell, spawned("<c>"), |_, _c| Ok(Value::Unit)).unwrap();
    wait_settled(&h3);
    let polled = builtin_poll(&[Value::Handle(Box::new(h3.clone()))], &shell).unwrap();
    expect_variant(&polled, "ready");
    assert_eq!(
        shell.local.workers.count(),
        1,
        "a ready poll leaves its entry"
    );
    await_handle(&h3, "await", &mut shell).expect("await ok");
    assert_eq!(shell.local.workers.count(), 0, "the await after it removes");

    // Block the worker on its own channel so the sample is deterministically
    // `` `running ``, with no timing guess.
    let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
    let h4 = spawn_child(&m, &mut shell, spawned("<d>"), move |_, _c| {
        unblock_rx.recv().unwrap();
        Ok(Value::Unit)
    })
    .unwrap();
    assert_eq!(shell.local.workers.count(), 1);
    let running = builtin_poll(&[Value::Handle(Box::new(h4.clone()))], &shell).unwrap();
    assert!(
        matches!(&running, Value::Variant { label, .. } if label.as_ref() == "running"),
        "the worker is blocked, so poll must observe it running: got {running:?}"
    );
    assert_eq!(
        shell.local.workers.count(),
        1,
        "a running poll must not touch the registry"
    );
    unblock_tx.send(()).unwrap();
    await_handle(&h4, "await", &mut shell).expect("await ok");
}

/// `race` removes the winner and every cancelled loser once drained: nothing
/// lingers in the registry once it returns.
#[test]
fn race_removes_winner_and_cancelled_losers() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.cleanup_grace = std::time::Duration::from_secs(5);
    let winner = spawn_child(&m, &mut shell, spawned("<winner>"), |_, _c| Ok(Value::Unit)).unwrap();

    let loser1 = spawn_child(&m, &mut shell, spawned("<loser1>"), check_loop).unwrap();
    let loser2 = spawn_child(&m, &mut shell, spawned("<loser2>"), check_loop).unwrap();
    assert_eq!(shell.local.workers.count(), 3);

    let args = [Value::list(vec![
        Value::Handle(Box::new(winner)),
        Value::Handle(Box::new(loser1.clone())),
        Value::Handle(Box::new(loser2.clone())),
    ])];
    builtin_race(&args, &m, &mut shell).expect("race must succeed");
    assert!(loser1.has_ended() && loser2.has_ended());
    assert_eq!(
        shell.local.workers.count(),
        0,
        "race removes the winner and both cancelled losers"
    );
}

/// The flow rule: a `spawn` nested in a worker's body registers into the
/// *same* registry the owning shell reads, not a copy.
#[test]
fn nested_spawn_registers_into_the_owning_shells_registry() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<usize>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
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
            ready_tx.send(child_shell.local.workers.count()).unwrap();
            // The body's exit joins `<inner>`, claiming it out of the registry.
            let _ = done_rx.recv();
            Ok(Value::Unit)
        },
    )
    .unwrap();
    // A worker's thread starts before its entry is filed.
    go_tx.send(()).unwrap();

    assert_eq!(ready_rx.recv().unwrap(), 2, "the worker's shell sees both");
    assert_eq!(
        shell.local.workers.count(),
        2,
        "the parent's shell sees both"
    );
    done_tx.send(()).unwrap();
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

/// The cap refuses the (cap+1)th birth at the entry, naming `workers` and
/// `cancel` and registering nothing; a cancelled worker still holds its seat
/// while `stopping` and frees it once execution ends.
#[test]
fn worker_cap_rejects_at_the_entry_and_frees_on_cancel() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(2);

    let mut gates = Vec::new();
    let mut handles = Vec::new();
    for cmd in ["<one>", "<two>"] {
        let (gate, gate_rx) = GateGuard::new();
        let handle = spawn_child(&m, &mut shell, spawned(cmd), move |_, _c| {
            let _ = gate_rx.recv();
            Ok(Value::Unit)
        })
        .expect("a birth under the cap must be admitted");
        gates.push(gate);
        handles.push(handle);
    }
    assert_eq!(shell.local.workers.count(), 2);

    let refused = spawn_child(&m, &mut shell, spawned("<three>"), |_, _c| Ok(Value::Unit));
    let err = match refused {
        Err(Break::Error(e)) => e,
        other => panic!("the capped birth must be refused, got {other:?}"),
    };
    for remedy in ["still executing or stopping", "`workers`", "cancel"] {
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
    assert_eq!(handles[0].sample(), Sample::Stopping);
    assert!(
        spawn_child(&m, &mut shell, spawned("<while-stopping>"), |_, _c| Ok(
            Value::Unit
        ))
        .is_err(),
        "a stopping worker still holds its seat until its execution ends"
    );

    gates.remove(0).release();
    wait_ended(&handles[0]);
    assert_eq!(handles[0].sample(), Sample::Consumed);
    spawn_child(&m, &mut shell, spawned("<after>"), |_, _c| Ok(Value::Unit))
        .expect("once the cancelled worker ends, its seat is freed");
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
    let entry = shell
        .local
        .workers
        .snapshot()
        .into_iter()
        .find(|e| e.handle.id == inner.id)
        .expect("registered");
    assert_eq!(entry.owner, Owner::Worker(outer.id));
    builtin_cancel(&[Value::Handle(Box::new(outer.clone()))]).unwrap();
    assert!(within(2, || inner.cancel.cause()
        == Some(CancelCause::Cancelled)
        && outer.has_ended()
        && inner.has_ended()
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
                 `ready: { |_| return () },\n\
                 `running: { |_| sleep 0.01; settle $h },\n\
                 `stopping: { |_| fail [status: 1, message: \"unexpected stopping\"] },\n\
                 `consumed: { |_| fail [status: 1, message: \"unexpected consumed\"] }\n\
             ] }\n\
             settle $c\n\
             return 1\n\
         }\n\
         await $h",
        None,
    );
    assert_eq!(status(ended.expect_err("the polled child failed")), 7);
}

/// An outside `await` of a child handle and its parent's join are mutually
/// exclusive: exactly one of them claims the child, so its bytes are replayed
/// exactly once.  Which of them wins is left to the race, not forced: the test
/// asserts the invariant over whichever outcome occurs.
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
            inner_tx.send(inner).unwrap();
            Ok(Value::Unit)
        },
    )
    .unwrap();
    let inner = inner_rx.recv().unwrap();
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
    assert_eq!(shell.local.workers.count(), 0);
    assert_eq!(Arc::strong_count(&shell.local.workers.live_ticket()), 2);
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
    let body = crate::shell::runtime::block_over(&shell.env);
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
    let owner = ScopeOwner::new(BroodOwner::Gather);
    shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());
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
        &owner,
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
                crate::source::SourceOrigin::named("<test>"),
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
    assert_eq!(shell.local.workers.count(), 0);
    assert_eq!(Arc::strong_count(&shell.local.workers.live_ticket()), 2);
}

/// I5 for a gather: its child hangs under the run's scope, so a stop striking
/// the run while the body is parked on the child reaches the child itself.
#[test]
fn a_gather_cancels_its_children_on_an_interrupt() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    shell.scope = Scope::clone(&m.home);
    let owner = ScopeOwner::new(BroodOwner::Gather);
    shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());
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
    owner.end(std::time::Duration::ZERO);
    assert_eq!(shell.local.workers.count(), 0);
}

/// I4′: a gather's exit waits for a child the body cancelled, though the
/// `cancel` claimed it.  The child outlives its cancel by a beat, as an
/// external's teardown does, so a gather that did not wait returns first.
#[test]
fn a_cancelled_child_is_drained_at_the_gathers_exit() {
    let mut shell = crate::test_helper::core_shell();
    let owner = ScopeOwner::new(BroodOwner::Gather);
    shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());
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
        &owner,
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
        crate::shell::run::RunRequest {
            deferred: Some(Arc::new(RecDeferred(batches.clone()))),
            ..crate::protocol::Run::captured(
                "audit { let h = spawn \"outer\" { spawn \"inner\" { return 1 } }; await $h }",
                crate::source::SourceOrigin::named("<test>"),
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
        .filter(|ev| crate::shell::runtime::DoneEvent::from_surface(ev).is_some())
        .count();
    assert_eq!(dones, 1, "only the outer worker reports its completion");
}

/// A claimed, running (`stopping`) child stays enrolled for I4′; a claimed,
/// ended (`consumed`) one is pruned when the owner closes (or compacts).
#[test]
fn a_brood_prunes_only_a_claimed_and_ended_child() {
    let owner = ScopeOwner::new(BroodOwner::Gather);
    let brood = owner.brood();
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
        brood.enrol(child.clone()).unwrap();
    }
    let next = handle_with_panicked_worker(b"", b"");
    brood.enrol(next.clone()).unwrap();
    owner.close();
    assert_eq!(owner.children(), [running, next]);
}

// ── prelude combinators over gather ──────────────────────────────────

/// The liveness half; `tests/builtins/combinators.ral` pins the value.
#[test]
fn first_of_takes_the_winner_and_drains_losers() {
    let mut shell = crate::Shell::root(
        crate::shell::boot::boot_runtime(
            crate::io::terminal::TerminalState::default(),
            crate::shell::boot::BakedPrelude::runtime(),
            &crate::HostSurface::default(),
        )
        .expect("a test host resolves its baseline")
        .into(),
        crate::test_helper::process_cwd(),
    );
    let started = std::time::Instant::now();
    let (ended, _) = run_captured(
        &mut shell,
        "first-of [{ sleep 30; return 2 }, { return 1 }]",
        None,
    );
    assert_eq!(ended.expect("the winner returned"), Value::Int(1));
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert_eq!(shell.local.workers.count(), 0);
    assert_eq!(Arc::strong_count(&shell.local.workers.live_ticket()), 2);
}

// ── lifecycle & ownership regressions (§11.1–11.4) ───────────────────

/// When `exit_brood` panics while joining a panicked child, `ScopeOwner::drop`
/// still stops every remaining sibling — both in a top-level `gather` and in a
/// nested `gather` inside a worker.
#[test]
fn gather_exit_join_panic_cancels_remaining_siblings_top_level_and_nested() {
    // 1. Top-level gather join panic with a gated sibling.
    {
        let mut shell = crate::test_helper::core_shell();
        let owner = ScopeOwner::new(BroodOwner::Gather);
        shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());
        let m = Mooring::adrift();

        let panicked = spawn_child(&m, &mut shell, spawned("<panicked>"), |_, _| {
            panic!("child boom")
        })
        .unwrap();
        wait_settled(&panicked);

        let (mut gate, gate_rx) = GateGuard::new();
        let sibling = spawn_child(&m, &mut shell, spawned("<sibling>"), move |_, _| {
            let _ = gate_rx.recv();
            Ok(Value::Unit)
        })
        .unwrap();
        assert_eq!(sibling.sample(), Sample::Running(()));

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let owner = owner;
            exit_brood(
                &owner,
                Ok(Value::Unit),
                std::time::Duration::from_secs(1),
                &mut shell,
            )
        }));
        assert!(unwind.is_err(), "joining the panicked child must unwind");
        assert_eq!(
            sibling.sample(),
            Sample::Stopping,
            "ScopeOwner::drop must claim and stop the remaining sibling on unwind"
        );
        assert!(sibling.cancel.is_cancelled());

        gate.release();
        wait_ended(&sibling);
        assert_eq!(sibling.sample(), Sample::Consumed);
    }

    // 2. Nested gather inside a worker: child panics at gather exit while its
    //    sibling is gated.
    {
        let mut shell = crate::test_helper::core_shell();
        let m = Mooring::adrift();
        let (sib_tx, sib_rx) = mpsc::channel::<HandleInner>();
        let (mut gate, gate_rx) = GateGuard::new();

        let outer = spawn_child(&m, &mut shell, spawned("<outer>"), move |mooring, child| {
            let gather_owner = ScopeOwner::new(BroodOwner::Gather);
            let prev = std::mem::replace(
                &mut child.local.cradle,
                Cradle::owning(child.scope.clone(), gather_owner.brood()),
            );
            let p = spawn_child(mooring, child, spawned("<nested-panic>"), |_, _| {
                panic!("nested child boom")
            })?;
            wait_settled(&p);
            let s = spawn_child(mooring, child, spawned("<nested-sibling>"), move |_, _| {
                let _ = gate_rx.recv();
                Ok(Value::Unit)
            })?;
            sib_tx.send(s).unwrap();
            child.local.cradle = prev;
            exit_brood(&gather_owner, Ok(Value::Unit), mooring.cleanup_grace, child)
        })
        .unwrap();

        let sibling = sib_rx.recv().unwrap();
        wait_settled(&outer);
        assert_eq!(
            sibling.sample(),
            Sample::Stopping,
            "nested gather's ScopeOwner::drop must stop its gated sibling when join unwinds"
        );
        assert!(sibling.cancel.is_cancelled());

        gate.release();
        wait_ended(&sibling);
        assert_eq!(sibling.sample(), Sample::Consumed);
        match outer.observe(|c| c.end.clone()) {
            Sample::Ready(ThreadEnd::Panicked(msg)) => {
                assert!(msg.contains("nested child boom"), "{msg}");
            }
            other => panic!("expected outer to settle as panicked, got {other:?}"),
        }
    }
}

/// If the `gather` body itself panics before reaching `exit_brood`, dropping
/// `ScopeOwner` closes membership and stops every enrolled child.
#[test]
fn gather_body_panic_cancels_enrolled_children_via_scope_owner_drop() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let (mut gate, gate_rx) = GateGuard::new();
    let mut child_slot: Option<HandleInner> = None;

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let owner = ScopeOwner::new(BroodOwner::Gather);
        shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());
        let child = spawn_child(&m, &mut shell, spawned("<gated>"), move |_, _| {
            let _ = gate_rx.recv();
            Ok(Value::Unit)
        })
        .unwrap();
        child_slot = Some(child);
        panic!("gather body exploded");
    }));
    assert!(unwind.is_err());

    let child = child_slot.expect("child was spawned before body panic");
    assert_eq!(child.sample(), Sample::Stopping);
    assert!(child.cancel.is_cancelled());

    gate.release();
    wait_ended(&child);
    assert_eq!(child.sample(), Sample::Consumed);
}

/// Handle completion (`CompletionPermit::finish`) happens before deferred sink
/// delivery and outside `HandleInner`'s mutex: a panicking `DeferredSink`
/// cannot poison the handle, and a blocked `DeferredSink` cannot delay `Ready`
/// or `await`.
#[test]
fn deferred_sink_panic_or_blocking_does_not_affect_handle_completion() {
    struct PanickingDeferred;
    impl DeferredSink for PanickingDeferred {
        fn deliver(&self, _batch: Vec<FOValue>) {
            panic!("deferred sink exploded");
        }
    }

    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.deferred = Some(Arc::new(PanickingDeferred));
    let handle = spawn_child(&m, &mut shell, spawned("<ok>"), |_, _| Ok(Value::Int(99))).unwrap();
    assert_eq!(
        await_handle(&handle, "await", &mut shell).expect("handle settles despite sink panic"),
        Value::Int(99)
    );
    assert_eq!(handle.sample(), Sample::Consumed);

    struct BlockingDeferred {
        entered_tx: Mutex<Option<mpsc::Sender<()>>>,
        release_rx: Mutex<mpsc::Receiver<()>>,
    }
    impl DeferredSink for BlockingDeferred {
        fn deliver(&self, _batch: Vec<FOValue>) {
            let tx = self.entered_tx.lock().unwrap().take();
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
            let _ = self.release_rx.lock().unwrap().recv();
        }
    }

    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (mut release_guard, release_rx) = GateGuard::new();
    let mut m_block = Mooring::adrift();
    m_block.deferred = Some(Arc::new(BlockingDeferred {
        entered_tx: Mutex::new(Some(entered_tx)),
        release_rx: Mutex::new(release_rx),
    }));

    let gated_sink_handle = spawn_child(&m_block, &mut shell, spawned("<fast>"), |_, _| {
        Ok(Value::Int(77))
    })
    .unwrap();
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("worker enters DeferredSink::deliver");

    // Even while `deliver` is blocked, the handle is already `Ready` and can be
    // awaited to `Consumed`.
    assert_eq!(gated_sink_handle.sample(), Sample::Ready(()));
    assert_eq!(
        await_handle(&gated_sink_handle, "await", &mut shell)
            .expect("await succeeds while sink blocked"),
        Value::Int(77)
    );
    assert_eq!(gated_sink_handle.sample(), Sample::Consumed);

    release_guard.release();
}

/// Unterminated `watch` lines on both stdout and stderr are flushed even when
/// the watched body panics or when joining a child at worker exit panics.
#[test]
fn watch_flushes_partial_lines_on_body_panic_and_child_join_panic() {
    // 1. Body panics after writing unterminated stdout and stderr bytes.
    {
        let mut shell = crate::test_helper::core_shell();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let birth = Birth::Watch {
            label: "job".into(),
        };
        let _handle = spawn_child(&m, &mut shell, birth, |_, child| {
            child.write_stdout(b"partial-out").unwrap();
            child.write_stderr(b"partial-err").unwrap();
            panic!("body exploded");
        })
        .unwrap();

        assert!(within(5, || {
            batches
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .any(|ev| crate::shell::runtime::DoneEvent::from_surface(ev).is_some())
        }));

        let got = batches.lock().unwrap().clone();
        let watch_lines: Vec<(String, String)> = got
            .iter()
            .flatten()
            .filter_map(WatchEvent::from_surface)
            .map(|w| (w.label, w.line))
            .collect();
        assert_eq!(
            watch_lines,
            vec![
                ("job".to_string(), "partial-out".to_string()),
                ("job:err".to_string(), "partial-err".to_string()),
            ]
        );
        let done = got
            .iter()
            .flatten()
            .find_map(crate::shell::runtime::DoneEvent::from_surface)
            .expect("trailing DoneEvent");
        assert_eq!(
            done.outcome,
            crate::shell::runtime::Done::Panic("body exploded".into())
        );
    }

    // 2. Child join panics during `exit_brood` after the parent wrote a partial line.
    {
        let mut shell = crate::test_helper::core_shell();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let birth = Birth::Watch {
            label: "parent".into(),
        };
        let _handle = spawn_child(&m, &mut shell, birth, |mooring, child| {
            child.write_stdout(b"parent-partial").unwrap();
            let c = spawn_child(mooring, child, spawned("<boom-child>"), |_, _| {
                panic!("child exploded")
            })?;
            wait_settled(&c);
            Ok(Value::Unit)
        })
        .unwrap();

        assert!(within(5, || {
            batches
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .any(|ev| crate::shell::runtime::DoneEvent::from_surface(ev).is_some())
        }));

        let got = batches.lock().unwrap().clone();
        let watch_lines: Vec<(String, String)> = got
            .iter()
            .flatten()
            .filter_map(WatchEvent::from_surface)
            .map(|w| (w.label, w.line))
            .collect();
        assert_eq!(
            watch_lines,
            vec![("parent".to_string(), "parent-partial".to_string())]
        );
        let done = got
            .iter()
            .flatten()
            .find_map(crate::shell::runtime::DoneEvent::from_surface)
            .expect("trailing DoneEvent");
        assert!(
            matches!(&done.outcome, crate::shell::runtime::Done::Panic(msg) if msg.contains("child exploded")),
            "unexpected done outcome: {:?}",
            done.outcome
        );
    }
}

/// Watch flush isolation, deterministic panic precedence
/// (`computation > stdout flush > stderr flush`), and no duplicate or trailing
/// flush when a whole-line callback panics mid-write.
#[test]
fn watch_flush_panic_isolation_and_precedence_and_no_duplicate_flush() {
    struct SelectivePanicSink {
        seen_watch: Mutex<Vec<(String, String)>>,
        dones: Mutex<Vec<crate::shell::runtime::DoneEvent>>,
        panic_on_stdout: Option<&'static str>,
        panic_on_stderr: Option<&'static str>,
    }

    impl DeferredSink for SelectivePanicSink {
        fn deliver(&self, batch: Vec<FOValue>) {
            for ev in &batch {
                if let Some(w) = WatchEvent::from_surface(ev) {
                    self.seen_watch
                        .lock()
                        .unwrap()
                        .push((w.label.clone(), w.line.clone()));
                    if w.label == "job"
                        && let Some(msg) = self.panic_on_stdout
                    {
                        panic!("{msg}");
                    }
                    if w.label == "job:err"
                        && let Some(msg) = self.panic_on_stderr
                    {
                        panic!("{msg}");
                    }
                }
                if let Some(d) = crate::shell::runtime::DoneEvent::from_surface(ev) {
                    self.dones.lock().unwrap().push(d);
                }
            }
        }
    }

    let run_case = |body_panic: Option<&'static str>,
                    panic_on_stdout: Option<&'static str>,
                    panic_on_stderr: Option<&'static str>| {
        let mut shell = crate::test_helper::core_shell();
        let sink = Arc::new(SelectivePanicSink {
            seen_watch: Mutex::new(Vec::new()),
            dones: Mutex::new(Vec::new()),
            panic_on_stdout,
            panic_on_stderr,
        });
        let mut m = Mooring::adrift();
        m.deferred = Some(sink.clone());
        let birth = Birth::Watch {
            label: "job".into(),
        };
        let handle = spawn_child(&m, &mut shell, birth, move |_, child| {
            child.write_stdout(b"out-part").unwrap();
            child.write_stderr(b"err-part").unwrap();
            if let Some(msg) = body_panic {
                panic!("{msg}");
            }
            Ok(Value::Unit)
        })
        .unwrap();

        assert!(within(5, || !sink.dones.lock().unwrap().is_empty()));
        wait_settled(&handle);
        let end = match handle.observe(|c| c.end.clone()) {
            Sample::Ready(end) => end,
            other => panic!("expected Ready handle, got {other:?}"),
        };
        let seen = sink.seen_watch.lock().unwrap().clone();
        let done = sink.dones.lock().unwrap().pop().unwrap();
        (end, seen, done)
    };

    // Case 1: computation panic beats both stdout and stderr flush panics, and
    // stdout flush panic does not skip stderr flush.
    let (end1, seen1, done1) = run_case(
        Some("comp-panic"),
        Some("stdout-flush-panic"),
        Some("stderr-flush-panic"),
    );
    assert_eq!(
        seen1,
        vec![
            ("job".to_string(), "out-part".to_string()),
            ("job:err".to_string(), "err-part".to_string()),
        ],
        "panicking stdout flush must not skip stderr partial-line flush"
    );
    assert!(matches!(end1, ThreadEnd::Panicked(ref s) if s == "comp-panic"));
    assert_eq!(
        done1.outcome,
        crate::shell::runtime::Done::Panic("comp-panic".into())
    );

    // Case 2: when the computation succeeds, stdout flush panic beats stderr flush panic.
    let (end2, seen2, done2) =
        run_case(None, Some("stdout-flush-panic"), Some("stderr-flush-panic"));
    assert_eq!(seen2.len(), 2);
    assert!(matches!(end2, ThreadEnd::Panicked(ref s) if s == "stdout-flush-panic"));
    assert_eq!(
        done2.outcome,
        crate::shell::runtime::Done::Panic("stdout-flush-panic".into())
    );

    // Case 3: when computation and stdout flush succeed, stderr flush panic surfaces.
    let (end3, seen3, done3) = run_case(None, None, Some("stderr-flush-panic"));
    assert_eq!(seen3.len(), 2);
    assert!(matches!(end3, ThreadEnd::Panicked(ref s) if s == "stderr-flush-panic"));
    assert_eq!(
        done3.outcome,
        crate::shell::runtime::Done::Panic("stderr-flush-panic".into())
    );

    // Case 4: a panicking whole-line callback during `write_stdout(b"a\nb\nc")`
    // drains the buffer before invoking the callback, so `"a"` is not retried
    // and `"b\nc"` is not flushed during post-body finalization.
    let mut shell = crate::test_helper::core_shell();
    let sink = Arc::new(SelectivePanicSink {
        seen_watch: Mutex::new(Vec::new()),
        dones: Mutex::new(Vec::new()),
        panic_on_stdout: Some("line-callback-panic"),
        panic_on_stderr: None,
    });
    let mut m = Mooring::adrift();
    m.deferred = Some(sink.clone());
    let birth = Birth::Watch {
        label: "job".into(),
    };
    let handle = spawn_child(&m, &mut shell, birth, |_, child| {
        child.write_stdout(b"a\nb\nc").unwrap();
        Ok(Value::Unit)
    })
    .unwrap();
    assert!(within(5, || !sink.dones.lock().unwrap().is_empty()));
    wait_settled(&handle);
    assert_eq!(
        *sink.seen_watch.lock().unwrap(),
        vec![("job".to_string(), "a".to_string())],
        "whole-line panic on `a` must not retry `a` or flush `b`/`c`"
    );
}

/// `spawn_child` on a closed brood cancels the child, keeps it registered
/// while `Stopping`, suppresses session-owned birth/done announcements, and
/// fails with `<form>: enclosing scope has already closed`.
#[test]
fn late_enrolment_on_closed_brood_cancels_and_reports_clean_error() {
    // `spawn_child` for both `Birth::Spawn` and `Birth::Watch` against a closed brood.
    for (birth, expected_msg) in [
        (
            spawned("<late-spawn>"),
            "spawn: the enclosing gather has ended; the new worker was cancelled",
        ),
        (
            Birth::Watch {
                label: "late-watch".into(),
            },
            "watch: the enclosing gather has ended; the new worker was cancelled",
        ),
    ] {
        let mut shell = crate::test_helper::core_shell();
        let owner = ScopeOwner::new(BroodOwner::Gather);
        owner.close();
        shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());

        let batches = Arc::new(Mutex::new(Vec::new()));
        let mut m = Mooring::adrift();
        m.deferred = Some(Arc::new(RecDeferred(batches.clone())));
        let trail_scope = shell
            .local
            .audit
            .open(crate::shell::runtime::CapturePolicy::default());

        let (mut gate, gate_rx) = GateGuard::new();
        let res = spawn_child(&m, &mut shell, birth, move |_, _| {
            let _ = gate_rx.recv();
            Ok(Value::Unit)
        });
        let Break::Error(err) = res.expect_err("closed brood must refuse birth") else {
            panic!("expected Break::Error");
        };
        assert_eq!(err.message, expected_msg);

        // While gated, the rejected child is `Stopping` with cancellation
        // signalled and remains in `WorkerRegistry` until its thread ends.
        let snapshot = shell.local.workers.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].handle.sample(), Sample::Stopping);
        assert!(snapshot[0].handle.cancel.is_cancelled());

        gate.release();
        wait_ended(&snapshot[0].handle);
        assert_eq!(snapshot[0].handle.sample(), Sample::Consumed);
        assert_eq!(shell.local.workers.count(), 0);

        let observations = shell.local.audit.close(trail_scope);
        let has_birth = observations
            .iter()
            .any(|o| matches!(o.what, crate::fact::Observed::Worker { .. }));
        assert!(!has_birth, "closed-brood birth is not session-owned");
        assert!(
            batches.lock().unwrap().is_empty(),
            "closed-brood birth emits no deferred DoneEvent"
        );
    }
}

/// Both `exit_brood` on a halting body and `builtin_race` wait for children
/// that were already `Stopping` before the halt or race drain began.
#[test]
fn exit_brood_and_race_wait_for_already_stopping_children() {
    let slow_body = |_: &Mooring, child: &mut Shell| loop {
        if let Err(stop) = child.poll() {
            std::thread::sleep(std::time::Duration::from_millis(80));
            return Err(stop);
        }
        std::thread::yield_now();
    };

    // 1. `exit_brood` on a halting gather body waits for a pre-cancelled child.
    {
        let mut shell = crate::test_helper::core_shell();
        let owner = ScopeOwner::new(BroodOwner::Gather);
        shell.local.cradle = Cradle::owning(shell.scope.clone(), owner.brood());
        let child =
            spawn_child(&Mooring::adrift(), &mut shell, spawned("<slow>"), slow_body).unwrap();
        builtin_cancel(&[Value::Handle(Box::new(child.clone()))]).unwrap();
        assert_eq!(child.sample(), Sample::Stopping);

        let err = exit_brood(
            &owner,
            Err(sig("gather halted")),
            std::time::Duration::from_secs(5),
            &mut shell,
        )
        .expect_err("halt propagates");
        assert_eq!(status(err), 1);
        assert!(
            child.has_ended(),
            "exit_brood on halt must drain an already-stopping child"
        );
    }

    // 2. `builtin_race` waits for an already-stopping handle in its input list.
    {
        let mut shell = crate::test_helper::core_shell();
        let mut m = Mooring::adrift();
        m.cleanup_grace = std::time::Duration::from_secs(5);
        let slow = spawn_child(&m, &mut shell, spawned("<slow-race>"), slow_body).unwrap();
        builtin_cancel(&[Value::Handle(Box::new(slow.clone()))]).unwrap();
        assert_eq!(slow.sample(), Sample::Stopping);

        let winner =
            spawn_child(&m, &mut shell, spawned("<win>"), |_, _| Ok(Value::Int(5))).unwrap();
        let args = [Value::list(vec![
            Value::Handle(Box::new(slow.clone())),
            Value::Handle(Box::new(winner)),
        ])];
        assert_eq!(
            builtin_race(&args, &m, &mut shell).expect("winner wins"),
            Value::Int(5)
        );
        assert!(
            slow.has_ended(),
            "race must drain an already-stopping handle before returning"
        );
    }
}

/// A non-cooperative gated worker that outlives a short cleanup grace remains
/// visible in `workers` as `stopping` and continues to hold its admission seat
/// until its gate is released and it reaches `Consumed`.
#[test]
fn non_cooperative_worker_outliving_grace_stays_stopping_and_consumes_cap() {
    let mut shell = crate::test_helper::core_shell();
    let mut m = Mooring::adrift();
    m.worker_cap = Some(2);
    m.cleanup_grace = std::time::Duration::from_millis(20);

    let winner = spawn_child(&m, &mut shell, spawned("<winner>"), |_, _| {
        Ok(Value::Int(1))
    })
    .unwrap();
    let (mut gate, gate_rx) = GateGuard::new();
    let stubborn = spawn_child(&m, &mut shell, spawned("<stubborn>"), move |_, _| {
        let _ = gate_rx.recv();
        Ok(Value::Unit)
    })
    .unwrap();

    let args = [Value::list(vec![
        Value::Handle(Box::new(winner)),
        Value::Handle(Box::new(stubborn.clone())),
    ])];
    assert_eq!(
        builtin_race(&args, &m, &mut shell).expect("winner finishes"),
        Value::Int(1)
    );

    // `stubborn` ignored cancellation past the 20ms grace, so it is still
    // `Stopping`, listed in `workers`, and holding a seat in the registry.
    assert_eq!(stubborn.sample(), Sample::Stopping);
    assert_eq!(shell.local.workers.count(), 1);
    let Value::List(listed) = builtin_workers(&shell) else {
        panic!("expected list from builtin_workers");
    };
    assert_eq!(listed.len(), 1);
    let row = listed.iter().next().unwrap();
    assert_eq!(
        expect_map(&row).get("state").as_deref(),
        Some(&Value::variant("stopping", None))
    );

    m.worker_cap = Some(1);
    assert!(
        spawn_child(&m, &mut shell, spawned("<blocked-by-stopping>"), |_, _| Ok(
            Value::Unit
        ))
        .is_err(),
        "a stubborn stopping worker must still count against worker_cap"
    );

    gate.release();
    wait_ended(&stubborn);
    assert_eq!(stubborn.sample(), Sample::Consumed);
    assert_eq!(shell.local.workers.count(), 0);
    spawn_child(&m, &mut shell, spawned("<admitted-after-end>"), |_, _| {
        Ok(Value::Unit)
    })
    .expect("seat is freed once the stopping worker finishes");
}

/// A pending cancel on the foreground shell's scope takes precedence over an
/// already-`Ready` handle in `await`, `first-settled`, and `race`.
#[test]
fn pending_foreground_cancel_beats_already_ready_result() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();
    let h_await = spawn_child(&m, &mut shell, spawned("<a>"), |_, _| Ok(Value::Int(1))).unwrap();
    let h_first = spawn_child(&m, &mut shell, spawned("<f>"), |_, _| Ok(Value::Int(2))).unwrap();
    let h_race = spawn_child(&m, &mut shell, spawned("<r>"), |_, _| Ok(Value::Int(3))).unwrap();
    wait_settled(&h_await);
    wait_settled(&h_first);
    wait_settled(&h_race);

    let foreground = Mooring::adrift();
    shell.scope = Scope::clone(&foreground.home);
    foreground.home.cancel(CancelCause::Interrupted);

    let err_await = await_handle(&h_await, "await", &mut shell)
        .expect_err("cancelled foreground must beat ready await");
    assert!(matches!(
        err_await,
        Break::Escape(Escape::Stopped {
            cause: CancelCause::Interrupted,
            ..
        })
    ));
    assert_eq!(
        h_await.sample(),
        Sample::Ready(()),
        "interrupted await must not claim the ready handle"
    );

    let err_first = builtin_first_settled(
        &[Value::list(vec![Value::Handle(Box::new(h_first.clone()))])],
        &shell,
    )
    .expect_err("cancelled foreground must beat ready first-settled");
    assert!(matches!(
        err_first,
        Break::Escape(Escape::Stopped {
            cause: CancelCause::Interrupted,
            ..
        })
    ));
    assert_eq!(
        h_first.sample(),
        Sample::Ready(()),
        "interrupted first-settled must not claim the ready handle"
    );

    let err_race = builtin_race(
        &[Value::list(vec![Value::Handle(Box::new(h_race.clone()))])],
        &m,
        &mut shell,
    )
    .expect_err("cancelled foreground must beat ready race");
    assert!(matches!(
        err_race,
        Break::Escape(Escape::Stopped {
            cause: CancelCause::Interrupted,
            ..
        })
    ));
    assert_eq!(
        h_race.sample(),
        Sample::Consumed,
        "interrupted race still claims and drains its handles"
    );
}

/// `is-done` returns `false` on `running` and `stopping`, `true` on `ready` and
/// `consumed`, and never claims the handle or re-raises a `ready` worker's
/// panic or `exit`.
#[test]
fn is_done_reports_terminal_states_without_claiming_or_reraising() {
    let mut shell = crate::test_helper::core_shell();
    let m = Mooring::adrift();

    // `running` -> false, `ready` -> true, `consumed` -> true.
    let (mut gate, gate_rx) = GateGuard::new();
    let h = spawn_child(&m, &mut shell, spawned("<run>"), move |_, _| {
        let _ = gate_rx.recv();
        Ok(Value::Int(10))
    })
    .unwrap();
    let arg = [Value::Handle(Box::new(h.clone()))];
    assert_eq!(builtin_is_done(&arg).unwrap(), Value::Bool(false));

    gate.release();
    wait_settled(&h);
    assert_eq!(builtin_is_done(&arg).unwrap(), Value::Bool(true));
    assert_eq!(h.sample(), Sample::Ready(()), "is-done must not claim");

    assert_eq!(
        await_handle(&h, "await", &mut shell).unwrap(),
        Value::Int(10)
    );
    assert_eq!(h.sample(), Sample::Consumed);
    assert_eq!(builtin_is_done(&arg).unwrap(), Value::Bool(true));

    // `stopping` -> false, then `consumed` -> true once ended.
    let (mut stop_gate, stop_rx) = GateGuard::new();
    let stopping = spawn_child(&m, &mut shell, spawned("<stopping>"), move |_, _| {
        let _ = stop_rx.recv();
        Ok(Value::Unit)
    })
    .unwrap();
    let stopping_arg = [Value::Handle(Box::new(stopping.clone()))];
    builtin_cancel(&stopping_arg).unwrap();
    assert_eq!(stopping.sample(), Sample::Stopping);
    assert_eq!(builtin_is_done(&stopping_arg).unwrap(), Value::Bool(false));

    stop_gate.release();
    wait_ended(&stopping);
    assert_eq!(stopping.sample(), Sample::Consumed);
    assert_eq!(builtin_is_done(&stopping_arg).unwrap(), Value::Bool(true));

    // A panicked `ready` handle and an exited `ready` handle both report `true`
    // without re-raising or claiming.
    let panicked = handle_with_panicked_worker(b"out", b"err");
    assert_eq!(
        builtin_is_done(&[Value::Handle(Box::new(panicked.clone()))]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(panicked.sample(), Sample::Ready(()));

    let exited = exiting_worker(&mut shell);
    assert_eq!(
        builtin_is_done(&[Value::Handle(Box::new(exited.clone()))]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(exited.sample(), Sample::Ready(()));
}

/// If a handle inside prelude `par` is claimed before `_raise-if-failed`
/// inspects it, `par` fails with `par: this worker's result was already claimed`.
#[test]
fn par_surfaces_outside_claim_with_par_prefix() {
    fn claim_and_pick_zero(
        args: &[Value],
        _mooring: &Mooring,
        _shell: &mut Shell,
    ) -> Settled<Value> {
        let [Value::List(list)] = args else {
            return Err(Error::new("expected handle list").into());
        };
        let first = list.iter().next().expect("non-empty handle list");
        let Value::Handle(h) = first.as_ref() else {
            return Err(Error::new("expected handle").into());
        };
        h.claim_and_stop();
        Ok(Value::Int(0))
    }

    let mut shell = crate::Shell::root(
        crate::shell::boot::boot_runtime(
            crate::io::terminal::TerminalState::default(),
            crate::shell::boot::BakedPrelude::runtime(),
            &crate::HostSurface::default(),
        )
        .expect("a test host resolves its baseline")
        .into(),
        crate::test_helper::process_cwd(),
    );
    let override_entry = BuiltinEntry::new(
        "first-settled".into(),
        crate::typecheck::builtins::scheme::first_settled,
        "test override",
        BuiltinBody::Static(claim_and_pick_zero),
    );
    Arc::make_mut(&mut shell.sig).install_natives([(
        "first-settled".to_string(),
        Value::Native {
            entry: Arc::new(override_entry),
            applied: Vec::new().into(),
        },
    )]);

    let (ended, _) = run_captured(&mut shell, "par { |x| return $x } [1] 1", None);
    let Break::Error(err) = ended.expect_err("claimed child must fail par") else {
        panic!("expected Break::Error");
    };
    assert_eq!(err.message, "par: this worker's result was already claimed");
}

/// A `Stopping` worker is never stamped or reaped by retention, and its `Ready`
/// child is likewise exempt while the parent remains `Stopping`.
#[test]
fn stopping_worker_is_exempt_from_retention_and_spares_its_ready_child() {
    let mut shell = crate::test_helper::core_shell();
    shell.arm_worker_retention(1);
    let (inner_tx, inner_rx) = mpsc::channel();
    let (mut gate, gate_rx) = GateGuard::new();

    let outer = spawn_child(
        &Mooring::adrift(),
        &mut shell,
        spawned("<outer>"),
        move |mooring, child| {
            let inner = spawn_child(mooring, child, spawned("<inner>"), |_, _| Ok(Value::Int(2)))?;
            inner_tx.send(inner).unwrap();
            let _ = gate_rx.recv();
            Ok(Value::Unit)
        },
    )
    .unwrap();
    let inner = inner_rx.recv().unwrap();
    wait_settled(&inner);

    // Cancel `outer` while it is still gated so its state is `Stopping`.
    builtin_cancel(&[Value::Handle(Box::new(outer.clone()))]).unwrap();
    assert_eq!(outer.sample(), Sample::Stopping);
    assert_eq!(inner.sample(), Sample::Ready(()));

    for _ in 0..3 {
        tick(&shell, 5);
        shell.local.workers.sweep_retention();
    }
    let snapshot = shell.local.workers.snapshot();
    assert_eq!(
        snapshot.len(),
        2,
        "neither the stopping parent nor its ready child may be reaped"
    );
    assert!(snapshot.iter().all(|e| e.settled_epoch.is_none()));
    assert_eq!(shell.take_worker_reap_notices().len(), 0);

    gate.release();
    wait_ended(&outer);
    wait_ended(&inner);
    assert_eq!(shell.local.workers.count(), 0);
    assert_eq!(shell.take_worker_reap_notices().len(), 0);
}
