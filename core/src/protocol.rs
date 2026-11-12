//! The engine protocol's algebra: the frames between a front-end and the ral
//! engine, and nothing that carries them.
//!
//! Attach/Ping/Pong open and keep alive one connection; Detach/Ended certify
//! teardown, while EOF initiates it. Dispatch carries one whole run, Event flows
//! engine to front-end, and Control flows front-end to engine. The carriers
//! that interpret the algebra are [`crate::carrier`]'s.
use crate::fact::{ErrorFact, ExitCode as _, FrameKind, ProbeFailure, Reason};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU8;

use crate::data::FOValue;
use crate::environ::EnvVars;
use crate::error::Error;
use crate::io::{Captured, RunIo, RunStdin};
use crate::path::EnginePath;
use crate::process::{CancelCause, RequestedTerminalAccess, RootCause};
use crate::shell::runtime::CapturePolicy;
use probe::Probe;

pub mod channel;
pub(crate) mod json;
pub mod probe;

/// The engine's contract with the host: frames and command confinement.
/// Checked at every `Attach`; a mismatch refuses even development media.
///
/// Packaging compares the recorded version in `vm-image/build-tools` (`check-media`).
pub const PROTOCOL_VERSION: u32 = 47;

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

/// One frame that crosses the engine protocol in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// The only legal first frame: an engine speaks no shell until told a
    /// version and an installer.
    Attach(Attach),
    /// Front-end → engine: close the session after teardown; answered by `Ended`.
    Detach,
    /// One whole run.  Boxed so no other variant is sized to `Run`.
    Dispatch(DispatchId, Box<Run>),
    /// A pure, boundary-time read of session state — no wall, no sinks, no
    /// clock, absent by type — hence a `Frame` and not a `Run`. It rides the
    /// engine's worker rendezvous alongside dispatches, so a probe sent mid-run
    /// gets the same "engine busy" answer a second dispatch would, and is
    /// answered by an [`Event::Reading`] under the same `DispatchId`.
    Probe(DispatchId, Probe),
    /// Engine → front-end, inside a dispatch's claim-to-Report window. May
    /// arrive while that Dispatch is outstanding.
    Event(DispatchId, Event),
    /// Engine → front-end lifecycle verdicts and deferred worker batches,
    /// outside any dispatch's window.
    Session(SessionEvent),
    /// Front-end → engine. Out-of-band: deliverable while a Dispatch is
    /// outstanding.
    Control(Control),
    /// Front-end → engine: the dual of `Event::Enquiry`, correlated by the
    /// enquiry alone — `Parks::fill` keys its slots by `EnquiryId`, and the
    /// dispatch it belongs to is implicit in which enquiry is outstanding.
    Answer(EnquiryId, Result<FOValue, WireError>),
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

impl Frame {
    /// Portable identity for a frame out of place.
    pub(crate) fn kind(&self) -> FrameKind {
        match self {
            Self::Attach(_) => FrameKind::Attach,
            Self::Detach => FrameKind::Detach,
            Self::Dispatch(..) => FrameKind::Dispatch,
            Self::Probe(..) => FrameKind::Probe,
            Self::Event(..) => FrameKind::Event,
            Self::Session(..) => FrameKind::Session,
            Self::Control(..) => FrameKind::Control,
            Self::Answer(..) => FrameKind::Answer,
            Self::Ping(_) => FrameKind::Ping,
            Self::Pong(_) => FrameKind::Pong,
        }
    }
}

/// What every engine is born from, under either carrier: `Frame::Attach`'s
/// payload, and `IdentityTransport::boot`'s argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attach {
    pub engine: EngineSpec,
    pub origin: Origin,
}

/// What names the engine itself, root or child: the binary half of its origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSpec {
    /// Checked against `PROTOCOL_VERSION`; a mismatch refuses the attach.
    pub proto_version: u32,
    /// Tag naming the boot recipe the engine applies once attached —
    /// `"repl"`, `"exarch-agent"`. Each front-end binary re-execs *itself*
    /// with `--engine` and resolves the tag against its own compiled-in
    /// `EngineInstaller` table, so only the tag crosses, never the functions
    /// it names.
    pub installer: String,
    pub terminal: crate::io::terminal::TerminalState,
    /// The engine's `HOME`, on the machine it runs on.
    pub home: EnginePath,
}

