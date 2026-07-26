//! Transport-parametric host seam — the frame algebra that separates a
//! front-end from the ral engine.
//!
//! A "frame" names one of the rails that cross between front-end and
//! engine: Attach/Detach/Ping/Pong bracket and keep alive one connection —
//! the connection *is* the session — while Dispatch carries one whole
//! run, Event flows engine→front-end, and Control flows front-end→engine.
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
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// The same-host engine child is a Unix handoff (a socketpair end inherited on
// fd 3), so its handle — and only it — stays platform-gated; every other part
// of the wire transport is the frame protocol, which is not.
#[cfg(unix)]
use crate::process::ChildHandle;
use crate::serial::FOValue;
use crate::types::DeferredSink;
use crate::types::SurfaceSink;
use std::sync::OnceLock;

pub const PROTOCOL_VERSION: u32 = 4;

static CURRENT_CONTROL: OnceLock<ControlSender> = OnceLock::new();

// ── Dispatch identity ─────────────────────────────────────────────────

/// Correlation token for one outstanding dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DispatchId(pub u64);

/// Correlation token for one outstanding enquiry.
///
/// Unused under the identity
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
        /// Protocol version the front-end speaks (checked against `PROTOCOL_VERSION`).
        proto_version: u32,
        /// Tag naming the compiled-in builtin installer the engine child
        /// must apply after booting its shell — e.g. `"exarch-agent"` or
        /// `"repl"`. Both halves of the single binary map this string to
        /// their own compiled-in installer table; the tag is the only
        /// thing that crosses, never the installer function itself.
        installer: String,
    },
    /// Front-end drops: cancel in-flight dispatch, reap foreground
    /// subtree, restore terminal state.
    Detach,
    /// One whole run.  Boxed so every other `Frame` variant is not sized
    /// to `Run`'s stack footprint.
    Dispatch(DispatchId, Box<Run>),
    /// A pure, boundary-time read of session state — no wall, no sinks, no
    /// clock, absent by type, which is why it is a `Frame` and not a `Run`
    /// variant. Answered through the same `Event::Report` rail as a
    /// dispatch and correlated by the same `DispatchId`; it serialises with
    /// dispatches on the engine's single worker rendezvous, so a probe sent
    /// while a run runs gets the same "engine busy" answer a second
    /// dispatch would. The `FOValue` names the reading class (a
    /// `Variant` label, decoded by [`answer_probe`]); an unrecognised class
    /// answers an error naming it, never a silent default.
    Probe(DispatchId, FOValue),
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
    /// Front-end → engine heartbeat (`dev/docs/VM/SYNOD.md` §3).  On a
    /// transport whose failure mode is silence — a virtual-socket stream
    /// into a guest, where a dead host produces no EOF — liveness must be
    /// detected, not assumed.  The law: *any* received frame is proof of
    /// life; heartbeats exist only to manufacture traffic, so that silence
    /// can mean nothing but death.  Never sent before `Attach`.  The first
    /// Ping arms the engine's read deadline: a front-end that pings has
    /// promised to keep pinging, while one that never pings (the same-host
    /// socketpair front-ends, whose engine death is a kernel-guaranteed
    /// EOF) leaves the engine's patience infinite.
    Ping(u64),
    /// Engine → front-end: the echo of one `Ping`, carrying its sequence
    /// number back.  This is what keeps an *idle* engine visibly alive; a
    /// busy one already proves itself with `Event` traffic.
    Pong(u64),
}

/// One whole run: the program to evaluate and the conditions it runs under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// What the run evaluates.
    pub program: Program,
    /// Label for the root source context (`"<stdin>"` for the REPL,
    /// `"<tool>"` for exarch).
    pub script_name: String,
    /// The capability ceiling pushed for the eval's dynamic extent.
    /// `Capabilities::root()` is the ⊤ element — the identity on authority
    /// (the REPL) — while a narrower profile attenuates the session's grant
    /// (exarch).
    pub caps: crate::types::Capabilities,
    /// The run's foreground wall: `Some(d)` arms a `Deadline` cancel on the
    /// run's foreground scope `d` after it starts (exarch's per-tool wall);
    /// `None` leaves the run uncapped (the REPL).
    pub wall: Option<std::time::Duration>,
    /// The lease for workers the run defers at the durable root. `None`
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
    pub io: crate::run::RunIo,
    /// Whether the run may hand the controlling terminal to a child.
    pub terminal: crate::run::RequestedTerminalAccess,
    /// Stdin source.
    pub stdin: crate::run::RunStdin,
}

/// What a run runs: source text compiled and typechecked against the live
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
    /// A live surface value from the foreground run, ordered before this
    /// dispatch's Report.
    Surface(FOValue),
    /// A deferred worker's surface batch, delivered when it settles and
    /// rendered by the host at the next run boundary.
    DeferredSurface(Vec<FOValue>),
    /// One enquiry the running dispatch raised on its host desk, nested
    /// inside this dispatch and ordered before its Report. Answered by a
    /// `Frame::Answer` carrying the same `EnquiryId`.
    Enquiry(EnquiryId, FOValue),
    /// The dispatch's sole terminal frame.
    Report(Report),
}

/// What crosses the wire when an enquiry is refused or its handler fails.
///
/// Kept as message *and* status (never collapsed into `crate::types::Error`,
/// which also carries a `loc` core has no business shipping across the
/// seam) so a refused enquiry raises the same error under both transports;
/// its location is stamped engine-side, at the enquiring builtin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEndpoint {
    /// The session lease, if the front-end owns a terminal.
    #[serde(skip)]
    pub lease: Option<crate::process::TerminalLease>,
    /// The terminal state for IO setup.
    pub state: crate::io::TerminalState,
}

/// `TerminalLease` is deliberately not `Clone` (its security argument rests
/// on being unforgeable), so cloning an endpoint cannot duplicate the
/// capability — only the terminal state comes along; the clone carries no
/// lease.
impl Clone for TerminalEndpoint {
    fn clone(&self) -> Self {
        Self {
            lease: None,
            state: self.state,
        }
    }
}

// ── The terminal frame ────────────────────────────────────────────────

/// The run's terminal frame: the protocol projection of the engine's
/// [`RunReport`](crate::run::RunReport), produced by
/// [`RunReport::into_report`](crate::run::RunReport::into_report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Report {
    /// Parse/type/host failure: the run never reached evaluation. The host
    /// renders the diagnostics and treats the run as status 1.
    Static { diagnostics: Diagnostics },
    /// The run ran to a settled result. `status` is the transport status
    /// computed once; `single_command` is whether the source compiled to a
    /// single command (for runtime-error rendering); `captured` is `Some`
    /// under [`RunIo::Capture`](crate::run::RunIo); `timed_out` is
    /// whether the wall fired.
    Ran {
        /// The run's settled result. `Ok` carries a [`FOValue`] so a
        /// successful result crosses the wire; a non-transportable result
        /// (e.g. a live `Handle`) is reported as an error instead.
        result: Result<FOValue, Break>,
        status: i32,
        single_command: bool,
        captured: Option<crate::run::Captured>,
        timed_out: bool,
    },
}

/// Rendered diagnostics from a run that never ran: the protocol projection
/// of [`StaticDiagnostics`](crate::run::StaticDiagnostics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Diagnostics {
    Parse(String),
    Types(Vec<String>),
    Host(String),
}

/// First-order projection of a settled run's
/// [`Break`](crate::types::Break).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Break {
    /// A caught runtime error, already rendered to its full diagnostic string
    /// (see [`RunReport::into_report`](crate::run::RunReport::into_report)) —
    /// the host prints it verbatim.
    /// `command_exit` carries the one classification a host's own didactics
    /// need — whether the status was an external command's non-zero exit,
    /// as opposed to a raised error — as a fact beside the rendering, since
    /// the structured `Status` it derives from never crosses the seam.
    Error {
        rendered: String,
        command_exit: bool,
    },
    Exit(i32),
    #[cfg(unix)]
    Stopped {
        pgid: i32,
        signal: i32,
        signal_name: String,
    },
}

