use super::*;
use crate::agent::gauge::{EVICT_THRESHOLD, Measure};
use crate::agent::testkit::*;
use crate::bus::{AgentOutcome, Item, Post, Stamped};
use crate::cancel::InterruptTarget;
use crate::provider::scripted::{Reply, Script};
use crate::record::AgentId;
use genai::chat::ChatRole;
use ral_core::Shell;
use ral_core::Value;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry, Mooring, Settled};
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use std::borrow::Cow;

/// A sub-agent returns through `reply`: the payload is deposited raw, it
/// settles `Replied`, and the run ends `ReadyForUser`.
#[test]
fn sub_agent_returns_through_reply() {
    let parent = Avatar::for_test("system").unwrap();
    let mut child = parent.fork().expect("fork child");
    child.seed("write a report".into());
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply #'# Report\nline one\nline two'#",
        )])),
    );
    let outcome = drive_peer(&mut child, provider);
    assert!(
        matches!(outcome, AgentOutcome::Replied),
        "a reply settles Replied, got {outcome:?}"
    );
    assert_eq!(
        child.agent.reply().and_then(|v| match v {
            FOValue::String { value } => Some(value),
            _ => None,
        }),
        Some("# Report\nline one\nline two".into()),
        "the markdown payload is deposited raw, newlines intact"
    );
    assert!(
        child.is_ready(),
        "a replied deliberation must leave the session ReadyForUser"
    );
}

/// `reply` settles the whole owned subtree: children still running when the
/// parent returns are cancelled and abandoned, siblings left untouched.
#[test]
fn reply_cancels_live_descendants() {
    let parent = Avatar::for_test("system").unwrap();
    let mut child = parent.fork().expect("fork child");
    child.seed("return early".into());

    let mut direct = TestAgentSpec::new("direct");
    direct.role = crate::agent::Role::child(&child.agent);
    let transport = bare_transport();
    direct.reach = InterruptTarget::new(ral_core::carrier::Transport::control(&transport).clone());
    let direct = test_agent(&child.fleet, direct).expect("a live child of the replying agent");
    let mut grandchild = TestAgentSpec::new("grandchild");
    grandchild.role = crate::agent::Role::child(&direct);
    let grandchild = test_agent(&child.fleet, grandchild).expect("a live grandchild");
    let mut sibling = TestAgentSpec::new("sibling");
    sibling.role = crate::agent::Role::child(&parent.agent);
    let sibling = test_agent(&child.fleet, sibling).expect("a live sibling of the replier");

    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'done'",
        )])),
    );
    let outcome = drive_peer(&mut child, provider);

    assert!(matches!(outcome, AgentOutcome::Replied));
    assert_eq!(
        child.agent.reply(),
        Some(FOValue::String {
            value: "done".into()
        })
    );
    // The cascade runs inside the deliberation that replied, so it has
    // already landed by the time that reply is the settled outcome.
    assert!(
        direct.token.is_cancelled(),
        "the direct child is cancelled by the reply itself"
    );
    assert!(
        ral_core::carrier::Transport::session_ended(&transport)
            .expect("an identity transport answers")
            .is_some(),
        "the cascade cancels the abandoned child's eval layer too"
    );
    assert!(
        grandchild.token.is_cancelled(),
        "grandchild is cancelled recursively"
    );
    assert!(
        !sibling.token.is_cancelled(),
        "a sibling outside the replying subtree is untouched"
    );
    sibling.report(AgentOutcome::Stopped("still delivers".into()));
    assert!(
        matches!(parent.inbox.next_item(), Some(Next::Item(Item::Agent(_)))),
        "reply must not poison a sibling's delivery to their shared parent"
    );

    // The abandoned subtree leaves the tree as its holders drop, which in
    // production is each child's own worker retiring it.
    drop(grandchild);
    drop(direct);
    assert!(
        child.agent.walk().is_empty(),
        "the abandoned subtree is pruned once nothing holds it"
    );
}

