use super::*;
use crate::bus::{Drained, Input};
use crate::record::{ChildStatus, Forensic, Record, Transcript};
use ral_core::carrier::Transport as _;
use ral_core::data::FOValue;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry};
use std::borrow::Cow;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

thread_local! {
    static REFUSE_BOOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BOOTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static BOOT_ROOTS: std::cell::RefCell<Vec<DurableRoot>> = const { std::cell::RefCell::new(Vec::new()) };
}
static INSTALLERS: [EngineInstaller; 1] = [EngineInstaller {
    tag: crate::shell_eval::builtins::INSTALLER_TAG,
    runtime: |spec| {
        BOOTS.set(BOOTS.get() + 1);
        if REFUSE_BOOT.replace(false) {
            return Err("test boot refusal".into());
        }
        let mut runtime = crate::boot::engine_runtime(spec)?;
        runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        Ok(runtime)
    },
    root: |root, shell| {
        crate::boot::engine_root(root, shell)?;
        BOOT_ROOTS.with_borrow_mut(|roots| roots.push(shell.cancel_handle()));
        Ok(())
    },
    narrow: crate::policy::base_layer,
}];

fn identity(agent: &Agent) -> Arc<ral_core::carrier::IdentityTransport> {
    let crate::agent::engine::EngineKind::Identity(transport) = agent.engine().kind() else {
        panic!("an identity test engine")
    };
    transport
}

#[test]
fn clear_dismisses_and_joins_helpers_drops_reports_and_keeps_human_and_peer_input() {
    let mut parent = Agent::for_test("system").unwrap();
    let child = parent.fork_named("helper").unwrap();
    let child_engine = identity(&child);
    let (emit, _rx) = crate::bus::dummy_emitter();
    let helper = launch_child(&mut parent, child, None, &emit);
    let peer = test_agent(&parent.fleet, TestAgentSpec::new("peer")).unwrap();
    parent.handle().steer("human before clear".into()).unwrap();
    parent
        .handle()
        .message(&peer, "peer before clear".into())
        .unwrap();
    parent
        .mailbox
        .address()
        .post(Input::Child {
            id: helper.id(),
            name: "helper".into(),
            status: ChildStatus::Stopped,
            elapsed: Duration::ZERO,
        })
        .unwrap();
    parent
        .mailbox
        .address()
        .post(Input::Surface {
            generation: parent.generation,
            values: vec![FOValue::Unit],
        })
        .unwrap();
    parent.clear().unwrap();
    assert!(!helper.is_live());
    assert_eq!(child_engine.session_ended().unwrap(), Some(143));
    assert!(
        parent.fleet.registry.resolve("helper").is_none(),
        "clear joined the helper and released its name"
    );
    assert!(!parent.rest().pending);
    assert!(parent.body.nursery.named("helper").is_none());
    assert!(parent.fleet.registry.name("helper").is_ok());
    let inputs = parent.mailbox.drain();
    assert!(
        matches!(&inputs[..], [Drained::Input(Input::Human(text)), Drained::Input(Input::Message(message))]
        if text == "human before clear" && message.text == "peer before clear")
    );
}

#[test]
fn a_batch_from_the_old_generation_is_declined_but_the_new_sink_is_live() {
    let mut agent = Agent::for_test("system").unwrap();
    let old = crate::shell_eval::deferred_sink(&agent.mailbox.address(), agent.generation);
    old.deliver(vec![FOValue::Unit]);
    agent.seed("keep queued".into());
    let outgoing = identity(&agent);
    agent.clear().unwrap();
    assert_eq!(outgoing.session_ended().unwrap(), Some(143));
    assert_eq!(agent.generation, 1);
    assert_eq!(agent.handle().queued(), ["keep queued"]);
    let queued = agent.mailbox.drain();
    assert!(matches!(&queued[..], [Drained::Input(Input::Human(_))]));
    old.deliver(vec![FOValue::Unit]);
    let Some(Drained::Input(input)) = agent.mailbox.drain().pop() else {
        panic!("late old batch")
    };
    assert!(agent.admit(input).is_none());
    assert!(agent.handle().status().unwrap().job.is_none());
    let fresh = crate::shell_eval::deferred_sink(&agent.mailbox.address(), agent.generation);
    fresh.deliver(vec![FOValue::Unit]);
    let Some(Drained::Input(input)) = agent.mailbox.drain().pop() else {
        panic!("new batch")
    };
    assert!(agent.admit(input).is_some());
}

