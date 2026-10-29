use super::*;
use crate::agent::gauge::{EVICT_THRESHOLD, Measure};
use crate::agent::testkit::*;
use crate::bus::{AgentOutcome, Item, ParkMode, Scoped};
use crate::provider::scripted::{Reply, Script};
use crate::record::{Display, Protocol, Publish, Record, Recorded, Settlement};
use genai::chat::ChatRole;
use ral_core::process::{CancelCause, DurableRoot, ScopeNode};
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::{Shell, Value};
use std::borrow::Cow;
use std::time::{Duration, Instant};

fn open_for_drain(session: &Agent) {
    session.seed("open".into());
    open_next(&session.inbox);
}

fn attend_until_reply(child: &mut Agent, expected: &str, emit: &Emitter) -> AgentOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};
    let attending = Arc::new(AtomicBool::new(true));
    let watcher = {
        let handle = child.handle();
        let attending = attending.clone();
        let expected = FOValue::String {
            value: expected.into(),
        };
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while attending.load(Ordering::Acquire) && Instant::now() < deadline {
                if handle.reply() == Some(expected.clone()) {
                    handle.end();
                    return true;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            handle.end();
            false
        })
    };
    let outcome = child.attend(emit);
    attending.store(false, Ordering::Release);
    assert!(
        watcher.join().unwrap(),
        "the follow-up did not deposit {expected:?}: {outcome:?}"
    );
    outcome
}

#[test]
fn public_deliberate_refuses_a_display_first_recording_failure_before_any_request() {
    let mut session = Agent::for_test("system").unwrap();
    let recorder = session.recorder();
    let before = session.rendered_messages();
    recorder.fail_after(7);
    recorder
        .emit(Display::Answer {
            text: "display was first".into(),
        })
        .unwrap_err();
    let bytes = session.log_dir().read("record.jsonl").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let provider = scripted("must not be called", Script::new());
    let result = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("refused".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(result, Err(Fault::Unavailable(_))), "{result:?}");
    assert!(!session.is_ready());
    assert!(!session.handle().is_live());
    assert_eq!(
        serde_json::to_value(session.rendered_messages()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(session.log_dir().read("record.jsonl").unwrap(), bytes);
}

#[test]
fn a_real_tool_display_failure_stops_the_batch_and_the_next_model_request() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let recorder = session.recorder();
    T2_ARRIVAL.set(Some(Box::new(move || recorder.fail_after(17))));
    let ran_second = Arc::new(AtomicBool::new(false));
    let ran = ran_second.clone();
    T2_CANCEL.set(Some(Box::new(move || ran.store(true, Ordering::Release))));
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![
                ral_call("first", "t2-arrive"),
                ral_call("second", "t2-cancel-now"),
            ]))
            .then(Reply::panicking()),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("run both".into(), Voice::Human)),
        None,
        &emit,
    );
    let _ = T2_CANCEL.take();
    assert!(matches!(result, Err(Fault::Unavailable(_))), "{result:?}");
    assert!(!ran_second.load(Ordering::Acquire));
    assert!(
        !session.log.borrow().context().is_ready(),
        "no tool results may be fabricated"
    );
    assert!(!session.handle().is_live());
    let records =
        crate::record::read_records(&session.log_dir().path().join("record.jsonl")).unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
            .count(),
        1
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Protocol(Protocol::ToolResults { .. })))
    );
}

#[test]
fn a_severed_engine_cuts_its_batch_short_and_answers_the_loss() {
    use ral_core::carrier::{Severed, Transport as _};
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    open_for_drain(&session);
    let crate::agent::engine::EngineKind::Identity(transport) = session.engine.kind() else {
        panic!("an identity root");
    };
    let cause = Severed::Closed("engine lost mid-batch".into());
    let lost = cause.clone();
    T2_CANCEL.set(Some(Box::new(move || {
        transport.sever(lost);
    })));
    let ran_second = Arc::new(AtomicBool::new(false));
    let ran = ran_second.clone();
    T2_ARRIVAL.set(Some(Box::new(move || ran.store(true, Ordering::Release))));
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![
                ral_call("first", "t2-cancel-now"),
                ral_call("second", "t2-arrive"),
            ]))
            .then(Reply::panicking()),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = session.deliberate(
        &session.inbox.exchange(),
        &provider,
        Some(("run both".into(), Voice::Human)),
        None,
        &emit,
    );
    let _ = T2_CANCEL.take();
    let _ = T2_ARRIVAL.take();
    assert!(
        matches!(result, Err(Fault::Severed(ref c)) if *c == cause),
        "{result:?}"
    );
    assert!(!ran_second.load(Ordering::Acquire));
    assert!(!session.handle().is_live());
    let records =
        crate::record::read_records(&session.log_dir().path().join("record.jsonl")).unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
            .count(),
        1
    );
    let second = records
        .iter()
        .find_map(|record| match record {
            Record::Protocol(Protocol::ToolResults { results }) => {
                results.iter().find(|result| result.id == "second")
            }
            _ => None,
        })
        .expect("the second tool result");
    assert_eq!(
        second.content,
        crate::agent::EngineLost::running(&cause, session.run_dir()).to_string()
    );
}

