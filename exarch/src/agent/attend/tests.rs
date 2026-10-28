use super::*;
use crate::agent::TestTrunk;
use crate::agent::testkit::*;
use crate::provider::Refusal;
use crate::provider::scripted::{Reply, Script};
use crate::record::{Forensic, Memory, Record};
use ral_core::data::FOValue;
use std::time::Duration;

type DrawHook = Box<dyn FnOnce(&Avatar)>;

thread_local! {
    static BEFORE_DRAW: std::cell::Cell<Option<DrawHook>> =
        const { std::cell::Cell::new(None) };
    static AFTER_ARRIVAL: std::cell::Cell<Option<DrawHook>> =
        const { std::cell::Cell::new(None) };
}

pub(super) fn before_draw(avatar: &Avatar) {
    if let Some(hook) = BEFORE_DRAW.take() {
        hook(avatar);
    }
}

pub(super) fn after_arrival(avatar: &Avatar) {
    if let Some(hook) = AFTER_ARRIVAL.take() {
        hook(avatar);
    }
}

#[test]
fn stop_after_arrival_acceptance_preserves_stop_and_new_queue_contents() {
    let mut session = Avatar::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.couple(&emit);
    session.inbox.push_user("old".into());
    let drawn = session.draw(Idle::Return).unwrap();
    let (accepted, accepting) = std::sync::mpsc::channel();
    let (stopped, stopping) = std::sync::mpsc::channel();
    let agent = session.agent.clone();
    let worker = std::thread::spawn(move || {
        accepting.recv().unwrap();
        agent.stop();
        agent.post(Post::Nudge {
            prompt: 2,
            text: "new nudge".into(),
        });
        agent.steer("fresh".into());
        stopped.send(()).unwrap();
    });
    AFTER_ARRIVAL.set(Some(Box::new(move |_| {
        accepted.send(()).unwrap();
        stopping.recv().unwrap();
    })));
    let step = session
        .take_up_drawn(&drawn.next, &drawn.stamp, &emit)
        .unwrap();
    worker.join().unwrap();
    assert!(step.settled.is_none());
    assert!(session.agent.token.is_cancelled());
    assert_refused_human(&session, &crate::bus::drain_records(&rx), "old");
    assert!(
        matches!(session.inbox.next_item(), Some(Next::Item(Item::Nudge { text, .. })) if text == "new nudge")
    );
    assert_eq!(session.agent.queued(), vec!["fresh"]);
}

fn assert_refused_human(session: &Avatar, records: &[Record], expected: &str) {
    assert!(session.is_ready());
    assert!(session.agent.token.is_cancelled());
    assert_eq!(records.iter().filter(|record| matches!(
        record,
        Record::Protocol(crate::record::Protocol::UserPrompt { text, voice: crate::record::Voice::Human, .. }) if text == expected
    )).count(), 1);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
    );
}

#[test]
fn stop_after_draw_records_the_removed_human_without_running_old_work() {
    let mut session = Avatar::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.couple(&emit);
    session.agent.steer("old".into());
    let drawn = session.draw(Idle::Return).unwrap();
    assert_eq!(session.agent.stop(), Vec::<String>::new());
    session.agent.steer("fresh".into());
    let step = session
        .take_up_drawn(&drawn.next, &drawn.stamp, &emit)
        .unwrap();
    assert!(step.settled.is_none());
    assert_refused_human(&session, &crate::bus::drain_records(&rx), "old");
    assert_eq!(session.agent.queued(), vec!["fresh"]);
}

#[test]
fn a_pre_stop_draw_is_refused_before_any_arrival_fact() {
    let mut session = Avatar::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.couple(&emit);
    session
        .agent
        .mailbox
        .stamp()
        .post(crate::bus::Stamped::AgentResult(crate::bus::AgentResult {
            id: crate::record::AgentId::new(77),
            name: "old helper".into(),
            outcome: AgentOutcome::Cancelled,
            elapsed: Duration::ZERO,
        }));
    let drawn = session.draw(Idle::Return).unwrap();
    session.agent.stop();
    let step = session
        .take_up_drawn(&drawn.next, &drawn.stamp, &emit)
        .unwrap();
    assert!(step.settled.is_none());
    assert!(session.agent.token.is_cancelled());
    assert!(
        !crate::bus::drain_records(&rx).iter().any(|record| matches!(
            record,
            Record::Display(crate::record::Display::SubagentDone { .. })
        ))
    );
}

