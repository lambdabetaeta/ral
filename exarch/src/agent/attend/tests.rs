use super::*;
use crate::agent::testkit::*;
use crate::agent::{TestTrunk, Trunk};
use crate::bus::{Input, Signal};
use crate::provider::Refusal;
use crate::provider::scripted::{Reply, Script};
use crate::record::{Display, Memory, Protocol, Record, Voice};
use ral_core::carrier::{Severed, Transport};
use ral_core::data::FOValue;
use ral_core::process::{CancelCause, DurableRoot};
use std::time::{Duration, Instant};

fn emitter(session: &Agent) -> (Emitter, crate::bus::BusReceiver) {
    let (tx, rx) = crate::bus::channel();
    (Emitter::new(tx, session.id()), rx)
}

fn human_exchange(session: &mut Agent, text: &str, emit: &Emitter) -> AgentOutcome {
    session.handle().steer(text.into()).unwrap();
    let Next::Take { input, exchange } = session.mailbox.next(&session.rest()) else {
        panic!("a queued human input is taken up")
    };
    let prompt = session.admit(input).unwrap();
    assert!(session.mailbox.begin());
    session.body.state.readings.nudges.reset();
    session.exchange(&exchange, prompt, emit)
}

fn settle(session: &mut Agent, last: AgentOutcome) {
    let Next::Settled { stopped } = session.mailbox.next(&session.rest()) else {
        panic!("an empty job with no pending helpers settles")
    };
    session.settled(stopped, Some(last));
}

fn leave_tool_owed(session: &mut Agent) {
    let log = &mut session.body.log;
    log.append_user("unfinished".into(), None, Voice::Human)
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

fn wait_until(reason: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "{reason}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn reply(value: &str) -> Reply {
    Reply::tool_calls(vec![ral_call(
        "reply",
        &format!("exarch-agents `reply {value}"),
    )])
}

#[test]
fn an_ended_life_takes_nothing_and_announces_no_idle_state() {
    let session = Agent::for_test("system").unwrap();
    session
        .handle()
        .post_command(crate::bus::Command::Read(crate::bus::Read::Context));
    session.recorder().fail_after(0);
    session.recorder().emit_or_end(Forensic::Error {
        text: "fail".into(),
    });
    let (emit, rx) = emitter(&session);
    let handle = session.handle();
    let _teardown = session.run(&emit);
    assert!(!handle.is_live());
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|signal| matches!(signal, Signal::Transient(_, Transient::State(_))))
    );
}

#[test]
fn a_severed_engine_ends_the_life_and_records_its_cause() {
    let session = Agent::for_test("system").unwrap();
    let cause = Severed::Closed("engine lost".into());
    session.engine().transport().sever(cause.clone());
    let (emit, rx) = emitter(&session);
    let handle = session.handle();
    let _teardown = session.run(&emit);
    assert!(!handle.is_live());
    let records = crate::bus::drain_records(&rx);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record,
                Record::Forensic(Forensic::EngineSevered { cause: recorded }) if recorded == &cause
            ))
            .count(),
        1
    );
}

#[test]
fn failed_mandatory_repair_ends_the_child_not_its_parent() {
    let parent = Agent::for_test("system").unwrap();
    let mut child = parent.fork().unwrap();
    let descendant = child.fork().unwrap();
    leave_tool_owed(&mut child);
    child.recorder().fail_after(0);
    assert!(child.abort_unready().is_err());
    assert!(!child.is_ready());
    assert!(!child.handle().is_live());
    assert!(!descendant.handle().is_live());
    let _teardown = child.teardown();
    assert!(matches!(
        parent.mailbox.next(&parent.rest()),
        Next::Take {
            input: Input::Child {
                status: ChildStatus::Failed(_),
                ..
            },
            ..
        }
    ));
    assert!(parent.handle().is_live());
    assert!(parent.check_available().is_ok());
}

