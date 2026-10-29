use super::*;
use crate::agent::Trunk;
use crate::agent::engine::EngineKind;
use crate::agent::nursery::{Joined, Teardown};
use crate::agent::spawn::hatch_root;
use crate::agent::testkit::{self, ral_call, scripted, set_provider, trunk};
use crate::agent::{Admission, TestTrunk};
use crate::bus::Input;
use crate::bus::{Command, Read, Rewrite, Signal, WORKER_PANIC_PREFIX};
use crate::provider::scripted::{Reply, Script};
use crate::record::{AgentId, ChildStatus, Forensic, Memory, Protocol, Record, Transient, Voice};
use ral_core::test_helper::eventually;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn roots() -> Vec<(Agent, Option<Resumed>)> {
    let first = trunk(Trunk::Converses);
    let second = hatch_root(&first, Memory::Amnemon, Some("second".into()))
        .expect("another root in the same fleet");
    vec![(first, None), (second, None)]
}

#[test]
fn dropping_a_host_joins_every_root_without_end() {
    let roots = roots();
    let fleet = roots[0].0.fleet.clone();
    let engines: Vec<_> = roots
        .iter()
        .map(|(root, _)| match root.engine().kind() {
            EngineKind::Identity(transport) => Arc::downgrade(&transport),
            EngineKind::Wire { .. } => panic!("test roots use identity engines"),
        })
        .collect();
    let host = Host::adopt(roots).expect("launch both roots");
    let handles = host.roots().to_vec();

    drop(host);

    assert!(handles.iter().all(|handle| !handle.is_live()));
    assert!(fleet.is_empty(), "drop waits for every name to leave");
    assert!(fleet.is_finished(), "drop waits for terminal publication");
    assert!(engines.iter().all(|engine| engine.upgrade().is_none()));
    assert!(fleet.join_roots().is_empty(), "drop consumes every join");
}

#[test]
fn end_joins_initial_roots_and_a_root_opened_by_a_command() {
    let host = Host::adopt(roots()).expect("launch both roots");
    let fleet = host.fleet().clone();
    host.roots()[0].post_command(Command::Read(Read::Hatch {
        memory: Memory::Amnemon,
        name: Some("third".into()),
    }));
    let deadline = Instant::now() + WAIT;
    loop {
        if let Signal::Born(_, born) = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the command launches a third root")
            && born.name == "third"
        {
            break;
        }
    }
    let handles = fleet.live();
    assert_eq!(handles.len(), 3);

    let failures = host.end();

    assert!(failures.is_empty(), "{failures:?}");
    assert!(handles.iter().all(|handle| !handle.is_live()));
    assert!(fleet.is_empty());
    assert!(fleet.is_finished());
    assert!(fleet.join_roots().is_empty(), "end consumes every join");
}

#[test]
fn completion_waits_for_terminal_publication_and_preserves_join_failures() {
    let fleet = Fleet::for_test();
    let bus = FleetBus::session();
    let id = AgentId::new(0);
    let emit = bus.emitter(id);
    let (release, gate) = mpsc::channel();
    fleet.adopt_root(Joined::new(std::thread::spawn(move || {
        gate.recv_timeout(WAIT)
            .expect("release terminal publication");
        emit.fleet_sink().transient(Transient::Fault {
            text: "terminal fault".into(),
        });
        emit.fleet_sink()
            .transient(Transient::Died { failures: vec![] });
        Teardown {
            failures: vec!["uncertified teardown".into()],
        }
    })));
    let host = Host {
        roots: Vec::new(),
        bus,
        signals: None,
        fleet,
    };

    assert!(host.fleet().is_empty());
    assert!(
        !host.is_finished(),
        "an empty directory cannot overtake terminal publication"
    );
    release.send(()).expect("release the retained root thread");
    assert!(eventually(WAIT, || host.is_finished().then_some(())).is_some());
    let signals: Vec<_> = std::iter::from_fn(|| host.feed().try_recv().ok()).collect();
    assert!(matches!(signals.as_slice(), [
        Signal::Transient(fault_id, Transient::Fault { text }),
        Signal::Transient(died_id, Transient::Died { .. }),
    ] if *fault_id == id && *died_id == id && text == "terminal fault"));
    assert_eq!(host.end(), ["uncertified teardown"]);
}