#[test]
fn failed_clear_preparation_preserves_helpers_workers_history_and_registers() {
    use genai::chat::{ContentPart, ToolCall};
    for failure in ["unready", "unavailable", "boot"] {
        let mut agent = Agent::for_test_with(TestTrunk {
            installers: &INSTALLERS,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let (emit, _rx) = crate::bus::dummy_emitter();
        let result = agent.ral(
            &DurableRoot::default().child(),
            r#"let keep = 1
spawn "blocker" { test-clear-block-forever }
exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep this pin"]]]]"#,
            5,
            &emit,
        );
        assert!(!result.failed, "{}", result.text);
        let entries = workers(&agent);
        assert_eq!(entries.len(), 1);
        let child = agent.fork_named("helper").unwrap();
        let helper_engine = identity(&child);
        let helper = launch_child(&mut agent, child, None, &emit);
        crate::agent::testkit::schedule(
            &mut agent,
            crate::schedule::Trigger::After(Duration::from_hours(1)),
            "ping".into(),
            "ping".into(),
        );
        agent.seed("keep queued".into());
        agent.body.readings.measure = Some(crate::agent::gauge::Measure { tokens: 17, at: 2 });
        assert!(agent.body.readings.gauges.disk(2, 1).is_some());
        let facts = crate::agent::nudge::Facts {
            must_reply: false,
            pinned: Some("keep".into()),
            quiet: true,
        };
        {
            let body = &mut *agent.body;
            assert!(
                body.readings
                    .nudges
                    .react(
                        Ok(&crate::agent::outcome::Outcome::Complete),
                        &facts,
                        &body.log
                    )
                    .is_some()
            );
            body.log
                .append_user("keep history".into(), None, Voice::Human)
                .unwrap();
            if failure == "unready" {
                body.log
                    .append_assistant(
                        ChatMessage::assistant(vec![ContentPart::ToolCall(ToolCall {
                            call_id: "pending".into(),
                            fn_name: "ral".into(),
                            fn_arguments: serde_json::json!({"command": "pwd"}),
                            thought_signatures: None,
                        })]),
                        vec!["pending".into()],
                        None,
                    )
                    .unwrap();
            }
        }
        let before = serde_json::to_vec(&agent.rendered_messages()).unwrap();
        let outgoing = identity(&agent);
        let boots = BOOTS.get();
        match failure {
            "boot" => REFUSE_BOOT.set(true),
            "unavailable" => {
                let recorder = agent.recorder();
                recorder.fail_after(1);
                assert!(
                    recorder
                        .emit(Forensic::SystemNote {
                            text: "fail".into()
                        })
                        .is_err()
                );
            }
            "unready" => {}
            _ => unreachable!(),
        }
        assert_ne!(agent.clear().unwrap_err(), "");
        assert_eq!(BOOTS.get() - boots, u32::from(failure == "boot"));
        assert!(Arc::ptr_eq(&outgoing, &identity(&agent)));
        assert_eq!(agent.generation, 0);
        assert_eq!(outgoing.session_ended().unwrap(), None);
        if failure != "unavailable" {
            assert_eq!(helper_engine.session_ended().unwrap(), None);
        }
        assert_eq!(helper.is_live(), failure != "unavailable");
        assert!(agent.rest().pending);
        if failure != "unavailable" {
            assert_eq!(agent.handle().queued(), ["keep queued"]);
        }
        assert!(
            agent
                .engine()
                .read(|transport| transport.bindings())
                .unwrap()
                .iter()
                .any(|binding| binding.name == "keep" && binding.preview == "1")
        );
        assert!(!entries[0].handle.cancel.is_cancelled());
        assert_eq!(crate::agent::testkit::schedules(&agent).len(), 1);
        assert_eq!(agent.body.pins.len(), 1);
        assert_eq!(
            agent.body.readings.measure.map(|m| (m.tokens, m.at)),
            Some((17, 2))
        );
        assert!(agent.body.readings.gauges.disk(2, 1).is_none());
        {
            let body = &mut *agent.body;
            assert!(
                body.readings
                    .nudges
                    .react(
                        Ok(&crate::agent::outcome::Outcome::Complete),
                        &facts,
                        &body.log
                    )
                    .is_none()
            );
        }
        assert_eq!(
            serde_json::to_vec(&agent.rendered_messages()).unwrap(),
            before
        );
    }
}

#[test]
fn failed_clear_commit_keeps_the_outgoing_engine_and_ends_the_life() {
    let mut agent = Agent::for_test_with(TestTrunk {
        installers: &INSTALLERS,
        ..TestTrunk::new("system")
    })
    .unwrap();
    let outgoing = identity(&agent);
    let boots = BOOTS.get();
    agent.recorder().fail_after(1);
    assert!(agent.clear().is_err());
    assert_eq!(BOOTS.get(), boots + 1, "a prepared engine was booted");
    BOOT_ROOTS.with_borrow(|roots| {
        assert!(
            roots.last().unwrap().is_cancelled(),
            "the discarded prepared engine was ended"
        );
        assert!(
            !roots[roots.len() - 2].is_cancelled(),
            "the outgoing engine is still owned"
        );
    });
    assert!(!agent.handle().is_live());
    assert!(Arc::ptr_eq(&outgoing, &identity(&agent)));
    assert_eq!(agent.generation, 0);
    assert_eq!(outgoing.session_ended().unwrap(), None);
    assert_eq!(agent.teardown().failures, [] as [String; 0]);
    assert_eq!(outgoing.session_ended().unwrap(), Some(143));
    assert_eq!(agent.teardown().failures, [] as [String; 0]);
}

#[test]
fn clear_refuses_an_adopted_engine_before_dismissing_its_helper() {
    let parent = Agent::for_test("system").unwrap();
    let mut child = forked_child(&parent, "adopted", "`inherit");
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = child.ral(
        &DurableRoot::default().child(),
        r#"let keep = 1
exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep this pin"]]]]"#,
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    let grandchild = child.fork_named("grandchild").unwrap();
    let helper = launch_child(&mut child, grandchild, None, &emit);
    child.seed("keep queued".into());
    crate::agent::testkit::schedule(
        &mut child,
        crate::schedule::Trigger::After(Duration::from_hours(1)),
        "ping".into(),
        "ping".into(),
    );
    let before = serde_json::to_vec(&child.rendered_messages()).unwrap();
    assert!(child.clear().unwrap_err().contains("new conversation"));
    assert!(helper.is_live());
    assert!(child.rest().pending);
    assert_eq!(child.handle().queued(), ["keep queued"]);
    assert_eq!(crate::agent::testkit::schedules(&child).len(), 1);
    assert_eq!(child.body.pins.len(), 1);
    assert!(scope_has(&mut child, "keep"));
    assert_eq!(
        serde_json::to_vec(&child.rendered_messages()).unwrap(),
        before
    );
}

#[test]
fn a_ring_answered_after_clear_posts_no_wakeup() {
    let mut agent = Agent::for_test("system").unwrap();
    crate::agent::testkit::schedule(
        &mut agent,
        crate::schedule::Trigger::After(Duration::from_hours(1)),
        "old".into(),
        "old".into(),
    );
    agent.body.schedules.ring("old", &agent.mailbox.address());
    agent.clear().unwrap();
    let commits = |agent: &Agent| {
        let (_, commits) = agent
            .body
            .log
            .record_emitter()
            .reader()
            .since(None)
            .unwrap();
        commits.map(Result::unwrap).count()
    };
    let before = commits(&agent);
    agent.fire_schedules().unwrap();
    assert!(agent.mailbox.drain().is_empty());
    assert_eq!(commits(&agent), before);
}

#[test]
fn clear_appends_to_the_one_record_and_shared_emitters_keep_publishing() {
    let mut agent = Agent::for_test("system").unwrap();
    let record = agent.log_dir().path().join(RECORD);
    let (tx, rx) = crate::bus::channel();
    agent.couple(&Emitter::new(tx, agent.id()));
    let recorder = agent.recorder();
    agent.clear().unwrap();
    assert!(!record.with_file_name("record.jsonl.0").exists());
    drop(
        recorder
            .emit(Forensic::SystemNote {
                text: "after the clear".into(),
            })
            .unwrap(),
    );
    let after = |record: &Record| matches!(record, Record::Forensic(Forensic::SystemNote { text }) if text == "after the clear");
    let records = crate::record::read_records(&record).unwrap();
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::Transcript(Transcript::Cleared)))
    );
    assert!(records.iter().any(after));
    let published = crate::bus::drain_records(&rx);
    assert!(
        published
            .iter()
            .any(|record| matches!(record, Record::Transcript(Transcript::Cleared)))
    );
    assert!(published.iter().any(after));
}

