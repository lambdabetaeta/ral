use super::*;
use crate::agent::TestTrunk;
use crate::agent::testkit::*;
use crate::bus::Scoped;
use crate::provider::Refusal;
use crate::provider::scripted::{Reply, Script};
use crate::record::{Forensic, Memory, Record, Settlement};
use ral_core::data::FOValue;
use ral_core::process::{CancelCause, DurableRoot};
use std::time::Duration;

fn leave_tool_owed(session: &Agent) {
    let mut log = session.log.borrow_mut();
    log.append_user("unfinished".into(), None, crate::record::Voice::Human)
        .unwrap();
    log.append_assistant(
        genai::chat::ChatMessage::assistant(vec![genai::chat::ContentPart::ToolCall(ral_call(
            "owed", "pwd",
        ))]),
        vec!["owed".into()],
        None,
    )
    .unwrap();
}

#[test]
fn an_ended_life_draws_nothing_and_announces_no_idle_state() {
    let mut session = Agent::for_test("system").unwrap();
    session.handle().post_read(crate::bus::Read::Context);
    session.recorder().fail_after(0);
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    assert!(matches!(session.attend(&emit), AgentOutcome::Cancelled));
    assert!(!session.handle().is_live());
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|signal| matches!(
            signal,
            crate::bus::Signal::Transient(_, Transient::State(_))
        ))
    );
}

#[test]
fn a_severed_engine_ends_the_life_and_its_attendance() {
    let mut session = Agent::for_test("system").unwrap();
    let cause = Severed::Closed("engine lost".into());
    session.engine.transport().sever(cause.clone());
    assert!(!session.handle().is_live());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let lost = EngineLost::running(&cause, session.run_dir()).to_string();
    assert!(matches!(session.attend(&emit), AgentOutcome::Failed(why) if why == lost));
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|signal| matches!(
            signal,
            crate::bus::Signal::Transient(_, Transient::State(_))
        ))
    );
}

#[test]
fn failed_mandatory_repair_ends_only_its_session_and_reports_to_its_parent() {
    let mut parent = Agent::for_test("system").unwrap();
    let mut child = parent.fork().unwrap();
    let descendant = child.fork().unwrap();
    let handle = child.handle();
    leave_tool_owed(&child);
    child.seed("must not request a model".into());
    let recorder = child.recorder();
    recorder.fail_after(19);
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    let outcome = child.attend(&emit);
    let AgentOutcome::Failed(why) = &outcome else {
        panic!("{outcome:?}");
    };
    let why = why.clone();
    assert!(why.contains("injected record write failure"), "{why}");
    assert!(!child.is_ready());
    assert!(!handle.is_live());
    assert!(!descendant.handle().is_live());
    assert!(handle.steer("no more work".into()).is_err());
    assert!(!child.log.borrow().context().is_ready());
    let records =
        crate::record::read_records(&child.log_dir().path().join("record.jsonl")).unwrap();
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Protocol(crate::record::Protocol::ToolResults { .. })
            | Record::Forensic(Forensic::TurnStarted { .. })
    )));
    drop(descendant);
    child.settle(outcome, &emit);
    let faults: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|signal| match signal {
            crate::bus::Signal::Transient(_, Transient::Fault { text }) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(faults, vec![why.clone()]);
    let report = pop(&parent.inbox).expect("the parent receives failure");
    assert!(
        matches!(report, Next::Item(Item::Agent { settled: Settlement::Failed { reason }, .. }) if reason == why)
    );
    assert!(parent.handle().is_live());
    assert!(parent.check_available().is_ok());
    set_provider(
        &parent,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "reply",
                "exarch-agents `reply 42",
            )])),
        ),
    );
    parent.seed("independent work".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    assert!(matches!(parent.attend(&emit), AgentOutcome::Replied));
}

