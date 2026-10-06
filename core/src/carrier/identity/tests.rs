//! The identity carrier's laws: cancel races, enquiry doors, durability and
//! probes, all through one in-process engine.
use super::*;
use crate::carrier::{Dispatching, dispatch_to_report};
use crate::protocol::{Ending, EnquiryError, Report};
use crate::types::Shell;

mod identity_cancel_tests {
    use super::*;
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
                    crate::protocol::Run::captured("sleep 30", "<test>"),
                    &(Arc::new(()) as Arc<dyn Host>),
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
        race(ControlSender::interrupt);
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

    /// A SIGINT reaches an identity engine only as its host's
    /// `Control::Interrupt`: the run unwinds, the session lives on.
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
// that does — the shape a real enquiring door runs under.
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
mod enquiry_tests {
    use super::*;
    use crate::run::tests::install_act;
    use crate::run::{RunReport, RunRequest};
    use crate::types::{Desk, Mooring};
    use std::sync::Mutex;

    /// No desk installed answers the honest absence error, verbatim.
    #[test]
    fn absent_desk_answers_the_honest_error() {
        let shell = crate::test_helper::core_shell();
        let err = shell
            .enquire(&Mooring::adrift(), FOValue::Unit)
            .expect_err("no desk is installed");
        assert_eq!(err.message, crate::types::NO_DESK);
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
        let mut shell = crate::test_helper::core_shell();
        let answer = std::sync::Arc::new(Mutex::new(None));
        let asked = answer.clone();
        install_act(&mut shell, "ask-desk", move |mooring, shell| {
            *asked.lock().unwrap() = Some(shell.enquire(mooring, FOValue::Int { value: 41 }));
        });
        let desk: Desk = Arc::new(IdentityDesk {
            host: Arc::new(IncrementSeam),
            events: no_events(),
        });

        match shell.run(RunRequest {
            desk: Some(desk),
            ..RunRequest::from(Run::captured("ask-desk", "<test>"))
        }) {
            RunReport::Ran { .. } => {}
            RunReport::Static { .. } => panic!("`ask-desk` must reach evaluation"),
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

    /// The same guard on the other door.
    struct ReentrantDispatchSeam(std::sync::Arc<IdentityTransport>);
    impl Host for ReentrantDispatchSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            let _ = dispatch_to_report(
                &*self.0,
                crate::protocol::Run::captured("", "<test>"),
                Arc::new(()),
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

// ── Run-door durability, seen through the protocol ────────────────────
mod durability_tests {
    use super::*;

    fn builtin_panic_now(
        _args: &[crate::types::Value],
        _mooring: &crate::types::Mooring,
        _shell: &mut Shell,
    ) -> crate::types::Settled<crate::types::Value> {
        panic!("transport test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::ty::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit)))
    }

    static PANIC_BUILTINS_ARR: [crate::types::BuiltinEntry; 1] = [crate::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("protocol-panic-now"),
        scheme_panic_now,
        "test-only: panic the evaluator mid-run.",
        crate::types::BuiltinBody::Static(builtin_panic_now),
    )];

    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
    )]
    fn panicky(_attach: &Attach) -> Result<crate::engine::Booted, String> {
        let mut shell = crate::test_helper::core_shell();
        shell.install_builtins(&PANIC_BUILTINS_ARR);
        Ok(crate::engine::Booted {
            shell,
            keep: Box::new(()),
        })
    }

    static PANICKY: [EngineInstaller; 1] = [crate::carrier::testkit::installer("panicky", panicky)];

    /// A panicking run arrives as an ordinary `Report::Static{Host}` — the run
    /// door caught it and rolled the shell back — and the same transport
    /// dispatches the next run cleanly.
    #[test]
    fn panicking_dispatch_reports_and_the_session_survives() {
        let transport = crate::carrier::testkit::boot(&PANICKY);

        let report = crate::carrier::testkit::eval(&transport, "protocol-panic-now");
        match report {
            Report::Static { rendered, .. } => {
                assert!(rendered.contains("run panicked"), "got {rendered:?}");
            }
            other @ Report::Ran { .. } => {
                panic!("a panicking run must report Static Host, got {other:?}")
            }
        }

        let report = crate::carrier::testkit::eval(&transport, "$[1 + 1]");
        match report {
            Report::Ran { ending, .. } => assert!(matches!(ending, Ending::Settled { .. })),
            Report::Static { .. } => panic!("the healed session must evaluate"),
        }
    }
}

// ── Probe tests ───────────────────────────────────────────────────────
//
// Identity-only. Engine-busy serialisation needs a second process racing the
// wire engine's rendezvous, which no unit test can stage.
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod probe_tests {
    use super::*;
    use crate::carrier::testkit::{BARE, attach, boot, boot_at, eval};
    use crate::protocol::probe::PathEntry;

    fn fresh() -> IdentityTransport {
        boot(&BARE)
    }

    /// A bare engine seated at `dir`.
    fn at(dir: &std::path::Path) -> IdentityTransport {
        boot_at(
            &BARE,
            &Attach {
                cwd: dir.to_path_buf(),
                ..attach(&BARE)
            },
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
        assert!(
            read::<u64>(&transport, &Probe::GrantDepth).is_ok_and(|depth| depth >= 1),
            "a fresh shell still carries the ambient root frame"
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
        let mut attach = attach(&BARE);
        attach.env.push(("PROBE_TEST_VAR".into(), "42".into()));
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
            Run::captured("$[1 + 1]", "<test>"),
            Arc::new(()),
        );
        assert_eq!(refused, Err(first));
    }

    /// A parked fork is adopted once, and narrowed under the parent's own
    /// installer — which, for a test engine, has no base lexicon to narrow by.
    #[test]
    fn a_parked_fork_is_adopted_once_under_the_parent_installer() {
        let transport = fresh();
        let park = || transport.nursery.park(crate::test_helper::core_shell());
        let id = park();
        assert!(transport.adopt_parked(id, &SpawnGrant::Inherit).is_ok());
        let again = transport
            .adopt_parked(id, &SpawnGrant::Inherit)
            .err()
            .expect("a fork adopts once");
        assert!(again.contains("no forked session is parked"), "{again}");
        let based = transport
            .adopt_parked(park(), &SpawnGrant::Base("confined".into()))
            .err()
            .expect("a bare engine has no base lexicon");
        assert!(based.contains("`confined`"), "{based}");
    }
}