#[test]
fn a_recording_failure_is_announced_even_when_the_life_was_already_ended() {
    for ended in [false, true] {
        let mut parent = trunk(Trunk::Converses);
        let child = parent.fork().unwrap();
        let handle = child.handle();
        let recorder = child.recorder();
        if ended {
            handle.end();
        }
        recorder.fail_after(0);
        let failure = recorder
            .emit(Forensic::Error {
                text: "late".into(),
            })
            .unwrap_err()
            .to_string();
        let (emit, rx) = emitter(&parent);
        let helper = launch_child(&mut parent, child, None, &emit);
        let failures = parent.body.state.nursery.join(helper.id());
        assert_eq!(failures, vec![failure.clone()]);
        let faults: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|signal| match signal {
                Signal::Transient(_, Transient::Fault { text }) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(faults, vec![failure]);
        assert!(matches!(
            parent.mailbox.next(&parent.rest()),
            Next::Take {
                input: Input::Child {
                    status: ChildStatus::Failed(_),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            handle.status().report,
            Some(ChildStatus::Failed(_))
        ));
    }
}

#[test]
fn ordinary_log_refusal_repairs_and_remains_usable() {
    let mut session = Agent::for_test("system").unwrap();
    leave_tool_owed(&mut session);
    let (emit, _rx) = emitter(&session);
    let outcome = human_exchange(&mut session, "refused while results owed", &emit);
    assert!(
        matches!(&outcome, AgentOutcome::Failed(reason) if !reason.starts_with(WORKER_PANIC_PREFIX))
    );
    assert!(session.is_ready());
    assert!(session.handle().is_live());
    settle(&mut session, outcome);
    set_provider(
        &session,
        scripted("test-model", Script::new().then(reply("42"))),
    );
    assert!(matches!(
        human_exchange(&mut session, "now admissible", &emit),
        AgentOutcome::Replied
    ));
}

#[test]
fn stop_after_a_mid_job_pop_records_the_prompt_without_running_a_turn() {
    let mut session = Agent::for_test("system").unwrap();
    let (emit, rx) = emitter(&session);
    session.couple(&emit);
    assert!(session.mailbox.begin());
    session.handle().steer("old".into()).unwrap();
    let Next::Take { input, exchange } = session.mailbox.next(&session.rest()) else {
        panic!("human input")
    };
    let prompt = session.admit(input).unwrap();
    let job = session.handle().status().job.unwrap();
    assert_eq!(session.handle().stop(job), Some(vec![]));
    session.handle().steer("fresh".into()).unwrap();
    assert!(matches!(
        session.exchange(&exchange, prompt, &emit),
        AgentOutcome::Cancelled
    ));
    assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
    assert_eq!(session.handle().queued(), vec!["fresh"]);
    assert!(session.is_ready());
    let records = crate::bus::drain_records(&rx);
    assert_eq!(records.iter().filter(|record| matches!(record,
        Record::Protocol(Protocol::UserPrompt { text, voice: Voice::Human, .. }) if text == "old"
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
fn reply_is_refused_identically_for_every_conversing_root() {
    let mut root = trunk(Trunk::Converses);
    let (emit, _rx) = emitter(&root);
    let refusal = "you converse with the user; you do not return";
    let result = root
        .ral(
            &DurableRoot::default().child(),
            "exarch-agents `reply 1",
            5,
            &emit,
        )
        .text;
    assert!(result.contains(refusal), "{result}");
    let mut other =
        crate::agent::spawn::hatch_root(&root, Memory::Amnemon, Some("other".into())).unwrap();
    let result = other
        .ral(
            &DurableRoot::default().child(),
            "exarch-agents `reply 1",
            5,
            &emit,
        )
        .text;
    assert!(result.contains(refusal), "{result}");
    assert!(!other.handle.0.role.returns());
}

#[test]
fn a_child_without_reply_is_renudged_then_publishes_failure_at_rest() {
    let parent = trunk(Trunk::Converses);
    let mut child = parent.fork().unwrap();
    let script = (0..4).fold(Script::new(), |script, _| {
        script.then(Reply::text("prose, no reply"))
    });
    set_provider(&child, scripted("test-model", script));
    let (emit, rx) = emitter(&child);
    let outcome = human_exchange(&mut child, "do the thing", &emit);
    assert!(matches!(&outcome, AgentOutcome::Failed(reason) if reason == NO_REPLY_REASON));
    settle(&mut child, outcome);
    assert!(
        matches!(child.handle().status().report, Some(ChildStatus::Failed(reason)) if reason == NO_REPLY_REASON)
    );
    assert!(child.is_ready());
    assert_eq!(
        crate::bus::drain_records(&rx)
            .iter()
            .filter(|record| matches!(record, Record::Forensic(Forensic::Nudge { .. })))
            .count(),
        3
    );
}

#[test]
fn provider_panic_is_recorded_and_the_next_prompt_still_deliberates() {
    let mut session = Agent::for_test("system").unwrap();
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new().then(Reply::panicking()).then(reply("42")),
        ),
    );
    let (emit, rx) = emitter(&session);
    let panicked = human_exchange(&mut session, "crash", &emit);
    assert!(
        matches!(&panicked, AgentOutcome::Failed(reason) if reason.starts_with(WORKER_PANIC_PREFIX))
    );
    assert!(session.is_ready());
    settle(&mut session, panicked);
    assert!(matches!(
        human_exchange(&mut session, "recover", &emit),
        AgentOutcome::Replied
    ));
    let recorded_panic = |record: &Record| {
        matches!(record,
            Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)
        )
    };
    assert!(crate::bus::drain_records(&rx).iter().any(recorded_panic));
    assert!(
        crate::record::read_records(&session.log_dir().path().join(crate::record::RECORD))
            .unwrap()
            .iter()
            .any(recorded_panic)
    );
}

#[test]
fn pinned_state_reminder_reads_the_live_register_and_continues_the_prompt() {
    let mut session = Agent::for_test("system").unwrap();
    let (emit, rx) = emitter(&session);
    session.ral(
        &DurableRoot::default().child(),
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "ship the reminder"]]]]"#,
        5,
        &emit,
    );
    assert!(
        session
            .pinned_digest()
            .unwrap()
            .contains("ship the reminder")
    );
    set_provider(
        &session,
        scripted(
            "test-model",
            Script::new().then(Reply::text("done")).then(reply("'x'")),
        ),
    );
    assert!(matches!(
        human_exchange(&mut session, "get on with it", &emit),
        AgentOutcome::Replied
    ));
    assert!(
        crate::bus::drain_records(&rx)
            .iter()
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
fn pin_reminder_does_not_relivelock_a_conversing_root() {
    let mut session = trunk(Trunk::Converses);
    let (emit, rx) = emitter(&session);
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
            Script::new()
                .then(Reply::text("working"))
                .then(Reply::text("still working")),
        ),
    );
    let outcome = human_exchange(&mut session, "go", &emit);
    settle(&mut session, outcome);
    assert!(session.handle().status().job.is_none());
    assert_eq!(
        crate::bus::drain_records(&rx)
            .iter()
            .filter(|record| matches!(record,
                Record::Forensic(Forensic::Nudge { cause, .. }) if cause == "pinned-state reminder"
            ))
            .count(),
        1
    );
}

