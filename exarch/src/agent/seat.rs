//! The agent's seat: the transport its runs run through, plus the
//! host-side state that seat kind owns.  Every engine-side reach — per-call
//! installs, `/clear`'s rebuild, the registry's cancel reach — is a seat
//! method, so a second seat kind (the wire engine) is one more variant
//! here, never a second copy of the agent.

use crate::agent::event::AgentLog;
use crate::bootstrap::Scratch;
use crate::fleet::desk::{AbsentDesk, DeskBinding, ExarchDesk, SurfaceApplier};
use crate::fleet::registry::{EvalReach, RunScope};
use crate::shell_eval::builtins;
use ral_core::Shell;
use ral_core::transport::{IdentityTransport, Transport};
use std::sync::{Arc, Mutex};

/// One agent's engine-side attachment.  A closed enum, not a trait object:
/// the operations that differ per seat live off the [`Transport`] trait.
pub(crate) enum Seat {
    /// In-process: owns the session [`Scratch`] (`/clear` reboots from it),
    /// the working directory the shell is re-seeded with on every such
    /// reboot, whether the host granted `detach` (see [`boot_root_shell`]),
    /// and the run-scope cell the registry interrupts through, which must
    /// stay live across a `/clear` rebuild.
    Identity {
        transport: Box<IdentityTransport>,
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        detach: bool,
        run_scope: RunScope,
    },
    /// Out-of-process: the engine lives on the far end of `transport`, one
    /// process per session. It holds nothing per call: a wire run's desk
    /// and applier ride
    /// [`Agent::run_shell`](crate::agent::Agent::run_shell)'s own arguments
    /// into the drain loop's enquiry arm — the engine asks over
    /// [`ral_core::transport::Event::Enquiry`], never a direct call, so there is no engine-side
    /// slot to fill or retire. No scratch: the session's real one lives
    /// inside the guest this transport dials. No nursery: sub-agent forks
    /// are refused at the desk (fuel 0), so there is nothing to adopt into.
    Wire {
        transport: Box<ral_core::transport::WireTransport>,
    },
}

/// One `ral` call's capture set, installed for the extent of the eval —
/// built fresh per call so nothing a desk handler captures can go stale.
pub(crate) struct RunInstall {
    pub(crate) desk: Arc<ExarchDesk>,
    pub(crate) apply: SurfaceApplier,
    pub(crate) deferred: Arc<dyn ral_core::types::DeferredSink>,
    pub(crate) nursery: ral_core::types::Nursery,
}

/// Retires the install on drop — [`AbsentDesk`] back in, nursery cleared —
/// on *every* exit, including a panic [`crate::agent::Agent::attend`] recovers from, where
/// straight-line teardown would leave the desk's whole capture (its
/// [`crate::bus::Emitter`] included) installed for the rest of the session.
pub(crate) struct RunGuard<'s>(&'s Seat);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        match self.0 {
            Seat::Identity { transport, .. } => {
                transport.set_desk(Arc::new(AbsentDesk));
                transport.clear_nursery();
            }
            Seat::Wire { .. } => {}
        }
    }
}

impl Seat {
    /// The identity ceremony — trunk construction, every fork, and the
    /// desk's spawn spine all route here; `/clear` re-runs it through
    /// [`Self::clear`] onto the same run-scope cell.
    pub(crate) fn identity(
        shell: Shell,
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        detach: bool,
        log: &AgentLog,
    ) -> Self {
        let run_scope: RunScope = Arc::new(Mutex::new(None));
        let transport = Box::new(identity_ceremony(shell, log, &run_scope, cwd.clone()));
        Self::Identity {
            transport,
            scratch,
            cwd,
            detach,
            run_scope,
        }
    }

    /// The wire ceremony: attach `transport` over `cwd`/`home` — stated
    /// explicitly by the caller, never read from this process's own state,
    /// since under a VM the workspace is a guest path this host cannot even
    /// resolve — tagged with exarch's compiled-in builtin installer, then
    /// seat it with no per-call install yet.
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

