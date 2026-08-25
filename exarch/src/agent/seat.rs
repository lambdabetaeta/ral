//! The agent's seat: the transport its runs go through, plus whatever
//! host-side state that seat kind owns.  Every engine-side reach is a
//! method here, so a new seat kind is one more variant, not a second agent.

use crate::agent::event::AgentLog;
use crate::bootstrap::Scratch;
use crate::fleet::desk::{AbsentDesk, DeskBinding, HostSeam};
use crate::fleet::registry::{EvalReach, InterruptTarget};
use crate::shell_eval::builtins;
use ral_core::Shell;
use ral_core::transport::{IdentityTransport, Transport};
use std::sync::{Arc, Mutex};

/// One agent's engine-side attachment; what differs per call already lives
/// off the [`Transport`] trait, so this stays a closed enum.
pub(crate) enum Seat {
    /// In-process.  `/clear` rebuilds it, but onto the *same* `interrupt_target`:
    /// the cell the registry interrupts through must outlive the rebuild.
    Identity {
        transport: Box<IdentityTransport>,
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        detach: bool,
        interrupt_target: InterruptTarget,
    },
    /// Out-of-process, one engine per session, holding nothing per call: a
    /// wire run's desk and applier ride `Agent::run_shell`'s arguments into
    /// the drain loop's enquiry arm, the real scratch lives in the guest,
    /// and forks are refused at the desk for fuel 0, so no fork door either.
    Wire {
        transport: Box<ral_core::transport::WireTransport>,
    },
}

/// One `ral` call's capture set, built fresh per call so nothing a desk
/// handler captures can go stale.
pub(crate) struct RunInstall {
    pub(crate) seam: Arc<HostSeam>,
    pub(crate) deferred: Arc<dyn ral_core::types::DeferredSink>,
    pub(crate) fork: ral_core::types::Fork,
}

/// Retires the install on *every* exit, including a panic `Agent::attend`
/// recovers from — straight-line teardown would leave the desk's whole
/// capture installed for the rest of the session.
pub(crate) struct RunGuard<'s>(&'s Seat);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        match self.0 {
            Seat::Identity { transport, .. } => {
                transport.set_desk(Arc::new(AbsentDesk));
                transport.clear_fork();
            }
            Seat::Wire { .. } => {}
        }
    }
}

impl Seat {
    /// Trunk construction, every fork, and the desk's spawn spine all seat
    /// here; [`Self::clear`] re-runs the ceremony onto the same cell.
    pub(crate) fn identity(
        shell: Shell,
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        detach: bool,
        log: &AgentLog,
    ) -> Self {
        let interrupt_target: InterruptTarget = Arc::new(Mutex::new(None));
        let transport = Box::new(identity_ceremony(
            shell,
            log,
            &interrupt_target,
            cwd.clone(),
        ));
        Self::Identity {
            transport,
            scratch,
            cwd,
            detach,
            interrupt_target,
        }
    }

    /// `cwd` and `home` are the caller's word, never read from this
    /// process: under a VM they are guest paths this host cannot resolve.
    pub(crate) fn wire(
        transport: ral_core::transport::WireTransport,
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    ) -> Self {
        transport.attach(
            ral_core::transport::TerminalEndpoint {
                lease: None,
                state: ral_core::io::TerminalState::default(),
            },
            cwd,
            home,
            None,
            builtins::INSTALLER_TAG.to_string(),
        );
        Self::Wire {
            transport: Box::new(transport),
        }
    }

    pub(crate) fn transport(&self) -> &dyn Transport {
        match self {
            Self::Identity { transport, .. } => &**transport,
            Self::Wire { transport, .. } => &**transport,
        }
    }

