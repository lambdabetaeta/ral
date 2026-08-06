//! The engine process: a connection-lived child holding one `Shell`, running
//! what the front-end dispatches over a framed socket.
//!
//! Two laws shape the loop. Any received frame is proof the front-end lives, so
//! the first `Ping` arms a read deadline, while a peer that never pings leaves
//! the patience infinite — its death arrives as a kernel-guaranteed EOF. And no
//! teardown abandons a run: however the loop exits, it cancels the in-flight run
//! and the durable root under it, then waits for the worker to report and park.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::process::{CancelCause, ForegroundScope};
use crate::serial::FOValue;
use crate::sync::LockExt;
use crate::transport::{
    Control, DispatchId, EnquiryError, EnquiryId, Event, Frame, Report, Run, SessionEvent,
    answer_probe,
};
use crate::types::{DeferredSink, EnquiryDesk, Error, Nursery, Shell, SurfaceSink};
use crate::wire::WireChannel;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

/// One compiled-in boot recipe the engine can be told, at `Attach`, to become.
///
/// Each front-end binary — the REPL, exarch — re-execs itself as its own
/// engine and passes its own table; only the tag crosses the wire, never
/// the function.
pub struct EngineInstaller {
    pub tag: &'static str,
    /// Prelude, host surface, libraries, env seeding, ledger arming — at
    /// Attach, once.
    pub boot: fn() -> Shell,
}

/// The engine's one write door: locks `writer`, writes `frame`, and on error
/// severs the connection under the guard and raises `fault` — the flag the
/// reader loop consults on exit so a front-end that stopped reading is never
/// mistaken for one that cleanly detached.
///
/// Every engine-side write crosses here: a surface emit, a deferred batch, an
/// enquiry, a report, a `Pong`. None of them may park the reader loop, so a
/// stalled write becomes exactly the same fatal, non-blocking event a severed
/// pipe already is.
///
/// `writer`'s poison is never recovered, unlike [`WireDesk::slots`] below: a
/// panic between `write_frame` and `shutdown` can leave a partial frame
/// already on the socket with `fault` unset, and resuming writes into that
/// torn stream is worse than the panic propagating.
fn engine_write(writer: &Mutex<WireChannel>, fault: &AtomicBool, frame: &Frame) -> io::Result<()> {
    let outcome = {
        let mut guard = writer.lock().unwrap();
        let outcome = guard.write_frame(frame);
        if outcome.is_err() {
            guard.shutdown();
        }
        outcome
    };
    if outcome.is_err() {
        fault.store(true, Ordering::SeqCst);
    }
    outcome
}

/// Writes `Event::Surface` frames as values are produced, stamped at emit time
/// with whatever dispatch is then in flight.
struct ChannelSurfaceSink {
    current_dispatch: Arc<AtomicU64>,
    writer: Arc<Mutex<WireChannel>>,
    fault: Arc<AtomicBool>,
}

impl crate::types::EventSink for ChannelSurfaceSink {
    fn emit(&self, ev: &FOValue) {
        let id = DispatchId(self.current_dispatch.load(Ordering::Relaxed));
        let _ = engine_write(
            &self.writer,
            &self.fault,
            &Frame::Event(id, Event::Surface(ev.clone())),
        );
    }
}

/// A detached worker's batch outlives the run that spawned it, so it has no
/// dispatch to stamp and rides `Frame::Session` instead.
struct ChannelDeferredSink {
    writer: Arc<Mutex<WireChannel>>,
    fault: Arc<AtomicBool>,
}

impl crate::types::DeferredSink for ChannelDeferredSink {
    fn deliver(&self, batch: Vec<FOValue>) {
        let _ = engine_write(
            &self.writer,
            &self.fault,
            &Frame::Session(SessionEvent::DeferredSurface(batch)),
        );
    }
}

/// A cancel trips the enquiring run's scope from the reader thread, never this
/// rendezvous, so the park must poll; this bounds how stale a cancel can go.
const ENQUIRY_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(75);

/// Wake cadence while armed: brisk enough to notice a death, slow enough that
/// an idle engine does not spin.
const TICK: Duration = Duration::from_secs(1);

/// The armed silence the engine reads as the front-end's death — six times the
/// host's default 5s ping interval, so no scheduling jitter can fake one.
const HOST_SILENCE_DEADLINE: Duration = Duration::from_secs(30);

/// How long the engine tolerates a silent front-end and a stalled write before
/// declaring it dead. The two deadlines are conceptually distinct — one
/// bounds an absent read, the other a stuck write — so a test that must see a
/// write stall resolved without waiting out the production silence deadline
/// gets its own brisk `Patience`, rather than the constant being hollowed out
/// into a knob. Production always runs [`Patience::default`].
#[derive(Debug, Clone, Copy)]
struct Patience {
    silence: Duration,
    write_stall: Duration,
}

impl Default for Patience {
    fn default() -> Self {
        Self {
            silence: HOST_SILENCE_DEADLINE,
            write_stall: HOST_SILENCE_DEADLINE,
        }
    }
}