/// Project an engine [`Break`](crate::types::Break) onto the transport
/// [`Break`], rendering a caught runtime error against `sources` into its
/// full diagnostic string. `single_command` selects the compact one-liner
/// over the source-span caret, exactly as the batch host chooses.
fn render_break(
    break_: crate::types::Break,
    sources: &crate::source::SourceDb,
    single_command: bool,
) -> Break {
    use crate::types::{Break as EngineBreak, Escape};
    match break_ {
        EngineBreak::Error(e) => {
            let command_exit = matches!(
                e.status,
                crate::types::Status::Process(crate::process::CommandFailure::ExitCode(_))
            );
            Break::Error {
                rendered: crate::diagnostic::format_runtime_error_auto(sources, &e, single_command),
                command_exit,
            }
        }
        EngineBreak::Escape(esc) => match esc {
            Escape::Exit(code) => Break::Exit(code.clamp(0, 255)),
            #[cfg(unix)]
            Escape::Stopped { pgid, signal, .. } => Break::Stopped {
                pgid: pgid.as_raw(),
                signal: signal.number(),
                signal_name: signal.name().unwrap_or("?").to_string(),
            },
        },
    }
}

impl crate::run::RunReport {
    /// Project into the protocol [`Report`]: the live `Value` becomes a
    /// first-order [`FOValue`] (a non-transportable result — e.g. a live
    /// `Handle` — is reported as an error rather than dropped silently),
    /// and rich diagnostics render to strings against `sources`.  The one
    /// lossy step between the engine and the seam.
    ///
    /// A caught runtime error is rendered here — at the seam, while the
    /// engine's [`SourceDb`](crate::source::SourceDb) is still in hand — into
    /// the same full diagnostic string the batch host produces
    /// ([`format_runtime_error_auto`](crate::diagnostic::format_runtime_error_auto)):
    /// `error:` prefix, exit status, hint, and source-span caret.  The host
    /// then only has to print it.
    pub fn into_report(self, sources: &crate::source::SourceDb) -> Report {
        use crate::run::StaticDiagnostics;
        match self {
            Self::Static { diagnostics } => Report::Static {
                diagnostics: match diagnostics {
                    StaticDiagnostics::Parse(e) => Diagnostics::Parse(e.message),
                    StaticDiagnostics::Types(errs) => Diagnostics::Types(
                        errs.iter().map(std::string::ToString::to_string).collect(),
                    ),
                    StaticDiagnostics::Host(e) => Diagnostics::Host(e.message),
                },
            },
            Self::Ran {
                result,
                status,
                single_command,
                captured,
                timed_out,
            } => Report::Ran {
                result: match result {
                    Ok(v) => match FOValue::try_from(&v) {
                        Ok(fo) => Ok(fo),
                        Err(_) => Err(Break::Error {
                            rendered: "run result is not transportable across the host seam".into(),
                            command_exit: false,
                        }),
                    },
                    Err(break_) => Err(render_break(break_, sources, single_command)),
                },
                // Clamp here, at the single point a raw code becomes a
                // transport code, so `status` and `Break::Exit` (clamped
                // just above) always agree on the same fact.
                status: status.clamp(0, 255),
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
    fn dispatch(&self, id: DispatchId, run: Run);

    /// Run a pure, boundary-time read of session state synchronously and
    /// return its answer. Serialises with `dispatch` on the same rendezvous
    /// (busy → the same "engine busy" error a second dispatch would get);
    /// an unrecognised reading class answers `Err` naming the class.
    ///
    /// # Errors
    /// Returns `Err` if the engine is busy on the rendezvous (the "engine
    /// busy" error), or if the reading class is unrecognised (naming it).
    fn probe(&self, reading: FOValue) -> Result<FOValue, String>;

    /// The out-of-band control sender — writable while a dispatch is in
    /// flight.  Under the identity transport this writes directly to the
    /// foreground `CancelScope`.
    fn control(&self) -> &ControlSender;

    /// The event stream the front-end drains.
    fn events(&self) -> &EventReceiver;

    /// Convey the session terminal endpoint and bootstrap state.
    ///
    /// `installer` names the compiled-in builtin installer the wire
    /// engine child must apply once it boots its shell (§ `Frame::Attach`);
    /// the identity transport ignores it — its shell is already booted and
    /// dressed with the host's builtins by the caller before `attach`.
    fn attach(
        &self,
        endpoint: TerminalEndpoint,
        cwd: PathBuf,
        home: PathBuf,
        rc_path: Option<PathBuf>,
        installer: String,
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

/// Mint a dispatch id, send `run` down `transport`, and drain the event
/// stream to the run's terminal [`Report`](Event::Report), handing each
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
/// `on_enquiry` answers one [`Event::Enquiry`] this dispatch's run raised on
/// its host desk; the answer is written back through
/// [`Transport::answer`]. Under the identity transport `Event::Enquiry` never
/// arrives here at all — the installed `Desk` is a direct call the run makes
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
    run: Run,
    mut on_surface: impl FnMut(FOValue),
    mut on_deferred: impl FnMut(Vec<FOValue>),
    mut on_enquiry: impl FnMut(FOValue) -> Result<FOValue, EnquiryError>,
) -> Option<Report> {
    let id = mint_dispatch_id();

    transport.dispatch(id, run);

    while let Some((did, event)) = transport.events().recv() {
        if did != id {
            continue;
        }
        match event {
            Event::Surface(val) => on_surface(val),
            Event::DeferredSurface(batch) => on_deferred(batch),
            Event::Enquiry(eid, req) => {
                let answer = on_enquiry(req);
                transport.answer(id, eid, answer);
            }
            Event::Report(report) => return Some(report),
        }
    }
    None
}

/// Mint one correlation id for the Dispatch → Report rail. Dispatches and
/// probes share the mint: both are answered by an [`Event::Report`]
/// correlated by [`DispatchId`], so two counters would let a probe's Report
/// be mistaken for a dispatch's.
fn mint_dispatch_id() -> DispatchId {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    DispatchId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

// ── Probe decoder ─────────────────────────────────────────────────────

/// The one decoder for every probe reading, shared by `IdentityTransport`
/// and the wire engine (`core/src/engine.rs`) so a new reading class costs
/// one arm here, never a second copy per transport.
///
/// Each class is an
/// `FOValue::Variant` label, matching the accessor traffic
/// `exarch/src/agent.rs` used to read straight through `shell_mut()`:
/// `` `worker-count ``, `` `binding-count ``, `` `leased-binding-count ``,
/// `` `env-var [name] ``, `` `cwd ``, `` `grant-depth ``,
/// `` `largest-binding-bytes ``, `` `workers ``. An
/// unrecognised class answers `Err` naming it, never a silent default.
///
/// # Errors
/// Returns `Err` if `req` is not a variant, if the `` `env-var `` class lacks
/// a string payload, or if the reading class is unrecognised (naming it).
///
/// # Panics
/// Panics if a worker handle's `state` or `last_observed` mutex is poisoned,
/// reachable only through the `` `workers `` class.
pub fn answer_probe(shell: &mut crate::types::Shell, req: &FOValue) -> Result<FOValue, String> {
    let FOValue::Variant { label, payload } = req else {
        return Err(format!("probe request must be a variant, got {req:?}"));
    };
    match label.as_str() {
        "worker-count" =>
        {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "usize cardinality below i64::MAX"
            )]
            Ok(FOValue::Int {
                value: shell.worker_count() as i64,
            })
        }
        "binding-count" =>
        {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "usize cardinality below i64::MAX"
            )]
            Ok(FOValue::Int {
                value: shell.binding_count() as i64,
            })
        }
        "leased-binding-count" =>
        {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "usize cardinality below i64::MAX"
            )]
            Ok(FOValue::Int {
                value: shell.leased_binding_count() as i64,
            })
        }
        "env-var" => {
            let name = match payload.as_deref() {
                Some(FOValue::String { value }) => value.as_str(),
                _ => return Err("`env-var probe requires a string payload".into()),
            };
            Ok(match shell.env_var(name) {
                Some(value) => FOValue::Variant {
                    label: "some".into(),
                    payload: Some(Box::new(FOValue::String { value })),
                },
                None => FOValue::Variant {
                    label: "none".into(),
                    payload: None,
                },
            })
        }
        "cwd" => Ok(FOValue::String {
            value: shell.cwd().display().to_string(),
        }),
        "grant-depth" =>
        {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "usize cardinality below i64::MAX"
            )]
            Ok(FOValue::Int {
                value: shell.grant_depth() as i64,
            })
        }
        "largest-binding-bytes" =>
        {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "an in-memory shallow byte estimate is far below i64::MAX"
            )]
            Ok(FOValue::Int {
                value: shell.largest_binding_shallow_size() as i64,
            })
        }
        "workers" => {
            use crate::types::{HandleState, LeaseClass};
            #[allow(
                clippy::cast_possible_wrap,
                reason = "worker counts, sequential WorkerId(u64) ids, and u64 uptime/idle/settled-epoch quantities are all far below i64::MAX in any live process"
            )]
            let items = shell
                .workers()
                .into_iter()
                .map(|entry| {
                    let running = *entry.handle.state.lock().unwrap() == HandleState::Running;
                    let up_secs = entry.started.elapsed().unwrap_or_default().as_secs();
                    let idle_secs = entry
                        .handle
                        .last_observed
                        .lock()
                        .unwrap()
                        .elapsed()
                        .as_secs();
                    FOValue::Map {
                        entries: vec![
                            (
                                "id".into(),
                                FOValue::Int {
                                    value: entry.id.0 as i64,
                                },
                            ),
                            ("cmd".into(), FOValue::String { value: entry.cmd }),
                            (
                                "class".into(),
                                FOValue::String {
                                    value: match entry.class {
                                        LeaseClass::Worker => "worker".into(),
                                        LeaseClass::Durable => "durable".into(),
                                    },
                                },
                            ),
                            ("running".into(), FOValue::Bool { value: running }),
                            (
                                "up-secs".into(),
                                FOValue::Int {
                                    value: up_secs as i64,
                                },
                            ),
                            (
                                "idle-secs".into(),
                                FOValue::Int {
                                    value: idle_secs as i64,
                                },
                            ),
                            (
                                "settled-epoch".into(),
                                match entry.settled_epoch {
                                    Some(epoch) => FOValue::Variant {
                                        label: "some".into(),
                                        payload: Some(Box::new(FOValue::Int {
                                            value: epoch as i64,
                                        })),
                                    },
                                    None => FOValue::Variant {
                                        label: "none".into(),
                                        payload: None,
                                    },
                                },
                            ),
                        ],
                    }
                })
                .collect();
            Ok(FOValue::List { items })
        }
        other => Err(format!("unknown probe class `{other}")),
    }
}