#[test]
fn a_late_recording_failure_reaches_the_parent_even_after_cancelled_attendance() {
    for cancelled in [false, true] {
        let parent = Agent::for_test("system").unwrap();
        let mut child = parent.fork().unwrap();
        child.seed("work".into());
        if cancelled {
            child.handle().end();
        } else {
            set_provider(
                &child,
                scripted(
                    "test-model",
                    Script::new().then(Reply::error(ProviderError::Other(
                        "ordinary refusal".into(),
                    ))),
                ),
            );
        }
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.id());
        let outcome = child.attend(&emit);
        assert!(child.check_available().is_ok());
        assert!(matches!(
            outcome,
            AgentOutcome::Failed(_) | AgentOutcome::Cancelled
        ));
        let recorder = child.recorder();
        recorder.fail_after(11);
        let why = recorder
            .emit(crate::record::Display::Answer {
                text: "late".into(),
            })
            .unwrap_err()
            .to_string();
        child.settle(outcome, &emit);
        let report = pop(&parent.inbox).expect("the parent receives terminal failure");
        assert!(
            matches!(report, Next::Item(Item::Agent { settled: Settlement::Failed { reason }, .. }) if reason == why)
        );
        assert!(parent.handle().is_live());
    }
}

#[test]
fn ordinary_log_refusal_repairs_and_remains_usable() {
    let mut session = Agent::for_test("system").unwrap();
    leave_tool_owed(&session);
    session.seed("refused while results owed".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    assert!(matches!(session.attend(&emit), AgentOutcome::Failed(_)));
    assert!(session.is_ready());
    assert!(session.check_available().is_ok());
    assert!(session.handle().is_live());
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "reply",
                "exarch-agents `reply 42",
            )])),
        ),
    );
    session.seed("now admissible".into());
    assert!(matches!(session.attend(&emit), AgentOutcome::Replied));
}

fn assert_refused_human(session: &Agent, records: &[Record], expected: &str) {
    assert!(session.is_ready());
    assert!(session.inbox.exchange().is_cancelled());
    assert_eq!(records.iter().filter(|record| matches!(record,
        Record::Protocol(crate::record::Protocol::UserPrompt { text, voice: crate::record::Voice::Human, .. }) if text == expected
    )).count(), 1);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::Cancelled)))
    );
}

#[test]
fn stop_after_arrival_acceptance_preserves_stop_and_new_queue_contents() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);
    session.seed("old".into());
    let job = session.inbox.await_job().unwrap();
    let drawn = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    let Next::Item(item) = &drawn.next else {
        panic!("human item")
    };
    assert!(session.arrive(item).unwrap());
    assert_eq!(session.handle().stop(job.id()), Some(vec![]));
    session.handle().steer("fresh".into()).unwrap();
    let step = session.take_up(drawn, &emit).unwrap();
    assert!(matches!(step.settled, Some(AgentOutcome::Cancelled)));
    assert_refused_human(&session, &crate::bus::drain_records(&rx), "old");
    assert_eq!(session.handle().queued(), vec!["fresh"]);
}

#[test]
fn stop_after_draw_records_the_removed_human_without_running_old_work() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);
    session.seed("old".into());
    let job = session.inbox.await_job().unwrap();
    let next = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    assert_eq!(session.handle().stop(job.id()), Some(vec![]));
    session.handle().steer("fresh".into()).unwrap();
    let step = session.take_up(next, &emit).unwrap();
    assert!(matches!(step.settled, Some(AgentOutcome::Cancelled)));
    assert_refused_human(&session, &crate::bus::drain_records(&rx), "old");
    assert_eq!(session.handle().queued(), vec!["fresh"]);
}

#[test]
fn interrupt_after_pop_records_the_prompt_without_starting_a_turn() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);
    session.seed("interrupted".into());
    let next = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    session.handle().interrupt();
    let step = session.take_up(next, &emit).unwrap();
    assert!(matches!(step.settled, Some(AgentOutcome::Cancelled)));
    assert_refused_human(&session, &crate::bus::drain_records(&rx), "interrupted");
    assert_eq!(
        session.inbox.exchange().cause(),
        Some(CancelCause::Interrupted)
    );
}

#[test]
fn only_a_current_reply_completion_parks_a_child_for_followups() {
    let held = trunk(true);
    assert_eq!(held.park_snapshot(), ParkMode::Held);
    let parent = Agent::for_test("system").unwrap();
    let child = parent.fork().unwrap();
    assert_eq!(child.park_snapshot(), ParkMode::Quiesce);
    child.handle().steer("hi".into()).unwrap();
    assert_eq!(child.park_snapshot(), ParkMode::Quiesce);
    child.inbox.deposit_reply(FOValue::Int { value: 1 });
    assert_eq!(
        child.park_snapshot(),
        ParkMode::Quiesce,
        "input outdates the reply"
    );
    assert!(pop(&child.inbox).is_some());
    child.inbox.deposit_reply(FOValue::Int { value: 2 });
    assert_eq!(child.park_snapshot(), ParkMode::Engaged);
    child.inbox.disarm_reply_lease();
    assert!(child.handle().has_reply());
    assert_eq!(child.park_snapshot(), ParkMode::Quiesce);
}