/// A prompt typed while a tool batch runs is committed as a steering turn
/// between the results and the next assistant turn — the one mid-exchange
/// user ingress there is.  Several drained at once coalesce into one
/// message, blank-line joined.
#[test]
fn steering_typed_during_a_tool_batch_is_committed_between_results_and_reply() {
    let mut session = Avatar::for_test("system").unwrap();
    session
        .inbox
        .push(Post::UserSteering("actually, stop after this".into()));
    session.inbox.push(Post::UserSteering("and report".into()));

    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let steer_x = 1")]))
            .then(Reply::text("ok")),
    );
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    match session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit) {
        Ok(Outcome::Complete) => {}
        other => panic!("the steering prompt must be admitted mid-exchange, got {other:?}"),
    }

    let ms = session.rendered_messages();
    assert_eq!(
        ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::User,
            ChatRole::Assistant,
        ],
        "the steering turn sits between the tool results and the next reply"
    );
    assert_eq!(
        ms[3].content.first_text(),
        Some("actually, stop after this\nand report"),
        "both drained prompts coalesce into the one admitted message"
    );
    assert!(
        crate::bus::drain_records(&rx)
            .into_iter()
            .any(|record| matches!(
                record,
                crate::record::Record::Protocol(crate::record::Protocol::Steering {
                    text,
                    voice: Voice::Human,
                }) if text.contains("and report")
            )),
        "the arrival must be recorded as human steering as it enters context"
    );
    assert!(session.is_ready());
}

/// Context pressure reaches the model at a tool boundary, as steering
/// trailing the batch's arrivals: an agentic run may
/// take two hundred tool turns before the next exchange, by which time the
/// cut has long since happened.  Edge-triggered, so two boundaries under
/// one excursion carry one reminder.
#[test]
fn pressure_rides_the_steering_channel_once_per_excursion() {
    use crate::agent::gauge::PRESSURE_THRESHOLD_FALLBACK;
    const EXCHANGES: usize = 6;
    let mut session = Avatar::for_test("system").unwrap();
    // A scripted model has no known context window, so the gauge
    // reads bytes: sized between the soft line and the eviction trigger,
    // so pressure is due and nothing has been shed yet.
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
    let emit = Emitter::new(tx, session.agent.id);
    match session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit) {
        Ok(Outcome::Complete) => {}
        other => panic!("the run must reach its reply, got {other:?}"),
    }

    let reminders: Vec<String> = session
        .rendered_messages()
        .into_iter()
        .filter_map(|m| m.content.first_text().map(str::to_string))
        .filter(|text| text.contains("Context pressure"))
        .collect();
    assert_eq!(
        reminders.len(),
        1,
        "one excursion owes exactly one reminder, got {reminders:?}"
    );
}

