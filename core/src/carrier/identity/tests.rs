//! The identity carrier's laws: cancel races, enquiry hosts, durability and
//! probes, all through one in-process engine.
use super::*;
use crate::carrier::{Dispatching, dispatch_to_report};
use crate::error::Error;
use crate::path::EnginePath;
use crate::process::CancelCause;
use crate::protocol::{Ending, Ran, Report, Root};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::Shell;

#[cfg(unix)]
mod within;

mod identity_cancel_tests {
    use super::*;
    #[cfg(unix)]
    use crate::fact::ExitCode as _;
    use crate::test_helper::SLEEP;
    use std::time::{Duration, Instant};

    /// Dispatch `sleep 30` from another thread while this one holds the
    /// session lock, so `dispatch` parks right after it opens its scope and
    /// before it ever reaches the run; strike the scope with `strike`, then let
    /// the run go. A failure here is the race, not an absence of wiring.
    fn race(strike: impl FnOnce(&ControlSender)) {
        let transport = Arc::new(crate::carrier::testkit::boot(
            &crate::carrier::testkit::BARE,
        ));
        let guard = transport.engine.lock();

        let worker = {
            let transport = transport.clone();
            std::thread::spawn(move || {
                transport.dispatch(
                    DispatchId(1),
                    crate::protocol::Run::captured(
                        format!("{SLEEP} 30"),
                        crate::source::SourceOrigin::named("<test>"),
                    ),
                    &(Arc::new(()) as Arc<dyn Host>),
                    None,
                );
            })
        };

        // `dispatch` opens its scope ahead of the session lock; wait for that
        // rather than guessing at a delay.
        let deadline = Instant::now() + Duration::from_secs(5);
        while transport.scopes.current() != Some(DispatchId(1)) {
            assert!(
                Instant::now() < deadline,
                "dispatch must open its scope before acquiring the session lock"
            );
            std::thread::yield_now();
        }

        strike(transport.control());
        drop(guard);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.join().expect("dispatch must not panic");
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a strike recorded before the run's frame exists must still settle it promptly, \
             not run `sleep 30` to completion"
        );
    }

    /// The run's frame hangs off the scope a strike lands on, even one struck
    /// before the frame exists.
    #[test]
    fn a_cancel_racing_a_fresh_dispatch_is_not_dropped() {
        race(|control| control.cancel(DispatchId(1)));
    }

    #[test]
    fn an_interrupt_racing_a_fresh_dispatch_is_not_dropped() {
        race(|control| control.strike(DispatchId(1), CancelCause::Interrupted));
    }

    /// Dispatch a child under a forwarder, `raise` once the child is running,
    /// and return the run's status and whether the session then ended.
    #[cfg(unix)]
    fn forwarded(raise: fn()) -> (u8, Option<u8>) {
        let _serial = crate::process::cancel::REQUEST_SERIAL.lock();
        let dir = tempfile::tempdir().expect("a temp dir");
        let marker = dir.path().join("running");
        let transport = Arc::new(crate::carrier::testkit::boot(
            &crate::carrier::testkit::BARE,
        ));
        let _signals = transport.control().forward_signals();
        let worker = {
            let transport = transport.clone();
            let src = format!("sh -c 'touch {}; exec sleep 30'", marker.display());
            std::thread::spawn(move || crate::carrier::testkit::eval(&transport, &src))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "the child never touched its marker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        raise();
        let report = worker.join().expect("dispatch must not panic");
        crate::process::cancel::clear_root_request();
        let Report::Ran(Ran { ending, .. }) = report else {
            panic!("the child must reach evaluation, got {report:?}");
        };
        let ended = transport.session_ended().expect("the probe answers");
        (ending.code(), ended)
    }

    /// Forwarding lags the key: an interrupt raised before a dispatch was
    /// sent, heard only once it is in flight, must not unwind it.
    #[cfg(unix)]
    #[test]
    fn an_interrupt_raised_before_a_dispatch_never_reaches_it() {
        let _serial = crate::process::cancel::REQUEST_SERIAL.lock();
        let transport = Arc::new(crate::carrier::testkit::boot(
            &crate::carrier::testkit::BARE,
        ));
        crate::process::request_interrupt();
        let worker = {
            let transport = transport.clone();
            std::thread::spawn(move || {
                crate::carrier::testkit::eval(&transport, &format!("{SLEEP} 1"))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while transport.control().in_flight().is_none() {
            assert!(Instant::now() < deadline, "the dispatch never went out");
            std::thread::yield_now();
        }
        transport.control().hear(crate::process::Ambient::Interrupt);
        let Report::Ran(Ran { ending, .. }) = worker.join().expect("dispatch must not panic")
        else {
            panic!("the run must reach evaluation");
        };
        assert_eq!(ending.code(), 0, "the interrupt was aimed at no dispatch");
    }

    /// A SIGINT reaches an identity engine only as its host's
    /// `Control::Strike(_, Interrupted)`: the run unwinds, the session lives on.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_interrupt_unwinds_the_run_in_flight() {
        assert_eq!(
            forwarded(crate::process::request_interrupt),
            (130, None),
            "an interrupt ends the run, never the session"
        );
    }

    /// Ctrl-`\` arrives as `Control::End(Aborted)`, ending the session with
    /// its own cause, reported 131 rather than a SIGTERM's 143.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_root_abort_ends_the_session() {
        assert_eq!(
            forwarded(|| crate::process::request_root_cancel(crate::process::RootCause::Aborted)),
            (131, Some(131)),
        );
    }

    /// SIGTERM arrives as `Control::End(Terminated)`.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_terminate_ends_the_session() {
        assert_eq!(
            forwarded(|| crate::process::request_root_cancel(
                crate::process::RootCause::Terminated
            )),
            (143, Some(143)),
        );
    }
}

