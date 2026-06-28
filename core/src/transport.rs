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
//! Phase 2 will encode these same frame types through `SerialValue` and
//! `subprocess_codec` across a process boundary.  Every `Value` in the
//! frame types below is annotated with `// Phase 2: SerialValue`.
//!
//! See [[decisions/260628_host-seam-transport-parametric]].

use std::sync::mpsc;
use std::sync::Arc;

use crate::types::Boundary;
use crate::types::SurfaceSink;
use crate::types::Value;
use std::sync::OnceLock;

static CURRENT_CONTROL: OnceLock<ControlSender> = OnceLock::new();

// ── Dispatch identity ─────────────────────────────────────────────────

/// Correlation token for one outstanding dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DispatchId(pub u64);

// ── Frame algebra ─────────────────────────────────────────────────────

/// One frame that crosses the host seam in either direction.
pub enum Frame {
    /// Front-end conveys the session terminal endpoint.
    Attach(TerminalEndpoint),
    /// Front-end drops: cancel in-flight dispatch, reap foreground
    /// subtree, restore terminal state.
    Detach,
    /// One whole turn.
    Dispatch(DispatchId, Turn),
    /// Engine → front-end. May arrive while a Dispatch is outstanding.
    Event(DispatchId, Event),
    /// Front-end → engine. Out-of-band: deliverable while a Dispatch is
    /// outstanding.
    Control(Control),
}

/// The program of one dispatch: either source text or a hook invocation.
pub enum Turn {
    /// A top-level source turn: text compiled and typechecked against the
    /// live session.
    Source { src: String, req: ReqMirror },
    /// A hook invocation: a registered named entry point applied to
    /// ground arguments.  // Phase 2: args becomes Vec<SerialValue>
    Hook {
        name: crate::types::HookName,
        args: Vec<Value>,
        req: ReqMirror,
    },
}

/// Engine → front-end event frame.
pub enum Event {
    /// A live surface value, ordered before this dispatch Report.
    /// Phase 2: becomes Surface(SerialValue)
    Surface(Value),
    /// A detached worker deferred batch, flushed at a turn boundary.
    /// Phase 2: becomes BoundarySurface(Vec<SerialValue>)
    BoundarySurface(Vec<Value>),
    /// The dispatch sole terminal frame.
    Report(ReportMirror),
}

/// Front-end → engine out-of-band control frame.
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
#[derive(Debug, Clone, Copy)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
}

// ── Terminal endpoint ─────────────────────────────────────────────────

/// The terminal endpoint the front-end conveys at attach.
pub struct TerminalEndpoint {
    /// The session lease, if the front-end owns a terminal.
    pub lease: Option<crate::process::TerminalLease>,
    /// The terminal state for IO setup.
    pub state: crate::io::TerminalState,
}

// ── Protocol mirrors ──────────────────────────────────────────────────

/// Mirror of `TurnRequest`: everything a turn needs that is *data*.
/// The live handles `surface`, `boundary`, `lifecycle` are not here —
/// they become Event frames and engine-side defaults.
pub struct ReqMirror {
    /// Label for the root source context.
    pub script_name: String,
    /// The capability ceiling.
    pub caps: CapsMirror,
    /// Foreground wall duration.
    pub turn_limit: Option<std::time::Duration>,
    /// Lifetime ceiling for detached workers.
    pub detached_limit: Option<std::time::Duration>,
    /// Byte IO regime.
    pub io: crate::driver::TurnIo,
    /// Whether the turn may hand the controlling terminal to a child.
    pub terminal: crate::driver::RequestedTerminalAccess,
    /// Stdin source.
    pub stdin: crate::driver::TurnStdin,
}

/// Mirror of `Capabilities`: a serialisable projection of the dynamic
/// capability ceiling.
#[derive(Clone)]
pub enum CapsMirror {
    /// The ⊤ element — full authority (REPL).
    Root,
    /// A narrowed capability profile.
    Narrowed(Box<LiveCaps>),
}