/// A provider error mid-deliberation strands the session in
/// `AwaitingAssistantAfterToolResults`; the attend loop's per-iteration
/// quiesce must leave the next prompt admissible.
#[test]
fn provider_error_mid_deliberation_does_not_wedge_session() {
    let mut session = Avatar::for_test("system").unwrap();
    // A tool call that completes, then a stream error mid-protocol, then
    // clean replies for the second exchange and its nudges.
    let provider = scripted(
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
    );
    session.agent.provider.swap(provider);
    session.seed("first exchange".into());
    session.seed("second exchange after error".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.attend(&emit);
    assert!(
        session.is_ready(),
        "session must be ReadyForUser after a mid-deliberation provider error"
    );
    assert!(
        scope_has(&session, "x12_a"),
        "a binding from a completed tool call must survive a later provider error"
    );
    match outcome {
        AgentOutcome::Failed(msg) => {
            assert!(
                !msg.contains("tool results are pending"),
                "second exchange must not be rejected; got: {msg}"
            );
        }
        other => panic!("expected Failed from no-reply nudges, got {other:?}"),
    }
}

/// An overflow stating its limit narrows the provider's window, sheds the
/// older turns, and the retried turn completes the exchange.
#[test]
fn an_overflow_narrows_the_window_evicts_and_retries_once() {
    let mut session = Avatar::for_test("system").unwrap();
    for i in 0..4 {
        let mut log = session.log.borrow_mut();
        let prompt = if i == 0 {
            "OLDEST".into()
        } else {
            "x".repeat(1000)
        };
        log.append_user(prompt, None, Voice::Human).unwrap();
        log.append_assistant(
            genai::chat::ChatMessage::assistant("y".repeat(1000)),
            vec![],
            None,
        )
        .unwrap();
    }
    let overflow = || {
        Reply::error(ProviderError::Overflow {
            model: "test-model".into(),
            provider: None,
            sent: Some(250_015),
            limit: Some(250_000),
            listed: None,
            body: None,
        })
    };
    let provider = scripted(
        "test-model",
        Script::new().then(overflow()).then(Reply::text("done")),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit);
    assert!(matches!(outcome, Ok(Outcome::Complete)), "got {outcome:?}");
    assert_eq!(provider.context_window(), Some(250_000));
    assert!(
        !session
            .rendered_messages()
            .iter()
            .any(|m| m.content.first_text() == Some("OLDEST")),
        "the older turns should have been shed"
    );
}

/// A second overflow in a row is the exchange's end, the error carrying the
/// window exarch had been working to.
#[test]
fn a_second_overflow_ends_the_exchange_with_the_listed_window_stamped() {
    let mut session = Avatar::for_test("system").unwrap();
    for i in 0..4 {
        let mut log = session.log.borrow_mut();
        let prompt = if i == 0 {
            "OLDEST".into()
        } else {
            "x".repeat(1000)
        };
        log.append_user(prompt, None, Voice::Human).unwrap();
        log.append_assistant(
            genai::chat::ChatMessage::assistant("y".repeat(1000)),
            vec![],
            None,
        )
        .unwrap();
    }
    let overflow = || {
        Reply::error(ProviderError::Overflow {
            model: "test-model".into(),
            provider: None,
            sent: Some(250_015),
            limit: Some(250_000),
            listed: None,
            body: None,
        })
    };
    let provider = scripted(
        "test-model",
        Script::new().then(overflow()).then(overflow()),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    match session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit) {
        Err(Fault::Provider(ProviderError::Overflow { limit, listed, .. })) => {
            assert_eq!((limit, listed), (Some(250_000), Some(250_000)));
        }
        other => panic!("expected the overflow to surface, got {other:?}"),
    }
}

#[test]
fn stale_token_measure_is_unknown_until_the_next_completion() {
    let mut session = Avatar::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        log.append_user("old context".into(), None, Voice::Human)
            .unwrap();
        log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
            .unwrap();
    }
    let measured_at = session.log.borrow().context().log_len();
    session.readings.measure = Some(Measure {
        tokens: 99_000,
        at: measured_at,
    });
    assert_eq!(session.measured_input(), Some(99_000));
    session
        .log
        .borrow_mut()
        .evict(crate::record::Span::single(2), None, EditAuthority::Model)
        .unwrap();

    assert_eq!(
        session.measured_input(),
        None,
        "a measure taken before an edit says nothing about what is left"
    );

    let provider = scripted(
        "test-model",
        Script::new().then(Reply::text("fresh completion")),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.deliberate(
        &provider,
        Some(("new context".into(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)));
    assert_eq!(session.measured_input(), Some(0));
}

thread_local! {
    /// The token [`builtin_t2_cancel_now`] cancels — nothing else lets a
    /// bare builtin reach the token `deliberate` is watching.
    static T2_CANCEL_TOKEN: std::cell::RefCell<Option<crate::cancel::Token>> =
        const { std::cell::RefCell::new(None) };
}

/// test-only: cancels whatever token is staged in [`T2_CANCEL_TOKEN`].
#[allow(
    clippy::unnecessary_wraps,
    reason = "fixed BuiltinBody::Static signature"
)]
fn builtin_t2_cancel_now(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    T2_CANCEL_TOKEN.with(|cell| {
        if let Some(token) = cell.borrow().as_ref() {
            token.cancel(ral_core::process::CancelCause::Interrupted);
        }
    });
    Ok(Value::Unit)
}

fn scheme_t2_cancel_now(_u: &mut Unifier) -> Scheme {
    mk_scheme(&[], &[], thunk(pure(Ty::Unit)))
}

thread_local! {
    /// The mailbox [`builtin_t2_queue_prompt`] posts to — a human typing
    /// while a batch runs, from inside that batch.
    static T2_QUEUE: std::cell::RefCell<Option<crate::bus::Mailbox>> =
        const { std::cell::RefCell::new(None) };
    static T2_ARRIVAL: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

/// test-only: queues "queued prompt" on the mailbox staged in [`T2_QUEUE`].
#[allow(
    clippy::unnecessary_wraps,
    reason = "fixed BuiltinBody::Static signature"
)]
fn builtin_t2_queue_prompt(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    T2_QUEUE.with(|cell| {
        if let Some(mailbox) = cell.borrow().as_ref() {
            mailbox.push(Post::UserSteering("queued prompt".into()));
        }
    });
    Ok(Value::Unit)
}

