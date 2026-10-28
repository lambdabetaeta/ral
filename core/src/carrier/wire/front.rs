//! The front-end's wire carrier: the engine runs across a duplex stream, frames
//! crossing on a [`WireChannel`] as length-prefixed JSON.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak, mpsc};
use std::time::Duration;

use crate::carrier::{ControlRoute, ControlSender, EventReceiver, forbid_reentry};
use crate::carrier::{
    Ends, Host, Line, ProbeError, READER_SEVERS_FIRST, Severed, Transport, mint_dispatch_id, sever,
};
use crate::data::FOValue;
use crate::process::Scope;
use crate::protocol::channel::{Liveness, WireChannel, WireStream, WriteError, ticks};
use crate::protocol::probe::Probe;
use crate::protocol::{
    Attach, DispatchId, EnquiryError, EnquiryId, Event, Frame, Report, Run, SessionEvent,
};
use crate::shell::runtime::DeferredSink;
use crate::sync::{CondvarExt as _, LockExt};

/// The out-of-process transport: the engine runs across a duplex stream, frames
/// crossing on a `WireChannel` as length-prefixed JSON.
///
/// [`WireTransport::adopt`] drives an *existing* stream, in production the
/// virtual socket into a guest VM, whose failure mode is silence rather than
/// EOF, and so runs a ticker.
///
/// A reader thread forwards `Event` frames into the `EventReceiver`, swallows
/// `Pong`s, and timestamps every frame it reads. All writes go through its
/// [`Line`], the ticker's `Ping` included.
pub struct WireTransport {
    /// The severance cell is shared with the reader and the line; the reader
    /// and the ticker check it to break early; the sink with the reader, so
    /// `set_deferred_sink` takes effect on the next batch.
    ends: Ends,
    /// Owned here alone; see [`Line`].
    line: Arc<Line>,
    /// Never joined: the thread exits when the channel closes. `Mutex` only
    /// for `Sync`.
    _reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// `Ping`s sent with no frame back since — what the ticker counts silence
    /// in. Any frame read resets it; see [`Liveness::probes`].
    unanswered: Arc<AtomicU32>,
    /// The heartbeat still owed, parked by [`WireTransport::adopt`] and taken
    /// by the first [`WireTransport::attach`].
    pending_heartbeat: Mutex<Option<Liveness>>,
    session: Arc<(Mutex<SessionState>, Condvar)>,
    /// How long [`WireTransport::await_attached`] waits for the verdict before
    /// declaring the engine [`Severed::Silent`].
    patience: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Opening,
    Attached,
    Ended,
}

