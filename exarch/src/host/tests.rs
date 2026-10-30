mod resume;

use super::*;
use crate::agent::engine::EngineKind;
use crate::agent::nursery::{Joined, Teardown};
use crate::agent::testkit::human_child;
use crate::agent::testkit::{self, ral_call, scripted, set_provider, trunk};
use crate::agent::{Admission, TestLaunch};
use crate::bus::Input;
use crate::bus::{Command, Read, Rewrite, Signal, WORKER_PANIC_PREFIX};
use crate::provider::scripted::{Reply, Script};
use crate::record::{ChildStatus, Forensic, Memory, Record, Transcript, Voice};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn initial_children() -> Vec<(Agent, Option<Resumed>)> {
    let first = trunk(false);
    let second = human_child(&first, Memory::Amnemon, Some("second".into()))
        .expect("another root in the same fleet");
    vec![(first, None), (second, None)]
}

#[test]
fn dropping_a_host_joins_every_root_without_end() {
    let roots = initial_children();
    let fleet = roots[0].0.fleet.clone();
    let engines: Vec<_> = roots
        .iter()
        .map(|(root, _)| match root.engine().kind() {
            EngineKind::Identity(transport) => Arc::downgrade(&transport),
            EngineKind::Wire { .. } => panic!("test roots use identity engines"),
        })
        .collect();
    let host = Host::adopt(roots).expect("launch both roots");
    let handles: Vec<_> = host
        .children()
        .iter()
        .map(|id| fleet.registry.get(*id).expect("a live root"))
        .collect();

    drop(host);

    assert!(handles.iter().all(|handle| !handle.is_live()));
    assert!(
        fleet.registry.is_empty(),
        "drop waits for every name to leave"
    );
    assert_eq!(Arc::strong_count(&fleet), 1);
    assert!(engines.iter().all(|engine| engine.upgrade().is_none()));
    assert!(fleet.join_children().is_empty(), "drop consumes every join");
}

fn open_unnamed(host: &Host, expected_name: &str) -> AgentHandle {
    let main = host.children()[0];
    host.port()
        .command(
            main,
            Command::Read(Read::Hatch {
                memory: Memory::Amnemon,
                name: None,
            }),
        )
        .unwrap();
    let deadline = Instant::now() + WAIT;
    let mut child = None;
    loop {
        match host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the unnamed child is adopted and acknowledged")
        {
            Signal::Born(id, born) if born.name == expected_name => {
                assert!(born.parent.is_none());
                child = Some(id);
            }
            Signal::Commit(id, commit) if id == main => {
                if let Some(child) = child
                    && commit.records().iter().any(|record| {
                        matches!(record.value(),
                        Record::Forensic(Forensic::SystemNote { text })
                            if *text == format!("session {expected_name} opened (agent {child})"))
                    })
                {
                    return host.fleet().registry.get(child).unwrap();
                }
            }
            _ => {}
        }
    }
}

#[test]
fn unnamed_children_are_adopted_in_order_and_end_joins_all_three() {
    let host = Host::adopt(vec![(trunk(false), None)]).unwrap();
    let fleet = host.fleet().clone();
    let main = fleet.registry.get(host.children()[0]).unwrap();
    assert_eq!(main.0.name, "main");
    let first = open_unnamed(&host, "agent-1");
    assert_eq!(host.children(), [main.id(), first.id()]);
    let second = open_unnamed(&host, "agent-2");
    let children = [main.id(), first.id(), second.id()];
    assert_eq!(host.children(), children);

    host.port()
        .command(first.id(), Command::Rewrite(Rewrite::Close))
        .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if matches!(host.feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the first unnamed child ends"),
            Signal::Transient(id, crate::record::Transient::Died { .. }) if id == first.id())
        {
            break;
        }
    }
    assert!(!first.is_live());
    assert_eq!(host.children(), children, "ended children remain adopted");

    let failures = host.end();

    assert!(failures.is_empty(), "{failures:?}");
    assert!([main, first, second].iter().all(|handle| !handle.is_live()));
    assert!(fleet.registry.is_empty());
    assert_eq!(Arc::strong_count(&fleet), 1);
    assert!(fleet.join_children().is_empty(), "end consumes every join");
}