thread_local! {
    static T2_CANCEL: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
    static T2_QUEUE: std::cell::RefCell<Option<crate::agent::AgentHandle>> = const { std::cell::RefCell::new(None) };
    static T2_ARRIVAL: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}

#[allow(clippy::unnecessary_wraps, reason = "fixed builtin signature")]
fn builtin_t2_cancel_now(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    if let Some(cancel) = T2_CANCEL.take() {
        cancel();
    }
    Ok(Value::Unit)
}

fn scheme_t2_cancel_now(_u: &mut Unifier) -> Scheme {
    mk_scheme(&[], &[], thunk(pure(Ty::Unit)))
}

#[allow(clippy::unnecessary_wraps, reason = "fixed builtin signature")]
fn builtin_t2_queue_prompt(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    T2_QUEUE.with(|cell| {
        if let Some(handle) = cell.borrow().as_ref() {
            handle.steer("queued prompt".into()).unwrap();
        }
    });
    Ok(Value::Unit)
}

#[allow(clippy::unnecessary_wraps, reason = "fixed builtin signature")]
fn builtin_t2_queue_read(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    T2_QUEUE.with(|cell| {
        if let Some(handle) = cell.borrow().as_ref() {
            handle.post_read(crate::bus::Read::Resources);
        }
    });
    Ok(Value::Unit)
}

#[allow(clippy::unnecessary_wraps, reason = "fixed builtin signature")]
fn builtin_t2_arrive(_args: &[Value], _mooring: &Mooring, _shell: &mut Shell) -> Settled<Value> {
    if let Some(arrive) = T2_ARRIVAL.take() {
        arrive();
    }
    Ok(Value::Unit)
}

static T2_CANCEL_BUILTINS_ARR: [BuiltinEntry; 4] = [
    BuiltinEntry::new(
        Cow::Borrowed("t2-arrive"),
        scheme_t2_cancel_now,
        "test-only: deliver input during a tool batch.",
        BuiltinBody::Static(builtin_t2_arrive),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("t2-cancel-now"),
        scheme_t2_cancel_now,
        "test-only: cancel this exchange.",
        BuiltinBody::Static(builtin_t2_cancel_now),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("t2-queue-prompt"),
        scheme_t2_cancel_now,
        "test-only: queue a human prompt.",
        BuiltinBody::Static(builtin_t2_queue_prompt),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("t2-queue-read"),
        scheme_t2_cancel_now,
        "test-only: queue a resources read.",
        BuiltinBody::Static(builtin_t2_queue_read),
    ),
];
static T2_CANCEL_BUILTINS: &[BuiltinEntry] = &T2_CANCEL_BUILTINS_ARR;

#[test]
fn stop_after_mid_exchange_draw_records_all_removed_humans_without_another_request() {
    struct StopOnSteering {
        handle: crate::agent::AgentHandle,
        job: crate::bus::JobId,
        exchange: ScopeNode,
        sink: Box<dyn Publish>,
    }
    impl Publish for StopOnSteering {
        fn fact(&self, recorded: Recorded<Record>) {
            if matches!(recorded.value(), Record::Protocol(Protocol::Steering { text, voice: Voice::Human }) if text == "first")
            {
                assert_eq!(self.handle.stop(self.job), Some(vec![]));
                self.exchange.cancel(CancelCause::Cancelled);
                self.handle.steer("fresh".into()).unwrap();
            }
            self.sink.fact(recorded);
        }
        fn transient(&self, transient: Transient) {
            self.sink.transient(transient);
        }
    }
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    session.seed("go".into());
    let job = session.inbox.await_job().unwrap();
    open_next(&session.inbox);
    let exchange = DurableRoot::default().child();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let recorder = session.recorder();
    let handle = session.handle();
    let sink = StopOnSteering {
        handle: handle.clone(),
        job: job.id(),
        exchange: exchange.clone(),
        sink: emit.fleet_sink(),
    };
    T2_ARRIVAL.set(Some(Box::new(move || {
        handle.steer("first".into()).unwrap();
        handle.post_read(crate::bus::Read::Resources);
        handle.steer("second".into()).unwrap();
        recorder.attach(Box::new(sink));
    })));
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call("one", "t2-arrive")])),
    );
    let outcome = session
        .deliberate(
            &exchange,
            &provider,
            Some(("go".into(), Voice::Human)),
            None,
            &emit,
        )
        .unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert!(exchange.is_cancelled());
    assert!(session.is_ready());
    assert_eq!(session.handle().queued(), vec!["fresh"]);
    let records = crate::bus::drain_records(&rx);
    let humans: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Protocol(Protocol::Steering {
                text,
                voice: Voice::Human,
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(humans, vec!["first", "second"]);
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::Forensic(Forensic::TurnStarted { .. })))
            .count(),
        1
    );
}

