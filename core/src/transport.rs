//! Transport-parametric host seam — the frame algebra that separates a
//! front-end from the ral engine.
//!
//! A "frame" names one of the four rails that cross between front-end and
//! engine: Attach/Detach bracket a session; Dispatch carries one whole
//! turn; Event flows engine→front-end; Control flows front-end→engine.
//!
//! Under the **identity transport** (Phase 1) the channel is a direct Rust
//! call: frames are passed by move, never encoded.  The `Transport` trait
//! is the front-end handle; its engine-side dual runs the matching door
//! synchronously on the calling thread.  Surface events are written to a
//! channel the front-end drains; control frames trip the thread-safe
//! `CancelScope` directly.
//!
//! Phase 2 will encode these same frame types through `subprocess_codec`
//! across a process boundary.
//!
//! See [[decisions/260628_host-seam-transport-parametric]].
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

#[cfg(unix)]
use crate::process::ChildHandle;
use crate::serial::FOValue;
use crate::types::DeferredSink;
use crate::types::SurfaceSink;
use std::sync::OnceLock;

pub const PROTOCOL_VERSION: u32 = 2;

static CURRENT_CONTROL: OnceLock<ControlSender> = OnceLock::new();

// ── Dispatch identity ─────────────────────────────────────────────────

/// Correlation token for one outstanding dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DispatchId(pub u64);

/// Correlation token for one outstanding enquiry. Unused under the identity
/// transport (a direct call, no frames to correlate); minted per enquiry by
/// the wire engine's desk and carried on both `Event::Enquiry` and its
/// answering `Frame::Answer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnquiryId(pub u64);

// ── Frame algebra ─────────────────────────────────────────────────────

/// One frame that crosses the host seam in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// Front-end conveys the terminal endpoint, session bootstrap, and protocol version.
    Attach {
        /// The terminal endpoint for IO setup.
        endpoint: TerminalEndpoint,
        /// Working directory the engine should start in.
        cwd: PathBuf,
        /// Home directory for `~` expansion.
        home: PathBuf,
        /// Optional rc file to load at session start.
        rc_path: Option<PathBuf>,
        /// Protocol version the front-end speaks (checked against PROTOCOL_VERSION).
        proto_version: u32,
    },
    /// Front-end drops: cancel in-flight dispatch, reap foreground
    /// subtree, restore terminal state.
    Detach,
    /// One whole turn.  Boxed so every other `Frame` variant is not sized
    /// to `Turn`'s stack footprint.
    Dispatch(DispatchId, Box<Turn>),
    /// Engine → front-end. May arrive while a Dispatch is outstanding.
    Event(DispatchId, Event),
    /// Front-end → engine. Out-of-band: deliverable while a Dispatch is
    /// outstanding.
    Control(Control),
    /// Front-end → engine, answering one enquiry a running dispatch raised.
    /// The first answered frame in this direction — the dual of
    /// `Event::Enquiry`, correlated by both the dispatch and the enquiry.
    /// The error arm keeps message *and* status so a refused enquiry raises
    /// the same error under both transports.
    Answer(DispatchId, EnquiryId, Result<FOValue, EnquiryError>),
}

/// One whole turn: the program to run and the conditions it runs under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// What the turn runs.
    pub program: Program,
    /// Label for the root source context (`"<stdin>"` for the REPL,
    /// `"<tool>"` for exarch).
    pub script_name: String,
    /// The capability ceiling pushed for the eval's dynamic extent.
    /// `Capabilities::root()` is the ⊤ element — the identity on authority
    /// (the REPL) — while a narrower profile attenuates the session's grant
    /// (exarch).
    pub caps: crate::types::Capabilities,
    /// The turn's foreground wall: `Some(d)` arms a `Deadline` cancel on the
    /// turn's foreground scope `d` after it starts (exarch's per-tool wall);
    /// `None` leaves the turn uncapped (the REPL).
    pub turn_limit: Option<std::time::Duration>,
    /// The lease for workers the turn defers at the durable root. `None`
    /// (the interactive ral host) leaves a worker until `cancel`, root
    /// abort, or session exit; `Some(lease)` (an agent host) reaps a
    /// still-running worker once unobserved for `lease.idle` — renewed by
    /// every `poll`/`await`/`race` naming its handle — under the
    /// `lease.backstop` absolute ceiling.
    pub deferred_lease: Option<crate::types::WorkerLease>,
    /// Admission cap on concurrently running workers, enforced at the spawn
    /// door: with `Some(cap)` a spawn is refused while `cap` workers of any
    /// class are still running — settled entries lingering under retention
    /// never block admission. `None` (the interactive ral host) admits
    /// freely.
    pub worker_cap: Option<usize>,
    /// The byte IO regime.
    pub io: crate::driver::TurnIo,
    /// Whether the turn may hand the controlling terminal to a child.
    pub terminal: crate::driver::RequestedTerminalAccess,
    /// Stdin source.
    pub stdin: crate::driver::TurnStdin,
}