#[test]
fn only_a_current_reply_completion_parks_a_child_for_followups() {
    let held = trunk(true);
    assert_eq!(held.park_mode(), ParkMode::Held);
    let parent = Avatar::for_test("system").unwrap();
    let child = parent.fork().expect("fork child");
    assert_eq!(child.park_mode(), ParkMode::Quiesce);
    child.agent.mailbox.steer("hi".into());
    assert_eq!(child.park_mode(), ParkMode::Quiesce);
    child
        .agent
        .deposit_reply(&parent.fleet, FOValue::Int { value: 1 });
    assert_eq!(child.park_mode(), ParkMode::Engaged);
    child.agent.disarm_reply_lease();
    assert!(child.agent.has_reply());
    assert_eq!(child.park_mode(), ParkMode::Quiesce);
}

/// A conversing trunk with no human attached — the embedded one synod
/// drives — holds for live children and quiesces once they settle, where
/// an attended one would park on the human who is not there to type.
#[test]
fn an_unattended_conversing_trunk_waits_on_its_fleet_alone() {
    let embedded = trunk(false);
    assert!(
        !embedded.agent.role.returns() && !embedded.fleet.launch.trunk.attended(),
        "the fixture is a conversing trunk nobody types into"
    );
    assert_eq!(embedded.park_mode(), ParkMode::Quiesce);
    let mut spec = TestAgentSpec::new("helper");
    spec.role = crate::agent::Role::child(&embedded.agent);
    let _helper = test_agent(&embedded.fleet, spec).expect("a live child");
    assert_eq!(embedded.park_mode(), ParkMode::HeldByChildren);
}

/// The `reply` refusal keys on the role, not on trunk-ness, and is an ordinary call error rather than a termination.
#[test]
fn reply_refused_identically_for_every_conversing_root() {
    let root = trunk(true);
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, root.agent.id);
    let root_result = root.ral("exarch-agents `reply 1", 5, &emit).text;
    let refusal = "you converse with the user; you do not return";
    assert!(root_result.contains(refusal), "got: {root_result}");

    // A distinct name: `root` already holds `TRUNK_NAME` in this same
    // fleet, and names are unique among the live.
    let other = crate::agent::spawn::hatch_root(&root, Memory::Amnemon, Some("other".into()))
        .expect("a second root");
    let other_result = other.ral("exarch-agents `reply 1", 5, &emit).text;
    assert!(
        other_result.contains(refusal),
        "a second root must be refused with the same text, got: {other_result}"
    );
    assert_eq!(
        other.park_mode(),
        ParkMode::Held,
        "a second root still parks Held after a refused reply"
    );
}

/// An un-replied finish is re-nudged within budget, then settles `Failed`
/// — the final prose is never scraped as the answer.
#[test]
fn sub_agent_without_reply_is_re_nudged_then_fails() {
    let parent = Avatar::for_test("system").unwrap();
    let mut child = parent.fork().expect("fork child");
    child.seed("do the thing".into());
    // More prose-only replies than the budget will consume, so the test
    // does not couple to the exact budget.
    let mut script = Script::new();
    for _ in 0..8 {
        script = script.then(Reply::text("here is prose, but no reply"));
    }
    let outcome = drive_peer(&mut child, scripted("test-model", script));
    assert!(
        matches!(outcome, AgentOutcome::Failed(_)),
        "an un-replied finish settles Failed, got {outcome:?}"
    );
    assert!(
        child.agent.reply().is_none(),
        "the final prose must not be scraped as a reply"
    );
    assert!(child.is_ready());
}

/// A host-side unwind — transport, decode, render — is recorded, fails the
/// item, and leaves the log ready, so the next prompt still deliberates.
/// The eval-side panics in `agent/shell.rs` never reach this arm:
/// `Shell::run` rolls those back long before the loop sees them.
#[test]
fn host_panic_is_recorded_and_the_next_prompt_still_deliberates() {
    let mut session = Avatar::for_test("system").unwrap();
    // The first prompt unwinds; the rest answer the second and the no-reply
    // nudges it draws.
    let mut script = Script::new().then(Reply::panicking());
    for _ in 0..8 {
        script = script.then(Reply::text("recovered"));
    }
    session.agent.provider.swap(scripted("test-model", script));

    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.seed("crash on this one".into());
    let panicked = session.attend(&emit);
    assert!(
        matches!(&panicked, AgentOutcome::Failed(m) if m.starts_with(WORKER_PANIC_PREFIX)),
        "the unwind must fail the item, not sink the attend thread: {panicked:?}"
    );
    assert!(
        session.is_ready(),
        "the panicked exchange must be wound back"
    );

    // Seeded only now: consecutive prompts coalesce into one inbox entry.
    session.seed("but answer this one".into());
    let outcome = session.attend(&emit);

    let signals: Vec<crate::bus::Signal> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        signals.iter().any(|s| matches!(
            s,
            crate::bus::Signal::Fact(_, fact)
                if matches!(fact.value(), Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX))
        )),
        "the unwind must reach the user as an error too"
    );
    assert!(
        signals.iter().any(|s| matches!(
            s,
            crate::bus::Signal::Transient(_, crate::record::Transient::Token(t)) if t == "recovered"
        )),
        "the prompt after the panic must still deliberate"
    );
    assert!(
        matches!(outcome, AgentOutcome::Failed(_)),
        "an un-replied run settles Failed, got {outcome:?}"
    );
    assert!(session.is_ready());

    let parsed = crate::record::read_records(&session.log_dir().path().join("record.jsonl"))
        .expect("the panicked exchange must leave a parseable log");
    assert!(
        parsed.iter().any(|r| matches!(
            r,
            Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)
        )),
        "the panic is recorded in the log too"
    );
}

