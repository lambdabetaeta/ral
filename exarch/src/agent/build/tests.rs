use super::*;
use crate::agent::testkit::*;
use crate::bus::{Emitter, Item, Next, ParkMode};
use crate::provider::scripted::Script;
use crate::record::{Memory, RECORD, Voice};
use genai::chat::ChatMessage;
use ral_core::process::DurableRoot;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry};
use std::borrow::Cow;
use std::fs;
use std::sync::atomic::AtomicBool;

fn root_config(run_dir: &OwnDir, fuel: u32) -> RootConfig {
    RootConfig {
        system: "system".into(),
        run_dir: run_dir.clone(),
        trunk: Trunk::Attended,
        tools: Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel,
        search: true,
        factory: None,
        bureau: Arc::new(crate::provider::Bureau::Scripted),
        roles: crate::provider::roles::Roles::default(),
    }
}

fn identity_engine(tag: &str) -> RootEngine {
    RootEngine::Identity {
        scratch: Arc::new(Scratch::for_test(crate::app::EXARCH, tag).expect("scratch")),
        cwd: ral_core::host::cwd().expect("test process has a cwd"),
        grants: ral_core::authority::policy::GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    }
}

#[test]
fn fork_inherits_host_builtins() {
    let names = |session: &Agent| session.engine.read(|t| t.builtin_names()).unwrap();
    let session = Agent::for_test("system").unwrap();
    assert!(names(&session).iter().any(|n| n == "view-text"));
    let child = names(&session.fork().expect("fork child"));
    for name in ["view-text", "grep-files", "edit-hash", "explore-dir"] {
        assert!(
            child.iter().any(|n| n == name),
            "the child inherits `{name}`"
        );
    }
}

#[test]
fn fork_fans_out_without_spending_the_parents_fuel() {
    let parent = Agent::for_test("system").unwrap();
    assert_eq!(parent.handle.0.fuel, SPAWN_FUEL);
    for _ in 0..3 {
        let child = parent.fork().unwrap();
        assert_eq!(child.handle.0.fuel, SPAWN_FUEL - 1);
    }
    assert_eq!(parent.handle.0.fuel, SPAWN_FUEL);
    let mut chain = vec![parent];
    for expected in (0..SPAWN_FUEL).rev() {
        let child = chain.last().unwrap().fork().unwrap();
        assert_eq!(child.handle.0.fuel, expected);
        chain.push(child);
    }
    assert_eq!(chain.last().unwrap().handle.0.fuel, 0);
}

#[test]
fn fork_inherits_its_parents_search_reach() {
    let parent = Agent::for_test("system").unwrap();
    assert!(parent.fork().unwrap().handle.0.search);
    let searchless = searchless_trunk();
    assert!(!searchless.fork().unwrap().handle.0.search);
}

#[test]
fn fork_seeds_its_own_provider_handle() {
    let parent = Agent::for_test("system").unwrap();
    set_provider(&parent, scripted("p-a", Script::new()));
    let child = parent.fork().unwrap();
    assert_eq!(child.handle.current_provider().model(), "p-a");
    set_provider(&parent, scripted("p-b", Script::new()));
    assert_eq!(parent.handle.current_provider().model(), "p-b");
    assert_eq!(child.handle.current_provider().model(), "p-a");
}

#[test]
fn mnemon_root_imports_context_withholds_reply_and_resumes() {
    let parent = trunk(true);
    {
        let mut log = parent.log.borrow_mut();
        log.append_user("what did we learn?".into(), None, Voice::Human)
            .unwrap();
        log.append_assistant(
            ChatMessage::assistant("the invariant matters"),
            vec![],
            None,
        )
        .unwrap();
    }
    let child =
        crate::agent::spawn::hatch_root(&parent, Memory::Mnemon, Some("fork".into())).unwrap();
    let view = serde_json::to_string(&child.rendered_messages()).unwrap();
    assert!(view.contains("what did we learn?"));
    assert!(view.contains("the invariant matters"));
    assert!(view.rfind("the invariant matters") > view.rfind("what did we learn?"));
    assert!(!child.handle.0.role.returns());
    assert!(child.handle.0.role.parent().is_none());
    assert_eq!(
        child.engine.read(|t| t.grants()).unwrap(),
        parent.engine.read(|t| t.grants()).unwrap()
    );
    assert_eq!(child.handle.0.fuel, parent.handle.0.fuel);
    let sessions = parent.fleet.launch.sessions_root.clone();
    let live = AgentLog::live_roots(&sessions).unwrap();
    let names: Vec<_> = live.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["main", "fork"]);
    let id = child.id();
    let before = child.rendered_messages();
    drop(child);
    let resumed = AgentLog::resume(&sessions, id).unwrap();
    assert_eq!(
        serde_json::to_vec(&resumed.context().rendered()).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
}

