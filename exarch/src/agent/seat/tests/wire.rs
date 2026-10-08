use super::super::*;
use crate::agent::desk::{ExarchDesk, HostServices, RunHost, SurfaceApplier};
use crate::agent::fleet::Fleet;
use crate::bus::{Emitter, Inbox};
use crate::record::{AgentId, AgentLog};
use ral_core::carrier::{Host, WireTransport};
use ral_core::protocol::channel::Liveness;
use ral_core::protocol::{EnquiryError, Report};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Each log takes the next session id, as in `agent::desk`'s fixture, so
/// two of them are never the same session to the wire.
fn test_log() -> AgentLog {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    AgentLog::for_test(
        AgentId::new(n),
        "test",
        &crate::record::RecordedAccount::for_test("test"),
    )
    .expect("session log")
}

/// The fd-3 handoff ral-daemon's `spawn` performs, so the host end can be
/// taken with `adopt` — the constructor `Seat::wire`, and so synod, calls.
fn spawn_engine(liveness: Liveness) -> (WireTransport, std::process::Child) {
    let (host, guest) = UnixStream::pair().expect("socketpair");
    let guest_fd = guest.as_raw_fd();
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg(ral_core::Role::Engine.flag());
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    // SAFETY: runs between fork and exec, calling only async-signal-safe
    // `dup2`/`close`, with no allocation and no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(guest_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if guest_fd != 3 {
                libc::close(guest_fd);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn engine child");
    // The child holds it as fd 3 now; a copy kept here would hide the
    // child's death from this end's own EOF.
    drop(guest);
    (
        WireTransport::adopt(host, liveness).expect("adopt host stream"),
        child,
    )
}

/// The trunk a wire desk's handlers scope against — inert: this file's
/// tests never spawn from it.
fn test_trunk(fleet: &Arc<Fleet>) -> Arc<crate::agent::Agent> {
    let mut spec = crate::agent::testkit::TestAgentSpec::new("wire-trunk");
    spec.returns = true;
    crate::agent::testkit::test_agent(fleet, spec).expect("a fresh fleet's trunk")
}

fn wire_seat(liveness: Liveness) -> (Seat, std::process::Child) {
    let (transport, child) = spawn_engine(liveness);
    let dir = std::env::temp_dir();
    (
        Seat::wire(transport, dir.clone(), dir).expect("a freshly spawned engine attaches"),
        child,
    )
}

/// The wire seat's own shape as `Avatar::host_services` builds it: a fresh
/// fleet and its one trunk, no scratch.
fn wire_host_services(emit: &Emitter, parent: &Arc<crate::agent::Agent>) -> HostServices {
    HostServices {
        fleet: Fleet::for_test(),
        kind: SeatKind::Wire {
            cwd: std::env::temp_dir(),
            home: std::env::temp_dir(),
        },
        stamp: parent.mailbox.stamp(),
        agent: parent.clone(),
        emit: emit.clone(),
        reply: crate::agent::ReplyCell::default(),
        log: crate::agent::LogCell::new(test_log()),
        branch: None,
        acts: crate::agent::desk::ActFragment::default(),
        principal: ral_core::host::user(),
    }
}

/// A `Host` that only records the values it is surfaced, for the
/// round-trip below — this run raises no enquiry and forks nothing.
struct SurfaceCollector(std::sync::Mutex<Vec<ral_core::first_order::FOValue>>);

impl Host for SurfaceCollector {
    fn surface(&self, val: &ral_core::first_order::FOValue) {
        self.0.lock().unwrap().push(val.clone());
    }
    fn enquire(
        &self,
        _req: ral_core::first_order::FOValue,
    ) -> Result<ral_core::first_order::FOValue, EnquiryError> {
        unreachable!("this run raises no enquiry")
    }
}