/// The reminder is built from the register the model actually wrote, and it
/// becomes the next committed prompt: `pinned_digest` → `Facts` →
/// `Post::Nudge` → the user turn the next deliberation sends.  The nudge
/// unit tests hand `react` a hand-written digest; only this joins it to the
/// live register.
#[test]
fn pinned_state_reminder_reads_the_live_register_and_becomes_the_next_prompt() {
    let mut session = Avatar::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.ral(
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "ship the reminder"]]]]"#,
        5,
        &emit,
    );
    let digest = session
        .pinned_digest()
        .expect("the model's own pin must reach the register");
    assert!(digest.contains("ship the reminder"), "got: {digest}");

    // A finish with nothing returned draws the reminder; the single
    // `reply` that follows ends the loop, with no verification round.
    session.agent.provider.swap(scripted(
        "test-model",
        Script::new()
            .then(Reply::text("done"))
            .then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'x'",
            )])),
    ));
    session.seed("get on with it".into());
    let outcome = session.attend(&emit);

    assert!(
        matches!(outcome, AgentOutcome::Replied),
        "the loop must terminate on the first reply, got {outcome:?}"
    );
    assert!(
        crate::bus::drain_records(&rx)
            .into_iter()
            .any(|record| matches!(
                record,
                Record::Forensic(Forensic::Nudge { cause, .. }) if cause == "pinned-state reminder"
            )),
        "the live register must raise a pinned-state nudge"
    );
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(
        view.contains("There is pinned state: ship the reminder"),
        "the reminder must be committed as the next prompt, not dropped: {view}"
    );
}

/// A returning agent has no reply-triggered nudge round to relive; the
/// livelock regression instead lives here, for the interactive root: a
/// stationary model-written pin queues at most one nudge, never a
/// perpetual `Complete → nudge → Complete` cycle that would never let the
/// loop park.
#[test]
fn pin_reminder_does_not_relivelock_the_interactive_root() {
    let mut session = trunk(true);
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.ral(
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep going"]]]]"#,
        5,
        &emit,
    );

    session.agent.provider.swap(scripted(
        "test-model",
        Script::new().then(Reply::text("working on it")),
    ));
    session.seed("go".into());
    let item = session.inbox.next_item().expect("the seeded item");
    session
        .take_up(&item, &emit)
        .expect("an identity seat never severs");

    let nudge = session
        .inbox
        .next_item()
        .expect("the first quiet completion must queue one pin reminder");
    assert!(
        matches!(&nudge, Next::Item(Item::Nudge { text, .. }) if text.contains("There is pinned state")),
        "expected a pin reminder, got {nudge:?}"
    );

    session.agent.provider.swap(scripted(
        "test-model",
        Script::new().then(Reply::text("still working")),
    ));
    session
        .take_up(&nudge, &emit)
        .expect("an identity seat never severs");

    assert!(
        session.inbox.next_item().is_none(),
        "a stationary pin must queue no second nudge: the loop would park, not relivelock"
    );
}

/// `--chat` withholds the tool, so nothing is left to steer the model
/// toward: the empty turn that
/// `nudge::tests::empty_turn_nudges_and_consumes_budget` nudges over here
/// stands as the model left it, and no synthetic prompt is committed.
#[test]
fn chat_trunk_never_nudges() {
    let mut session = chat_trunk();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session
        .agent
        .provider
        .swap(scripted("test-model", Script::new().then(Reply::empty())));
    session.seed("hello".into());
    session.attend_backlog(&emit);

    assert!(
        !crate::bus::drain_records(&rx)
            .into_iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::Nudge { .. }))),
        "a chat trunk raises no nudge"
    );
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(
        view.contains("hello"),
        "the exchange must have happened at all"
    );
    assert!(
        !view.contains("EXARCH_REMINDER"),
        "nothing synthetic may join a chat conversation: {view}"
    );
}

