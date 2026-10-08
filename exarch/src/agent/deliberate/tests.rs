use super::*;
use crate::agent::testkit::*;
use crate::bus::{AgentOutcome, Item, Post, Stamped};
use crate::cancel::InterruptTarget;
use crate::provider::scripted::{Reply, Script};
use crate::record::AgentId;
use genai::chat::ChatRole;
use ral_core::Shell;
use ral_core::Value;
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
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
    direct.parent = Some(child.agent.clone());
    let transport = bare_transport();
    direct.reach = InterruptTarget::new(ral_core::carrier::Transport::control(&transport).clone());
    let direct = test_agent(&child.fleet, direct).expect("a live child of the replying agent");
    let mut grandchild = TestAgentSpec::new("grandchild");
    grandchild.parent = Some(direct.clone());
    let grandchild = test_agent(&child.fleet, grandchild).expect("a live grandchild");
    let mut sibling = TestAgentSpec::new("sibling");
    sibling.parent = Some(parent.agent.clone());
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
    match session.deliberate(&provider, Some("go".into()), None, &emit) {
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
                crate::record::Record::Display(crate::record::Display::Prompt { text, .. })
                    if text.contains("and report")
            )),
        "the arrival must be announced as it enters context"
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
            log.append_user(bulk.clone(), None).unwrap();
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
    match session.deliberate(&provider, Some("go".into()), None, &emit) {
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

#[test]
fn stale_token_measure_is_unknown_until_the_next_completion() {
    let mut session = Avatar::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        log.append_user("old context".into(), None).unwrap();
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
        .evict(&[2], None, EditAuthority::Model)
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
    let outcome = session.deliberate(&provider, Some("new context".into()), None, &emit);
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
            mailbox.push_user("queued prompt".into());
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

static T2_CANCEL_BUILTINS_ARR: [BuiltinEntry; 3] = [
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
    let mut session = dressed_trunk(|shell| shell.install_builtins(T2_CANCEL_BUILTINS));
    T2_CANCEL_TOKEN.with(|cell| *cell.borrow_mut() = Some(session.agent.token.clone()));

    let provider = scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![
            ral_call("r1", "reply 'stale'"),
            ral_call("c2", "t2-cancel-now"),
        ])),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    match session.deliberate(&provider, Some("go".into()), None, &emit) {
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
    match session.deliberate(&provider2, Some("continue".into()), None, &emit) {
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
    let mut session = dressed_trunk(|shell| shell.install_builtins(T2_CANCEL_BUILTINS));
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
    let mut session = dressed_trunk(|shell| shell.install_builtins(T2_CANCEL_BUILTINS));
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
/// typed beside a prompt forks with it.
#[test]
fn a_read_queued_after_a_prompt_runs_after_it_lands() {
    let mut session = dressed_trunk(|shell| shell.install_builtins(T2_CANCEL_BUILTINS));
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
                    crate::record::Record::Display(
                        crate::record::Display::Prompt { text, .. }
                    ) if text == "queued prompt"
                )
            )
        })
        .expect("the prompt's arrival is announced");
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
    let parent = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let child = parent.fork().expect("fork child");

    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, child.agent.id, child.inbox.mailbox());
    let _ = child.ral("spawn { test-clear-block-forever }", 30, &emit);

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
