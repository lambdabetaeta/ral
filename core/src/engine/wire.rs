//! The wire carrier: a connection-lived engine process, running what the
//! front-end dispatches over a framed socket.
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

use super::{Engine, EngineInstaller, Outlet, Rails};
use crate::process::{CancelCause, ForegroundScope};
use crate::protocol::{
    Attach, Control, DispatchId, EnquiryError, EnquiryId, Event, Frame, Report, Run, SessionEvent,
};
use crate::serial::FOValue;
use crate::sync::LockExt;
use crate::types::{DeferredSink, EnquiryDesk, Error, Fork};
use crate::wire::WireChannel;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// The engine's half of the severance law: [`crate::wire::write_or_sever`]
/// recording into `fault` — the flag the reader loop consults on exit so a
/// front-end that stopped reading is never mistaken for one that cleanly
/// detached.
///
/// Every engine-side write crosses here: a surface emit, a deferred batch, an
/// enquiry, a report, a `Pong`. None of them may park the reader loop, so a
/// stalled write becomes exactly the same fatal, non-blocking event a severed
/// pipe already is.
fn engine_write(writer: &Mutex<WireChannel>, fault: &AtomicBool, frame: &Frame) -> io::Result<()> {
    crate::wire::write_or_sever(writer, frame, |_| fault.store(true, Ordering::SeqCst))
}

/// A detached worker's batch outlives the run that spawned it, so it has no
/// dispatch to stamp and rides `Frame::Session` instead.
struct ChannelDeferredSink {
    writer: Arc<Mutex<WireChannel>>,
    fault: Arc<AtomicBool>,
}

impl DeferredSink for ChannelDeferredSink {
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
pub(crate) const HOST_SILENCE_DEADLINE: Duration = Duration::from_secs(30);

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
/// can never wedge the exit. The bound is paid for in cleanup: the exit runs no
/// destructor on the worker's thread, so an atomic write still in flight at the
/// bound leaves its staged `.ral-write.tmp` sibling behind.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

const SETTLE_POLL: Duration = Duration::from_millis(50);

/// Every live enquiry park, keyed by the id its answer will carry. Session
/// wide, so no id repeats across dispatches: a late answer to a park that gave
/// up finds nothing, never a successor's park. Every mutation is a whole-entry
/// insert or remove, so poison is recovered ([`LockExt`]) rather than
/// propagated.
#[derive(Default)]
struct Parks {
    next: AtomicU64,
    parked: Mutex<HashMap<EnquiryId, mpsc::SyncSender<Result<FOValue, EnquiryError>>>>,
}

impl Parks {
    /// Called from the reader loop. An answer whose park has gone finds no
    /// sender, or races its receiver's drop and fails to send; either way it
    /// is dropped.
    fn fill(&self, eid: EnquiryId, answer: Result<FOValue, EnquiryError>) {
        let park = self.parked.lock_ignore_poison().remove(&eid);
        if let Some(tx) = park {
            let _ = tx.send(answer);
        }
    }
}

/// One dispatch's enquiry desk. `enquire` mints an [`EnquiryId`], sends
/// `Event::Enquiry`, and parks until the front-end's `Frame::Answer` fills it
/// — or the run's own cancel scope fires, polled at the receive timeout.
struct WireDesk {
    id: DispatchId,
    outlet: Outlet,
    parks: Arc<Parks>,
}

impl EnquiryDesk for WireDesk {
    fn enquire(
        &self,
        req: FOValue,
        cancel: &crate::process::CancelScope,
    ) -> Result<FOValue, Error> {
        let eid = EnquiryId(self.parks.next.fetch_add(1, Ordering::Relaxed));
        // Registered before the send: the answer may be back before this
        // thread reaches the park below.
        let (tx, rx) = mpsc::sync_channel(1);
        self.parks.parked.lock_ignore_poison().insert(eid, tx);

        if !(self.outlet)(self.id, Event::Enquiry(eid, req)) {
            self.parks.parked.lock_ignore_poison().remove(&eid);
            return Err(Error::new("enquiry lost: the host connection is down", 1));
        }

        // The sender leaves `parked` only into `fill`'s send or this park's own
        // exit, so a disconnect can only trail an answer already returned.
        const ORPHANED: &str = "an enquiry's sender outlives its park";

        // Answer, then cancel, then park — in that order, so an answer already
        // in hand outranks a cancel pending beside it, and an enquiry raised
        // under an already-cancelled scope returns without parking at all. The
        // park mints its error through `Error::cancelled`, as every poll point
        // does.
        let deliver = |answer: Result<FOValue, EnquiryError>| {
            answer.map_err(|e| Error::new(e.message, e.status))
        };
        loop {
            match rx.try_recv() {
                Ok(answer) => return deliver(answer),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => unreachable!("{ORPHANED}"),
            }
            if let Some(cause) = cancel.cause() {
                self.parks.parked.lock_ignore_poison().remove(&eid);
                return Err(Error::cancelled(cause));
            }
            match rx.recv_timeout(ENQUIRY_CANCEL_POLL) {
                Ok(answer) => return deliver(answer),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!("{ORPHANED}"),
            }
        }
    }
}

/// The engine's rendezvous, held for one run or one probe. Winning a claim is
/// the only way to mint one and only a `Dispatch` rides the channel, so the
/// busy flag can never stand raised without work behind it.
struct Dispatch {
    id: DispatchId,
    busy: Arc<AtomicBool>,
}

impl Dispatch {
    fn claim(busy: &Arc<AtomicBool>, id: DispatchId) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                id,
                busy: busy.clone(),
            })
    }
}

