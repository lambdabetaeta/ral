//! The engine protocol's algebra: the frames between a front-end and the ral
//! engine, and nothing that carries them.
//!
//! Attach/Ping/Pong open and keep alive one connection; Detach/Ended certify
//! teardown, while EOF initiates it. Dispatch carries one whole run, Event flows
//! engine to front-end, and Control flows front-end to engine. The carriers
//! that interpret the algebra are [`crate::carrier`]'s.
use crate::fact::{ErrorRecord, ExitCode as _};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU8;

use crate::data::FOValue;
use crate::error::Error;
use crate::io::{Captured, RunIo, RunStdin};
use crate::path::EnginePath;
use crate::process::{CancelCause, RequestedTerminalAccess, RootCause};
use crate::shell::runtime::CapturePolicy;
use probe::Probe;

pub mod channel;
pub mod probe;

/// The engine's contract with the host: frames and command confinement.
/// Checked at every `Attach`; a mismatch refuses even development media.
///
/// Packaging compares the recorded version in `vm-image/build-tools` (`check-media`).
pub const PROTOCOL_VERSION: u32 = 43;

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
    /// The variant's name, for a diagnostic about a frame out of place.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Attach(_) => "Attach",
            Self::Detach => "Detach",
            Self::Dispatch(..) => "Dispatch",
            Self::Probe(..) => "Probe",
            Self::Event(..) => "Event",
            Self::Session(..) => "Session",
            Self::Control(..) => "Control",
            Self::Answer(..) => "Answer",
            Self::Ping(_) => "Ping",
            Self::Pong(_) => "Pong",
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
/// its parent. A child's cwd and env ride its seal, so nothing else is named.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Origin {
    Root(Root),
    /// A child's parent, sealed.
    Child(crate::seed::Sealed),
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
    pub env: Vec<(String, String)>,
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
            env: Vec::new(),
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
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
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

    /// A child's attach: `engine`, then its parent's seal.
    pub fn child(engine: EngineSpec, sealed: crate::seed::Sealed) -> Self {
        Self {
            engine,
            origin: Origin::Child(sealed),
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
    /// policy, and carries it home on [`Report::Ran`]; `None` neither opens a
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

/// What a run runs: source text compiled and typechecked against the live
/// session, or a registered hook applied to first-order arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Program {
    Source(String),
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
    /// The answer to a [`Frame::Probe`]: the reading, or the engine's refusal in
    /// its own words. Its own event, not a run that ended: a probe has no
    /// ending, status or trail to forge.
    Reading(Result<FOValue, String>),
    /// The dispatch's sole terminal frame.
    Report(Report),
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

/// An [`Error`] as it crosses the transport, a desk's refusal or an engine's
/// refusal to attach: its message line, context included, and its hints,
/// nothing else.
///
/// Both carriers pass every refusal through [`WireError::of`] and back
/// through [`WireError::to_error`], so a refusal reads the same under
/// either. The enquiring builtin supplies the engine-side provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub message: String,
    pub hints: Vec<String>,
}

impl WireError {
    /// What of `error` the transport carries.
    #[must_use]
    pub fn of(error: &Error) -> Self {
        Self {
            message: error.to_string(),
            hints: error.hints().iter().map(ToString::to_string).collect(),
        }
    }

    /// The error this refusal was, on the far side.
    #[must_use]
    pub fn to_error(&self) -> Error {
        self.hints
            .iter()
            .fold(Error::new(self.message.as_str()), |error, hint| {
                error.with_hint(hint.as_str())
            })
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
    Static { rendered: String, status: NonZeroU8 },
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
pub type Ending = crate::fact::Ending<FOValue, ErrorRecord, CancelCause>;

impl Report {
    /// The process status a host exits on.
    pub fn code(&self) -> u8 {
        match self {
            Self::Static { status, .. } | Self::Faulted { status, .. } => status.get(),
            Self::Ran(ran) => ran.ending.code(),
        }
    }
}

#[cfg(test)]
mod control_wire_tests {
    use super::*;
    use strum::VariantArray as _;

    /// A session's end crosses the wire with its cause, every one of them.
    #[test]
    fn an_end_round_trips_its_cause() {
        for &cause in RootCause::VARIANTS {
            let control = Control::End(cause);
            let wire = serde_json::to_string(&control).expect("encode");
            let back: Control = serde_json::from_str(&wire).expect("decode");
            assert_eq!(back, control, "{wire}");
        }
    }
}

#[cfg(test)]
mod ending_wire_round_trip_tests {
    //! The Report round-trips over every `Ending` arm: whatever a wire engine
    //! encodes, a front-end decodes back byte-for-byte.

    use super::*;
    use crate::fact::Reason;

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
        let encoded = serde_json::to_string(report).expect("Report must encode");
        let decoded: Report = serde_json::from_str(&encoded).expect("Report must decode");
        assert_eq!(&decoded, report, "round trip must be lossless: {encoded}");
    }

    #[test]
    fn ok_round_trips() {
        round_trips(&ran(Ending::Ok(FOValue::Int { value: 1 })));
    }

    #[test]
    fn an_enquiry_refusal_carries_message_and_hints_and_becomes_a_plain_raise() {
        use crate::fact::FailureCode as _;

        let refused = Error::new("the desk refused")
            .with_context("enquire")
            .with_hint("try the foreground")
            .with_hint("or ask again");
        let refusal = WireError::of(&refused);
        assert_eq!(
            serde_json::to_string(&refusal).unwrap(),
            r#"{"message":"enquire: the desk refused","hints":["try the foreground","or ask again"]}"#
        );
        let frame = Frame::Answer(EnquiryId(7), Err(refusal));
        let encoded = serde_json::to_string(&frame).unwrap();
        let decoded: Frame = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, frame);
        let Frame::Answer(_, Err(refusal)) = decoded else {
            panic!("the refusal must survive the wire")
        };
        let error = refusal.to_error();
        assert_eq!(
            error.reason,
            Reason::Raised("enquire: the desk refused".into())
        );
        assert_eq!(error.hints(), refused.hints());
        assert_eq!(error.failure_code(), NonZeroU8::MIN);
    }

    #[test]
    fn err_round_trips() {
        let record = ErrorRecord::new(
            Reason::Raised("boom".into()),
            "cmd".into(),
            crate::test_helper::site(),
        );
        round_trips(&ran(Ending::Err(record)));
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
        });
    }

    #[test]
    fn faulted_round_trips() {
        round_trips(&Report::Faulted {
            rendered: "Error: boom\n".into(),
            status: nz(crate::fact::PANIC_CODE),
        });
    }
}
