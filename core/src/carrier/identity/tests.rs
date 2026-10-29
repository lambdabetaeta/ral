//! The identity carrier's laws: cancel races, enquiry hosts, durability and
//! probes, all through one in-process engine.
use super::*;
use crate::carrier::{Dispatching, dispatch_to_report};
use crate::path::EnginePath;
use crate::process::CancelCause;
use crate::protocol::{Ending, EnquiryError, Report, Root};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::Shell;

#[cfg(unix)]
mod within;

mod identity_cancel_tests {
    use super::*;
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
    fn forwarded(raise: fn()) -> (i32, Option<i32>) {
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
        let Report::Ran { ending, .. } = report else {
            panic!("the child must reach evaluation, got {report:?}");
        };
        let ended = transport.session_ended().expect("the probe answers");
        (ending.status(), ended)
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
        let Report::Ran { ending, .. } = worker.join().expect("dispatch must not panic") else {
            panic!("the run must reach evaluation");
        };
        assert_eq!(ending.status(), 0, "the interrupt was aimed at no dispatch");
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

    /// Ctrl-`\` arrives as `Control::Abort`, ending the session with its own
    /// cause: `RootAbort`, reported 131 rather than a SIGTERM's 143.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_root_abort_ends_the_session() {
        assert_eq!(
            forwarded(|| crate::process::request_root_cancel(crate::process::CancelCause::Aborted)),
            (131, Some(131)),
        );
    }

    /// SIGTERM arrives as `Control::Terminate`.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_terminate_ends_the_session() {
        assert_eq!(
            forwarded(|| crate::process::request_root_cancel(
                crate::process::CancelCause::Terminated
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

    /// No desk installed answers the honest absence error, verbatim.
    #[test]
    fn absent_desk_answers_the_honest_error() {
        let shell = crate::test_helper::core_shell();
        let Break::Error(err) = shell
            .enquire(&Mooring::adrift(), FOValue::Unit)
            .expect_err("no desk is installed")
        else {
            panic!("an absent desk is an error, not a stop");
        };
        assert_eq!(err.message, crate::shell::runtime::NO_DESK);
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
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
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
            RunReport::Faulted { error } => panic!("the run faulted: {}", error.message),
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
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
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
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
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

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::ty::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit)))
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

    /// A panicking run arrives as a `Report::Faulted` — the run entry caught it
    /// and rolled the shell back — and the same transport dispatches the next
    /// run cleanly.
    #[test]
    fn panicking_dispatch_reports_and_the_session_survives() {
        let transport = crate::carrier::testkit::boot(&PANICKY);

        let report = crate::carrier::testkit::eval(&transport, "protocol-panic-now");
        match report {
            Report::Faulted { rendered, status } => {
                assert!(rendered.contains("run panicked"), "got {rendered:?}");
                assert_eq!(status.get(), 101);
            }
            other @ (Report::Static { .. } | Report::Ran { .. }) => {
                panic!("a panicking run must report Faulted, got {other:?}")
            }
        }

        let report = crate::carrier::testkit::eval(&transport, "$[1 + 1]");
        match report {
            Report::Ran { ending, .. } => assert!(matches!(ending, Ending::Settled { .. })),
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
            let Err(Severed::Faulted(msg)) = transport.end() else {
                panic!("a worker outliving teardown must fault the end");
            };
            assert!(msg.starts_with("1 ral worker thread(s)"), "{msg}");
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

    /// A fresh shell's ledger is unarmed and nothing is spawned or bound.
    #[test]
    fn a_fresh_shell_reads_empty() {
        let transport = fresh();
        assert!(transport.binding_count().is_ok());
        assert_eq!(transport.leased_binding_count(), Ok(0));
        assert_eq!(transport.largest_binding_bytes(), Ok(0));
        assert_eq!(transport.workers(), Ok(Vec::new()));
        assert!(transport.cwd().is_ok());
        assert!(transport.home().is_ok());
        assert!(transport.builtin_names().is_ok());
    }

    #[cfg(feature = "test-util")]
    #[test]
    fn the_test_only_probes_answer() {
        use crate::carrier::read;
        let transport = fresh();
        assert_eq!(read::<u64>(&transport, &Probe::WorkerCount), Ok(0));
        assert_eq!(
            transport.grants(),
            Ok(crate::authority::policy::GrantStack::root().summary()),
            "a fresh shell shows the ambient root as nothing"
        );
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

    /// An engine seated under a layer that withholds the net and reads one
    /// folder.
    static GRANTED: [crate::engine::EngineInstaller; 1] = [crate::carrier::testkit::installer(
        "granted",
        |_| crate::HostSurface::default().runtime(crate::io::terminal::TerminalState::default()),
        |_, shell| {
            use crate::authority::policy::{Capabilities, FsPolicy, Net};
            use crate::path::FrozenPath;
            use crate::test_helper::Tree;
            shell
                .context
                .grants
                .push(crate::test_helper::layer(Capabilities {
                    fs: Some(FsPolicy {
                        read_prefixes: vec![FrozenPath::from_surface(
                            &Tree::nowhere().at("/ral-probe/work"),
                        )],
                        ..FsPolicy::default()
                    }),
                    net: Some(Net::Offline),
                    ..Capabilities::default()
                }));
            Ok(())
        },
    )];

    /// `Probe::Grants` answers what the session's stack shows, not the stack.
    #[test]
    fn grants_answer_a_summary_of_the_sessions_stack() {
        use crate::authority::policy::Net;
        let transport = boot(&GRANTED);
        let shown = transport.grants().expect("the probe answers");
        let [layer] = shown.layers() else {
            panic!("the ambient root shows nothing, the one layer does: {shown:?}");
        };
        assert_eq!(layer.net, Some(Net::Offline));
        let read = &layer.fs.as_ref().expect("fs is shown").read;
        assert_eq!(read.len(), 1, "{read:?}");
        assert!(
            std::path::Path::new(&read[0]).ends_with("ral-probe/work"),
            "{read:?}"
        );
    }

    #[test]
    fn largest_binding_bytes_measures_what_is_bound() {
        let transport = fresh();
        eval(&transport, "let probe_sized = 'a dozen bytes or so'");
        assert!(transport.largest_binding_bytes().is_ok_and(|n| n > 0));
    }

    #[test]
    fn env_var_reads_the_overlay() {
        let attach = Attach::root(
            attach(&BARE).engine,
            Root::new(EnginePath::Host(std::env::temp_dir()))
                .with_env(vec![("PROBE_TEST_VAR".into(), "42".into())]),
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
        transport.control().terminate();
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
        let first = Severed::Faulted("the first".into());
        assert_eq!(transport.sever(first.clone()), first);
        assert_eq!(
            transport.sever(Severed::Faulted("the second".into())),
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
        assert!(again.contains("no forked session is parked"), "{again}");
        let based = transport
            .adopt_parked(park(SpawnGrant::Base("confined".into())))
            .err()
            .expect("a bare engine has no base lexicon");
        assert!(based.contains("`confined`"), "{based}");
    }
}