// Core installs no builtin that enquires, so these install a test builtin
// that does — the shape a real enquiring host runs under.
mod enquiry_tests {
    use super::*;
    use crate::shell::run::tests::install_act;
    use crate::shell::run::{RunReport, RunRequest};
    use crate::shell::runtime::{Break, Desk, Mooring};
    use crate::source::SourceOrigin;
    use std::sync::Mutex;

    /// The absent-desk refusal carries its hint.
    #[test]
    fn absent_desk_carries_its_hint() {
        let shell = crate::test_helper::core_shell();
        let Break::Error(err) = shell
            .enquire(&Mooring::adrift(), FOValue::Unit)
            .expect_err("no desk is installed")
        else {
            panic!("an absent desk is an error, not a stop");
        };
        assert_eq!(err.hints(), []);
        assert_eq!(err.reason.advice(), [crate::shell::runtime::NO_DESK_HINT]);
    }

    /// Identity uses exactly the wire's reason/report normalization.
    #[test]
    fn a_host_refusal_reads_as_over_the_wire() {
        use crate::carrier::testkit::{assert_normalized, host_error, reason_families};
        struct Refuser(Error);
        impl Host for Refuser {
            fn surface(&self, _val: &FOValue) {}
            fn enquire(&self, _req: FOValue) -> Result<FOValue, Error> {
                Err(self.0.clone())
            }
        }
        for reason in reason_families() {
            let original = host_error(reason);
            let desk = IdentityDesk {
                host: Arc::new(Refuser(original.clone())),
                events: no_events(),
            };
            let Err(Break::Error(err)) = crate::shell::runtime::EnquiryDesk::enquire(
                &desk,
                FOValue::Unit,
                &crate::process::DurableRoot::default().child(),
            ) else {
                panic!("a refusal is an error, not a stop");
            };
            assert_normalized(&original, &err);
        }
    }

    /// A no-op receiver, for a desk built without a real dispatch behind it.
    fn no_events() -> Arc<EventReceiver> {
        let (_tx, rx) = mpsc::channel();
        Arc::new(EventReceiver::new(rx))
    }