// ── Senders and receivers ─────────────────────────────────────────────

/// Out-of-band control sender.
#[derive(Clone)]
pub struct ControlSender {
    /// When `Some`, writes `Control` frames to a `WireChannel`.
    /// When `None`, acts directly on the in-process foreground scope.
    wire: Option<Arc<Mutex<crate::wire::WireChannel>>>,
    /// The transport's own record of the in-flight dispatch id (`0` = none),
    /// shared with whichever bookkeeping the transport already keeps for
    /// event correlation — `cancel_current` names the dispatch it means
    /// rather than a mintable sentinel.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

impl ControlSender {
    pub(crate) fn new(current_dispatch: Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            wire: None,
            current_dispatch,
        }
    }

    pub(crate) fn new_wire(
        ch: Arc<Mutex<crate::wire::WireChannel>>,
        current_dispatch: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            wire: Some(ch),
            current_dispatch,
        }
    }

    pub(crate) fn publish(self) {
        let _ = CURRENT_CONTROL.set(self);
    }

    /// Cancel this sender's own in-flight dispatch: read `current_dispatch`
    /// and send `Control::Cancel(DispatchId(id))` down whichever channel this
    /// sender writes — the wire, or (under the identity transport) the
    /// process-global foreground scope directly.
    pub fn cancel_in_flight(&self) {
        let id = self
            .current_dispatch
            .load(std::sync::atomic::Ordering::Relaxed);
        self.send(Control::Cancel(DispatchId(id)));
    }

    pub fn cancel_current() {
        if let Some(ctrl) = CURRENT_CONTROL.get() {
            ctrl.cancel_in_flight();
        }
    }

    /// # Panics
    /// Panics if the wire-channel mutex is poisoned.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the wire arm moves `ctrl` into the `Frame` it writes, and `Control` is not `Copy`; only the identity fallback below merely reads it"
    )]
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
                // scope the run runs under, not a transport-side sibling.
                crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
            }
            Control::Suspend | Control::Resume | Control::Resize(_) => {
                // Under the identity transport these are handled by the
                // process-level signal machinery in-process.
            }
        }
    }
}

/// Event receiver: one correlated event per item.
///
/// The front-end drains this while a dispatch is outstanding; `Surface`
/// events are ordered before the `Report`.
pub struct EventReceiver {
    rx: std::sync::Mutex<mpsc::Receiver<(DispatchId, Event)>>,
    /// Events a probe's reply drain read past while waiting for its own
    /// Report — e.g. a deferred worker's batch settling mid-probe. The
    /// ordering law (§6.5) forbids dropping them, so they are handed back
    /// to the next `recv`, in arrival order, ahead of the live channel.
    stash: std::sync::Mutex<std::collections::VecDeque<(DispatchId, Event)>>,
}

impl EventReceiver {
    fn new(rx: mpsc::Receiver<(DispatchId, Event)>) -> Self {
        Self {
            rx: std::sync::Mutex::new(rx),
            stash: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// # Panics
    /// Panics if the stash or receiver mutex is poisoned.
    pub fn recv(&self) -> Option<(DispatchId, Event)> {
        let stashed = self.stash.lock().unwrap().pop_front();
        if let Some(item) = stashed {
            return Some(item);
        }
        self.rx.lock().unwrap().recv().ok()
    }

    /// Non-blocking drain: a stashed event first (arrival order, per §6.5's
    /// ordering law), then the channel. `None` when both are empty right now
    /// — never blocks.
    ///
    /// # Panics
    /// Panics if the stash or receiver mutex is poisoned.
    pub fn try_recv(&self) -> Option<(DispatchId, Event)> {
        let stashed = self.stash.lock().unwrap().pop_front();
        if let Some(item) = stashed {
            return Some(item);
        }
        self.rx.lock().unwrap().try_recv().ok()
    }
}

#[cfg(test)]
mod event_receiver_tests {
    use super::*;

    fn receiver() -> (EventReceiver, mpsc::Sender<(DispatchId, Event)>) {
        let (tx, rx) = mpsc::channel();
        (EventReceiver::new(rx), tx)
    }

    /// A stashed event is returned before whatever the channel holds, and
    /// without blocking — the same arrival-order precedence `recv` gives the
    /// stash, proven here on the non-blocking door.
    #[test]
    fn try_recv_drains_the_stash_before_the_channel() {
        let (receiver, tx) = receiver();
        let channelled = (DispatchId(2), Event::Surface(FOValue::Unit));
        let stashed = (DispatchId(1), Event::DeferredSurface(vec![]));
        tx.send(channelled.clone()).unwrap();
        receiver.stash.lock().unwrap().push_back(stashed.clone());

        assert_eq!(receiver.try_recv(), Some(stashed));
        assert_eq!(receiver.try_recv(), Some(channelled));
    }

    /// With neither a stashed event nor a channel send pending, `try_recv`
    /// returns `None` immediately rather than blocking.
    #[test]
    fn try_recv_returns_none_when_empty() {
        let (receiver, _tx) = receiver();
        assert_eq!(receiver.try_recv(), None);
    }
}

// ── Transport sink ────────────────────────────────────────────────────

/// The identity transport's sink for both surface regimes: a live value
/// ([`EventSink`](crate::types::EventSink)) and a deferred worker's settled
/// batch ([`DeferredSink`]), each forwarded onto the event channel stamped
/// with the in-flight dispatch id.
struct TransportSink {
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    /// The in-flight dispatch id (`0` = none), the one atomic `dispatch`
    /// sets and both impls read. A live surface value emitted while it reads
    /// `0` has no dispatch to correlate to and is dropped; a deferred batch
    /// is stamped with whatever is in flight — `0` when it settles between
    /// runs.
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
                .send((DispatchId(id), Event::Surface(ev.clone())));
        }
    }
}