/// The one place the flag is lowered, so no path can skip it — including an
/// unwind out of the worker. Only infallible work belongs here: a panic raised
/// during an unwind aborts the process.
impl Drop for Dispatch {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

/// Raised while the worker holds an item, lowered once that item's Report is
/// on the wire. Admission ([`Dispatch`]) and this are different questions: a
/// claim is released *before* its own report write, so only this answers the
/// teardown's — is the worker still producing frames? A guard, like the claim,
/// so an unwind cannot leave it raised.
struct Writing(Arc<AtomicBool>);

impl Writing {
    fn raise(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag.clone())
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
pub fn run_engine(installers: &'static [EngineInstaller]) -> ! {
    // SAFETY: fd 3 is the socket inherited from the front-end
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    // The handoff must leave fd 3 open across exec, so set CLOEXEC the instant
    // the engine owns it: no external command a run spawns may inherit the wire.
    if let Err(e) = rustix::io::fcntl_setfd(&stream, rustix::io::FdFlags::CLOEXEC) {
        eprintln!("engine: failed to set CLOEXEC on the wire fd: {e}");
        #[allow(
            clippy::disallowed_methods,
            reason = "the process has adopted fd 3 and nothing else: no shell is booted, so there is no lease, no watched child and no staged write to unwind"
        )]
        std::process::exit(1);
    }
    let reader_ch = WireChannel::from_stream(stream);
    #[allow(
        clippy::disallowed_methods,
        reason = "`engine_session`'s teardown settle is the shutdown: it cancels the durable root, tears the hatched children down and waits the worker out before returning here. An engine's stdio is /dev/null, so its session mints no TerminalLease and no ForegroundGuard can exist to strand."
    )]
    std::process::exit(engine_session(reader_ch, installers, Patience::default()));
}

/// The process-level half of an attach, which only a carrier that owns its
/// process may perform. Both restores skip when the value already holds: an
/// in-process engine attaches with its host's own cwd and HOME, and must not
/// disturb either.
fn restore_process_dirs(attach: &Attach) {
    #[allow(
        clippy::disallowed_methods,
        reason = "engine cwd restore during Attach — sets engine process cwd, not Shell logical cwd"
    )]
    if std::env::current_dir().is_ok_and(|d| d != attach.cwd)
        && let Err(e) = std::env::set_current_dir(&attach.cwd)
    {
        eprintln!("engine: failed to set cwd to {}: {e}", attach.cwd.display());
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "engine HOME restore during Attach — process env, not Shell state"
    )]
    if std::env::var_os("HOME").as_deref() != Some(attach.home.as_os_str()) {
        // SAFETY: single-threaded engine startup, no other threads
        unsafe {
            std::env::set_var("HOME", &attach.home);
        }
    }
}

