//! Transport-parametric host seam: the frame algebra between a front-end and
//! the ral engine.
//!
//! Attach/Detach/Ping/Pong bracket and keep alive one connection — the
//! connection *is* the session — while Dispatch carries one whole run, Event
//! flows engine→front-end, and Control flows front-end→engine.
//!
//! Two transports realise the algebra. `IdentityTransport` is a direct call in
//! one address space: frames move, never encode. `WireTransport` encodes the
//! same frames as length-prefixed JSON over a socket, answered by the engine
//! process in `engine.rs`.
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// Only the same-host child handle is platform-gated: its handoff is a
// socketpair end inherited on fd 3. The frame protocol itself is not.
#[cfg(unix)]
use crate::process::ChildHandle;
use crate::serial::FOValue;
use crate::sync::LockExt;
use crate::types::CapturePolicy;
use crate::types::DeferredSink;
use crate::types::SurfaceSink;
use std::sync::OnceLock;

pub const PROTOCOL_VERSION: u32 = 6;

/// The byte before the first frame, written by a guest that has just spawned
/// a child engine onto this connection and read by the host that dialled it.
///
/// The algebra offers no substitute: the host speaks first and `Attach` is the
/// only legal first frame, so this byte is the whole of the guest's readiness
/// signal — and without it a roster can name a child whose `spawn` has not yet
/// returned. ASCII ACK, and portable, because the two ends need not share an
/// operating system.
pub const HATCH_ACK: u8 = 0x06;

static CURRENT_CONTROL: OnceLock<ControlSender> = OnceLock::new();

// ── Dispatch identity ─────────────────────────────────────────────────

/// Correlation token for one outstanding dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DispatchId(pub u64);

/// Correlation token for one outstanding enquiry, minted by the wire engine's
/// desk. The identity transport, whose enquiry is a direct call, never needs
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnquiryId(pub u64);

// ── Frame algebra ─────────────────────────────────────────────────────

/// One frame that crosses the host seam in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// The only legal first frame: an engine speaks no shell until told a
    /// version and an installer.
    Attach {
        endpoint: TerminalEndpoint,
        cwd: PathBuf,
        home: PathBuf,
        rc_path: Option<PathBuf>,
        /// Checked against `PROTOCOL_VERSION`; a mismatch refuses the attach.
        proto_version: u32,
        /// Tag naming the boot recipe the engine applies once attached —
        /// `"repl"`, `"exarch-agent"`. Each front-end binary re-execs *itself*
        /// with `--engine` and resolves the tag against its own compiled-in
        /// `EngineInstaller` table, so only the tag crosses, never the
        /// functions it names.
        installer: String,
    },
    /// Front-end drops: cancel in-flight dispatch, reap foreground
    /// subtree, restore terminal state.
    Detach,
    /// One whole run.  Boxed so no other variant is sized to `Run`.
    Dispatch(DispatchId, Box<Run>),
    /// A pure, boundary-time read of session state — no wall, no sinks, no
    /// clock, absent by type — hence a `Frame` and not a `Run`. It rides the
    /// engine's worker rendezvous alongside dispatches, so a probe sent mid-run
    /// gets the same "engine busy" answer a second dispatch would, and is
    /// answered on the `Event::Report` rail under the same `DispatchId`. The
    /// `FOValue` is a `Variant` naming the class [`answer_probe`] decodes.
    Probe(DispatchId, FOValue),
    /// Engine → front-end, inside a dispatch's claim-to-Report window. May
    /// arrive while that Dispatch is outstanding.
    Event(DispatchId, Event),
    /// Engine → front-end, outside any dispatch's window: emitted by a
    /// detached worker that settles between runs, so it has no dispatch to
    /// ride and reaches the front-end through the transport's session sink
    /// instead of the per-run event stream.
    Session(SessionEvent),
    /// Front-end → engine. Out-of-band: deliverable while a Dispatch is
    /// outstanding.
    Control(Control),
    /// Front-end → engine: the dual of `Event::Enquiry`, correlated by the
    /// enquiry alone — `WireDesk::fill` keys its slots by `EnquiryId`, and the
    /// dispatch it belongs to is implicit in which enquiry is outstanding.
    Answer(EnquiryId, Result<FOValue, EnquiryError>),
    /// Front-end → engine heartbeat, never sent before `Attach`. Where the
    /// failure mode is silence — a virtual socket into a guest, whose dead host
    /// produces no EOF — liveness must be manufactured: *any* received frame is
    /// proof of life, and pings exist only so that silence can mean nothing but
    /// death. The first Ping arms the engine's read deadline; a front-end that
    /// never pings leaves its patience infinite.
    Ping(u64),
    /// Engine → front-end: the echo of one `Ping`, keeping an *idle* engine
    /// visibly alive.  A busy one already proves itself with `Event` traffic.
    Pong(u64),
}

/// One whole run: the program to evaluate and the conditions it runs under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub program: Program,
    /// Label for the root source context: `"<stdin>"` for the REPL, `"<tool>"`
    /// for exarch.
    pub script_name: String,
    /// The capability ceiling pushed for the eval's dynamic extent;
    /// `Capabilities::root()` is ⊤, the identity on authority.
    pub caps: crate::types::Capabilities,
    /// `Some(d)` arms a deadline cancel on the run's foreground scope `d`
    /// after it starts; `None` leaves the run uncapped.
    pub wall: Option<std::time::Duration>,
    /// For workers deferred at the durable root. `None` keeps one until
    /// `cancel`, root abort, or session exit; `Some` reaps a still-running one
    /// unobserved for `lease.idle` — renewed by every `poll`/`await`/`race`
    /// naming its handle — under `lease.backstop`.
    pub deferred_lease: Option<crate::types::WorkerLease>,
    /// Enforced at the spawn door: `Some(cap)` refuses a spawn while `cap`
    /// workers of any class still run. Settled entries lingering under
    /// retention never block admission.
    pub worker_cap: Option<usize>,
    pub io: crate::run::RunIo,
    pub terminal: crate::run::RequestedTerminalAccess,
    pub stdin: crate::run::RunStdin,
    /// `Some` delimits this dispatch's own trail, at the given capture
    /// policy, and carries it home on [`Report::Ran`]; `None` neither opens a
    /// scope nor collects one. exarch's tool calls ask with
    /// [`CapturePolicy::Off`]; the REPL never asks.
    pub trail: Option<CapturePolicy>,
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

/// Engine → front-end event frame, emitted between a dispatch's claim and
/// that dispatch's Report. Anything produced outside that window is a
/// [`SessionEvent`], carried by `Frame::Session` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// A live surface value from the foreground run, ordered before this
    /// dispatch's Report.
    Surface(FOValue),
    /// One enquiry the running dispatch raised on its host desk, ordered
    /// before this dispatch's Report and answered by a `Frame::Answer`
    /// carrying the same `EnquiryId`.
    Enquiry(EnquiryId, FOValue),
    /// The dispatch's sole terminal frame.
    Report(Report),
}

/// Engine → front-end event with no dispatch to ride.
///
/// Produced by something that outlives the run which started it. One variant
/// today — a detached worker's surface batch, settling after its spawning
/// run's Report — but the enum, not `Frame`, is where the next session-lived
/// event class joins it: this names the lifetime class once, so a new member
/// costs one variant here rather than a new frame kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// A deferred worker's surface batch, delivered when it settles and
    /// rendered by the host at the next run boundary.
    DeferredSurface(Vec<FOValue>),
}

/// What crosses when an enquiry is refused or its handler fails: message *and*
/// status, so a refusal raises the same error under both transports.
///
/// Not `crate::types::Error`, whose `span` resolves against an engine-side
/// `SourceDb`; the location is stamped at the enquiring builtin instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnquiryError {
    pub message: String,
    pub status: i32,
}

impl EnquiryError {
    /// The refusal of a host that installed no desk, in the one wording
    /// [`crate::types::NO_DESK`] fixes.
    #[must_use]
    pub fn no_desk() -> Self {
        Self {
            message: crate::types::NO_DESK.to_string(),
            status: crate::types::NO_DESK_STATUS,
        }
    }
}