impl crate::types::DeferredSink for TransportSink {
    fn deliver(&self, batch: Vec<FOValue>) {
        let _ = self.event_tx.send((
            DispatchId(
                self.current_dispatch
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            Event::DeferredSurface(batch),
        ));
    }
}

// ── Identity transport ────────────────────────────────────────────────

/// A session lock that cannot poison-panic.  A run that unwinds drops its
/// guard mid-mutation, which would otherwise poison the mutex and turn every
/// later `lock()` into a second panic (`PoisonError`); recovering the guard
/// instead means a panicking run can never wedge the session.  This is the
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
    /// same way `lock` does — a run that unwound mid-mutation must not wedge
    /// the move-out.
    fn into_inner(self) -> EngineInner {
        self.0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    /// Event receiver for the front-end. `Arc`-wrapped so a later drain-then-
    /// handle adapter can hold its own clone alongside the transport, rather
    /// than borrowing one tied to `&self`.
    events_recv: Arc<EventReceiver>,
    /// Stamped with the dispatching thread's id for the duration of
    /// `dispatch`, so a desk handler that reenters the session lock
    /// (`dispatch`/`shell_mut`/`with_shell`) panics instead of deadlocking
    /// (§3's reentrancy law). A separate short lock, never the session
    /// lock — checking it must not touch `self.engine`.
    dispatch_thread: std::sync::Mutex<Option<std::thread::ThreadId>>,
    /// Set by [`Self::observe_foreground`]: a cell this transport writes
    /// each run's freshly-installed foreground scope into, at the run's
    /// very start.  Lives outside `engine`'s own lock, so a caller holding
    /// a clone of the cell can read (and cancel) the *current* run's
    /// scope from another thread without waiting on a dispatch in flight —
    /// the extension point a forked-session host uses to interrupt one
    /// run without touching the session's durable root.
    run_scope: Option<Arc<std::sync::Mutex<Option<crate::process::ForegroundScope>>>>,
}

pub struct EngineInner {
    /// The shell that executes runs.
    pub shell: crate::types::Shell,
    /// Send events to the front-end.
    pub(crate) event_tx: mpsc::Sender<(DispatchId, Event)>,
    /// The transport sink, serving both surface regimes.
    surface_sink: Arc<TransportSink>,
    /// The session-lived deferred sink for deferred workers.  Seeded with
    /// the transport sink; a host may install its own (`set_deferred_sink`).
    deferred_sink: Option<Arc<dyn DeferredSink>>,
    /// The installed enquiry desk, if any host has set one. `None` in
    /// Phase A: no desk installed by any host, so `enquire` answers its
    /// honest absence error.
    desk: Option<crate::types::Desk>,
    /// The installed nursery for engine-side session forks, if any host has
    /// set one. `None` until a host calls `set_nursery`, so `fork_into_nursery`
    /// answers its honest absence error.
    nursery: Option<crate::types::Nursery>,
    /// The session terminal lease (set by Attach).
    terminal_lease: Option<crate::process::TerminalLease>,
    /// Shared dispatch id for deferred-sink correlation.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

/// Clears the dispatch-thread stamp on drop, even on unwind — a run that
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
        let control = ControlSender::new(current_dispatch.clone());
        control.clone().publish();

        let engine = EngineInner {
            shell,
            event_tx,
            surface_sink: sink.clone(),
            deferred_sink: Some(sink),
            desk: None,
            nursery: None,
            terminal_lease: None,
            current_dispatch,
        };

        Self {
            engine: SessionLock::new(engine),
            control,
            events_recv: Arc::new(EventReceiver::new(event_rx)),
            dispatch_thread: std::sync::Mutex::new(None),
            run_scope: None,
        }
    }

    /// Return a shared handle to the front-end's event receiver, for a
    /// caller that wants to drain it alongside the transport rather than
    /// through a borrow tied to `&self`.
    pub fn events_shared(&self) -> Arc<EventReceiver> {
        self.events_recv.clone()
    }

    /// Set the session deferred sink for deferred worker batches.
    pub fn set_deferred_sink(&self, deferred: Arc<dyn DeferredSink>) {
        self.engine.lock().deferred_sink = Some(deferred);
    }

    /// Arm this transport to publish each run's freshly-installed
    /// foreground scope into `cell` at the start of every dispatch, before
    /// evaluation begins.  Call once, right after construction: a forked
    /// session (exarch's agent fleet) keeps its own clone of `cell`
    /// alongside the session's durable root, so an interrupt can cancel
    /// whichever scope the in-flight run actually installed without ever
    /// touching the root a later run would inherit.
    pub fn observe_foreground(
        &mut self,
        cell: Arc<std::sync::Mutex<Option<crate::process::ForegroundScope>>>,
    ) {
        self.run_scope = Some(cell);
    }

    /// Install the session's enquiry desk. Per-run hosts (e.g. exarch, once
    /// the migration lands) call this before each dispatch so whatever the
    /// desk captures is fresh — the same reasoning `set_deferred_sink`
    /// follows.
    pub fn set_desk(&self, desk: crate::types::Desk) {
        self.engine.lock().desk = Some(desk);
    }

    /// Install the session's nursery for engine-side session forks. Per-run
    /// hosts call this before each dispatch so a stale fork from an earlier
    /// generation is never adoptable, the same reasoning `set_desk` follows.
    pub fn set_nursery(&self, nursery: crate::types::Nursery) {
        self.engine.lock().nursery = Some(nursery);
    }

    /// Uninstall the session's nursery. `set_desk` has no symmetric
    /// counterpart because a desk retires to the `AbsentDesk` sentinel
    /// instead — `Desk` is a trait object, so an "answers no enquiries"
    /// value is cheap to construct and install in `desk`'s place. A
    /// `Nursery` has no such sentinel: an empty one would still accept a
    /// park and answer `fork_into_nursery` successfully, which is not
    /// absence. `None` is the only honest "no nursery installed" value, so
    /// this is the door that reaches it between calls.
    pub fn clear_nursery(&self) {
        self.engine.lock().nursery = None;
    }

    /// Consume the transport and recover the owned `Shell`.  The inverse of
    /// [`IdentityTransport::new`]: a caller that handed a shell in to route a
    /// run through production can move it back out afterward.
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

/// [`IdentityTransport::observe_foreground`]'s engine-side half: writes the
/// run's freshly-installed foreground scope into the caller's cell in
/// `pre_exec`, which core's own run door runs after installing that scope
/// but before evaluation starts.
struct ForegroundCapture(Arc<std::sync::Mutex<Option<crate::process::ForegroundScope>>>);

impl crate::run::RunLifecycle for ForegroundCapture {
    fn pre_exec(&mut self, shell: &mut crate::types::Shell, _src: &str) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(shell.foreground().clone());
    }
}

impl Transport for IdentityTransport {
    fn dispatch(&self, id: DispatchId, run: Run) {
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

        // The live handles this dispatch lends the run, joined with the
        // protocol `Run` in the request the engine door runs.
        let lifecycle: Box<dyn crate::run::RunLifecycle> = match &self.run_scope {
            Some(cell) => Box::new(ForegroundCapture(cell.clone())),
            None => Box::new(()),
        };
        let req = crate::run::RunRequest {
            run,
            surface: Some(engine.surface_sink.clone() as SurfaceSink),
            deferred: engine.deferred_sink.clone(),
            desk: engine.desk.clone(),
            nursery: engine.nursery.clone(),
            lifecycle,
        };
        let run_report = engine.shell.run(req);
        let report = run_report.into_report(engine.shell.sources());

        engine
            .current_dispatch
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let _ = engine.event_tx.send((id, Event::Report(report)));
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "Transport::probe signature is fixed by the trait; the sibling impl consumes `reading` into a Frame"
    )]
    fn probe(&self, reading: FOValue) -> Result<FOValue, String> {
        self.check_not_reentrant();
        let mut engine = self.engine.lock();
        answer_probe(&mut engine.shell, &reading)
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
        _installer: String,
    ) {
        let mut engine = self.engine.lock();
        engine.terminal_lease = endpoint.lease;
    }

    fn detach(&self) {
        // Cancel any in-flight run via the signal-reachable scope.
        crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
        self.engine.lock().terminal_lease = None;
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Wire transport ────────────────────────────────────────────────────

/// How aggressively a front-end manufactures traffic to keep an otherwise
/// idle engine visibly alive, and how much total silence it tolerates
/// before declaring the peer dead (`dev/docs/VM/SYNOD.md` §3).
///
/// Liveness is *any* received frame (the law on [`Frame::Ping`]), so the
/// interval must be comfortably shorter than the deadline: several pings
/// (and their pongs) fall inside one deadline window, and a single dropped
/// exchange never condemns a live peer.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    /// The cadence at which the front-end manufactures `Ping` traffic.
    pub interval: Duration,
    /// The longest total silence tolerated before the peer is declared dead.
    pub deadline: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            deadline: Duration::from_secs(25),
        }
    }
}