/// What a turn runs: source text compiled and typechecked against the live
/// session, or a registered hook applied to first-order arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Program {
    Source(String),
    Hook {
        name: crate::types::HookName,
        args: Vec<FOValue>,
    },
}

/// Engine → front-end event frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// A live surface value from the foreground turn, ordered before this
    /// dispatch's Report.
    Surface(FOValue),
    /// A deferred worker's surface batch, delivered when it settles and
    /// rendered by the host at the next turn boundary.
    DeferredSurface(Vec<FOValue>),
    /// One enquiry the running dispatch raised on its host desk, nested
    /// inside this dispatch and ordered before its Report. Answered by a
    /// `Frame::Answer` carrying the same `EnquiryId`.
    Enquiry(EnquiryId, FOValue),
    /// The dispatch's sole terminal frame.
    Report(Report),
}

/// What crosses the wire when an enquiry is refused or its handler fails.
/// Kept as message *and* status (never collapsed into `crate::types::Error`,
/// which also carries a `loc` core has no business shipping across the
/// seam) so a refused enquiry raises the same error under both transports;
/// its location is stamped engine-side, at the enquiring builtin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnquiryError {
    pub message: String,
    pub status: i32,
}

/// Front-end → engine out-of-band control frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Cancel the named dispatch.
    Cancel(DispatchId),
    /// Suspend the engine (SIGTSTP semantics).
    Suspend,
    /// Resume after Suspend.
    Resume,
    /// Terminal size changed.
    Resize(Winsize),
}

/// Terminal window size in rows × columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
}

// ── Terminal endpoint ─────────────────────────────────────────────────

/// The terminal endpoint the front-end conveys at attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEndpoint {
    /// The session lease, if the front-end owns a terminal.
    #[serde(skip)]
    pub lease: Option<crate::process::TerminalLease>,
    /// The terminal state for IO setup.
    pub state: crate::io::TerminalState,
}

// ── The terminal frame ────────────────────────────────────────────────

/// The turn's terminal frame: the protocol projection of the engine's
/// [`TurnReport`](crate::driver::TurnReport), produced by
/// [`TurnReport::into_report`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Report {
    /// Parse/type/host failure: the turn never reached evaluation. The host
    /// renders the diagnostics and treats the turn as status 1.
    Static { diagnostics: Diagnostics },
    /// The turn ran to a settled result. `status` is the transport status
    /// computed once; `single_command` is whether the source compiled to a
    /// single command (for runtime-error rendering); `captured` is `Some`
    /// under [`TurnIo::Capture`](crate::driver::TurnIo); `timed_out` is
    /// whether the wall fired.
    Ran {
        /// The turn's settled result. `Ok` carries a [`FOValue`] so a
        /// successful result crosses the wire; a non-transportable result
        /// (e.g. a live `Handle`) is reported as an error instead.
        result: Result<FOValue, Break>,
        status: i32,
        single_command: bool,
        captured: Option<crate::driver::Captured>,
        timed_out: bool,
    },
}

/// Rendered diagnostics from a turn that never ran: the protocol projection
/// of [`StaticDiagnostics`](crate::turn::StaticDiagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Diagnostics {
    Parse(String),
    Types(Vec<String>),
    Host(String),
}

/// First-order projection of a settled turn's
/// [`Break`](crate::types::Break).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Break {
    Error(String),
    Exit(i32),
    #[cfg(unix)]
    Stopped {
        pgid: i32,
        signal: i32,
        signal_name: String,
    },
}

impl From<crate::types::Break> for Break {
    fn from(break_: crate::types::Break) -> Break {
        use crate::types::{Break as EngineBreak, Escape};
        match break_ {
            EngineBreak::Error(e) => Break::Error(e.message),
            EngineBreak::Escape(esc) => match esc {
                Escape::Exit(code) => Break::Exit(code.clamp(0, 255)),
                #[cfg(unix)]
                Escape::Stopped { pgid, signal, .. } => Break::Stopped {
                    pgid: pgid.0,
                    signal: signal.number(),
                    signal_name: signal.name().unwrap_or("?").to_string(),
                },
            },
        }
    }
}

