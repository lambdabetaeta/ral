//! The agent's seat: the transport its runs go through, plus whatever
//! host-side state that seat kind owns.  Every engine-side reach is a
//! method here, so a new seat kind is one more variant, not a second agent.

use crate::agent::cancel::InterruptTarget;
use crate::bootstrap::Scratch;
use crate::shell_eval::builtins;
use ral_core::engine::EngineInstaller;
use ral_core::protocol::{Attach, IdentityTransport, ProbeError, Severed, Transport};
use std::sync::Arc;

/// How a fork of this seat's engine reaches the desk that adopts it: parked in
/// the parent's own transport, or dialled across a wire. `` `start `` and
/// `/branch` choose their arm on this fact alone.
pub(crate) enum SeatKind {
    Identity(Arc<IdentityTransport>),
    Wire,
}

/// One agent's engine-side attachment; what differs per call already lives
/// off the [`Transport`] trait, so this stays a closed enum.
pub(crate) enum Seat {
    /// In-process.  `/clear` reboots it onto the *same* `target`: the cell an
    /// interrupt reaches the run through must outlive the rebuild.
    Identity {
        transport: Arc<IdentityTransport>,
        target: InterruptTarget,
        /// `None` for an adopted fork, whose authority a fresh boot would
        /// not carry.
        rebirth: Option<Rebirth>,
    },
    /// Out-of-process, one engine per session: a fork is hatched guest-side
    /// and dialled by the desk's wire arm.
    Wire {
        transport: Box<ral_core::protocol::WireTransport>,
        target: InterruptTarget,
    },
}

/// What an identity root reboots from: the recipes and Attach it was born
/// from, and the scratch that Attach names, which outlives every reboot.
pub(crate) struct Rebirth {
    installers: &'static [EngineInstaller],
    attach: Attach,
    _scratch: Arc<Scratch>,
}

/// When in a session's life its engine was lost, which is the whole of what
/// decides what a person should do next.
///
/// A start failure is a thing to retry; a mid-session death is a thing to
/// abandon — telling someone who has already produced something to "start a
/// new session" is advice for a conversation that never began.  Nothing
/// else about the two cases differs, which is why this is a two-variant enum
/// and not a description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePhase {
    /// The engine went before the session ever took a turn — the attach was
    /// refused, or the engine died between being spawned and answering it.
    Starting,
    /// The engine went with a session already under way, taking the
    /// conversation's whole state with it.
    Running,
}

/// The one thing every edge says about a severed engine, said once.
///
/// The transport, [`Severed`]'s `Display`, the seat, and the front-end all
/// report through this single, short, fixed sentence rather than each
/// wrapping the fact in its own words.  The one thing beside it is the one
/// thing a reader can act on: the run's log directory.
///
/// Neither the engine's own words nor [`Severed::code`] belong in a window —
/// a paragraph of machinery and a token for a bug report.  Both go to the
/// log, which is what [`Self::logged`] is for and why the sentence names it.
///
/// It is an [`Error`](std::error::Error) so that the one edge that has to
/// cross an [`io::Error`](std::io::Error) — `Avatar::root`, whose other
/// failures are ordinary filesystem ones — can carry it whole rather than
/// flattened to a string.  A front-end downcasts to tell a severance from a
/// log directory it could not create, and so knows whether it has a guest
/// console worth capturing before it tears the machine down.
#[derive(Debug)]
pub struct EngineLost {
    phase: EnginePhase,
    cause: Severed,
    /// The run directory, not the session's: a start failure has no session
    /// worth naming, and the engine's captured output is a property of the
    /// run.  `None` where the caller genuinely has no log to point at, in
    /// which case the sentence simply omits the invitation rather than
    /// sending the reader somewhere that does not exist.
    log_dir: Option<std::path::PathBuf>,
}

impl EngineLost {
    /// The engine went before the session ever took a turn.
    pub fn starting(cause: &Severed, log_dir: Option<&std::path::Path>) -> Self {
        Self {
            phase: EnginePhase::Starting,
            cause: cause.clone(),
            log_dir: log_dir.map(std::path::Path::to_path_buf),
        }
    }

    /// The engine went with a session already under way.
    pub fn running(cause: &Severed, log_dir: Option<&std::path::Path>) -> Self {
        Self {
            phase: EnginePhase::Running,
            cause: cause.clone(),
            log_dir: log_dir.map(std::path::Path::to_path_buf),
        }
    }

    /// The run directory this failure invites a reader into, if there is one.
    #[must_use]
    pub fn log_dir(&self) -> Option<&std::path::Path> {
        self.log_dir.as_deref()
    }

    #[must_use]
    pub const fn phase(&self) -> EnginePhase {
        self.phase
    }

