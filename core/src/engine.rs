//! Engine process: the connection-lived child that holds the Shell
//! and executes turns sent over a framed socket by the front-end.
//!
//! # Liveness law
//! Any received frame is proof the front-end lives; heartbeats exist only
//! to manufacture that proof when the engine is idle (the law on
//! `Frame::Ping`). Here it makes the deadline opt-in: the first `Ping`
//! arms a read deadline — a pinging front-end has promised to keep
//! pinging, so silence past [`HOST_SILENCE_DEADLINE`] can only be its
//! death — while a front-end that never pings leaves the engine's patience
//! infinite, its death arriving as a kernel-guaranteed EOF, not silence.
//!
//! # Durability law
//! A teardown never abandons a running turn. Whether the peer died silent,
//! the read faulted, or the front-end detached, the loop first cancels the
//! in-flight turn and the shell's durable root, then waits — bounded — for
//! the worker to settle its turn (write its `Report` and re-park) before
//! the process exits. Nothing the turn spawned is left running when the
//! engine is gone.

use std::collections::HashMap;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::process::CancelCause;
use crate::serial::FOValue;
use crate::transport::{
    Control, DispatchId, EnquiryError, EnquiryId, Event, Frame, Report, Turn, answer_probe,
};
use crate::types::{DeferredSink, EnquiryDesk, Error, Shell, SurfaceSink};
use crate::wire::WireChannel;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

/// One compiled-in shell-parity target the engine child can be told, at
/// `Attach`, to become: the whole boot recipe to run.
///
/// Both halves of the single binary
/// (exarch, the REPL) pass their own table; the tag on the wire is the
/// only thing that names one, never the installer function itself —
/// code never crosses the seam.
pub struct EngineInstaller {
    /// The tag `Frame::Attach` carries — matched verbatim against the
    /// installer table this process was started with.
    pub tag: &'static str,
    /// The whole boot recipe for the engine's one shell — prelude, host
    /// surface, libraries, env seeding, ledger arming. Run once, at Attach.
    pub boot: fn() -> Shell,
}

/// A surface sink that writes `Event::Surface` frames live to the wire
/// as values are produced, rather than buffering. Built once, before the
/// worker spawns; the in-flight dispatch id is read at emit time from the
/// shared `current_dispatch`.
struct ChannelSurfaceSink {
    current_dispatch: Arc<AtomicU64>,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::EventSink for ChannelSurfaceSink {
    fn emit(&self, ev: &FOValue) {
        let id = DispatchId(self.current_dispatch.load(Ordering::Relaxed));
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(id, Event::Surface(ev.clone())));
    }
}

/// A deferred sink that writes `Event::DeferredSurface` frames to the wire.
/// Built once, before the worker spawns: a batch settling between turns is
/// stamped id 0, the identity transport's already-lawful shape.
struct ChannelDeferredSink {
    current_dispatch: Arc<AtomicU64>,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::DeferredSink for ChannelDeferredSink {
    fn deliver(&self, batch: Vec<FOValue>) {
        let id = DispatchId(self.current_dispatch.load(Ordering::Relaxed));
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(id, Event::DeferredSurface(batch)));
    }
}

/// Cadence at which a parked enquiry re-checks the foreground cancel slot.
/// `Control::Cancel` arrives on the reader thread and trips the published
/// foreground scope, never this rendezvous — the park must poll, and this
/// bounds how stale a cancel can go unnoticed.
const ENQUIRY_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(75);

/// How long the reader parks in `poll_readable` before waking to re-check
/// the silence deadline. Small enough that a death is noticed within a
/// second of the deadline lapsing, large enough that an idle engine is not
/// spinning.
const TICK: Duration = Duration::from_secs(1);

/// The stretch of unbroken silence, once armed, that the engine reads as
/// the front-end's death. A generous multiple of the host's default 5s
/// ping interval — six missed pings — so that no amount of scheduling
/// jitter on either side can fake a death; only a genuinely gone peer
/// stays silent this long.
const HOST_SILENCE_DEADLINE: Duration = Duration::from_secs(30);

