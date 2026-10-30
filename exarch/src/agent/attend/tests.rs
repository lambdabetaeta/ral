use super::*;
use crate::agent::TestLaunch;
use crate::agent::testkit::*;
use crate::bus::{Input, Signal};
use crate::provider::Refusal;
use crate::provider::scripted::{Reply, Script};
use crate::record::{Display, Memory, Record, Transcript, Voice};
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
    let Next::Take { input, exchange } = session.mailbox.next(session.rest()) else {
        panic!("a queued human input is taken up")
    };
    let prompt = session.admit(input).unwrap();
    assert!(session.mailbox.begin());
    session.body.readings.nudges.reset();
    session.exchange(&exchange, prompt, emit)
}

fn task_exchange(session: &mut Agent, text: &str, emit: &Emitter) -> AgentOutcome {
    let handle = session.handle();
    let parent = handle.parent().expect("a helper's parent");
    handle
        .0
        .address
        .post(Input::Task(crate::bus::AgentMessage {
            from: parent.id(),
            from_name: parent.0.name.clone(),
            text: text.into(),
        }))
        .unwrap();
    let Next::Take { input, exchange } = session.mailbox.next(session.rest()) else {
        panic!("a queued task is taken up")
    };
    let prompt = session.admit(input).unwrap();
    assert!(session.mailbox.begin());
    session.body.readings.nudges.reset();
    session.exchange(&exchange, prompt, emit)
}

