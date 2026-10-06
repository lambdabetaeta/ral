//! The carriers of the engine protocol: the two interpretations of the frame
//! algebra ([`crate::protocol`]) a front-end drives an [`Engine`](crate::engine::Engine)
//! through, and the laws they share.
//!
//! [`IdentityTransport`] calls the engine in one address space: frames move,
//! never encode. [`WireTransport`] encodes the same frames as length-prefixed
//! JSON over a socket, answered by the engine process in `wire/serve.rs`. Both
//! hold their front-end side in one [`Ends`], so the severance law, the event
//! stream and the session sink are stated once.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::engine::Scopes;
use crate::first_order::FOValue;
use crate::first_order::datum::Datum;
use crate::protocol::channel::{WireChannel, write_or_sever};
use crate::protocol::probe::{BindingRow, CompletionNames, PathEntry, Probe, WorkerRow};
use crate::protocol::{Control, DispatchId, EnquiryError, EnquiryId, Event, Frame, Report, Run};
use crate::sync::LockExt as _;
use crate::types::DeferredSink;

mod identity;
#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;
mod wire;

pub use identity::IdentityTransport;
pub use wire::WireTransport;
#[cfg(unix)]
pub use wire::run_engine;

// ── Severance ──────────────────────────────────────────────────────────

/// Why no further frame will cross the protocol. Terminal: a severed transport
/// never recovers, so a front-end that sees one ends its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severed {
    /// The engine refused `Attach`, in its own words: a protocol version
    /// mismatch, an unknown installer, a seed it could not apply.
    Refused(String),
    /// The stream closed or a frame failed to cross.
    Closed(String),
    /// Nothing arrived for the liveness deadline — a virtual socket whose far
    /// end died without an EOF.
    Silent(Duration),
    /// The engine answered outside the protocol — a refused boundary-time
    /// probe, say — so nothing it says can be trusted further.
    Faulted(String),
}

impl Severed {
    /// A short, stable name for *why* the engine went, meant to be shown to a
    /// person and quoted back — in a bug report, in a support mail, in a
    /// search of this tree.  The variant's own [`Display`](std::fmt::Display)
    /// is a sentence written for a log; this is the handle a reader holds on
    /// to when the sentence has scrolled away.
    ///
    /// Stability is the whole point: these strings are part of what a
    /// front-end shows, so renaming one silently reclassifies every failure a
    /// user has already learnt to recognise.  Add a variant and add a name;
    /// never repurpose a name that has shipped.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Refused(_) => "engine-refused",
            Self::Closed(_) => "engine-closed",
            Self::Silent(_) => "engine-silent",
            Self::Faulted(_) => "engine-faulted",
        }
    }
}

impl std::fmt::Display for Severed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(msg) => write!(f, "the engine refused to attach: {msg}"),
            Self::Closed(msg) => write!(f, "the connection to the engine closed: {msg}"),
            Self::Silent(d) => write!(
                f,
                "the engine fell silent for {}s and was declared dead",
                d.as_secs()
            ),
            Self::Faulted(msg) => write!(f, "the engine broke the protocol: {msg}"),
        }
    }
}

/// First cause wins; the one that stands.
fn sever(severance: &OnceLock<Severed>, cause: Severed) -> &Severed {
    severance.get_or_init(|| cause)
}

/// Why a probe has no reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The engine answered but would not read: the rendezvous held by an
    /// in-flight run, or a panic in the reading. A program error on the
    /// caller's side, since probes are legal only at a run boundary.
    Rejected(String),
    /// No answer will ever come.
    Severed(Severed),
}

// ── The host ───────────────────────────────────────────────────────────

/// The host's side of one run: where its surfaced values go, and who answers
/// its enquiries. One object, so the rails a run speaks on can never be bound
/// to two hosts.
pub trait Host: Send + Sync {
    fn surface(&self, val: &FOValue);

    /// Answer one enquiry.
    ///
    /// # Errors
    /// Returns `Err` when this host cannot answer `req`.
    fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError>;
}

/// The mute host: renders nothing, answers nothing.
impl Host for () {
    fn surface(&self, _val: &FOValue) {}