    /// The transport a dispatch or probe runs against.
    pub(crate) fn transport(&self) -> &dyn Transport {
        match self {
            Self::Identity { transport, .. } => &**transport,
            Self::Wire { transport, .. } => &**transport,
        }
    }

    /// Direct engine-state access, identity-only — the test suite's state
    /// inspection door and [`crate::agent::Agent::fork_with`]'s [`Shell::fork_session`] reach. Panics on a
    /// wire seat (see the panic message).
    pub(crate) fn shell_mut(&self) -> std::sync::MutexGuard<'_, ral_core::transport::EngineInner> {
        match self {
            Self::Identity { transport, .. } => transport.shell_mut(),
            Self::Wire { .. } => panic!(
                "direct engine-state access has no meaning on a wire seat: the engine lives in \
                 a separate process, reachable only through Transport's dispatch/probe/control \
                 frames — docs/ral-wiki/decisions/260722_session-is-a-process.md"
            ),
        }
    }

    /// Install one call's capture set; the guard retires it on every exit.
    /// A wire seat has nothing to install: its run's desk and applier ride
    /// [`Agent::run_shell`](crate::agent::Agent::run_shell)'s own arguments
    /// into the drain loop's enquiry arm, so the slots this fills are
    /// identity-only.
    pub(crate) fn install_run(&self, install: RunInstall) -> RunGuard<'_> {
        match self {
            Self::Identity { transport, .. } => {
                transport.set_deferred_sink(install.deferred);
                transport.set_nursery(install.nursery);
                // The drain-then-handle adapter: a handler's chrome must
                // never jump ahead of surface output still queued on the
                // event channel.
                transport.set_desk(Arc::new(DeskBinding {
                    desk: install.desk,
                    events: transport.events_shared(),
                    apply: install.apply,
                }));
            }
            Self::Wire { .. } => {}
        }
        RunGuard(self)
    }

    /// The cancel reach the fleet registry stores for this agent.
    pub(crate) fn eval_reach(&self) -> EvalReach {
        match self {
            Self::Identity {
                transport,
                run_scope,
                ..
            } => EvalReach::Identity {
                eval_root: transport.shell_mut().shell.cancel_handle(),
                run_scope: run_scope.clone(),
            },
            Self::Wire { transport, .. } => EvalReach::Wire(transport.control().clone()),
        }
    }

    /// `/clear`'s engine half: reboot the shell from the owned scratch and
    /// re-run the ceremony onto the *same* run-scope cell.  Replacing the
    /// transport drops the outgoing shell, whose teardown cancels its
    /// registered workers — `/clear` outranks every lease. Panics on a wire
    /// seat (see the panic message).
    pub(crate) fn clear(&mut self, log: &AgentLog) {
        match self {
            Self::Identity {
                transport,
                scratch,
                cwd,
                detach,
                run_scope,
            } => {
                **transport = identity_ceremony(
                    boot_root_shell(scratch, cwd.clone(), *detach),
                    log,
                    run_scope,
                    cwd.clone(),
                );
            }
            Self::Wire { .. } => panic!(
                "/clear has no meaning on a wire seat as a transport swap: session-is-a-process \
                 says a wire session clears by killing its engine process and booting a fresh \
                 one from the same recipe, not by rebuilding this seat in place — a front-end \
                 starts over by replacing the child process, so no caller routes /clear here \
                 and reaching this arm is a host bug \
                 (docs/ral-wiki/decisions/260722_session-is-a-process.md)"
            ),
        }
    }
}