/// Opaque wrapper so `CapsMirror` can hold live `Capabilities`.
#[derive(Clone)]
pub struct LiveCaps(pub(crate) crate::types::Capabilities);

impl CapsMirror {
    pub fn root() -> Self {
        CapsMirror::Root
    }
    pub fn from_live(caps: crate::types::Capabilities) -> Self {
        CapsMirror::Narrowed(Box::new(LiveCaps(caps)))
    }
    pub fn into_live(self) -> crate::types::Capabilities {
        match self {
            CapsMirror::Root => crate::types::Capabilities::root(),
            CapsMirror::Narrowed(lc) => lc.0,
        }
    }
}

/// Mirror of `TurnReport`.
pub enum ReportMirror {
    Static { diagnostics: DiagMirror },
    Ran {
        /// Phase 2: result becomes a SerialValue-based mirror
        result: ResultMirror,
        status: i32,
        single_command: bool,
        captured: Option<(Vec<u8>, Vec<u8>)>,
        timed_out: bool,
    },
}

/// Mirror of `StaticDiagnostics`.
pub enum DiagMirror {
    Parse(String),
    Types(Vec<String>),
    Host(String),
}

/// Mirror of `Settled<Value>`.
pub enum ResultMirror {
    /// Phase 2: becomes Ok(SerialValue)
    Ok(Value),
    Err(BreakMirror),
}

/// Mirror of `Break`.
pub enum BreakMirror {
    Error(String),
    Exit(i32),
    #[cfg(unix)]
    Stopped {
        pgid: i32,
        signal: i32,
        signal_name: String,
    },
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

    /// Convey the session terminal endpoint.
    fn attach(&self, endpoint: TerminalEndpoint);

    /// Detach: cancel in-flight dispatch, reap foreground subtree,
    /// restore terminal state.
    fn detach(&self);
}

// ── Senders and receivers ─────────────────────────────────────────────

/// Out-of-band control sender.
#[derive(Clone)]
pub struct ControlSender {

}

impl ControlSender {
    pub(crate) fn new() -> Self {
        ControlSender {}
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
        match ctrl {
            Control::Cancel(_id) => {
                // Trip the signal-reachable foreground scope — the same
                // static the SIGINT relay writes to.  This is the actual
                // scope the turn runs under, not a transport-side sibling.
                crate::process::request_foreground_cancel(
                    crate::process::CancelCause::Explicit,
                );
            }
            Control::Suspend | Control::Resume | Control::Resize(_) => {
                // Under the identity transport these are handled by the
                // process-level signal machinery in-process.
            }
        }
    }

    pub fn cancel(&self, _id: DispatchId) {
        self.send(Control::Cancel(_id));
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        self.send(Control::Resize(Winsize { rows, cols }));
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

    pub fn try_recv(&self) -> Option<Frame> {
        self.rx.lock().unwrap().try_recv().ok()
    }

    pub fn drain(&self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(f) = self.try_recv() {
            frames.push(f);
        }
        frames
    }
}

// ── Per-dispatch surface sink ─────────────────────────────────────────

struct PerDispatchSink {
    event_tx: mpsc::Sender<Frame>,
    current: std::sync::Mutex<Option<DispatchId>>,
}

impl PerDispatchSink {
    fn new(event_tx: mpsc::Sender<Frame>) -> Self {
        Self { event_tx, current: std::sync::Mutex::new(None) }
    }

    fn set_dispatch(&self, id: DispatchId) {
        *self.current.lock().unwrap() = Some(id);
    }