#[test]
fn rewind_validates_the_anchor_and_rearms_nudge_edges() {
    let mut agent = Agent::for_test("system").unwrap();
    for (prompt, answer) in [
        ("one", "answer one"),
        ("two", "answer two"),
        ("three", "answer three"),
    ] {
        close_exchange(&mut agent, prompt, answer);
    }
    let facts = crate::agent::nudge::Facts {
        must_reply: false,
        pinned: Some("pin".into()),
        quiet: true,
    };
    let react = |agent: &mut Agent| {
        let body = &mut *agent.body;
        body.readings.nudges.react(
            Ok(&crate::agent::outcome::Outcome::Complete),
            &facts,
            &body.log,
        )
    };
    assert!(react(&mut agent).is_some());
    assert!(react(&mut agent).is_none());
    let (emit, rx) = crate::bus::dummy_emitter();
    assert_eq!(
        agent.rewind(9, &emit).unwrap_err(),
        "turn 9 is not recorded: the latest is 6"
    );
    agent.rewind(3, &emit).unwrap();
    assert_eq!(
        agent
            .body
            .log
            .context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.span.from())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(react(&mut agent).is_some());
    assert!(crate::bus::drain_records(&rx).iter().any(|record| matches!(
        record,
        Record::Transcript(Transcript::Rewound { anchor: 3 })
    )));
}