    fn enquire(&self, _req: FOValue) -> Result<FOValue, EnquiryError> {
        Err(EnquiryError::no_desk())
    }
}

// ── The front-end's state ─────────────────────────────────────────────

/// What both carriers hold of the front-end's side of one connection: its
/// control door, the events it drains, the cause that ended it, and the
/// session sink.
pub struct Ends {
    control: ControlSender,
    events: Arc<EventReceiver>,
    severance: Arc<OnceLock<Severed>>,
    /// `None` until a host installs one, and while it is `None` a settling
    /// worker's batch is dropped: a front-end declining session events, not a
    /// loss the carrier owes anyone.
    deferred_sink: Arc<Mutex<Option<Arc<dyn DeferredSink>>>>,
}

impl Ends {
    /// Ends over `control` and `events`, whose severance is `severance`: the
    /// cell `control`'s own door records into, if it writes a wire.
    fn new(
        control: ControlSender,
        events: EventReceiver,
        severance: Arc<OnceLock<Severed>>,
    ) -> Self {
        Self {
            control,
            events: Arc::new(events),
            severance,
            deferred_sink: Arc::default(),
        }
    }

    /// The cause that stands: the first one recorded.
    fn sever(&self, cause: Severed) -> Severed {
        sever(&self.severance, cause).clone()
    }
}

// ── Transport trait ───────────────────────────────────────────────────

/// The front-end side of the engine protocol. Construction is attach: a
/// transport in hand is one its engine accepted.
///
/// A carrier supplies its [`Ends`] and the three verbs that differ in
/// carriage; everything the front-end does with them is provided.
pub trait Transport: Send + Sync {
    /// This carrier's front-end state.
    fn ends(&self) -> &Ends;

    /// Run a dispatch synchronously. The `Report` arrives as the final `Event`,
    /// after any `Surface` events, which may be drained concurrently. `host`
    /// is the host's side of this run; see [`dispatch_to_report`] for how each
    /// transport uses it.
    fn dispatch(&self, id: DispatchId, run: Run, host: &Arc<dyn Host>);

    /// Read session state at a run boundary, synchronously. The typed doors
    /// below are the way to ask.
    ///
    /// # Errors
    /// [`ProbeError::Rejected`] is a program error on the caller's side:
    /// `probe` is legal only at a run boundary, so a caller that could see it
    /// has already broken that rule. A wire caller cannot extend that trust
    /// to the far side, and treats a `Rejected` there as the protocol fault
    /// it would then be. [`ProbeError::Severed`] is the engine's death, never
    /// a program error.
    fn probe(&self, probe: &Probe) -> Result<FOValue, ProbeError>;

    /// Detach: cancel in-flight dispatch, reap foreground subtree,
    /// restore terminal state.
    fn detach(&self);

    /// Answer one enquiry the engine raised on `Event::Enquiry`. A no-op under
    /// the identity transport, whose enquiry is a direct call through the
    /// installed `Desk` and never a frame.
    fn answer(&self, _eid: EnquiryId, _answer: Result<FOValue, EnquiryError>) {}

    /// The out-of-band control sender: writable while a dispatch is in
    /// flight.
    fn control(&self) -> &ControlSender {
        &self.ends().control
    }

    /// The event stream the front-end drains.
    fn events(&self) -> &EventReceiver {
        &self.ends().events
    }

    /// Why no further frame will cross the protocol, if that has happened: a
    /// write error, a read EOF, a refused `Attach`, or a silence deadline.
    /// This is how a front-end tells *detached* from merely failed.
    fn severed(&self) -> Option<Severed> {
        self.ends().severance.get().cloned()
    }

    /// Declare the engine dead for a cause the front-end observed itself — an
    /// answer outside the protocol, say — and answer the cause that stands,
    /// which is the first one recorded.
    fn sever(&self, cause: Severed) -> Severed {
        self.ends().sever(cause)
    }

    /// Install the session sink a `Frame::Session` batch is delivered to.
    fn set_deferred_sink(&self, sink: Arc<dyn DeferredSink>) {
        *self.ends().deferred_sink.lock_ignore_poison() = Some(sink);
    }