/// How the engine's session comes to be: a root from a recipe, a child from
/// its parent. A child's cwd and env are marshalled, so nothing else is named.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Origin {
    Root(Root),
    /// A child's parent, marshalled.
    Child(crate::marshal::Marshalled),
}

/// What only a root is told: the session it starts from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Root {
    /// The session's logical cwd, on the machine the engine runs on.
    pub cwd: EnginePath,
    /// Layers the engine pushes once its recipe has run, oldest first, as permanent
    /// session layers: resolved on the engine's host, so they mean nothing on another.
    /// Empty for none.
    pub grants: crate::authority::policy::GrantStack,
    /// Per-session variables the host seeds into the engine's environment.
    pub env: EnvVars,
    /// The installer's own settings: its recipe decodes them, and refuses
    /// ill-shaped ones in its own words. Core never reads them.
    pub config: FOValue,
}

impl EngineSpec {
    /// A spec at this build's protocol version, on the default terminal.
    pub fn new(installer: impl Into<String>, home: EnginePath) -> Self {
        Self {
            proto_version: PROTOCOL_VERSION,
            installer: installer.into(),
            terminal: crate::io::terminal::TerminalState::default(),
            home,
        }
    }
}

impl Root {
    /// A root at `cwd`, seeding no variables and configuring nothing.
    pub fn new(cwd: EnginePath) -> Self {
        Self {
            cwd,
            grants: crate::authority::policy::GrantStack::default(),
            env: EnvVars::new(),
            config: FOValue::Unit,
        }
    }

    /// Install these layers permanently after the root recipe.
    #[must_use]
    pub fn with_grants(mut self, grants: crate::authority::policy::GrantStack) -> Self {
        self.grants = grants;
        self
    }

    /// Seed these variables into the session's environment.
    #[must_use]
    pub fn with_env(mut self, env: EnvVars) -> Self {
        self.env = env;
        self
    }

    /// Hand the installer these settings.
    #[must_use]
    pub fn with_config(mut self, config: FOValue) -> Self {
        self.config = config;
        self
    }
}

impl Attach {
    /// A root's attach, seeding no variables and configuring nothing.
    pub fn new(installer: impl Into<String>, cwd: EnginePath, home: EnginePath) -> Self {
        Self {
            engine: EngineSpec::new(installer, home),
            origin: Origin::Root(Root::new(cwd)),
        }
    }

    /// A root's attach: `engine`, then the session it starts from.
    pub fn root(engine: EngineSpec, root: Root) -> Self {
        Self {
            engine,
            origin: Origin::Root(root),
        }
    }

    /// A child's attach: `engine`, then its parent, marshalled.
    pub fn child(engine: EngineSpec, parent: crate::marshal::Marshalled) -> Self {
        Self {
            engine,
            origin: Origin::Child(parent),
        }
    }

    #[must_use]
    pub fn with_terminal(mut self, terminal: crate::io::terminal::TerminalState) -> Self {
        self.engine.terminal = terminal;
        self
    }
}

/// One whole run: the program to evaluate and the conditions it runs under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub program: Program,
    /// Where the program's text came from: the script file batch runs, or a
    /// virtual `<stdin>` for the REPL, `<tool>` for exarch.
    pub origin: crate::source::SourceOrigin,
    /// `Some(d)` arms a deadline cancel on the run's foreground scope `d`
    /// after it starts; `None` leaves the run uncapped.
    pub wall: Option<std::time::Duration>,
    /// What a `guard` cleanup gets, from the strike, before the stop ends
    /// it: the host's policy, as `wall` is.
    pub cleanup_grace: std::time::Duration,
    /// Enforced at the spawn door: `Some(cap)` refuses a spawn while `cap`
    /// workers still run. Settled entries lingering under
    /// retention never block admission.
    pub worker_cap: Option<usize>,
    /// Whether a `spawn` must name its work, checked at the spawn step.
    pub worker_title: crate::shell::runtime::TitlePolicy,
    pub io: RunIo,
    pub terminal: RequestedTerminalAccess,
    pub stdin: RunStdin,
    /// `Some` delimits this dispatch's own trail, at the given capture
    /// policy, and carries it home on the report; `None` neither opens a
    /// scope nor collects one. exarch's tool calls ask with
    /// [`CapturePolicy::Off`]; the REPL never asks.
    pub trail: Option<CapturePolicy>,
    /// On return, wait for the session's loose workers and replay their bytes,
    /// dropping their endings. Batch sets this on its script.
    pub last: bool,
}