#[test]
fn sub_agent_returns_through_reply() {
    let parent = Agent::for_test("system").unwrap();
    let mut child = parent.fork().unwrap();
    child.seed("write a report".into());
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply #'# Report\nline one\nline two'#",
        )])),
    );
    let outcome = drive_peer(&mut child, provider);
    assert!(matches!(outcome, AgentOutcome::Replied), "{outcome:?}");
    assert_eq!(
        child.handle().reply().and_then(|v| match v {
            FOValue::String { value } => Some(value),
            _ => None,
        }),
        Some("# Report\nline one\nline two".into())
    );
    assert!(child.is_ready());
}

#[test]
fn reply_cancels_live_descendants() {
    let parent = Agent::for_test("system").unwrap();
    let mut child = parent.fork().unwrap();
    child.seed("return early".into());
    let direct = test_agent(
        &child.fleet,
        TestAgentSpec::child("direct", &child.handle(), child.inbox.mailbox()),
    )
    .unwrap();
    let grandchild = test_agent(
        &child.fleet,
        TestAgentSpec::child("grandchild", &direct, direct.inbox.mailbox()),
    )
    .unwrap();
    let sibling = test_agent(
        &child.fleet,
        TestAgentSpec::child("sibling", &parent.handle(), parent.inbox.mailbox()),
    )
    .unwrap();
    let outcome = drive_peer(
        &mut child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'done'",
            )])),
        ),
    );
    assert!(matches!(outcome, AgentOutcome::Replied));
    assert_eq!(
        child.handle().reply(),
        Some(FOValue::String {
            value: "done".into()
        })
    );
    assert!(!direct.is_live());
    assert!(!grandchild.is_live());
    assert!(sibling.is_live());
    sibling.inbox.retire("still delivers".into());
    assert!(matches!(
        pop(&parent.inbox),
        Some(Next::Item(Item::Agent { .. }))
    ));
    drop(grandchild);
    drop(direct);
    assert!(
        child
            .fleet
            .live()
            .iter()
            .all(|h| child.handle().descendant(h).is_none())
    );
}

#[test]
fn steering_typed_during_a_tool_batch_is_committed_between_results_and_reply() {
    let mut session = Agent::for_test("system").unwrap();
    open_for_drain(&session);
    session
        .handle()
        .steer("actually, stop after this".into())
        .unwrap();
    session.handle().steer("and report".into()).unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let steer_x = 1")]))
            .then(Reply::text("ok")),
    );
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    let ms = session.rendered_messages();
    assert_eq!(
        ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::User,
            ChatRole::Assistant
        ]
    );
    assert_eq!(
        ms[3].content.first_text(),
        Some("actually, stop after this\nand report")
    );
    assert!(crate::bus::drain_records(&rx).into_iter().any(|r| matches!(r,
        Record::Protocol(Protocol::Steering { text, voice: Voice::Human }) if text.contains("and report")
    )));
    assert!(session.is_ready());
}

#[test]
fn pressure_rides_the_steering_channel_once_per_excursion() {
    use crate::agent::gauge::PRESSURE_THRESHOLD_FALLBACK;
    const EXCHANGES: usize = 6;
    let mut session = Agent::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        let bulk = "x".repeat(
            usize::midpoint(PRESSURE_THRESHOLD_FALLBACK, EVICT_THRESHOLD) / (EXCHANGES * 2),
        );
        for _ in 0..EXCHANGES {
            log.append_user(bulk.clone(), None, Voice::Human).unwrap();
            log.append_assistant(
                genai::chat::ChatMessage::assistant(bulk.clone()),
                vec![],
                None,
            )
            .unwrap();
        }
    }
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call(
                "c1",
                "let pressure_a = 1",
            )]))
            .then(Reply::tool_calls(vec![ral_call(
                "c2",
                "let pressure_b = 2",
            )]))
            .then(Reply::text("noted")),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    let reminders: Vec<String> = session
        .rendered_messages()
        .into_iter()
        .filter_map(|m| m.content.first_text().map(str::to_string))
        .filter(|text| text.contains("Context pressure"))
        .collect();
    assert_eq!(reminders.len(), 1, "{reminders:?}");
}