/// test-only: queues a `/resources` read on the mailbox staged in [`T2_QUEUE`].
#[allow(
    clippy::unnecessary_wraps,
    reason = "fixed BuiltinBody::Static signature"
)]
fn builtin_t2_queue_read(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    T2_QUEUE.with(|cell| {
        if let Some(mailbox) = cell.borrow().as_ref() {
            mailbox.push(Post::Read(crate::bus::Read::Resources));
        }
    });
    Ok(Value::Unit)
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "fixed BuiltinBody::Static signature"
)]
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
        "test-only: cancel the token staged in T2_CANCEL_TOKEN.",
        BuiltinBody::Static(builtin_t2_cancel_now),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("t2-queue-prompt"),
        scheme_t2_cancel_now,
        "test-only: queue a human prompt on the mailbox staged in T2_QUEUE.",
        BuiltinBody::Static(builtin_t2_queue_prompt),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("t2-queue-read"),
        scheme_t2_cancel_now,
        "test-only: queue a /resources read on the mailbox staged in T2_QUEUE.",
        BuiltinBody::Static(builtin_t2_queue_read),
    ),
];
static T2_CANCEL_BUILTINS: &[BuiltinEntry] = &T2_CANCEL_BUILTINS_ARR;

/// A `reply` staged mid-batch and then overtaken by a cancellation must not
/// survive into the next deliberation: the batch replies, then cancels the
/// token `deliberate` watches, landing between `run_batch` and the drain.
#[test]
fn cancel_between_run_batch_and_drain_does_not_leak_reply_into_next_deliberation() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = Some(session.agent.token.clone()));

    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![
            ral_call("r1", "exarch-agents `reply 'stale'"),
            ral_call("c2", "t2-cancel-now"),
        ])),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    match session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit) {
        Ok(Outcome::Cancelled) => {}
        other => panic!("expected the cancel to win before the reply drains, got {other:?}"),
    }
    T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = None);
    assert!(
        session.agent.reply().is_none(),
        "a reply overtaken by a cancel is never deposited"
    );

    // The exchange boundary the attend loop would cross.
    session.agent.token.reset();
    // The next deliberation's first batch must itself reach the reply-drain
    // check: a leaked payload hard-terminates exactly there, on `c3`.
    let provider2 = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c3", "1")]))
            .then(Reply::text("done")),
    );
    match session.deliberate(
        &provider2,
        Some(("continue".into(), Voice::Human)),
        None,
        &emit,
    ) {
        Ok(Outcome::Complete) => {}
        other => panic!(
            "a reply staged in a cancelled batch must not leak into the next deliberation, got {other:?}"
        ),
    }
}

/// A prompt queued behind a batch that Esc then cancels is not steering on
/// the exchange being dropped: it waits, opens the next exchange, and that
/// exchange's request still carries the interrupted work in full.
#[test]
fn a_prompt_queued_across_an_interrupt_opens_the_next_exchange_over_the_whole_context() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = Some(session.agent.token.clone()));
    T2_QUEUE.with(|cell| *cell.borrow_mut() = Some(session.agent.mailbox.clone()));
    session.agent.provider.swap(scripted(
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
    ));
    session.seed("go".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.attend(&emit);
    T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = None);
    T2_QUEUE.with(|cell| *cell.borrow_mut() = None);

    assert_eq!(
        session.log.borrow().context().current_prompt(),
        Some(3),
        "the queued prompt lands as turn 3 after turns 1–2, rather than steering turn 1"
    );
    let rendered = session.rendered_messages();
    let position = |text: &str| {
        rendered
            .iter()
            .position(|m| m.content.first_text() == Some(text))
            .unwrap_or_else(|| panic!("{text:?} is not in the context"))
    };
    let tool_result = rendered
        .iter()
        .position(|m| m.role == ChatRole::Tool)
        .expect("the cancelled call's result is in the context");
    assert!(position("go") < tool_result);
    assert!(tool_result < position("queued prompt"));
    assert!(position("queued prompt") < position("answered"));
}

/// A read typed mid-exchange runs at the tool boundary, ahead of the next
/// assistant step, and the prompt typed after it lands as steering at that
/// same boundary.
#[test]
fn a_read_queued_mid_exchange_runs_at_the_tool_boundary_ahead_of_the_next_step() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    T2_QUEUE.with(|cell| *cell.borrow_mut() = Some(session.agent.mailbox.clone()));
    session.agent.provider.swap(scripted(
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
    ));
    session.seed("go".into());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.agent.mailbox.clone());
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
        .expect("the read ran and published its fold");
    let answer = signals
        .iter()
        .position(|sig| {
            matches!(
                sig,
                crate::bus::Signal::Fact(_, rec) if matches!(
                    rec.value(),
                    crate::record::Record::Protocol(
                        crate::record::Protocol::AssistantMessage { message, .. }
                    ) if message.content.first_text() == Some("answered")
                )
            )
        })
        .expect("the exchange's answer is recorded");
    assert!(fold < answer, "the fold precedes the next assistant step");

    let rendered = session.rendered_messages();
    let position = |text: &str| {
        rendered
            .iter()
            .position(|m| m.content.first_text() == Some(text))
            .unwrap_or_else(|| panic!("{text:?} is not in the context"))
    };
    let tool_result = rendered
        .iter()
        .position(|m| m.role == ChatRole::Tool)
        .expect("the batch's results are in the context");
    assert!(tool_result < position("queued prompt"));
    assert!(position("queued prompt") < position("answered"));
}