/// Boot a root session shell: the shared exarch boot plus `cwd` seeded as
/// its logical working directory and the scratch's env/binding seeding.
/// Forks instead snapshot their parent through [`Shell::fork_session`],
/// inheriting the seeding.
///
/// `detach` says whether the host decided the verb has meaning at all (no OS
/// sandbox engages, and the platform can double-fork). Naming the verb and
/// arming its budget is deliberately one act: a shell that has the name
/// always has the budget, and the two cannot drift. Where `detach` is false
/// the name is simply absent, and calling it is an ordinary unknown-command
/// diagnostic rather than a builtin that resolves and refuses. `/clear`
/// reboots through here, so a fresh shell re-gains both from the seat's own
/// answer — there is no second install site.
#[cfg_attr(
    not(unix),
    allow(
        unused_variables,
        reason = "detach is born by double-fork, a POSIX act: off unix core publishes no builtin to install"
    )
)]
pub(crate) fn boot_root_shell(scratch: &Scratch, cwd: std::path::PathBuf, detach: bool) -> Shell {
    let mut shell = crate::bootstrap::boot_shell();
    // The trunk session owns this process's signals: an Esc or an async
    // SIGINT interrupts its in-flight run, a SIGTERM its whole session. A
    // sub-agent's session is a `fork_session` of it, and stays deaf — the
    // registry stops one through its own cancel handle.
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

/// Seed the session-dir variable, arm the ledgers over everything just
/// seeded (seeding then arming stay one visible sequence), then seat the
/// shell behind a fresh transport observing `run_scope` and attach the
/// host endpoint.
/// `cwd` is the same directory [`boot_root_shell`] seeded onto the shell —
/// restated here only because [`Transport::attach`]'s signature is shared with the wire
/// transport, which does read its `cwd` argument.
fn identity_ceremony(
    mut shell: Shell,
    log: &AgentLog,
    run_scope: &RunScope,
    cwd: std::path::PathBuf,
) -> IdentityTransport {
    // `EXARCH_SESSION_DIR` must always point at the live session's
    // event-log directory, on construction and every `/clear` rebuild.
    crate::bootstrap::seed_var(
        &mut shell,
        "EXARCH_SESSION_DIR",
        &log.dir().to_string_lossy(),
    );
    crate::bootstrap::arm_session_ledgers(&mut shell);
    let mut transport = IdentityTransport::new(shell);
    transport.observe_foreground(run_scope.clone());
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

// Wire-seat tests drive a *real* engine child (`std::env::current_exe()`
// re-exec'd with `--engine`, exactly the pre_exec handoff
// `WireTransport::new`/`exarch/tests/wire_liveness.rs` use), never an
// in-process `engine_session` thread. `engine_session` faces its process's
// signals, so its runs fold the process-lifetime ambient cancel causes;
// core serialises its own tests against those cells with a lock that is
// `pub(crate)` to core and unreachable here. A same-process engine thread
// would race the cells against whatever sibling test in this same lib
// binary is mid-run; a child process owns its cells alone.
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
    use crate::provider::{Provider, ProviderKind, scripted::Script};
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
        }
    }

    /// A session log under a scratch dir unique to this call, mirroring
    /// `fleet::desk`'s own test fixture — tests run in parallel, and one
    /// fixed path would race a sibling's `remove_dir_all` against this
    /// call's `create_dir_all`.
    fn test_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("exarch-wire-seat-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        AgentLog::root(&root, n, "test", "test", 0).expect("session log")
    }

    /// Spawn a real `--engine` child over a fresh socketpair and adopt the
    /// host end by hand — the guest end crosses as fd 3, the same `pre_exec`
    /// handoff `WireTransport::new` performs internally, done here through
    /// `adopt` instead since that is the one constructor `Seat::wire` (and
    /// so synod) actually calls.
    fn spawn_engine(liveness: Liveness) -> (WireTransport, std::process::Child) {
        let (host, guest) = UnixStream::pair().expect("socketpair");
        let guest_fd = guest.as_raw_fd();
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
        cmd.arg("--engine");
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        // SAFETY: runs between fork and exec, calling only the
        // async-signal-safe `dup2`/`close` with no allocation and no
        // locking — the same pre_exec `WireTransport::new` and
        // `exarch/tests/wire_liveness.rs` use.
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
        // The guest end has crossed into the child as fd 3; holding it here
        // too would hide the child's death from this end's own EOF.
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

    /// A [`HostServices`] fixture with a fresh registry and no scratch — the
    /// wire seat's own shape (`Agent::host_services`), sufficient for the
    /// `agent-list` enquiry the round-trip test drives.
    fn wire_host_services(emit: &Emitter, registry: &AgentRegistry) -> HostServices {
        HostServices {
            registry: registry.clone(),
            scratch: None,
            parent: 0,
            mailbox: Inbox::new().mailbox(),
            emit: emit.clone(),
            provider: crate::agent::ProviderHandle::new(Arc::new(Provider::scripted(
                "test-model",
                ProviderKind::Openai,
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
            indexes: crate::prompt::BuiltinIndexes::resolve(&ral_core::Shell::new(
                ral_core::io::TerminalState::default(),
            )),
            interactive: false,
            nursery: Nursery::default(),
            generation: 0,
            disk_warn_bytes: None,
            egress: crate::egress::Egress::for_test(),
        }
    }

    /// A wire-seat run round-trips through a real engine child: dispatch,
    /// drain, and a live `` `surface `` value reaches `on_surface` in order
    /// before the terminal `Report`.
    #[test]
    fn wire_seat_run_round_trips_and_surfaces_a_value() {
        let (seat, mut child) = wire_seat(Liveness::default());

        let mut surfaced = Vec::new();
        let report = ral_core::transport::dispatch_to_report(
            seat.transport(),
            source_run("surface `ping"),
            |v| surfaced.push(v),
            |_| {},
            |_| unreachable!("this run raises no enquiry"),
        )
        .expect("the engine must answer the dispatch with a Report");

        assert!(
            matches!(report, Report::Ran { result: Ok(_), .. }),
            "`surface \\`ping` must settle to Report::Ran {{ Ok }}, got {report:?}"
        );
        assert_eq!(
            surfaced,
            vec![ral_core::serial::FOValue::Variant {
                label: "ping".into(),
                payload: None
            }],
            "the live surfaced value must reach on_surface, in order, before the Report"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// An enquiry the engine raises mid-run (the `agents` builtin's
    /// `` `agent-list ``) crosses as a real `Event::Enquiry` frame and is
    /// answered by an `ExarchDesk` in the drain loop's enquiry arm — the
    /// production binding exactly: `Agent::run_shell` hands its desk
    /// straight to `shell_eval::run_shell`'s closure, never through the
    /// seat.
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
            source_run("agents"),
            |_| {},
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
            Report::Ran { result: Ok(_), .. } => {}
            other => panic!("`agents` must settle through the installed desk, got {other:?}"),
        }

        let _ = child.kill();
        let _ = child.wait();
    }

    /// `eval_reach().interrupt()` — the registry's per-tab interrupt path —
    /// cancels an in-flight wire run promptly: a `sleep 30` that could not
    /// settle inside the ceiling on its own does, once cancelled.  Generous
    /// timing throughout: the dev fleet includes a jittery VM.
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
                    |_| {},
                    |_| unreachable!("this run raises no enquiry"),
                )
            });
            // Let the engine actually enter the sleep before interrupting,
            // so what gets cancelled is a genuinely in-flight run.
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

    /// `/clear` on a wire seat panics with the didactic session-is-a-process
    /// message — no real engine communication is needed to prove it, since
    /// the panic fires before any frame would cross.
    #[test]
    #[should_panic(expected = "/clear has no meaning on a wire seat")]
    fn wire_seat_clear_panics_didactically() {
        let (host, _guest) = UnixStream::pair().expect("socketpair");
        let transport = WireTransport::adopt(host, Liveness::default()).expect("adopt");
        let dir = std::env::temp_dir();
        let mut seat = Seat::wire(transport, dir.clone(), dir);
        seat.clear(&test_log());
    }

    /// `shell_mut` on a wire seat panics with the didactic
    /// session-is-a-process message.
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