impl crate::driver::TurnReport {
    /// Project into the protocol [`Report`]: the live `Value` becomes a
    /// first-order [`FOValue`] (a non-transportable result — e.g. a live
    /// `Handle` — is reported as an error rather than dropped silently),
    /// and rich diagnostics render to strings.  The one lossy step between
    /// the engine and the seam.
    pub fn into_report(self) -> Report {
        use crate::driver::TurnReport;
        use crate::turn::StaticDiagnostics;
        match self {
            TurnReport::Static { diagnostics } => Report::Static {
                diagnostics: match diagnostics {
                    StaticDiagnostics::Parse(e) => Diagnostics::Parse(e.message),
                    StaticDiagnostics::Types(errs) => {
                        Diagnostics::Types(errs.iter().map(|e| e.to_string()).collect())
                    }
                    StaticDiagnostics::Host(e) => Diagnostics::Host(e.message),
                },
            },
            TurnReport::Ran {
                result,
                status,
                single_command,
                captured,
                timed_out,
            } => Report::Ran {
                result: match result {
                    Ok(v) => match FOValue::try_from(&v) {
                        Ok(fo) => Ok(fo),
                        Err(_) => Err(Break::Error(
                            "turn result is not transportable across the host seam".into(),
                        )),
                    },
                    Err(break_) => Err(break_.into()),
                },
                status,
                single_command,
                captured,
                timed_out,
            },
        }
    }
}

// ── Transport trait ───────────────────────────────────────────────────

/// The front-end side of the host seam.
pub trait Transport: Send + Sync {
    /// Run a dispatch synchronously.  The `Report` arrives as the final
    /// `Event`; `Surface` events are written to the event channel during
    /// execution and may be drained concurrently.
    fn dispatch(&self, id: DispatchId, turn: Turn);

    /// The out-of-band control sender — writable while a dispatch is in
    /// flight.  Under the identity transport this writes directly to the
    /// foreground `CancelScope`.
    fn control(&self) -> &ControlSender;

    /// The event stream the front-end drains.
    fn events(&self) -> &EventReceiver;

    /// Convey the session terminal endpoint and bootstrap state.
    fn attach(
        &self,
        endpoint: TerminalEndpoint,
        cwd: PathBuf,
        home: PathBuf,
        rc_path: Option<PathBuf>,
    );

    /// Detach: cancel in-flight dispatch, reap foreground subtree,
    /// restore terminal state.
    fn detach(&self);

    /// Answer one enquiry the engine raised on `Event::Enquiry`, correlated
    /// by `id` and `eid`. A no-op under the identity transport: an identity
    /// enquiry is a direct call through the installed `Desk`, never a frame,
    /// so there is nothing here to write.
    fn answer(&self, _id: DispatchId, _eid: EnquiryId, _answer: Result<FOValue, EnquiryError>) {}

    /// Returns `self` as an `&dyn Any` for downcast support.
    fn as_any(&self) -> &dyn std::any::Any;
}

// ── Dispatch loop ─────────────────────────────────────────────────────

/// Mint a dispatch id, send `turn` down `transport`, and drain the event
/// stream to the turn's terminal [`Report`](Event::Report), handing each
/// surface regime to its caller-supplied handler.
///
/// The two surface regimes stay distinct on purpose: `on_surface` sees each
/// live [`Event::Surface`] value in order, while `on_deferred` sees a
/// deferred worker's [`Event::DeferredSurface`] batch delivered at its
/// settlement. A host that tracks live pins (exarch) must update its mirror
/// only from `on_surface` — the deferred batch does not touch the pin map —
/// so flattening the two into one undistinguished stream would be a
/// behaviour change, not a fold.
///
/// `on_enquiry` answers one [`Event::Enquiry`] this dispatch's turn raised on
/// its host desk; the answer is written back through
/// [`Transport::answer`]. Under the identity transport `Event::Enquiry` never
/// arrives here at all — the installed `Desk` is a direct call the turn makes
/// mid-evaluation (§3 of the enquiry-channel ADR), so `on_enquiry` is dead
/// code on that transport and live only under the wire, where the loop is
/// the front-end's side of the desk's rendezvous.
///
/// Returns the [`Report`], or `None` if the stream closed without a
/// Report (impossible under the identity transport, which sends the Report
/// synchronously before `dispatch` returns; each caller keeps its own
/// no-Report handling).
pub fn dispatch_to_report(
    transport: &dyn Transport,
    turn: Turn,
    mut on_surface: impl FnMut(FOValue),
    mut on_deferred: impl FnMut(Vec<FOValue>),
    mut on_enquiry: impl FnMut(FOValue) -> Result<FOValue, EnquiryError>,
) -> Option<Report> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = DispatchId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));

    transport.dispatch(id, turn);

    while let Some(frame) = transport.events().recv() {
        match frame {
            Frame::Event(did, event) if did == id => match event {
                Event::Surface(val) => on_surface(val),
                Event::DeferredSurface(batch) => on_deferred(batch),
                Event::Enquiry(eid, req) => {
                    let answer = on_enquiry(req);
                    transport.answer(id, eid, answer);
                }
                Event::Report(report) => return Some(report),
            },
            _ => {}
        }
    }
    None
}