/// The arrivals keep the order typed: a read queued *after* a prompt at
/// the same boundary runs after that prompt has landed, so a `/branch`
/// typed beside a prompt carries it.
#[test]
fn a_read_queued_after_a_prompt_runs_after_it_lands() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    T2_QUEUE.with(|cell| *cell.borrow_mut() = Some(session.agent.mailbox.clone()));
    session.agent.provider.swap(scripted(
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
    ));
    session.seed("go".into());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.agent.mailbox.clone());
    session.attend(&emit);
    T2_QUEUE.with(|cell| *cell.borrow_mut() = None);

    let signals: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let prompt = signals
        .iter()
        .position(|sig| {
            matches!(
                sig,
                crate::bus::Signal::Fact(_, rec) if matches!(
                    rec.value(),
                    crate::record::Record::Protocol(
                        crate::record::Protocol::Steering { text, .. }
                    ) if text == "queued prompt"
                )
            )
        })
        .expect("the prompt's arrival is recorded");
    let fold = signals
        .iter()
        .position(|sig| {
            matches!(
                sig,
                crate::bus::Signal::Transient(_, Transient::Resources { .. })
            )
        })
        .expect("the read ran and published its fold");
    assert!(
        prompt < fold,
        "the read runs after the prompt typed before it"
    );
}

/// A worker delivers before it retires, so a result that settled across a
/// `/clear` still reaches the inbox — and its stale epoch must keep it
/// out of the attend loop entirely.
#[test]
fn stale_epoch_agent_result_never_reaches_the_model() {
    let mut session = Avatar::for_test("system").unwrap();
    let stamp = session.agent.mailbox.stamp();
    session.inbox.clear();
    stamp.post(Stamped::AgentResult(crate::bus::AgentResult {
        id: AgentId::new(7),
        name: "late".into(),
        outcome: AgentOutcome::Stopped("done".into()),
        elapsed: std::time::Duration::ZERO,
    }));
    session
        .agent
        .provider
        .swap(scripted("test-model", Script::new()));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.attend(&emit);
    assert!(
        matches!(outcome, AgentOutcome::Failed(_)),
        "a stale result must be dropped, not attended to; got {outcome:?}"
    );
    assert!(session.agent.reply().is_none());
    assert!(session.is_ready());
}

/// The positive half: a live-epoch result is delivered and drives the
/// provider.
#[test]
fn current_epoch_agent_result_reaches_the_model() {
    let mut session = Avatar::for_test("system").unwrap();
    session
        .agent
        .mailbox
        .stamp()
        .post(Stamped::AgentResult(crate::bus::AgentResult {
            id: AgentId::new(7),
            name: "worker".into(),
            outcome: AgentOutcome::Stopped("found it".into()),
            elapsed: std::time::Duration::ZERO,
        }));
    session.agent.provider.swap(scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'done'",
        )])),
    ));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.attend(&emit);
    assert!(
        matches!(outcome, AgentOutcome::Replied),
        "a live-epoch result must be delivered; got {outcome:?}"
    );
    assert_eq!(
        session.agent.reply(),
        Some(FOValue::String {
            value: "done".into()
        })
    );
}

/// The same fence over a deferred `spawn` batch: composed off the
/// attend thread, so the producer cannot judge its own staleness.
#[test]
fn stale_epoch_surface_batch_never_reaches_the_model() {
    let mut session = Avatar::for_test("system").unwrap();
    let stamp = session.agent.mailbox.stamp();
    session.inbox.clear();
    stamp.post(Stamped::Surface {
        id: session.agent.id,
        values: Vec::new(),
    });
    session
        .agent
        .provider
        .swap(scripted("test-model", Script::new()));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.attend(&emit);
    assert!(
        matches!(outcome, AgentOutcome::Failed(_)),
        "a stale surface batch must be dropped, not attended to; got {outcome:?}"
    );
    assert!(session.agent.reply().is_none());
    assert!(session.is_ready());
}