impl Run {
    /// `program`, its streams captured, the terminal denied and stdin empty:
    /// exarch's tool runs, and most tests.
    pub fn captured(program: impl Into<Program>, origin: crate::source::SourceOrigin) -> Self {
        Self {
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            ..Self::foreground(program, origin)
        }
    }

    /// `program`, on the inherited streams and the leased terminal: the REPL
    /// and batch.
    pub fn foreground(program: impl Into<Program>, origin: crate::source::SourceOrigin) -> Self {
        Self {
            program: program.into(),
            origin,
            wall: None,
            cleanup_grace: crate::process::DEFAULT_CLEANUP_GRACE,
            worker_cap: None,
            worker_title: crate::shell::runtime::TitlePolicy::Optional,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
            last: false,
        }
    }
}

/// Source text to run or check against the live session, or a registered
/// hook applied to first-order arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Program {
    Source(String),
    /// Compile as a source run, but do not evaluate or renew binding leases.
    Check(String),
    Hook {
        name: crate::shell::runtime::HookName,
        args: Vec<FOValue>,
    },
}

impl From<&str> for Program {
    fn from(source: &str) -> Self {
        Self::Source(source.to_owned())
    }
}

impl From<String> for Program {
    fn from(source: String) -> Self {
        Self::Source(source)
    }
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
    /// The answer to a [`Frame::Probe`]: the reading, or its typed refusal.
    /// Its own event, not a run that ended: a probe has no ending, status
    /// or trail to forge.
    Reading(Result<FOValue, ProbeFailure>),
    /// The dispatch's sole terminal frame.
    Report(Report),
}

// Portable probe refusals use the same Datum convention as error records.
impl Serialize for ProbeFailure {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::data::datum::as_datum::serialize(self, serializer)
    }
}
impl<'de> Deserialize<'de> for ProbeFailure {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::data::datum::as_datum::deserialize(deserializer)
    }
}

/// Engine → front-end traffic with no dispatch to ride: the attach verdict,
/// teardown acknowledgment, and a worker's batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// The engine booted the installer `Attach` named and takes dispatches.
    Attached,
    /// The engine refuses to attach, in its own words, and exits.
    Refused(WireError),
    /// The engine and every session worker have torn down.
    Ended,
    /// A deferred worker's surface batch, delivered when it settles and
    /// rendered by the host at the next run boundary.
    DeferredSurface(Vec<FOValue>),
}

/// A refusal's portable reason and ordered report facts, without host provenance.
/// Both carriers normalize through this envelope; the enquiring operation
/// claims its own engine-side provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    #[serde(with = "crate::data::datum::as_datum")]
    pub reason: Reason,
    pub context: Option<String>,
    pub hints: Vec<String>,
}

impl WireError {
    #[must_use]
    pub fn of(error: &Error) -> Self {
        Self {
            reason: error.reason.clone(),
            context: error.context().map(str::to_owned),
            hints: error.hints().iter().map(ToString::to_string).collect(),
        }
    }

    #[must_use]
    pub fn to_error(&self) -> Error {
        let error = Error::new(self.reason.clone()).with_hints(self.hints.iter().cloned());
        match &self.context {
            Some(context) => error.with_context(context),
            None => error,
        }
    }

    /// The message line for presentation, never failure identity.
    #[must_use]
    pub fn message(&self) -> String {
        self.to_error().to_string()
    }
}

/// Front-end → engine out-of-band control frame: one meaning under either
/// carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Strike only this dispatch with its cause, even one not yet arrived;
    /// a stale id strikes nothing.
    Strike(DispatchId, CancelCause),
    /// Cancel the session's durable root with its cause: every run and
    /// every worker, for good.
    End(RootCause),
}

// ── The terminal frame ────────────────────────────────────────────────