/// The ceiling on the teardown settle: how long the loop waits for the
/// worker to fail its in-flight turn and re-park before the engine exits
/// regardless. Bounds a teardown against a turn that ignores cancellation,
/// so a dead peer can never wedge the exit.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cadence at which the teardown settle re-reads the busy flag.
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// The wire engine's enquiry desk: the desk impl *is* the codec (§3 of the
/// enquiry-channel ADR). `enquire` mints a fresh [`EnquiryId`], writes
/// `Event::Enquiry` up the wire inside the in-flight dispatch, and parks on
/// a rendezvous slot keyed by that id until the reader thread's
/// `Frame::Answer` arm fills it — or the turn's foreground cancel fires,
/// polled at the condvar wait's timeout.
struct WireDesk {
    writer: Arc<Mutex<WireChannel>>,
    /// The in-flight dispatch id, stamped by the worker before each turn so
    /// the enquiry frame correlates to the dispatch that raised it.
    current_dispatch: Arc<AtomicU64>,
    next_eid: AtomicU64,
    /// Outstanding rendezvous slots: `None` = parked awaiting an answer,
    /// `Some` = answered, to be taken by the parked thread. An answer for an
    /// id absent from the map (a cancelled park removed it) is dropped by id.
    slots: Mutex<HashMap<EnquiryId, Option<Result<FOValue, EnquiryError>>>>,
    answered: Condvar,
}

impl WireDesk {
    /// The reader thread's `Frame::Answer` arm: fill the slot and wake the
    /// parked enquirer. A late answer for a dead id — the park already
    /// returned cancelled and removed its slot — is dropped here by id.
    fn fill(&self, eid: EnquiryId, answer: Result<FOValue, EnquiryError>) {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&eid) {
            *slot = Some(answer);
            self.answered.notify_all();
        }
    }
}

impl EnquiryDesk for WireDesk {
    fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
        let id = DispatchId(self.current_dispatch.load(Ordering::Relaxed));
        let eid = EnquiryId(self.next_eid.fetch_add(1, Ordering::Relaxed));
        self.slots.lock().unwrap().insert(eid, None);

        if self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(id, Event::Enquiry(eid, req)))
            .is_err()
        {
            self.slots.lock().unwrap().remove(&eid);
            return Err(Error::new("enquiry lost: the host connection is down", 1));
        }

        // Park on the rendezvous. The enquiring thread is the worker running
        // the turn — the thread that published this turn's foreground scope —
        // so the global cancel slot polled here is exactly that scope, and a
        // `Control::Cancel` (relayed by the reader thread) wakes the park at
        // the next timeout tick. The cancellation message matches
        // `process::check`'s cause vocabulary, so the enquiring builtin
        // raises the same error every other cancelled poll point does.
        let mut slots = self.slots.lock().unwrap();
        loop {
            if let Some(Some(_)) = slots.get(&eid) {
                let answer = slots.remove(&eid).flatten().expect("slot just seen filled");
                return answer.map_err(|e| Error::new(e.message, e.status));
            }
            if let Some(cause) = crate::process::foreground_cancel_cause() {
                slots.remove(&eid);
                return Err(Error::new(cause.message(), cause.exit_code()));
            }
            slots = self
                .answered
                .wait_timeout(slots, ENQUIRY_CANCEL_POLL)
                .unwrap()
                .0;
        }
    }
}

/// Validate the front-end's protocol version and resolve its installer tag
/// against this binary's compiled-in table. Returned as a `Result` rather
/// than exiting directly so the refusal path is unit-testable without a
/// live wire child; `engine_session` is the only caller and turns an `Err`
/// into the loud exit both refusals share.
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

/// Write one correlated `Report` frame.
fn write_report(writer: &Mutex<WireChannel>, id: DispatchId, report: Report) {
    let _ = writer
        .lock()
        .unwrap()
        .write_frame(&Frame::Event(id, Event::Report(report)));
}