/// Front-end → engine out-of-band control frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    Cancel(DispatchId),
    /// SIGTSTP semantics.
    Suspend,
    Resume,
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
    /// The session lease, when the front-end owns a terminal. `#[serde(skip)]`:
    /// no terminal descriptor crosses a wire, so an engine child attaches
    /// without one and never foregrounds against the host's terminal.
    #[serde(skip)]
    pub lease: Option<crate::process::TerminalLease>,
    pub state: crate::io::TerminalState,
}

/// `TerminalLease` is deliberately not `Clone` — its security argument rests on
/// being unforgeable — so a cloned endpoint carries no lease.
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
    /// The run ran to a settled result.
    Ran {
        ending: Ending,
        /// `Some` under [`RunIo::Capture`](crate::run::RunIo).
        captured: Option<crate::run::Captured>,
        /// The dispatch's own trail, projected; empty unless [`Run::trail`]
        /// asked. Unbounded by declaration — the wire's frame fuse is the
        /// shared backstop, exactly as it is for `captured`.
        trail: Vec<FOValue>,
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

/// How a run left evaluation, wire-shaped.
///
/// The protocol projection of the engine's [`Ending`](crate::run::Ending),
/// rendered against a [`SourceDb`](crate::source::SourceDb) — a live `Error`
/// cannot cross the seam, so [`Self::Raised`]/[`Self::Walled`] carry the
/// string it rendered to instead. `status` is carried explicitly wherever it
/// is not the whole of the arm's payload, since a renderer that has already
/// discarded the engine `Error` for its `rendered` string has no other way
/// back to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Ending {
    Settled {
        value: FOValue,
        status: i32,
    },
    /// `command_exit` says whether the status was an external command's
    /// non-zero exit rather than a raised error — the one classification a
    /// host's didactics need from a `Status` that never crosses the seam.
    /// `single_command` picks between the exit-code remedy's two wordings.
    Raised {
        rendered: String,
        command_exit: bool,
        single_command: bool,
        status: i32,
    },
    /// A raise the wall itself caused. No `command_exit`/`single_command`:
    /// the timeout remedy is unconditional, so no renderer reads them here.
    Walled {
        rendered: String,
        status: i32,
    },
    Exited(i32),
    /// `pending` is the run's unfinished atomic writes, which the host parks
    /// with the job: its end, not this frame's, decides each one.
    #[cfg(unix)]
    Stopped {
        pgid: i32,
        signal: i32,
        signal_name: String,
        pending: Vec<crate::PendingWrite>,
    },
}

impl Ending {
    /// The flat status view: the wire's own `status` field for the arms that
    /// carry one, the whole of the payload for the two that don't.
    #[must_use]
    pub fn status(&self) -> i32 {
        match self {
            Self::Settled { status, .. }
            | Self::Raised { status, .. }
            | Self::Walled { status, .. } => *status,
            Self::Exited(code) => *code,
            #[cfg(unix)]
            Self::Stopped { signal, .. } => 128 + signal,
        }
    }
}

/// Project an engine [`Ending`](crate::run::Ending) onto the wire, rendering
/// a caught runtime error against `sources` — the one lossy step between the
/// engine and the seam: the live `Error` renders to the string the host
/// prints verbatim.
fn render_ending(ending: crate::run::Ending, sources: &crate::source::SourceDb) -> Ending {
    use crate::run::Ending as Raw;
    match ending {
        Raw::Settled { value, status } => {
            let status = status.clamp(0, 255);
            // A top-level result is not an `Observation` — it has no
            // placeholder vocabulary of its own — so a value the wire cannot
            // carry (a live `Handle`, say) is reported as an error instead of
            // silently dropped.
            match FOValue::try_from(&value) {
                Ok(value) => Ending::Settled { value, status },
                Err(_) => Ending::Raised {
                    rendered: "run result is not transportable across the host seam".into(),
                    command_exit: false,
                    single_command: false,
                    status,
                },
            }
        }
        Raw::Raised {
            error,
            single_command,
            root,
        } => render_raise(&error, single_command, root, sources, false),
        Raw::Walled {
            error,
            single_command,
            root,
        } => render_raise(&error, single_command, root, sources, true),
        Raw::Exited(code) => Ending::Exited(code.clamp(0, 255)),
        #[cfg(unix)]
        Raw::Stopped {
            pgid,
            signal,
            pending,
            ..
        } => Ending::Stopped {
            pgid: pgid.as_raw(),
            signal: signal.number(),
            signal_name: signal.name().unwrap_or("?").to_string(),
            pending,
        },
    }
}

/// `Raised` and `Walled` differ only in tag: same rendering, same
/// `command_exit`/`single_command` inputs, distinguished by `walled` alone.
fn render_raise(
    error: &crate::types::Error,
    single_command: bool,
    root: crate::source::FileId,
    sources: &crate::source::SourceDb,
    walled: bool,
) -> Ending {
    let status = error.exit_code().clamp(0, 255);
    let command_exit = matches!(
        error.status,
        crate::types::Status::Process(crate::process::CommandFailure::ExitCode(_))
    );
    let rendered = crate::diagnostic::format_runtime_error_auto(
        sources,
        error,
        single_command.then_some(root),
    );
    if walled {
        Ending::Walled { rendered, status }
    } else {
        Ending::Raised {
            rendered,
            command_exit,
            single_command,
            status,
        }
    }
}