/// The run's terminal frame: the protocol projection of the engine's
/// [`RunReport`](crate::shell::run::RunReport), produced by
/// [`RunReport::into_report`](crate::shell::run::RunReport::into_report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Report {
    /// Parse/type/host refusal: the run never reached evaluation, so no
    /// command ran and nothing was bound. `rendered` is the whole report —
    /// prefix, code, caret, hint — so the host prints it and exits on `status`.
    Static {
        rendered: String,
        status: NonZeroU8,
        trail: Vec<FOValue>,
    },
    /// The source checked successfully, without evaluation.
    Checked { trail: Vec<FOValue> },
    /// The run ran to a settled result.
    Ran(Ran),
    /// The engine could not report the run: a panic in the
    /// report plumbing, or a report the frame fuse would not pass. What the run
    /// did is unknown (it may have run in part or whole), so a host must read it
    /// as neither a refusal nor an ending. `rendered` is the whole one-liner;
    /// the host prints it and exits on `status`.
    Faulted { rendered: String, status: NonZeroU8 },
}

/// A run that ran to a settled result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ran {
    pub ending: Ending,
    /// The ending's whole diagnostic; empty for values, exits and reader-gone stops.
    pub rendered: String,
    /// Whether the program was one command: compact drawing and exit-tip wording.
    pub single_command: bool,
    /// `Some` under [`RunIo::Capture`].
    pub captured: Option<Captured>,
    /// The dispatch's own trail, projected; empty unless [`Run::trail`]
    /// asked. Unbounded by declaration — the wire's frame fuse is the
    /// shared backstop, exactly as it is for `captured`.
    pub trail: Vec<FOValue>,
}

/// How a run ended, projected onto the wire.
pub type Ending = crate::fact::Ending<FOValue, ErrorFact, CancelCause>;

impl Report {
    /// The process status a host exits on.
    pub fn code(&self) -> u8 {
        match self {
            Self::Static { status, .. } | Self::Faulted { status, .. } => status.get(),
            Self::Checked { .. } => 0,
            Self::Ran(ran) => ran.ending.code(),
        }
    }
}

#[cfg(test)]
mod ending_wire_round_trip_tests {
    //! The Report round-trips over every `Ending` arm: whatever a wire engine
    //! encodes, a front-end decodes back byte-for-byte.

    use super::*;

    fn nz(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).unwrap()
    }

    fn ran(ending: Ending) -> Report {
        Report::Ran(Ran {
            ending,
            rendered: String::new(),
            single_command: false,
            captured: None,
            trail: Vec::new(),
        })
    }

    fn round_trips(report: &Report) {
        let encoded = crate::data::json::to_vec(report).expect("Report must encode");
        let decoded: Report = crate::data::json::from_slice(&encoded).expect("Report must decode");
        assert_eq!(&decoded, report);
    }

    #[test]
    fn stopped_round_trips() {
        let ending = Ending::Stopped(CancelCause::TimedOut);
        assert_eq!(
            serde_json::to_string(&ending).unwrap(),
            r#"{"stopped":"timed-out"}"#
        );
        round_trips(&ran(ending));
    }

    #[test]
    fn exited_round_trips() {
        round_trips(&ran(Ending::Exited(3)));
    }

    #[test]
    fn panic_round_trips() {
        round_trips(&ran(Ending::Panic("boom".into())));
    }

    #[test]
    fn static_round_trips() {
        round_trips(&Report::Static {
            rendered: "Error: boom\n".into(),
            status: NonZeroU8::MIN,
            trail: vec![FOValue::Int { value: 3 }],
        });
    }

    #[test]
    fn checked_round_trips_and_exits_zero() {
        let report = Report::Checked {
            trail: vec![FOValue::Int { value: 3 }],
        };
        assert_eq!(report.code(), 0);
        round_trips(&report);
    }

    #[test]
    fn check_program_round_trips() {
        let program = Program::Check("return 1".into());
        let encoded = crate::data::json::to_vec(&program).expect("Program must encode");
        let decoded: Program =
            crate::data::json::from_slice(&encoded).expect("Program must decode");
        assert_eq!(decoded, program);
    }

    #[test]
    fn faulted_round_trips() {
        round_trips(&Report::Faulted {
            rendered: "Error: boom\n".into(),
            status: nz(crate::fact::PANIC_CODE),
        });
    }
}