// ── Senders and receivers ─────────────────────────────────────────────

/// Out-of-band control sender.
#[derive(Clone)]
pub struct ControlSender {
    /// When `Some`, writes `Control` frames to a `WireChannel`.
    /// When `None`, acts directly on the in-process foreground scope.
    #[cfg(unix)]
    wire: Option<Arc<Mutex<crate::wire::WireChannel>>>,
}

impl ControlSender {
    pub(crate) fn new() -> Self {
        ControlSender {
            #[cfg(unix)]
            wire: None,
        }
    }

    #[cfg(unix)]
    pub(crate) fn new_wire(ch: Arc<Mutex<crate::wire::WireChannel>>) -> Self {
        ControlSender { wire: Some(ch) }
    }

    pub(crate) fn publish(self) {
        let _ = CURRENT_CONTROL.set(self);
    }

    pub fn cancel_current() {
        if let Some(ctrl) = CURRENT_CONTROL.get() {
            ctrl.send(Control::Cancel(DispatchId(0)));
        }
    }

    pub fn send(&self, ctrl: Control) {
        #[cfg(unix)]
        if let Some(ch) = &self.wire {
            let frame = Frame::Control(ctrl);
            // Phase 2 Task 8: EPIPE here means the engine died.
            // A full teardown needs a shared death flag.
            let _ = ch.lock().unwrap().write_frame(&frame);
            return;
        }
        match ctrl {
            Control::Cancel(_id) => {
                // Trip the signal-reachable foreground scope — the same
                // static the SIGINT relay writes to.  This is the actual
                // scope the turn runs under, not a transport-side sibling.
                crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
            }
            Control::Suspend | Control::Resume | Control::Resize(_) => {
                // Under the identity transport these are handled by the
                // process-level signal machinery in-process.
            }
        }
    }

}

/// Event receiver.  The front-end drains this while a dispatch is
/// outstanding; `Surface` events are ordered before the `Report`.
pub struct EventReceiver {
    rx: std::sync::Mutex<mpsc::Receiver<Frame>>,
}

impl EventReceiver {
    pub fn recv(&self) -> Option<Frame> {
        self.rx.lock().unwrap().recv().ok()
    }
}

// ── Transport sink ────────────────────────────────────────────────────

