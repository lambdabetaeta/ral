//! Engine process: the session-lived child that holds the Shell
//! and executes turns sent over a framed socket by the front-end.

use std::collections::HashMap;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;

use crate::driver::BakedPrelude;
use crate::serial::FOValue;
use crate::transport::{
    Control, DispatchId, EnquiryError, EnquiryId, Event, Frame, Report, Turn, answer_probe,
};
use crate::types::{DeferredSink, EnquiryDesk, Error, Shell, SurfaceSink};
use crate::wire::WireChannel;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

/// One compiled-in shell-parity target the engine child can be told, at
/// `Attach`, to become: the prelude to boot with and the host builtin
/// installer to apply afterward. Both halves of the single binary
/// (exarch, the REPL) pass their own table; the tag on the wire is the
/// only thing that names one, never the installer function itself —
/// code never crosses the seam.
pub struct EngineInstaller {
    /// The tag `Frame::Attach` carries — matched verbatim against the
    /// installer table this process was started with.
    pub tag: &'static str,
    /// The prelude to boot the engine's shell with (identical source
    /// across hosts today, but baked into each host's own binary).
    pub prelude: &'static BakedPrelude,
    /// The host builtin installer to run on the freshly booted shell.
    /// `|_| {}` names "no host builtins" (the REPL's table).
    pub install: fn(&mut Shell),
}

/// A surface sink that writes Event::Surface frames live to the wire
/// as values are produced, rather than buffering.
struct ChannelSurfaceSink {
    id: crate::transport::DispatchId,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::EventSink for ChannelSurfaceSink {
    fn emit(&self, ev: &FOValue) {
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(self.id, Event::Surface(ev.clone())));
    }
}

/// A deferred sink that writes Event::DeferredSurface frames to the wire.
struct ChannelDeferredSink {
    id: DispatchId,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::DeferredSink for ChannelDeferredSink {
    fn deliver(&self, batch: Vec<FOValue>) {
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(self.id, Event::DeferredSurface(batch)));
    }
}

/// Cadence at which a parked enquiry re-checks the foreground cancel slot.
/// `Control::Cancel` arrives on the reader thread and trips the published
/// foreground scope, never this rendezvous — the park must poll, and this
/// bounds how stale a cancel can go unnoticed.
const ENQUIRY_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(75);

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
/// live wire child; `run_engine` is the only caller and turns an `Err`
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