#[test]
fn provider_error_mid_deliberation_does_not_wedge_session() {
    let mut session = Agent::for_test("system").unwrap();
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let x12_a = 7")]))
                .then(Reply::error(ProviderError::Other(
                    "stream ended without End event".into(),
                )))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok")),
        ),
    );
    session.seed("first exchange".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.attend(&emit);
    session.seed("second exchange after error".into());
    let outcome = session.attend(&emit);
    assert!(session.is_ready());
    assert!(scope_has(&session, "x12_a"));
    match outcome {
        AgentOutcome::Failed(msg) => assert!(!msg.contains("tool results are pending"), "{msg}"),
        other => panic!("expected Failed from no-reply nudges, got {other:?}"),
    }
}

fn overflow_context(session: &Agent) {
    let mut log = session.log.borrow_mut();
    for i in 0..4 {
        log.append_user(
            if i == 0 {
                "OLDEST".into()
            } else {
                "x".repeat(1000)
            },
            None,
            Voice::Human,
        )
        .unwrap();
        log.append_assistant(
            genai::chat::ChatMessage::assistant("y".repeat(1000)),
            vec![],
            None,
        )
        .unwrap();
    }
}

fn overflow() -> Reply {
    Reply::error(ProviderError::Overflow {
        model: "test-model".into(),
        provider: None,
        sent: Some(250_015),
        limit: Some(250_000),
        listed: None,
        body: None,
    })
}

#[test]
fn an_overflow_narrows_the_window_evicts_and_retries_once() {
    let mut session = Agent::for_test("system").unwrap();
    overflow_context(&session);
    let provider = scripted(
        "test-model",
        Script::new().then(overflow()).then(Reply::text("done")),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    assert_eq!(provider.context_window(), Some(250_000));
    assert!(
        !session
            .rendered_messages()
            .iter()
            .any(|m| m.content.first_text() == Some("OLDEST"))
    );
}

#[test]
fn a_second_overflow_ends_the_exchange_with_the_listed_window_stamped() {
    let mut session = Agent::for_test("system").unwrap();
    overflow_context(&session);
    let provider = scripted(
        "test-model",
        Script::new().then(overflow()).then(overflow()),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    match session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    ) {
        Err(Fault::Provider(ProviderError::Overflow { limit, listed, .. })) => {
            assert_eq!((limit, listed), (Some(250_000), Some(250_000)));
        }
        other => panic!("expected overflow, got {other:?}"),
    }
}

#[test]
fn stale_token_measure_is_unknown_until_the_next_completion() {
    let mut session = Agent::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        log.append_user("old context".into(), None, Voice::Human)
            .unwrap();
        log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
            .unwrap();
    }
    let at = session.log.borrow().context().log_len();
    session.readings.measure = Some(Measure { tokens: 99_000, at });
    assert_eq!(session.measured_input(), Some(99_000));
    session
        .log
        .borrow_mut()
        .evict(crate::record::Span::single(2), None, EditAuthority::Model)
        .unwrap();
    assert_eq!(session.measured_input(), None);
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::text("fresh completion")),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("new context".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)));
    assert_eq!(session.measured_input(), Some(0));
}

#[test]
fn cancel_between_run_batch_and_drain_does_not_leak_reply_into_next_deliberation() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let exchange = DurableRoot::default().child();
    let cancel = exchange.clone();
    T2_CANCEL.set(Some(Box::new(move || {
        cancel.cancel(CancelCause::Interrupted);
    })));
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![
            ral_call("r1", "exarch-agents `reply 'stale'"),
            ral_call("c2", "t2-cancel-now"),
        ])),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &exchange,
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Cancelled)), "{outcome:?}");
    assert!(session.handle().reply().is_none());
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c3", "1")]))
            .then(Reply::text("done")),
    );
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("continue".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(
        matches!(outcome, Ok(Outcome::Complete)),
        "a cancelled batch must not leak its reply: {outcome:?}"
    );
}

#[test]
fn a_prompt_queued_across_an_interrupt_opens_the_next_exchange_over_the_whole_context() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let handle = session.handle();
    T2_CANCEL.set(Some(Box::new(move || handle.interrupt())));
    T2_QUEUE.with(|cell| *cell.borrow_mut() = Some(session.handle()));
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![
                    ral_call("c1", "t2-queue-prompt"),
                    ral_call("c2", "t2-cancel-now"),
                ]))
                .then(Reply::text("answered"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok")),
        ),
    );
    session.seed("go".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.attend(&emit);
    T2_QUEUE.with(|cell| *cell.borrow_mut() = None);
    assert_eq!(session.log.borrow().context().current_prompt(), Some(3));
    let rendered = session.rendered_messages();
    let position = |text: &str| {
        rendered
            .iter()
            .position(|m| m.content.first_text() == Some(text))
            .unwrap_or_else(|| panic!("{text:?} is absent"))
    };
    let tool_result = rendered
        .iter()
        .position(|m| m.role == ChatRole::Tool)
        .unwrap();
    assert!(position("go") < tool_result);
    assert!(tool_result < position("queued prompt"));
    assert!(position("queued prompt") < position("answered"));
}