/// The out-of-process transport: the engine runs across a duplex stream,
/// frames crossing on a `WireChannel` (length-prefixed JSON).
///
/// Two constructors give it two lives. `WireTransport::new` — Unix only, since
/// what it does is hand a socketpair end to a child on fd 3 — spawns the engine
/// as a same-host child, where the child's death is a kernel-guaranteed EOF and
/// heartbeats would be noise.
/// [`WireTransport::adopt`] drives the protocol over an *existing* stream —
/// in production the virtual-socket connection to a guest VM — whose failure
/// mode is silence rather than EOF, and so runs a heartbeat ticker that
/// manufactures the traffic silence is measured against.
///
/// A dedicated reader thread forwards `Event` frames from the wire into the
/// `EventReceiver` the front-end drains, swallows `Pong` frames (liveness
/// bookkeeping, never surfaced), and records the instant of every received
/// frame so the ticker can tell silence from life.  Writes — `Dispatch`,
/// `Attach`, `Detach`, `Control`, and the ticker's `Ping` — go through a
/// shared `Mutex<WireChannel>` so they are concurrent-safe with each other.
pub struct WireTransport {
    /// Event receiver fed by the reader thread.
    events_recv: EventReceiver,
    /// Control sender that writes `Control` frames to the wire.
    control: ControlSender,
    /// Shared write end, behind a mutex for concurrent access.
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    /// The engine child process, when this transport spawned one.
    /// `Some` under [`WireTransport::new`], whose engine is a same-host
    /// child to be killed and reaped on drop; `None` under
    /// [`WireTransport::adopt`], which drives a stream it does not own the
    /// far end of — and so the only field of this transport that is a
    /// platform's, rather than the protocol's.
    #[cfg(unix)]
    child: Option<ChildHandle>,
    /// Reader thread handle (never joined — the thread exits when the
    /// channel closes).  Wrapped in a `Mutex<Option<…>>` for `Sync`.
    _reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set when the peer dies — a write error, a read EOF, or the ticker's
    /// deadline elapsing.  The reader and ticker both check it to break
    /// early, and it is the honest source for [`WireTransport::dead`].
    death: Arc<AtomicBool>,
    /// The in-flight dispatch id (`0` = none), stamped by `dispatch` and
    /// shared with the published `ControlSender` so `cancel_current` names
    /// the dispatch it means.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
    /// The instant of the last frame the reader received — the fact the
    /// heartbeat ticker measures silence against.
    last_seen: Arc<Mutex<Instant>>,
    /// The heartbeat an adopted stream still owes: parked here by
    /// [`WireTransport::adopt`], taken and spawned by the first
    /// [`Transport::attach`]. `None` under [`WireTransport::new`], whose
    /// socketpair never pings at all.
    pending_heartbeat: Mutex<Option<Liveness>>,
}

/// The reader loop shared by both constructors: forward `Event` frames onto
/// `event_tx`, record the instant of *every* successfully read frame in
/// `last_seen` (the liveness law: any frame is proof of life), swallow
/// `Pong` frames as bookkeeping the front-end never sees, and — on EOF, a
/// read error, or an already-set death flag — set `death` before exiting,
/// so `dead()` is honest under every teardown path and dropping `event_tx`
/// closes the event channel that fails the in-flight dispatch's `recv`.
fn spawn_wire_reader(
    mut reader_ch: crate::wire::WireChannel,
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    death: Arc<AtomicBool>,
    last_seen: Arc<Mutex<Instant>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            if death.load(Ordering::SeqCst) {
                break;
            }
            match reader_ch.read_frame() {
                Ok(Some(frame)) => {
                    *last_seen.lock().unwrap() = Instant::now();
                    // A `Pong`'s whole job was the `last_seen` refresh above.
                    if let Frame::Event(id, ev) = frame
                        && event_tx.send((id, ev)).is_err()
                    {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        death.store(true, Ordering::SeqCst);
    })
}

/// The heartbeat ticker of an adopted stream, spawned by the first
/// [`Transport::attach`] once the `Attach` frame is through the write lock —
/// never at [`WireTransport::adopt`] — so no `Ping` precedes the handshake
/// by construction. Every `liveness.interval` it either exits (death already
/// declared), declares death itself (silence has outrun `liveness.deadline`),
/// or manufactures one `Ping` with an incrementing sequence. On declaring
/// death — from silence or a failed write — it sets the flag and shuts the
/// wire down, so the parked reader wakes, drops its `event_tx`, and closes
/// the event channel whose `recv` returning `None` is what fails the
/// in-flight dispatch. Never joined.
fn spawn_heartbeat(
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    death: Arc<AtomicBool>,
    last_seen: Arc<Mutex<Instant>>,
    liveness: Liveness,
) {
    std::thread::spawn(move || {
        let mut seq: u64 = 0;
        loop {
            std::thread::sleep(liveness.interval);
            if death.load(Ordering::SeqCst) {
                break;
            }
            if last_seen.lock().unwrap().elapsed() >= liveness.deadline {
                death.store(true, Ordering::SeqCst);
                write_tx.lock().unwrap().shutdown();
                break;
            }
            seq += 1;
            if write_tx
                .lock()
                .unwrap()
                .write_frame(&Frame::Ping(seq))
                .is_err()
            {
                death.store(true, Ordering::SeqCst);
                write_tx.lock().unwrap().shutdown();
                break;
            }
        }
    });
}

impl WireTransport {
    /// Spawn the engine child and set up the wire channel.
    ///
    /// Creates a `WireChannel` socketpair, passes one end to the engine
    /// child as fd 3, and starts a reader thread that funnels incoming
    /// `Event` frames into the event receiver.
    ///
    /// No heartbeat ticker is started: a same-host child's death is a
    /// kernel-guaranteed EOF, so heartbeats belong to
    /// [`WireTransport::adopt`].
    ///
    /// # Errors
    /// Returns `Err` if creating the socketpair fails, if duplicating the
    /// front-end fd fails, if resolving the current executable path fails, or
    /// if spawning the engine child process fails.
    #[cfg(unix)]
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

        // Event channel: reader thread writes, front-end drains.
        let (event_tx, event_rx) = mpsc::channel();

        // No ticker under `new`; the shared reader records `last_seen`
        // unconditionally.
        let last_seen = Arc::new(Mutex::new(Instant::now()));
        let reader = spawn_wire_reader(frontend, event_tx, death.clone(), last_seen.clone());

        let write_tx = Arc::new(Mutex::new(writer));
        let current_dispatch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let control = ControlSender::new_wire(write_tx.clone(), current_dispatch.clone());

        Ok(Self {
            events_recv: EventReceiver::new(event_rx),
            control,
            write_tx,
            child: Some(ChildHandle::from_std(child)),
            _reader: Mutex::new(Some(reader)),
            death,
            current_dispatch,
            last_seen,
            pending_heartbeat: Mutex::new(None),
        })
    }

    /// Drive the protocol over an existing duplex `stream` — no child spawn.
    ///
    /// This is the guest-VM path, and the one constructor both platforms have:
    /// `stream` is the virtual-socket connection to the engine running in the
    /// guest — an `AF_VSOCK` descriptor under Virtualization.framework, an
    /// `AF_HYPERV` socket under Hyper-V — adopted through
    /// [`crate::wire::WireStream`], whose docs say why std's stream types can
    /// carry either.  Such a stream can fall silent without ever tearing (a
    /// dead host produces no EOF), so the first [`Transport::attach`] — not
    /// this constructor — spawns a heartbeat ticker under `liveness`, whose
    /// declared death shuts the wire down and closes the event channel: the
    /// mechanism that fails an in-flight dispatch as cancelled and marks the
    /// session detached.
    ///
    /// # Errors
    /// Returns `Err` if the stream cannot be duplicated into separate read
    /// and write handles.
    pub fn adopt(
        stream: impl Into<crate::wire::WireStream>,
        liveness: Liveness,
    ) -> io::Result<Self> {
        let reader_ch = crate::wire::WireChannel::from_stream(stream);
        // A duplicate fd so the reader can park in `read_frame` while the
        // ticker and the front-end write concurrently on the same socket.
        let writer = reader_ch.try_clone()?;

        let death = Arc::new(AtomicBool::new(false));
        let last_seen = Arc::new(Mutex::new(Instant::now()));
        let (event_tx, event_rx) = mpsc::channel();
        let reader = spawn_wire_reader(reader_ch, event_tx, death.clone(), last_seen.clone());

        let write_tx = Arc::new(Mutex::new(writer));
        let current_dispatch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let control = ControlSender::new_wire(write_tx.clone(), current_dispatch.clone());

        Ok(Self {
            events_recv: EventReceiver::new(event_rx),
            control,
            write_tx,
            #[cfg(unix)]
            child: None,
            _reader: Mutex::new(Some(reader)),
            death,
            current_dispatch,
            last_seen,
            pending_heartbeat: Mutex::new(Some(liveness)),
        })
    }

    /// Whether this session's peer has been declared dead — by a write
    /// error, a read EOF, or the heartbeat deadline elapsing. The front-end's
    /// way to mark a session *detached* rather than merely failed: a dead
    /// transport's in-flight dispatch fails as cancelled, and no further
    /// frame will ever cross.
    pub fn dead(&self) -> bool {
        self.death.load(Ordering::SeqCst)
    }

    /// Write a frame to the wire channel. Any write error is fatal: a
    /// dropped dispatch (e.g. `ECONNRESET`, not only `BrokenPipe`) means no
    /// `Report` will ever arrive on this connection, so the host's `recv`
    /// must be told the transport is dead rather than blocking forever.
    fn write(&self, frame: &Frame) {
        let outcome = self.write_tx.lock().unwrap().write_frame(frame);
        if let Err(e) = outcome {
            self.death.store(true, Ordering::SeqCst);
            eprintln!("wire: engine process died ({e})");
        }
    }
}