#[test]
fn a_closed_root_is_not_resumable() {
    let mut session = Agent::for_test("system").unwrap();
    let sessions = session.fleet.launch.sessions_root.clone();
    assert_eq!(AgentLog::live_roots(&sessions).unwrap().len(), 1);
    let (emit, _rx) = crate::bus::dummy_emitter();
    assert!(
        session
            .rewrite(&crate::bus::Rewrite::Close, &emit)
            .is_break()
    );
    assert_eq!(AgentLog::live_roots(&sessions).unwrap(), vec![]);
}

#[test]
fn clear_fences_the_old_incarnation_only_after_successful_preparation() {
    use ral_core::carrier::Transport as _;
    thread_local! {
        static REFUSE_BOOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    static INSTALLERS: [EngineInstaller; 1] = [EngineInstaller {
        tag: crate::shell_eval::builtins::INSTALLER_TAG,
        runtime: |spec| {
            if REFUSE_BOOT.replace(false) {
                Err("test boot refusal".into())
            } else {
                crate::boot::engine_runtime(spec)
            }
        },
        root: crate::boot::engine_root,
        narrow: crate::policy::base_layer,
    }];
    for fails in [false, true] {
        let mut session = Agent::for_test_with(TestTrunk {
            installers: &INSTALLERS,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let crate::agent::engine::EngineKind::Identity(old) = session.engine.kind() else {
            panic!("an identity root");
        };
        let incarnation = session.engine.scope();
        let (emit, _rx) = crate::bus::dummy_emitter();
        let sink = crate::shell_eval::deferred_sink(
            &emit,
            session.inbox.life_mailbox().vouched_by(incarnation.clone()),
        );
        sink.deliver(vec![ral_core::data::FOValue::Unit]);
        session.seed("keep on refusal".into());
        let work = session.inbox.mailbox();
        REFUSE_BOOT.set(fails);
        assert_eq!(session.clear().is_err(), fails);
        assert_eq!(incarnation.is_cancelled(), !fails);
        assert_eq!(incarnation.id() == session.engine.scope().id(), fails);
        assert!(!session.engine.scope().is_cancelled());
        assert_eq!(work.is_current(), fails);
        sink.deliver(vec![ral_core::data::FOValue::Unit]);
        assert_eq!(session.inbox.is_empty(), !fails);
        assert_eq!(old.session_ended().unwrap(), (!fails).then_some(143));
        if fails {
            assert_eq!(session.handle().queued(), ["keep on refusal"]);
            session.clear().unwrap();
            assert!(incarnation.is_cancelled());
            assert!(session.inbox.is_empty());
        }
    }
}

#[test]
fn a_reborn_engine_carries_the_life() {
    use ral_core::carrier::{Severed, Transport as _};
    let mut session = Agent::for_test("system").unwrap();
    let crate::agent::engine::EngineKind::Identity(outgoing) = session.engine.kind() else {
        panic!("an identity root");
    };
    session.clear().unwrap();
    assert!(session.handle().is_live());
    outgoing.sever(Severed::Closed("the outgoing engine".into()));
    assert!(
        session.handle().is_live(),
        "the outgoing engine no longer holds the life"
    );
    session
        .engine
        .transport()
        .sever(Severed::Closed("the reborn engine".into()));
    assert!(!session.handle().is_live());
}

#[test]
fn failed_clear_preserves_the_live_session() {
    use genai::chat::{ContentPart, ToolCall};
    use ral_core::carrier::Transport as _;
    thread_local! {
        static REFUSE_BOOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static BOOTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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
        root: crate::boot::engine_root,
        narrow: crate::policy::base_layer,
    }];
    for failure in ["unready", "unavailable", "boot", "commit"] {
        let mut session = Agent::for_test_with(TestTrunk {
            installers: &INSTALLERS,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let (emit, rx) = crate::bus::dummy_emitter();
        let result = session.ral(
            &DurableRoot::default().child(),
            r#"let keep = 1
spawn "blocker" { test-clear-block-forever }
exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep this pin"]]]]"#,
            5,
            &emit,
        );
        assert!(!result.failed, "{}", result.text);
        let workers = workers(&session);
        assert_eq!(workers.len(), 1);
        let mailbox = session.inbox.mailbox();
        let child = test_agent(
            &session.fleet,
            TestAgentSpec::child("child", &session.handle(), mailbox.clone()),
        )
        .unwrap();
        session
            .schedules
            .schedule(
                crate::schedule::Trigger::After(Duration::from_hours(1)),
                "ping".into(),
                "ping".into(),
                &mailbox,
            )
            .unwrap();
        session.seed("keep queued".into());
        session.readings.measure = Some(crate::agent::gauge::Measure { tokens: 17, at: 2 });
        assert!(session.readings.gauges.disk(2, 1).is_some());
        let facts = crate::agent::nudge::Facts {
            must_reply: false,
            pinned: Some("keep".into()),
            quiet: true,
        };
        assert!(
            session
                .readings
                .nudges
                .react(
                    Ok(&crate::agent::outcome::Outcome::Complete),
                    &facts,
                    &session.log.borrow()
                )
                .is_some()
        );
        {
            let mut log = session.log.borrow_mut();
            log.append_user("keep history".into(), None, Voice::Human)
                .unwrap();
            if failure == "unready" {
                log.append_assistant(
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
        let before = serde_json::to_vec(&session.rendered_messages()).unwrap();
        let incarnation = session.engine.scope();
        let crate::agent::engine::EngineKind::Identity(old) = session.engine.kind() else {
            panic!("an identity root");
        };
        let boots = BOOTS.get();
        match failure {
            "boot" => REFUSE_BOOT.set(true),
            "commit" => session.recorder().fail_after(1),
            "unavailable" => {
                let recorder = session.recorder();
                recorder.fail_after(1);
                assert!(
                    recorder
                        .emit(crate::record::Forensic::SystemNote {
                            text: "fail".into()
                        })
                        .is_err()
                );
            }
            "unready" => {}
            _ => unreachable!(),
        }
        let why = session.clear().unwrap_err();
        assert_eq!(
            BOOTS.get() - boots,
            u32::from(matches!(failure, "boot" | "commit"))
        );
        assert_ne!(why, "");
        if matches!(failure, "boot" | "unready") {
            assert!(mailbox.is_current(), "{failure}");
            assert!(!incarnation.is_cancelled(), "{failure}");
            assert_eq!(incarnation.id(), session.engine.scope().id());
            assert!(child.is_live());
            assert!(session.handle().has_busy_children());
            assert_eq!(session.handle().queued(), ["keep queued"]);
        } else {
            assert!(!session.handle().is_live(), "{failure}");
        }
        assert_eq!(old.session_ended().unwrap(), None);
        assert!(
            session
                .engine
                .read(|t| t.bindings())
                .unwrap()
                .iter()
                .any(|binding| binding.name == "keep" && binding.preview == "1")
        );
        assert!(!workers[0].handle.cancel.is_cancelled());
        assert_eq!(session.schedules.list().len(), 1);
        assert_eq!(session.pins.lock_ignore_poison().len(), 1);
        assert_eq!(
            session.readings.measure.map(|m| (m.tokens, m.at)),
            Some((17, 2))
        );
        assert!(session.readings.gauges.disk(2, 1).is_none());
        assert!(
            session
                .readings
                .nudges
                .react(
                    Ok(&crate::agent::outcome::Outcome::Complete),
                    &facts,
                    &session.log.borrow()
                )
                .is_none()
        );
        assert_eq!(
            serde_json::to_vec(&session.rendered_messages()).unwrap(),
            before
        );
        assert!(crate::bus::drain_records(&rx).iter().all(|r| !matches!(
            r,
            crate::record::Record::Protocol(crate::record::Protocol::Cleared)
        )));
    }
}

/// A trunk whose `test-held` ignores cancellation until `release`, then
/// settles on it; teardown waits only briefly for it.
fn held_trunk(release: &Arc<AtomicBool>) -> Agent {
    let release = Arc::clone(release);
    let held: Arc<[BuiltinEntry]> = Arc::new([BuiltinEntry::new(
        Cow::Borrowed("test-held"),
        scheme_test_clear_block_forever,
        "test-only: ignore cancellation until released, then settle on it.",
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
fn clear_cancels_registered_workers_and_drops_their_late_surface() {
    let release = Arc::new(AtomicBool::new(false));
    let mut session = held_trunk(&release);
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    for _ in 0..2 {
        session.ral(
            &DurableRoot::default().child(),
            "spawn \"held\" { test-held }",
            30,
            &emit,
        );
    }
    let entries = workers(&session);
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        assert!(!entry.handle.cancel.is_cancelled());
    }
    session.clear().unwrap();
    for entry in &entries {
        assert!(entry.handle.cancel.is_cancelled(), "{}", entry.handle.title);
    }
    assert_eq!(
        probe_count(&session, ral_core::test_access::worker_count),
        0
    );
    release.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    for entry in &entries {
        while !entry.handle.sample().has_ended() {
            assert!(
                std::time::Instant::now() < deadline,
                "cancelled worker did not settle"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    assert!(
        session.inbox.is_empty(),
        "old incarnation batches are refused"
    );
    assert!(
        session.inbox.draw(|_| ParkMode::Quiesce).is_none(),
        "a reboot's late batches must not enter the rebuilt context"
    );
}

#[test]
fn clear_preserves_human_input_during_preparation_and_outgoing_teardown() {
    use crate::provider::scripted::Reply;
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
    close_exchange(&session, "old context", "old answer");
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
    let old_work = session.inbox.mailbox();
    let old_engine = session.engine.scope();
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
            assert!(old_work.is_current());
            assert!(!old_engine.is_cancelled());
            assert!(handle.steer("during boot".into()).is_ok());
            assert!(handle.steer("another fragment".into()).is_ok());
            assert_eq!(
                handle.queued(),
                ["discard this old tail\nduring boot\nanother fragment"]
            );
            release_boot.send(()).unwrap();
            teardown.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(!old_work.is_current());
            assert!(old_engine.is_cancelled());
            assert!(!old_work.post(crate::bus::Scoped::Surface {
                id: handle.id(),
                values: vec![ral_core::data::FOValue::Unit],
            }));
            assert!(handle.steer("during teardown".into()).is_ok());
            release_old.send(()).unwrap();
        });
        session.clear().unwrap();
        poster.join().unwrap();
    });
    let prompt = "during boot\nanother fragment\nduring teardown";
    assert_eq!(session.handle().queued(), [prompt]);
    assert!(session.rendered_messages().is_empty());
    assert_eq!(
        crate::bus::drain_records(&rx)
            .iter()
            .filter(|r| matches!(
                r,
                crate::record::Record::Protocol(crate::record::Protocol::Cleared)
            ))
            .count(),
        1
    );
    set_provider(
        &session,
        scripted(
            "fresh",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "reply",
                "exarch-agents `reply \"fresh answer\"",
            )])),
        ),
    );
    assert!(matches!(
        session.attend(&emit),
        crate::bus::AgentOutcome::Replied
    ));
    let messages = serde_json::to_string(&session.rendered_messages()).unwrap();
    assert!(messages.contains("during boot\\nanother fragment\\nduring teardown"));
    assert!(!messages.contains("old context"));
    assert!(!messages.contains("discard this old tail"));
    assert!(session.is_ready());
}

#[test]
fn late_child_result_after_clear_is_refused_at_publication() {
    let mut session = Agent::for_test("system").unwrap();
    let worker = test_agent(
        &session.fleet,
        TestAgentSpec::child("worker", &session.handle(), session.inbox.mailbox()),
    )
    .unwrap();
    session.clear().unwrap();
    worker.inbox.retire("late".into());
    assert!(
        session.inbox.is_empty(),
        "abandoned work refuses late results"
    );
    assert!(session.inbox.draw(|_| ParkMode::Quiesce).is_none());
}

#[test]
fn a_clear_in_one_tab_leaves_another_tabs_late_result_alone() {
    let trunk = Agent::for_test("system").unwrap();
    let mut branch = Agent::for_test("system").unwrap();
    let worker = test_agent(
        &trunk.fleet,
        TestAgentSpec::child("worker", &trunk.handle(), trunk.inbox.mailbox()),
    )
    .unwrap();
    branch.clear().unwrap();
    worker.inbox.retire("done".into());
    assert!(matches!(
        pop(&trunk.inbox),
        Some(Next::Item(Item::Agent { .. }))
    ));
}

#[test]
fn agent_drop_cancels_its_own_unclosed_workers() {
    let agent = dressed_trunk(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, agent.id());
    agent.ral(
        &DurableRoot::default().child(),
        "spawn \"blocker\" { test-clear-block-forever }",
        30,
        &emit,
    );
    let entries = workers(&agent);
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].handle.cancel.is_cancelled());
    drop(agent);
    assert!(entries[0].handle.cancel.is_cancelled());
}

#[test]
fn a_failed_outgoing_teardown_survives_a_clean_final_one() {
    let release = Arc::new(AtomicBool::new(false));
    let mut session = held_trunk(&release);
    let fleet = session.fleet.clone();
    let witness = test_agent(&fleet, TestAgentSpec::new("witness")).unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        "spawn \"held\" { test-held }",
        30,
        &emit,
    );
    session.clear().unwrap();
    drop(session);
    let why = fleet
        .wait_settled(witness.tenant(), &crate::bus::Pumping::for_test())
        .unwrap_err();
    assert!(
        why.contains("ended without certified engine teardown"),
        "{why}"
    );
    release.store(true, Ordering::Release);
}

#[test]
fn agent_drop_clears_its_own_armed_schedules() {
    let agent = Agent::for_test("system").unwrap();
    let schedules = agent.schedules.clone();
    schedules
        .schedule(
            crate::schedule::Trigger::After(std::time::Duration::from_hours(1)),
            "ping".into(),
            "ping".into(),
            &agent.inbox.mailbox(),
        )
        .unwrap();
    assert_eq!(schedules.list().len(), 1);
    drop(agent);
    assert!(schedules.list().is_empty());
}

#[test]
fn clear_reseals_baseline_and_forgets_ledger() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        "let pre_clear_x = 1",
        5,
        &emit,
    );
    session.clear().unwrap();
    assert!(!scope_has(&session, "pre_clear_x"));
}