#[test]
fn a_read_queued_mid_exchange_runs_at_the_tool_boundary_ahead_of_the_next_step() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    T2_QUEUE.with(|cell| *cell.borrow_mut() = Some(session.handle()));
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![
                    ral_call("c1", "t2-queue-read"),
                    ral_call("c2", "t2-queue-prompt"),
                ]))
                .then(Reply::text("answered"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok")),
        ),
    );
    session.seed("go".into());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.attend(&emit);
    T2_QUEUE.with(|cell| *cell.borrow_mut() = None);
    let signals: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let fold = signals
        .iter()
        .position(|sig| {
            matches!(
                sig,
                crate::bus::Signal::Transient(_, Transient::Resources { .. })
            )
        })
        .unwrap();
    let answer = signals.iter().position(|sig| matches!(sig,
        crate::bus::Signal::Fact(_, rec) if matches!(rec.value(), Record::Protocol(Protocol::AssistantMessage { message, .. }) if message.content.first_text() == Some("answered"))
    )).unwrap();
    assert!(fold < answer);
    let rendered = session.rendered_messages();
    let position = |text: &str| {
        rendered
            .iter()
            .position(|m| m.content.first_text() == Some(text))
            .unwrap()
    };
    let tool_result = rendered
        .iter()
        .position(|m| m.role == ChatRole::Tool)
        .unwrap();
    assert!(tool_result < position("queued prompt"));
    assert!(position("queued prompt") < position("answered"));
}

#[test]
fn a_read_queued_after_a_prompt_runs_after_it_lands() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    T2_QUEUE.with(|cell| *cell.borrow_mut() = Some(session.handle()));
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![
                    ral_call("c1", "t2-queue-prompt"),
                    ral_call("c2", "t2-queue-read"),
                ]))
                .then(Reply::text("answered"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok")),
        ),
    );
    session.seed("go".into());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.attend(&emit);
    T2_QUEUE.with(|cell| *cell.borrow_mut() = None);
    let signals: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let prompt = signals.iter().position(|sig| matches!(sig,
        crate::bus::Signal::Fact(_, rec) if matches!(rec.value(), Record::Protocol(Protocol::Steering { text, .. }) if text == "queued prompt")
    )).unwrap();
    let fold = signals
        .iter()
        .position(|sig| {
            matches!(
                sig,
                crate::bus::Signal::Transient(_, Transient::Resources { .. })
            )
        })
        .unwrap();
    assert!(prompt < fold);
}

#[test]
fn work_vouched_deliveries_never_reach_the_model_after_abandonment() {
    for surface in [false, true] {
        let mut session = Agent::for_test("system").unwrap();
        let mailbox = session.inbox.mailbox();
        session.abandon(Queue::Keep);
        mailbox.post(if surface {
            Scoped::Surface {
                id: session.id(),
                values: Vec::new(),
            }
        } else {
            Scoped::Agent {
                name: "late".into(),
                settled: Settlement::Failed {
                    reason: "done".into(),
                },
                elapsed: Duration::ZERO,
            }
        });
        set_provider(
            &session,
            scripted("test-model", Script::new().then(Reply::panicking())),
        );
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id());
        let outcome = session.attend(&emit);
        assert!(matches!(outcome, AgentOutcome::Failed(_)), "{outcome:?}");
        assert!(session.handle().reply().is_none());
        assert!(session.is_ready());
        assert!(
            session.rendered_messages().is_empty(),
            "dead provenance must not wake the model"
        );
    }
}

#[test]
fn live_work_vouched_deliveries_reach_the_model() {
    for surface in [false, true] {
        let mut session = Agent::for_test("system").unwrap();
        session.inbox.mailbox().post(if surface {
            Scoped::Surface {
                id: session.id(),
                values: Vec::new(),
            }
        } else {
            Scoped::Agent {
                name: "worker".into(),
                settled: Settlement::Failed {
                    reason: "found it".into(),
                },
                elapsed: Duration::ZERO,
            }
        });
        set_provider(
            &session,
            scripted(
                "test-model",
                Script::new().then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'done'",
                )])),
            ),
        );
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id());
        let outcome = session.attend(&emit);
        assert!(matches!(outcome, AgentOutcome::Replied), "{outcome:?}");
        assert_eq!(
            session.handle().reply(),
            Some(FOValue::String {
                value: "done".into()
            })
        );
    }
}