impl crate::run::RunReport {
    /// Project into the protocol [`Report`] — the one lossy step between the
    /// engine and the seam: the live `Value` becomes an [`FOValue`], and rich
    /// diagnostics render to strings. Rendering belongs here because this is
    /// the last point at which the engine's
    /// [`SourceDb`](crate::source::SourceDb) is in hand; the host receives the
    /// full string — prefix, status, hint, caret — and only has to print it.
    ///
    /// # Panics
    /// Never: [`Observation::to_wire`] is total over exactly the vocabulary
    /// [`FOValue`] admits.
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
                ending,
                captured,
                trail,
            } => Report::Ran {
                ending: render_ending(ending, sources),
                captured,
                trail: trail
                    .iter()
                    .map(|obs| {
                        FOValue::try_from(&obs.to_wire())
                            .expect("Observation::to_wire is total over the wire vocabulary")
                    })
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
mod ending_wire_round_trip_tests {
    //! The Report round-trips over every `Ending` arm: whatever a wire engine
    //! encodes, a front-end decodes back byte-for-byte.

    use super::*;

    fn ran(ending: Ending) -> Report {
        Report::Ran {
            ending,
            captured: None,
            trail: Vec::new(),
        }
    }

    fn round_trips(report: &Report) {
        let encoded = serde_json::to_string(report).expect("Report must encode");
        let decoded: Report = serde_json::from_str(&encoded).expect("Report must decode");
        assert_eq!(&decoded, report, "round trip must be lossless: {encoded}");
    }

    #[test]
    fn settled_round_trips() {
        round_trips(&ran(Ending::Settled {
            value: FOValue::Int { value: 1 },
            status: 0,
        }));
    }

    #[test]
    fn raised_round_trips() {
        round_trips(&ran(Ending::Raised {
            rendered: "error: boom\n".into(),
            command_exit: true,
            single_command: false,
            status: 7,
        }));
    }

    #[test]
    fn walled_round_trips() {
        round_trips(&ran(Ending::Walled {
            rendered: "error: timed out\n".into(),
            status: 143,
        }));
    }

    #[test]
    fn exited_round_trips() {
        round_trips(&ran(Ending::Exited(3)));
    }

    #[cfg(unix)]
    #[test]
    fn stopped_round_trips() {
        round_trips(&ran(Ending::Stopped {
            pgid: 1234,
            signal: 20,
            signal_name: "SIGTSTP".into(),
            // Non-empty on purpose: a stopped job's staged writes have to
            // reach the host that will finish them.
            pending: vec![crate::PendingWrite {
                tmp: ".sigil.ral-write.tmp".into(),
                target: "out.txt".into(),
            }],
        }));
    }

    #[test]
    fn static_round_trips() {
        round_trips(&Report::Static {
            diagnostics: Diagnostics::Host("boom".into()),
        });
    }
}

// ── Transport trait ───────────────────────────────────────────────────

/// The front-end side of the host seam.
pub trait Transport: Send + Sync {
    /// Run a dispatch synchronously. The `Report` arrives as the final `Event`,
    /// after any `Surface` events, which may be drained concurrently.
    fn dispatch(&self, id: DispatchId, run: Run);

    /// Read session state at a run boundary, synchronously.
    ///
    /// # Errors
    /// The `Err` string conflates two failures different in kind: the
    /// rendezvous held by an in-flight dispatch ("engine busy", transport
    /// state) and an unrecognised reading class (a program error, naming
    /// it). No host may string-match either out of this text — `probe` is
    /// legal only at a run boundary, so a caller that could observe "engine
    /// busy" has already broken that rule. This is why
    /// `exarch/src/agent/probe.rs` treats the whole channel as
    /// `unreachable!`.
    fn probe(&self, reading: FOValue) -> Result<FOValue, String>;

    /// The out-of-band control sender — writable while a dispatch is in
    /// flight.  Under the identity transport this raises the ambient
    /// foreground interrupt directly.
    fn control(&self) -> &ControlSender;

    /// The event stream the front-end drains.
    fn events(&self) -> &EventReceiver;

    /// Convey the session terminal endpoint and bootstrap state.
    ///
    /// `installer` is the boot-recipe tag of `Frame::Attach`. The identity
    /// transport ignores it: its shell was booted and dressed with the host's
    /// builtins by the caller before `attach`.
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

    /// Answer one enquiry the engine raised on `Event::Enquiry`. A no-op under
    /// the identity transport, whose enquiry is a direct call through the
    /// installed `Desk` and never a frame.
    fn answer(&self, _eid: EnquiryId, _answer: Result<FOValue, EnquiryError>) {}

    /// Install the session sink a `Frame::Session` batch is delivered to.
    /// With no sink installed, a batch is dropped — a front-end explicitly
    /// declining session events, not a loss the transport owes anyone.
    fn set_deferred_sink(&self, sink: Arc<dyn DeferredSink>);

    /// Returns `self` as an `&dyn Any` for downcast support.
    fn as_any(&self) -> &dyn std::any::Any;
}

// ── Dispatch loop ─────────────────────────────────────────────────────

/// Mint a dispatch id, send `run` down `transport`, and drain events to the
/// run's terminal [`Report`](Event::Report).
///
/// `on_enquiry` is dead under the identity transport, whose `Desk` the run
/// calls directly mid-evaluation; under the wire it is the front-end's side of
/// that rendezvous, answered through [`Transport::answer`].
///
/// The `did != id` filter rejects a cancelled predecessor's late run frames,
/// and nothing else: every `Event` belongs to some dispatch, and this is the
/// one whose Report is awaited. A session-lived batch never reaches here at
/// all — it rides `Frame::Session`, delivered to whatever sink
/// `Transport::set_deferred_sink` installed.
///
/// `None` means the stream closed without a Report — impossible under the
/// identity transport, which sends it before `dispatch` returns.
pub fn dispatch_to_report(
    transport: &dyn Transport,
    run: Run,
    mut on_surface: impl FnMut(FOValue),
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
            Event::Enquiry(eid, req) => {
                let answer = on_enquiry(req);
                transport.answer(eid, answer);
            }
            Event::Report(report) => return Some(report),
        }
    }
    None
}

/// Dispatches and probes share this mint: both are answered by an
/// [`Event::Report`] under a [`DispatchId`], so two counters would let a
/// probe's Report be mistaken for a dispatch's.
fn mint_dispatch_id() -> DispatchId {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    DispatchId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

// ── Probe decoder ─────────────────────────────────────────────────────

/// The one decoder for every probe reading, shared by `IdentityTransport` and
/// the wire engine in `engine.rs`, so a new class costs one arm here rather
/// than a copy per transport.
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

/// What an identity transport's `Control::Cancel` arm aims at: the id and
/// scope of the dispatch it last minted, written by
/// [`IdentityTransport::dispatch`] ahead of the engine lock, so a cancel
/// naming that dispatch has a scope to land on before the run's own frame is
/// born. Replaced each dispatch, never cleared — so between runs it still
/// names a settled one, and the id is what lets a cancel that outlived its
/// run miss rather than strike the successor.
type CancelTarget = Arc<std::sync::Mutex<Option<(DispatchId, crate::process::ForegroundScope)>>>;

/// Out-of-band control sender.
#[derive(Clone)]
pub struct ControlSender {
    /// `Some` writes `Control` frames to a `WireChannel`, carrying that
    /// transport's death flag so a failed control write declares the death a
    /// failed data write declares; `None` acts on the in-process foreground
    /// scope instead.
    wire: Option<(Arc<Mutex<crate::wire::WireChannel>>, Arc<AtomicBool>)>,
    /// Shared with the transport's own event-correlation bookkeeping, so
    /// `cancel_current` names a real dispatch rather than a sentinel.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
    /// `Some` for an identity sender, `None` for a wire one, whose cancel
    /// already reaches the run across the wire without a local scope to trip.
    cancel_target: Option<CancelTarget>,
}

impl ControlSender {
    pub(crate) fn new(
        current_dispatch: Arc<std::sync::atomic::AtomicU64>,
        cancel_target: CancelTarget,
    ) -> Self {
        Self {
            wire: None,
            current_dispatch,
            cancel_target: Some(cancel_target),
        }
    }

    pub(crate) fn new_wire(
        ch: Arc<Mutex<crate::wire::WireChannel>>,
        death: Arc<AtomicBool>,
        current_dispatch: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            wire: Some((ch, death)),
            current_dispatch,
            cancel_target: None,
        }
    }

    pub(crate) fn publish(self) {
        let _ = CURRENT_CONTROL.set(self);
    }

    /// Cancel this sender's own in-flight dispatch, down whichever channel it
    /// writes.
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
        if let Some((ch, death)) = &self.wire {
            // A control frame that cannot be written is as fatal as a dispatch
            // that cannot: the peer is gone, and waiting on the reader's
            // eventual EOF to say so leaves a cancel silently lost meanwhile.
            let _ = write_through(ch, death, &Frame::Control(ctrl));
            return;
        }
        match ctrl {
            Control::Cancel(id) => {
                // Cancellation is sticky and a fold walks the chain live, so a
                // scope cancelled here before the run's frame exists is still
                // observed once that frame is minted a descendant of it.
                if let Some(target) = &self.cancel_target {
                    let recorded = target.lock_ignore_poison();
                    if let Some((current, scope)) = recorded.as_ref()
                        && *current == id
                    {
                        scope.cancel(crate::process::CancelCause::Explicit);
                    }
                }
            }
            Control::Suspend | Control::Resume | Control::Resize(_) => {
                // The process-level signal machinery already handles these.
            }
        }
    }
}

/// The front-end drains this while a dispatch is outstanding; `Surface` events
/// are ordered before the `Report`.
///
/// Admits exactly one drainer at a time — [`dispatch_to_report`]'s loop and a
/// desk's own pre-drain (`exarch`'s `DeskBinding::enquire`) never run
/// together, because the desk call happens synchronously inside
/// `Transport::dispatch`, on the same thread, strictly before the loop's own
/// first `recv`. The `stash` and `rx` mutexes exist only to make
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
    pub fn try_recv(&self) -> Option<(DispatchId, Event)> {
        let stashed = self.stash.lock_ignore_poison().pop_front();
        if let Some(item) = stashed {
            return Some(item);
        }
        self.rx.lock_ignore_poison().try_recv().ok()
    }
}

#[cfg(test)]
mod event_receiver_tests {
    use super::*;

