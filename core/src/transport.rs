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
use serde::{Serialize, Deserialize};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::types::Boundary;
use crate::types::SurfaceSink;
use crate::types::Value;
use std::sync::OnceLock;
use crate::serial::SerialValue;

pub const PROTOCOL_VERSION: u32 = 1;

static CURRENT_CONTROL: OnceLock<ControlSender> = OnceLock::new();

// ── Dispatch identity ─────────────────────────────────────────────────

/// Correlation token for one outstanding dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DispatchId(pub u64);

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
    /// One whole turn.
    Dispatch(DispatchId, Turn),
    /// Engine → front-end. May arrive while a Dispatch is outstanding.
    Event(DispatchId, Event),
    /// Front-end → engine. Out-of-band: deliverable while a Dispatch is
    /// outstanding.
    Control(Control),
}

/// The program of one dispatch: either source text or a hook invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Turn {
    /// A top-level source turn: text compiled and typechecked against the
    /// live session.
    Source { src: String, req: ReqMirror },
    /// A hook invocation: a registered named entry point applied to
    /// ground arguments.  // Phase 2: args becomes Vec<SerialValue>
    Hook {
        name: crate::types::HookName,
        args: Vec<crate::serial::SerialValue>,
        req: ReqMirror,
    },
}

/// Engine → front-end event frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// A live surface value, ordered before this dispatch Report.
    /// Phase 2: becomes Surface(SerialValue)
    Surface(crate::serial::SerialValue),
    /// A detached worker deferred batch, flushed at a turn boundary.
    /// Phase 2: becomes BoundarySurface(Vec<SerialValue>)
    BoundarySurface(Vec<crate::serial::SerialValue>),
    /// The dispatch sole terminal frame.
    Report(ReportMirror),
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

// ── Protocol mirrors ──────────────────────────────────────────────────

/// Mirror of `TurnRequest`: everything a turn needs that is *data*.
/// The live handles `surface`, `boundary`, `lifecycle` are not here —
/// they become Event frames and engine-side defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqMirror {
    /// Label for the root source context.
    pub script_name: String,
    /// The capability ceiling.
    pub caps: crate::types::Capabilities,
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

/// Mirror of `TurnReport`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReportMirror {
    Static { diagnostics: DiagMirror },
    Ran {
        /// The turn's settled result, `SerialValue`-encoded for the wire.
        result: ResultMirror,
        status: i32,
        single_command: bool,
        captured: Option<(Vec<u8>, Vec<u8>)>,
        timed_out: bool,
    },
}

/// Mirror of `StaticDiagnostics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagMirror {
    Parse(String),
    Types(Vec<String>),
    Host(String),
}

/// Mirror of `Settled<Value>`.  `Ok` carries a `SerialValue` so a
/// successful result crosses the wire; a non-transportable result
/// (e.g. a live `Handle`) is reported as an error instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResultMirror {
    Ok(SerialValue),
    Err(BreakMirror),
}

/// Mirror of `Break`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Convey the session terminal endpoint and bootstrap state.
    fn attach(&self, endpoint: TerminalEndpoint, cwd: PathBuf, home: PathBuf, rc_path: Option<PathBuf>);

    /// Detach: cancel in-flight dispatch, reap foreground subtree,
    /// restore terminal state.
    fn detach(&self);
    /// Returns `self` as an `&dyn Any` for downcast support.
    fn as_any(&self) -> &dyn std::any::Any;
}

// ── Senders and receivers ─────────────────────────────────────────────

/// Out-of-band control sender.
#[derive(Clone)]
pub struct ControlSender {
    /// When `Some`, writes `Control` frames to a `WireChannel`.
    /// When `None`, acts directly on the in-process foreground scope.
    wire: Option<Arc<Mutex<crate::wire::WireChannel>>>,
}

