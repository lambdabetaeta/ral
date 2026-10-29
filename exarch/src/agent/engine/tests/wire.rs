use super::super::*;
use crate::agent::Agent;
use crate::agent::desk::{Call, ExarchDesk, RunHost};
use crate::bus::{Admission, AgentMailbox, Drained, Emitter, Input};
use crate::shell_eval::surface::SurfaceApplier;
use crate::test_helper::engine_over_socketpair;
use atomic_refcell::AtomicRefCell;
use ral_core::carrier::{Host, WireTransport};
use ral_core::process::{CancelCause, DurableRoot};
use ral_core::protocol::channel::Liveness;
use ral_core::protocol::{EnquiryError, Report};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wire_engine(liveness: Liveness) -> (Engine, std::process::Child) {
    let (transport, child) = engine_over_socketpair(liveness);
    let (spec, root) = crate::boot::test_attach();
    (
        Engine::wire(transport, ral_core::protocol::Attach::root(spec, root))
            .expect("a freshly spawned engine attaches"),
        child,
    )
}

fn with_wire_host<R>(
    parent: &mut Agent,
    emit: &Emitter,
    run: impl FnOnce(Arc<dyn Host>) -> R,
) -> R {
    let call = Call {
        fleet: parent.fleet.clone(),
        kind: EngineKind::Wire {
            engine: crate::boot::test_attach().0,
        },
        handle: parent.handle(),
        life: parent.mailbox.life(),
        emit: emit.clone(),
        acts: AtomicRefCell::new(Vec::new()),
    };
    let recorder = parent.recorder();
    parent.body.lend(|loan| {
        run(Arc::new(RunHost {
            desk: ExarchDesk {
                loan: loan.clone(),
                call,
            },
            apply: SurfaceApplier::new(recorder),
        }))
    })
}

/// A `Host` that only records the values it is surfaced, for the
/// round-trip below — this run raises no enquiry and forks nothing.
struct SurfaceCollector(std::sync::Mutex<Vec<ral_core::data::FOValue>>);

impl Host for SurfaceCollector {
    fn surface(&self, val: &ral_core::data::FOValue) {
        self.0.lock().unwrap().push(val.clone());
    }
    fn enquire(
        &self,
        _req: ral_core::data::FOValue,
    ) -> Result<ral_core::data::FOValue, EnquiryError> {
        unreachable!("this run raises no enquiry")
    }
}