/// Bounds the settle against a run that ignores cancellation, so a dead peer
/// can never wedge the exit.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

const SETTLE_POLL: Duration = Duration::from_millis(50);

/// The wire engine's enquiry desk. `enquire` mints an [`EnquiryId`], writes
/// `Event::Enquiry` inside the in-flight dispatch, and parks on a slot keyed by
/// that id until the front-end's `Frame::Answer` fills it — or the run's own
/// cancel scope fires, polled at the condvar's timeout.
struct WireDesk {
    writer: Arc<Mutex<WireChannel>>,
    fault: Arc<AtomicBool>,
    /// Stamped by the worker, so an enquiry names the run that raised it.
    current_dispatch: Arc<AtomicU64>,
    next_eid: AtomicU64,
    /// `None` = parked awaiting an answer, `Some` = answered and awaiting
    /// collection. Absence means the park died; an answer for it is dropped.
    /// Every mutation is a whole-entry insert/remove/overwrite, so poison
    /// here is recovered ([`LockExt`]) rather than propagated.
    slots: Mutex<HashMap<EnquiryId, Option<Result<FOValue, EnquiryError>>>>,
    answered: Condvar,
}

impl WireDesk {
    /// Called from the reader loop. An answer arriving after its park gave up
    /// finds no slot and is dropped.
    fn fill(&self, eid: EnquiryId, answer: Result<FOValue, EnquiryError>) {
        let mut slots = self.slots.lock_ignore_poison();
        if let Some(slot) = slots.get_mut(&eid) {
            *slot = Some(answer);
            self.answered.notify_all();
        }
    }
}

impl EnquiryDesk for WireDesk {
    fn enquire(
        &self,
        req: FOValue,
        cancel: &crate::process::CancelScope,
    ) -> Result<FOValue, Error> {
        let id = DispatchId(self.current_dispatch.load(Ordering::Relaxed));
        let eid = EnquiryId(self.next_eid.fetch_add(1, Ordering::Relaxed));
        self.slots.lock_ignore_poison().insert(eid, None);

        if engine_write(
            &self.writer,
            &self.fault,
            &Frame::Event(id, Event::Enquiry(eid, req)),
        )
        .is_err()
        {
            self.slots.lock_ignore_poison().remove(&eid);
            return Err(Error::new("enquiry lost: the host connection is down", 1));
        }

        // The park raises `CancelCause`'s own words, so a cancelled enquiry
        // reads like every other cancelled poll point (`process::check`).
        let mut slots = self.slots.lock_ignore_poison();
        loop {
            if let Some(Some(_)) = slots.get(&eid) {
                let answer = slots.remove(&eid).flatten().expect("slot just seen filled");
                return answer.map_err(|e| Error::new(e.message, e.status));
            }
            if let Some(cause) = cancel.cause() {
                slots.remove(&eid);
                return Err(Error::new(cause.message(), cause.exit_code()));
            }
            slots = self
                .answered
                .wait_timeout(slots, ENQUIRY_CANCEL_POLL)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// A `Result` rather than a direct exit, so the refusal path is testable;
/// `engine_session`, its only caller, does the exiting.
fn resolve_installer<'a>(
    installers: &'a [EngineInstaller],
    proto_version: u32,
    installer: &str,
) -> Result<&'a EngineInstaller, String> {
    use crate::transport::PROTOCOL_VERSION;
    if proto_version != PROTOCOL_VERSION {
        return Err(format!(
            "engine: protocol version mismatch (front-end {proto_version}, engine {PROTOCOL_VERSION})"
        ));
    }
    installers
        .iter()
        .find(|i| i.tag == installer)
        .ok_or_else(|| format!("engine: unknown builtin installer '{installer}'"))
}

fn write_report(writer: &Mutex<WireChannel>, fault: &AtomicBool, id: DispatchId, report: Report) {
    let _ = engine_write(writer, fault, &Frame::Event(id, Event::Report(report)));
}

/// The engine's rendezvous, held for one run or one probe. Winning a claim is
/// the only way to mint one and only a `Dispatch` rides the channel, so the
/// busy flag can never stand raised without work behind it.
struct Dispatch {
    id: DispatchId,
    busy: Arc<AtomicBool>,
    stamp: Arc<AtomicU64>,
}

impl Dispatch {
    fn claim(busy: &Arc<AtomicBool>, stamp: &Arc<AtomicU64>, id: DispatchId) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                id,
                busy: busy.clone(),
                stamp: stamp.clone(),
            })
    }
}

/// The one place the stamp and the flag are lowered, so no path can skip it —
/// including an unwind out of the worker. Only infallible work belongs here: a
/// panic raised during an unwind aborts the process.
impl Drop for Dispatch {
    fn drop(&mut self) {
        self.stamp.store(0, Ordering::Relaxed);
        self.busy.store(false, Ordering::Release);
    }
}