impl ControlSender {
    pub(crate) fn new() -> Self {
        ControlSender { wire: None }
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
            if let Ok(sv) = SerialValue::from_ground(ev) {
                let _ = self.event_tx.send(Frame::Event(id, Event::Surface(sv)));
            }
        }
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
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
            engine: SessionLock::new(engine),
            control,
            events_recv: EventReceiver { rx: std::sync::Mutex::new(event_rx) },
        }
    }

    /// Set the session boundary sink for deferred worker batches.
    pub fn set_boundary(&self, boundary: Boundary) {
        self.engine.lock().boundary_sink = Some(boundary);
    }

    /// Consume the transport and recover the owned `Shell`.  The inverse of
    /// [`IdentityTransport::new`]: a caller that handed a shell in to route a
    /// turn through production can move it back out afterward.
    pub fn into_shell(self) -> crate::types::Shell {
        self.engine.into_inner().shell
    }



    /// Access the underlying `Shell` through the mutex guard directly.
    /// This is needed when the caller must combine shell access with
    /// other borrows (e.g. the REPL session's frontend, jobs table).
    /// Prefer `with_shell` for simple operations.
    pub fn shell_mut(&self) -> std::sync::MutexGuard<'_, EngineInner> {
        self.engine.lock()
    }


    /// Access the underlying `Shell` mutably for session bootstrap.
    pub fn with_shell<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut crate::types::Shell) -> R,
    {
        f(&mut self.engine.lock().shell)
    }
}

impl Transport for IdentityTransport {
    fn dispatch(&self, id: DispatchId, turn: Turn) {
        let mut engine = self.engine.lock();

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
            caps: req.caps.clone(),
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
                // Decode the ground arguments off the seam.
                let live_args: Vec<Value> = args
                    .into_iter()
                    .filter_map(|sv| sv.into_ground().ok())
                    .collect();
                engine.shell.run_hook(&name, live_args, turn_req)
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

    fn attach(&self, endpoint: TerminalEndpoint, _cwd: PathBuf, _home: PathBuf, _rc_path: Option<PathBuf>) {
        let mut engine = self.engine.lock();
        engine.terminal_lease = endpoint.lease;
    }

    fn detach(&self) {
        // Cancel any in-flight turn via the signal-reachable scope.
        crate::process::request_foreground_cancel(
            crate::process::CancelCause::Explicit,
        );
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
    _child: std::process::Child,
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
    pub fn new() -> io::Result<Self> {
        let (frontend, engine) = crate::wire::WireChannel::pair()?;

        // Duplicate the front-end fd so we can read on one handle
        // and write on the other concurrently.
        let writer = frontend.try_clone()?;

        // Spawn the engine child with the engine end of the socket on fd 3.
        let engine_fd = engine.as_raw_fd();
        let mut cmd = std::process::Command::new(
            std::env::current_exe().map_err(io::Error::other)?,
        );
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
            _child: child,
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
        let _ = self._child.wait();
    }
}

#[cfg(unix)]
impl Transport for WireTransport {
    fn dispatch(&self, id: DispatchId, turn: Turn) {
        self.write(&Frame::Dispatch(id, turn));
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
    fn attach(&self, endpoint: TerminalEndpoint, cwd: PathBuf, home: PathBuf, rc_path: Option<PathBuf>) {
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
    fn as_any(&self) -> &dyn std::any::Any {
        self
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
        let sv_batch: Vec<SerialValue> = batch
            .into_iter()
            .filter_map(|v| SerialValue::from_ground(&v).ok())
            .collect();
        let _ = self
            .event_tx
            .send(Frame::Event(DispatchId(self.current_dispatch.load(std::sync::atomic::Ordering::Relaxed)), Event::BoundarySurface(sv_batch)));
    }
}

// ── Report conversion ─────────────────────────────────────────────────

pub(crate) fn report_to_mirror(report: crate::driver::TurnReport) -> ReportMirror {
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
                Ok(v) => {
                    // Encode the result so it crosses the wire.  A
                    // non-transportable value (e.g. a live `Handle`) cannot
                    // cross; surface it as a diagnostic rather than dropping
                    // it silently.
                    match SerialValue::from_ground(&v) {
                        Ok(sv) => ResultMirror::Ok(sv),
                        Err(_) => ResultMirror::Err(BreakMirror::Error(
                            "turn result is not transportable across the host seam".into(),
                        )),
                    }
                }
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