/// A trunk refused twice, until a reset `resets_in` after each refusal.
fn refused_twice(resume_on_reset: bool, resets_in: Duration) -> Avatar {
    let mut session = Avatar::for_test_with(TestTrunk {
        resume_on_reset,
        ..TestTrunk::new("system")
    })
    .unwrap();
    let refusal = || {
        Reply::error(ProviderError::Refused(Refusal::for_test(
            Limit::Allowance,
            Some(resets_in),
        )))
    };
    let script = Script::new().then(refusal()).then(refusal());
    session.agent.provider.swap(scripted("test-model", script));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.couple(&emit);
    for prompt in ["first", "again"] {
        session.seed(prompt.into());
        let item = session.inbox.next_item().expect("the seeded item");
        session
            .take_up(&item, &emit)
            .expect("an identity seat never severs");
    }
    session
}

/// A deferred refusal leaves exactly one wakeup at its reset, however many
/// times it is refused.
#[test]
fn a_deferred_refusal_arms_one_resume_wakeup() {
    let session = refused_twice(true, Duration::from_hours(5));
    assert!(
        session.agent.schedules.unschedule(RESUME_LABEL),
        "the wakeup bears the provider-reset label"
    );
    assert!(
        !session.agent.schedules.armed(),
        "the second refusal replaced the first rather than adding to it"
    );
}

#[test]
fn only_a_resuming_trunk_arms_a_resume_and_only_past_the_in_place_wait() {
    let armed = |resumes, resets_in| refused_twice(resumes, resets_in).agent.schedules.armed();
    assert!(!armed(false, Duration::from_hours(5)));
    assert!(!armed(true, Duration::from_secs(5)));
}

/// A queued `DoneEvent` for a worker already claimed by `await` must not
/// open a new exchange in `take_up`: it replays cards to the scrollback,
/// skips the provider, and leaves exchange latches, token, and queued nudges
/// untouched.
#[test]
fn take_up_suppresses_already_awaited_done_event_without_resetting_exchange() {
    use crate::bus::Stamped;
    use crate::card::testkit::card_value;
    use crate::record::Display;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};

    let mut session = Avatar::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.couple(&emit);

    session.ral("let h = spawn \"claimed\" { 7 }\nawait $h", 10, &emit);
    while session.inbox.next_item().is_some() {}
    let _ = crate::bus::drain_records(&rx);

    let done = DoneEvent {
        id: WorkerId(1),
        title: "claimed".into(),
        elapsed_ms: 10,
        outcome: Done::Ok,
    }
    .to_surface();
    session.inbox.mailbox().stamp().post(Stamped::Surface {
        id: session.agent.id,
        values: vec![card_value(vec![]), done],
    });
    session.inbox.push(Post::Nudge {
        prompt: 1,
        text: "continue exchange".into(),
    });
    session
        .agent
        .token
        .cancel(ral_core::process::CancelCause::Interrupted);
    session.agent.provider.swap(scripted(
        "test-model",
        Script::new().then(Reply::panicking()),
    ));

    let surface_next = session.inbox.next_item().expect("queued surface batch");
    let step = session
        .take_up(&surface_next, &emit)
        .expect("an identity seat never severs");
    assert!(
        step.settled.is_none(),
        "a suppressed completion settles nothing"
    );
    assert!(
        session.rendered_messages().is_empty(),
        "the provider must not be called and no message may enter context"
    );
    assert!(
        session.agent.token.is_cancelled(),
        "a suppressed completion must not reset the exchange token"
    );
    assert!(
        matches!(
            session.inbox.next_item(),
            Some(Next::Item(Item::Nudge { ref text, .. })) if text == "continue exchange"
        ),
        "a suppressed completion must not drop queued nudges"
    );
    let records = crate::bus::drain_records(&rx);
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Record::Display(Display::Card { .. }))),
        "cards in the suppressed batch still replay: {records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Record::Display(Display::Done { id: Some(1), .. }))),
        "the done display line still replays: {records:?}"
    );
}