#[test]
fn an_unattended_conversing_trunk_waits_on_its_fleet_alone() {
    let embedded = trunk(false);
    assert!(!embedded.handle.0.role.returns() && !embedded.fleet.launch.trunk.attended());
    assert_eq!(embedded.park_snapshot(), ParkMode::Quiesce);
    let _helper = test_agent(
        &embedded.fleet,
        TestAgentSpec::child("helper", &embedded.handle(), embedded.inbox.mailbox()),
    )
    .unwrap();
    assert_eq!(embedded.park_snapshot(), ParkMode::HeldByChildren);
}

#[test]
fn reply_refused_identically_for_every_conversing_root() {
    let root = trunk(true);
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, root.id());
    let root_result = root
        .ral(
            &DurableRoot::default().child(),
            "exarch-agents `reply 1",
            5,
            &emit,
        )
        .text;
    let refusal = "you converse with the user; you do not return";
    assert!(root_result.contains(refusal), "{root_result}");
    let other =
        crate::agent::spawn::hatch_root(&root, Memory::Amnemon, Some("other".into())).unwrap();
    let other_result = other
        .ral(
            &DurableRoot::default().child(),
            "exarch-agents `reply 1",
            5,
            &emit,
        )
        .text;
    assert!(other_result.contains(refusal), "{other_result}");
    assert_eq!(other.park_snapshot(), ParkMode::Held);
}

#[test]
fn sub_agent_without_reply_is_re_nudged_then_fails() {
    let parent = Agent::for_test("system").unwrap();
    let mut child = parent.fork().unwrap();
    child.seed("do the thing".into());
    let script = (0..8).fold(Script::new(), |s, _| {
        s.then(Reply::text("here is prose, but no reply"))
    });
    let outcome = drive_peer(&mut child, scripted("test-model", script));
    assert!(matches!(outcome, AgentOutcome::Failed(_)), "{outcome:?}");
    assert!(child.handle().reply().is_none());
    assert!(child.is_ready());
}

#[test]
fn host_panic_is_recorded_and_the_next_prompt_still_deliberates() {
    let mut session = Agent::for_test("system").unwrap();
    let script = (0..8).fold(Script::new().then(Reply::panicking()), |s, _| {
        s.then(Reply::text("recovered"))
    });
    set_provider(&session, scripted("test-model", script));
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.seed("crash on this one".into());
    let panicked = session.attend(&emit);
    assert!(
        matches!(&panicked, AgentOutcome::Failed(m) if m.starts_with(WORKER_PANIC_PREFIX)),
        "{panicked:?}"
    );
    assert!(session.is_ready());
    session.seed("but answer this one".into());
    let outcome = session.attend(&emit);
    let signals: Vec<crate::bus::Signal> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(signals.iter().any(|s| matches!(s,
        crate::bus::Signal::Commit(_, commit) if commit.records().iter().any(|fact| matches!(fact.value(), Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)))
    )));
    assert!(signals.iter().any(|s| matches!(s,
        crate::bus::Signal::Transient(_, crate::record::Transient::Token(t)) if t == "recovered"
    )));
    assert!(matches!(outcome, AgentOutcome::Failed(_)), "{outcome:?}");
    assert!(session.is_ready());
    let parsed =
        crate::record::read_records(&session.log_dir().path().join("record.jsonl")).unwrap();
    assert!(parsed.iter().any(|r| matches!(r,
        Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)
    )));
}

#[test]
fn pinned_state_reminder_reads_the_live_register_and_becomes_the_next_prompt() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "ship the reminder"]]]]"#,
        5,
        &emit,
    );
    let digest = session.pinned_digest().unwrap();
    assert!(digest.contains("ship the reminder"), "{digest}");
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::text("done"))
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'x'",
                )])),
        ),
    );
    session.seed("get on with it".into());
    let outcome = session.attend(&emit);
    assert!(matches!(outcome, AgentOutcome::Replied), "{outcome:?}");
    assert!(
        crate::bus::drain_records(&rx)
            .into_iter()
            .any(|record| matches!(record,
                Record::Forensic(Forensic::Nudge { cause, .. }) if cause == "pinned-state reminder"
            ))
    );
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(
        view.contains("There is pinned state: ship the reminder"),
        "{view}"
    );
}