    /// Why the transport says the engine is gone, in its own words — the
    /// paragraph kept out of the user's sentence.  Written into files and
    /// durable records; never shown as the whole of a failure.
    #[must_use]
    pub fn cause(&self) -> &Severed {
        &self.cause
    }

    /// The form that belongs in a log rather than in a window: the sentence a
    /// user is shown, then the code to quote and the engine's own account of
    /// itself, so the durable record keeps what the sentence dropped.
    #[must_use]
    pub fn logged(&self) -> String {
        format!("{self}\n\n({}) {}", self.cause.code(), self.cause)
    }
}

impl std::error::Error for EngineLost {}

/// What happened and what to do, in two full stops, then a bracket holding
/// the one thing worth following: a path.  A dash would ask the reader which
/// half is the advice, and a code beside the path is a word they cannot act
/// on standing where the thing they can act on should be.
impl std::fmt::Display for EngineLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sentence = match (self.phase, &self.cause) {
            // A refusal is deterministic: trying again meets it again.
            (EnginePhase::Starting, Severed::Refused(_)) => "The assistant could not be started.",
            (EnginePhase::Starting, _) => "The assistant could not be started. Try again.",
            (EnginePhase::Running, _) => {
                "The assistant stopped, so this conversation cannot go on. Start a new one."
            }
        };
        f.write_str(sentence)?;
        match &self.log_dir {
            Some(dir) => write!(f, " (details in {})", dir.display()),
            None => Ok(()),
        }
    }
}

/// The sentence is the product here, so it is the thing under test: that it
/// stays one sentence, that it tells a start apart from a death, and that the
/// two things a debugger needs are both in it.  Platform-independent, unlike
/// the wire fixtures at the foot of this file, because nothing below spawns an
/// engine.
#[cfg(test)]
mod lost {
    use super::{EngineLost, EnginePhase};
    use ral_core::protocol::Severed;

    fn closed() -> Severed {
        Severed::Closed("the engine closed the connection".into())
    }

    /// The bug this replaced: a session that never started was told to start
    /// a new one.  The two phases must not read alike.
    #[test]
    fn a_start_failure_and_a_death_give_different_advice() {
        let starting = EngineLost::starting(&closed(), None).to_string();
        let running = EngineLost::running(&closed(), None).to_string();
        assert_ne!(starting, running);
        assert!(
            !starting.to_lowercase().contains("start a new"),
            "a session that never began cannot be told to begin another: {starting}"
        );
        assert!(
            running.to_lowercase().contains("start a new"),
            "a session whose state is gone is worth abandoning: {running}"
        );
    }

    /// A refusal is deterministic, so a refused start is not told to retry.
    #[test]
    fn a_refused_start_is_not_told_to_try_again() {
        let shown =
            EngineLost::starting(&Severed::Refused("protocol version 9".into()), None).to_string();
        assert!(!shown.to_lowercase().contains("try again"), "{shown}");
    }

    /// A user's line carries somewhere to go and nothing else: no code, no
    /// transport's restatement, and no dash holding the advice at arm's
    /// length.
    #[test]
    fn the_sentence_carries_a_path_and_nothing_else() {
        let dir = std::path::PathBuf::from("/state/synod/proj/2026-09-16-170235-26184");
        let shown = EngineLost::starting(&closed(), Some(&dir)).to_string();
        assert!(shown.contains("2026-09-16-170235-26184"), "{shown}");
        assert!(
            !shown.contains("engine-closed"),
            "a code is for the log, not the window: {shown}"
        );
        assert!(
            !shown.contains('—'),
            "the advice is its own sentence, not a clause after a dash: {shown}"
        );
        assert!(
            !shown.contains("the engine closed the connection"),
            "the transport's own restatement is what this replaced: {shown}"
        );
        assert_eq!(
            shown.lines().count(),
            1,
            "what the user is shown is one line: {shown}"
        );
    }

    /// With nowhere to send a reader the invitation is simply absent — never
    /// a dangling "details in", and never an empty bracket in its place.
    #[test]
    fn a_failure_with_no_log_is_the_sentence_alone() {
        let lost = EngineLost::running(&Severed::Faulted("junk frame".into()), None);
        let shown = lost.to_string();
        assert!(!shown.contains("details in"), "{shown}");
        assert!(!shown.contains('('), "{shown}");
        assert!(
            lost.logged().contains("engine-faulted"),
            "the code the window dropped is still in the record: {}",
            lost.logged()
        );
    }

    /// What goes into a record keeps the engine's own account of itself; what
    /// goes into a window does not.  The split is the whole design.
    #[test]
    fn the_logged_form_keeps_what_the_sentence_drops() {
        let lost = EngineLost::running(&Severed::Refused("protocol version 9".into()), None);
        let logged = lost.logged();
        assert!(logged.starts_with(&lost.to_string()), "{logged}");
        assert!(
            logged.contains("protocol version 9"),
            "the engine's own refusal is worth keeping somewhere: {logged}"
        );
        assert!(
            logged.contains("engine-refused") && !lost.to_string().contains("engine-refused"),
            "the code left the window for the record, not the bin: {logged}"
        );
        assert_eq!(lost.phase(), EnginePhase::Running);
        assert_eq!(lost.cause().code(), "engine-refused");
    }
}