#[test]
fn clear_rebinds_service_captures_to_fresh_work() {
    let mut root = trunk(false);
    let (emit, _rx) = crate::bus::dummy_emitter();
    let before = root.host_services(&emit).mailbox;
    assert!(before.is_current());
    root.clear().unwrap();
    assert!(!before.is_current());
    assert!(root.host_services(&emit).mailbox.is_current());
}

#[test]
fn clear_refuses_an_adopted_engine_before_abandoning_work() {
    let parent = Agent::for_test("system").unwrap();
    let mut child = started_child(&parent, "adopted", "`inherit");
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = child.ral(
        &DurableRoot::default().child(),
        r#"let keep = 1
exarch-pins `set [key: "goal", body: `text [spans: [[text: "keep this pin"]]]]"#,
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    assert_eq!(child.pins.lock_ignore_poison().len(), 1);
    let mailbox = child.inbox.mailbox();
    let grandchild = test_agent(
        &child.fleet,
        TestAgentSpec::child("grandchild", &child.handle(), mailbox.clone()),
    )
    .unwrap();
    child.seed("keep queued".into());
    child
        .schedules
        .schedule(
            crate::schedule::Trigger::After(std::time::Duration::from_hours(1)),
            "ping".into(),
            "ping".into(),
            &mailbox,
        )
        .unwrap();
    let before = child.rendered_messages();
    assert_eq!(
        child.engine.prepare_rebirth().err().as_deref(),
        Some(crate::agent::engine::Engine::CANNOT_REBOOT)
    );
    assert!(child.clear().unwrap_err().contains("new conversation"));
    assert!(mailbox.is_current());
    assert_eq!(child.handle().queued(), ["keep queued"]);
    assert_eq!(child.schedules.list().len(), 1);
    assert_eq!(child.pins.lock_ignore_poison().len(), 1);
    assert!(grandchild.is_live());
    assert!(child.handle().has_busy_children());
    assert!(scope_has(&child, "keep"));
    assert_eq!(
        serde_json::to_vec(&child.rendered_messages()).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
}

#[test]
fn rewind_validates_the_anchor_and_sheds_nudges() {
    let mut session = Agent::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        for (prompt, answer) in [
            ("one", "answer one"),
            ("two", "answer two"),
            ("three", "answer three"),
        ] {
            log.append_user(prompt.into(), None, Voice::Human).unwrap();
            log.append_assistant(ChatMessage::assistant(answer), Vec::new(), None)
                .unwrap();
        }
    }
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    assert_eq!(
        session.rewind(9, &emit).unwrap_err(),
        "turn 9 is not recorded: the latest is 6"
    );
    session.seed("open".into());
    open_next(&session.inbox);
    session.inbox.nudge(5, "stale continuation".into());
    session.rewind(3, &emit).unwrap();
    assert_eq!(
        session
            .log
            .borrow()
            .context()
            .context_survey()
            .rows
            .iter()
            .map(|row| row.span.from())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(!matches!(
        pop(&session.inbox),
        Some(Next::Item(Item::Nudge { .. }))
    ));
    assert!(crate::bus::drain_records(&rx).iter().any(|record| matches!(
        record,
        crate::record::Record::Protocol(crate::record::Protocol::Rewound { anchor: 3 })
    )));
}

#[test]
fn fork_child_inherited_scratch_is_baseline() {
    let session = Agent::for_test("system").unwrap();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(
        &DurableRoot::default().child(),
        "let parent_scratch = 1",
        5,
        &emit,
    );
    let child = started_child(&session, "child", "`inherit");
    assert!(scope_has(&child, "parent_scratch"));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    for _ in 0..(crate::shell_eval::BINDING_IDLE_CALLS + 5) {
        child.ral(
            &DurableRoot::default().child(),
            "let _child_spin = 0",
            5,
            &emit,
        );
    }
    assert!(scope_has(&child, "parent_scratch"));
}

#[test]
fn resumed_agent_adds_only_the_fresh_shell_note() {
    let dir = tmp("resume-agent");
    let sessions = dir.make_dir("sessions").unwrap();
    let mut log = AgentLog::root(
        &sessions,
        AgentId::new(0),
        "main",
        &RecordedModel::for_test("old-model"),
        &RecordedAccount::for_test("old-provider"),
        "system",
        None,
    )
    .unwrap();
    log.append_user("before the crash".into(), None, Voice::Human)
        .unwrap();
    log.append_assistant(ChatMessage::assistant("saved answer"), vec![], None)
        .unwrap();
    let before = log.context().rendered();
    drop(log);
    let [(agent, resumed)] = <[_; 1]>::try_from(
        Agent::resume(
            root_config(&dir, 0),
            identity_engine("resume-agent"),
            &scripted("new-model", Script::new()),
        )
        .unwrap(),
    )
    .unwrap_or_else(|roots| panic!("one open session, got {}", roots.len()));
    assert_eq!(resumed.turn, 2);
    assert!(agent.is_ready());
    let messages = agent.rendered_messages();
    assert_eq!(
        serde_json::to_vec(&messages[..before.len()]).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
    assert_eq!(messages.len(), before.len() + 1);
    let note = messages.last().unwrap();
    assert_eq!(note.role, genai::chat::ChatRole::User);
    let text = note.content.first_text().unwrap();
    for loss in [
        "shell is fresh",
        "bindings",
        "workers",
        "cwd",
        "scratch",
        "pinned state",
        "scheduled events",
        "sub-agents",
    ] {
        assert!(text.contains(loss), "resume note must name {loss}: {text}");
    }
    let records = crate::record::read_records(&dir.path().join("sessions/0/record.jsonl")).unwrap();
    assert!(records.iter().any(|record| matches!(
        record,
        crate::record::Record::Forensic(crate::record::Forensic::SessionResumed { .. })
    )));
}

#[test]
fn clear_keeps_human_input_posted_by_its_commit_publication() {
    struct OnClear(Mailbox, Arc<AtomicU64>);
    impl crate::record::Publish for OnClear {
        fn commit(&self, commit: crate::record::Commit) {
            if commit.records().iter().any(|recorded| {
                matches!(
                    recorded.value(),
                    crate::record::Record::Protocol(crate::record::Protocol::Cleared)
                )
            }) {
                assert!(self.0.is_current());
                assert!(self.0.steer("during publication".into()));
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn transient(&self, _transient: crate::record::Transient) {}
    }
    let mut session = Agent::for_test("system").unwrap();
    session.seed("old queued".into());
    let count = Arc::new(AtomicU64::new(0));
    session
        .recorder()
        .attach(Box::new(OnClear(session.inbox.mailbox(), count.clone())));
    session.clear().unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(session.handle().queued(), ["during publication"]);
}

#[test]
fn clear_appends_to_the_one_record_and_shared_emitters_keep_publishing() {
    use crate::record::{Forensic, Protocol, Record};
    let mut session = Agent::for_test("system").unwrap();
    let record = session.log_dir().path().join(RECORD);
    let (tx, rx) = crate::bus::channel();
    session.couple(&Emitter::new(tx, session.id()));
    let recorder = session.recorder();
    session.clear().unwrap();
    assert!(!record.with_file_name("record.jsonl.0").exists());
    let _recorded = recorder
        .emit(Forensic::SystemNote {
            text: "after the clear".into(),
        })
        .unwrap();
    let after = |r: &Record| matches!(r, Record::Forensic(Forensic::SystemNote { text }) if text == "after the clear");
    let records = crate::record::read_records(&record).unwrap();
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Record::Protocol(Protocol::Cleared)))
    );
    assert!(records.iter().any(after));
    let published = crate::bus::drain_records(&rx);
    assert!(
        published
            .iter()
            .any(|r| matches!(r, Record::Protocol(Protocol::Cleared)))
    );
    assert!(published.iter().any(after));
}

#[test]
fn resume_seeds_child_ids_past_existing_session_directories() {
    let dir = tmp("resume-id-seed");
    let sessions = dir.make_dir("sessions").unwrap();
    let log = AgentLog::root(
        &sessions,
        AgentId::new(0),
        "main",
        &RecordedModel::for_test("old-model"),
        &RecordedAccount::for_test("old-provider"),
        "system",
        None,
    )
    .unwrap();
    let sentinel = sessions.path().join("1/sentinel");
    fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    fs::write(&sentinel, b"keep").unwrap();
    drop(log);
    let (root, _) = Agent::resume(
        root_config(&dir, 1),
        identity_engine("resume-id-seed"),
        &scripted("new-model", Script::new()),
    )
    .unwrap()
    .remove(0);
    let child = root.fork().unwrap();
    assert!(child.id().get() >= 2);
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
}

#[test]
fn a_root_runs_under_its_launch_grants() {
    use ral_core::authority::policy::{Capabilities, FsPolicy};
    use ral_core::path::FrozenPath;
    let cwd = ral_core::host::cwd().unwrap();
    let grants = ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
        shell: Some(std::collections::BTreeSet::new()),
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(cwd.as_path()).unwrap()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })]);
    let mut session = Agent::for_test_with(TestTrunk {
        grants,
        ..TestTrunk::new("system")
    })
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("file"), "hidden").unwrap();
    let check = |session: &Agent| {
        assert_eq!(
            session.engine.read(|t| t.path_entries(tmp.path())).unwrap(),
            [] as [ral_core::protocol::probe::PathEntry; 0]
        );
        let (emit, _rx) = crate::bus::dummy_emitter();
        for _ in 0..2 {
            let result = session.ral(&DurableRoot::default().child(), "cd .", 5, &emit);
            assert!(
                result.failed && result.text.contains("denied"),
                "{}",
                result.text
            );
        }
        let result = session.ral(&DurableRoot::default().child(), "$[1 + 1]", 5, &emit);
        assert!(
            !result.failed && result.text.contains("VALUE:\n2"),
            "{}",
            result.text
        );
    };
    check(&session);
    session.clear().unwrap();
    check(&session);
    let root = crate::agent::spawn::hatch_root(&session, Memory::Amnemon, Some("new-root".into()))
        .unwrap();
    check(&root);
}