struct ReleaseOnDrop(Arc<AtomicBool>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn held_trunk(release: &Arc<AtomicBool>) -> Agent {
    let release = release.clone();
    let held: Arc<[BuiltinEntry]> = Arc::new([BuiltinEntry::new(
        Cow::Borrowed("test-held"),
        scheme_test_clear_block_forever,
        "test-only: ignore cancellation until released.",
        BuiltinBody::Captured(Arc::new(move |_, _, shell| {
            loop {
                if release.load(Ordering::Acquire) {
                    shell.poll()?;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })),
    )]);
    dressed_trunk_over(
        move |runtime| runtime.install_captured_builtins(&held),
        |shell| shell.set_worker_drain_grace(Duration::from_millis(20)),
    )
}

#[test]
fn clear_cancels_workers_and_records_the_outgoing_engines_failure_once() {
    let release = Arc::new(AtomicBool::new(false));
    let _release = ReleaseOnDrop(release.clone());
    let mut agent = held_trunk(&release);
    let record = agent.log_dir().path().join(RECORD);
    let _fleet = agent.fleet.clone();
    let outgoing = identity(&agent);
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = agent.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"held\" { test-held }",
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    let entries = workers(&agent);
    assert_eq!(entries.len(), 1);
    agent.clear().unwrap();
    assert_eq!(outgoing.session_ended().unwrap(), Some(143));
    assert!(entries[0].handle.cancel.is_cancelled());
    assert_eq!(probe_count(&agent, ral_core::test_access::worker_count), 0);
    let failures = agent.teardown().failures;
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("ended without certified engine teardown"));
    assert!(
        agent.teardown().failures.is_empty(),
        "teardown does not retire an engine twice"
    );
    drop(agent);
    let recorded = crate::record::read_records(&record)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record {
            Record::Forensic(Forensic::TeardownFailed { failure }) => Some(failure),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded, failures,
        "a clean final engine never loses or duplicates the outgoing failure"
    );
    release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !entries[0].handle.sample().has_ended() {
        assert!(Instant::now() < deadline, "released worker did not settle");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn clear_preserves_human_input_during_preparation_and_outgoing_teardown() {
    use std::sync::{Mutex, mpsc};
    let (started, ready) = mpsc::channel();
    let (stopping, teardown) = mpsc::channel();
    let (release_old, finish_old) = mpsc::channel();
    let finish_old = Mutex::new(finish_old);
    let held: Arc<[BuiltinEntry]> = Arc::new([BuiltinEntry::new(
        Cow::Borrowed("test-held"),
        scheme_test_clear_block_forever,
        "test-only: pause during outgoing teardown.",
        BuiltinBody::Captured(Arc::new(move |_, _, shell| {
            started.send(()).unwrap();
            loop {
                if let Err(stopped) = shell.poll() {
                    stopping.send(()).unwrap();
                    finish_old
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release outgoing teardown");
                    return Err(stopped);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })),
    )]);
    let mut session = dressed_trunk_over(
        move |runtime| runtime.install_captured_builtins(&held),
        |shell| shell.set_worker_drain_grace(Duration::from_secs(15)),
    );
    close_exchange(&mut session, "old context", "old answer");
    let (emit, rx) = crate::bus::dummy_emitter();
    let result = session.ral(
        &DurableRoot::default().child(),
        "let held = spawn \"held\" { test-held }",
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    session.seed("discard this old tail".into());
    let outgoing = identity(&session);
    let old_generation = session.generation;
    let (preparing, preparation) = mpsc::channel();
    let (release_boot, finish_boot) = mpsc::channel();
    let _ = dressed(move |_| {
        preparing.send(()).unwrap();
        finish_boot
            .recv_timeout(Duration::from_secs(10))
            .expect("release prepared boot");
    });
    let handle = session.handle();
    std::thread::scope(|scope| {
        let poster = scope.spawn(move || {
            preparation.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(outgoing.session_ended().unwrap(), None);
            assert!(handle.steer("during boot".into()).is_ok());
            assert!(handle.steer("another fragment".into()).is_ok());
            assert_eq!(
                handle.queued(),
                ["discard this old tail\nduring boot\nanother fragment"]
            );
            release_boot.send(()).unwrap();
            teardown.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                handle.is_live(),
                "outgoing teardown must not end the new engine's life"
            );
            handle
                .0
                .address
                .post(Input::Surface {
                    generation: old_generation,
                    values: vec![FOValue::Unit],
                })
                .unwrap();
            assert!(handle.steer("during teardown".into()).is_ok());
            release_old.send(()).unwrap();
        });
        session.clear().unwrap();
        poster.join().unwrap();
    });
    let prompt = "discard this old tail\nduring boot\nanother fragment\nduring teardown";
    assert_eq!(session.handle().queued(), [prompt]);
    assert!(session.rendered_messages().is_empty());
    assert_eq!(
        crate::bus::drain_records(&rx)
            .iter()
            .filter(|r| matches!(
                r,
                crate::record::Record::Transcript(crate::record::Transcript::Cleared)
            ))
            .count(),
        1
    );
    assert!(
        session
            .mailbox
            .drain()
            .into_iter()
            .all(|drained| matches!(drained, Drained::Input(Input::Human(_))))
    );
}

#[test]
fn clear_resets_pins_schedules_and_readings_without_resetting_the_address() {
    let mut agent = Agent::for_test("system").unwrap();
    let handle = agent.handle();
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = agent.ral(
        &DurableRoot::default().child(),
        r#"exarch-pins `set [key: "goal", body: `text [spans: [[text: "old goal"]]]]"#,
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    crate::agent::testkit::schedule(
        &mut agent,
        crate::schedule::Trigger::After(Duration::from_hours(1)),
        "ping".into(),
        "ping".into(),
    );

    agent.body.readings.measure = Some(crate::agent::gauge::Measure { tokens: 17, at: 2 });
    assert!(agent.body.readings.gauges.disk(2, 1).is_some());
    let facts = crate::agent::nudge::Facts {
        must_reply: false,
        pinned: Some("old goal".into()),
        quiet: true,
    };
    {
        let body = &mut *agent.body;
        assert!(
            body.readings
                .nudges
                .react(
                    Ok(&crate::agent::outcome::Outcome::Complete),
                    &facts,
                    &body.log
                )
                .is_some()
        );
    }
    agent.clear().unwrap();
    assert!(handle.is_live());
    assert_eq!(agent.id(), handle.id());
    assert!(agent.body.pins.is_empty());
    assert!(crate::agent::testkit::schedules(&agent).is_empty());

    assert!(agent.body.readings.measure.is_none());
    assert!(agent.body.readings.gauges.disk(2, 1).is_some());
    let body = &mut *agent.body;
    assert!(
        body.readings
            .nudges
            .react(
                Ok(&crate::agent::outcome::Outcome::Complete),
                &facts,
                &body.log
            )
            .is_some()
    );
}

#[test]
fn clear_in_one_root_leaves_another_roots_queued_report_alone() {
    let parent = Agent::for_test("system").unwrap();
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let mut branch =
        crate::agent::spawn::hatch_root(&parent, Memory::Amnemon, Some("branch".into())).unwrap();
    branch.clear().unwrap();
    assert!(helper.is_live());
    helper.report(ChildStatus::Replied(FOValue::Unit)).unwrap();
    let inputs = parent.mailbox.drain();
    assert!(
        matches!(&inputs[..], [Drained::Input(Input::Child { id, status: ChildStatus::Replied(FOValue::Unit), .. })] if *id == helper.id())
    );
}