#[test]
fn wire_seat_run_round_trips_and_surfaces_a_value() {
    let (seat, mut child) = wire_seat(Liveness::default());

    let host = Arc::new(SurfaceCollector(std::sync::Mutex::new(Vec::new())));
    let report = ral_core::carrier::dispatch_to_report(
        seat.transport(),
        ral_core::protocol::Run::captured("exarch-surface `ping", "<test>"),
        host.clone() as Arc<dyn Host>,
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
        surfaced.contains(&ral_core::first_order::FOValue::Variant {
            label: "ping".into(),
            payload: None
        }),
        "the live surfaced value must reach on_surface before the Report, got {surfaced:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The binding is production's exactly: `Avatar::ral` hands its
/// desk straight to `shell_eval::run_shell`'s closure, not to the seat.
#[test]
fn wire_seat_enquiry_is_answered_through_the_drain_loop() {
    let (seat, mut child) = wire_seat(Liveness::default());
    let (emit, _rx) = crate::bus::dummy_emitter();
    let fleet = Fleet::for_test();
    let trunk = test_trunk(&fleet);
    let host: Arc<dyn Host> = Arc::new(RunHost {
        desk: ExarchDesk {
            services: wire_host_services(&emit, &trunk),
        },
        apply: SurfaceApplier::new(crate::record::Emitter::none()),
    });

    let report = ral_core::carrier::dispatch_to_report(
        seat.transport(),
        ral_core::protocol::Run::captured("exarch-agents `list", "<test>"),
        host,
    )
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

/// A detached `spawn` worker settles after its spawning dispatch has
/// already reported, so its batch has no dispatch to ride: it crosses on
/// `Frame::Session` and reaches the `InboxDeferred` this seat installs,
/// landing in `inbox` as a `Post::Surface`. Polled with no dispatch in
/// flight, the shape a real front-end sees between tool calls.
#[test]
fn wire_seat_install_deferred_installs_the_sink_for_a_settled_spawn_worker() {
    let (seat, mut child) = wire_seat(Liveness::default());
    let inbox = Inbox::new();
    let (tx, _rx) = crate::bus::channel();
    let fleet = Fleet::for_test();
    let trunk = test_trunk(&fleet);
    let root_id = trunk.id;
    let emit = Emitter::with_mailbox(tx, root_id, inbox.mailbox());

    seat.install_deferred(crate::shell_eval::deferred_sink(&emit));
    let host: Arc<dyn Host> = Arc::new(RunHost {
        desk: ExarchDesk {
            services: wire_host_services(&emit, &trunk),
        },
        apply: SurfaceApplier::new(crate::record::Emitter::none()),
    });

    let report = ral_core::carrier::dispatch_to_report(
        seat.transport(),
        ral_core::protocol::Run::captured("let h = spawn { sleep 1 }", "<test>"),
        host,
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

    // No dispatch is in flight from here on: the worker settles on its own
    // and its batch must still reach `inbox` through the installed sink.
    let deadline = Instant::now() + Duration::from_secs(10);
    let item = loop {
        if let Some(item) = inbox.next_item() {
            break item;
        }
        assert!(
            Instant::now() < deadline,
            "the settled worker's batch never reached the inbox"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    match item {
        crate::bus::Next::Item(crate::bus::Item::Surface { id, values, .. }) => {
            assert_eq!(id, root_id, "stamped with the root session id");
            // Not a singleton: a `sleep 1` worker's batch also carries its
            // own exec-summary record ahead of the completion marker, so
            // this asserts on content rather than length.
            assert!(
                values.iter().any(
                    |v| matches!(v, ral_core::first_order::FOValue::Variant { label, .. } if label == "done")
                ),
                "the batch must carry the worker's completion marker, got {values:?}"
            );
        }
        other => {
            panic!("expected the settled batch to surface as Item::Surface, got {other:?}")
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// `reach().interrupt()` is the per-tab interrupt path.
/// Generous timing throughout: the dev fleet includes a jittery VM.
#[test]
fn wire_reach_interrupt_settles_an_in_flight_run_promptly() {
    const WAIT: Duration = Duration::from_secs(20);
    let (seat, mut child) = wire_seat(Liveness::default());
    let reach = seat.reach();

    let settled = std::thread::scope(|s| {
        let dispatch = s.spawn(|| {
            ral_core::carrier::dispatch_to_report(
                seat.transport(),
                ral_core::protocol::Run::captured("sleep 30", "<test>"),
                Arc::new(()) as Arc<dyn Host>,
            )
        });
        // Interrupt only once the engine is genuinely inside the sleep.
        std::thread::sleep(Duration::from_secs(1));
        let started = Instant::now();
        reach.interrupt();
        let report = dispatch.join().expect("dispatch thread");
        (report, started.elapsed())
    });
    let (report, elapsed) = settled;

    assert!(report.is_ok(), "the engine must still answer with a Report");
    assert!(
        elapsed < WAIT,
        "cancel must settle the run well inside {WAIT:?} rather than run `sleep 30` to \
         term, took {elapsed:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A wire seat has no recipe to reboot in place, so `/clear` answers in
/// words rather than panicking. No engine child needed: nothing crosses.
#[test]
fn wire_seat_clear_answers_a_sentence() {
    let (host, _guest) = UnixStream::pair().expect("socketpair");
    let transport = WireTransport::adopt(host, Liveness::default()).expect("adopt");
    let mut seat = Seat::Wire {
        target: InterruptTarget::new(transport.control().clone()),
        transport: Box::new(transport),
        cwd: std::env::temp_dir(),
        home: std::env::temp_dir(),
    };
    let why = seat.clear().expect_err("a wire seat cannot reboot");
    assert!(why.contains("new conversation"), "{why}");
}