    /// The engine's logical cwd.
    ///
    /// # Errors
    /// [`ProbeError`]: a refusal, or the severance an ill-shaped answer causes.
    fn cwd(&self) -> Result<PathBuf, ProbeError> {
        read::<String>(self, &Probe::Cwd).map(PathBuf::from)
    }

    /// The engine's `HOME`, through its own env overlay.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn home(&self) -> Result<Option<PathBuf>, ProbeError> {
        read::<Option<String>>(self, &Probe::Home).map(|home| home.map(PathBuf::from))
    }

    /// One variable of the engine's environment: its overlay over its own
    /// process env, which across a wire is not this process's.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn env_var(&self, name: &str) -> Result<Option<String>, ProbeError> {
        read(self, &Probe::EnvVar(name.to_string()))
    }

    /// Every builtin the engine's shell resolves, internals included.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn builtin_names(&self) -> Result<Vec<String>, ProbeError> {
        read(self, &Probe::BuiltinNames)
    }

    /// The recursive byte size of `path` in the engine's own filesystem,
    /// resolved against its cwd.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn path_bytes(&self, path: &Path) -> Result<u64, ProbeError> {
        read(self, &Probe::PathBytes(path.to_path_buf()))
    }

    /// How many bindings the session holds.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn binding_count(&self) -> Result<u64, ProbeError> {
        read(self, &Probe::BindingCount)
    }

    /// How many bindings the lease ledger governs.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn leased_binding_count(&self) -> Result<u64, ProbeError> {
        read(self, &Probe::LeasedBindingCount)
    }

    /// The shallow byte estimate of the session's largest binding.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn largest_binding_bytes(&self) -> Result<u64, ProbeError> {
        read(self, &Probe::LargestBindingBytes)
    }

    /// The engine's worker table.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn workers(&self) -> Result<Vec<WorkerRow>, ProbeError> {
        read(self, &Probe::Workers)
    }

    /// The exit status of whatever ended the session — its durable root's
    /// cancel cause — or `None` while it lives.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn session_ended(&self) -> Result<Option<i32>, ProbeError> {
        read(self, &Probe::SessionEnded)
    }

    /// The binding and handler names in scope, sorted.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn completion_names(&self) -> Result<CompletionNames, ProbeError> {
        read(self, &Probe::CompletionNames)
    }

    /// Every binding in scope, rendered.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn bindings(&self) -> Result<Vec<BindingRow>, ProbeError> {
        read(self, &Probe::Bindings)
    }

    /// The entries of `dir` in the engine's own filesystem, resolved against
    /// its cwd.
    ///
    /// # Errors
    /// As [`Self::cwd`].
    fn path_entries(&self, dir: &Path) -> Result<Vec<PathEntry>, ProbeError> {
        read(self, &Probe::PathEntries(dir.to_path_buf()))
    }
}

/// Ask `probe` of `t`, and decode its answer as a `T`; an answer outside that
/// shape severs `t`.
pub(crate) fn read<T: Datum>(
    t: &(impl Transport + ?Sized),
    probe: &Probe,
) -> Result<T, ProbeError> {
    let answer = t.probe(probe)?;
    T::decode(&answer).map_err(|why| {
        ProbeError::Severed(t.sever(Severed::Faulted(format!(
            "the {probe:?} probe answered outside its shape: {why}"
        ))))
    })
}

// ── Dispatch loop ─────────────────────────────────────────────────────