#[test]
fn an_unnamed_human_child_skips_a_live_helpers_name() {
    let mut main = trunk(false);
    let child = main.fork_named("agent-1").unwrap();
    let (emit, _feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut main, child, None, &emit);
    let host = Host::adopt(vec![(main, None)]).unwrap();
    assert!(helper.is_live());
    let child = open_unnamed(&host, "agent-2");
    assert_eq!(host.children(), [host.children()[0], child.id()]);
    assert!(helper.is_live());
    assert_eq!(host.end(), [] as [String; 0]);
    assert!(!helper.is_live());
}

#[test]
fn end_preserves_join_failures() {
    let fleet = Fleet::for_test();
    let bus = FleetBus::session();
    let (release, gate) = mpsc::channel();
    let owner = testkit::test_agent(&fleet, testkit::TestAgentSpec::new("retained")).unwrap();
    fleet.adopt(
        (*owner).clone(),
        Joined::new(std::thread::spawn(move || {
            drop(owner);
            gate.recv_timeout(WAIT)
                .expect("release the retained root thread");
            Teardown {
                failures: vec!["uncertified teardown".into()],
            }
        })),
    );
    let host = Host {
        bus,
        signals: None,
        fleet: fleet.clone(),
    };

    release.send(()).expect("release the retained root thread");
    assert_eq!(host.end(), ["uncertified teardown"]);
    assert_eq!(Arc::strong_count(&fleet), 1);
    assert!(fleet.registry.is_empty());
}

#[test]
fn a_job_settles_on_the_feed_while_a_sender_is_retained() {
    let mut root = trunk(false);
    set_provider(
        &mut root,
        scripted("test-model", Script::new().then(Reply::text("done"))),
    );
    let host = Host::adopt(vec![(root, None)]).expect("launch root");
    let _sender = host.bus.emitter(host.children()[0]);
    host.port().submit(host.children()[0], "go".into()).unwrap();
    let records = until_settled(&host);
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Display(crate::record::Display::Answer { text, .. }) if text == "done"
    )));
    assert!(
        host.port().is_live(host.children()[0]),
        "settlement does not end the host"
    );
    assert_eq!(host.end(), Vec::<String>::new());
}

#[test]
fn a_recovered_provider_panic_reaches_the_feed_as_a_forensic_error() {
    let mut root = trunk(false);
    set_provider(
        &mut root,
        scripted("test-model", Script::new().then(Reply::panicking())),
    );
    let host = Host::adopt(vec![(root, None)]).expect("launch root");
    host.port().submit(host.children()[0], "go".into()).unwrap();
    let records = until_settled(&host);
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::Forensic(Forensic::Error { text })
                if text.starts_with(WORKER_PANIC_PREFIX) && text.contains("scripted host panic")
        )),
        "the recovered panic must remain visible: {records:?}"
    );
    assert!(
        host.end().is_empty(),
        "a recovered exchange panic is not a join failure"
    );
}

fn until_settled(host: &Host) -> Vec<Record> {
    let deadline = Instant::now() + WAIT;
    let mut records = Vec::new();
    loop {
        let signal = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the root settles its job without disconnecting the bus");
        if let Signal::Commit(_, commit) = signal {
            let settled = commit.records().iter().any(|record| {
                matches!(
                    record.value(),
                    Record::Control(crate::record::Control::JobSettled { .. })
                )
            });
            records.extend(
                commit
                    .into_records()
                    .into_iter()
                    .map(crate::record::Recorded::into_value),
            );
            if settled {
                return records;
            }
        }
    }
}

#[test]
fn a_retained_port_does_not_retain_the_factory_after_end() {
    struct UnusedFactory;
    impl crate::agent::Factory for UnusedFactory {
        fn engine(&self) -> Result<ral_core::protocol::channel::WireStream, String> {
            panic!("a parked identity root needs no helper engine")
        }
    }
    let factory = Arc::new(UnusedFactory);
    let weak_factory = Arc::downgrade(&factory);
    let dir = ral_core::own::OwnDir::for_test("host-factory").unwrap();
    let cfg = LaunchConfig {
        system: "system".into(),
        run_dir: (*dir).clone(),
        returns: false,
        admission: crate::agent::Admission::Free,
        helper_idle: crate::agent::fleet::HELPER_IDLE,
        max_tokens: None,
        tools: crate::provider::Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel: 0,
        search: false,
        factory: Some(factory.clone()),
        bureau: Arc::new(crate::provider::Bureau::scripted([])),
        roles: crate::provider::roles::Roles::default(),
    };
    let engine = RootEngine::Identity {
        scratch: Arc::new(
            crate::app::Scratch::for_test(crate::app::EXARCH, "host-factory").unwrap(),
        ),
        cwd: ral_core::host::cwd().unwrap(),
        grants: ral_core::authority::policy::GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    };
    let host = Host::found(cfg, engine, scripted("test-model", Script::new())).unwrap();
    let port = host.port();
    let root = host.children()[0];

    assert_eq!(host.end(), Vec::<String>::new());
    assert!(port.roster().is_empty());
    assert_eq!(port.queued(root), Err(crate::port::Ended(root)));

    assert!(
        Arc::into_inner(factory).is_some(),
        "the retained port does not hold the launch's factory"
    );
    assert!(weak_factory.upgrade().is_none());
}