#[test]
fn ending_a_sub_agent_then_dropping_its_engine_cancels_its_workers() {
    let parent = dressed_trunk(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let child = started_child(&parent, "child", "`inherit");
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    child.ral(
        &DurableRoot::default().child(),
        "spawn \"blocker\" { test-clear-block-forever }",
        30,
        &emit,
    );
    let entries = workers(&child);
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].handle.cancel.is_cancelled());
    child.handle().end();
    assert!(!child.handle().is_live());
    drop(child);
    assert!(entries[0].handle.cancel.is_cancelled());
}

static DELIBERATE_STOP_ENTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static DELIBERATE_STOP_RELEASE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
struct DeliberateReleaseGuard(&'static std::sync::atomic::AtomicBool);
impl Drop for DeliberateReleaseGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn builtin_deliberate_hold_until_released(
    _args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    DELIBERATE_STOP_ENTERED.store(true, std::sync::atomic::Ordering::Release);
    while !DELIBERATE_STOP_RELEASE.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(2));
    }
    shell.poll()?;
    Ok(Value::Unit)
}

static DELIBERATE_HOLD_BUILTINS: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed("test-deliberate-hold"),
    scheme_t2_cancel_now,
    "test-only: hold until released, then poll.",
    BuiltinBody::Static(builtin_deliberate_hold_until_released),
)];

#[test]
fn answer_suppresses_awaited_and_stopping_done_events_while_replaying_cards() {
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    DELIBERATE_STOP_ENTERED.store(false, std::sync::atomic::Ordering::Release);
    DELIBERATE_STOP_RELEASE.store(false, std::sync::atomic::Ordering::Release);
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(&DELIBERATE_HOLD_BUILTINS));
    // Declared after the session, so the hold ends before its teardown.
    let _release = DeliberateReleaseGuard(&DELIBERATE_STOP_RELEASE);
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        "let h_stop = spawn \"stopping\" { test-deliberate-hold }",
        10,
        &emit,
    );
    while !DELIBERATE_STOP_ENTERED.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(2));
    }
    session.ral(
        &DurableRoot::default().child(),
        "cancel $h_stop\nlet h_claimed = spawn \"claimed\" { 7 }\nawait $h_claimed",
        10,
        &emit,
    );
    while session.inbox.draw(|_| ParkMode::Quiesce).is_some() {}
    open_for_drain(&session);
    let rows = session.engine.read(|t| t.workers()).unwrap();
    assert!(
        matches!(rows.as_slice(), [row] if matches!(row.state, WorkerState::Stopping)),
        "{rows:?}"
    );
    let stop_id = rows[0].id;
    let _ = crate::bus::drain_records(&rx);
    for (id, title) in [(stop_id + 1, "claimed"), (stop_id, "stopping")] {
        let done = DoneEvent {
            id: WorkerId(id),
            title: title.into(),
            elapsed_ms: 15,
            outcome: Done::Ok,
        }
        .to_surface();
        session.inbox.life_mailbox().post(Scoped::Surface {
            id: session.id(),
            values: vec![card_value(vec![]), done],
        });
    }
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "1")]))
            .then(Reply::text("done")),
    );
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    let ms = session.rendered_messages();
    assert_eq!(
        ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Assistant
        ],
        "{ms:?}"
    );
    let records = crate::bus::drain_records(&rx);
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::Display(Display::Card { .. })))
            .count(),
        2,
        "{records:?}"
    );
}

#[test]
fn answer_delivers_unclaimed_ready_and_non_done_surfaces_and_fences_abandoned_work() {
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let stale = session.inbox.mailbox();
    session.clear().unwrap();
    open_for_drain(&session);
    assert!(!stale.post(Scoped::Surface {
        id: session.id(),
        values: vec![card_value(vec![])],
    }));
    session.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"ready-worker\" { 99 }",
        10,
        &emit,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session
        .engine
        .read(|t| t.workers())
        .unwrap()
        .iter()
        .any(|r| matches!(r.state, WorkerState::Ready(_)))
        || session
            .inbox
            .source_depths()
            .iter()
            .map(|(_, count)| count)
            .sum::<u64>()
            == 0
    {
        assert!(Instant::now() < deadline, "worker did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
    session.inbox.life_mailbox().post(Scoped::Surface {
        id: session.id(),
        values: vec![card_value(vec![])],
    });
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "1")]))
            .then(Reply::text("done")),
    );
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)), "{outcome:?}");
    let ms = session.rendered_messages();
    assert_eq!(
        ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::User,
            ChatRole::User,
            ChatRole::Assistant
        ]
    );
    let first = ms[3].content.first_text().unwrap();
    assert!(
        first.contains("ready-worker")
            && first
                .contains("If you kept its handle and the result is still unclaimed, `await $h`"),
        "{first}"
    );
    let second = ms[4].content.first_text().unwrap();
    assert!(second.contains("errand finished."), "{second}");
}