/// The identity transport's sink for both surface regimes: a live value
/// ([`EventSink`](crate::types::EventSink)) and a deferred worker's settled
/// batch ([`DeferredSink`]), each forwarded onto the event channel stamped
/// with the in-flight dispatch id.
struct TransportSink {
    event_tx: mpsc::Sender<Frame>,
    /// The in-flight dispatch id (`0` = none), the one atomic `dispatch`
    /// sets and both impls read. A live surface value emitted while it reads
    /// `0` has no dispatch to correlate to and is dropped; a deferred batch
    /// is stamped with whatever is in flight — `0` when it settles between
    /// turns.  // Phase 2: correlate by DispatchId
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

impl crate::types::EventSink for TransportSink {
    fn emit(&self, ev: &FOValue) {
        let id = self
            .current_dispatch
            .load(std::sync::atomic::Ordering::Relaxed);
        if id != 0 {
            let _ = self
                .event_tx
                .send(Frame::Event(DispatchId(id), Event::Surface(ev.clone())));
        }
    }
}

impl crate::types::DeferredSink for TransportSink {
    fn deliver(&self, batch: Vec<FOValue>) {
        let _ = self.event_tx.send(Frame::Event(
            DispatchId(
                self.current_dispatch
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            Event::DeferredSurface(batch),
        ));
    }
}

// ── Identity transport ────────────────────────────────────────────────

/// A session lock that cannot poison-panic.  A turn that unwinds drops its
/// guard mid-mutation, which would otherwise poison the mutex and turn every
/// later `lock()` into a second panic (`PoisonError`); recovering the guard
/// instead means a panicking turn can never wedge the session.  This is the
/// only lock over the engine state, so the failure is impossible by
/// construction — there is no `unwrap()` to forget.
struct SessionLock(std::sync::Mutex<EngineInner>);

impl SessionLock {
    fn new(inner: EngineInner) -> Self {
        Self(std::sync::Mutex::new(inner))
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, EngineInner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Consume the lock and recover the engine state, recovering poison the
    /// same way `lock` does — a turn that unwound mid-mutation must not wedge
    /// the move-out.
    fn into_inner(self) -> EngineInner {
        self.0.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// The in-process transport: one kernel, one address space.
///
/// `dispatch` runs the matching door synchronously on the calling thread.
/// Surface events are written to the event channel during execution;
/// control frames trip the foreground `CancelScope` directly.
pub struct IdentityTransport {
    /// The engine-side state, behind a poison-free [`SessionLock`] so
    /// `dispatch` can take `&self`.
    engine: SessionLock,
    /// Control sender (wired to the foreground cancel scope).
    control: ControlSender,
    /// Event receiver for the front-end.
    events_recv: EventReceiver,
    /// Stamped with the dispatching thread's id for the duration of
    /// `dispatch`, so a desk handler that reenters the session lock
    /// (`dispatch`/`shell_mut`/`with_shell`) panics instead of deadlocking
    /// (§3's reentrancy law). A separate short lock, never the session
    /// lock — checking it must not touch `self.engine`.
    dispatch_thread: std::sync::Mutex<Option<std::thread::ThreadId>>,
}

pub struct EngineInner {
    /// The shell that runs turns.
    pub shell: crate::types::Shell,
    /// Send events to the front-end.
    pub(crate) event_tx: mpsc::Sender<Frame>,
    /// The transport sink, serving both surface regimes.
    surface_sink: Arc<TransportSink>,
    /// The session-lived deferred sink for deferred workers.  Seeded with
    /// the transport sink; a host may install its own (`set_deferred_sink`).
    deferred_sink: Option<Arc<dyn DeferredSink>>,
    /// The installed enquiry desk, if any host has set one. `None` in
    /// Phase A: no desk installed by any host, so `enquire` answers its
    /// honest absence error.
    desk: Option<crate::types::Desk>,
    /// The session terminal lease (set by Attach).
    terminal_lease: Option<crate::process::TerminalLease>,
    /// Shared dispatch id for deferred-sink correlation.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

/// Clears the dispatch-thread stamp on drop, even on unwind — a turn that
/// panics must not leave the stamp set, or the next legitimate
/// `shell_mut`/`with_shell` call would false-trip the reentrancy panic.
struct DispatchStampGuard<'a> {
    slot: &'a std::sync::Mutex<Option<std::thread::ThreadId>>,
}

impl Drop for DispatchStampGuard<'_> {
    fn drop(&mut self) {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl IdentityTransport {
    /// Create a new identity transport that owns `shell`.
    pub fn new(shell: crate::types::Shell) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let current_dispatch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink = Arc::new(TransportSink {
            event_tx: event_tx.clone(),
            current_dispatch: current_dispatch.clone(),
        });
        let control = ControlSender::new();
        control.clone().publish();

        let engine = EngineInner {
            shell,
            event_tx,
            surface_sink: sink.clone(),
            deferred_sink: Some(sink),
            desk: None,
            terminal_lease: None,
            current_dispatch,
        };

        IdentityTransport {
            engine: SessionLock::new(engine),
            control,
            events_recv: EventReceiver {
                rx: std::sync::Mutex::new(event_rx),
            },
            dispatch_thread: std::sync::Mutex::new(None),
        }
    }

    /// Set the session deferred sink for deferred worker batches.
    pub fn set_deferred_sink(&self, deferred: Arc<dyn DeferredSink>) {
        self.engine.lock().deferred_sink = Some(deferred);
    }

    /// Install the session's enquiry desk. Per-turn hosts (e.g. exarch, once
    /// the migration lands) call this before each dispatch so whatever the
    /// desk captures is fresh — the same reasoning `set_deferred_sink`
    /// follows.
    pub fn set_desk(&self, desk: crate::types::Desk) {
        self.engine.lock().desk = Some(desk);
    }

    /// Consume the transport and recover the owned `Shell`.  The inverse of
    /// [`IdentityTransport::new`]: a caller that handed a shell in to route a
    /// turn through production can move it back out afterward.
    pub fn into_shell(self) -> crate::types::Shell {
        self.engine.into_inner().shell
    }

    /// Panic loudly if called from the thread currently running a dispatch —
    /// the reentrancy law (§3): a desk handler that reaches back through
    /// `dispatch`/`shell_mut`/`with_shell` would deadlock on the session lock
    /// its own stack already holds. Checked *before* touching `self.engine`,
    /// so a reentrant call panics instead of hanging.
    fn check_not_reentrant(&self) {
        let current = std::thread::current().id();
        if *self
            .dispatch_thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == Some(current)
        {
            panic!(
                "reentrant session access: a desk handler must not take the session lock \
                 (dispatch/shell_mut/with_shell) — it runs inside the dispatch on the \
                 dispatching thread and would deadlock under the identity transport"
            );
        }
    }

    /// Access the underlying `Shell` through the mutex guard directly.
    /// This is needed when the caller must combine shell access with
    /// other borrows (e.g. the REPL session's frontend, jobs table).
    /// Prefer `with_shell` for simple operations.
    pub fn shell_mut(&self) -> std::sync::MutexGuard<'_, EngineInner> {
        self.check_not_reentrant();
        self.engine.lock()
    }

    /// Access the underlying `Shell` mutably for session bootstrap.
    pub fn with_shell<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut crate::types::Shell) -> R,
    {
        self.check_not_reentrant();
        f(&mut self.engine.lock().shell)
    }
}

impl Transport for IdentityTransport {
    fn dispatch(&self, id: DispatchId, turn: Turn) {
        self.check_not_reentrant();
        let mut engine = self.engine.lock();
        *self
            .dispatch_thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::thread::current().id());
        let _stamp_guard = DispatchStampGuard {
            slot: &self.dispatch_thread,
        };