#[test]
fn chat_trunk_never_nudges() {
    let mut session = chat_trunk();
    let (emit, rx) = emitter(&session);
    set_provider(
        &session,
        scripted("test-model", Script::new().then(Reply::empty())),
    );
    let outcome = human_exchange(&mut session, "hello", &emit);
    settle(&mut session, outcome);
    assert!(
        !crate::bus::drain_records(&rx)
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::Nudge { .. })))
    );
    let view = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(view.contains("hello"));
    assert!(!view.contains("EXARCH_REMINDER"), "{view}");
}

fn refused_twice(resume_on_reset: bool, resets_in: Duration) -> Agent {
    let session = Agent::for_test_with(TestTrunk {
        resume_on_reset,
        ..TestTrunk::new("system")
    })
    .unwrap();
    for _ in 0..2 {
        session.record_failure(&ProviderError::Refused(Refusal::for_test(
            Limit::Allowance,
            Some(resets_in),
        )));
    }
    session
}

#[test]
fn a_deferred_refusal_arms_only_one_resume_wakeup() {
    let session = refused_twice(true, Duration::from_hours(5));
    assert!(session.body.state.schedules.unschedule(RESUME_LABEL));
    assert!(!session.body.state.schedules.armed());
}

#[test]
fn only_a_resuming_root_arms_a_resume_past_the_in_place_wait() {
    let armed = |resumes, delay| refused_twice(resumes, delay).body.state.schedules.armed();
    assert!(!armed(false, Duration::from_hours(5)));
    assert!(!armed(true, Duration::from_secs(5)));
}