/// The positive half: a live-epoch surface batch is delivered and
/// drives the provider.
#[test]
fn current_epoch_surface_batch_reaches_the_model() {
    let mut session = Avatar::for_test("system").unwrap();
    session.agent.mailbox.stamp().post(Stamped::Surface {
        id: session.agent.id,
        values: Vec::new(),
    });
    session.agent.provider.swap(scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'done'",
        )])),
    ));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.attend(&emit);
    assert!(
        matches!(outcome, AgentOutcome::Replied),
        "a live-epoch surface batch must be delivered; got {outcome:?}"
    );
    assert_eq!(
        session.agent.reply(),
        Some(FOValue::String {
            value: "done".into()
        })
    );
}

/// The cancel cascade never touches a shell's worker registry — it
/// only cancels the agent's `eval_root`.  That suffices: a worker's cancel
/// scope is a child of that root and `is_cancelled` walks ancestors.
/// Pinned because it must keep holding with no wiring of its own.
#[test]
fn cancel_cascade_reaches_a_cancelled_sub_agents_workers() {
    let parent = dressed_trunk(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let child = started_child(&parent, "child", "`inherit");

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, child.agent.id, child.inbox.mailbox());
    let _ = child.ral("spawn \"blocker\" { test-clear-block-forever }", 30, &emit);

    let entries = workers(&child);
    assert_eq!(entries.len(), 1, "the child's own spawn must register");
    assert!(!entries[0].handle.cancel.is_cancelled(), "freshly spawned");

    child
        .agent
        .cancel_tree(ral_core::process::CancelCause::Cancelled);

    assert!(
        entries[0].handle.cancel.is_cancelled(),
        "the subtree cascade must reach a cancelled sub-agent's own \
         workers through its shell's durable root"
    );
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
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    shell.poll()?;
    Ok(Value::Unit)
}

static DELIBERATE_HOLD_BUILTINS: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed("test-deliberate-hold"),
    scheme_t2_cancel_now,
    "test-only: hold until DELIBERATE_STOP_RELEASE, then poll.",
    BuiltinBody::Static(builtin_deliberate_hold_until_released),
)];

/// Mid-exchange `answer` suppresses steering for a `DoneEvent` whose worker
/// was already claimed by `await` or `cancel` (`Stopping`), while still
/// replaying its cards to the scrollback.
#[test]
fn answer_suppresses_awaited_and_stopping_done_events_while_replaying_cards() {
    use crate::card::testkit::card_value;
    use crate::record::{Display, Record};
    use ral_core::protocol::probe::WorkerState;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};

    DELIBERATE_STOP_ENTERED.store(false, std::sync::atomic::Ordering::Release);
    DELIBERATE_STOP_RELEASE.store(false, std::sync::atomic::Ordering::Release);
    let _release = DeliberateReleaseGuard(&DELIBERATE_STOP_RELEASE);
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(&DELIBERATE_HOLD_BUILTINS));
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    // Pre-create one Stopping worker (#1) and one Consumed worker (#2).
    session.ral(
        "let h_stop = spawn \"stopping\" { test-deliberate-hold }",
        10,
        &emit,
    );
    while !DELIBERATE_STOP_ENTERED.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    session.ral(
        "cancel $h_stop\n\
         let h_claimed = spawn \"claimed\" { 7 }\n\
         await $h_claimed",
        10,
        &emit,
    );
    while session.inbox.next_item().is_some() {}
    let rows = session.seat.read(|t| t.workers()).unwrap();
    assert!(
        matches!(rows.as_slice(), [row] if matches!(row.state, WorkerState::Stopping)),
        "expected only the stopping worker to remain in the table, got {rows:?}"
    );
    let stop_id = rows[0].id;
    let _ = crate::bus::drain_records(&rx);

    // Queue a DoneEvent for the Consumed worker and one for the Stopping worker,
    // each paired with a card that must still replay.
    for (id, title) in [(stop_id + 1, "claimed"), (stop_id, "stopping")] {
        let done = DoneEvent {
            id: WorkerId(id),
            title: title.into(),
            elapsed_ms: 15,
            outcome: Done::Ok,
        }
        .to_surface();
        session.inbox.mailbox().stamp().post(Stamped::Surface {
            id: session.agent.id,
            values: vec![card_value(vec![]), done],
        });
    }

    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "1")]))
            .then(Reply::text("done")),
    );
    let outcome = session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit);
    assert!(matches!(outcome, Ok(Outcome::Complete)), "got {outcome:?}");

    let ms = session.rendered_messages();
    assert_eq!(
        ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Assistant
        ],
        "neither suppressed completion may insert a steering turn: {ms:?}"
    );
    let records = crate::bus::drain_records(&rx);
    let cards = records
        .iter()
        .filter(|r| matches!(r, Record::Display(Display::Card { .. })))
        .count();
    assert_eq!(
        cards, 2,
        "both batches must still replay their cards: {records:?}"
    );
}