    fn clear_dispatch(&self) {
        *self.current.lock().unwrap() = None;
    }
}

impl crate::types::EventSink for PerDispatchSink {
    fn emit(&self, ev: &Value) {
        if let Some(id) = *self.current.lock().unwrap() {
            // Phase 2: convert Value -> SerialValue here
            let _ = self.event_tx.send(Frame::Event(id, Event::Surface(ev.clone())));
        }
    }
}

// ── Identity transport ────────────────────────────────────────────────

/// The in-process transport: one kernel, one address space.
///
/// `dispatch` runs the matching door synchronously on the calling thread.
/// Surface events are written to the event channel during execution;
/// control frames trip the foreground `CancelScope` directly.
pub struct IdentityTransport {
    /// The engine-side state, mutex-protected so `dispatch` can take
    /// `&self`.
    engine: std::sync::Mutex<EngineInner>,
    /// Control sender (wired to the foreground cancel scope).
    control: ControlSender,
    /// Event receiver for the front-end.
    events_recv: EventReceiver,
}

pub struct EngineInner {
    /// The shell that runs turns.
    pub shell: crate::types::Shell,
    /// Send events to the front-end.
    pub(crate) event_tx: mpsc::Sender<Frame>,
    /// The per-dispatch surface sink.
    surface_sink: Arc<PerDispatchSink>,
    /// The session-lived boundary sink for detached workers.
    boundary_sink: Option<Boundary>,
    /// The session terminal lease (set by Attach).
    terminal_lease: Option<crate::process::TerminalLease>,
    /// Shared dispatch id for boundary-sink correlation.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

impl IdentityTransport {
    /// Create a new identity transport that owns `shell`.
    pub fn new(shell: crate::types::Shell) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let surface_sink = Arc::new(PerDispatchSink::new(event_tx.clone()));
        let current_dispatch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let control = ControlSender::new();
        control.clone().publish();

        let boundary_tx = event_tx.clone();
        let engine = EngineInner {
            shell,
            event_tx,
            surface_sink,
            boundary_sink: Some(Arc::new(TransportBoundarySink { event_tx: boundary_tx, current_dispatch: current_dispatch.clone() })),
            terminal_lease: None,
            current_dispatch: current_dispatch.clone(),
        };

        IdentityTransport {
            engine: std::sync::Mutex::new(engine),
            control,
            events_recv: EventReceiver { rx: std::sync::Mutex::new(event_rx) },
        }
    }

    /// Set the session boundary sink for deferred worker batches.
    pub fn set_boundary(&self, boundary: Boundary) {
        self.engine.lock().unwrap().boundary_sink = Some(boundary);
    }



    /// Access the underlying `Shell` through the mutex guard directly.
    /// This is needed when the caller must combine shell access with
    /// other borrows (e.g. the REPL session's frontend, jobs table).
    /// Prefer `with_shell` for simple operations.
    pub fn shell_mut(&self) -> std::sync::MutexGuard<'_, EngineInner> {
        self.engine.lock().unwrap()
    }


    /// Access the underlying `Shell` mutably for session bootstrap.
    pub fn with_shell<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut crate::types::Shell) -> R,
    {
        f(&mut self.engine.lock().unwrap().shell)
    }
}

impl Transport for IdentityTransport {
    fn dispatch(&self, id: DispatchId, turn: Turn) {
        let mut engine = self.engine.lock().unwrap();

        // Install the per-dispatch surface sink.
        engine.surface_sink.set_dispatch(id);
        engine.current_dispatch.store(id.0, std::sync::atomic::Ordering::Relaxed);


        // Extract the ReqMirror (same shape for both Source and Hook).
        let req = match &turn {
            Turn::Source { req, .. } | Turn::Hook { req, .. } => req,
        };

        // Build the TurnRequest from the mirror.  script_name is
        // borrowed from the req which lives on our stack.
        let script_name = req.script_name.clone();
        let turn_req = crate::driver::TurnRequest {
            script_name: &script_name,
            caps: req.caps.clone().into_live(),
            turn_limit: req.turn_limit,
            detached_limit: req.detached_limit,
            io: req.io,
            terminal: req.terminal,
            stdin: req.stdin,
            surface: Some(engine.surface_sink.clone() as SurfaceSink),
            boundary: engine.boundary_sink.clone(),
            lifecycle: Box::new(()),
        };

        // Run the turn against the shell.
        let report = match turn {
            Turn::Source { src, .. } => {
                engine.shell.run_source_turn(&src, turn_req)
            }
            Turn::Hook { name, args, .. } => {
                engine.shell.run_hook(&name, args, turn_req)
            }
        };

        // Convert TurnReport → ReportMirror.
        let report_mirror = report_to_mirror(report);

        engine.surface_sink.clear_dispatch();
        engine.current_dispatch.store(0, std::sync::atomic::Ordering::Relaxed);
        let _ = engine.event_tx.send(Frame::Event(id, Event::Report(report_mirror)));
    }