impl Drop for WireTransport {
    fn drop(&mut self) {
        // Declare death and shut the socket down: this wakes both the reader
        // and (under `adopt`) the ticker, which share the one underlying
        // socket, so neither is left parked past the transport's lifetime.
        self.death.store(true, Ordering::SeqCst);
        self.write_tx.lock().unwrap().shutdown();
        // A same-host child (`new`) is additionally killed and reaped; an
        // adopted stream (`adopt`) has no child to own — its far end is a
        // guest VM the session manager reaps.  Only Unix has the former, so
        // only Unix has anything to reap here.
        #[cfg(unix)]
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait_handling_stop(None, false);
        }
    }
}

impl Transport for WireTransport {
    fn dispatch(&self, id: DispatchId, run: Run) {
        self.current_dispatch
            .store(id.0, std::sync::atomic::Ordering::Relaxed);
        self.write(&Frame::Dispatch(id, Box::new(run)));
    }

    fn probe(&self, reading: FOValue) -> Result<FOValue, String> {
        let id = mint_dispatch_id();
        self.write(&Frame::Probe(id, reading));
        loop {
            match self.events_recv.recv() {
                Some((did, Event::Report(report))) if did == id => {
                    return match report {
                        Report::Ran { result: Ok(v), .. } => Ok(v),
                        Report::Ran {
                            result: Err(Break::Error { rendered, .. }),
                            ..
                        } => Err(rendered),
                        Report::Ran {
                            result: Err(break_),
                            ..
                        } => Err(format!("probe answered abnormally: {break_:?}")),
                        Report::Static { diagnostics } => Err(match diagnostics {
                            Diagnostics::Host(msg) | Diagnostics::Parse(msg) => msg,
                            Diagnostics::Types(errs) => errs.join("\n"),
                        }),
                    };
                }
                // An event that is not this probe's Report — another
                // dispatch's, a deferred worker's batch settling mid-probe
                // — is stashed for the ordinary drain, never dropped
                // (§6.5's ordering law).
                Some(item) => self.events_recv.stash.lock().unwrap().push_back(item),
                None => return Err("engine connection closed".into()),
            }
        }
    }

    fn control(&self) -> &ControlSender {
        &self.control
    }

    fn events(&self) -> &EventReceiver {
        &self.events_recv
    }