thread_local! {
    /// The transports this thread is dispatching on, innermost last.
    static DISPATCHING: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn key<T: ?Sized>(transport: &T) -> usize {
    std::ptr::from_ref(transport).cast::<()>().addr()
}

/// Panic, didactically, if this thread is inside a dispatch on `transport`: a
/// `Host` handler runs on the dispatching thread, inside that very dispatch.
pub(crate) fn forbid_reentry<T: ?Sized>(transport: &T) {
    let key = key(transport);
    assert!(
        !DISPATCHING.with_borrow(|within| within.contains(&key)),
        "reentrant session access: a Host handler runs inside the dispatch that called it, on \
         the dispatching thread, so it must not dispatch, probe, or take the session of that \
         same transport: the identity transport would deadlock on its own session lock, and \
         the wire's drain loop would swallow the outer run's events. Did the handler mean to \
         answer from state it captured instead?"
    );
}

/// This thread's claim on a transport for one [`dispatch_to_report`].
struct Dispatching;

impl Dispatching {
    fn enter(transport: &dyn Transport) -> Self {
        forbid_reentry(transport);
        DISPATCHING.with_borrow_mut(|within| within.push(key(transport)));
        Self
    }
}

impl Drop for Dispatching {
    fn drop(&mut self) {
        DISPATCHING.with_borrow_mut(Vec::pop);
    }
}

/// Mint a dispatch id, send `run` down `transport`, and drain events to the
/// run's terminal [`Report`](Event::Report).
///
/// `host` is the host's side of this run. Under the identity transport it
/// rides straight onto the dispatch as the run's desk (`IdentityDesk`), so an
/// enquiry never appears on this loop at all; under the wire, where an
/// enquiry crosses as a frame on `events()`, this loop is what answers it,
/// through [`Transport::answer`].
///
/// The `did != id` filter rejects a cancelled predecessor's late run frames,
/// and nothing else: every `Event` belongs to some dispatch, and this is the
/// one whose Report is awaited. A session-lived batch never reaches here at
/// all — it rides `Frame::Session`, delivered to whatever sink
/// `Transport::set_deferred_sink` installed.
///
/// # Errors
/// The transport's severance: recorded already, or met while draining.
///
/// # Panics
/// If a `Host` handler reaches back into `transport` from inside this
/// dispatch. The `expect` on a closed event stream never fires: a severed
/// cause is always recorded before `event_tx` drops.
#[allow(
    clippy::needless_pass_by_value,
    reason = "an owned Arc mirrors Transport::dispatch's own host handoff; the body only ever borrows it"
)]
pub fn dispatch_to_report(
    transport: &dyn Transport,
    run: Run,
    host: Arc<dyn Host>,
) -> Result<Report, Severed> {
    let _dispatching = Dispatching::enter(transport);
    if let Some(cause) = transport.severed() {
        return Err(cause);
    }
    let id = mint_dispatch_id();

    transport.dispatch(id, run, &host);

    while let Some((did, event)) = transport.events().recv() {
        if did != id {
            continue;
        }
        match event {
            Event::Surface(val) => host.surface(&val),
            Event::Enquiry(eid, req) => {
                let answer = host.enquire(req);
                transport.answer(eid, answer);
            }
            Event::Reading(_) => {
                return Err(transport.sever(Severed::Faulted(
                    "it answered a dispatch with a probe's reading".into(),
                )));
            }
            Event::Report(report) => return Ok(report),
        }
    }
    Err(transport.severed().expect(READER_SEVERS_FIRST))
}

/// A closed event stream always has a cause: every reader exit path severs
/// before it drops the sender.
const READER_SEVERS_FIRST: &str = "the reader severs before it closes the event stream";