    fn control(&self) -> &ControlSender {
        &self.control
    }

    fn events(&self) -> &EventReceiver {
        &self.events_recv
    }

    fn attach(&self, endpoint: TerminalEndpoint) {
        let mut engine = self.engine.lock().unwrap();
        engine.terminal_lease = endpoint.lease;
    }

    fn detach(&self) {
        // Cancel any in-flight turn via the signal-reachable scope.
        crate::process::request_foreground_cancel(
            crate::process::CancelCause::Explicit,
        );
        self.engine.lock().unwrap().terminal_lease = None;
    }
}



// ── Transport boundary sink ───────────────────────────────────────────

/// A `BoundarySink` that forwards detached-worker surface batches to the
/// event channel as `Event::BoundarySurface` frames.  Preserves the
/// `joined` deliver-once latch: only the first caller to set the latch
/// wins and has its batch forwarded.
struct TransportBoundarySink {
    event_tx: mpsc::Sender<Frame>,
    /// The dispatch id of the turn that installed this boundary, or 0 if
    /// installed between turns.  // Phase 2: correlate by DispatchId
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

impl crate::types::BoundarySink for TransportBoundarySink {
    fn deliver(
        &self,
        batch: Vec<Value>,
        joined: std::sync::Arc<std::sync::Mutex<bool>>,
    ) {
        // Test-and-set: only the first deliverer forwards the batch.
        let already = {
            let mut guard = joined.lock().unwrap();
            let was = *guard;
            *guard = true;
            was
        };
        if already {
            return;
        }
        let _ = self
            .event_tx
            .send(Frame::Event(DispatchId(self.current_dispatch.load(std::sync::atomic::Ordering::Relaxed)), Event::BoundarySurface(batch)));
    }
}

// ── Report conversion ─────────────────────────────────────────────────

fn report_to_mirror(report: crate::driver::TurnReport) -> ReportMirror {
    match report {
        crate::driver::TurnReport::Static { diagnostics } => {
            use crate::turn::StaticDiagnostics;
            ReportMirror::Static {
                diagnostics: match diagnostics {
                    StaticDiagnostics::Parse(e) => DiagMirror::Parse(e.message),
                    StaticDiagnostics::Types(errs) => {
                        DiagMirror::Types(errs.iter().map(|e| e.to_string()).collect())
                    }
                    StaticDiagnostics::Host(e) => DiagMirror::Host(e.message),
                },
            }
        }
        crate::driver::TurnReport::Ran {
            result,
            status,
            single_command,
            captured,
            timed_out,
        } => {
            let result_mirror = match result {
                Ok(v) => ResultMirror::Ok(v),
                Err(break_) => ResultMirror::Err(break_to_mirror(break_)),
            };
            let captured_bytes = captured.map(|c| (c.stdout, c.stderr));
            ReportMirror::Ran {
                result: result_mirror,
                status,
                single_command,
                captured: captured_bytes,
                timed_out,
            }
        }
    }
}

fn break_to_mirror(break_: crate::types::Break) -> BreakMirror {
    use crate::types::{Break, Escape};
    match break_ {
        Break::Error(e) => BreakMirror::Error(e.message),
        Break::Escape(esc) => match esc {
            Escape::Exit(code) => BreakMirror::Exit(code.clamp(0, 255)),
            #[cfg(unix)]
            Escape::Stopped { pgid, signal, .. } => BreakMirror::Stopped {
                pgid: pgid.0,
                signal: signal.number(),
                signal_name: signal.name().unwrap_or("?").to_string(),
            },
        },
    }
}