fn settle(session: &mut Agent, last: AgentOutcome) {
    let Next::Settled { stopped } = session.mailbox.next(session.rest()) else {
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
    let _posted = session
        .handle()
        .command(crate::bus::Command::Read(crate::bus::Read::Context));
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
    take_task(&child.handle(), &child.mailbox);
    leave_tool_owed(&mut child);
    child.recorder().fail_after(0);
    assert!(child.abort_unready().is_err());
    assert!(!child.is_ready());
    assert!(!child.handle().is_live());
    assert!(!descendant.handle().is_live());
    let _teardown = child.teardown();
    assert!(matches!(
        parent.mailbox.next(parent.rest()),
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
        let mut parent = trunk(false);
        let child = parent.fork().unwrap();
        let handle = child.handle();
        take_task(&handle, &child.mailbox);
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
        let failures = parent.body.nursery.join(helper.id());
        assert_eq!(failures, vec![failure.clone()]);
        let faults: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|signal| match signal {
                Signal::Transient(_, Transient::Fault { text }) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(faults, vec![failure]);
        assert!(matches!(
            parent.mailbox.next(parent.rest()),
            Next::Take {
                input: Input::Child {
                    status: ChildStatus::Failed(_),
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            handle.status().unwrap().report,
            None,
            "a failed append cannot manufacture a committed Status"
        );
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
        &mut session,
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
    let Next::Take { input, exchange } = session.mailbox.next(session.rest()) else {
        panic!("human input")
    };
    let prompt = session.admit(input).unwrap();
    let job = session.handle().status().unwrap().job.unwrap();
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
        Record::Transcript(Transcript::UserPrompt { text, voice: Voice::Human, .. }) if text == "old"
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
    let mut root = trunk(false);
    let (emit, _rx) = emitter(&root);
    let refusal = "you converse with the human; you do not return";
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
        crate::agent::testkit::human_child(&root, Memory::Amnemon, Some("other".into())).unwrap();
    let result = other
        .ral(
            &DurableRoot::default().child(),
            "exarch-agents `reply 1",
            5,
            &emit,
        )
        .text;
    assert!(result.contains(refusal), "{result}");
    assert!(
        !other
            .handle
            .0
            .address
            .transact(|txn| txn.mail().returns())
            .unwrap()
    );
}

#[test]
fn a_child_without_reply_is_renudged_then_publishes_failure_at_rest() {
    let parent = trunk(false);
    let mut child = parent.fork().unwrap();
    let script = (0..4).fold(Script::new(), |script, _| {
        script.then(Reply::text("prose, no reply"))
    });
    set_provider(&mut child, scripted("test-model", script));
    let (emit, rx) = emitter(&child);
    let outcome = task_exchange(&mut child, "do the thing", &emit);
    assert!(matches!(&outcome, AgentOutcome::Failed(reason) if reason == NO_REPLY_REASON));
    settle(&mut child, outcome);
    assert!(
        matches!(child.handle().status().unwrap().report, Some(ChildStatus::Failed(reason)) if reason == NO_REPLY_REASON)
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
        &mut session,
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
        &mut session,
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
    let mut session = trunk(false);
    let (emit, rx) = emitter(&session);
    session.ral(
        &DurableRoot::default().child(),
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep going"]]]]"#,
        5,
        &emit,
    );
    set_provider(
        &mut session,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::text("working"))
                .then(Reply::text("still working")),
        ),
    );
    let outcome = human_exchange(&mut session, "go", &emit);
    settle(&mut session, outcome);
    assert!(session.handle().status().unwrap().job.is_none());
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
        &mut session,
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
    let mut session = Agent::for_test_with(TestLaunch {
        resume_on_reset,
        ..TestLaunch::new("system")
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
    let mut session = refused_twice(true, Duration::from_hours(5));
    let body = &mut *session.body;
    assert!(
        session
            .mailbox
            .transact(|txn| body.schedules.unschedule(txn, RESUME_LABEL))
            .unwrap()
            .unwrap()
    );
    assert!(crate::agent::testkit::schedules(&session).is_empty());
}

#[test]
fn only_a_resuming_root_arms_a_resume_past_the_in_place_wait() {
    let armed = |resumes, delay| {
        !crate::agent::testkit::schedules(&refused_twice(resumes, delay)).is_empty()
    };
    assert!(!armed(false, Duration::from_hours(5)));
    assert!(!armed(true, Duration::from_secs(5)));
}

#[test]
fn an_already_claimed_worker_batch_replays_cards_but_opens_no_job() {
    use crate::card::testkit::card_value;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    let mut session = trunk(false);
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
    assert!(session.handle().status().unwrap().job.is_none());
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
            .any(|record| matches!(record, Record::Control(crate::record::Control::JobOpened)))
    );
}

#[test]
fn ready_workers_and_non_done_surfaces_are_admitted_but_stale_batches_are_not() {
    use crate::card::testkit::card_value;
    use ral_core::protocol::probe::WorkerState;
    use ral_core::shell::runtime::{Done, DoneEvent, WorkerId};
    let mut session = trunk(false);
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
    let parent = Agent::for_test_with(TestLaunch {
        allow_schedule: true,
        ..TestLaunch::new("system")
    })
    .unwrap();
    let mut child = parent.fork_named("helper").unwrap();
    take_task(&child.handle(), &child.mailbox);
    crate::agent::testkit::schedule(
        &mut child,
        Trigger::After(Duration::from_hours(2)),
        "continue".into(),
        "continuation".into(),
    );
    assert!(!crate::agent::testkit::schedules(&child).is_empty());
    assert!(child.handle().status().unwrap().owes);
    assert!(!child.rest().pending);
    assert!(!child.mailbox.has_input());
    assert!(child.mailbox.drain().is_empty());
    let child_handle = child.handle();
    let _child_timeout = ral_core::process::arm_callback(Duration::from_secs(10), move || {
        child_handle.end();
    });
    let (emit, rx) = emitter(&child);
    set_provider(
        &mut child,
        scripted(
            "test-model",
            Script::new().then(Reply::text("wait for the schedule")),
        ),
    );
    let _outcome = human_exchange(&mut child, "work", &emit);
    let records = crate::bus::drain_records(&rx);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Forensic(Forensic::Nudge { .. })
            | Record::Control(crate::record::Control::Status { .. })
    )));
    assert!(child.handle().status().unwrap().owes);
    assert!(!crate::agent::testkit::schedules(&child).is_empty());
    child.report(ChildStatus::Replied(FOValue::Unit));
    assert!(crate::agent::testkit::schedules(&child).is_empty());
    assert!(!child.handle().status().unwrap().owes);
    let parent_handle = parent.handle();
    let _parent_timeout = ral_core::process::arm_callback(Duration::from_secs(10), move || {
        parent_handle.end();
    });
    let Next::Take {
        input,
        exchange: _parent_exchange,
    } = parent.mailbox.next(parent.rest())
    else {
        panic!("the child's explicit report")
    };
    assert!(matches!(
        input,
        Input::Child { id, status: ChildStatus::Replied(FOValue::Unit), .. }
            if id == child.id()
    ));
    assert!(parent.mailbox.drain().is_empty());
    let _records = crate::bus::drain_records(&rx);
    let grandchild = test_agent(
        &child.fleet,
        TestAgentSpec::child("grandchild", &child.handle(), child.mailbox.life().child()),
    )
    .unwrap();
    take_task(&grandchild, &grandchild.mailbox);
    let handle = (*grandchild).clone();
    child.body.nursery.adopt(
        handle.clone(),
        crate::agent::nursery::Joined::new(std::thread::spawn(move || {
            wait_until("grandchild ended", || !grandchild.is_live());
            drop(grandchild);
            Teardown { failures: vec![] }
        })),
    );
    assert!(child.rest().pending);
    handle.report(ChildStatus::Replied(FOValue::Unit)).unwrap();
    assert!(!handle.status().unwrap().owes);
    assert!(!child.rest().pending);
    let Next::Take { input, exchange } = child.mailbox.next(child.rest()) else {
        panic!("grandchild report")
    };
    assert!(matches!(&input, Input::Child { id, .. } if *id == handle.id()));
    let prompt = child.admit(input).unwrap();
    assert!(child.mailbox.begin());
    assert!(!child.handle().status().unwrap().owes);
    set_provider(
        &mut child,
        scripted(
            "test-model",
            Script::new().then(Reply::text("noted the helper report")),
        ),
    );
    let outcome = child.exchange(&exchange, prompt, &emit);
    settle(&mut child, outcome);
    let records = crate::bus::drain_records(&rx);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Forensic(Forensic::Nudge { .. })
            | Record::Control(crate::record::Control::Status { .. })
    )));
    assert!(!child.handle().status().unwrap().owes);
    assert!(!child.handle().status().unwrap().working());
    assert!(matches!(
        child.handle().status().unwrap().report,
        Some(ChildStatus::Replied(FOValue::Unit))
    ));
    assert!(parent.mailbox.drain().is_empty());
    let failures = child.body.nursery.dismiss_all();
    assert_eq!(failures, Vec::<String>::new());
    assert!(!child.rest().pending);
}

mod lifecycle;

#[test]
fn a_failed_status_append_delivers_one_terminal_failure_only_when_owed() {
    for owed in [false, true] {
        let parent = trunk(false);
        let mut child = parent.fork().unwrap();
        let handle = child.handle();
        take_task(&handle, &child.mailbox);
        if !owed {
            handle.report(ChildStatus::Replied(FOValue::Unit)).unwrap();
            assert_eq!(parent.mailbox.drain().len(), 1);
        }
        handle.steer("unread local input".into()).unwrap();
        crate::agent::testkit::schedule(
            &mut child,
            crate::schedule::Trigger::After(Duration::from_hours(1)),
            "later".into(),
            "later".into(),
        );
        let before = handle.status().unwrap();
        assert_eq!(before.owes, owed);
        let pending = handle.0.address.queued_posts();
        let recorder = child.recorder();
        let bytes = child.log_dir().read(crate::record::RECORD).unwrap();
        recorder.fail_after(0);
        let failure = handle
            .report(ChildStatus::Replied(FOValue::Unit))
            .unwrap_err();
        let after = handle.status().unwrap();
        assert_eq!(after.report, before.report);
        assert_eq!(after.owes, before.owes);
        assert_eq!(after.job, before.job);
        assert_eq!(after.selected, before.selected);
        assert_eq!(handle.0.address.queued_posts(), pending);
        assert_eq!(crate::agent::testkit::schedules(&child).len(), 1);
        assert_eq!(child.log_dir().read(crate::record::RECORD).unwrap(), bytes);
        let _teardown = child.teardown();
        assert!(handle.report(ChildStatus::Stopped).is_err());
        let notices = parent.mailbox.drain();
        assert_eq!(notices.len(), usize::from(owed));
        if owed {
            assert!(
                matches!(&notices[..], [crate::bus::Drained::Input(Input::Child {
                id, status: ChildStatus::Failed(reason), ..
            })] if *id == handle.id() && reason == &failure.to_string())
            );
        }
        assert!(parent.handle().is_live());
        assert!(parent.check_available().is_ok());
    }
}

#[test]
fn a_poison_report_then_owner_teardown_delivers_only_one_failure() {
    let parent = trunk(false);
    let mut child = parent.fork().unwrap();
    let handle = child.handle();
    take_task(&handle, &child.mailbox);
    let crate::agent::engine::EngineKind::Identity(engine) = child.engine().kind() else {
        panic!("an identity test engine");
    };
    assert!(parent.mailbox.begin());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _result = handle
                .0
                .address
                .transact(|_| panic!("poison before reporting"));
        }))
        .is_err()
    );
    let failure = handle
        .report(ChildStatus::Replied(FOValue::Unit))
        .unwrap_err();
    let _teardown = child.teardown();
    assert!(engine.session_ended().unwrap().is_some());
    assert!(!handle.is_live());
    assert!(handle.report(ChildStatus::Stopped).is_err());
    let notices = parent.mailbox.drain();
    assert!(
        matches!(&notices[..], [crate::bus::Drained::Input(Input::Child {
        id, status: ChildStatus::Failed(reason), ..
    })] if *id == handle.id() && reason == &failure.to_string())
    );
    assert!(matches!(
        parent.mailbox.next(parent.rest()),
        Next::Settled { stopped: false }
    ));
    assert!(parent.mailbox.drain().is_empty());
}

#[test]
fn cell_poison_does_not_replace_an_earlier_recording_failure() {
    let parent = trunk(false);
    let mut child = parent.fork().unwrap();
    let handle = child.handle();
    take_task(&handle, &child.mailbox);
    let recorder = child.recorder();
    recorder.fail_after(0);
    let first = recorder
        .emit(Display::Answer {
            text: "lost".into(),
        })
        .unwrap_err()
        .to_string();
    assert!(first.starts_with("recording to "));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _result = handle
                .0
                .address
                .transact(|_| panic!("poison after recording failed"));
        }))
        .is_err()
    );
    assert_eq!(handle.status().err().unwrap().to_string(), first);
    assert_eq!(
        handle.report(ChildStatus::Stopped).unwrap_err().to_string(),
        first
    );
    let _teardown = child.teardown();
    assert!(
        matches!(&parent.mailbox.drain()[..], [crate::bus::Drained::Input(Input::Child {
        status: ChildStatus::Failed(reason), ..
    })] if reason == &first)
    );
    assert_eq!(recorder.check_available().unwrap_err().to_string(), first);
}