#[test]
fn a_child_gets_parent_meet_narrowing_without_replay_at_any_depth() {
    use ral_core::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
    use ral_core::path::{FrozenPath, shown};
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::write(&a, "a").unwrap();
    fs::write(&b, "child-readable-b").unwrap();
    let cwd = ral_core::host::cwd().unwrap();
    let here = FrozenPath::freeze(cwd.as_path()).unwrap();
    let a_path = FrozenPath::freeze(&a).unwrap();
    let b_path = FrozenPath::freeze(&b).unwrap();
    let runtime = crate::shell_eval::builtins::host_surface()
        .runtime(ral_core::io::terminal::TerminalState::default())
        .unwrap();
    let explicit_reads =
        ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![here.clone()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        })]);
    for path in [&a, &b] {
        assert!(
            !explicit_reads.admits_fs(
                runtime.baseline(),
                &ral_core::authority::policy::FsOp::Read,
                &ral_core::test_helper::trace(path)
            ),
            "{}",
            shown(path)
        );
    }
    let mut grants = ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
        exec: Some(ExecGrant::of(
            [ExecKey::Path(a_path.clone()), ExecKey::Path(b_path.clone())],
            [],
        )),
        fs: Some(FsPolicy {
            read_prefixes: vec![here.clone()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })]);
    let parent = Agent::for_test_with(TestTrunk {
        grants: grants.clone(),
        ..TestTrunk::new("system")
    })
    .unwrap();
    assert_eq!(
        parent.engine.read(|t| t.grants()).unwrap(),
        grants.summary()
    );
    let c = format!(
        "`restrict [exec: [allow: [`absolute '{}']], fs: [read: [`cwd ., `absolute '{}']]]",
        shown(&a),
        shown(&b)
    );
    let child = started_child(&parent, "child", &c);
    grants.push(ral_core::test_helper::layer(Capabilities {
        exec: Some(ExecGrant::of([ExecKey::Path(a_path)], [])),
        fs: Some(FsPolicy {
            read_prefixes: vec![here, b_path],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    }));
    assert_eq!(child.engine.read(|t| t.grants()).unwrap(), grants.summary());
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = child.ral(
        &DurableRoot::default().child(),
        &format!("view-text '{}' 1 2", shown(&b)),
        5,
        &emit,
    );
    assert!(
        !result.failed && result.text.contains("child-readable-b"),
        "{}",
        result.text
    );
    let grandchild = started_child(&child, "grandchild", "`restrict [shell: []]");
    grants.push(ral_core::test_helper::layer(Capabilities {
        shell: Some(std::collections::BTreeSet::new()),
        ..Capabilities::default()
    }));
    assert_eq!(
        grandchild.engine.read(|t| t.grants()).unwrap(),
        grants.summary()
    );
}