impl Seat {
    /// An identity root, booted through `installers` from what this session
    /// states: its cwd, terminal, scratch, and log directory.
    ///
    /// # Errors
    /// The recipe's refusal.
    pub(crate) fn root(
        installers: &'static [EngineInstaller],
        cwd: std::path::PathBuf,
        terminal: ral_core::io::TerminalState,
        scratch: Arc<Scratch>,
        session_dir: &std::path::Path,
    ) -> Result<Self, Severed> {
        let home = ral_core::host::home().unwrap_or_default();
        let mut attach = Attach::new(builtins::INSTALLER_TAG, cwd, home.into());
        attach.terminal = terminal;
        attach.env = scratch.env();
        attach.env.push((
            "EXARCH_SESSION_DIR".into(),
            session_dir.to_string_lossy().into_owned(),
        ));
        let transport = Arc::new(IdentityTransport::boot(installers, &attach)?);
        Ok(Self::Identity {
            target: InterruptTarget::new(transport.control().clone()),
            transport,
            rebirth: Some(Rebirth {
                installers,
                attach,
                _scratch: scratch,
            }),
        })
    }

    /// A fork its parent's transport adopted.
    pub(crate) fn adopted(transport: IdentityTransport) -> Self {
        Self::Identity {
            target: InterruptTarget::new(transport.control().clone()),
            transport: Arc::new(transport),
            rebirth: None,
        }
    }

    /// `cwd` and `home` are the caller's word, never read from this
    /// process: under a VM they are guest paths this host cannot resolve.
    ///
    /// # Errors
    /// The transport's severance, if the engine refuses the attach or falls
    /// silent before answering it.
    pub(crate) fn wire(
        transport: ral_core::protocol::WireTransport,
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    ) -> Result<Self, Severed> {
        transport.attach(Attach::new(builtins::INSTALLER_TAG, cwd, home));
        transport.await_attached()?;
        Ok(Self::Wire {
            target: InterruptTarget::new(transport.control().clone()),
            transport: Box::new(transport),
        })
    }

    pub(crate) fn transport(&self) -> &dyn Transport {
        match self {
            Self::Identity { transport, .. } => &**transport,
            Self::Wire { transport, .. } => &**transport,
        }
    }

    pub(crate) fn kind(&self) -> SeatKind {
        match self {
            Self::Identity { transport, .. } => SeatKind::Identity(transport.clone()),
            Self::Wire { .. } => SeatKind::Wire,
        }
    }

    /// Install the session sink a settling worker's deferred batch reaches.
    pub(crate) fn install_deferred(&self, sink: Arc<dyn ral_core::types::DeferredSink>) {
        self.transport().set_deferred_sink(sink);
    }

    /// Why no further frame will cross this seat's transport, if that has
    /// happened.
    pub(crate) fn severed(&self) -> Option<Severed> {
        self.transport().severed()
    }

    /// Take one reading through `door`. A refusal is a protocol fault here —
    /// probes are asked only at run boundaries — so it severs the seat.
    ///
    /// # Errors
    /// The engine's severance.
    pub(crate) fn read<T>(
        &self,
        door: impl FnOnce(&dyn Transport) -> Result<T, ProbeError>,
    ) -> Result<T, Severed> {
        let t = self.transport();
        door(t).map_err(|e| match e {
            ProbeError::Severed(cause) => cause,
            ProbeError::Rejected(why) => t.sever(Severed::Faulted(why)),
        })
    }

    /// Where this agent's interrupt and terminate land.
    pub(crate) fn reach(&self) -> InterruptTarget {
        match self {
            Self::Identity { target, .. } | Self::Wire { target, .. } => target.clone(),
        }
    }

    /// `/clear`'s engine half: boot afresh from the same Attach, onto the same
    /// target.  Replacing the transport drops the outgoing engine, whose
    /// teardown cancels its workers: `/clear` outranks leases.
    ///
    /// # Errors
    /// A seat with no recipe to reboot from, or the recipe's refusal.
    pub(crate) fn clear(&mut self) -> Result<(), String> {
        let Self::Identity {
            transport,
            target,
            rebirth: Some(rebirth),
        } = self
        else {
            return Err(
                "/clear cannot start this conversation over: its engine was not \
                 booted here, and starting afresh means a new conversation"
                    .to_string(),
            );
        };
        *transport = Arc::new(
            IdentityTransport::boot(rebirth.installers, &rebirth.attach)
                .map_err(|s| s.to_string())?,
        );
        target.republish(transport.control().clone());
        Ok(())
    }
}