/// Dispatches and probes share this mint: both are answered by an [`Event`]
/// under a [`DispatchId`], so two counters would let a probe's reading be
/// mistaken for a dispatch's.
fn mint_dispatch_id() -> DispatchId {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    DispatchId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

// ── Senders and receivers ─────────────────────────────────────────────

/// A wire sender's write door and the severance cell a failed write records
/// into.
type WireControl = (Arc<Mutex<WireChannel>>, Arc<OnceLock<Severed>>);

/// The out-of-band control door: [`Control`]'s verbs, meaning the same under
/// either carrier.
#[derive(Clone)]
pub struct ControlSender(Door);

#[derive(Clone)]
enum Door {
    /// In-process: straight onto the engine's own scopes.
    Identity(Arc<Scopes>),
    /// A `Control` frame, recording into the transport's severance cell, so a
    /// failed control write severs the connection as a failed data write does.
    Wire(WireControl),
}

impl ControlSender {
    /// Unwind whatever dispatch is in flight; none in flight, nothing happens.
    pub fn interrupt(&self) {
        self.send(Control::Interrupt);
    }

    /// Unwind dispatch `id`, even one the engine has not yet seen.
    pub fn cancel(&self, id: DispatchId) {
        self.send(Control::Cancel(id));
    }

    /// End every run and every detached worker of the session, for good.
    pub fn terminate(&self) {
        self.send(Control::Terminate);
    }

    /// Hear this process's signals as this session's `Control` until the
    /// guard drops: no engine folds them itself.
    pub fn forward_signals(&self) -> crate::process::AmbientForward {
        let door = self.clone();
        crate::process::forward_ambient(move |ambient| door.send(Control::hearing(ambient)))
    }

    fn send(&self, control: Control) {
        match &self.0 {
            Door::Identity(scopes) => scopes.apply(control),
            // A control frame that cannot be written is as fatal as a dispatch
            // that cannot: the peer is gone, and waiting on the reader's
            // eventual EOF to say so leaves a cancel silently lost meanwhile.
            Door::Wire((ch, severance)) => {
                let _ = write_through(ch, severance, &Frame::Control(control));
            }
        }
    }
}

/// The front-end drains this while a dispatch is outstanding; `Surface` events
/// are ordered before the `Report`.
///
/// Admits exactly one drainer at a time — [`dispatch_to_report`]'s loop and a
/// desk's own pre-drain (`IdentityDesk::enquire`) never run together, because
/// the desk call happens synchronously inside `Transport::dispatch`, on the
/// same thread, strictly before the loop's own first `recv`. The `stash` and
/// `rx` mutexes exist only to make
/// `mpsc::Receiver` `Sync` across that single drainer; they do not arbitrate
/// between two, and nothing here enforces the invariant — it is a scheduling
/// fact about the callers, not a property of the locks.
pub struct EventReceiver {
    rx: std::sync::Mutex<mpsc::Receiver<(DispatchId, Event)>>,
    /// Events a probe's drain read past on its way to its own Report — a
    /// deferred worker's batch settling mid-probe, say. Handed back to the
    /// next `recv` in arrival order rather than dropped.
    stash: std::sync::Mutex<std::collections::VecDeque<(DispatchId, Event)>>,
}

impl EventReceiver {
    fn new(rx: mpsc::Receiver<(DispatchId, Event)>) -> Self {
        Self {
            rx: std::sync::Mutex::new(rx),
            stash: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub fn recv(&self) -> Option<(DispatchId, Event)> {
        let stashed = self.stash.lock_ignore_poison().pop_front();
        if let Some(item) = stashed {
            return Some(item);
        }
        self.rx.lock_ignore_poison().recv().ok()
    }

    /// Non-blocking on the channel: the stash first, then one
    /// `mpsc::Receiver::try_recv`. `None` when both are empty right now.
    ///
    /// This is non-blocking only with respect to the channel, not the lock:
    /// [`Self::recv`] holds the same `rx` mutex across its own blocking
    /// receive, so a `try_recv` that ran concurrently with an outstanding
    /// `recv` would park on the mutex for as long as that `recv` blocks.
    /// Nothing here makes that safe — see the type's own doc for why no two
    /// callers may drain at once.
    pub(crate) fn try_recv(&self) -> Option<(DispatchId, Event)> {
        let stashed = self.stash.lock_ignore_poison().pop_front();
        if let Some(item) = stashed {
            return Some(item);
        }
        self.rx.lock_ignore_poison().try_recv().ok()
    }
}

/// The front-end's half of the severance law: [`write_or_sever`]
/// recording into `severance`. Shared by the wire transport's writes and the
/// wire arm of `ControlSender`; `Err` is handed back so a caller that must
/// react further — the heartbeat's `Ping` arm breaking its loop — can, without
/// repeating the severing itself.
fn write_through(
    ch: &Mutex<WireChannel>,
    severance: &OnceLock<Severed>,
    frame: &Frame,
) -> io::Result<()> {
    write_or_sever(ch, frame, |e| {
        sever(severance, Severed::Closed(e.to_string()));
    })
}