    /// Identity-only: the test suite's state-inspection door and
    /// `Agent::fork_with`'s [`Shell::fork_session`] reach.
    pub(crate) fn shell_mut(&self) -> std::sync::MutexGuard<'_, ral_core::transport::EngineInner> {
        match self {
            Self::Identity { transport, .. } => transport.shell_mut(),
            Self::Wire { .. } => panic!(
                "direct engine-state access has no meaning on a wire seat: the engine lives in \
                 a separate process, reachable only through Transport's dispatch/probe/control \
                 frames"
            ),
        }
    }

    pub(crate) fn install_run(&self, install: RunInstall) -> RunGuard<'_> {
        match self {
            Self::Identity { transport, .. } => {
                transport.set_deferred_sink(install.deferred);
                transport.set_fork(install.fork);
                // Drain-then-handle: a handler's chrome must never jump
                // ahead of surface output still queued on the channel.
                transport.set_desk(Arc::new(DeskBinding {
                    seam: install.seam,
                    events: transport.events_shared(),
                }));
            }
            Self::Wire { transport } => {
                transport.set_deferred_sink(install.deferred);
            }
        }
        RunGuard(self)
    }

    pub(crate) fn eval_reach(&self) -> EvalReach {
        match self {
            Self::Identity {
                transport,
                interrupt_target,
                ..
            } => EvalReach::Identity {
                eval_root: Some(transport.shell_mut().shell.cancel_handle()),
                interrupt_target: interrupt_target.clone(),
            },
            Self::Wire { transport, .. } => EvalReach::Wire(transport.control().clone()),
        }
    }

    /// `/clear`'s engine half: reboot from the owned scratch onto the
    /// same interrupt target.  Replacing the transport drops the outgoing
    /// shell, whose teardown cancels its workers: `/clear` outranks leases.
    pub(crate) fn clear(&mut self, log: &AgentLog) {
        match self {
            Self::Identity {
                transport,
                scratch,
                cwd,
                detach,
                interrupt_target,
            } => {
                **transport = identity_ceremony(
                    boot_root_shell(scratch, cwd.clone(), *detach),
                    log,
                    interrupt_target,
                    cwd.clone(),
                );
            }
            Self::Wire { .. } => panic!(
                "/clear has no meaning on a wire seat as a transport swap: a wire session clears \
                 by killing its engine process and booting a fresh one from the same recipe, not \
                 by rebuilding this seat in place — a front-end starts over by replacing the \
                 child process, so no caller routes /clear here and reaching this arm is a host \
                 bug"
            ),
        }
    }
}

/// Boot a root session shell; forks instead snapshot their parent through
/// [`Shell::fork_session`], inheriting the seeding.
///
/// `detach` asks whether the verb means anything on this platform at all,
/// which is the host's question, not a capability a `grant` answers per
/// call.  Naming the verb and arming its budget is one act so the two
/// cannot drift, and where `detach` is false the name is simply absent —
/// calling it is an unknown-command diagnostic, not a refusal.
#[cfg_attr(
    not(unix),
    allow(
        unused_variables,
        reason = "detach is born by double-fork, a POSIX act: off unix core publishes no builtin to install"
    )
)]
pub(crate) fn boot_root_shell(scratch: &Scratch, cwd: std::path::PathBuf, detach: bool) -> Shell {
    let mut shell = crate::bootstrap::boot_shell();
    // The trunk owns this process's signals: an Esc or async SIGINT
    // interrupts its in-flight run, a SIGTERM the session. A sub-agent's
    // `fork_session` stays deaf; the registry stops one by cancel handle.
    shell.face_signals();
    shell.seed_cwd(cwd);
    scratch.install_into(&mut shell);
    #[cfg(unix)]
    if detach {
        shell.install_builtins(ral_core::builtins::DETACH_BUILTIN);
        shell.arm_detach(crate::shell_eval::DETACH_BIRTH_BUDGET);
    }
    shell
}

/// Seed first, then arm: the binding ledger exempts whatever is bound when
/// it is armed, so a name seeded afterwards would fall under the lease and
/// be reaped for idleness.  `cwd` is what [`boot_root_shell`] already put
/// on the shell, restated because [`Transport::attach`]'s signature is
/// shared with the wire transport — the one that reads it.
fn identity_ceremony(
    mut shell: Shell,
    log: &AgentLog,
    interrupt_target: &InterruptTarget,
    cwd: std::path::PathBuf,
) -> IdentityTransport {
    // Must point at the live session's event-log directory, on
    // construction and on every `/clear` rebuild alike.
    crate::bootstrap::seed_var(
        &mut shell,
        "EXARCH_SESSION_DIR",
        &log.dir().to_string_lossy(),
    );
    crate::bootstrap::arm_session_ledgers(&mut shell);
    let mut transport = IdentityTransport::new(shell);
    transport.set_interrupt_target(interrupt_target.clone());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    transport.attach(
        ral_core::transport::TerminalEndpoint {
            lease: None,
            state: crate::bootstrap::probe_terminal(),
        },
        cwd,
        std::path::PathBuf::from(&home),
        None, // rc_path
        builtins::INSTALLER_TAG.to_string(),
    );
    transport
}