#[test]
fn a_job_settles_on_the_feed_while_a_sender_is_retained() {
    let root = trunk(Trunk::Converses);
    set_provider(
        &root,
        scripted("test-model", Script::new().then(Reply::text("done"))),
    );
    let host = Host::adopt(vec![(root, None)]).expect("launch root");
    let _sender = host.bus.emitter(host.roots()[0].id());
    host.roots()[0].steer("go".into()).unwrap();
    let records = until_settled(&host);
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Display(crate::record::Display::Answer { text, .. }) if text == "done"
    )));
    assert!(
        host.roots()[0].is_live(),
        "settlement does not end the host"
    );
    assert_eq!(host.end(), Vec::<String>::new());
}

#[test]
fn a_recovered_provider_panic_reaches_the_feed_as_a_forensic_error() {
    let root = trunk(Trunk::Converses);
    set_provider(
        &root,
        scripted("test-model", Script::new().then(Reply::panicking())),
    );
    let host = Host::adopt(vec![(root, None)]).expect("launch root");
    host.roots()[0].steer("go".into()).unwrap();
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
                    Record::Forensic(Forensic::JobSettled { .. })
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
fn end_releases_the_factory_before_returning() {
    struct UnusedFactory;
    impl crate::agent::Factory for UnusedFactory {
        fn engine(&self) -> Result<ral_core::protocol::channel::WireStream, String> {
            panic!("a parked identity root needs no helper engine")
        }
    }
    let factory = Arc::new(UnusedFactory);
    let dir = ral_core::own::OwnDir::for_test("host-factory").unwrap();
    let cfg = RootConfig {
        system: "system".into(),
        run_dir: (*dir).clone(),
        trunk: Trunk::Converses,
        admission: crate::agent::Admission::Free,
        helper_idle: crate::agent::fleet::HELPER_IDLE,
        tools: crate::provider::Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel: 0,
        search: false,
        factory: Some(factory.clone()),
        bureau: Arc::new(crate::provider::Bureau::Scripted),
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

    assert_eq!(host.end(), Vec::<String>::new());

    assert!(
        Arc::into_inner(factory).is_some(),
        "the signal forwarder and every root released the factory synchronously"
    );
}

#[test]
fn host_end_returns_a_final_session_end_recording_failure() {
    let root = trunk(Trunk::Converses);
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
        .filter(|record| matches!(record, Record::Forensic(Forensic::JobOpened)))
        .count()
}

#[test]
fn a_scheduled_wakeup_opens_a_job_at_rest_through_the_host() {
    let root = Agent::for_test_with(TestTrunk {
        trunk: Trunk::Converses,
        allow_schedule: true,
        ..TestTrunk::new("system")
    })
    .unwrap();
    set_provider(
        &root,
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
    host.roots()[0].steer("arm a wakeup".into()).unwrap();
    let first = until_settled(&host);
    assert_eq!(job_opened(&first), 1);

    let second = until_settled(&host);

    assert_eq!(
        job_opened(&second),
        1,
        "the wakeup at rest opens its own job"
    );
    assert!(second.iter().any(|record| matches!(record,
        Record::Protocol(Protocol::UserPrompt { voice: Voice::Wakeup, text, .. })
            if text.contains("stir")
    )));
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn clear_through_the_host_ends_helpers_drops_reports_and_keeps_human_text() {
    let mut root = Agent::for_test_with(TestTrunk {
        trunk: Trunk::Converses,
        admission: Admission::Gated,
        ..TestTrunk::new("system")
    })
    .unwrap();
    set_provider(
        &root,
        scripted("test-model", Script::new().then(Reply::text("ok"))),
    );
    let child = testkit::forked_child(&root, "helper", "`inherit");
    let (emit, _unused_feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    root.handle().steer("human before clear".into()).unwrap();
    // Queued behind the Clear, ahead of the report: the gate holds the human text back.
    root.handle().post_command(Command::Rewrite(Rewrite::Clear));
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
    let root = host.roots()[0].clone();
    let id = root.id();
    let cleared = |signal: &Signal| {
        matches!(signal, Signal::Commit(agent, commit) if *agent == id
            && commit.records().iter().any(|record|
                matches!(record.value(), Record::Protocol(Protocol::Cleared))))
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
            Record::Protocol(Protocol::UserPrompt { text, voice, .. }) => {
                Some((text.as_str(), *voice))
            }
            _ => None,
        })
        .collect();
    assert_eq!(prompts, [("human before clear", Voice::Human)]);
    assert_eq!(host.end(), [] as [String; 0]);
}