/// The transport's reader loop. *Every* frame read clears
/// `unanswered`, not just a `Pong` — any frame is proof of life. On EOF, a read
/// error, a refused `Attach`, or a frame only a front-end sends it severs
/// before exiting, so `severed()` is
/// honest under every teardown path and the dropped `event_tx` closes the
/// channel whose `recv` is what fails the in-flight dispatch — the reader
/// severs *before* it drops `event_tx`, on every exit path.
///
/// `Frame::Event` goes onto the per-run channel; `Frame::Session` goes to
/// whichever sink `deferred_sink` holds at the moment it arrives, or is
/// dropped if none is installed; `SessionEvent::Attached` wakes
/// [`WireTransport::await_attached`].
fn spawn_wire_reader(
    mut reader_ch: WireChannel,
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    deferred_sink: Arc<Mutex<Option<Arc<dyn DeferredSink>>>>,
    severance: Arc<OnceLock<Severed>>,
    unanswered: Arc<AtomicU32>,
    session: Arc<(Mutex<SessionState>, Condvar)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Severs on every exit, panics included (e.g. from `DeferredSink::deliver`),
        // so `dispatch_to_report`'s `expect` always finds a cause. Declared first so
        // it drops after the loop's own locals but before the captured `event_tx`.
        struct SeverOnExit<'a> {
            severance: &'a OnceLock<Severed>,
            session: &'a (Mutex<SessionState>, Condvar),
        }
        impl Drop for SeverOnExit<'_> {
            fn drop(&mut self) {
                sever(
                    self.severance,
                    Severed::Closed("the reader thread ended".into()),
                );
                self.session.1.notify_all();
            }
        }
        let _sever_on_exit = SeverOnExit {
            severance: &severance,
            session: &session,
        };

        loop {
            if severance.get().is_some() {
                break;
            }
            match reader_ch.read_frame() {
                Ok(Some(frame)) => {
                    unanswered.store(0, Ordering::Relaxed);
                    // A `Pong`'s whole job is the reset above.
                    match frame {
                        Frame::Event(id, ev) => {
                            if event_tx.send((id, ev)).is_err() {
                                break;
                            }
                        }
                        Frame::Session(SessionEvent::DeferredSurface(batch)) => {
                            let sink = deferred_sink.lock_ignore_poison().clone();
                            if let Some(sink) = sink {
                                sink.deliver(batch);
                            }
                        }
                        Frame::Session(SessionEvent::Attached) => {
                            *session.0.lock_ignore_poison() = SessionState::Attached;
                            session.1.notify_all();
                        }
                        Frame::Session(SessionEvent::Ended) => {
                            *session.0.lock_ignore_poison() = SessionState::Ended;
                            session.1.notify_all();
                            break;
                        }
                        Frame::Session(SessionEvent::Refused(msg)) => {
                            sever(&severance, Severed::Refused(msg));
                            break;
                        }
                        Frame::Pong(_) => {}
                        other => {
                            sever(
                                &severance,
                                Severed::Faulted(format!(
                                    "it sent a {} frame, which only a front-end sends",
                                    other.kind()
                                )),
                            );
                            break;
                        }
                    }
                }
                Ok(None) => {
                    sever(
                        &severance,
                        Severed::Closed("the engine closed the connection".into()),
                    );
                    break;
                }
                Err(e) => {
                    sever(&severance, Severed::Closed(e.to_string()));
                    break;
                }
            }
        }
    })
}

/// The heartbeat ticker of an adopted stream, spawned by the first
/// [`WireTransport::attach`] once the `Attach` frame is through the write lock —
/// never at [`WireTransport::adopt`] — so no `Ping` precedes the handshake by
/// construction. On declaring death, from silence or a failed write, it shuts
/// the wire down too, waking the parked reader so the event channel closes.
/// Never joined.
///
/// The deadline arm never takes the line's write lock: severing from silence must
/// not itself be capable of parking behind a write some other thread is
/// stalled on, so a severed transport always implies `shutdown` runs in
/// bounded time. The `Ping` arm goes through [`Line::write`], which severs
/// on failure the same way any other front-end write does. It holds the line
/// weakly and ends at the first tick after its transport is gone.
fn spawn_heartbeat(line: Weak<Line>, unanswered: Arc<AtomicU32>, liveness: Liveness) {
    let patience = liveness.probes();
    std::thread::spawn(move || {
        let mut seq: u64 = 0;
        loop {
            std::thread::sleep(liveness.interval);
            // Held for one tick, never across the sleep.
            let Some(line) = line.upgrade() else { break };
            if line.severance.get().is_some() {
                break;
            }
            // Only probes this thread actually sent can condemn the peer, so a
            // host that was asleep — sending none — wakes accusing nobody.
            if unanswered.load(Ordering::Relaxed) >= patience {
                line.sever(Severed::Silent(liveness.deadline));
                break;
            }
            seq += 1;
            match line.write(&Frame::Ping(seq)) {
                Err(WriteError::Severed) => break,
                Err(WriteError::Unencodable(e)) => unreachable!("a ping is a few bytes: {e}"),
                Ok(()) => {}
            }
            unanswered.fetch_add(1, Ordering::Relaxed);
        }
    });
}