#[test]
fn answer_propagates_severed_from_arrival_worker_probe() {
    use crate::card::testkit::card_value;
    use ral_core::carrier::Severed;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    struct SeverOnCard {
        transport: Arc<ral_core::carrier::IdentityTransport>,
        cause: Severed,
    }
    impl Publish for SeverOnCard {
        fn fact(&self, recorded: Recorded<Record>) {
            if matches!(recorded.value(), Record::Display(Display::Card { .. })) {
                use ral_core::carrier::Transport as _;
                self.transport.sever(self.cause.clone());
            }
        }
        fn transient(&self, _transient: Transient) {}
    }
    static ARM_SEVER: std::sync::Mutex<Option<(crate::record::Emitter, SeverOnCard)>> =
        std::sync::Mutex::new(None);
    #[allow(clippy::unnecessary_wraps, reason = "fixed builtin signature")]
    fn builtin_arm_sever(
        _args: &[Value],
        _mooring: &Mooring,
        _shell: &mut Shell,
    ) -> Settled<Value> {
        use ral_core::sync::LockExt as _;
        let armed = ARM_SEVER.lock_ignore_poison().take();
        if let Some((recorder, sink)) = armed {
            recorder.attach(Box::new(sink));
        }
        Ok(Value::Unit)
    }
    static ARM_SEVER_BUILTINS: [BuiltinEntry; 1] = [BuiltinEntry::new(
        Cow::Borrowed("test-arm-sever-on-card"),
        scheme_t2_cancel_now,
        "test-only: attach SeverOnCard after coupling.",
        BuiltinBody::Static(builtin_arm_sever),
    )];
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(&ARM_SEVER_BUILTINS));
    open_for_drain(&session);
    let crate::agent::engine::EngineKind::Identity(transport) = session.engine.kind() else {
        panic!("identity engine")
    };
    let cause = Severed::Closed("probe severed".into());
    {
        use ral_core::sync::LockExt as _;
        *ARM_SEVER.lock_ignore_poison() = Some((
            session.recorder(),
            SeverOnCard {
                transport,
                cause: cause.clone(),
            },
        ));
    }
    let done = DoneEvent {
        id: WorkerId(1),
        title: "boom".into(),
        elapsed_ms: 10,
        outcome: Done::Ok,
    }
    .to_surface();
    session.inbox.life_mailbox().post(Scoped::Surface {
        id: session.id(),
        values: vec![card_value(vec![]), done],
    });
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call(
                "c1",
                "test-arm-sever-on-card",
            )]))
            .then(Reply::text("unreachable")),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("go".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(
        matches!(outcome, Err(Fault::Severed(ref c)) if *c == cause),
        "{outcome:?}"
    );
}