    fn receiver() -> (EventReceiver, mpsc::Sender<(DispatchId, Event)>) {
        let (tx, rx) = mpsc::channel();
        (EventReceiver::new(rx), tx)
    }

    /// The stash precedence `recv` gives, proven on the non-blocking door.
    #[test]
    fn try_recv_drains_the_stash_before_the_channel() {
        let (receiver, tx) = receiver();
        let channelled = (DispatchId(2), Event::Surface(FOValue::Unit));
        let stashed = (DispatchId(1), Event::Surface(FOValue::Unit));
        tx.send(channelled.clone()).unwrap();
        receiver.stash.lock().unwrap().push_back(stashed.clone());

        assert_eq!(receiver.try_recv(), Some(stashed));
        assert_eq!(receiver.try_recv(), Some(channelled));
    }

    /// Empty on both sides returns `None` rather than blocking.
    #[test]
    fn try_recv_returns_none_when_empty() {
        let (receiver, _tx) = receiver();
        assert_eq!(receiver.try_recv(), None);
    }
}

// ── Transport sink ────────────────────────────────────────────────────

/// The identity transport's sink for a live surface value
/// ([`EventSink`](crate::types::EventSink)), forwarded onto the event
/// channel.
struct TransportSink {
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    /// `0` = no dispatch in flight, so a value emitted then has nothing to
    /// correlate to and is dropped.
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

// ── Identity transport ────────────────────────────────────────────────

/// A session lock that cannot poison-panic, named for the one state it
/// guards: a run that unwinds drops its guard mid-mutation, and recovering
/// the poison (via [`LockExt`]) rather than unwrapping it means such a run
/// can never wedge the session for whatever runs next.
struct SessionLock(std::sync::Mutex<EngineInner>);

impl SessionLock {
    fn new(inner: EngineInner) -> Self {
        Self(std::sync::Mutex::new(inner))
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, EngineInner> {
        self.0.lock_ignore_poison()
    }
    /// Recovers poison the same way `lock` does: an unwound run must not wedge
    /// the move-out either.
    fn into_inner(self) -> EngineInner {
        self.0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The in-process transport: one kernel, one address space.
///
/// `dispatch` runs the engine door synchronously on the calling thread; a
/// `Control::Cancel` trips the scope `dispatch` mints for its own id ahead of
/// the engine lock, rather than crossing anything.
pub struct IdentityTransport {
    /// Behind a poison-free [`SessionLock`] so `dispatch` can take `&self`.
    engine: SessionLock,
    control: ControlSender,
    /// `Arc`-wrapped so a drain-then-handle adapter can hold its own clone
    /// alongside the transport rather than a borrow tied to `&self`.
    events_recv: Arc<EventReceiver>,
    /// Stamped for the duration of `dispatch`, so a desk handler reentering
    /// `dispatch`/`shell_mut`/`with_shell` panics instead of deadlocking. A
    /// separate short lock: checking it must not touch `self.engine`.
    dispatch_thread: std::sync::Mutex<Option<std::thread::ThreadId>>,
    /// The destination a host injects through [`Self::set_interrupt_target`],
    /// republished with each dispatch's scope. Outside `engine`'s lock, so a
    /// holder of a clone can cancel the *current* dispatch from another thread
    /// without waiting on it — how an observing host interrupts one run
    /// without touching the session's durable root. An interrupt names no id,
    /// so it needs none: it strikes whatever runs now.
    interrupt_target: Option<Arc<std::sync::Mutex<Option<crate::process::ForegroundScope>>>>,
    /// Minted once at construction; each dispatch hangs a child off it, ahead
    /// of the engine lock, so a `Control::Cancel` naming that dispatch always
    /// has a scope to land on.
    dispatches: crate::process::ForegroundScope,
    /// The id and scope of the last dispatch to claim `dispatches` — shared
    /// with `control`'s `Control::Cancel` arm, which uses the id to tell a
    /// live cancel from one that outlived its run.
    cancel_target: CancelTarget,
}

pub struct EngineInner {
    pub shell: crate::types::Shell,
    pub(crate) event_tx: mpsc::Sender<(DispatchId, Event)>,
    surface_sink: Arc<TransportSink>,
    /// Session-lived: `None` until a host installs one through
    /// `set_deferred_sink`, and while it is `None` a settling worker's batch is
    /// dropped.
    deferred_sink: Option<Arc<dyn DeferredSink>>,
    /// `None` until a host calls `set_desk`, in which case `enquire` answers
    /// its honest absence error.
    desk: Option<crate::types::Desk>,
    /// `None` until a host calls `set_fork`, in which case
    /// `fork_into_nursery` answers its honest absence error.
    fork: Option<crate::types::Fork>,
    /// Set by Attach.
    terminal_lease: Option<crate::process::TerminalLease>,
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
}

/// Clears the dispatch-thread stamp on drop, unwind included: a stamp left set
/// by a panicking run would false-trip the next legitimate `shell_mut`.
struct DispatchStampGuard<'a> {
    slot: &'a std::sync::Mutex<Option<std::thread::ThreadId>>,
}

impl Drop for DispatchStampGuard<'_> {
    fn drop(&mut self) {
        *self.slot.lock_ignore_poison() = None;
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
        let dispatches = shell.run_cancel_handle();
        let cancel_target: CancelTarget = Arc::new(std::sync::Mutex::new(None));
        let control = ControlSender::new(current_dispatch.clone(), cancel_target.clone());
        control.clone().publish();

        let engine = EngineInner {
            shell,
            event_tx,
            surface_sink: sink,
            deferred_sink: None,
            desk: None,
            fork: None,
            terminal_lease: None,
            current_dispatch,
        };

        Self {
            engine: SessionLock::new(engine),
            control,
            events_recv: Arc::new(EventReceiver::new(event_rx)),
            dispatch_thread: std::sync::Mutex::new(None),
            interrupt_target: None,
            dispatches,
            cancel_target,
        }
    }

    /// A shared handle on the event receiver, for a caller that drains it
    /// alongside the transport rather than through a borrow of `&self`.
    pub fn events_shared(&self) -> Arc<EventReceiver> {
        self.events_recv.clone()
    }

    /// Install `target` as where an interrupt lands. This call writes no scope;
    /// each later dispatch republishes its own into it as the dispatch is
    /// minted — ahead of the engine lock, so the target never names a run that
    /// has already ended while its successor waits to begin. Call once, right
    /// after construction: exarch's agent fleet keeps its own clone, so an
    /// interrupt cancels the dispatch in flight without touching the durable
    /// root a later run would inherit.
    ///
    /// The target outlives any one transport — a `/clear` rebuild republishes
    /// into the same one — so the host injects it rather than borrowing ours.
    pub fn set_interrupt_target(
        &mut self,
        target: Arc<std::sync::Mutex<Option<crate::process::ForegroundScope>>>,
    ) {
        self.interrupt_target = Some(target);
    }

    /// Install the session's enquiry desk. A per-run host calls this before
    /// each dispatch, so whatever the desk captures is fresh.
    pub fn set_desk(&self, desk: crate::types::Desk) {
        self.engine.lock().desk = Some(desk);
    }

    /// Install the session's [`Fork`](crate::types::Fork) door. Called per run,
    /// so a stale fork from an earlier generation is never adoptable.
    pub fn set_fork(&self, fork: crate::types::Fork) {
        self.engine.lock().fork = Some(fork);
    }

    /// Uninstall it. `set_desk` needs no counterpart — a desk retires by
    /// installing one that answers nothing — but an empty `Nursery` would still
    /// accept a park, so `None` is the only honest absence here.
    pub fn clear_fork(&self) {
        self.engine.lock().fork = None;
    }

    /// The inverse of [`IdentityTransport::new`], for a caller that lent a
    /// shell in to route one run through production.
    pub fn into_shell(self) -> crate::types::Shell {
        self.engine.into_inner().shell
    }

    /// A desk handler reaching back through `dispatch`/`shell_mut`/`with_shell`
    /// would deadlock on the session lock its own stack holds. Checked *before*
    /// touching `self.engine`, so it panics rather than hangs.
    fn check_not_reentrant(&self) {
        let current = std::thread::current().id();
        assert!(
            *self.dispatch_thread.lock_ignore_poison() != Some(current),
            "reentrant session access: a desk handler must not take the session lock \
             (dispatch/shell_mut/with_shell) — it runs inside the dispatch on the \
             dispatching thread and would deadlock under the identity transport"
        );
    }

    /// The guard itself, for a caller that must hold shell access across other
    /// borrows (the REPL's frontend and jobs table). Prefer `with_shell`.
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
    fn dispatch(&self, id: DispatchId, run: Run) {
        self.check_not_reentrant();

        // Minted ahead of the engine lock, so a cancel raised in that window —
        // a `Control::Cancel` naming `id`, or an observing host's interrupt —
        // has a scope to land on even while this call is still waiting to
        // acquire it. Both targets name the same scope; only the control arm
        // needs the id, because only a cancel names which dispatch it meant.
        let scope = self.dispatches.child();
        *self.cancel_target.lock_ignore_poison() = Some((id, scope.clone()));
        if let Some(target) = &self.interrupt_target {
            *target.lock_ignore_poison() = Some(scope.clone());
        }

        let mut engine = self.engine.lock();
        *self.dispatch_thread.lock_ignore_poison() = Some(std::thread::current().id());
        let _stamp_guard = DispatchStampGuard {
            slot: &self.dispatch_thread,
        };

        // The one atomic both sinks read to stamp and gate their frames.
        engine
            .current_dispatch
            .store(id.0, std::sync::atomic::Ordering::Relaxed);

        // The live, non-transportable handles this dispatch lends the run,
        // joined with the protocol `Run` the engine door takes.
        let req = crate::run::RunRequest {
            run,
            surface: Some(engine.surface_sink.clone() as SurfaceSink),
            deferred: engine.deferred_sink.clone(),
            desk: engine.desk.clone(),
            fork: engine.fork.clone(),
            lifecycle: Box::new(()),
        };
        let run_report = engine.shell.run_under(&scope, req);
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
        crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
        self.engine.lock().terminal_lease = None;
    }

    fn set_deferred_sink(&self, sink: Arc<dyn DeferredSink>) {
        self.engine.lock().deferred_sink = Some(sink);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod identity_cancel_tests {
    use super::*;
    use crate::run::{RequestedTerminalAccess, RunIo, RunStdin};
    use crate::types::{Capabilities, Shell};
    use std::time::{Duration, Instant};

    fn sleep_run(src: &str) -> Run {
        Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        }
    }

    /// A `Control::Cancel` sent while `dispatch` is still waiting on the
    /// engine lock — before the run's own frame exists — must still settle
    /// the run promptly rather than let it run to completion. The engine
    /// lock is held here so `dispatch`, called on another thread, is forced
    /// to park right after it records its scope and before it ever reaches
    /// `shell.run_under`; `face_signals` makes the run's frame signal-facing,
    /// so a failure here is the race and not a plain absence of wiring.
    #[test]
    fn a_cancel_racing_a_fresh_dispatch_is_not_dropped() {
        let _g = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let transport = Arc::new(IdentityTransport::new(shell));

        let guard = transport.shell_mut();

        let worker = {
            let transport = transport.clone();
            std::thread::spawn(move || {
                transport.dispatch(DispatchId(1), sleep_run("sleep 30"));
            })
        };

        // `dispatch` records its scope ahead of the engine lock; wait for
        // that record rather than guessing at a delay.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if matches!(
                *transport.cancel_target.lock_ignore_poison(),
                Some((current, _)) if current == DispatchId(1)
            ) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "dispatch must record its scope before acquiring the engine lock"
            );
            std::thread::yield_now();
        }