// These drive a real `--engine` child, never an in-process
// `engine_session` thread: that faces its process's signals, so a
// same-process engine would race the ambient cancel cells against whatever
// sibling test in this lib binary is mid-run, and core's lock over those
// cells is unreachable from here.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::event::AgentLog;
    use crate::agent::testkit::source_run;
    use crate::bus::{Emitter, Inbox};
    use crate::fleet::Fleet;
    use crate::fleet::desk::{ExarchDesk, HostServices, RunHost, SurfaceApplier};
    use ral_core::protocol::{EnquiryError, Host, Liveness, Report, WireTransport};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Each log takes the next session id, as in `fleet::desk`'s fixture, so
    /// two of them are never the same session to the wire.
    fn test_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        AgentLog::for_test(n, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    /// The fd-3 handoff ral-daemon's `spawn` performs, so the host end can be
    /// taken with `adopt` — the constructor `Seat::wire`, and so synod, calls.
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
            fleet: Fleet::new(),
            kind: SeatKind::Wire,
            stamp: parent.mailbox().stamp(),
            agent: parent.clone(),
            emit: emit.clone(),
            cwd: std::env::temp_dir(),
            home: Some(std::env::temp_dir()),
            reply: crate::agent::ReplyCell::default(),
            log: crate::agent::LogCell::new(test_log()),
            branch: None,
            acts: crate::fleet::desk::ActFragment::default(),
            principal: ral_core::host::user(),
        }
    }

    /// A `Host` that only records the values it is surfaced, for the
    /// round-trip below — this run raises no enquiry and forks nothing.
    struct SurfaceCollector(std::sync::Mutex<Vec<ral_core::serial::FOValue>>);

    impl Host for SurfaceCollector {
        fn surface(&self, val: &ral_core::serial::FOValue) {
            self.0.lock().unwrap().push(val.clone());
        }
        fn enquire(
            &self,
            _req: ral_core::serial::FOValue,
        ) -> Result<ral_core::serial::FOValue, EnquiryError> {
            unreachable!("this run raises no enquiry")
        }
    }

    #[test]
    fn wire_seat_run_round_trips_and_surfaces_a_value() {
        let (seat, mut child) = wire_seat(Liveness::default());

        let host = Arc::new(SurfaceCollector(std::sync::Mutex::new(Vec::new())));
        let report = ral_core::protocol::dispatch_to_report(
            seat.transport(),
            source_run("exarch-surface `ping"),
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
            surfaced.contains(&ral_core::serial::FOValue::Variant {
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
        let fleet = Fleet::new();
        let trunk = test_trunk(&fleet);
        let host: Arc<dyn Host> = Arc::new(RunHost {
            desk: ExarchDesk {
                services: wire_host_services(&emit, &trunk),
            },
            apply: SurfaceApplier {
                recorder: crate::record::Emitter::none(),
            },
        });

        let report = ral_core::protocol::dispatch_to_report(
            seat.transport(),
            source_run("exarch-agents `list"),
            host,
        )
        .expect("the engine must answer the dispatch with a Report");

        match report {
            Report::Ran {
                ending: ral_core::protocol::Ending::Settled { .. },
                ..
            } => {}
            other => panic!(
                "`exarch-agents `list` must settle through the installed desk, got {other:?}"
            ),
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
        let fleet = Fleet::new();
        let trunk = test_trunk(&fleet);
        let root_id = trunk.id;
        let emit = Emitter::with_mailbox(tx, root_id, inbox.mailbox());

        seat.install_deferred(crate::shell_eval::deferred_sink(&emit));
        let host: Arc<dyn Host> = Arc::new(RunHost {
            desk: ExarchDesk {
                services: wire_host_services(&emit, &trunk),
            },
            apply: SurfaceApplier {
                recorder: crate::record::Emitter::none(),
            },
        });

        let report = ral_core::protocol::dispatch_to_report(
            seat.transport(),
            source_run("let h = spawn { sleep 1 }"),
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
            crate::bus::Item::Surface { id, values, .. } => {
                assert_eq!(id, root_id, "stamped with the root session id");
                // Not a singleton: a `sleep 1` worker's batch also carries its
                // own exec-summary record ahead of the completion marker, so
                // this asserts on content rather than length.
                assert!(
                    values.iter().any(
                        |v| matches!(v, ral_core::serial::FOValue::Variant { label, .. } if label == "done")
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
                ral_core::protocol::dispatch_to_report(
                    seat.transport(),
                    source_run("sleep 30"),
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
        };
        let why = seat.clear().expect_err("a wire seat cannot reboot");
        assert!(why.contains("new conversation"), "{why}");
    }
}