#[test]
fn host_end_returns_a_final_session_end_recording_failure() {
    let root = trunk(false);
    let recorder = root.recorder();
    let host = Host::adopt(vec![(root, None)]).unwrap();
    recorder.fail_after(0);
    let failures = host.end();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("injected record write failure"));
}

fn job_opened(records: &[Record]) -> usize {
    records
        .iter()
        .filter(|record| matches!(record, Record::Control(crate::record::Control::JobOpened)))
        .count()
}

#[test]
fn a_scheduled_wakeup_opens_a_job_at_rest_through_the_host() {
    let mut root = Agent::for_test_with(TestLaunch {
        returns: false,
        allow_schedule: true,
        ..TestLaunch::new("system")
    })
    .unwrap();
    set_provider(
        &mut root,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call(
                    "arm",
                    "exarch-schedules `add [trigger: `after '1s', label: 'soon', prompt: #'stir'#]",
                )]))
                .then(Reply::text("armed"))
                .then(Reply::text("awake")),
        ),
    );
    let host = testkit::host(root);
    host.port()
        .submit(host.children()[0], "arm a wakeup".into())
        .unwrap();
    let first = until_settled(&host);
    assert_eq!(job_opened(&first), 1);

    let second = until_settled(&host);

    assert_eq!(
        job_opened(&second),
        1,
        "the wakeup at rest opens its own job"
    );
    assert!(second.iter().any(|record| matches!(record,
        Record::Transcript(Transcript::UserPrompt { voice: Voice::Wakeup, text, .. })
            if text.contains("stir")
    )));
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn clear_through_the_host_ends_helpers_drops_reports_and_keeps_human_text() {
    let mut root = Agent::for_test_with(TestLaunch {
        returns: false,
        admission: Admission::Gated,
        ..TestLaunch::new("system")
    })
    .unwrap();
    set_provider(
        &mut root,
        scripted("test-model", Script::new().then(Reply::text("ok"))),
    );
    let child = testkit::forked_child(&root, "helper", "`inherit");
    let (emit, _unused_feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    root.handle().steer("human before clear".into()).unwrap();
    // Queued behind the Clear, ahead of the report: the gate holds the human text back.
    let _posted = root.handle().command(Command::Rewrite(Rewrite::Clear));
    root.mailbox
        .address()
        .post(Input::Child {
            id: helper.id(),
            name: "helper".into(),
            status: ChildStatus::Stopped,
            elapsed: Duration::ZERO,
        })
        .unwrap();
    let host = testkit::host(root);
    let root = host
        .fleet()
        .registry
        .get(host.children()[0])
        .expect("a live root");
    let id = root.id();
    let cleared = |signal: &Signal| {
        matches!(signal, Signal::Commit(agent, commit) if *agent == id
            && commit.records().iter().any(|record|
                matches!(record.value(), Record::Transcript(Transcript::Cleared))))
    };
    loop {
        let signal = host.feed().recv_timeout(WAIT).expect("the root clears");
        if cleared(&signal) {
            break;
        }
    }

    assert!(!helper.is_live());
    assert_eq!(root.queued(), ["human before clear"]);
    assert!(root.release());
    let records = until_settled(&host);
    let prompts: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Transcript(Transcript::UserPrompt { text, voice, .. }) => {
                Some((text.as_str(), *voice))
            }
            _ => None,
        })
        .collect();
    assert_eq!(prompts, [("human before clear", Voice::Human)]);
    assert_eq!(host.end(), [] as [String; 0]);
}