        transport.control().send(Control::Cancel(DispatchId(1)));
        drop(guard);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.join().expect("dispatch must not panic");
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a cancel recorded before the run's frame exists must still settle it promptly, \
             not run `sleep 30` to completion"
        );
    }

    /// The same race down the other cancel channel: an observing host — exarch's
    /// fleet, holding a clone of the interrupt target — interrupts by cancelling
    /// whatever scope it finds there. Published as the dispatch is minted rather
    /// than once its run frame exists, the target can never name a run that has
    /// already ended while its successor waits on the engine lock. The shell is left
    /// deaf to signals here, so nothing but this cancel can settle the run.
    #[test]
    fn an_observed_interrupt_racing_a_fresh_dispatch_is_not_dropped() {
        let target: Arc<std::sync::Mutex<Option<crate::process::ForegroundScope>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        transport.set_interrupt_target(target.clone());
        let transport = Arc::new(transport);

        let guard = transport.shell_mut();

        let worker = {
            let transport = transport.clone();
            std::thread::spawn(move || {
                transport.dispatch(DispatchId(1), sleep_run("sleep 30"));
            })
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        let scope = loop {
            let published = target.lock_ignore_poison().clone();
            if let Some(scope) = published {
                break scope;
            }
            assert!(
                Instant::now() < deadline,
                "the observed cell must hold this dispatch's scope before the engine lock"
            );
            std::thread::yield_now();
        };

        scope.cancel(crate::process::CancelCause::Interrupt);
        drop(guard);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.join().expect("dispatch must not panic");
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "an interrupt on the observed scope must settle the run it names even when raised \
             before that run's frame is born"
        );
    }
}

// ── Wire transport ────────────────────────────────────────────────────

/// The one front-end write door: locks `ch`, writes `frame`, and on error
/// severs the connection before the lock is released — stores `death`, shuts
/// the channel down while the guard is still held, and reports the failure.
/// Shared by [`WireTransport::write`] and the wire arm of [`ControlSender::send`].
///
/// Invariant: a stream that suffered a failed write is severed before the
/// lock is released, so nothing can append a fresh frame after a truncated
/// one. `Err` is handed back so a caller that must react further — the
/// heartbeat's `Ping` arm breaking its loop — can, without repeating the
/// severing itself.
///
/// Deliberately outside [`LockExt`]'s recover-poison policy: a panic between
/// `write_frame` and `shutdown` can leave a partial frame already on the
/// socket, severed by neither `death` nor a shutdown call. Recovering the
/// poison would let the next writer resume into that torn frame stream, which
/// is worse than the panic propagating; a poisoned `ch` must stay poisoned.
fn write_through(
    ch: &Mutex<crate::wire::WireChannel>,
    death: &AtomicBool,
    frame: &Frame,
) -> io::Result<()> {
    let outcome = {
        let mut guard = ch.lock().unwrap();
        let outcome = guard.write_frame(frame);
        if outcome.is_err() {
            guard.shutdown();
        }
        outcome
    };
    if let Err(e) = &outcome {
        death.store(true, Ordering::SeqCst);
        eprintln!("wire: engine process died ({e})");
    }
    outcome
}

/// How briskly a front-end manufactures traffic to keep an idle engine
/// visibly alive, and how much silence it tolerates before declaring the
/// peer dead.
///
/// The same `deadline` also arms [`crate::wire::WireChannel::set_write_deadline`],
/// so it doubles as the bound on how long a single stalled write may block
/// before that, too, is read as the peer's death.
///
/// Since liveness is *any* received frame (the law on [`Frame::Ping`]), the
/// interval must be comfortably shorter than the deadline: several exchanges
/// fall inside one window, so a single dropped one never condemns a live peer.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    pub interval: Duration,
    pub deadline: Duration,
}

/// The peer-patience deadline used wherever one socket needs a single
/// deadline but no full [`Liveness`]: [`WireTransport::new`]'s same-host child
/// (no ticker, so only a write deadline applies) and `Liveness::default`'s own
/// deadline, so the two never drift apart into two literals.
const DEFAULT_PEER_PATIENCE: Duration = Duration::from_secs(25);

impl Default for Liveness {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            deadline: DEFAULT_PEER_PATIENCE,
        }
    }
}