    /// A stub host that maps `Int{n}` to `Int{n+1}`, otherwise echoes.
    struct IncrementSeam;
    impl Host for IncrementSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
            match req {
                FOValue::Int { value } => Ok(FOValue::Int { value: value + 1 }),
                other => Ok(other),
            }
        }
    }

    /// The host's transform reaches `enquire`'s caller, not just `IdentityDesk`.
    #[test]
    fn enquire_round_trips_through_a_stub_desk() {
        let mut runtime = crate::test_helper::core_runtime();
        let answer = std::sync::Arc::new(Mutex::new(None));
        let asked = answer.clone();
        install_act(&mut runtime, "ask-desk", move |mooring, shell| {
            *asked.lock().unwrap() = Some(shell.enquire(mooring, FOValue::Int { value: 41 }));
        });
        let mut shell = crate::Shell::root(runtime.into(), crate::test_helper::process_cwd());
        let desk: Desk = Arc::new(IdentityDesk {
            host: Arc::new(IncrementSeam),
            events: no_events(),
        });

        match shell.run(RunRequest {
            desk: Some(desk),
            ..RunRequest::from(Run::captured("ask-desk", SourceOrigin::named("<test>")))
        }) {
            RunReport::Ran { .. } => {}
            RunReport::Static { .. } => panic!("`ask-desk` must reach evaluation"),
            RunReport::Checked { .. } => panic!("a source run must not report check-only success"),
        }

        let answer = answer
            .lock()
            .unwrap()
            .take()
            .expect("`ask-desk` must enquire");
        match answer {
            Ok(v) => assert_eq!(
                v,
                FOValue::Int { value: 42 },
                "enquire must return the host's transformed answer"
            ),
            Err(e) => panic!("enquire must succeed through the stub desk, got {e:?}"),
        }
    }

    /// A handler reaching back into `probe` would take the session lock its
    /// own stack already holds. Lacking a core builtin that enquires, the test
    /// enters `dispatch_to_report`'s claim by hand and exercises the same guard.
    struct ReentrantProbeSeam(std::sync::Arc<IdentityTransport>);
    impl Host for ReentrantProbeSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
            let _ = self.0.cwd();
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_probe_panics_never_hangs() {
        reenter(|transport| Arc::new(ReentrantProbeSeam(transport)));
    }

    /// Enquire, from inside a dispatch's claim, of the host `seam` builds.
    fn reenter(seam: impl FnOnce(Arc<IdentityTransport>) -> Arc<dyn Host>) {
        let transport = Arc::new(crate::carrier::testkit::boot(
            &crate::carrier::testkit::BARE,
        ));
        let _dispatching = Dispatching::enter(&*transport);
        let shell = crate::test_helper::core_shell();
        let mut mooring = Mooring::adrift();
        mooring.desk = Some(Arc::new(IdentityDesk {
            host: seam(transport),
            events: no_events(),
        }) as Desk);
        let _ = shell.enquire(&mooring, FOValue::Unit);
    }

    /// The same guard on the other path.
    struct ReentrantDispatchSeam(std::sync::Arc<IdentityTransport>);
    impl Host for ReentrantDispatchSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
            let _ = dispatch_to_report(
                &*self.0,
                crate::protocol::Run::captured("", crate::source::SourceOrigin::named("<test>")),
                Arc::new(()),
                None,
            );
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_dispatch_panics_never_hangs() {
        reenter(|transport| Arc::new(ReentrantDispatchSeam(transport)));
    }
}

// ── Run durability, seen through the protocol ────────────────────
mod durability_tests {
    use super::*;