impl WireTransport {
    /// Drive the protocol over an existing duplex `stream`.
    ///
    /// `stream` is the virtual-socket connection to the engine in the guest —
    /// an `AF_VSOCK` descriptor under Virtualization.framework, an `AF_HYPERV`
    /// socket under Hyper-V — adopted through [`WireStream`],
    /// whose docs say why std's stream types can carry either. Such a stream
    /// can fall silent without ever tearing, so the first
    /// [`WireTransport::attach`] spawns a heartbeat ticker under `liveness`.
    ///
    /// # Errors
    /// Returns `Err` if the stream cannot be duplicated into separate read
    /// and write handles.
    pub fn adopt(stream: impl Into<WireStream>, liveness: Liveness) -> io::Result<Self> {
        let reader_ch = WireChannel::from_stream(stream);
        // A duplicate fd, so the reader can park in `read_frame` while the
        // ticker and the front-end write on the same socket.
        let writer = reader_ch.try_clone()?;
        let deadline = writer.set_write_deadline(liveness.deadline);
        // A second duplicate, held back from ever taking the write lock, so
        // `Drop` can sever the connection without parking behind a write in
        // progress.
        let shutdown = writer.try_clone()?;

        let severance = Arc::new(OnceLock::new());
        let unanswered = Arc::new(AtomicU32::new(0));
        let (event_tx, event_rx) = mpsc::channel();
        let session = Arc::new((Mutex::new(SessionState::Opening), Condvar::new()));
        let line = Arc::new(Line {
            channel: Mutex::new(writer),
            shutdown,
            severance: severance.clone(),
        });
        let control = ControlSender::new(ControlRoute::Wire(Arc::downgrade(&line)));
        let ends = Ends::new(control, EventReceiver::new(event_rx), severance);
        // Darwin refuses options on a socket whose peer is already gone:
        // the death Linux learns at the first read, learnt here.
        if let Err(e) = deadline {
            line.sever(Severed::Closed(e.to_string()));
        }
        let reader = spawn_wire_reader(
            reader_ch,
            event_tx,
            ends.deferred_sink.clone(),
            ends.severance.clone(),
            unanswered.clone(),
            session.clone(),
        );

        Ok(Self {
            ends,
            line,
            _reader: Mutex::new(Some(reader)),
            unanswered,
            pending_heartbeat: Mutex::new(Some(liveness)),
            session,
            patience: liveness.deadline,
        })
    }

    /// Write `attach`, the only legal first frame, and only then start the
    /// heartbeat: no `Ping` may precede the handshake. The verdict is
    /// [`Self::await_attached`]'s.
    pub fn attach(&self, attach: Attach) {
        // No session yet to refuse into: the refusal is the attach's verdict.
        if let Err(WriteError::Unencodable(e)) = self.write(&Frame::Attach(attach)) {
            self.sever(Severed::Refused(format!(
                "the attach could not be sent: {e}"
            )));
            return;
        }
        let pending = self.pending_heartbeat.lock_ignore_poison().take();
        if let Some(liveness) = pending {
            // Silence is counted from here, not from `adopt`: a front-end that
            // adopted long before attaching must not find its booting engine
            // already declared `Silent` on the heartbeat's first tick.
            self.unanswered.store(0, Ordering::Relaxed);
            spawn_heartbeat(
                Arc::downgrade(&self.line),
                self.unanswered.clone(),
                liveness,
            );
        }
    }

    /// Block until the engine answers the `Attach` this transport wrote, so a
    /// refusal is learnt at construction, not at the first dispatch.
    ///
    /// Patience is spent in waits this thread observed, never in elapsed
    /// clock: a suspended host resumes mid-wait having watched one tick, not
    /// the thousands its clock ran through. Same law as [`Liveness::probes`].
    ///
    /// # Errors
    /// The transport's severance — a refused `Attach`, a closed connection, or
    /// [`Severed::Silent`] once no verdict arrives within `self.patience`.
    pub fn await_attached(&self) -> Result<(), Severed> {
        self.await_state(SessionState::Attached)
    }