/// Adopt the socket the front-end left on fd 3 and run the engine on it.
///
/// Only the adoption and the final `exit` live here; the protocol itself is
/// [`engine_session`], separated so a test can drive a real engine over a
/// [`WireChannel`] pair without leaving the process.
///
/// # Panics
/// Panics if the inherited wire channel cannot be cloned for the writer.
pub fn run_engine(installers: &[EngineInstaller]) -> ! {
    // SAFETY: fd 3 is the socket inherited from the front-end
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    // The handoff must leave fd 3 open across exec, so set CLOEXEC the instant
    // the engine owns it: no external command a run spawns may inherit the wire.
    if let Err(e) = rustix::io::fcntl_setfd(&stream, rustix::io::FdFlags::CLOEXEC) {
        eprintln!("engine: failed to set CLOEXEC on the wire fd: {e}");
        std::process::exit(1);
    }
    let reader_ch = WireChannel::from_stream(stream);
    std::process::exit(engine_session(reader_ch, installers, Patience::default()));
}

/// The engine's whole protocol life over one already-open channel: `Attach`
/// handshake, worker rendezvous, reader loop, teardown settle.
///
/// Returns the process exit code — `0` for a clean detach or EOF, `1` for a
/// protocol fault, a read error, silence past the deadline, or a write that
/// stalled past `patience.write_stall`: a front-end that stops reading is,
/// within that bound, treated exactly as one that has gone silent. An
/// unrecognised installer tag refuses as loudly as a version mismatch: an
/// engine speaking the wrong builtins is exactly the incoherence this rail
/// rules out.
///
/// # Panics
/// Panics if the wire channel cannot be cloned for the writer.
fn engine_session(
    reader_ch: WireChannel,
    installers: &[EngineInstaller],
    patience: Patience,
) -> i32 {
    let writer_ch = reader_ch.try_clone().expect("try_clone engine channel");
    writer_ch
        .set_write_deadline(patience.write_stall)
        .expect("set write deadline on engine channel");
    let writer = Arc::new(Mutex::new(writer_ch));
    let wire_fault = Arc::new(AtomicBool::new(false));
    let mut reader_ch = reader_ch;

    // Nothing but Attach is a legal first frame, so this is one read, not a
    // loop: the engine speaks no shell until told a version and an installer.
    let shell = match reader_ch.read_frame() {
        Ok(Some(Frame::Attach {
            endpoint,
            cwd,
            home,
            rc_path,
            proto_version,
            installer,
        })) => {
            let target = match resolve_installer(installers, proto_version, &installer) {
                Ok(target) => target,
                Err(msg) => {
                    eprintln!("{msg}");
                    return 1;
                }
            };
            // Both restores skip when the value already holds: an in-process
            // engine attaches with its host's own cwd and HOME, and must not
            // disturb either.
            #[allow(
                clippy::disallowed_methods,
                reason = "engine cwd restore during Attach — sets engine process cwd, not Shell logical cwd"
            )]
            if std::env::current_dir().is_ok_and(|d| d != cwd)
                && let Err(e) = std::env::set_current_dir(&cwd)
            {
                eprintln!("engine: failed to set cwd to {}: {e}", cwd.display());
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "engine HOME restore during Attach — process env, not Shell state"
            )]
            if std::env::var_os("HOME").as_deref() != Some(home.as_os_str()) {
                // SAFETY: single-threaded engine startup, no other threads
                unsafe {
                    std::env::set_var("HOME", &home);
                }
            }
            // Conveyed but unused: no terminal fds cross the socket, and rc
            // loading belongs to the REPL host that owns the rc machinery.
            let _ = endpoint;
            let _ = rc_path;

            let mut shell = (target.boot)();
            // This process is the session's host, so its runs must fold the
            // signals the process is sent, not just the `Control::Cancel` arm.
            shell.face_signals();
            // Only ral-daemon's closed environment sets RAL_GUEST. Gated on the
            // env var and never on the installer tag, so every boot recipe is
            // jailed alike without any of them knowing jails exist.
            #[cfg(target_os = "linux")]
            if std::env::var("RAL_GUEST").is_ok() {
                shell.install_guest_jail(std::sync::Arc::new(
                    crate::process::jail::GuestJail::new(
                        std::path::PathBuf::from("/sys/fs/cgroup/ral-exec"),
                        100_000,
                        crate::process::jail::JailLimits::default(),
                    ),
                ));
            }
            // A wire-hatched child's whole reason for existing: the parent's
            // `hatch` named this fd, so a seed is waiting to become this
            // shell's scope and ceiling before anything else runs.
            if let Ok(fd) = std::env::var(crate::hatch::RAL_ENGINE_SEED_FD_ENV)
                && let Err(msg) = crate::hatch::apply_seed(&fd, &mut shell)
            {
                eprintln!("engine: {msg}");
                return 1;
            }
            shell
        }
        Ok(Some(Frame::Detach) | None) => return 0,
        Ok(Some(_)) => {
            eprintln!("engine: expected Attach as the first frame");
            return 1;
        }
        Err(e) => {
            eprintln!("engine: read error awaiting attach: {e}");
            return 1;
        }
    };

    // A probe rides the same rendezvous as a run, so it serialises with
    // dispatches for free: sent mid-run it gets "engine busy", the same arm a
    // second dispatch gets.
    enum WorkItem {
        /// Boxed so a probe is not sized to `Run`'s stack footprint. The scope
        /// is the one its frame is born under.
        Run(Box<Run>, ForegroundScope),
        Probe(FOValue),
    }

    // Taken before the shell moves into the worker: the teardown must reach the
    // shell's deferred workers without the shell in hand.
    let root = shell.cancel_handle();
    // Each dispatch hangs a child off this and is cancelled through it, so one
    // dispatch's cancel cannot reach the next.
    let dispatches = shell.run_cancel_handle();

    // Lowered by the claimed `Dispatch`'s `Drop`, which runs once the worker
    // has written its Report, so "engine busy" means a run genuinely in
    // flight, not a worker slow to re-park.
    let busy = Arc::new(AtomicBool::new(false));
    let (run_tx, run_rx) = mpsc::channel::<(Dispatch, WorkItem)>();

    let current_dispatch = Arc::new(AtomicU64::new(0));
    let desk = Arc::new(WireDesk {
        writer: writer.clone(),
        fault: wire_fault.clone(),
        current_dispatch: current_dispatch.clone(),
        next_eid: AtomicU64::new(1),
        slots: Mutex::new(HashMap::new()),
        answered: Condvar::new(),
    });
    let surface = Arc::new(ChannelSurfaceSink {
        current_dispatch: current_dispatch.clone(),
        writer: writer.clone(),
        fault: wire_fault.clone(),
    });
    let deferred = Arc::new(ChannelDeferredSink {
        writer: writer.clone(),
        fault: wire_fault.clone(),
    });

    // ── Worker thread: owns the Shell ──────────────────────────────
    let worker_writer = writer.clone();
    let worker_fault = wire_fault.clone();
    let worker_desk = desk.clone();
    std::thread::spawn(move || {
        let mut shell = shell;
        while let Ok((dispatch, item)) = run_rx.recv() {
            let id = dispatch.id;
            // `Shell::run` already catches, rolls back, and reports a panic in
            // the run itself; this outer catch is for one escaping the report
            // plumbing, or `answer_probe`'s own lock poisoning, either of which
            // would otherwise kill the thread unreported.
            let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match item {
                WorkItem::Run(run, scope) => {
                    // What the desk and the sinks read to stamp their frames.
                    dispatch.stamp.store(id.0, Ordering::Relaxed);

                    let req = crate::run::RunRequest {
                        run: *run,
                        surface: Some(surface.clone() as SurfaceSink),
                        deferred: Some(deferred.clone() as Arc<dyn DeferredSink>),
                        desk: Some(worker_desk.clone() as crate::types::Desk),
                        // Fresh per dispatch, exactly as the host's own
                        // `Agent::run_shell` mints one per call: a wire
                        // trunk's `agent` builtin forks into it and, on the
                        // hatch arm, adopts straight back out of it within
                        // this same dispatch — the desk across the wire
                        // never needs to reach it.
                        nursery: Some(Nursery::default()),
                        lifecycle: Box::new(()),
                    };
                    let run_report = shell.run_under(&scope, req);
                    run_report.into_report(shell.sources())
                }
                WorkItem::Probe(reading) => match answer_probe(&mut shell, &reading) {
                    Ok(v) => Report::Ran {
                        ending: crate::transport::Ending::Settled {
                            value: v,
                            status: 0,
                        },
                        captured: None,
                        trail: Vec::new(),
                    },
                    Err(message) => Report::Ran {
                        ending: crate::transport::Ending::Raised {
                            rendered: message,
                            command_exit: false,
                            single_command: false,
                            status: 1,
                        },
                        captured: None,
                        trail: Vec::new(),
                    },
                },
            }))
            .unwrap_or_else(|_| Report::Static {
                diagnostics: crate::transport::Diagnostics::Host(
                    "engine: dispatch panicked in the engine worker".into(),
                ),
            });

            // `dispatch` drops after this, never before: the engine reads idle
            // only once the front-end has the report in hand.
            write_report(&worker_writer, &worker_fault, id, report);
        }
    });

    // Only the frame that wins the `false → true` flip may hand the worker
    // work; the rest get the refusal below. `WorkerGone` is reachable because an
    // unwind lowers the flag on its way out, so a closed channel here is a dead
    // thread rather than a stale claim. `Busy` is distinguished from `Took` so a
    // refused dispatch leaves the in-flight run's cancel handle standing.
    enum Claimed {
        Took,
        Busy,
        WorkerGone,
    }
    let claim = |id: DispatchId, item: WorkItem| -> Claimed {
        if let Some(dispatch) = Dispatch::claim(&busy, &current_dispatch, id) {
            if run_tx.send((dispatch, item)).is_ok() {
                Claimed::Took
            } else {
                Claimed::WorkerGone
            }
        } else {
            write_report(
                &writer,
                &wire_fault,
                id,
                Report::Static {
                    diagnostics: crate::transport::Diagnostics::Host("engine busy".into()),
                },
            );
            Claimed::Busy
        }
    };

    // ── Reader loop (this thread) ──────────────────────────────────
    // The exit code is how the parent tells a session that ended on request
    // from one that ended on corruption.
    let mut armed = false;
    let mut last_frame = Instant::now();
    // The dispatch a `Cancel` may name and the scope that stops it. Replaced,
    // never cleared: a cancel that has outlived its run no longer finds the id
    // it names, so it cannot touch the run that followed.
    let mut cancellable: Option<(DispatchId, ForegroundScope)> = None;
    // The converse: a `Cancel` naming a dispatch not yet arrived. The host
    // stamps the id before taking the write lock, so a cancel raised in that
    // window crosses first; it waits here for the `Dispatch` that claims it.
    let mut foretold: Option<DispatchId> = None;

    let exit_code = loop {
        // Armed, park on a `TICK` so silence past the deadline is noticed;
        // unarmed, block in `read_frame`, that front-end's death being EOF.
        let read = if armed {
            match reader_ch.poll_readable(Some(TICK)) {
                Ok(true) => reader_ch.read_frame(),
                Ok(false) => {
                    let silent = last_frame.elapsed();
                    if silent >= patience.silence {
                        eprintln!(
                            "engine: front-end silent for {}s (deadline {}s) — failing the in-flight run and exiting",
                            silent.as_secs(),
                            patience.silence.as_secs()
                        );
                        break 1;
                    }
                    continue;
                }
                Err(e) => Err(e),
            }
        } else {
            reader_ch.read_frame()
        };

        let frame = match read {
            Ok(Some(frame)) => frame,
            Ok(None) => break 0, // EOF: gone, but cleanly
            Err(e) => {
                eprintln!("engine: read error: {e}");
                break 1;
            }
        };
        // Any frame at all is proof of life, not just a `Ping`.
        last_frame = Instant::now();

        match frame {
            Frame::Dispatch(id, run) => {
                // Minted ahead of the handoff, so a `Cancel` arriving while the
                // worker is still waking finds a scope to land on.
                let scope = dispatches.child();
                if foretold.take_if(|pending| *pending == id).is_some() {
                    scope.cancel(CancelCause::Explicit);
                }
                match claim(id, WorkItem::Run(run, scope.clone())) {
                    Claimed::Took => cancellable = Some((id, scope)),
                    Claimed::Busy => {}
                    Claimed::WorkerGone => break 0,
                }
            }
            Frame::Probe(id, reading) => match claim(id, WorkItem::Probe(reading)) {
                Claimed::Took | Claimed::Busy => {}
                Claimed::WorkerGone => break 0,
            },
            Frame::Answer(eid, answer) => {
                desk.fill(eid, answer);
            }
            Frame::Control(Control::Cancel(did)) => {
                // The dispatch's own scope, not the interrupt watermark: the
                // watermark reaches only frames already born, and cancellation
                // on a scope is sticky, so a run not yet started still reads it.
                match &cancellable {
                    Some((id, scope)) if *id == did => scope.cancel(CancelCause::Explicit),
                    // Either it overtook the `Dispatch` it names or it names a
                    // run already reported; ids are minted once, so holding it
                    // can only ever serve the first.
                    _ => foretold = Some(did),
                }
            }
            Frame::Control(Control::Resize(_winsize)) => {
                // No terminal fds reach the engine, so it has nothing to resize.
            }
            Frame::Control(Control::Suspend | Control::Resume) => {}
            Frame::Ping(seq) => {
                armed = true;
                let _ = engine_write(&writer, &wire_fault, &Frame::Pong(seq));
            }
            Frame::Pong(_) => {
                eprintln!("engine: unexpected Pong — the engine never pings");
            }
            Frame::Attach { .. } => {
                eprintln!("engine: unexpected second Attach");
            }
            Frame::Detach => break 0,
            Frame::Event(..) => {
                eprintln!("engine: unexpected Event frame");
            }
            Frame::Session(..) => {
                eprintln!("engine: unexpected Session frame");
            }
        }
    };

    // ── Teardown settle ────────────────────────────────────────────
    // The loop may have exited with a run in flight: cancel it and the durable
    // root under it, then wait.
    crate::process::request_foreground_cancel(CancelCause::Explicit);
    root.cancel(CancelCause::Explicit);
    crate::hatch::teardown_hatched();
    let settle_by = Instant::now() + SETTLE_TIMEOUT;
    while busy.load(Ordering::Acquire) && Instant::now() < settle_by {
        std::thread::sleep(SETTLE_POLL);
    }
    // A severed wire is never the clean detach the EOF arm's `0` reports: a
    // front-end that stopped reading loses its in-flight run exactly as one
    // that fell silent does, and the exit code must say so.
    if exit_code == 0 && wire_fault.load(Ordering::SeqCst) {
        1
    } else {
        exit_code
    }
}