#[test]
fn pin_reminder_does_not_relivelock_the_interactive_root() {
    let mut session = trunk(true);
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep going"]]]]"#,
        5,
        &emit,
    );
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new().then(Reply::text("working on it")),
        ),
    );
    session.seed("go".into());
    let item = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    session.take_up(item, &emit).unwrap();
    let nudge = session
        .inbox
        .draw(|_| ParkMode::Quiesce)
        .expect("first completion queues one reminder");
    assert!(
        matches!(&nudge.next, Next::Item(Item::Nudge { text, .. }) if text.contains("There is pinned state")),
        "{:?}",
        nudge.next
    );
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new().then(Reply::text("still working")),
        ),
    );
    session.take_up(nudge, &emit).unwrap();
    assert!(session.inbox.draw(|_| ParkMode::Quiesce).is_none());
}

#[test]
fn chat_trunk_never_nudges() {
    let mut session = chat_trunk();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    set_provider(
        &session,
        scripted("test-model", Script::new().then(Reply::empty())),
    );
    session.seed("hello".into());
    let next = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    session.take_up(next, &emit).unwrap();
    assert!(session.inbox.draw(|_| ParkMode::Quiesce).is_none());
    assert!(
        !crate::bus::drain_records(&rx)
            .into_iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::Nudge { .. })))
    );
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(view.contains("hello"));
    assert!(!view.contains("EXARCH_REMINDER"), "{view}");
}

fn refused_twice(resume_on_reset: bool, resets_in: Duration) -> Agent {
    let mut session = Agent::for_test_with(TestTrunk {
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
    set_provider(
        &session,
        scripted("test-model", Script::new().then(refusal()).then(refusal())),
    );
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);
    for prompt in ["first", "again"] {
        session.seed(prompt.into());
        let item = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
        session.take_up(item, &emit).unwrap();
    }
    session
}

#[test]
fn a_deferred_refusal_arms_one_resume_wakeup() {
    let session = refused_twice(true, Duration::from_hours(5));
    assert!(session.schedules.unschedule(RESUME_LABEL));
    assert!(!session.schedules.armed());
}

#[test]
fn only_a_resuming_trunk_arms_a_resume_and_only_past_the_in_place_wait() {
    let armed = |resumes, resets_in| refused_twice(resumes, resets_in).schedules.armed();
    assert!(!armed(false, Duration::from_hours(5)));
    assert!(!armed(true, Duration::from_secs(5)));
}

#[test]
fn take_up_suppresses_already_awaited_done_event_without_resetting_exchange() {
    use crate::card::testkit::card_value;
    use crate::record::Display;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);
    session.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"claimed\" { 7 }\nawait $h",
        10,
        &emit,
    );
    while session.inbox.draw(|_| ParkMode::Quiesce).is_some() {}
    let _ = crate::bus::drain_records(&rx);
    session.seed("open".into());
    open_next(&session.inbox);
    let exchange = session.inbox.exchange();
    let done = DoneEvent {
        id: WorkerId(1),
        title: "claimed".into(),
        elapsed_ms: 10,
        outcome: Done::Ok,
    }
    .to_surface();
    session.inbox.life_mailbox().post(Scoped::Surface {
        id: session.id(),
        values: vec![card_value(vec![]), done],
    });
    session.inbox.nudge(1, "continue exchange".into());
    set_provider(
        &session,
        scripted("test-model", Script::new().then(Reply::panicking())),
    );
    let next = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    let step = session.take_up(next, &emit).unwrap();
    assert!(step.settled.is_none());
    assert!(session.rendered_messages().is_empty());
    assert_eq!(session.inbox.exchange().id(), exchange.id());
    assert!(
        !exchange.is_cancelled(),
        "a declined surface neither opens nor closes an exchange"
    );
    assert!(
        matches!(pop(&session.inbox), Some(Next::Item(Item::Nudge { text, .. })) if text == "continue exchange")
    );
    let records = crate::bus::drain_records(&rx);
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Record::Display(Display::Card { .. }))),
        "{records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Record::Display(Display::Done { id: Some(1), .. }))),
        "{records:?}"
    );
}