        // Mark the in-flight dispatch: the one atomic both the surface and
        // deferred sinks read to stamp and gate their frames.
        engine
            .current_dispatch
            .store(id.0, std::sync::atomic::Ordering::Relaxed);

        // The live handles this dispatch lends the turn, joined with the
        // protocol `Turn` in the request the engine door runs.
        let req = crate::driver::TurnRequest {
            turn,
            surface: Some(engine.surface_sink.clone() as SurfaceSink),
            deferred: engine.deferred_sink.clone(),
            desk: engine.desk.clone(),
            lifecycle: Box::new(()),
        };
        let report = engine.shell.run_turn(req).into_report();

        engine
            .current_dispatch
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let _ = engine
            .event_tx
            .send(Frame::Event(id, Event::Report(report)));
    }

    fn control(&self) -> &ControlSender {
        &self.control
    }

    fn events(&self) -> &EventReceiver {
        &self.events_recv
    }

    fn attach(
        &self,
        endpoint: TerminalEndpoint,
        _cwd: PathBuf,
        _home: PathBuf,
        _rc_path: Option<PathBuf>,
    ) {
        let mut engine = self.engine.lock();
        engine.terminal_lease = endpoint.lease;
    }

    fn detach(&self) {
        // Cancel any in-flight turn via the signal-reachable scope.
        crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
        self.engine.lock().terminal_lease = None;
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Wire transport ────────────────────────────────────────────────────

/// The out-of-process transport: engine runs as a child process,
/// frames cross on a `WireChannel` (length-prefixed JSON over a
/// Unix socketpair).
///
/// A dedicated reader thread forwards `Event` frames from the wire
/// into the `EventReceiver` the front-end drains.  Writes — `Dispatch`,
/// `Attach`, `Detach`, `Control` — go through a shared `Mutex<WireChannel>`
/// so they are concurrent-safe with the reader thread.
#[cfg(unix)]
pub struct WireTransport {
    /// Event receiver fed by the reader thread.
    events_recv: EventReceiver,
    /// Control sender that writes `Control` frames to the wire.
    control: ControlSender,
    /// Shared write end, behind a mutex for concurrent access.
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    /// The engine child process.
    _child: ChildHandle,
    /// Reader thread handle (never joined — the thread exits when the
    /// channel closes).  Wrapped in a `Mutex<Option<…>>` for `Sync`.
    _reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set when the engine dies (EPIPE on write).  The reader thread
    /// checks this to break early and close the event channel.
    death: Arc<AtomicBool>,
}

#[cfg(unix)]
impl WireTransport {
    /// Spawn the engine child and set up the wire channel.
    ///
    /// Creates a `WireChannel` socketpair, passes one end to the engine
    /// child as fd 3, and starts a reader thread that funnels incoming
    /// `Event` frames into the event receiver.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:engine-spawn] spawns the engine child process for the wire transport; an infrastructure handoff, not turn-time data I/O"
    )]
    pub fn new() -> io::Result<Self> {
        let (frontend, engine) = crate::wire::WireChannel::pair()?;

        // Duplicate the front-end fd so we can read on one handle
        // and write on the other concurrently.
        let writer = frontend.try_clone()?;

        // Spawn the engine child with the engine end of the socket on fd 3.
        let engine_fd = engine.as_raw_fd();
        let mut cmd =
            std::process::Command::new(std::env::current_exe().map_err(io::Error::other)?);
        cmd.arg("--engine");
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::dup2(engine_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Close the original fd — only fd 3 should remain.
                libc::close(engine_fd);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // Engine end no longer needed in the parent.
        drop(engine);

        // Death flag: set on EPIPE so the reader knows to tear down.
        let death = Arc::new(AtomicBool::new(false));
        let death_reader = death.clone();

        // Event channel: reader thread writes, front-end drains.
        let (event_tx, event_rx) = mpsc::channel();

        // Reader thread: loop reading frames, forward Events.
        let mut reader_ch = frontend;
        let reader = std::thread::spawn(move || {
            loop {
                if death_reader.load(Ordering::SeqCst) {
                    break;
                }
                match reader_ch.read_frame() {
                    Ok(Some(Frame::Event(id, ev))) => {
                        if event_tx.send(Frame::Event(id, ev)).is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
                    // Ignore non-Event frames on the read side —
                    // the engine only sends Events.
                    _ => {}
                }
            }
        });

        let write_tx = Arc::new(Mutex::new(writer));
        let control = ControlSender::new_wire(write_tx.clone());

        Ok(WireTransport {
            events_recv: EventReceiver {
                rx: Mutex::new(event_rx),
            },
            control,
            write_tx,
            _child: ChildHandle::from_std(child),
            _reader: Mutex::new(Some(reader)),
            death,
        })
    }

    /// Write a frame to the wire channel.
    fn write(&self, frame: &Frame) {
        match self.write_tx.lock().unwrap().write_frame(frame) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.death.store(true, Ordering::SeqCst);
                eprintln!("wire: engine process died (EPIPE)");
            }
            Err(_) => {} // other write errors are non-fatal
        }
    }
}

#[cfg(unix)]
impl Drop for WireTransport {
    fn drop(&mut self) {
        // Kill the engine child so the reader thread sees EOF and exits.
        let _ = self._child.kill();
        let _ = self._child.wait_handling_stop(None, false);
    }
}

#[cfg(unix)]
impl Transport for WireTransport {
    fn dispatch(&self, id: DispatchId, turn: Turn) {
        self.write(&Frame::Dispatch(id, Box::new(turn)));
    }