#[test]
fn an_already_claimed_worker_batch_replays_cards_but_opens_no_job() {
    use crate::card::testkit::card_value;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    let mut session = trunk(Trunk::Converses);
    let (emit, rx) = emitter(&session);
    session.couple(&emit);
    session.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"claimed\" { 7 }\nawait $h",
        10,
        &emit,
    );
    let _records = crate::bus::drain_records(&rx);
    let done = DoneEvent {
        id: WorkerId(1),
        title: "claimed".into(),
        elapsed_ms: 10,
        outcome: Done::Ok,
    }
    .to_surface();
    assert!(
        session
            .admit(Input::Surface {
                generation: session.generation,
                values: vec![card_value(vec![]), done]
            })
            .is_none()
    );
    assert!(session.handle().status().job.is_none());
    let records = crate::bus::drain_records(&rx);
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::Display(Display::Card { .. })))
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::Display(Display::Done { id: Some(1), .. })))
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::JobOpened)))
    );
}

#[test]
fn ready_workers_and_non_done_surfaces_are_admitted_but_stale_batches_are_not() {
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    let mut session = trunk(Trunk::Converses);
    let (emit, _rx) = emitter(&session);
    let stale = session.generation;
    session.clear().unwrap();
    assert!(
        session
            .admit(Input::Surface {
                generation: stale,
                values: vec![card_value(vec![])]
            })
            .is_none()
    );
    let result = session.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"ready-job\" { 42 }",
        10,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    wait_until("worker did not become ready", || {
        session
            .engine()
            .read(|transport| transport.workers())
            .unwrap()
            .iter()
            .any(|worker| matches!(worker.state, WorkerState::Ready(_)))
    });
    let worker = session
        .engine()
        .read(|transport| transport.workers())
        .unwrap()
        .into_iter()
        .find(|worker| worker.title == "ready-job" && matches!(worker.state, WorkerState::Ready(_)))
        .expect("the ready worker is visible");
    let generation = session.generation;
    let batch = || Input::Surface {
        generation,
        values: vec![
            DoneEvent {
                id: WorkerId(worker.id),
                title: "ready-job".into(),
                elapsed_ms: 5,
                outcome: Done::Ok,
            }
            .to_surface(),
        ],
    };
    let prompt = session.admit(batch()).unwrap();
    assert!(
        prompt
            .text
            .contains("If you kept its handle and the result is still unclaimed, `await $h`"),
        "{}",
        prompt.text
    );
    let prompt = session
        .admit(Input::Surface {
            generation: session.generation,
            values: vec![card_value(vec![])],
        })
        .unwrap();
    assert!(prompt.text.contains("errand finished."), "{}", prompt.text);
    session
        .engine()
        .transport()
        .sever(Severed::Closed("engine lost".into()));
    assert!(session.admit(batch()).is_none());
    assert!(!session.handle().is_live());
}

#[test]
fn a_returning_agent_waits_for_armed_schedules_and_reports_clear_them() {
    let parent = Agent::for_test_with(TestTrunk {
        allow_schedule: true,
        ..TestTrunk::new("system")
    })
    .unwrap();
    let mut child = parent.fork_named("helper").unwrap();
    child
        .body
        .state
        .schedules
        .schedule(
            Trigger::After(Duration::from_hours(2)),
            "continue".into(),
            "continuation".into(),
            &child.mailbox.address(),
        )
        .unwrap();
    assert!(child.rest().schedules);
    child.report(ChildStatus::Replied(FOValue::Unit));
    assert!(!child.rest().schedules);
    let grandchild = test_agent(
        &child.fleet,
        TestAgentSpec::child("grandchild", &child.handle(), child.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*grandchild).clone();
    child.body.state.nursery.adopt(
        handle,
        crate::agent::nursery::Joined::new(std::thread::spawn(move || {
            drop(grandchild);
            Teardown { failures: vec![] }
        })),
    );
    assert!(child.rest().pending());
    let failures = child.body.state.nursery.dismiss_all();
    assert_eq!(failures, Vec::<String>::new());
    assert!(!child.rest().pending());
}

mod lifecycle;