    /// End the remote session and await proof that its workers have torn down.
    ///
    /// # Errors
    /// A severed connection or missing teardown acknowledgment.
    pub fn end(&self) -> Result<(), Severed> {
        forbid_reentry(self);
        if *self.session.0.lock_ignore_poison() == SessionState::Ended {
            return Ok(());
        }
        if let Some(cause) = self.severed() {
            return Err(cause);
        }
        if let Err(WriteError::Unencodable(e)) = self.write(&Frame::Detach) {
            return Err(self.sever(Severed::Faulted(format!("detach: {e}"))));
        }
        self.await_state(SessionState::Ended)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard is the loop's own state, retaken by every wait"
    )]
    fn await_state(&self, wanted: SessionState) -> Result<(), Severed> {
        const TICK: Duration = Duration::from_millis(100);
        let mut left = ticks(self.patience, TICK);
        let mut guard = self.session.0.lock_ignore_poison();
        loop {
            // Acknowledged teardown survives EOF; attachment does not.
            if wanted == SessionState::Ended && *guard == wanted {
                return Ok(());
            }
            if let Some(cause) = self.severed() {
                return Err(cause);
            }
            if *guard == wanted {
                return Ok(());
            }
            if left == 0 {
                return Err(self.line.sever(Severed::Silent(self.patience)).clone());
            }
            let (next, timed_out) = self.session.1.wait_timeout_ignore_poison(guard, TICK);
            guard = next;
            if timed_out.timed_out() {
                left -= 1;
            }
        }
    }

    /// Any I/O error is fatal, `ECONNRESET` no less than `BrokenPipe`: a
    /// dropped frame means no `Report` will ever arrive, so the host's `recv`
    /// must learn of the severance rather than block forever. A frame the fuse
    /// refuses never reached the wire: each caller answers that refusal where
    /// it stands.
    fn write(&self, frame: &Frame) -> Result<(), WriteError> {
        self.line.write(frame)
    }
}

impl Drop for WireTransport {
    /// The session's end: the engine reads EOF.
    fn drop(&mut self) {
        self.line.sever(Severed::Closed(
            "the front-end dropped the transport".into(),
        ));
    }
}

impl Transport for WireTransport {
    fn ends(&self) -> &Ends {
        &self.ends
    }

    fn dispatch(&self, id: DispatchId, run: Run, _host: &Arc<dyn Host>, _within: Option<&Scope>) {
        // The host is consulted by the drain loop, not here: an enquiry
        // crossing a wire is a frame on `events()`, answered by whoever drains
        // it — `dispatch_to_report`.
        if let Err(WriteError::Unencodable(e)) = self.write(&Frame::Dispatch(id, Box::new(run))) {
            let report = Report::refused(format!("dispatch: the run could not be sent: {e}"));
            self.ends
                .events
                .stash
                .lock_ignore_poison()
                .push_back((id, Event::Report(report)));
        }
    }

    fn probe(&self, probe: &Probe) -> Result<FOValue, ProbeError> {
        forbid_reentry(self);
        let id = mint_dispatch_id();
        if let Err(WriteError::Unencodable(e)) = self.write(&Frame::Probe(id, probe.clone())) {
            return Err(ProbeError::Rejected(format!(
                "probe: the probe could not be sent: {e}"
            )));
        }
        // Foreign events read past on the way to this probe's own reading are
        // held here rather than restashed mid-loop: `recv`'s stash-first law
        // would hand a mid-loop stash straight back on the next iteration,
        // spinning forever on a foreign event.
        let mut carried = VecDeque::new();
        let outcome = loop {
            match self.ends.events.recv() {
                Some((did, Event::Reading(answer))) if did == id => {
                    break answer.map_err(ProbeError::Rejected);
                }
                Some(item) => carried.push_back(item),
                None => {
                    break Err(ProbeError::Severed(
                        self.severed().expect(READER_SEVERS_FIRST),
                    ));
                }
            }
        };
        // Anything that was not this probe's reading — another dispatch's, a
        // worker's batch settling mid-probe — is stashed for the ordinary
        // drain rather than dropped.
        self.ends.events.stash.lock_ignore_poison().extend(carried);
        outcome
    }

    /// Also shuts the connection, so the engine sees EOF.
    fn sever(&self, cause: Severed) -> Severed {
        self.line.sever(cause).clone()
    }

    /// An answer the fuse refuses is replaced by its refusal, so the parked
    /// engine is not left waiting.
    fn answer(&self, eid: EnquiryId, answer: Result<FOValue, EnquiryError>) {
        if let Err(WriteError::Unencodable(e)) = self.write(&Frame::Answer(eid, answer)) {
            let refusal = EnquiryError {
                message: format!("answer: the host could not reply: {e}"),
                status: 1,
            };
            let _ = self.write(&Frame::Answer(eid, Err(refusal)));
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