// ── Wire desk tests ───────────────────────────────────────────────────
//
// A peer `WireChannel` end plays the front-end and `fill` is called by hand, as
// the reader loop calls it; each test states its own scope, so none touch
// process-global state. A real wire child is out of reach here —
// `WireTransport::new` re-execs the current binary with `--engine`, a flag only
// the host binaries handle, so a core test binary would re-run the harness.
#[cfg(test)]
mod wire_desk_tests {
    use super::*;
    use crate::process::{CancelCause, CancelScope};

    /// The enquiry is stamped with the in-flight dispatch, and the answer
    /// `fill` delivers is what `enquire` returns.
    #[test]
    fn enquire_round_trips_through_the_rendezvous() {
        let (ours, mut peer) = WireChannel::pair().expect("socketpair");
        let desk = Arc::new(WireDesk {
            writer: Arc::new(Mutex::new(ours)),
            fault: Arc::new(AtomicBool::new(false)),
            current_dispatch: Arc::new(AtomicU64::new(7)),
            next_eid: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            answered: Condvar::new(),
        });

        let filler = desk.clone();
        let front_end = std::thread::spawn(move || {
            let frame = peer.read_frame().expect("read").expect("open");
            let Frame::Event(did, Event::Enquiry(eid, req)) = frame else {
                panic!("expected Event::Enquiry, got {frame:?}");
            };
            assert_eq!(did, DispatchId(7), "stamped with the in-flight dispatch");
            assert_eq!(req, FOValue::Int { value: 41 });
            filler.fill(eid, Ok(FOValue::Int { value: 42 }));
        });

        let answer = desk.enquire(FOValue::Int { value: 41 }, &CancelScope::default());
        front_end.join().expect("front-end thread");
        assert_eq!(answer.expect("answered"), FOValue::Int { value: 42 });
        assert!(
            desk.slots.lock().unwrap().is_empty(),
            "the answered slot is removed"
        );
    }