/// Run the engine loop on an inherited socket (fd 3).
/// The front-end passes the socket as fd 3 before exec.
///
/// The fd adoption and the final `exit` live here; the whole protocol —
/// handshake, worker, reader loop, teardown settle — is [`engine_session`],
/// separated so a test can drive a real engine over a [`WireChannel`] pair
/// without leaving the process. Never returns.
///
/// # Panics
/// Panics if the inherited wire channel cannot be cloned for the writer.
pub fn run_engine(installers: &[EngineInstaller]) -> ! {
    // SAFETY: fd 3 is the socket inherited from the front-end
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    // `dup2` (the front-end's pre_exec handoff) always clears CLOEXEC on
    // the duplicate it creates, so fd 3 arrives here open-across-exec by
    // necessity. Set it CLOEXEC now, the instant the engine owns it, so no
    // external command any turn spawns inherits the protocol socket.
    if let Err(e) = rustix::io::fcntl_setfd(&stream, rustix::io::FdFlags::CLOEXEC) {
        eprintln!("engine: failed to set CLOEXEC on the wire fd: {e}");
        std::process::exit(1);
    }
    let reader_ch = WireChannel::from_stream(stream);
    std::process::exit(engine_session(reader_ch, installers));
}

/// The engine's whole protocol life over one already-open channel.
///
/// `Attach` handshake (version + installer resolution, environment
/// restoration, shell boot), the single worker rendezvous, the reader loop
/// with its armed-by-first-ping patience, and the teardown settle. Returns
/// the process exit code — `0` for a clean detach or EOF, `1` for a
/// protocol fault, read error, or silence past the deadline.
///
/// `installers` is this binary's compiled-in table of shell-parity
/// targets; the tag `Attach` names is looked up here, never guessed —
/// an unrecognised tag refuses the session as loudly as a protocol
/// mismatch, since a wire engine speaking the wrong builtins is exactly
/// the incoherence this rail exists to rule out.
///
/// # Panics
/// Panics if the wire channel cannot be cloned for the writer.
pub fn engine_session(reader_ch: WireChannel, installers: &[EngineInstaller]) -> i32 {
    let writer_ch = reader_ch.try_clone().expect("try_clone engine channel");
    let writer = Arc::new(Mutex::new(writer_ch));
    let mut reader_ch = reader_ch;

    // ── Await Attach before anything else: the engine speaks no shell
    // until the front-end has named its protocol version and its
    // builtin installer. Nothing else is a legal first frame, so this is
    // a single read, not a loop.
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
            // Restore the session environment. Both restores skip when the
            // value already holds — an in-process engine (the test rig)
            // attaches with its own cwd/HOME, and must not mutate either.
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
            let _ = endpoint; // TODO Phase 2 Task 6: pass terminal fds via SCM_RIGHTS
            let _ = rc_path; // TODO: load rc_path — needs the host's RcCtx/plugin machinery, not a core-level concern yet

            #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
            let mut shell = (target.boot)();
            // Only ral-daemon's closed engine environment sets RAL_GUEST,
            // so this is the whole "am I inside a guest?" signal. Gated on
            // the env var, never on `installer`'s tag, so every boot
            // recipe is jailed alike without any bootstrap knowing jails
            // exist.
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

    // One item of work the rendezvous hands the worker: a whole turn, or a
    // pure boundary-time probe. Both ride the same rendezvous so a probe
    // serialises with dispatches for free — busy while a turn runs answers
    // "engine busy", the same arm a second dispatch gets.
    enum WorkItem {
        /// Boxed for the same reason `Frame::Dispatch` boxes it: a probe
        /// must not be sized to `Turn`'s stack footprint.
        Turn(DispatchId, Box<Turn>),
        Probe(DispatchId, FOValue),
    }

    // Captured before the shell moves into the worker thread: the teardown
    // settle must reach the shell's deferred workers without the shell in
    // hand.
    let root = shell.cancel_handle();

    // True while a work item is in flight. Claimed by CAS at the reader,
    // cleared by the worker only after its Report is written and it is
    // about to re-park — so "engine busy" reflects a turn genuinely in
    // flight, never a worker that has merely not yet re-parked.
    let busy = Arc::new(AtomicBool::new(false));
    let (turn_tx, turn_rx) = mpsc::channel::<WorkItem>();

    let current_dispatch = Arc::new(AtomicU64::new(0));
    let desk = Arc::new(WireDesk {
        writer: writer.clone(),
        current_dispatch: current_dispatch.clone(),
        next_eid: AtomicU64::new(1),
        slots: Mutex::new(HashMap::new()),
        answered: Condvar::new(),
    });
    // The engine's sinks: built once here, stamped per emission off the
    // shared `current_dispatch` — a boundary batch lands as id 0.
    let surface = Arc::new(ChannelSurfaceSink {
        current_dispatch: current_dispatch.clone(),
        writer: writer.clone(),
    });
    let deferred = Arc::new(ChannelDeferredSink {
        current_dispatch: current_dispatch.clone(),
        writer: writer.clone(),
    });

    // ── Worker thread: owns the Shell ──────────────────────────────
    let worker_writer = writer.clone();
    let worker_desk = desk.clone();
    let worker_current_dispatch = current_dispatch.clone();
    let worker_busy = busy.clone();
    std::thread::spawn(move || {
        let mut shell = shell;
        while let Ok(item) = turn_rx.recv() {
            match item {
                WorkItem::Turn(id, turn) => {
                    // Stamp the in-flight dispatch so the desk's enquiry
                    // frames correlate to the turn that raised them.
                    worker_current_dispatch.store(id.0, Ordering::Relaxed);

                    let req = crate::driver::TurnRequest {
                        turn: *turn,
                        surface: Some(surface.clone() as SurfaceSink),
                        deferred: Some(deferred.clone() as Arc<dyn DeferredSink>),
                        desk: Some(worker_desk.clone() as crate::types::Desk),
                        nursery: None,
                        lifecycle: Box::new(()),
                    };
                    // Liveness backstop only: a turn-time panic is already
                    // caught, rolled back, and reported inside
                    // `Shell::run_turn` itself. This outer catch exists for
                    // a panic escaping `into_report` or the report plumbing
                    // on this worker thread — under `panic = "unwind"` such
                    // a panic would otherwise tear the thread down silently,
                    // no `Report` frame would ever be written, and the
                    // front-end's `recv` would block forever.
                    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let turn_report = shell.run_turn(req);
                        turn_report.into_report(shell.sources())
                    }))
                    .unwrap_or_else(|_| Report::Static {
                        diagnostics: crate::transport::Diagnostics::Host(
                            "engine: turn report plumbing panicked".into(),
                        ),
                    });

                    worker_current_dispatch.store(0, Ordering::Relaxed);
                    write_report(&worker_writer, id, report);
                }
                WorkItem::Probe(id, reading) => {
                    let report = match answer_probe(&mut shell, &reading) {
                        Ok(v) => Report::Ran {
                            result: Ok(v),
                            status: 0,
                            single_command: false,
                            captured: None,
                            timed_out: false,
                        },
                        Err(message) => Report::Ran {
                            result: Err(crate::transport::Break::Error {
                                rendered: message,
                                command_exit: false,
                            }),
                            status: 1,
                            single_command: false,
                            captured: None,
                            timed_out: false,
                        },
                    };
                    write_report(&worker_writer, id, report);
                }
            }
            // The Report is written; only now — about to re-park on
            // `recv` — is the worker actually ready for the next item.
            worker_busy.store(false, Ordering::Release);
        }
    });

    // Claim the worker atomically: only the frame that flips `false → true`
    // may hand it work, so "busy" reflects a turn genuinely in flight,
    // never a worker that has merely not yet re-parked.
    let claim = |id: DispatchId, item: WorkItem| -> bool {
        if busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            turn_tx.send(item).is_ok()
        } else {
            write_report(
                &writer,
                id,
                Report::Static {
                    diagnostics: crate::transport::Diagnostics::Host("engine busy".into()),
                },
            );
            true
        }
    };

    // ── Reader loop (this thread) ──────────────────────────────────
    // The exit code distinguishes a clean detach from a protocol fault: a
    // parent that can no longer tell the two apart (both used to exit 0)
    // cannot know whether the session ended on request or on corruption.
    //
    // `armed` latches true on the first `Ping`; `last_frame` is when the last
    // frame of any kind arrived. See the module docs for the laws this loop
    // enforces.
    let mut armed = false;
    let mut last_frame = Instant::now();

    let exit_code = loop {
        // Armed → park on a `TICK` so silence past the deadline is noticed;
        // unarmed → block in `read_frame`, that front-end's death being EOF.
        let read = if armed {
            match reader_ch.poll_readable(Some(TICK)) {
                Ok(true) => reader_ch.read_frame(),
                Ok(false) => {
                    let silent = last_frame.elapsed();
                    if silent >= HOST_SILENCE_DEADLINE {
                        eprintln!(
                            "engine: front-end silent for {}s (deadline {}s) — failing the in-flight turn and exiting",
                            silent.as_secs(),
                            HOST_SILENCE_DEADLINE.as_secs()
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
            // A clean EOF: the front-end is gone. Exit clean, then settle.
            Ok(None) => break 0,
            Err(e) => {
                eprintln!("engine: read error: {e}");
                break 1;
            }
        };
        // Any received frame is proof of life — reset the silence clock.
        last_frame = Instant::now();

        match frame {
            Frame::Dispatch(id, turn) => {
                if !claim(id, WorkItem::Turn(id, turn)) {
                    break 0; // worker died
                }
            }
            Frame::Probe(id, reading) => {
                if !claim(id, WorkItem::Probe(id, reading)) {
                    break 0; // worker died
                }
            }
            Frame::Answer(_, eid, answer) => {
                // Correlated by EnquiryId alone: the dispatch id names the
                // turn for the front-end's benefit, but the slot is the
                // enquiry's. A late answer for a dead id drops in `fill`.
                desk.fill(eid, answer);
            }
            Frame::Control(Control::Cancel(did)) => {
                // Dispatch-precision guard: only the named, still-in-flight
                // dispatch is cancelled; a stale cancel is dropped.
                if did.0 != 0 && did.0 == current_dispatch.load(Ordering::Relaxed) {
                    crate::process::request_foreground_cancel(CancelCause::Explicit);
                }
            }
            Frame::Control(Control::Resize(_winsize)) => {
                // no-op for now, TODO task 7
            }
            Frame::Control(Control::Suspend | Control::Resume) => {}
            Frame::Ping(seq) => {
                armed = true;
                let _ = writer.lock().unwrap().write_frame(&Frame::Pong(seq));
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
        }
    };

    // ── Teardown settle (durability law) ───────────────────────────
    // The loop may exit with a turn in flight: cancel it, and the shell's
    // durable root (its deferred workers), then wait — bounded — for the
    // busy flag to clear. A clean Detach or EOF still exits 0; a silent
    // peer or read fault exits 1.
    crate::process::request_foreground_cancel(CancelCause::Explicit);
    root.cancel(CancelCause::Explicit);
    let settle_by = Instant::now() + SETTLE_TIMEOUT;
    while busy.load(Ordering::Acquire) && Instant::now() < settle_by {
        std::thread::sleep(SETTLE_POLL);
    }
    exit_code
}

// ── Wire desk tests ───────────────────────────────────────────────────
//
// The desk's rendezvous is driven in-process: a peer `WireChannel` end plays
// the front-end, and `WireDesk::fill` is called exactly as the reader loop's
// `Frame::Answer` arm calls it. A full wire-child integration test is not
// practical here — `WireTransport::new` re-execs the current binary with
// `--engine`, a flag only the host binaries handle, and a core unit-test
// binary would just re-run the test harness.
//
// Every test that parks an enquiry holds `SLOT_SERIAL`: the park polls the
// process-global foreground cancel slot, which other tests publish.
#[cfg(test)]
mod wire_desk_tests {
    use super::*;
    use crate::process::cancel::SLOT_SERIAL;
    use crate::process::{CancelCause, publish_foreground};

    /// The round-trip: `enquire` writes `Event::Enquiry` stamped with the
    /// in-flight dispatch, parks, and returns the answer `fill` delivers —
    /// the reader loop's `Answer` arm, driven by hand from the peer end.
    #[test]
    fn enquire_round_trips_through_the_rendezvous() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let (ours, mut peer) = WireChannel::pair().expect("socketpair");
        let desk = Arc::new(WireDesk {
            writer: Arc::new(Mutex::new(ours)),
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

        let answer = desk.enquire(FOValue::Int { value: 41 });
        front_end.join().expect("front-end thread");
        assert_eq!(answer.expect("answered"), FOValue::Int { value: 42 });
        assert!(
            desk.slots.lock().unwrap().is_empty(),
            "the answered slot is removed"
        );
    }

    /// An `EnquiryError` answer raises engine-side with the same message and
    /// status the front-end refused with — the both-transports error law.
    #[test]
    fn refused_enquiry_raises_message_and_status() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let (ours, mut peer) = WireChannel::pair().expect("socketpair");
        let desk = Arc::new(WireDesk {
            writer: Arc::new(Mutex::new(ours)),
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
            filler.fill(
                eid,
                Err(EnquiryError {
                    message: "this host answers no enquiries".into(),
                    status: 1,
                }),
            );
        });

        let err = desk.enquire(FOValue::Unit).expect_err("refused");
        front_end.join().expect("front-end thread");
        assert_eq!(err.message, "this host answers no enquiries");
        assert_eq!(err.status, crate::types::Status::Code(1));
    }

    /// A foreground cancel wakes a parked enquiry: the park returns the
    /// cancellation error at its next poll tick, never hangs — and its dead
    /// slot is removed, so the answer that never came has nowhere to land.
    #[test]
    fn cancel_wakes_a_parked_enquiry() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let (ours, _peer) = WireChannel::pair().expect("socketpair");
        let desk = WireDesk {
            writer: Arc::new(Mutex::new(ours)),
            current_dispatch: Arc::new(AtomicU64::new(1)),
            next_eid: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            answered: Condvar::new(),
        };

        // The turn's foreground scope, published as the turn doors publish
        // it, cancelled as `Control::Cancel`'s reader arm cancels it.
        let scope = crate::process::CancelScope::default();
        let _slot = publish_foreground(&scope);
        scope.cancel(CancelCause::Explicit);

        let err = desk.enquire(FOValue::Unit).expect_err("cancelled");
        assert_eq!(err.message, "cancelled");
        assert!(
            desk.slots.lock().unwrap().is_empty(),
            "a cancelled park removes its slot"
        );

        // The late answer for the dead id (the first minted, EnquiryId(1))
        // is dropped by id: the map stays empty, nothing wakes.
        desk.fill(EnquiryId(1), Ok(FOValue::Unit));
        assert!(desk.slots.lock().unwrap().is_empty());
    }

    /// A late `Answer` for an id that was never parked (or already died) is
    /// dropped by id: `fill` inserts nothing.
    #[test]
    fn late_answer_for_a_dead_id_is_dropped() {
        let (ours, _peer) = WireChannel::pair().expect("socketpair");
        let desk = WireDesk {
            writer: Arc::new(Mutex::new(ours)),
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
// A real `engine_session` on a thread over one end of a `WireChannel`
// pair, the test playing the host on the other. The Attach carries the
// test process's own cwd and HOME, so the engine's environment restoration
// is a no-op by value — an in-process engine must never chdir the test
// process. Timing is poll-until with multi-second slack (the dev fleet
// includes a jittery VM); "cancelled promptly" is proven by using a turn
// (`sleep 30`) that could not settle inside the ceiling on its own.
//
// The engine's shell publishes the process-global foreground/durable-root
// cancel slots for every turn it runs (the single-session default), so any
// test here that dispatches a turn serialises on `SLOT_SERIAL` — the same
// discipline `transport.rs`'s durability test follows.
#[cfg(test)]
mod engine_session_tests {
    use super::*;
    use crate::process::cancel::SLOT_SERIAL;
    use crate::transport::{PROTOCOL_VERSION, TerminalEndpoint};

    /// The poll-until ceiling every await obeys.
    const WAIT: Duration = Duration::from_secs(20);

    fn boot() -> Shell {
        static PRELUDE: std::sync::OnceLock<crate::driver::BakedPrelude> =
            std::sync::OnceLock::new();
        crate::driver::boot_shell(
            crate::io::TerminalState::default(),
            PRELUDE.get_or_init(crate::driver::BakedPrelude::bake_runtime),
            &crate::driver::HostSurface::default(),
        )
    }

    static INSTALLERS: &[EngineInstaller] = &[EngineInstaller { tag: "test", boot }];

    /// One capturing turn under the ⊤ capability ceiling.
    fn turn(src: &str) -> Turn {
        Turn {
            program: crate::transport::Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: crate::types::Capabilities::root(),
            turn_limit: None,
            deferred_lease: None,
            worker_cap: None,
            io: crate::driver::TurnIo::Capture,
            terminal: crate::driver::RequestedTerminalAccess::Denied,
            stdin: crate::driver::TurnStdin::Empty,
        }
    }

    /// The host half: the engine thread, the channel, and a buffer of
    /// frames read past while awaiting a specific one.
    struct Host {
        ch: WireChannel,
        seen: Vec<Frame>,
        engine: std::thread::JoinHandle<i32>,
    }

    fn start() -> Host {
        let (host_ch, engine_ch) = WireChannel::pair().expect("socketpair");
        let engine = std::thread::spawn(move || engine_session(engine_ch, INSTALLERS));
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
            self.send(&Frame::Dispatch(DispatchId(id), Box::new(turn(src))));
        }

        /// Await the first frame matching `pred` within [`WAIT`], buffering
        /// every other frame for later awaits.
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
                result: Ok(FOValue::Int { value }),
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

    /// A dispatch round-trips through `Event::Report`, and detach exits 0.
    #[test]
    fn dispatch_round_trips_to_a_report() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let mut host = start();
        assert_eq!(ran_int(&host.run(1, "$[1 + 1]")), 2);
        assert_eq!(host.detach_and_join(), 0);
    }

    /// A second dispatch and a probe sent while a turn runs both answer
    /// "engine busy": the single worker rendezvous, one arm for both riders.
    #[test]
    fn busy_refuses_a_second_dispatch_and_a_probe() {
        let _g = SLOT_SERIAL.lock().unwrap();
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

    /// `Control::Cancel` settles an in-flight turn promptly — a `sleep 30`
    /// could not settle inside the await ceiling on its own — and detach
    /// still exits 0.
    #[test]
    fn cancel_settles_an_in_flight_turn_promptly() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let mut host = start();
        host.dispatch(1, "sleep 30");
        // Let the worker stamp its dispatch and install its foreground scope.
        std::thread::sleep(Duration::from_secs(1));
        host.cancel(1);
        host.report(1);
        assert_eq!(host.detach_and_join(), 0);
    }

    /// A deferred worker's boundary batch arrives stamped `DispatchId(0)`.
    #[test]
    fn deferred_batch_is_stamped_dispatch_zero() {
        let _g = SLOT_SERIAL.lock().unwrap();
        let mut host = start();
        host.run(1, "let h = spawn { sleep 1 }");
        let frame = host.await_frame(|f| matches!(f, Frame::Event(_, Event::DeferredSurface(_))));
        let Frame::Event(did, _) = frame else {
            unreachable!("await_frame matched a DeferredSurface");
        };
        assert_eq!(did, DispatchId(0), "a boundary batch is stamped id 0");
        assert_eq!(host.detach_and_join(), 0);
    }
}