    /// TODO Phase 2 Task 6: When the endpoint has a lease, pass the
    /// controlling terminal fds over the socket via `SCM_RIGHTS` (Unix
    /// ancillary data).  For now the endpoint's lease field is
    /// `#[serde(skip)]` and the engine stores it as a placeholder.
    fn attach(
        &self,
        endpoint: TerminalEndpoint,
        cwd: PathBuf,
        home: PathBuf,
        rc_path: Option<PathBuf>,
        installer: String,
    ) {
        self.write(&Frame::Attach {
            endpoint,
            cwd,
            home,
            rc_path,
            proto_version: PROTOCOL_VERSION,
            installer,
        });
        // Only now, with the Attach frame through the write lock, may an
        // adopted stream's heartbeat start: no Ping can precede the handshake.
        let pending = self.pending_heartbeat.lock().unwrap().take();
        if let Some(liveness) = pending {
            spawn_heartbeat(
                self.write_tx.clone(),
                self.death.clone(),
                self.last_seen.clone(),
                liveness,
            );
        }
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
// stub desk. There is no `enquire` builtin yet, so a run's body (parsed ral
// source) has no way to call `Shell::enquire` itself; the round-trip test
// below drives it the same way the run door's own lifecycle
// tests do — a `RunLifecycle` given `&mut Shell` mid-run, exactly the
// shape the migration's first handler will run under.
#[cfg(test)]
mod enquiry_tests {
    use super::*;
    use crate::run::{
        RequestedTerminalAccess, RunIo, RunLifecycle, RunReport, RunRequest, RunStdin,
    };
    use crate::types::{Capabilities, Desk, EnquiryDesk, Error, Shell};
    use std::sync::Mutex;

    /// The minimal capturing request under the ⊤ capability ceiling: no
    /// surface, no deferred sink, no desk — matches `run.rs`'s own
    /// `capture_req`.
    fn capture_req<'a>(src: &str) -> RunRequest<'a> {
        RunRequest {
            run: Run {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        }
    }

    /// A `Shell` with no desk installed answers every enquiry with the
    /// honest absence error, verbatim.
    #[test]
    fn absent_desk_answers_the_honest_error() {
        let shell = Shell::new(crate::io::TerminalState::default());
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

    /// A `RunLifecycle` that enquires mid-run (`pre_exec`, which runs with
    /// the run frame — and its desk — installed) and records the answer.
    #[derive(Clone)]
    struct AskDuringRun {
        req: FOValue,
        answer: std::sync::Arc<Mutex<Option<Result<FOValue, Error>>>>,
    }
    impl RunLifecycle for AskDuringRun {
        fn pre_exec(&mut self, shell: &mut Shell, _src: &str) {
            *self.answer.lock().unwrap() = Some(shell.enquire(self.req.clone()));
        }
    }

    /// `Shell::enquire` round-trips through a stub desk installed on the
    /// run: the desk's transform (`Int{41} -> Int{42}`) is visible to the
    /// caller of `enquire`, not just to the desk.
    #[test]
    fn enquire_round_trips_through_a_stub_desk() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let answer = std::sync::Arc::new(Mutex::new(None));
        let lifecycle = AskDuringRun {
            req: FOValue::Int { value: 41 },
            answer: answer.clone(),
        };

        match shell.run(RunRequest {
            desk: Some(std::sync::Arc::new(IncrementDesk) as Desk),
            lifecycle: Box::new(lifecycle),
            ..capture_req("$[1 + 1]")
        }) {
            RunReport::Ran { .. } => {}
            RunReport::Static { .. } => panic!("`$[1 + 1]` must reach evaluation"),
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
        let transport = std::sync::Arc::new(IdentityTransport::new(Shell::new(
            crate::io::TerminalState::default(),
        )));
        *transport.dispatch_thread.lock().unwrap() = Some(std::thread::current().id());
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.run.desk = Some(std::sync::Arc::new(ReentrantShellMutDesk(transport)) as Desk);
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
                Run {
                    program: Program::Source(String::new()),
                    script_name: "<test>".into(),
                    caps: Capabilities::root(),
                    wall: None,
                    deferred_lease: None,
                    worker_cap: None,
                    io: RunIo::Capture,
                    terminal: RequestedTerminalAccess::Denied,
                    stdin: RunStdin::Empty,
                },
            );
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_dispatch_panics_never_hangs() {
        let transport = std::sync::Arc::new(IdentityTransport::new(Shell::new(
            crate::io::TerminalState::default(),
        )));
        *transport.dispatch_thread.lock().unwrap() = Some(std::thread::current().id());
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.run.desk = Some(std::sync::Arc::new(ReentrantDispatchDesk(transport)) as Desk);
        let _ = shell.enquire(FOValue::Unit);
    }

    /// `IdentityTransport::set_desk` installs the desk that `dispatch` hands
    /// onto the `RunRequest` it builds: the same `Arc` reaches
    /// `EngineInner.desk`.
    #[test]
    fn set_desk_installs_onto_engine_inner() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
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

    /// `IdentityTransport::clear_nursery` uninstalls the nursery
    /// `set_nursery` installed — the symmetric teardown `set_desk`'s
    /// retire-to-`AbsentDesk` pattern has no direct equivalent for, since a
    /// `Nursery` has no sentinel "absent" value. `engine.nursery` reverting
    /// to `None` is what makes a later dispatch's `fork_into_nursery`
    /// answer the honest absence error rather than finding a stale nursery
    /// still installed.
    #[test]
    fn clear_nursery_uninstalls_from_engine_inner() {
        use crate::types::Nursery;

        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        transport.set_nursery(Nursery::default());
        assert!(
            transport.shell_mut().nursery.is_some(),
            "set_nursery must install onto engine.nursery"
        );

        transport.clear_nursery();
        assert!(
            transport.shell_mut().nursery.is_none(),
            "clear_nursery must uninstall it, mirroring set_desk's retirement"
        );
    }
}

// ── Run-door durability, seen through the seam ───────────────────────
#[cfg(test)]
mod durability_tests {
    use super::*;
    use crate::types::Shell;

    fn builtin_panic_now(
        _args: &[crate::types::Value],
        _shell: &mut Shell,
    ) -> crate::types::Settled<crate::types::Value> {
        panic!("transport test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::typecheck::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], &[], thunk(pure(crate::typecheck::Ty::Unit)))
    }

    static PANIC_BUILTINS: &[crate::types::BuiltinEntry] = &[crate::types::BuiltinEntry {
        name: std::borrow::Cow::Borrowed("seam-panic-now"),
        type_rule: crate::typecheck::builtins::BuiltinTypeRule::Scheme(Some(0), scheme_panic_now),
        doc: "test-only: panic the evaluator mid-run.",
        body: crate::types::BuiltinBody::Static(builtin_panic_now),
    }];

    fn run(src: &str) -> Run {
        Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: crate::types::Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: crate::run::RunIo::Capture,
            terminal: crate::run::RequestedTerminalAccess::Denied,
            stdin: crate::run::RunStdin::Empty,
        }
    }

    /// A panicking run dispatched through the identity seam arrives as an
    /// ordinary `Report::Static{Host}` event — the run door caught it and
    /// rolled the shell back — and the session dispatches the next run
    /// cleanly on the same transport.
    #[test]
    fn panicking_dispatch_reports_and_the_session_survives() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(PANIC_BUILTINS);
        let transport = IdentityTransport::new(shell);

        let report = dispatch_to_report(
            &transport,
            run("seam-panic-now"),
            |_| {},
            |_| {},
            |_| {
                Err(EnquiryError {
                    message: "no desk".into(),
                    status: 1,
                })
            },
        )
        .expect("the identity transport sends the Report synchronously");
        match report {
            Report::Static {
                diagnostics: Diagnostics::Host(msg),
            } => assert!(msg.contains("run panicked"), "got {msg:?}"),
            other => panic!("a panicking run must report Static Host, got {other:?}"),
        }

        let report = dispatch_to_report(
            &transport,
            run("$[1 + 1]"),
            |_| {},
            |_| {},
            |_| {
                Err(EnquiryError {
                    message: "no desk".into(),
                    status: 1,
                })
            },
        )
        .expect("the next dispatch must still answer");
        match report {
            Report::Ran { result, .. } => assert!(result.is_ok()),
            Report::Static { .. } => panic!("the healed session must evaluate"),
        }
    }
}

// ── Probe tests ───────────────────────────────────────────────────────
//
// Identity-only: engine-busy serialisation needs a second process racing
// the wire engine's rendezvous, impractical to exercise as a unit test —
// skipped, noted rather than faked.
#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::types::Shell;

    /// `` `worker-count `` on a freshly-built shell answers `0` — nothing has
    /// been spawned.
    #[test]
    fn worker_count_answers_zero_on_a_fresh_shell() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        let answer = transport
            .probe(FOValue::Variant {
                label: "worker-count".into(),
                payload: None,
            })
            .expect("worker-count must answer");
        assert_eq!(answer, FOValue::Int { value: 0 });
    }

    /// `` `binding-count `` and `` `leased-binding-count `` both answer
    /// plain integers — a fresh shell has no leased bindings (the ledger is
    /// unarmed).
    #[test]
    fn binding_probes_answer_integers() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        match transport.probe(FOValue::Variant {
            label: "binding-count".into(),
            payload: None,
        }) {
            Ok(FOValue::Int { .. }) => {}
            other => panic!("binding-count must answer an Int, got {other:?}"),
        }
        let leased = transport
            .probe(FOValue::Variant {
                label: "leased-binding-count".into(),
                payload: None,
            })
            .expect("leased-binding-count must answer");
        assert_eq!(leased, FOValue::Int { value: 0 });
    }

    /// `` `env-var `` answers `` `none `` for a name never set, and
    /// `` `some [value] `` after `Shell::set_env_var` installs one.
    #[test]
    fn env_var_probe_round_trips_the_overlay() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.set_env_var("PROBE_TEST_VAR", "42");
        let transport = IdentityTransport::new(shell);

        let absent = transport
            .probe(FOValue::Variant {
                label: "env-var".into(),
                payload: Some(Box::new(FOValue::String {
                    value: "PROBE_TEST_VAR_ABSENT".into(),
                })),
            })
            .expect("env-var must answer");
        assert_eq!(
            absent,
            FOValue::Variant {
                label: "none".into(),
                payload: None
            }
        );