/// An unclaimed `Ready` worker's completion notice and a `Surface` batch
/// without a `DoneEvent` both wake the model in `take_up`, while a severed
/// `workers()` probe propagates `Severed` and a `/clear` fences stale batches.
#[test]
fn take_up_delivers_unclaimed_ready_and_non_done_surfaces_and_propagates_severed() {
    use crate::bus::Stamped;
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};

    let mut session = Avatar::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.agent.id);
    session.couple(&emit);

    // 1. `/clear` epoch fence: stamped before clear is rejected at inbox pop.
    let stale_stamp = session.inbox.mailbox().stamp();
    session.clear().unwrap();
    stale_stamp.post(Stamped::Surface {
        id: session.agent.id,
        values: vec![card_value(vec![])],
    });
    assert!(
        session.inbox.next_item().is_none(),
        "stale epoch batch is dropped at pop"
    );

    // 2. Genuine unclaimed Ready worker wakes the model.
    session.ral("let h = spawn \"ready-job\" { 42 }", 10, &emit);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let next = loop {
        let rows = session.seat.read(|t| t.workers()).unwrap();
        if rows
            .iter()
            .any(|r| matches!(r.state, WorkerState::Ready(_)))
            && let Some(next) = session.inbox.next_item()
        {
            break next;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    session.agent.provider.swap(scripted(
        "test-model",
        Script::new()
            .then(Reply::text("acknowledged ready"))
            .then(Reply::text("acknowledged card")),
    ));
    let step = session.take_up(&next, &emit).unwrap();
    assert!(step.settled.is_some());
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(
        view.contains("If you kept its handle and the result is still unclaimed, `await $h`"),
        "the unclaimed completion notice must enter model context: {view}"
    );

    // 3. Surface batch without a DoneEvent also wakes the model.
    session.inbox.mailbox().stamp().post(Stamped::Surface {
        id: session.agent.id,
        values: vec![card_value(vec![])],
    });
    let card_only = session.inbox.next_item().expect("card-only surface");
    let step = session.take_up(&card_only, &emit).unwrap();
    assert!(step.settled.is_some());
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(
        view.contains("errand finished."),
        "a non-done surface batch enters context: {view}"
    );

    // 4. Severed `workers()` probe propagates `Severed`.
    let cause = Severed::Closed("engine lost".into());
    session.seat.transport().sever(cause.clone());
    let severed_done = DoneEvent {
        id: WorkerId(1),
        title: "ready-job".into(),
        elapsed_ms: 5,
        outcome: Done::Ok,
    }
    .to_surface();
    session.inbox.mailbox().stamp().post(Stamped::Surface {
        id: session.agent.id,
        values: vec![severed_done],
    });
    let severed_next = session.inbox.next_item().expect("severed surface");
    assert!(matches!(session.take_up(&severed_next, &emit), Err(c) if c == cause));
}

#[test]
fn a_nonreply_followup_waits_for_real_schedules_or_busy_descendants() {
    for scheduled in [false, true] {
        let parent = Avatar::for_test_with(TestTrunk {
            allow_schedule: true,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let mut child = parent.fork_named("helper").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.agent.id);
        child.agent.provider.swap(scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "first",
                "exarch-agents `reply 'first'",
            )])),
        ));
        child.seed("initial task".into());
        let first = child.inbox.next_item().unwrap();
        child.take_up(&first, &emit).unwrap();
        assert!(child.agent.reply_completion_armed());
        let descendant = if scheduled {
            child
                .agent
                .schedules
                .schedule(
                    Trigger::After(Duration::from_hours(2)),
                    "continue later".into(),
                    "continuation".into(),
                    &child.agent.mailbox,
                )
                .unwrap();
            None
        } else {
            let mut spec = TestAgentSpec::new("grandchild");
            spec.role = Role::child(&child.agent);
            Some(test_agent(&child.fleet, spec).unwrap())
        };
        child.agent.provider.swap(scripted(
            "test-model",
            Script::new().then(Reply::error(ProviderError::Api {
                status: Some(400),
                model: "test-model".into(),
                message: "terminal request error".into(),
                body: None,
            })),
        ));
        parent.agent.message(&child.agent, "follow up".into());
        let followup = child.inbox.next_item().unwrap();
        let step = child.take_up(&followup, &emit).unwrap();
        assert!(matches!(step.settled, Some(AgentOutcome::Failed(_))));
        assert!(!child.agent.reply_completion_armed());
        assert_eq!(
            child.park_mode(),
            if scheduled {
                ParkMode::UntilCancelled
            } else {
                ParkMode::HeldByChildren
            }
        );
        assert!(child.is_ready());
        drop(descendant);
        child.agent.schedules.clear();
        assert_eq!(child.park_mode(), ParkMode::Quiesce);
    }
}

mod lifecycle;