#[test]
fn reply_boundary_preserves_live_deliveries_and_fences_abandoned_work() {
    #[derive(Clone, Copy, Debug)]
    enum Delivery {
        Message,
        Steering,
        Wakeup,
        Agent,
    }
    for source in [
        Delivery::Message,
        Delivery::Steering,
        Delivery::Wakeup,
        Delivery::Agent,
    ] {
        let parent = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
        let mut child = started_child(&parent, "helper", "`inherit");
        let grandchild = test_agent(
            &parent.fleet,
            TestAgentSpec::child("grandchild", &child.handle(), child.inbox.mailbox()),
        )
        .unwrap();
        let mailbox = child.inbox.mailbox();
        child
            .schedules
            .schedule(
                crate::schedule::Trigger::After(Duration::from_hours(1)),
                "scheduled follow-up".into(),
                "follow-up".into(),
                &mailbox,
            )
            .unwrap();
        assert!(child.schedules.armed());
        let report = mailbox.clone();
        let notice = Scoped::Agent {
            name: grandchild.0.name.clone().into(),
            settled: Settlement::Failed {
                reason: "boundary delivery".into(),
            },
            elapsed: Duration::ZERO,
        };
        let owner = parent.handle();
        let recipient = child.handle();
        T2_ARRIVAL.set(Some(Box::new(move || match source {
            Delivery::Message => recipient
                .message(&owner, "boundary delivery".into())
                .unwrap(),
            Delivery::Steering => recipient.steer("boundary delivery".into()).unwrap(),
            Delivery::Wakeup => {
                mailbox.post(Scoped::Wakeup {
                    id: crate::bus::ScheduleId::new(17),
                    label: "boundary".into(),
                    trigger: "after 1s".into(),
                    prompt: "boundary delivery".into(),
                });
            }
            Delivery::Agent => {
                report.post(notice);
            }
        })));
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![
                    ral_call("first", "exarch-agents `reply 'first'"),
                    ral_call("arrival", "t2-arrive"),
                ]))
                .then(Reply::tool_calls(vec![ral_call(
                    "second",
                    "exarch-agents `reply 'second'",
                )])),
        );
        set_provider(&child, provider.clone());
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.id());
        let outcome = child.deliberate(
            &DurableRoot::default().child(),
            &provider,
            Some(("original task".into(), Voice::Human)),
            None,
            &emit,
        );
        assert!(matches!(outcome, Ok(Outcome::Replied)), "{outcome:?}");
        assert!(child.is_ready());
        assert!(!child.schedules.armed(), "reply clears its armed schedules");
        let survives = matches!(source, Delivery::Message | Delivery::Steering);
        assert_eq!(child.inbox.is_empty(), !survives, "{source:?}");
        assert!(
            !child.handle().has_busy_children(),
            "reply abandons the whole awaiting set"
        );
        assert!(!grandchild.is_live());
        let first = crate::bus::drain_records(&rx);
        assert!(
            !first
                .iter()
                .any(|r| matches!(r, Record::Display(Display::SubagentDone { .. })))
        );
        assert!(
            !serde_json::to_string(&child.rendered_messages())
                .unwrap()
                .contains("boundary delivery")
        );
        if survives {
            let outcome = attend_until_reply(&mut child, "second", &emit);
            assert!(
                matches!(outcome, AgentOutcome::Replied),
                "{source:?}: {outcome:?}"
            );
            assert_eq!(
                child.handle().reply(),
                Some(FOValue::String {
                    value: "second".into()
                })
            );
        } else {
            assert!(
                child.inbox.draw(|_| ParkMode::Quiesce).is_none(),
                "{source:?} belonged to abandoned work"
            );
            assert_eq!(
                child.handle().reply(),
                Some(FOValue::String {
                    value: "first".into()
                })
            );
        }
        assert!(child.inbox.draw(|_| ParkMode::Quiesce).is_none());
        assert!(child.is_ready());
        let messages = child.rendered_messages();
        assert_eq!(
            messages
                .iter()
                .filter(|m| m
                    .content
                    .first_text()
                    .is_some_and(|t| t.contains("boundary delivery")))
                .count(),
            usize::from(survives)
        );
        let records = crate::bus::drain_records(&rx);
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, Record::Display(Display::SubagentDone { .. })))
                .count(),
            0
        );
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, Record::Forensic(Forensic::TurnStarted { .. })))
                .count(),
            usize::from(survives)
        );
        assert!(!child.handle().has_busy_children());
    }
}

#[test]
fn a_rewrite_queued_before_reply_boundary_input_runs_before_admission() {
    let parent = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let mut child = started_child(&parent, "helper", "`inherit");
    let handle = child.handle();
    T2_ARRIVAL.set(Some(Box::new(move || {
        handle.post_rewrite(crate::bus::Rewrite::Rewind(1));
        handle.steer("after rewind".into()).unwrap();
    })));
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![
                ral_call("first", "exarch-agents `reply 'first'"),
                ral_call("arrival", "t2-arrive"),
            ]))
            .then(Reply::tool_calls(vec![ral_call(
                "second",
                "exarch-agents `reply 'second'",
            )])),
    );
    set_provider(&child, provider.clone());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    let outcome = child.deliberate(
        &DurableRoot::default().child(),
        &provider,
        Some(("before rewind".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Replied)), "{outcome:?}");
    assert!(matches!(
        attend_until_reply(&mut child, "second", &emit),
        AgentOutcome::Replied
    ));
    let view = serde_json::to_string(&child.rendered_messages()).unwrap();
    assert!(!view.contains("before rewind"));
    assert!(view.contains("after rewind"));
    assert!(child.is_ready());
}

#[test]
fn a_headless_reply_leaves_arrivals_queued_without_an_extra_request() {
    let mut root = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let handle = root.handle();
    T2_ARRIVAL.set(Some(Box::new(move || {
        handle.steer("later task".into()).unwrap();
    })));
    set_provider(
        &root,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![
                    ral_call("first", "exarch-agents `reply 'first'"),
                    ral_call("arrival", "t2-arrive"),
                ]))
                .then(Reply::panicking()),
        ),
    );
    root.seed("one task".into());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, root.id());
    assert!(matches!(root.attend(&emit), AgentOutcome::Replied));
    assert!(root.is_ready());
    assert!(matches!(pop(&root.inbox), Some(Next::Item(Item::Human(t))) if t == "later task"));
    assert_eq!(
        crate::bus::drain_records(&rx)
            .iter()
            .filter(|r| matches!(r, Record::Forensic(Forensic::TurnStarted { .. })))
            .count(),
        1
    );
}