    fn builtin_panic_now(
        _args: &[crate::shell::runtime::Value],
        _mooring: &crate::shell::runtime::Mooring,
        _shell: &mut Shell,
    ) -> crate::shell::runtime::Settled<crate::shell::runtime::Value> {
        panic!("transport test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_: &mut crate::ty::Binders) -> crate::ty::Ty {
        use crate::typecheck::builtins::{pure, thunk};
        thunk(pure(crate::ty::Ty::unit()))
    }

    static PANIC_BUILTINS_ARR: [crate::shell::runtime::BuiltinEntry; 1] =
        [crate::shell::runtime::BuiltinEntry::new(
            std::borrow::Cow::Borrowed("protocol-panic-now"),
            scheme_panic_now,
            "test-only: panic the evaluator mid-run.",
            crate::shell::runtime::BuiltinBody::Static(builtin_panic_now),
        )];

    static PANICKY: [EngineInstaller; 1] = [crate::carrier::testkit::installer(
        "panicky",
        |_| {
            let mut runtime = crate::HostSurface::default()
                .runtime(crate::io::terminal::TerminalState::default())?;
            runtime.install_builtins(&PANIC_BUILTINS_ARR);
            Ok(runtime)
        },
        |_, _| Ok(()),
    )];

    /// A panic is a reported ending; the rolled-back session remains usable.
    #[test]
    fn panicking_dispatch_reports_and_the_session_survives() {
        let transport = crate::carrier::testkit::boot(&PANICKY);

        let report = dispatch_to_report(
            &transport,
            crate::protocol::Run {
                trail: Some(crate::shell::runtime::CapturePolicy::Off),
                ..crate::protocol::Run::captured(
                    "echo before; protocol-panic-now",
                    crate::source::SourceOrigin::named("<test>"),
                )
            },
            Arc::new(()),
            None,
        )
        .expect("a panic still reports");
        assert_eq!(report.code(), crate::fact::PANIC_CODE);
        match report {
            Report::Ran(Ran {
                ending: Ending::Panic(text),
                rendered,
                captured,
                trail,
                ..
            }) => {
                assert!(text.contains("deliberate mid-eval panic"), "{text:?}");
                assert!(rendered.contains("run panicked"), "got {rendered:?}");
                assert!(captured.is_none(), "a panic discards captured bytes");
                assert!(trail.is_empty(), "a panic discards its trail");
            }
            other => panic!("a panicking run must report Ran/Panic, got {other:?}"),
        }

        let report = crate::carrier::testkit::eval(&transport, "$[1 + 1]");
        match report {
            Report::Ran(Ran { ending, .. }) => assert!(matches!(ending, Ending::Ok(_))),
            Report::Checked { .. } => panic!("a source run must not report check-only success"),
            Report::Static { .. } | Report::Faulted { .. } => {
                panic!("the healed session must evaluate")
            }
        }
    }
}

// ── Teardown certification ───────────────────────────────────────
mod teardown_tests {
    use super::*;
    use crate::carrier::testkit::{BARE, boot};

    /// A held live ticket stands for a worker that ignores cancellation past
    /// the drain grace.
    #[test]
    fn identity_end_reports_workers_that_outlive_teardown() {
        let transport = boot(&BARE);
        let ticket = {
            let mut engine = transport.engine.lock();
            engine
                .shell
                .set_worker_drain_grace(std::time::Duration::from_millis(20));
            engine.shell.local.workers.live_ticket()
        };
        for _ in 0..2 {
            let Err(Severed::Faulted(cause)) = transport.end() else {
                panic!("a worker outliving teardown must fault the end");
            };
            assert_eq!(
                cause,
                crate::fact::ConnectionFault::WorkersSurvived(1u64.into())
            );
        }
        drop(ticket);
        assert_eq!(transport.end(), Ok(()));
    }
}

// ── Probe tests ───────────────────────────────────────────────────────
//
// Identity-only. Engine-busy serialisation needs a second process racing the
// wire engine's rendezvous, which no unit test can stage.
mod probe_tests {
    use super::*;
    use crate::carrier::testkit::{BARE, attach, boot, boot_at, eval};
    use crate::environ::EnvVars;
    use crate::protocol::probe::PathEntry;
    use crate::source::SourceOrigin;

    fn fresh() -> IdentityTransport {
        boot(&BARE)
    }

    /// A bare engine seated at `dir`.
    fn at(dir: &std::path::Path) -> IdentityTransport {
        boot_at(
            &BARE,
            &Attach::root(
                attach(&BARE).engine,
                Root::new(EnginePath::Host(dir.to_path_buf())),
            ),
        )
    }

    /// The depth probe counts every layer, the ambient root's included, so a
    /// frame left behind shows even where it restricts nothing.
    #[cfg(feature = "test-util")]
    #[test]
    fn the_grant_depth_probe_counts_the_root_layer_and_every_frame() {
        use crate::authority::policy::GrantLayer;
        use crate::carrier::read;
        use crate::data::datum::Datum;
        use crate::test_access::answer_probe;
        assert_eq!(read::<u64>(&fresh(), &Probe::GrantDepth), Ok(1));
        let mut shell = crate::test_helper::core_shell();
        shell.with_layer(GrantLayer::root(), |sh| {
            assert_eq!(sh.context.grants.len(), 2);
            assert_eq!(answer_probe(sh, &Probe::GrantDepth), 2u64.encode());
        });
        assert_eq!(answer_probe(&shell, &Probe::GrantDepth), 1u64.encode());
    }