/// Mid-exchange `answer` delivers an unclaimed `Ready` worker's completion
/// notice and a `Surface` batch without a `DoneEvent` as steering, while
/// dropping a stale-epoch surface batch fenced by `/clear`.
#[test]
fn answer_delivers_unclaimed_ready_and_non_done_surfaces_and_fences_stale_epoch() {
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;

    let mut session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    let stale_stamp = session.inbox.mailbox().stamp();
    session.clear().unwrap();
    // Stale-epoch surface queued before the batch must be dropped at mid-exchange pop.
    stale_stamp.post(Stamped::Surface {
        id: session.agent.id,
        values: vec![card_value(vec![])],
    });

    // Spawn an unclaimed worker and wait for its genuine DoneEvent surface in the inbox.
    session.ral("let h = spawn \"ready-worker\" { 99 }", 10, &emit);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !session
        .seat
        .read(|t| t.workers())
        .unwrap()
        .iter()
        .any(|r| matches!(r.state, WorkerState::Ready(_)))
        || session.inbox.is_empty()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not settle"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Also queue a surface batch without a DoneEvent.
    session.inbox.mailbox().stamp().post(Stamped::Surface {
        id: session.agent.id,
        values: vec![card_value(vec![])],
    });

    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "1")]))
            .then(Reply::text("done")),
    );
    let outcome = session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit);
    assert!(matches!(outcome, Ok(Outcome::Complete)), "got {outcome:?}");

    let ms = session.rendered_messages();
    assert_eq!(
        ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::User,
            ChatRole::User,
            ChatRole::Assistant,
        ],
        "the unclaimed Ready completion and non-done batch enter as steering"
    );
    let first_steering = ms[3].content.first_text().expect("first steering text");
    assert!(
        first_steering.contains("ready-worker")
            && first_steering
                .contains("If you kept its handle and the result is still unclaimed, `await $h`"),
        "expected unclaimed completion notice in first steering, got: {first_steering}"
    );
    let second_steering = ms[4].content.first_text().expect("second steering text");
    assert!(
        second_steering.contains("errand finished."),
        "expected non-done surface notice in second steering, got: {second_steering}"
    );
}