/// The out-of-process transport: the engine runs across a duplex stream, frames
/// crossing on a `WireChannel` as length-prefixed JSON.
///
/// Two constructors give it two lives. `WireTransport::new` — Unix only, since
/// all it does is hand a socketpair end to a child on fd 3 — spawns a same-host
/// engine child, whose death is a kernel-guaranteed EOF, so heartbeats would be
/// noise. [`WireTransport::adopt`] drives an *existing* stream, in production
/// the virtual socket into a guest VM, whose failure mode is silence rather
/// than EOF, and so runs a ticker.
///
/// A reader thread forwards `Event` frames into the `EventReceiver`, swallows
/// `Pong`s, and timestamps every frame it reads. All writes share one
/// `Mutex<WireChannel>`, the ticker's `Ping` included.
pub struct WireTransport {
    events_recv: EventReceiver,
    control: ControlSender,
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    /// `Some` under [`WireTransport::new`], whose child is killed and reaped on
    /// drop; `None` under [`WireTransport::adopt`], which does not own the far
    /// end. The one field belonging to a platform rather than the protocol.
    #[cfg(unix)]
    child: Option<ChildHandle>,
    /// Never joined: the thread exits when the channel closes. `Mutex` only
    /// for `Sync`.
    _reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set when the peer dies — a write error, a read EOF, or the ticker's
    /// deadline elapsing. Reader and ticker both check it to break early, and
    /// it is what [`WireTransport::dead`] reports.
    death: Arc<AtomicBool>,
    /// Shared with the published `ControlSender` so `cancel_current` names the
    /// dispatch it means.
    current_dispatch: Arc<std::sync::atomic::AtomicU64>,
    /// The instant of the last frame read — what the ticker measures silence
    /// against.
    last_seen: Arc<Mutex<Instant>>,
    /// The heartbeat an adopted stream still owes, parked by
    /// [`WireTransport::adopt`] and taken by the first [`Transport::attach`].
    /// `None` under [`WireTransport::new`], which never pings at all.
    pending_heartbeat: Mutex<Option<Liveness>>,
    /// Where the reader thread hands a `Frame::Session` batch. Shared with the
    /// reader so `set_deferred_sink` takes effect on the next batch without
    /// restarting anything; `None` drops a batch that arrives before a host
    /// installs one.
    deferred_sink: Arc<Mutex<Option<Arc<dyn DeferredSink>>>>,
    /// A shutdown-only duplicate of the socket, minted alongside `write_tx` by
    /// both constructors. `Drop` shuts this down directly rather than taking
    /// `write_tx`'s lock, so tearing the transport down never parks behind a
    /// write some other thread holds the lock over.
    shutdown: crate::wire::WireChannel,
}

/// The reader loop shared by both constructors. `last_seen` records *every*
/// frame read, not just `Pong`s — any frame is proof of life. On EOF, a read
/// error, or an already-set flag it sets `death` before exiting, so `dead()` is
/// honest under every teardown path and the dropped `event_tx` closes the
/// channel whose `recv` is what fails the in-flight dispatch.
///
/// `Frame::Event` goes onto the per-run channel; `Frame::Session` goes to
/// whichever sink `deferred_sink` holds at the moment it arrives, or is
/// dropped if none is installed.
fn spawn_wire_reader(
    mut reader_ch: crate::wire::WireChannel,
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    deferred_sink: Arc<Mutex<Option<Arc<dyn DeferredSink>>>>,
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
                    *last_seen.lock_ignore_poison() = Instant::now();
                    // A `Pong`'s whole job is the refresh above.
                    match frame {
                        Frame::Event(id, ev) => {
                            if event_tx.send((id, ev)).is_err() {
                                break;
                            }
                        }
                        Frame::Session(SessionEvent::DeferredSurface(batch)) => {
                            if let Some(sink) = deferred_sink.lock_ignore_poison().as_ref() {
                                sink.deliver(batch);
                            }
                        }
                        _ => {}
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
/// never at [`WireTransport::adopt`] — so no `Ping` precedes the handshake by
/// construction. On declaring death, from silence or a failed write, it shuts
/// the wire down too, waking the parked reader so the event channel closes.
/// Never joined.
///
/// The deadline arm never takes `write_tx`'s lock: declaring death from
/// silence must not itself be capable of parking behind a write some other
/// thread is stalled on, so `dead()==true` always implies `shutdown` runs in
/// bounded time. The `Ping` arm goes through [`write_through`], which severs
/// on failure the same way any other front-end write does.
fn spawn_heartbeat(
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    death: Arc<AtomicBool>,
    last_seen: Arc<Mutex<Instant>>,
    liveness: Liveness,
    shutdown: crate::wire::WireChannel,
) {
    std::thread::spawn(move || {
        let mut seq: u64 = 0;
        loop {
            std::thread::sleep(liveness.interval);
            if death.load(Ordering::SeqCst) {
                break;
            }
            if last_seen.lock_ignore_poison().elapsed() >= liveness.deadline {
                death.store(true, Ordering::SeqCst);
                shutdown.shutdown();
                break;
            }
            seq += 1;
            if write_through(&write_tx, &death, &Frame::Ping(seq)).is_err() {
                break;
            }
        }
    });
}

impl WireTransport {
    /// Spawn the engine child on one end of a fresh socketpair, inherited as
    /// fd 3, and start the reader on the other.
    ///
    /// No ticker: a same-host child's death is a kernel-guaranteed EOF, so
    /// heartbeats belong to [`WireTransport::adopt`].
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

        // A duplicate fd, so the reader can park in `read_frame` while the
        // front-end writes on the same socket.
        let writer = frontend.try_clone()?;
        writer.set_write_deadline(DEFAULT_PEER_PATIENCE)?;
        // A second duplicate, held back from ever taking the write lock, so
        // `Drop` can sever the connection without parking behind a write in
        // progress.
        let shutdown = writer.try_clone()?;

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
                // Only fd 3 may remain.
                libc::close(engine_fd);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // The parent must not hold the engine end open, or the child's exit
        // would never read as EOF.
        drop(engine);

        let death = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel();
        // No ticker here, but the shared reader keeps `last_seen` anyway.
        let last_seen = Arc::new(Mutex::new(Instant::now()));
        let deferred_sink = Arc::new(Mutex::new(None));
        let reader = spawn_wire_reader(
            frontend,
            event_tx,
            deferred_sink.clone(),
            death.clone(),
            last_seen.clone(),
        );

        let write_tx = Arc::new(Mutex::new(writer));
        let current_dispatch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let control =
            ControlSender::new_wire(write_tx.clone(), death.clone(), current_dispatch.clone());

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
            deferred_sink,
            shutdown,
        })
    }

    /// Drive the protocol over an existing duplex `stream` — no child spawn.
    ///
    /// The guest-VM path, and the one constructor both platforms have:
    /// `stream` is the virtual-socket connection to the engine in the guest —
    /// an `AF_VSOCK` descriptor under Virtualization.framework, an `AF_HYPERV`
    /// socket under Hyper-V — adopted through [`crate::wire::WireStream`],
    /// whose docs say why std's stream types can carry either. Such a stream
    /// can fall silent without ever tearing, so the first [`Transport::attach`]
    /// spawns a heartbeat ticker under `liveness`.
    ///
    /// # Errors
    /// Returns `Err` if the stream cannot be duplicated into separate read
    /// and write handles.
    pub fn adopt(
        stream: impl Into<crate::wire::WireStream>,
        liveness: Liveness,
    ) -> io::Result<Self> {
        let reader_ch = crate::wire::WireChannel::from_stream(stream);
        // A duplicate fd, so the reader can park in `read_frame` while the
        // ticker and the front-end write on the same socket.
        let writer = reader_ch.try_clone()?;
        writer.set_write_deadline(liveness.deadline)?;
        // A second duplicate, held back from ever taking the write lock, so
        // `Drop` can sever the connection without parking behind a write in
        // progress.
        let shutdown = writer.try_clone()?;

        let death = Arc::new(AtomicBool::new(false));
        let last_seen = Arc::new(Mutex::new(Instant::now()));
        let (event_tx, event_rx) = mpsc::channel();
        let deferred_sink = Arc::new(Mutex::new(None));
        let reader = spawn_wire_reader(
            reader_ch,
            event_tx,
            deferred_sink.clone(),
            death.clone(),
            last_seen.clone(),
        );

        let write_tx = Arc::new(Mutex::new(writer));
        let current_dispatch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let control =
            ControlSender::new_wire(write_tx.clone(), death.clone(), current_dispatch.clone());

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
            deferred_sink,
            shutdown,
        })
    }

    /// Whether the peer has been declared dead — a write error, a read EOF, or
    /// the heartbeat deadline. This is how a front-end tells *detached* from
    /// merely failed: no further frame will ever cross.
    pub fn dead(&self) -> bool {
        self.death.load(Ordering::SeqCst)
    }

    /// Any write error is fatal, `ECONNRESET` no less than `BrokenPipe`: a
    /// dropped frame means no `Report` will ever arrive, so the host's `recv`
    /// must learn of the death rather than block forever.
    fn write(&self, frame: &Frame) {
        let _ = write_through(&self.write_tx, &self.death, frame);
    }
}