    /// A refusal raises with the front-end's own message and status, so an
    /// enquiry fails alike under either transport.
    #[test]
    fn refused_enquiry_raises_message_and_status() {
        let (ours, mut peer) = WireChannel::pair().expect("socketpair");
        let desk = Arc::new(WireDesk {
            writer: Arc::new(Mutex::new(ours)),
            fault: Arc::new(AtomicBool::new(false)),
            current_dispatch: Arc::new(AtomicU64::new(1)),
            next_eid: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            answered: Condvar::new(),
        });

        let filler = desk.clone();
        let front_end = std::thread::spawn(move || {
            let frame = peer.read_frame().expect("read").expect("open");
            let Frame::Event(_, Event::Enquiry(eid, _)) = frame else {
                panic!("expected Event::Enquiry, got {frame:?}");
            };
            filler.fill(eid, Err(EnquiryError::no_desk()));
        });

        let err = desk
            .enquire(FOValue::Unit, &CancelScope::default())
            .expect_err("refused");
        front_end.join().expect("front-end thread");
        assert_eq!(err.message, crate::types::NO_DESK);
        assert_eq!(
            err.status,
            crate::types::Status::Code(crate::types::NO_DESK_STATUS)
        );
    }

    /// A cancelled park unwinds at its next poll tick and takes its slot with
    /// it, so the answer that never came has nowhere to land.
    #[test]
    fn cancel_wakes_a_parked_enquiry() {
        let (ours, _peer) = WireChannel::pair().expect("socketpair");
        let desk = WireDesk {
            writer: Arc::new(Mutex::new(ours)),
            fault: Arc::new(AtomicBool::new(false)),
            current_dispatch: Arc::new(AtomicU64::new(1)),
            next_eid: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            answered: Condvar::new(),
        };

        let scope = CancelScope::default();
        scope.cancel(CancelCause::Explicit);

        let err = desk.enquire(FOValue::Unit, &scope).expect_err("cancelled");
        assert_eq!(err.message, "cancelled");
        assert!(
            desk.slots.lock().unwrap().is_empty(),
            "a cancelled park removes its slot"
        );

        // `EnquiryId(1)` is the one this park minted and abandoned.
        desk.fill(EnquiryId(1), Ok(FOValue::Unit));
        assert!(desk.slots.lock().unwrap().is_empty());
    }