    /// A layer that withholds the net and reads one folder.
    fn granted() -> crate::authority::policy::GrantLayer {
        use crate::authority::policy::{Capabilities, FsPolicy, Net};
        use crate::path::FrozenPath;
        use crate::test_helper::Tree;
        crate::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::from_surface(
                    &Tree::nowhere().at("/ral-probe/work"),
                )],
                ..FsPolicy::default()
            }),
            net: Some(Net::Offline),
            ..Capabilities::default()
        })
    }

    /// An engine seated under [`granted`].
    static GRANTED: [crate::engine::EngineInstaller; 1] = [crate::carrier::testkit::installer(
        "granted",
        |_| crate::HostSurface::default().runtime(crate::io::terminal::TerminalState::default()),
        |_, shell| {
            shell.context.grants.push(granted());
            Ok(())
        },
    )];

    /// `Probe::Grants` answers the session's layers as their values, not the
    /// stack.
    #[test]
    fn grants_answer_the_values_of_the_sessions_layers() {
        let transport = boot(&GRANTED);
        let shown = transport.grants().expect("the probe answers");
        assert_eq!(
            shown.layers,
            [granted().value()],
            "the ambient root states nothing"
        );
        assert_eq!(
            shown.withholds_gpu,
            crate::authority::sandbox::WITHHOLDS_GPU
        );
    }

    #[test]
    fn env_var_reads_the_overlay() {
        let attach = Attach::root(
            attach(&BARE).engine,
            Root::new(EnginePath::Host(std::env::temp_dir()))
                .with_env(EnvVars::from_pairs([("PROBE_TEST_VAR", "42")]).unwrap()),
        );
        let transport = boot_at(&BARE, &attach);
        assert_eq!(transport.env_var("PROBE_TEST_VAR_ABSENT"), Ok(None));
        assert_eq!(
            transport.env_var("PROBE_TEST_VAR"),
            Ok(Some("42".to_string()))
        );
    }

    /// Relative to the engine's own cwd, and recursive.
    #[test]
    fn path_bytes_sums_a_tree_under_the_engine_cwd() {
        let dir = tempfile::tempdir().expect("a tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("a subdirectory");
        std::fs::write(dir.path().join("sub/five"), b"12345").expect("a file");
        let transport = at(dir.path());
        assert_eq!(transport.path_bytes(std::path::Path::new(".")), Ok(5));
    }

    /// Ended only once its durable root is cancelled, with that cause's status.
    #[test]
    fn session_ended_reads_the_root_cause() {
        let transport = fresh();
        assert_eq!(transport.session_ended(), Ok(None));
        transport
            .control()
            .end(crate::process::RootCause::Terminated);
        assert_eq!(transport.session_ended(), Ok(Some(143)));
    }

    #[test]
    fn bindings_and_completion_names_read_the_scope() {
        let transport = fresh();
        eval(&transport, "let probe_row = 7");
        let rows = transport.bindings().expect("a reading");
        let row = rows
            .iter()
            .find(|r| r.name == "probe_row")
            .expect("the row");
        assert_eq!((row.preview.as_str(), &row.handle), ("7", &None));
        let names = transport.completion_names().expect("a reading");
        assert!(names.bindings.iter().any(|n| n == "probe_row"));
    }

    /// A bound handle's row says whether its result is still to be claimed.
    #[test]
    fn a_bound_handle_row_shows_it_claimed() {
        let transport = fresh();
        eval(
            &transport,
            "let probe_h = !{spawn { return 1 }}\nawait $probe_h",
        );
        let rows = transport.bindings().expect("a reading");
        let row = rows.iter().find(|r| r.name == "probe_h").expect("the row");
        let handle = row.handle.as_ref().expect("a handle row");
        assert_eq!(handle.state, crate::shell::runtime::Sample::Consumed);
    }

    #[test]
    fn path_entries_lists_under_the_engine_cwd() {
        let dir = tempfile::tempdir().expect("a tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("a subdirectory");
        let transport = at(dir.path());
        assert_eq!(
            transport.path_entries(std::path::Path::new(".")),
            Ok(vec![PathEntry {
                name: "sub".into(),
                dir: true,
                exec: false,
            }])
        );
    }

    /// First cause wins, and a severed identity is as over as a wire.
    #[test]
    fn a_severed_identity_refuses_what_follows() {
        let transport = fresh();
        let first = Severed::Faulted(crate::fact::ConnectionFault::ReadingForDispatch);
        assert_eq!(transport.sever(first.clone()), first);
        assert_eq!(
            transport.sever(Severed::Closed(
                crate::fact::ConnectionClosed::TransportDropped
            )),
            first
        );
        assert_eq!(transport.cwd(), Err(ProbeError::Severed(first.clone())));
        let refused = dispatch_to_report(
            &transport,
            Run::captured("$[1 + 1]", SourceOrigin::named("<test>")),
            Arc::new(()),
            None,
        );
        assert_eq!(refused, Err(first));
    }

    /// A parked fork is adopted once, and narrowed under the parent's own
    /// installer — which, for a test engine, has no base lexicon to narrow by.
    #[test]
    fn a_parked_fork_is_adopted_once_under_the_parent_installer() {
        let transport = fresh();
        let park = |grant| {
            transport
                .nursery
                .park(crate::test_helper::core_shell().bequeath(grant))
        };
        let id = park(SpawnGrant::Inherit);
        assert!(transport.adopt_parked(id).is_ok());
        let again = transport
            .adopt_parked(id)
            .err()
            .expect("a bequest adopts once");
        assert_eq!(
            again.reason,
            crate::fact::Reason::State(Box::new(crate::fact::StateFailure::NurseryMissing(
                id.0.into()
            ),))
        );
        let based = transport
            .adopt_parked(park(SpawnGrant::Base("confined".into())))
            .err()
            .expect("a bare engine has no base lexicon");
        assert_eq!(
            based.reason,
            crate::fact::Reason::State(Box::new(crate::fact::StateFailure::NoChildEngines))
        );
    }
}

mod typed_refusal_reports {
    use super::*;
    use crate::carrier::testkit::{assert_enquiry_report, host_error, reason_families};
    use crate::data::datum::Datum as _;
    use crate::fact::Reason;

    static ASK: [EngineInstaller; 1] = [crate::carrier::testkit::installer(
        "ask",
        crate::carrier::testkit::enquiring_runtime,
        |_, _| Ok(()),
    )];
    static REFUSE: [EngineInstaller; 1] = [crate::carrier::testkit::installer(
        "refuse",
        crate::carrier::testkit::enquiring_runtime,
        |root, _| {
            Err(host_error(
                Reason::decode(&root.config).expect("fixture reason"),
            ))
        },
    )];

    struct Refuser(Error);
    impl Host for Refuser {
        fn surface(&self, _: &FOValue) {}
        fn enquire(&self, _: FOValue) -> Result<FOValue, Error> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn every_identity_attach_refusal_family_has_the_wire_normalization() {
        for reason in reason_families() {
            let mut request = crate::carrier::testkit::attach(&REFUSE);
            let crate::protocol::Origin::Root(root) = &mut request.origin else {
                unreachable!()
            };
            root.config = reason.clone().encode();
            let Err(Severed::Refused(wire)) = IdentityTransport::boot(&REFUSE, &request) else {
                panic!("the installer refuses before evaluation");
            };
            crate::carrier::testkit::assert_normalized(&host_error(reason), &wire.to_error());
        }
    }

    #[test]
    fn every_identity_enquiry_report_claims_engine_provenance_without_reclassifying_the_reason() {
        let transport = crate::carrier::testkit::boot(&ASK);
        for reason in reason_families() {
            let report = dispatch_to_report(
                &transport,
                Run::captured("ask-host", crate::source::SourceOrigin::named("engine.ral")),
                Arc::new(Refuser(host_error(reason.clone()))),
                None,
            )
            .unwrap();
            assert_enquiry_report(&report, &reason);
        }
    }
}