/// If the `workers()` probe inside `answer`'s mid-exchange arrival drain sees
/// a severed seat, `answer` propagates `Fault::Severed`.
#[test]
fn answer_propagates_severed_from_arrival_worker_probe() {
    use crate::card::testkit::card_value;
    use crate::record::{Display, Publish, Record, Recorded, Transient};
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

    #[allow(clippy::unnecessary_wraps)]
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
        "test-only: attach SeverOnCard after ral() couples the session.",
        BuiltinBody::Static(builtin_arm_sever),
    )];

    let mut session = dressed_trunk(|runtime| runtime.install_builtins(&ARM_SEVER_BUILTINS));
    let crate::agent::seat::SeatKind::Identity(transport) = session.seat.kind() else {
        panic!("identity seat");
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
    session.inbox.mailbox().stamp().post(Stamped::Surface {
        id: session.agent.id,
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
    let emit = Emitter::new(tx, session.agent.id);
    let outcome = session.deliberate(&provider, Some(("go".into(), Voice::Human)), None, &emit);
    assert!(
        matches!(outcome, Err(Fault::Severed(ref c)) if *c == cause),
        "expected Fault::Severed from arrival worker probe, got {outcome:?}"
    );
}

#[test]
fn reply_boundary_preserves_each_delivery_until_the_next_ingress() {
    #[derive(Clone, Copy)]
    enum Delivery {
        Message,
        Steering,
        Wakeup,
        AgentResult,
    }

    for source in [
        Delivery::Message,
        Delivery::Steering,
        Delivery::Wakeup,
        Delivery::AgentResult,
    ] {
        let parent = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
        let mut child = started_child(&parent, "helper", "`inherit");
        let mut spec = TestAgentSpec::new("grandchild");
        spec.role = crate::agent::Role::child(&child.agent);
        let grandchild = test_agent(&parent.fleet, spec).unwrap();
        let mailbox = child.agent.mailbox.clone();
        let result = crate::bus::AgentResult {
            id: grandchild.id,
            name: grandchild.name.clone(),
            outcome: AgentOutcome::Stopped("boundary delivery".into()),
            elapsed: std::time::Duration::ZERO,
        };
        let owner = parent.agent.clone();
        let recipient = child.agent.clone();
        T2_ARRIVAL.set(Some(Box::new(move || match source {
            Delivery::Message => owner.message(&recipient, "boundary delivery".into()),
            Delivery::Steering => mailbox.steer("boundary delivery".into()),
            Delivery::Wakeup => mailbox.stamp().post(Stamped::Wakeup {
                id: crate::bus::ScheduleId::new(17),
                label: "boundary".into(),
                trigger: "after 1s".into(),
                prompt: "boundary delivery".into(),
            }),
            Delivery::AgentResult => mailbox.stamp().post(Stamped::AgentResult(result)),
        })));
        child.agent.provider.swap(scripted(
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
        ));
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.agent.id);
        child.couple(&emit);
        let provider = child.agent.provider.current();
        assert!(matches!(
            child.deliberate(
                &provider,
                Some(("original task".into(), Voice::Human)),
                None,
                &emit
            ),
            Ok(Outcome::Replied)
        ));
        assert!(child.is_ready());
        assert!(
            !child.inbox.is_empty(),
            "the input must retain its delivery obligation"
        );
        assert!(
            child.agent.has_busy_children(),
            "reply must not acknowledge a queued result"
        );
        let first = crate::bus::drain_records(&rx);
        assert!(!first.iter().any(|r| matches!(
            r,
            crate::record::Record::Display(crate::record::Display::SubagentDone { .. })
        )));
        assert!(
            !serde_json::to_string(&child.rendered_messages())
                .unwrap()
                .contains("boundary delivery")
        );

        assert!(matches!(child.attend_backlog(&emit), AgentOutcome::Replied));
        assert_eq!(
            child.agent.reply(),
            Some(FOValue::String {
                value: "second".into()
            })
        );
        assert!(child.inbox.next_item().is_none());
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
            1
        );
        let records = crate::bus::drain_records(&rx);
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(
                    r,
                    crate::record::Record::Display(crate::record::Display::SubagentDone { .. })
                ))
                .count(),
            usize::from(matches!(source, Delivery::AgentResult))
        );
        assert_eq!(
            child.agent.has_busy_children(),
            !matches!(source, Delivery::AgentResult)
        );
        assert!(
            grandchild.token.terminated(),
            "reply retains subtree cancellation"
        );
    }
}

#[test]
fn a_rewrite_queued_before_reply_boundary_input_runs_before_admission() {
    let parent = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let mut child = started_child(&parent, "helper", "`inherit");
    let mailbox = child.agent.mailbox.clone();
    T2_ARRIVAL.set(Some(Box::new(move || {
        mailbox.push(Post::Rewrite(crate::bus::Rewrite::Rewind(1)));
        mailbox.steer("after rewind".into());
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
    child.agent.provider.swap(provider.clone());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.agent.id);
    assert!(matches!(
        child.deliberate(
            &provider,
            Some(("before rewind".into(), Voice::Human)),
            None,
            &emit
        ),
        Ok(Outcome::Replied)
    ));
    assert!(matches!(child.attend_backlog(&emit), AgentOutcome::Replied));
    let view = serde_json::to_string(&child.rendered_messages()).unwrap();
    assert!(!view.contains("before rewind"));
    assert!(view.contains("after rewind"));
    assert!(child.is_ready());
}

#[test]
fn a_headless_reply_leaves_arrivals_queued_without_an_extra_request() {
    let mut root = dressed_trunk(|runtime| runtime.install_builtins(T2_CANCEL_BUILTINS));
    let mailbox = root.agent.mailbox.clone();
    T2_ARRIVAL.set(Some(Box::new(move || mailbox.steer("later task".into()))));
    root.agent.provider.swap(scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![
                ral_call("first", "exarch-agents `reply 'first'"),
                ral_call("arrival", "t2-arrive"),
            ]))
            .then(Reply::panicking()),
    ));
    root.seed("one task".into());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, root.agent.id);
    assert!(matches!(root.attend(&emit), AgentOutcome::Replied));
    assert!(root.is_ready());
    assert!(
        matches!(root.inbox.next_item(), Some(Next::Item(Item::Human(t))) if t == "later task")
    );
    assert_eq!(
        crate::bus::drain_records(&rx)
            .iter()
            .filter(|r| matches!(
                r,
                crate::record::Record::Forensic(crate::record::Forensic::TurnStarted { .. })
            ))
            .count(),
        1
    );
}