    #[test]
    fn late_answer_for_a_dead_id_is_dropped() {
        let (ours, _peer) = WireChannel::pair().expect("socketpair");
        let desk = WireDesk {
            writer: Arc::new(Mutex::new(ours)),
            fault: Arc::new(AtomicBool::new(false)),
            current_dispatch: Arc::new(AtomicU64::new(1)),
            next_eid: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            answered: Condvar::new(),
        };
        desk.fill(EnquiryId(99), Ok(FOValue::Unit));
        assert!(
            desk.slots.lock().unwrap().is_empty(),
            "an unknown id must not mint a slot"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::PROTOCOL_VERSION;

    fn stub_boot() -> Shell {
        Shell::new(crate::io::TerminalState::default())
    }

    fn stub_installers() -> Vec<EngineInstaller> {
        vec![EngineInstaller {
            tag: "exarch-agent",
            boot: stub_boot,
        }]
    }

    #[test]
    fn resolve_installer_matches_known_tag() {
        let installers = stub_installers();
        match resolve_installer(&installers, PROTOCOL_VERSION, "exarch-agent") {
            Ok(target) => assert_eq!(target.tag, "exarch-agent"),
            Err(msg) => panic!("known tag must resolve, got {msg}"),
        }
    }

    #[test]
    fn resolve_installer_refuses_unknown_tag() {
        let installers = stub_installers();
        match resolve_installer(&installers, PROTOCOL_VERSION, "no-such-installer") {
            Ok(_) => panic!("unknown tag must be refused"),
            Err(msg) => {
                assert!(msg.contains("unknown builtin installer"));
                assert!(msg.contains("no-such-installer"));
            }
        }
    }

    #[test]
    fn resolve_installer_refuses_protocol_mismatch_before_tag_lookup() {
        let installers = stub_installers();
        match resolve_installer(&installers, PROTOCOL_VERSION + 1, "exarch-agent") {
            Ok(_) => panic!("a mismatched protocol version must be refused"),
            Err(msg) => assert!(msg.contains("protocol version mismatch")),
        }
    }
}

// ── Engine-session tests ────────────────────────────────────────────────
//
// A real `engine_session` on a thread over one end of a `WireChannel` pair, the
// test playing the host on the other; timing is poll-until with multi-second
// slack, the dev fleet including a jittery VM. `engine_session` faces this
// process's signals, so every test that dispatches must hold `REQUEST_SERIAL`
// against the siblings that raise or spend an ambient cancel cause.
#[cfg(test)]
mod engine_session_tests {
    use super::*;
    use crate::process::cancel::REQUEST_SERIAL;
    use crate::transport::{PROTOCOL_VERSION, TerminalEndpoint};

    const WAIT: Duration = Duration::from_secs(20);

    fn boot() -> Shell {
        static PRELUDE: std::sync::OnceLock<crate::boot::BakedPrelude> = std::sync::OnceLock::new();
        crate::boot::boot_shell(
            crate::io::TerminalState::default(),
            PRELUDE.get_or_init(crate::boot::BakedPrelude::bake_runtime),
            &crate::boot::HostSurface::default(),
        )
    }

    static INSTALLERS: &[EngineInstaller] = &[EngineInstaller { tag: "test", boot }];

    /// One capturing run under the ⊤ capability ceiling.
    fn run(src: &str) -> Run {
        Run {
            program: crate::transport::Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: crate::types::Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: crate::run::RunIo::Capture,
            terminal: crate::run::RequestedTerminalAccess::Denied,
            stdin: crate::run::RunStdin::Empty,
            trail: None,
        }
    }

    /// `seen` holds the frames read past while awaiting a particular one, so a
    /// later await can still find them.
    struct Host {
        ch: WireChannel,
        seen: Vec<Frame>,
        engine: std::thread::JoinHandle<i32>,
    }

    fn start() -> Host {
        start_with(Patience::default())
    }

    fn start_with(patience: Patience) -> Host {
        let (host_ch, engine_ch) = WireChannel::pair().expect("socketpair");
        let engine = std::thread::spawn(move || engine_session(engine_ch, INSTALLERS, patience));
        let mut host = Host {
            ch: host_ch,
            seen: Vec::new(),
            engine,
        };
        host.send(&Frame::Attach {
            endpoint: TerminalEndpoint {
                lease: None,
                state: crate::io::TerminalState::default(),
            },
            #[allow(
                clippy::disallowed_methods,
                reason = "[io-door:test] attach with the test process's own cwd/HOME so the engine's restore is a no-op"
            )]
            cwd: std::env::current_dir().expect("test cwd"),
            #[allow(
                clippy::disallowed_methods,
                reason = "[io-door:test] see cwd above"
            )]
            home: std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from),
            rc_path: None,
            proto_version: PROTOCOL_VERSION,
            installer: "test".into(),
        });
        host
    }

    impl Host {
        fn send(&mut self, frame: &Frame) {
            self.ch.write_frame(frame).expect("write to engine");
        }

        fn dispatch(&mut self, id: u64, src: &str) {
            self.send(&Frame::Dispatch(DispatchId(id), Box::new(run(src))));
        }

        /// Every frame that does not match is buffered, not discarded.
        fn await_frame(&mut self, pred: impl Fn(&Frame) -> bool) -> Frame {
            if let Some(i) = self.seen.iter().position(&pred) {
                return self.seen.remove(i);
            }
            let deadline = Instant::now() + WAIT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    self.ch.poll_readable(Some(left)).expect("poll engine"),
                    "awaited frame must arrive within {WAIT:?}; buffered: {:?}",
                    self.seen
                );
                let frame = self
                    .ch
                    .read_frame()
                    .expect("read from engine")
                    .expect("engine hung up mid-await");
                if pred(&frame) {
                    return frame;
                }
                self.seen.push(frame);
            }
        }

        fn report(&mut self, id: u64) -> Report {
            let frame = self.await_frame(
                |f| matches!(f, Frame::Event(d, Event::Report(_)) if *d == DispatchId(id)),
            );
            let Frame::Event(_, Event::Report(report)) = frame else {
                unreachable!("await_frame matched a Report");
            };
            report
        }

        fn run(&mut self, id: u64, src: &str) -> Report {
            self.dispatch(id, src);
            self.report(id)
        }

        fn cancel(&mut self, id: u64) {
            self.send(&Frame::Control(Control::Cancel(DispatchId(id))));
        }

        fn detach_and_join(mut self) -> i32 {
            self.send(&Frame::Detach);
            self.engine.join().expect("engine thread")
        }
    }

    fn ran_int(report: &Report) -> i64 {
        match report {
            Report::Ran {
                ending:
                    crate::transport::Ending::Settled {
                        value: FOValue::Int { value },
                        ..
                    },
                ..
            } => *value,
            other => panic!("expected Ran Ok Int, got {other:?}"),
        }
    }

    fn is_engine_busy(report: &Report) -> bool {
        matches!(report, Report::Static {
            diagnostics: crate::transport::Diagnostics::Host(msg),
        } if msg == "engine busy")
    }

    #[test]
    fn dispatch_round_trips_to_a_report() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        assert_eq!(ran_int(&host.run(1, "$[1 + 1]")), 2);
        assert_eq!(host.detach_and_join(), 0);
    }

    /// One rendezvous, so one refusal arm for both riders.
    #[test]
    fn busy_refuses_a_second_dispatch_and_a_probe() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        host.dispatch(1, "sleep 15");
        assert!(is_engine_busy(&host.run(2, "$[1 + 1]")));
        host.send(&Frame::Probe(
            DispatchId(3),
            FOValue::Variant {
                label: "cwd".into(),
                payload: None,
            },
        ));
        assert!(is_engine_busy(&host.report(3)));
        assert_eq!(host.detach_and_join(), 0, "teardown cancels the sleep");
    }

    /// `sleep 30` proves promptness: it could not report inside the await
    /// ceiling on its own. Cancelling with no delay proves the launch race is
    /// closed — the scope exists before the worker has the work.
    #[test]
    fn cancel_settles_an_in_flight_run_promptly() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        host.dispatch(1, "sleep 30");
        host.cancel(1);
        host.report(1);
        assert_eq!(host.detach_and_join(), 0);
    }

    #[test]
    fn deferred_batch_crosses_as_a_session_frame() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        host.run(1, "let h = spawn { sleep 1 }");
        let frame = host.await_frame(|f| matches!(f, Frame::Session(..)));
        let Frame::Session(SessionEvent::DeferredSurface(batch)) = frame else {
            unreachable!("await_frame matched a Session frame");
        };
        match batch.last() {
            Some(FOValue::Variant { label, .. }) if label == "done" => {}
            other => panic!("expected the batch to end in a `done` record, got {other:?}"),
        }
        assert_eq!(host.detach_and_join(), 0);
    }

    /// A front-end that keeps pinging, dispatches a run whose surface value
    /// overflows the socket buffer, and then simply stops reading must be
    /// treated exactly like one gone silent: the engine's own write stalls
    /// past `patience.write_stall`, and that — not a graceful EOF — is what
    /// ends the session. `engine_session` must return on its own, and with
    /// `1`, never wedged in its reader loop forever.
    #[test]
    fn a_front_end_that_stops_reading_is_treated_as_dead() {
        let _g = REQUEST_SERIAL.lock();
        let brisk = Patience {
            silence: Duration::from_millis(500),
            write_stall: Duration::from_millis(300),
        };
        let mut host = start_with(brisk);
        host.send(&Frame::Ping(1));

        let payload = "x".repeat(4 * 1024 * 1024);
        host.dispatch(1, &format!("let big = \"{payload}\"\nsurface `data $big\n"));

        // No further read: the host abandons the connection exactly like a
        // dead peer would, and never drains the surface write the engine now
        // owes it.
        let deadline = Instant::now() + WAIT;
        while !host.engine.is_finished() {
            assert!(
                Instant::now() < deadline,
                "engine_session must return once its surface write stalls past patience.write_stall"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let code = host.engine.join().expect("engine thread");
        assert_eq!(
            code, 1,
            "a front-end that stopped reading must exit 1, not the clean-detach 0"
        );
    }
}