/// Run the engine loop on an inherited socket (fd 3).
/// The front-end passes the socket as fd 3 before exec.
///
/// `installers` is this binary's compiled-in table of shell-parity
/// targets; the tag `Attach` names is looked up here, never guessed —
/// an unrecognised tag refuses the session as loudly as a protocol
/// mismatch, since a wire engine speaking the wrong builtins is exactly
/// the incoherence this rail exists to rule out. Never returns.
pub fn run_engine(installers: &[EngineInstaller]) -> ! {
    // SAFETY: fd 3 is the socket inherited from the front-end
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    // `dup2` (the front-end's pre_exec handoff) always clears CLOEXEC on
    // the duplicate it creates, so fd 3 arrives here open-across-exec by
    // necessity. Set it CLOEXEC now, the instant the engine owns it, so no
    // external command any turn spawns inherits the protocol socket.
    if unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        eprintln!(
            "engine: failed to set CLOEXEC on the wire fd: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }
    let reader_ch = WireChannel::from_stream(stream);
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
                    std::process::exit(1);
                }
            };
            // Restore the session environment.
            #[allow(
                clippy::disallowed_methods,
                reason = "engine cwd restore during Attach — sets engine process cwd, not Shell logical cwd"
            )]
            if let Err(e) = std::env::set_current_dir(&cwd) {
                eprintln!("engine: failed to set cwd to {}: {e}", cwd.display());
            }
            // SAFETY: single-threaded engine startup, no other threads
            unsafe {
                std::env::set_var("HOME", &home);
            }
            let _ = endpoint; // TODO Phase 2 Task 6: pass terminal fds via SCM_RIGHTS
            let _ = rc_path; // TODO: load rc_path — needs the host's RcCtx/plugin machinery, not a core-level concern yet

            let mut shell = crate::driver::boot_shell(crate::io::TerminalState::default(), target.prelude);
            (target.install)(&mut shell);
            shell
        }
        Ok(Some(Frame::Detach) | None) => std::process::exit(0),
        Ok(Some(_)) => {
            eprintln!("engine: expected Attach as the first frame");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("engine: read error awaiting attach: {e}");
            std::process::exit(1);
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

    // Dispatch channel, gated by an explicit readiness flag rather than
    // inferred from channel-parking state: `sync_channel(0)`'s try_send
    // reports "full" the instant the worker picks an item off the channel,
    // even though the worker is still mid-turn — but it *also* reports
    // "full" (spuriously) in the thin window after the worker has finished
    // writing its `Report` but before it has re-parked on `recv`, so a
    // perfectly serialized next turn could be misclassified "busy". A
    // dedicated flag names the true fact ("a turn is in flight") and is
    // cleared by the worker itself only after its Report is written and it
    // is about to re-park, closing that window.
    let worker_ready = Arc::new(AtomicBool::new(true));
    let (turn_tx, turn_rx) = mpsc::channel::<WorkItem>();

    // The engine's one desk, shared between the worker (which parks in it)
    // and the reader loop (whose `Answer` arm fills it).
    let desk = Arc::new(WireDesk {
        writer: writer.clone(),
        current_dispatch: Arc::new(AtomicU64::new(0)),
        next_eid: AtomicU64::new(1),
        slots: Mutex::new(HashMap::new()),
        answered: Condvar::new(),
    });

    // ── Worker thread: owns the Shell ──────────────────────────────
    let worker_writer = writer.clone();
    let worker_desk = desk.clone();
    let ready_for_worker = worker_ready.clone();
    std::thread::spawn(move || {
        let mut shell = shell;
        while let Ok(item) = turn_rx.recv() {
            match item {
                WorkItem::Turn(id, turn) => {
                    // Stamp the in-flight dispatch so the desk's enquiry
                    // frames correlate to the turn that raised them.
                    worker_desk.current_dispatch.store(id.0, Ordering::Relaxed);

                    // The live handles this turn runs under, joined with the
                    // protocol `Turn` in the request the engine door runs.
                    let req = crate::driver::TurnRequest {
                        turn: *turn,
                        surface: Some(Arc::new(ChannelSurfaceSink {
                            id,
                            writer: worker_writer.clone(),
                        }) as SurfaceSink),
                        deferred: Some(Arc::new(ChannelDeferredSink {
                            id,
                            writer: worker_writer.clone(),
                        }) as Arc<dyn DeferredSink>),
                        desk: Some(worker_desk.clone() as crate::types::Desk),
                        lifecycle: Box::new(()),
                    };
                    // The workspace pins `panic = "unwind"` so hosts can
                    // recover a panicking turn; without this the panic
                    // would tear down the worker thread silently, no
                    // `Report` frame would ever be written, and the
                    // front-end's `recv` would block forever.
                    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        shell.run_turn(req).into_report()
                    }))
                    .unwrap_or_else(|_| Report::Static {
                        diagnostics: crate::transport::Diagnostics::Host(
                            "engine: turn panicked".into(),
                        ),
                    });

                    worker_desk.current_dispatch.store(0, Ordering::Relaxed);

                    // Send the terminal Report frame.
                    let _ = worker_writer
                        .lock()
                        .unwrap()
                        .write_frame(&Frame::Event(id, Event::Report(report)));
                }
                WorkItem::Probe(id, reading) => {
                    let report = match answer_probe(&mut shell, reading) {
                        Ok(v) => Report::Ran {
                            result: Ok(v),
                            status: 0,
                            single_command: false,
                            captured: None,
                            timed_out: false,
                        },
                        Err(message) => Report::Ran {
                            result: Err(crate::transport::Break::Error(message)),
                            status: 1,
                            single_command: false,
                            captured: None,
                            timed_out: false,
                        },
                    };
                    let _ = worker_writer
                        .lock()
                        .unwrap()
                        .write_frame(&Frame::Event(id, Event::Report(report)));
                }
            }
            // The Report is written; only now — about to re-park on
            // `recv` — is the worker actually ready for the next item.
            ready_for_worker.store(true, Ordering::Release);
        }
    });

    // ── Reader loop (this thread) ──────────────────────────────────
    // The exit code distinguishes a clean detach from a protocol fault: a
    // parent that can no longer tell the two apart (both used to exit 0)
    // cannot know whether the session ended on request or on corruption.
    let exit_code = loop {
        match reader_ch.read_frame() {
            // A turn and a probe ride the same worker rendezvous, so a probe
            // sent while a turn runs gets the same "engine busy" answer a
            // second dispatch would — one arm, by construction.
            Ok(Some(frame @ (Frame::Dispatch(..) | Frame::Probe(..)))) => {
                let item = match frame {
                    Frame::Dispatch(id, turn) => WorkItem::Turn(id, turn),
                    Frame::Probe(id, reading) => WorkItem::Probe(id, reading),
                    _ => unreachable!("the arm admits only Dispatch and Probe"),
                };
                // Claim the worker atomically: only the dispatch that flips
                // `true → false` may hand it work, so "busy" reflects a
                // turn genuinely in flight, never a worker that has merely
                // not yet re-parked.
                let claimed = worker_ready
                    .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok();
                if claimed {
                    if turn_tx.send(item).is_err() {
                        break 0; // worker died
                    }
                } else {
                    let (WorkItem::Turn(id, _) | WorkItem::Probe(id, _)) = item;
                    let report = crate::transport::Report::Static {
                        diagnostics: crate::transport::Diagnostics::Host("engine busy".into()),
                    };
                    let _ = writer
                        .lock()
                        .unwrap()
                        .write_frame(&Frame::Event(id, Event::Report(report)));
                }
            }
            Ok(Some(Frame::Answer(_, eid, answer))) => {
                // Correlated by EnquiryId alone: the dispatch id names the
                // turn for the front-end's benefit, but the slot is the
                // enquiry's. A late answer for a dead id drops in `fill`.
                desk.fill(eid, answer);
            }
            Ok(Some(Frame::Control(Control::Cancel(_)))) => {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
            }
            Ok(Some(Frame::Control(Control::Resize(_winsize)))) => {
                // no-op for now, TODO task 7
            }
            Ok(Some(Frame::Control(Control::Suspend | Control::Resume))) => {}
            Ok(Some(Frame::Attach { .. })) => {
                eprintln!("engine: unexpected second Attach");
            }
            Ok(Some(Frame::Detach) | None) => {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
                break 0;
            }
            Ok(Some(Frame::Event(..))) => {
                eprintln!("engine: unexpected Event frame");
            }
            Err(e) => {
                eprintln!("engine: read error: {e}");
                break 1;
            }
        }
    };
    std::process::exit(exit_code);
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
    use crate::process::signal::SLOT_SERIAL;
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

    fn stub_installers() -> Vec<EngineInstaller> {
        vec![EngineInstaller {
            tag: "exarch-agent",
            prelude: {
                static P: std::sync::OnceLock<BakedPrelude> = std::sync::OnceLock::new();
                P.get_or_init(BakedPrelude::bake_runtime)
            },
            install: |_shell| {},
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