// These drive a real `--engine` child, never an in-process
// `engine_session` thread: that faces its process's signals, so a
// same-process engine would race the ambient cancel cells against whatever
// sibling test in this lib binary is mid-run, and core's lock over those
// cells is unreachable from here.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::bus::{Emitter, Inbox};
    use crate::fleet::desk::{ExarchDesk, HostServices};
    use crate::fleet::registry::AgentRegistry;
    use crate::provider::{Provider, scripted::Script};
    use ral_core::transport::{EnquiryError, Liveness, Program, Report, Run, WireTransport};
    use ral_core::types::{Capabilities, Nursery};
    use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn source_run(src: &str) -> Run {
        Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        }
    }

    /// Each log takes the next session id, as in `fleet::desk`'s fixture, so
    /// two of them are never the same session to the wire.
    fn test_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        AgentLog::for_test(n, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    /// Hand-rolls `WireTransport::new`'s fd-3 handoff so the host end can
    /// be taken with `adopt` — the constructor `Seat::wire`, and so synod,
    /// actually calls.
    fn spawn_engine(liveness: Liveness) -> (WireTransport, std::process::Child) {
        let (host, guest) = UnixStream::pair().expect("socketpair");
        let guest_fd = guest.as_raw_fd();
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
        cmd.arg("--engine");
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

    fn wire_seat(liveness: Liveness) -> (Seat, std::process::Child) {
        let (transport, child) = spawn_engine(liveness);
        let dir = std::env::temp_dir();
        (Seat::wire(transport, dir.clone(), dir), child)
    }

    /// The wire seat's own shape as `Agent::host_services` builds it:
    /// fresh registry, no scratch.
    fn wire_host_services(emit: &Emitter, registry: &AgentRegistry) -> HostServices {
        HostServices {
            registry: registry.clone(),
            scratch: None,
            parent: 0,
            mailbox: Inbox::new().mailbox(),
            emit: emit.clone(),
            provider: crate::agent::ProviderHandle::new(Arc::new(Provider::scripted(
                "test-model",
                Script::new(),
            ))),
            caps: Capabilities::root(),
            cwd: std::env::temp_dir(),
            fuel: 0,
            returns: true,
            allow_schedule: false,
            search: false,
            reply: crate::agent::ReplyCell::default(),
            schedules: crate::fleet::schedule::ScheduleRegistry::new(),
            log: crate::agent::LogCell::new(test_log()),
            system_template: String::new(),
            index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                ral_core::io::TerminalState::default(),
            )),
            interactive: false,
            nursery: Nursery::default(),
            generation: 0,
            disk_warn_bytes: None,
            egress: crate::egress::Egress::for_test(),
            acts: crate::fleet::desk::ActFragment::default(),
            principal: ral_core::host::user(),
            pins: None,
            wire_seat: true,
            dial: None,
        }
    }

    #[test]
    fn wire_seat_run_round_trips_and_surfaces_a_value() {
        let (seat, mut child) = wire_seat(Liveness::default());

        let mut surfaced = Vec::new();
        let report = ral_core::transport::dispatch_to_report(
            seat.transport(),
            source_run("surface `ping"),
            |v| surfaced.push(v),
            |_| unreachable!("this run raises no enquiry"),
        )
        .expect("the engine must answer the dispatch with a Report");

        assert!(
            matches!(
                report,
                Report::Ran {
                    ending: ral_core::transport::Ending::Settled { .. },
                    ..
                }
            ),
            "`surface \\`ping` must settle to Report::Ran {{ Ok }}, got {report:?}"
        );
        // Core also reports an observation per dispatch on this sink; the
        // kit's own value is the variant among them.
        assert!(
            surfaced.contains(&ral_core::serial::FOValue::Variant {
                label: "ping".into(),
                payload: None
            }),
            "the live surfaced value must reach on_surface before the Report, got {surfaced:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// The binding is production's exactly: `Agent::run_shell` hands its
    /// desk straight to `shell_eval::run_shell`'s closure, not to the seat.
    #[test]
    fn wire_seat_enquiry_is_answered_through_the_drain_loop() {
        let (seat, mut child) = wire_seat(Liveness::default());
        let (emit, _rx) = crate::bus::dummy_emitter();
        let registry = AgentRegistry::new();
        let desk = Arc::new(ExarchDesk {
            services: wire_host_services(&emit, &registry),
        });

        let report = ral_core::transport::dispatch_to_report(
            seat.transport(),
            source_run("agents `list"),
            |_| {},
            |req| {
                desk.handle(req).map_err(|e| EnquiryError {
                    status: e.exit_code(),
                    message: e.message,
                })
            },
        )
        .expect("the engine must answer the dispatch with a Report");

        match report {
            Report::Ran {
                ending: ral_core::transport::Ending::Settled { .. },
                ..
            } => {}
            other => panic!("`agents `list` must settle through the installed desk, got {other:?}"),
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
    fn wire_seat_install_run_installs_the_deferred_sink_for_a_settled_spawn_worker() {
        let (seat, mut child) = wire_seat(Liveness::default());
        let inbox = Inbox::new();
        let (tx, _rx) = crate::bus::channel();
        let root_id = crate::agent::fresh_id();
        let emit = Emitter::with_mailbox(tx, root_id, inbox.mailbox());
        let registry = AgentRegistry::new();

        let install = RunInstall {
            seam: Arc::new(HostSeam {
                desk: ExarchDesk {
                    services: wire_host_services(&emit, &registry),
                },
                apply: crate::fleet::desk::SurfaceApplier {
                    pins: None,
                    id: root_id,
                    recorder: crate::record::Emitter::none(),
                    surface: std::sync::Mutex::new(crate::record::commit::SurfaceBuffer::new()),
                },
            }),
            deferred: crate::shell_eval::deferred_sink(
                &emit,
                root_id,
                &registry,
                crate::record::Emitter::none(),
            ),
            fork: ral_core::types::Fork::Park(Nursery::default()),
        };
        let _guard = seat.install_run(install);

        let report = ral_core::transport::dispatch_to_report(
            seat.transport(),
            source_run("let h = spawn { sleep 1 }"),
            |_| {},
            |_| unreachable!("this run raises no enquiry"),
        )
        .expect("the engine must answer the dispatch with a Report");
        assert!(
            matches!(
                report,
                Report::Ran {
                    ending: ral_core::transport::Ending::Settled { .. },
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
            crate::bus::Item::Surface { id, values, .. } => {
                assert_eq!(id, root_id, "stamped with the root session id");
                // Not a singleton: a `sleep 1` worker's batch also carries its
                // own exec-summary record ahead of the completion marker, so
                // this asserts on content rather than length.
                assert!(
                    values.iter().any(
                        |v| matches!(v, ral_core::Value::Variant { label, .. } if label == "done")
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

    /// `eval_reach().interrupt()` is the registry's per-tab interrupt path.
    /// Generous timing throughout: the dev fleet includes a jittery VM.
    #[test]
    fn wire_eval_reach_cancel_settles_an_in_flight_run_promptly() {
        const WAIT: Duration = Duration::from_secs(20);
        let (seat, mut child) = wire_seat(Liveness::default());
        let reach = seat.eval_reach();

        let settled = std::thread::scope(|s| {
            let dispatch = s.spawn(|| {
                ral_core::transport::dispatch_to_report(
                    seat.transport(),
                    source_run("sleep 30"),
                    |_| {},
                    |_| unreachable!("this run raises no enquiry"),
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

        assert!(
            report.is_some(),
            "the engine must still answer with a Report"
        );
        assert!(
            elapsed < WAIT,
            "cancel must settle the run well inside {WAIT:?} rather than run `sleep 30` to \
             term, took {elapsed:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// No engine child needed: the panic fires before a frame would cross.
    #[test]
    #[should_panic(expected = "/clear has no meaning on a wire seat")]
    fn wire_seat_clear_panics_didactically() {
        let (host, _guest) = UnixStream::pair().expect("socketpair");
        let transport = WireTransport::adopt(host, Liveness::default()).expect("adopt");
        let dir = std::env::temp_dir();
        let mut seat = Seat::wire(transport, dir.clone(), dir);
        seat.clear(&test_log());
    }

    #[test]
    #[should_panic(expected = "direct engine-state access has no meaning on a wire seat")]
    fn wire_seat_shell_mut_panics_didactically() {
        let (host, _guest) = UnixStream::pair().expect("socketpair");
        let transport = WireTransport::adopt(host, Liveness::default()).expect("adopt");
        let dir = std::env::temp_dir();
        let seat = Seat::wire(transport, dir.clone(), dir);
        let _guard = seat.shell_mut();
    }
}