    fn control(&self) -> &ControlSender {
        &self.control
    }

    fn events(&self) -> &EventReceiver {
        &self.events_recv
    }

    /// TODO Phase 2 Task 6: When the endpoint has a lease, pass the
    /// controlling terminal fds over the socket via SCM_RIGHTS (Unix
    /// ancillary data).  For now the endpoint's lease field is
    /// `#[serde(skip)]` and the engine stores it as a placeholder.
    fn attach(
        &self,
        endpoint: TerminalEndpoint,
        cwd: PathBuf,
        home: PathBuf,
        rc_path: Option<PathBuf>,
    ) {
        self.write(&Frame::Attach {
            endpoint,
            cwd,
            home,
            rc_path,
            proto_version: PROTOCOL_VERSION,
        });
    }

    fn detach(&self) {
        self.write(&Frame::Detach);
    }

    fn answer(&self, id: DispatchId, eid: EnquiryId, answer: Result<FOValue, EnquiryError>) {
        self.write(&Frame::Answer(id, eid, answer));
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Enquiry desk tests ────────────────────────────────────────────────
//
// Phase A installs no desk anywhere in production (§"the exarch installation
// point" of the enquiry-channel ADR): the rail is exercised only here, by a
// stub desk. There is no `enquire` builtin yet, so a turn's body (parsed ral
// source) has no way to call `Shell::enquire` itself; the round-trip test
// below drives it the same way the turn door's own lifecycle
// tests do — a `TurnLifecycle` given `&mut Shell` mid-turn, exactly the
// shape the migration's first handler will run under.
#[cfg(test)]
mod enquiry_tests {
    use super::*;
    use crate::driver::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};
    use crate::turn::TurnLifecycle;
    use crate::types::{Capabilities, Desk, EnquiryDesk, Error, Shell};
    use std::sync::Mutex;

    /// The minimal capturing request under the ⊤ capability ceiling: no
    /// surface, no deferred sink, no desk — matches `turn.rs`'s own
    /// `capture_req`.
    fn capture_req<'a>(src: &str) -> TurnRequest<'a> {
        TurnRequest {
            turn: Turn {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: TurnStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            lifecycle: Box::new(()),
        }
    }

    /// A `Shell` with no desk installed answers every enquiry with the
    /// honest absence error, verbatim.
    #[test]
    fn absent_desk_answers_the_honest_error() {
        let shell = Shell::new(Default::default());
        let err = shell
            .enquire(FOValue::Unit)
            .expect_err("no desk is installed");
        assert_eq!(err.message, "this host answers no enquiries");
    }

    /// A stub desk that maps `Int{n}` to `Int{n+1}`, otherwise echoes.
    struct IncrementDesk;
    impl EnquiryDesk for IncrementDesk {
        fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
            match req {
                FOValue::Int { value } => Ok(FOValue::Int { value: value + 1 }),
                other => Ok(other),
            }
        }
    }

    /// A `TurnLifecycle` that enquires mid-turn (`pre_exec`, which runs with
    /// the turn frame — and its desk — installed) and records the answer.
    #[derive(Clone)]
    struct AskDuringTurn {
        req: FOValue,
        answer: std::sync::Arc<Mutex<Option<Result<FOValue, Error>>>>,
    }
    impl TurnLifecycle for AskDuringTurn {
        fn pre_exec(&mut self, shell: &mut Shell, _src: &str) {
            *self.answer.lock().unwrap() = Some(shell.enquire(self.req.clone()));
        }
    }

    /// `Shell::enquire` round-trips through a stub desk installed on the
    /// turn: the desk's transform (`Int{41} -> Int{42}`) is visible to the
    /// caller of `enquire`, not just to the desk.
    #[test]
    fn enquire_round_trips_through_a_stub_desk() {
        let mut shell = Shell::new(Default::default());
        let answer = std::sync::Arc::new(Mutex::new(None));
        let lifecycle = AskDuringTurn {
            req: FOValue::Int { value: 41 },
            answer: answer.clone(),
        };

        match shell.run_turn(TurnRequest {
            desk: Some(std::sync::Arc::new(IncrementDesk) as Desk),
            lifecycle: Box::new(lifecycle),
            ..capture_req("$[1 + 1]")
        }) {
            TurnReport::Ran { .. } => {}
            TurnReport::Static { .. } => panic!("`$[1 + 1]` must reach evaluation"),
        }

        let answer = answer.lock().unwrap().take().expect("pre_exec must fire");
        match answer {
            Ok(v) => assert_eq!(
                v,
                FOValue::Int { value: 42 },
                "enquire must return the desk's transformed answer"
            ),
            Err(e) => panic!("enquire must succeed through the stub desk, got {e:?}"),
        }
    }

    /// A desk whose handler reaches back into `IdentityTransport::shell_mut`
    /// — the reentrancy law (§3) forbids this: the handler runs on the
    /// dispatching thread, inside `dispatch`'s session-lock hold, so a
    /// second lock attempt would deadlock. The stamp `dispatch` sets for its
    /// duration is simulated here (there is no `enquire` builtin yet to
    /// drive a live encoded dispatch into calling the desk), exercising the
    /// exact guard every real dispatch installs.
    struct ReentrantShellMutDesk(std::sync::Arc<IdentityTransport>);
    impl EnquiryDesk for ReentrantShellMutDesk {
        fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
            let _guard = self.0.shell_mut();
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_shell_mut_panics_never_hangs() {
        let transport = std::sync::Arc::new(IdentityTransport::new(Shell::new(Default::default())));
        *transport.dispatch_thread.lock().unwrap() = Some(std::thread::current().id());
        let mut shell = Shell::new(Default::default());
        shell.turn.desk = Some(std::sync::Arc::new(ReentrantShellMutDesk(transport)) as Desk);
        let _ = shell.enquire(FOValue::Unit);
    }

    /// The `dispatch` variant of the same law: a desk handler that reaches
    /// back into `Transport::dispatch` must panic, never deadlock on the
    /// session lock its own stack already holds.
    struct ReentrantDispatchDesk(std::sync::Arc<IdentityTransport>);
    impl EnquiryDesk for ReentrantDispatchDesk {
        fn enquire(&self, req: FOValue) -> Result<FOValue, Error> {
            self.0.dispatch(
                DispatchId(0),
                Turn {
                    program: Program::Source(String::new()),
                    script_name: "<test>".into(),
                    caps: Capabilities::root(),
                    turn_limit: None,
                    deferred_lease: None,
                    worker_cap: None,
                    io: TurnIo::Capture,
                    terminal: RequestedTerminalAccess::Denied,
                    stdin: TurnStdin::Empty,
                },
            );
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_dispatch_panics_never_hangs() {
        let transport = std::sync::Arc::new(IdentityTransport::new(Shell::new(Default::default())));
        *transport.dispatch_thread.lock().unwrap() = Some(std::thread::current().id());
        let mut shell = Shell::new(Default::default());
        shell.turn.desk = Some(std::sync::Arc::new(ReentrantDispatchDesk(transport)) as Desk);
        let _ = shell.enquire(FOValue::Unit);
    }

    /// `IdentityTransport::set_desk` installs the desk that `dispatch` hands
    /// onto the `TurnRequest` it builds: the same `Arc` reaches
    /// `EngineInner.desk`.
    #[test]
    fn set_desk_installs_onto_engine_inner() {
        let transport = IdentityTransport::new(Shell::new(Default::default()));
        let desk: Desk = std::sync::Arc::new(IncrementDesk);
        transport.set_desk(desk.clone());
        let installed = transport
            .shell_mut()
            .desk
            .clone()
            .expect("set_desk must install a desk");
        assert!(
            std::sync::Arc::ptr_eq(&desk, &installed),
            "the installed desk must be the same Arc set_desk was given"
        );
    }
}