impl Drop for WireTransport {
    fn drop(&mut self) {
        // `self.shutdown` never takes `write_tx`'s lock, so this wakes the
        // reader and (under `adopt`) the ticker — which share the one socket —
        // without ever parking behind a write in progress.
        self.death.store(true, Ordering::SeqCst);
        self.shutdown.shutdown();
        // An adopted stream owns no child — its far end is a guest VM the
        // session manager reaps — so only `new`'s child is reaped here.
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
                        Report::Ran {
                            ending: Ending::Settled { value, .. },
                            ..
                        } => Ok(value),
                        Report::Ran {
                            ending:
                                Ending::Raised { rendered, .. } | Ending::Walled { rendered, .. },
                            ..
                        } => Err(rendered),
                        Report::Ran { ending, .. } => {
                            Err(format!("probe answered abnormally: {ending:?}"))
                        }
                        Report::Static { diagnostics } => Err(match diagnostics {
                            Diagnostics::Host(msg) | Diagnostics::Parse(msg) => msg,
                            Diagnostics::Types(errs) => errs.join("\n"),
                        }),
                    };
                }
                // Anything that is not this probe's Report — another
                // dispatch's, a worker's batch settling mid-probe — is stashed
                // for the ordinary drain rather than dropped.
                Some(item) => self.events_recv.stash.lock_ignore_poison().push_back(item),
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

    /// The endpoint's lease does not survive encoding, so the engine attaches
    /// with no terminal of its own.
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
        // Only now, with the Attach through the write lock, may the heartbeat
        // start: no Ping may precede the handshake.
        let pending = self.pending_heartbeat.lock_ignore_poison().take();
        if let Some(liveness) = pending {
            spawn_heartbeat(
                self.write_tx.clone(),
                self.death.clone(),
                self.last_seen.clone(),
                liveness,
                self.shutdown
                    .try_clone()
                    .expect("dup an already-open socket"),
            );
        }
    }

    fn detach(&self) {
        self.write(&Frame::Detach);
    }

    fn answer(&self, eid: EnquiryId, answer: Result<FOValue, EnquiryError>) {
        self.write(&Frame::Answer(eid, answer));
    }

    fn set_deferred_sink(&self, sink: Arc<dyn DeferredSink>) {
        *self.deferred_sink.lock_ignore_poison() = Some(sink);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Enquiry desk tests ────────────────────────────────────────────────
//
// Core installs no builtin that enquires, so a run's body cannot reach
// `Shell::enquire` from ral source here. These drive it through a
// `RunLifecycle` handed `&mut Shell` mid-run instead — the same shape a real
// enquiring builtin runs under.
#[cfg(test)]
mod enquiry_tests {
    use super::*;
    use crate::run::{
        RequestedTerminalAccess, RunIo, RunLifecycle, RunReport, RunRequest, RunStdin,
    };
    use crate::types::{Capabilities, Desk, EnquiryDesk, Error, Mooring, Shell};
    use std::sync::Mutex;

    /// The minimal capturing request under the ⊤ ceiling, mirroring `run.rs`'s
    /// own `capture_req`.
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
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            lifecycle: Box::new(()),
        }
    }

    /// No desk installed answers the honest absence error, verbatim.
    #[test]
    fn absent_desk_answers_the_honest_error() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let err = shell
            .enquire(&Mooring::adrift(), FOValue::Unit)
            .expect_err("no desk is installed");
        assert_eq!(err.message, crate::types::NO_DESK);
    }

    /// A stub desk that maps `Int{n}` to `Int{n+1}`, otherwise echoes.
    struct IncrementDesk;
    impl EnquiryDesk for IncrementDesk {
        fn enquire(
            &self,
            req: FOValue,
            _cancel: &crate::process::CancelScope,
        ) -> Result<FOValue, Error> {
            match req {
                FOValue::Int { value } => Ok(FOValue::Int { value: value + 1 }),
                other => Ok(other),
            }
        }
    }

    /// Enquires from `pre_exec`, which runs with the run frame — and so its
    /// desk — already installed.
    #[derive(Clone)]
    struct AskDuringRun {
        req: FOValue,
        answer: std::sync::Arc<Mutex<Option<Result<FOValue, Error>>>>,
    }
    impl RunLifecycle for AskDuringRun {
        fn pre_exec(&mut self, mooring: &Mooring, shell: &mut Shell, _src: &str) {
            *self.answer.lock().unwrap() = Some(shell.enquire(mooring, self.req.clone()));
        }
    }

    /// The desk's transform reaches `enquire`'s caller, not just the desk.
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

    /// A handler reaching back into `shell_mut` would take the session lock its
    /// own stack already holds. Lacking a core builtin that enquires, the test
    /// sets `dispatch`'s thread stamp by hand and exercises the same guard.
    struct ReentrantShellMutDesk(std::sync::Arc<IdentityTransport>);
    impl EnquiryDesk for ReentrantShellMutDesk {
        fn enquire(
            &self,
            req: FOValue,
            _cancel: &crate::process::CancelScope,
        ) -> Result<FOValue, Error> {
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
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut mooring = Mooring::adrift();
        mooring.desk = Some(std::sync::Arc::new(ReentrantShellMutDesk(transport)) as Desk);
        let _ = shell.enquire(&mooring, FOValue::Unit);
    }

    /// The same guard on the other door.
    struct ReentrantDispatchDesk(std::sync::Arc<IdentityTransport>);
    impl EnquiryDesk for ReentrantDispatchDesk {
        fn enquire(
            &self,
            req: FOValue,
            _cancel: &crate::process::CancelScope,
        ) -> Result<FOValue, Error> {
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
                    trail: None,
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
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut mooring = Mooring::adrift();
        mooring.desk = Some(std::sync::Arc::new(ReentrantDispatchDesk(transport)) as Desk);
        let _ = shell.enquire(&mooring, FOValue::Unit);
    }

    /// The same `Arc` `set_desk` was given is the one `dispatch` would hand
    /// onto its `RunRequest`.
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

    /// Reverting `engine.fork` to `None` is what makes a later dispatch's
    /// `fork_into_nursery` answer absence rather than find a stale nursery.
    #[test]
    fn clear_fork_uninstalls_from_engine_inner() {
        use crate::types::{Fork, Nursery};

        let transport = IdentityTransport::new(Shell::new(crate::io::TerminalState::default()));
        transport.set_fork(Fork::Park(Nursery::default()));
        assert!(
            transport.shell_mut().fork.is_some(),
            "set_fork must install onto engine.fork"
        );

        transport.clear_fork();
        assert!(
            transport.shell_mut().fork.is_none(),
            "clear_fork must uninstall it, mirroring set_desk's retirement"
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
        _mooring: &crate::types::Mooring,
        _shell: &mut Shell,
    ) -> crate::types::Settled<crate::types::Value> {
        panic!("transport test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::typecheck::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], &[], thunk(pure(crate::typecheck::Ty::Unit)))
    }

    static PANIC_BUILTINS_ARR: [crate::types::BuiltinEntry; 1] = [crate::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("seam-panic-now"),
        scheme_panic_now,
        "test-only: panic the evaluator mid-run.",
        crate::types::BuiltinBody::Static(builtin_panic_now),
    )];
    static PANIC_BUILTINS: &[crate::types::BuiltinEntry] = &PANIC_BUILTINS_ARR;

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
            trail: None,
        }
    }

    /// A panicking run arrives as an ordinary `Report::Static{Host}` — the run
    /// door caught it and rolled the shell back — and the same transport
    /// dispatches the next run cleanly.
    #[test]
    fn panicking_dispatch_reports_and_the_session_survives() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(PANIC_BUILTINS);
        let transport = IdentityTransport::new(shell);

        let report = dispatch_to_report(
            &transport,
            run("seam-panic-now"),
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
            |_| {
                Err(EnquiryError {
                    message: "no desk".into(),
                    status: 1,
                })
            },
        )
        .expect("the next dispatch must still answer");
        match report {
            Report::Ran { ending, .. } => assert!(matches!(ending, Ending::Settled { .. })),
            Report::Static { .. } => panic!("the healed session must evaluate"),
        }
    }
}