#[test]
fn wire_engine_run_round_trips_and_surfaces_a_value() {
    let (engine, mut child) = wire_engine(Liveness::default());

    let host = Arc::new(SurfaceCollector(std::sync::Mutex::new(Vec::new())));
    let report = ral_core::carrier::dispatch_to_report(
        engine.transport(),
        ral_core::protocol::Run::captured(
            "exarch-surface `ping",
            ral_core::source::SourceOrigin::named("<test>"),
        ),
        host.clone() as Arc<dyn Host>,
        None,
    )
    .expect("the engine must answer the dispatch with a Report");

    assert!(
        matches!(
            report,
            Report::Ran {
                ending: ral_core::protocol::Ending::Settled { .. },
                ..
            }
        ),
        "`surface \\`ping` must settle to Report::Ran {{ Ok }}, got {report:?}"
    );
    // Core also reports an observation per dispatch on this sink; the
    // kit's own value is the variant among them.
    let surfaced = host.0.lock().unwrap().clone();
    assert!(
        surfaced.contains(&ral_core::data::FOValue::Variant {
            label: "ping".into(),
            payload: None
        }),
        "the live surfaced value must reach on_surface before the Report, got {surfaced:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The binding is production's exactly: `Agent::ral` hands its
/// desk straight to `shell_eval::run_shell`'s closure, not to the engine.
#[test]
fn wire_engine_enquiry_is_answered_through_the_drain_loop() {
    let (engine, mut child) = wire_engine(Liveness::default());
    let (emit, _rx) = crate::bus::dummy_emitter();
    let mut parent = Agent::for_test("system").unwrap();
    let report = with_wire_host(&mut parent, &emit, |host| {
        ral_core::carrier::dispatch_to_report(
            engine.transport(),
            ral_core::protocol::Run::captured(
                "exarch-agents `list",
                ral_core::source::SourceOrigin::named("<test>"),
            ),
            host,
            None,
        )
    })
    .expect("the engine must answer the dispatch with a Report");

    match report {
        Report::Ran {
            ending: ral_core::protocol::Ending::Settled { .. },
            ..
        } => {}
        other => {
            panic!("`exarch-agents `list` must settle through the installed desk, got {other:?}")
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// A deferred worker batch crosses the wire with no dispatch in flight.
#[test]
fn wire_engine_install_deferred_installs_the_sink_for_a_settled_spawn_worker() {
    let (engine, mut child) = wire_engine(Liveness::default());
    let mailbox = AgentMailbox::new(
        DurableRoot::default().child(),
        crate::record::Emitter::none(),
        Admission::Free,
    );
    engine.install(&mailbox.address(), 17);
    let host: Arc<dyn Host> = Arc::new(SurfaceCollector(std::sync::Mutex::new(Vec::new())));

    let report = ral_core::carrier::dispatch_to_report(
        engine.transport(),
        ral_core::protocol::Run::captured(
            "let h = spawn { /bin/sleep 1 }",
            ral_core::source::SourceOrigin::named("<test>"),
        ),
        host,
        None,
    )
    .expect("the engine must answer the dispatch with a Report");
    assert!(
        matches!(
            report,
            Report::Ran {
                ending: ral_core::protocol::Ending::Settled { .. },
                ..
            }
        ),
        "the spawning statement itself must settle to Report::Ran {{ Ok }}, got {report:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let input = loop {
        if let Some(Drained::Input(input)) = mailbox.drain().pop() {
            break input;
        }
        assert!(
            Instant::now() < deadline,
            "the settled worker's batch never reached the mailbox"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let Input::Surface { generation, values } = input else {
        panic!("a deferred surface batch")
    };
    assert_eq!(generation, 17);
    assert!(values.iter().any(|value| matches!(value, ral_core::data::FOValue::Variant { label, .. } if label == "done")),
        "the batch carries the worker's completion marker: {values:?}");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn cancelled_within_settles_an_in_flight_wire_run_promptly() {
    const WAIT: Duration = Duration::from_secs(20);
    let (engine, mut child) = wire_engine(Liveness::default());
    let within = DurableRoot::default().child();
    let host = Arc::new(SurfaceCollector(std::sync::Mutex::new(Vec::new())));

    let settled = std::thread::scope(|s| {
        let dispatch = s.spawn(|| {
            ral_core::carrier::dispatch_to_report(
                engine.transport(),
                ral_core::protocol::Run::captured(
                    "exarch-surface `running\n/bin/sleep 30",
                    ral_core::source::SourceOrigin::named("<test>"),
                ),
                host.clone(),
                Some(&within),
            )
        });
        let deadline = Instant::now() + WAIT;
        let running = ral_core::data::FOValue::Variant {
            label: "running".into(),
            payload: None,
        };
        while !host.0.lock().unwrap().contains(&running) {
            assert!(Instant::now() < deadline, "the wire dispatch never started");
            assert!(
                !dispatch.is_finished(),
                "the run settled before its start marker"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!dispatch.is_finished(), "the run must still be in flight");
        let started = Instant::now();
        within.cancel(CancelCause::Cancelled);
        let report = dispatch.join().expect("dispatch thread");
        (report, started.elapsed())
    });
    let (report, elapsed) = settled;

    assert!(report.is_ok(), "the engine must still answer with a Report");
    assert!(
        matches!(
            report,
            Ok(Report::Ran {
                ending: ral_core::protocol::Ending::Stopped {
                    cause: CancelCause::Cancelled,
                    ..
                },
                ..
            })
        ),
        "the in-flight dispatch must report cancellation: {report:?}"
    );
    assert!(
        elapsed < WAIT,
        "cancel must settle the run well inside {WAIT:?} rather than run `/bin/sleep 30` to \
         term, took {elapsed:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_wire_root_refuses_resolved_grants_at_founding() {
    use crate::agent::{Agent, RootConfig, RootEngine, Trunk};
    use crate::provider::{Bureau, Provider, Toolset};
    use ral_core::authority::policy::{Capabilities, Net};

    let (transport, mut child) = engine_over_socketpair(Liveness::default());
    let (spec, root) = crate::boot::test_attach();
    let grants = ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
        net: Some(Net::Offline),
        ..Capabilities::default()
    })]);
    let run_dir = crate::agent::testkit::tmp("wire-root-grants");
    let result = Agent::root(
        RootConfig {
            system: "system".into(),
            run_dir: (*run_dir).clone(),
            trunk: Trunk::Converses,
            admission: Admission::Free,
            helper_idle: crate::agent::fleet::HELPER_IDLE,
            tools: Toolset::offered(false),
            allow_schedule: false,
            resume_on_reset: false,
            disk_warn_bytes: None,
            fuel: 0,
            search: true,
            factory: None,
            bureau: Arc::new(Bureau::Scripted),
            roles: crate::provider::roles::Roles::default(),
        },
        RootEngine::Wire {
            transport: Box::new(transport),
            attach: ral_core::protocol::Attach::root(spec, root.with_grants(grants)),
        },
        Arc::new(Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new(),
        )),
    );
    let _ = child.kill();
    let _ = child.wait();
    let Err(why) = result else {
        panic!("a wire root's resolved grants must be refused");
    };
    assert_eq!(
        why.to_string(),
        "a wire root's attach carries resolved grants, which name objects on this host, \
         but its engine may run on another: pass the ceiling unresolved in Root::config, \
         for the engine's recipe to resolve where it runs"
    );
}

/// A wire engine has no recipe to reboot in place, so `/clear` answers in
/// words rather than panicking. No engine child needed: nothing crosses.
#[test]
fn wire_engine_clear_answers_a_sentence() {
    let (host, _guest) = UnixStream::pair().expect("socketpair");
    let transport = WireTransport::adopt(host, Liveness::default()).expect("adopt");
    let engine = Engine::Wire {
        transport: Box::new(transport),
        engine: crate::boot::test_attach().0,
    };
    let why = engine
        .prepare_rebirth()
        .err()
        .expect("a wire engine cannot reboot");
    assert!(why.contains("new conversation"), "{why}");
}

/// The engine a parent's seal attaches is a child of that parent: its shell
/// holds what the parent set.
#[test]
fn a_sealed_attach_is_a_child_of_its_parent() {
    let mut parent = crate::boot::test_shell();
    parent.set_env_var("PARENT_MARK", "1");
    let seal = ral_core::seed::seal(&parent.bequeath(ral_core::SpawnGrant::Inherit))
        .expect("a bequest seals");
    let (transport, mut child) = engine_over_socketpair(Liveness::default());
    let engine = Engine::wire(
        transport,
        ral_core::protocol::Attach::child(crate::boot::test_attach().0, seal),
    )
    .expect("an engine attaches with its parent's seal");
    assert_eq!(
        engine
            .read(|t| t.env_var("PARENT_MARK"))
            .expect("the child answers"),
        Some("1".to_string())
    );
    let _ = child.kill();
    let _ = child.wait();
}