        let present = transport
            .probe(FOValue::Variant {
                label: "env-var".into(),
                payload: Some(Box::new(FOValue::String {
                    value: "PROBE_TEST_VAR".into(),
                })),
            })
            .expect("env-var must answer");
        assert_eq!(
            present,
            FOValue::Variant {
                label: "some".into(),
                payload: Some(Box::new(FOValue::String { value: "42".into() }))
            }
        );
    }

    /// `` `cwd `` answers the shell's logical cwd as a string.
    #[test]
    fn cwd_probe_answers_a_string() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        match transport.probe(FOValue::Variant {
            label: "cwd".into(),
            payload: None,
        }) {
            Ok(FOValue::String { .. }) => {}
            other => panic!("cwd must answer a String, got {other:?}"),
        }
    }

    /// `` `grant-depth `` answers the grant stack's frame count — at least
    /// the ambient root frame on a fresh shell.
    #[test]
    fn grant_depth_probe_answers_at_least_one() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        match transport.probe(FOValue::Variant {
            label: "grant-depth".into(),
            payload: None,
        }) {
            Ok(FOValue::Int { value }) => assert!(value >= 1),
            other => panic!("grant-depth must answer an Int, got {other:?}"),
        }
    }

    /// `` `largest-binding-bytes `` answers `0` on a bare shell and a
    /// positive shallow estimate once anything is bound.
    #[test]
    fn largest_binding_bytes_probe_measures_without_bindings_and_with() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let transport = IdentityTransport::new(shell);
        let empty = transport
            .probe(FOValue::Variant {
                label: "largest-binding-bytes".into(),
                payload: None,
            })
            .expect("largest-binding-bytes must answer");
        assert_eq!(empty, FOValue::Int { value: 0 });

        shell = transport.into_shell();
        shell.set_var(
            "probe_sized".into(),
            crate::types::Value::String("a dozen bytes or so".into()),
        );
        let transport = IdentityTransport::new(shell);
        match transport.probe(FOValue::Variant {
            label: "largest-binding-bytes".into(),
            payload: None,
        }) {
            Ok(FOValue::Int { value }) => assert!(value > 0),
            other => panic!("must answer a positive Int, got {other:?}"),
        }
    }

    /// `` `workers `` on a fresh shell answers an empty list.
    #[test]
    fn workers_probe_answers_an_empty_list_on_a_fresh_shell() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        let answer = transport
            .probe(FOValue::Variant {
                label: "workers".into(),
                payload: None,
            })
            .expect("workers must answer");
        assert_eq!(answer, FOValue::List { items: vec![] });
    }

    /// An unrecognised probe class answers `Err` naming the class, never a
    /// silent default.
    #[test]
    fn unknown_probe_class_names_itself_in_the_error() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        let err = transport
            .probe(FOValue::Variant {
                label: "not-a-real-class".into(),
                payload: None,
            })
            .expect_err("an unrecognised class must not answer Ok");
        assert!(
            err.contains("not-a-real-class"),
            "error must name the unrecognised class, got: {err}"
        );
    }

    /// A probe request that is not a `Variant` at all — no class to read —
    /// answers `Err` rather than panicking.
    #[test]
    fn non_variant_probe_request_is_an_honest_error() {
        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        let err = transport
            .probe(FOValue::Unit)
            .expect_err("a non-variant probe request must not answer Ok");
        assert!(err.contains("variant"));
    }
}

// ── Runtime-error seam tests ──────────────────────────────────────────
//
// A caught runtime error is rendered to its full diagnostic string at the
// seam, so every front-end (REPL included) regains batch-host parity —
// `error:` prefix, exit status, hint — and prints the string verbatim.
#[cfg(test)]
mod runtime_error_seam_tests {
    use super::*;
    use crate::source::SourceDb;
    use crate::types::{Break as EngineBreak, Error};

    #[test]
    fn runtime_error_projects_to_a_full_diagnostic_string() {
        let db = SourceDb::default();
        let err = Error::new("boom", 3).with_hint("try harder");
        // `single_command` selects the compact one-liner, no source span.
        let Break::Error { rendered, .. } = render_break(EngineBreak::Error(err), &db, true) else {
            panic!("an engine runtime error must project to a transport Break::Error");
        };
        assert!(
            rendered.contains("error"),
            "error prefix missing: {rendered:?}"
        );
        assert!(rendered.contains("boom"), "message missing: {rendered:?}");
        assert!(
            rendered.contains("exit status 3"),
            "exit status missing: {rendered:?}"
        );
        assert!(
            rendered.contains("try harder"),
            "hint missing: {rendered:?}"
        );
        assert!(
            rendered.ends_with('\n'),
            "the seam must supply the trailing newline the host prints verbatim: {rendered:?}"
        );
    }
}

// ── Wire liveness tests ───────────────────────────────────────────────
//
// These drive `WireTransport::adopt` over a `UnixStream::pair`, with a peer
// `WireChannel` on the far end standing in for the guest engine. They prove
// the liveness law of `dev/docs/VM/SYNOD.md` §3: any received frame is proof
// of life, and only genuine silence past the deadline is death. Timing
// margins are deliberately loose — the dev fleet includes a jittery Podman
// VM — so nothing here asserts a tight upper bound, only that death is or is
// not reached within multi-second slack.
#[cfg(all(test, unix))]
mod wire_liveness_tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A brisk heartbeat for the tests: several pings fall inside one
    /// deadline window, and both are far below the slack the assertions wait.
    fn brisk() -> Liveness {
        Liveness {
            interval: Duration::from_millis(25),
            deadline: Duration::from_millis(250),
        }
    }

    /// The attach every session opens with — the ticker does not start
    /// until it is written, so tests that expect heartbeat traffic (or
    /// deadline death) must attach first, exactly as a real front-end does.
    fn attach(transport: &WireTransport) {
        transport.attach(
            TerminalEndpoint {
                lease: None,
                state: crate::io::TerminalState::default(),
            },
            PathBuf::from("/"),
            PathBuf::from("/"),
            None,
            "repl".into(),
        );
    }

    /// `adopt` followed by `attach` puts an `Attach` frame on the wire
    /// carrying `PROTOCOL_VERSION` — the version handshake the guest engine
    /// checks before it will speak (§3).
    #[test]
    fn adopt_then_attach_conveys_the_protocol_version() {
        let (front, back) = UnixStream::pair().unwrap();
        let mut peer = crate::wire::WireChannel::from_stream(back);

        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
        attach(&transport);

        match peer
            .read_frame()
            .unwrap()
            .expect("an Attach frame must arrive")
        {
            Frame::Attach { proto_version, .. } => assert_eq!(proto_version, PROTOCOL_VERSION),
            other => panic!("expected an Attach frame, got {other:?}"),
        }
    }

    /// No `Ping` precedes the handshake, even with the attach delayed far
    /// past several ping intervals: the ticker starts inside `attach`, after
    /// the `Attach` frame is through the write lock, so the first frame on
    /// the wire is `Attach` by construction — not because the front-end
    /// usually wins a race.
    #[test]
    fn no_ping_precedes_the_attach_handshake() {
        let (front, back) = UnixStream::pair().unwrap();
        let mut peer = crate::wire::WireChannel::from_stream(back);

        let transport = WireTransport::adopt(front, brisk()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        attach(&transport);

        match peer.read_frame().unwrap().expect("a frame must arrive") {
            Frame::Attach { .. } => {}
            other => panic!("the first frame on the wire must be Attach, got {other:?}"),
        }
    }

    /// A peer that echoes every `Ping(n)` as `Pong(n)` keeps the session
    /// alive indefinitely: after wall-clock time far past the deadline, the
    /// transport is still not `dead()`. This is the law's positive half —
    /// received frames are proof of life, so a pinging front-end and a
    /// ponging engine never time each other out.
    #[test]
    fn a_ponging_peer_keeps_the_session_alive_past_the_deadline() {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, brisk()).unwrap();
        attach(&transport);

        let peer = std::thread::spawn(move || {
            let mut ch = crate::wire::WireChannel::from_stream(back);
            loop {
                match ch.read_frame() {
                    Ok(Some(Frame::Ping(n))) => {
                        if ch.write_frame(&Frame::Pong(n)).is_err() {
                            break;
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        });

        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !transport.dead(),
            "a ponging peer must keep the session alive well past the deadline"
        );

        drop(transport);
        let _ = peer.join();
    }

    /// A peer that connects and then stays silent forever is declared dead
    /// once silence outruns the deadline, and the front-end's event stream
    /// closes: `events().recv()` returns `None` — the mechanism that fails an
    /// in-flight dispatch as cancelled. The law's negative half: silence, and
    /// only silence, means death.
    #[test]
    fn a_silent_peer_is_declared_dead_and_closes_the_event_stream() {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, brisk()).unwrap();
        attach(&transport);

        // `back` is held open but never speaks: no EOF, only silence, so
        // death can come from nothing but the heartbeat deadline.
        assert!(
            transport.events().recv().is_none(),
            "a silent peer past the deadline must close the event stream"
        );
        assert!(transport.dead(), "silence past the deadline is death");

        drop(back);
    }

    /// A peer that hangs up — the same-EOF teardown a torn stream produces —
    /// closes the event stream and marks death: `events().recv()` returns
    /// `None` and `dead()` becomes true. The reader sets the flag before it
    /// drops its sender, so `dead()` is honest the instant `recv` unblocks.
    #[test]
    fn a_hangup_closes_the_event_stream_and_marks_death() {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();

        drop(back);

        assert!(
            transport.events().recv().is_none(),
            "a hangup must close the event stream"
        );
        assert!(
            transport.dead(),
            "a hangup is death under every teardown path"
        );
    }
}