#[test]
fn take_up_delivers_unclaimed_ready_and_non_done_surfaces_and_propagates_severed() {
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);
    let stale = session.inbox.mailbox();
    session.clear().unwrap();
    stale.post(Scoped::Surface {
        id: session.id(),
        values: vec![card_value(vec![])],
    });
    assert!(session.inbox.draw(|_| ParkMode::Quiesce).is_none());
    session.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"ready-job\" { 42 }",
        10,
        &emit,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let next = loop {
        let rows = session.engine.read(|t| t.workers()).unwrap();
        if rows
            .iter()
            .any(|r| matches!(r.state, WorkerState::Ready(_)))
            && let Some(next) = session.inbox.draw(|_| ParkMode::Quiesce)
        {
            break next;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::text("acknowledged ready"))
                .then(Reply::text("acknowledged card")),
        ),
    );
    assert!(session.take_up(next, &emit).unwrap().settled.is_some());
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(
        view.contains("If you kept its handle and the result is still unclaimed, `await $h`"),
        "{view}"
    );
    session.inbox.life_mailbox().post(Scoped::Surface {
        id: session.id(),
        values: vec![card_value(vec![])],
    });
    let next = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    assert!(session.take_up(next, &emit).unwrap().settled.is_some());
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(view.contains("errand finished."), "{view}");
    let done = DoneEvent {
        id: WorkerId(1),
        title: "ready-job".into(),
        elapsed_ms: 5,
        outcome: Done::Ok,
    }
    .to_surface();
    session.inbox.life_mailbox().post(Scoped::Surface {
        id: session.id(),
        values: vec![done],
    });
    // Drawn first: the severance ends the life, which draws nothing more.
    let next = session.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    let cause = Severed::Closed("engine lost".into());
    session.engine.transport().sever(cause.clone());
    assert!(matches!(session.take_up(next, &emit), Err(c) if c == cause));
    assert!(!session.handle().is_live());
}

#[test]
fn a_nonreply_followup_waits_for_real_schedules_or_busy_descendants() {
    for scheduled in [false, true] {
        let parent = Agent::for_test_with(TestTrunk {
            allow_schedule: true,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let mut child = parent.fork_named("helper").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, child.id());
        set_provider(
            &child,
            scripted(
                "test-model",
                Script::new().then(Reply::tool_calls(vec![ral_call(
                    "first",
                    "exarch-agents `reply 'first'",
                )])),
            ),
        );
        child.seed("initial task".into());
        let first = child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
        child.take_up(first, &emit).unwrap();
        assert!(child.handle().reply_since().is_some());
        let descendant = if scheduled {
            child
                .schedules
                .schedule(
                    Trigger::After(Duration::from_hours(2)),
                    "continue later".into(),
                    "continuation".into(),
                    &child.inbox.mailbox(),
                )
                .unwrap();
            None
        } else {
            Some(
                test_agent(
                    &child.fleet,
                    TestAgentSpec::child("grandchild", &child.handle(), child.inbox.mailbox()),
                )
                .unwrap(),
            )
        };
        set_provider(
            &child,
            scripted(
                "test-model",
                Script::new().then(Reply::error(ProviderError::Api {
                    status: Some(400),
                    model: "test-model".into(),
                    message: "terminal request error".into(),
                    body: None,
                })),
            ),
        );
        child
            .handle()
            .message(&parent.handle(), "follow up".into())
            .unwrap();
        let followup = child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
        let step = child.take_up(followup, &emit).unwrap();
        assert!(matches!(step.settled, Some(AgentOutcome::Failed(_))));
        assert!(child.handle().reply_since().is_none());
        assert_eq!(
            child.park_snapshot(),
            if scheduled {
                ParkMode::UntilCancelled
            } else {
                ParkMode::HeldByChildren
            }
        );
        assert!(child.is_ready());
        if let Some(descendant) = descendant {
            drop(descendant);
            let result = child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
            assert!(matches!(
                result.next,
                Next::Item(Item::Agent {
                    settled: Settlement::Failed { .. },
                    ..
                })
            ));
        }
        child.schedules.clear();
        assert_eq!(child.park_snapshot(), ParkMode::Quiesce);
    }
}

mod lifecycle;