/// The engine's whole protocol life over one already-open channel: `Attach`
/// handshake, worker rendezvous, reader loop, teardown settle.
///
/// Returns the process exit code — `0` for a clean detach or EOF, `1` for a
/// protocol fault, a read error, a dead worker, silence past the deadline, or
/// a write that stalled past `patience.write_stall`: a front-end that stops
/// reading is, within that bound, treated exactly as one that has gone
/// silent. A refused attach — a version mismatch, an unknown installer tag, a
/// recipe or seed that would not boot — is answered `Refused` and exits `1`.
///
/// # Panics
/// Panics if the wire channel cannot be cloned for the writer.
fn engine_session(
    reader_ch: WireChannel,
    installers: &'static [EngineInstaller],
    patience: Patience,
) -> i32 {
    // A hatched parent starts writing only once this process exists, so the
    // seed is taken before the wait for Attach: a seed larger than the
    // socketpair's buffer would otherwise wedge parent and child.
    let seed = match crate::hatch::seed_from_env() {
        Ok(seed) => seed,
        Err(msg) => {
            eprintln!("engine: {msg}");
            return 1;
        }
    };

    let writer_ch = reader_ch.try_clone().expect("try_clone engine channel");
    writer_ch
        .set_write_deadline(patience.write_stall)
        .expect("set write deadline on engine channel");
    let writer = Arc::new(Mutex::new(writer_ch));
    let wire_fault = Arc::new(AtomicBool::new(false));
    let mut reader_ch = reader_ch;

    // Nothing but Attach is a legal first frame, so this is one read, not a
    // loop: the engine speaks no shell until told a version and an installer.
    let engine = match reader_ch.read_frame() {
        Ok(Some(Frame::Attach(attach))) => {
            restore_process_dirs(&attach);
            match Engine::boot(installers, &attach, seed) {
                Ok(engine) => engine,
                Err(msg) => {
                    eprintln!("engine: {msg}");
                    let _ = engine_write(
                        &writer,
                        &wire_fault,
                        &Frame::Session(SessionEvent::Refused(msg)),
                    );
                    return 1;
                }
            }
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

    // This process is the engine's own, so it hears the process's signals as
    // its own `Control`, the way an identity host forwards them.
    let _signals = {
        let scopes = engine.scopes().clone();
        crate::process::forward_ambient(move |ambient| {
            scopes.apply(Control::hearing(ambient));
        })
    };

    // The engine now takes dispatches, and the front-end's `await_attached`
    // unblocks.
    let _ = engine_write(
        &writer,
        &wire_fault,
        &Frame::Session(SessionEvent::Attached),
    );

    // A probe rides the same rendezvous as a run, so it serialises with
    // dispatches for free: sent mid-run it gets "engine busy", the same arm a
    // second dispatch gets.
    enum WorkItem {
        /// Boxed so a probe is not sized to `Run`'s stack footprint. The scope
        /// is the one its frame is born under.
        Run(Box<Run>, ForegroundScope),
        Probe(FOValue),
    }

    // Taken before the engine moves into the worker: `Control` and the
    // teardown reach its runs from this thread.
    let scopes = engine.scopes().clone();
    // Lowered by the claimed `Dispatch`'s `Drop`, right before the worker
    // writes its Report, so a probe or dispatch the front-end sends the
    // instant it has the Report in hand is never refused as busy.
    let busy = Arc::new(AtomicBool::new(false));
    // The other half of the teardown's span: raised from the moment the worker
    // takes an item until its Report is written.
    let writing = Arc::new(AtomicBool::new(false));
    let (run_tx, run_rx) = mpsc::channel::<(Dispatch, WorkItem)>();
    let outlet: Outlet = {
        let (writer, fault) = (writer.clone(), wire_fault.clone());
        Arc::new(move |id, event| engine_write(&writer, &fault, &Frame::Event(id, event)).is_ok())
    };
    let parks = Arc::new(Parks::default());

    // ── Worker thread: owns the engine ─────────────────────────────
    {
        let (outlet, parks, writing) = (outlet.clone(), parks.clone(), writing.clone());
        let deferred: Arc<dyn DeferredSink> = Arc::new(ChannelDeferredSink {
            writer: writer.clone(),
            fault: wire_fault.clone(),
        });
        std::thread::spawn(move || {
            let mut engine = engine;
            while let Ok((claim, item)) = run_rx.recv() {
                let _writing = Writing::raise(&writing);
                let id = claim.id;
                // `Shell::run` already catches, rolls back, and reports a panic
                // in the run itself; this outer catch is for one escaping the
                // report plumbing or a probe, either of which would otherwise
                // kill the thread unreported.
                let report =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match item {
                        WorkItem::Run(run, scope) => {
                            let rails = Rails {
                                outlet: outlet.clone(),
                                deferred: Some(deferred.clone()),
                                desk: Arc::new(WireDesk {
                                    id,
                                    outlet: outlet.clone(),
                                    parks: parks.clone(),
                                }),
                                fork: Fork::Listen,
                            };
                            engine.run(id, *run, rails, &scope)
                        }
                        WorkItem::Probe(reading) => {
                            crate::protocol::reading::report(engine.probe(&reading))
                        }
                    }))
                    .unwrap_or_else(|_| {
                        Report::host_fault("engine: dispatch panicked in the engine worker")
                    });

                // `claim` drops before this, never after: the same thread
                // writes this Report and any later run's frames, so the wire
                // order is unchanged, and a probe the front-end sends on
                // receiving the Report is never refused "engine busy".
                drop(claim);
                outlet(id, Event::Report(report));
            }
        });
    }

    // Only the frame that wins the `false → true` flip may hand the worker
    // work; the rest are refused here, leaving the in-flight run's scope
    // standing.
    let admit = |id: DispatchId| {
        let claim = Dispatch::claim(&busy, id);
        if claim.is_none() {
            outlet(id, Event::Report(Report::host_fault("engine busy")));
        }
        claim
    };

    // ── Reader loop (this thread) ──────────────────────────────────
    /// How the reader loop ended — the distinction the parent needs, named
    /// rather than spelled as an exit code, so no break site can report a
    /// corrupted session as one that ended on request.
    enum SessionEnd {
        /// `Detach`, or the front-end's EOF.
        Requested,
        Corrupt,
    }

    let mut armed = false;
    // Silence counted in ticks this loop observed, never in elapsed clock: a
    // suspended guest resumes having watched one tick, not the thousands its
    // clock ran through, so waking is not the front-end's death.
    let mut silent_ticks: u32 = 0;

    let end = loop {
        // Armed, park on a `TICK` so silence past the deadline is noticed;
        // unarmed, block in `read_frame`, that front-end's death being EOF.
        let read = if armed {
            match reader_ch.poll_readable(Some(TICK)) {
                Ok(true) => reader_ch.read_frame(),
                Ok(false) => {
                    silent_ticks += 1;
                    if TICK.saturating_mul(silent_ticks) >= patience.silence {
                        eprintln!(
                            "engine: front-end silent for {}s — failing the in-flight run and exiting",
                            patience.silence.as_secs()
                        );
                        break SessionEnd::Corrupt;
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
            Ok(None) => break SessionEnd::Requested, // EOF: gone, but cleanly
            Err(e) => {
                eprintln!("engine: read error: {e}");
                break SessionEnd::Corrupt;
            }
        };
        // Any frame at all is proof of life, not just a `Ping`.
        silent_ticks = 0;

        // A closed work channel is a dead worker — an unwind lowers the claim
        // on its way out — so nothing will report what was just admitted:
        // corruption, not a requested end.
        match frame {
            Frame::Dispatch(id, run) => {
                if let Some(claim) = admit(id)
                    && run_tx
                        .send((claim, WorkItem::Run(run, scopes.open(id))))
                        .is_err()
                {
                    break SessionEnd::Corrupt;
                }
            }
            Frame::Probe(id, reading) => {
                if let Some(claim) = admit(id)
                    && run_tx.send((claim, WorkItem::Probe(reading))).is_err()
                {
                    break SessionEnd::Corrupt;
                }
            }
            Frame::Answer(eid, answer) => parks.fill(eid, answer),
            Frame::Control(control) => scopes.apply(control),
            Frame::Ping(seq) => {
                armed = true;
                let _ = engine_write(&writer, &wire_fault, &Frame::Pong(seq));
            }
            Frame::Detach => break SessionEnd::Requested,
            out_of_protocol @ (Frame::Attach(_)
            | Frame::Event(..)
            | Frame::Session(..)
            | Frame::Pong(_)) => {
                eprintln!(
                    "engine: the front-end sent a {} frame, which it never sends once attached \
                     — ending the session",
                    out_of_protocol.kind()
                );
                break SessionEnd::Corrupt;
            }
        }
    };

    // ── Teardown settle ────────────────────────────────────────────
    // The loop may have exited with a run in flight: cancel it and every
    // worker under the durable root, then wait.
    scopes.end(CancelCause::Explicit);
    crate::hatch::teardown_hatched();
    // Claimed *or* still writing: a claim is released ahead of its own report
    // write, so neither flag alone spans the work a settle must wait out.
    let settling = || writing.load(Ordering::Acquire) || busy.load(Ordering::Acquire);
    let settle_by = Instant::now() + SETTLE_TIMEOUT;
    while settling() && Instant::now() < settle_by {
        std::thread::sleep(SETTLE_POLL);
    }
    // A severed wire is never the requested end it may look like: a front-end
    // that stopped reading loses its in-flight run exactly as one that fell
    // silent does, and the exit code must say so.
    match end {
        SessionEnd::Requested if !wire_fault.load(Ordering::SeqCst) => 0,
        _ => 1,
    }
}

// ── Wire desk tests ───────────────────────────────────────────────────
//
// A peer `WireChannel` end plays the front-end and `fill` is called by hand, as
// the reader loop calls it; each test states its own scope, so none touch
// process-global state. A real wire child is out of reach here: an engine
// child is its host binary re-exec'd with `--engine`, a flag only the host
// binaries handle, so a core test binary would re-run the harness.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
mod wire_desk_tests {
    use super::*;
    use crate::process::{CancelCause, CancelScope};

    /// A desk for dispatch `id` whose events leave on `ours`.
    fn desk_over(ours: WireChannel, id: u64) -> WireDesk {
        let (writer, fault) = (Mutex::new(ours), AtomicBool::new(false));
        WireDesk {
            id: DispatchId(id),
            outlet: Arc::new(move |id, event| {
                engine_write(&writer, &fault, &Frame::Event(id, event)).is_ok()
            }),
            parks: Arc::default(),
        }
    }

    /// The enquiry is stamped with the in-flight dispatch, and the answer
    /// `fill` delivers is what `enquire` returns.
    #[test]
    fn enquire_round_trips_through_the_rendezvous() {
        let (ours, mut peer) = WireChannel::pair().expect("socketpair");
        let desk = Arc::new(desk_over(ours, 7));

        let filler = desk.clone();
        let front_end = std::thread::spawn(move || {
            let frame = peer.read_frame().expect("read").expect("open");
            let Frame::Event(did, Event::Enquiry(eid, req)) = frame else {
                panic!("expected Event::Enquiry, got {frame:?}");
            };
            assert_eq!(did, DispatchId(7), "stamped with the in-flight dispatch");
            assert_eq!(req, FOValue::Int { value: 41 });
            filler.parks.fill(eid, Ok(FOValue::Int { value: 42 }));
        });

        let answer = desk.enquire(FOValue::Int { value: 41 }, &CancelScope::default());
        front_end.join().expect("front-end thread");
        assert_eq!(answer.expect("answered"), FOValue::Int { value: 42 });
        assert!(
            desk.parks.parked.lock().unwrap().is_empty(),
            "an answered park is deregistered"
        );
    }

    /// A refusal raises with the front-end's own message and status, so an
    /// enquiry fails alike under either transport.
    #[test]
    fn refused_enquiry_raises_message_and_status() {
        let (ours, mut peer) = WireChannel::pair().expect("socketpair");
        let desk = Arc::new(desk_over(ours, 1));

        let filler = desk.clone();
        let front_end = std::thread::spawn(move || {
            let frame = peer.read_frame().expect("read").expect("open");
            let Frame::Event(_, Event::Enquiry(eid, _)) = frame else {
                panic!("expected Event::Enquiry, got {frame:?}");
            };
            filler.parks.fill(eid, Err(EnquiryError::no_desk()));
        });

        let err = desk
            .enquire(FOValue::Unit, &CancelScope::default())
            .expect_err("refused");
        front_end.join().expect("front-end thread");
        assert_eq!(err.message, crate::types::NO_DESK);
        assert_eq!(
            err.status,
            crate::types::Status::Raised(crate::types::NO_DESK_STATUS)
        );
    }

    fn lone_desk() -> (WireDesk, WireChannel) {
        let (ours, peer) = WireChannel::pair().expect("socketpair");
        (desk_over(ours, 1), peer)
    }

    /// A cancel raised *while* the enquiry is parked wakes it at the next poll
    /// tick and deregisters it, so the answer that never came has nowhere to
    /// land.
    #[test]
    fn cancel_wakes_a_parked_enquiry() {
        let (desk, _peer) = lone_desk();
        let scope = CancelScope::default();

        // Long enough that `enquire` is provably parked before the cancel
        // lands, so this test cannot pass by the pre-park check alone.
        let lead = ENQUIRY_CANCEL_POLL * 2;
        let canceller = {
            let scope = scope.clone();
            std::thread::spawn(move || {
                std::thread::sleep(lead);
                scope.cancel(CancelCause::Explicit);
            })
        };

        let started = std::time::Instant::now();
        let err = desk.enquire(FOValue::Unit, &scope).expect_err("cancelled");
        let waited = started.elapsed();
        canceller.join().expect("canceller thread");

        assert_eq!(err.message, "cancelled");
        assert!(
            waited >= lead,
            "the enquiry parked until the cancel: {waited:?}"
        );
        assert!(
            desk.parks.parked.lock().unwrap().is_empty(),
            "a cancelled park deregisters itself"
        );

        // `EnquiryId(0)` is the one this park minted and abandoned.
        desk.parks.fill(EnquiryId(0), Ok(FOValue::Unit));
        assert!(desk.parks.parked.lock().unwrap().is_empty());
    }

    /// An enquiry raised under an already-cancelled scope answers at once: it
    /// checks the cancel before it parks, so it never waits out a poll tick.
    #[test]
    fn cancel_before_the_enquiry_never_parks() {
        let (desk, _peer) = lone_desk();
        let scope = CancelScope::default();
        scope.cancel(CancelCause::Explicit);

        let started = std::time::Instant::now();
        let err = desk.enquire(FOValue::Unit, &scope).expect_err("cancelled");
        let waited = started.elapsed();

        assert_eq!(err.message, "cancelled");
        assert!(
            waited < ENQUIRY_CANCEL_POLL,
            "returned without parking for a tick: {waited:?}"
        );
        assert!(
            desk.parks.parked.lock().unwrap().is_empty(),
            "a cancelled park deregisters itself"
        );
    }

    #[test]
    fn late_answer_for_a_dead_id_is_dropped() {
        let (desk, _peer) = lone_desk();
        desk.parks.fill(EnquiryId(99), Ok(FOValue::Unit));
        assert!(
            desk.parks.parked.lock().unwrap().is_empty(),
            "an unknown id must not mint a park"
        );
    }
}

// ── Engine-session tests ────────────────────────────────────────────────
//
// A real `engine_session` on a thread over one end of a `WireChannel` pair, the
// test playing the host on the other; timing is poll-until with multi-second
// slack, the dev fleet including a jittery VM. `engine_session` hears this
// process's signals, so every test that runs one must hold `REQUEST_SERIAL`
// against the siblings that raise an ambient cause.
#[cfg(test)]
mod engine_session_tests {
    use super::*;
    use crate::engine::Booted;
    use crate::engine::testkit::run;
    use crate::process::cancel::REQUEST_SERIAL;

    const WAIT: Duration = Duration::from_secs(20);

    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
    )]
    fn boot(_attach: &Attach) -> Result<Booted, String> {
        static PRELUDE: std::sync::OnceLock<crate::boot::BakedPrelude> = std::sync::OnceLock::new();
        Ok(Booted {
            shell: crate::boot::boot_shell(
                crate::io::TerminalState::default(),
                PRELUDE.get_or_init(crate::boot::BakedPrelude::bake_runtime),
                &crate::boot::HostSurface::default(),
            ),
            keep: Box::new(()),
        })
    }

    static INSTALLERS: &[EngineInstaller] = &[crate::engine::testkit::installer("test", boot)];

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
        #[allow(
            clippy::disallowed_methods,
            reason = "[test] attach with the test process's own cwd/HOME so the engine's restore is a no-op"
        )]
        host.send(&Frame::Attach(Attach::new(
            "test",
            std::env::current_dir().expect("test cwd"),
            std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from),
        )));
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
                    crate::protocol::Ending::Settled {
                        value: FOValue::Int { value },
                        ..
                    },
                ..
            } => *value,
            other => panic!("expected Ran Ok Int, got {other:?}"),
        }
    }

    fn is_engine_busy(report: &Report) -> bool {
        matches!(report, Report::Static { rendered, .. } if rendered.contains("engine busy"))
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

    /// The engine's process is its own, so it hears that process's signals as
    /// its own `Control`: an interrupt ends the run in flight, not the session.
    /// Raised until the report lands, since the host cannot see the run open.
    #[test]
    fn a_process_interrupt_unwinds_the_wire_engines_run() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        assert_eq!(ran_int(&host.run(1, "$[1 + 1]")), 2);
        host.dispatch(2, "sleep 30");
        let report = loop {
            crate::process::request_interrupt();
            if host
                .ch
                .poll_readable(Some(Duration::from_millis(50)))
                .expect("poll engine")
            {
                break host.report(2);
            }
        };
        assert!(
            matches!(&report, Report::Ran { ending, .. } if ending.status() == 130),
            "an interrupted run reports 130, got {report:?}"
        );
        assert_eq!(ran_int(&host.run(3, "$[1 + 1]")), 2, "the session lives on");
        assert_eq!(host.detach_and_join(), 0);
    }

    /// A SIGTERM ends the wire engine's session: the root request latches, so
    /// the run started after it is cancelled too.
    #[test]
    fn a_process_terminate_ends_the_wire_engines_session() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        assert_eq!(ran_int(&host.run(1, "$[1 + 1]")), 2);
        crate::process::request_root_cancel(CancelCause::Terminate);
        let report = host.run(2, "sleep 30");
        crate::process::cancel::clear_root_request();
        assert!(
            matches!(&report, Report::Ran { ending, .. } if ending.status() == 143),
            "a terminated run reports 143, got {report:?}"
        );
        assert_eq!(host.detach_and_join(), 0);
    }

    #[test]
    fn deferred_batch_crosses_as_a_session_frame() {
        let _g = REQUEST_SERIAL.lock();
        let mut host = start();
        host.run(1, "let h = spawn { sleep 1 }");
        let frame =
            host.await_frame(|f| matches!(f, Frame::Session(SessionEvent::DeferredSurface(_))));
        let Frame::Session(SessionEvent::DeferredSurface(batch)) = frame else {
            unreachable!("await_frame matched a deferred batch");
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