// ── Probe tests ───────────────────────────────────────────────────────
//
// Identity-only. Engine-busy serialisation needs a second process racing the
// wire engine's rendezvous, which no unit test can stage.
#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::types::Shell;

    /// Nothing spawned yet.
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

    /// A fresh shell's ledger is unarmed, so nothing is leased.
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

    /// `` `none `` for a name never set, `` `some [value] `` once
    /// `Shell::set_env_var` installs one.
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

    /// The shell's *logical* cwd, not the process's.
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

    /// A fresh shell still carries the ambient root frame.
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

    /// `0` on a bare shell, a positive shallow estimate once anything is bound.
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

    /// An empty list, not an error, when there is nothing to report.
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

    /// An unrecognised class names itself in the error, never a silent default.
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

    /// No class to read at all is an error, not a panic.
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
// Rendering at the seam is what gives every front-end, the REPL included,
// batch-host parity: it prints the string verbatim and renders nothing itself.
#[cfg(test)]
mod runtime_error_seam_tests {
    use super::*;
    use crate::source::SourceDb;
    use crate::types::Error;

    #[test]
    fn runtime_error_projects_to_a_full_diagnostic_string() {
        let db = SourceDb::default();
        let err = Error::new("boom", 3).with_hint("try harder");
        // A single command whose error carries no span: the compact one-liner.
        let Ending::Raised { rendered, .. } =
            render_raise(&err, true, crate::source::FileId(0), &db, false)
        else {
            panic!("an engine runtime error must project to a wire Ending::Raised");
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
// These drive `WireTransport::adopt` over a `UnixStream::pair`, a peer
// `WireChannel` on the far end standing in for the guest engine, to prove the
// two halves of the liveness law: any received frame is proof of life, and only
// genuine silence past the deadline is death. Margins are deliberately loose,
// since the dev fleet includes a jittery VM — nothing here asserts a tight
// upper bound, only that death is or is not reached within seconds of slack.
#[cfg(all(test, unix))]
mod wire_liveness_tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// Several pings fall inside one deadline window, and both are far below
    /// the slack the assertions wait.
    fn brisk() -> Liveness {
        Liveness {
            interval: Duration::from_millis(25),
            deadline: Duration::from_millis(250),
        }
    }

    /// The ticker does not start until the attach is written, so any test
    /// expecting heartbeat traffic or deadline death must call this first.
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

    /// The version handshake the guest engine checks before it will speak.
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

    /// The attach is delayed past several ping intervals, so `Attach` arriving
    /// first proves the ordering is by construction, not by winning a race.
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

    /// The law's positive half: a ponging peer is alive well past the
    /// deadline, since received frames are proof of life.
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

    /// The law's negative half. A closed event stream is also the mechanism
    /// that fails an in-flight dispatch as cancelled.
    #[test]
    fn a_silent_peer_is_declared_dead_and_closes_the_event_stream() {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, brisk()).unwrap();
        attach(&transport);

        // `back` is held open but never speaks: no EOF, so death can come from
        // nothing but the heartbeat deadline.
        assert!(
            transport.events().recv().is_none(),
            "a silent peer past the deadline must close the event stream"
        );
        assert!(transport.dead(), "silence past the deadline is death");

        drop(back);
    }

    /// The reader sets the flag before dropping its sender, so `dead()` is
    /// honest the instant `recv` unblocks on a hangup.
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

    /// The control path holds the same law as the data path: a `Cancel` the
    /// wire will not take is death now, not once the reader gets round to an
    /// EOF.
    #[test]
    fn a_failed_control_write_marks_the_transport_dead() {
        let (front, back) = UnixStream::pair().unwrap();
        // Only the front's write half is shut, so a write fails while `back`,
        // held open and unattached, leaves the reader nothing to read and the
        // ticker unspawned: the control write is the sole possible cause.
        front.shutdown(std::net::Shutdown::Write).unwrap();
        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();

        transport.control().send(Control::Cancel(DispatchId(1)));

        assert!(
            transport.dead(),
            "a cancel that cannot be written is death, not silence"
        );
        drop(back);
    }

    /// A batch settling with no dispatch in flight — the shape a detached
    /// worker's completion takes between runs — reaches a sink installed
    /// through `set_deferred_sink`, and a dispatch that follows still
    /// round-trips to its own Report: `Frame::Session` routing on the reader
    /// does not disturb `Frame::Event`.
    #[test]
    fn session_frame_with_no_dispatch_reaches_the_installed_sink() {
        use crate::types::DeferredSink;

        struct RecordingSink {
            batches: Mutex<Vec<Vec<FOValue>>>,
        }
        impl DeferredSink for RecordingSink {
            fn deliver(&self, batch: Vec<FOValue>) {
                self.batches.lock().unwrap().push(batch);
            }
        }

        let (front, back) = UnixStream::pair().unwrap();
        let mut peer = crate::wire::WireChannel::from_stream(back);

        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
        attach(&transport);
        match peer
            .read_frame()
            .unwrap()
            .expect("the Attach handshake must cross first")
        {
            Frame::Attach { .. } => {}
            other => panic!("expected the Attach frame, got {other:?}"),
        }

        let sink = Arc::new(RecordingSink {
            batches: Mutex::new(Vec::new()),
        });
        transport.set_deferred_sink(sink.clone() as Arc<dyn DeferredSink>);

        peer.write_frame(&Frame::Session(SessionEvent::DeferredSurface(vec![
            FOValue::Int { value: 7 },
        ])))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !sink.batches.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the session batch never reached the installed sink"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            sink.batches.lock().unwrap()[0],
            vec![FOValue::Int { value: 7 }]
        );

        let id = DispatchId(1);
        transport.dispatch(
            id,
            Run {
                program: Program::Source("$[1 + 1]".into()),
                script_name: "<test>".into(),
                caps: crate::types::Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: crate::run::RunIo::Capture,
                terminal: crate::run::RequestedTerminalAccess::Denied,
                stdin: crate::run::RunStdin::Empty,
                trail: None,
            },
        );

        match peer
            .read_frame()
            .unwrap()
            .expect("the dispatch must cross the wire")
        {
            Frame::Dispatch(did, _) => assert_eq!(did, id),
            other => panic!("expected a Dispatch frame, got {other:?}"),
        }
        peer.write_frame(&Frame::Event(
            id,
            Event::Report(Report::Ran {
                ending: Ending::Settled {
                    value: FOValue::Int { value: 2 },
                    status: 0,
                },
                captured: None,
                trail: Vec::new(),
            }),
        ))
        .unwrap();

        match transport.events().recv() {
            Some((rid, Event::Report(_))) => {
                assert_eq!(rid, id, "the dispatch still round-trips to its own Report");
            }
            other => panic!("expected the dispatch's Report, got {other:?}"),
        }
    }
}
