//! The engine protocol's algebra: the frames between a front-end and the ral
//! engine, and nothing that carries them.
//!
//! Attach/Detach/Ping/Pong bracket and keep alive one connection (the
//! connection *is* the session); Dispatch carries one whole run, Event flows
//! engine to front-end, and Control flows front-end to engine. The carriers
//! that interpret the algebra are [`crate::carrier`]'s.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::first_order::FOValue;
use crate::io::{Captured, RunIo, RunStdin};
use crate::process::{Ambient, CancelCause, RequestedTerminalAccess};
use crate::types::{CapturePolicy, Error};
use probe::Probe;

pub mod channel;
pub mod probe;

/// The frame algebra's generation, checked at `Attach` and refused on
/// mismatch.  Public because a build has to compare it against the engine
/// sitting in the guest media beside it: see [`check_media`].
pub const PROTOCOL_VERSION: u32 = 14;

/// The key `vm-image/build-boot.sh` records [`PROTOCOL_VERSION`] under in the
/// boot media's manifest, `vm-image/out/boot/boot-manifest.txt`.
pub const MEDIA_KEY: &str = "proto_version";

/// Refuse to package a host around an engine it cannot talk to.
///
/// The `Attach` check is right and loud, but it is loud *in the guest*: the
/// host sees a socket close and can say only `engine-closed`.  This is the
/// same number where being wrong costs one failed build instead of an
/// installer that cannot start a conversation.  Same shape, same reasons and
/// same writer as `ral_daemon::boot::check_media` — which is why the two
/// live apart, each beside the constant it defends.
///
/// # Errors
/// If the media records no version, an unparseable one, or one this host has
/// outgrown.
pub fn check_media(manifest: &str, path: &str) -> Result<(), String> {
    const REMEDY: &str = "Rebuild the media from this checkout: `just guest-boot amd64` for the \
                          Windows guest, `just guest-boot` for the Mac's: and package again.";

    let recorded = manifest
        .lines()
        .filter_map(|line| line.trim().strip_prefix(MEDIA_KEY)?.strip_prefix('='))
        .next_back();

    let Some(recorded) = recorded else {
        return Err(format!(
            "the guest media described by {path} carries no `{MEDIA_KEY}=` line, so nothing here \
             can tell whether its engine speaks this host's protocol. One that does not refuses \
             the attach, and all the host learns is that a socket closed. {REMEDY}"
        ));
    };
    let recorded = recorded.trim();
    let recorded: u32 = recorded.parse().map_err(|err| {
        format!("{path} records `{MEDIA_KEY}={recorded}`, which is not a protocol version: {err}.")
    })?;

    if recorded != PROTOCOL_VERSION {
        return Err(format!(
            "the guest media's engine speaks protocol {recorded}, and this host speaks \
             {PROTOCOL_VERSION} ({path} records `{MEDIA_KEY}={recorded}`). That engine would \
             refuse this host's attach and close the connection, and the conversation would fail \
             to start with nothing but a closed socket to show for it. {REMEDY}"
        ));
    }
    Ok(())
}

/// The byte before the first frame, written by a guest that has just spawned
/// a child engine onto this connection and read by the host that dialled it.
///
/// The algebra offers no substitute: the host speaks first and `Attach` is the
/// only legal first frame, so this byte is the whole of the guest's readiness
/// signal — and without it a roster can name a child whose `spawn` has not yet
/// returned. ASCII ACK, and portable, because the two ends need not share an
/// operating system.
pub const HATCH_ACK: u8 = 0x06;

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
    /// Front-end drops: cancel in-flight dispatch, reap foreground
    /// subtree, restore terminal state.
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
    /// Engine → front-end, outside any dispatch's window: emitted by a
    /// detached worker that settles between runs, so it has no dispatch to
    /// ride and reaches the front-end through the transport's session sink
    /// instead of the per-run event stream.
    Session(SessionEvent),
    /// Front-end → engine. Out-of-band: deliverable while a Dispatch is
    /// outstanding.
    Control(Control),
    /// Front-end → engine: the dual of `Event::Enquiry`, correlated by the
    /// enquiry alone — `Parks::fill` keys its slots by `EnquiryId`, and the
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
    pub terminal: crate::terminal::TerminalState,
    /// The session's logical cwd.
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// Checked against `PROTOCOL_VERSION`; a mismatch refuses the attach.
    pub proto_version: u32,
    /// Tag naming the boot recipe the engine applies once attached —
    /// `"repl"`, `"exarch-agent"`. Each front-end binary re-execs *itself*
    /// with `--engine` and resolves the tag against its own compiled-in
    /// `EngineInstaller` table, so only the tag crosses, never the functions
    /// it names.
    pub installer: String,
    /// Per-session variables the host seeds into the engine's environment.
    pub env: Vec<(String, String)>,
    /// The installer's own settings: its recipe decodes them, and refuses
    /// ill-shaped ones in its own words. Core never reads them.
    pub config: FOValue,
}

impl Attach {
    /// An attach at this build's protocol version, seeding no variables and
    /// configuring nothing.
    pub fn new(installer: impl Into<String>, cwd: PathBuf, home: PathBuf) -> Self {
        Self {
            terminal: crate::terminal::TerminalState::default(),
            cwd,
            home,
            proto_version: PROTOCOL_VERSION,
            installer: installer.into(),
            env: Vec::new(),
            config: FOValue::Unit,
        }
    }

    #[must_use]
    pub fn with_config(self, config: FOValue) -> Self {
        Self { config, ..self }
    }
}

/// One whole run: the program to evaluate and the conditions it runs under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub program: Program,
    /// Label for the root source context: `"<stdin>"` for the REPL, `"<tool>"`
    /// for exarch.
    pub script_name: String,
    /// The capability ceiling pushed for the eval's dynamic extent, every
    /// layer of it: the stack is the meet, and folding it into one frame
    /// would lose what a layer says only about a resolved name.
    /// `GrantStack::root()` is ⊤, the identity on authority.
    pub caps: crate::capability::GrantStack,
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
    pub io: RunIo,
    pub terminal: RequestedTerminalAccess,
    pub stdin: RunStdin,
    /// `Some` delimits this dispatch's own trail, at the given capture
    /// policy, and carries it home on [`Report::Ran`]; `None` neither opens a
    /// scope nor collects one. exarch's tool calls ask with
    /// [`CapturePolicy::Off`]; the REPL never asks.
    pub trail: Option<CapturePolicy>,
}

impl Run {
    /// `program` under no authority beyond ⊤, its streams captured, the
    /// terminal denied and stdin empty: exarch's tool runs, and most tests.
    pub fn captured(program: impl Into<Program>, script_name: impl Into<String>) -> Self {
        Self {
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            ..Self::foreground(program, script_name)
        }
    }

    /// `program` under no authority beyond ⊤, on the inherited streams and the
    /// leased terminal: the REPL and batch.
    pub fn foreground(program: impl Into<Program>, script_name: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            script_name: script_name.into(),
            caps: crate::capability::GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        }
    }
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
/// and a detached worker's batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// The engine booted the installer `Attach` named and takes dispatches.
    Attached,
    /// The engine refuses to attach, in its own words, and exits.
    Refused(String),
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

impl From<EnquiryError> for Error {
    fn from(refusal: EnquiryError) -> Self {
        Self::raised(refusal.message, refusal.status)
    }
}

/// Front-end → engine out-of-band control frame: one meaning under either
/// carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Unwind whatever dispatch is in flight, as a Ctrl-C would; none in
    /// flight, nothing happens.
    Interrupt,
    /// Unwind this dispatch, even one not yet arrived; a stale id strikes
    /// nothing.
    Cancel(DispatchId),
    /// Cancel the session's durable root: every run and every detached
    /// worker, for good.
    Terminate,
    /// [`Terminate`](Self::Terminate), as Ctrl-`\` asks it: cause `RootAbort`.
    Abort,
}

impl Control {
    /// The verb an ambient cause asks of the session it reaches.
    pub(crate) fn hearing(ambient: Ambient) -> Self {
        match ambient {
            Ambient::Interrupt => Self::Interrupt,
            Ambient::Root(CancelCause::Aborted) => Self::Abort,
            Ambient::Root(_) => Self::Terminate,
        }
    }
}

// ── The terminal frame ────────────────────────────────────────────────

/// The run's terminal frame: the protocol projection of the engine's
/// [`RunReport`](crate::run::RunReport), produced by
/// [`RunReport::into_report`](crate::run::RunReport::into_report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Report {
    /// Parse/type/host failure: the run never reached evaluation. `rendered`
    /// is the whole report — prefix, code, caret, hint — so the host prints it
    /// and exits on `status`.
    Static { rendered: String, status: i32 },
    /// The run ran to a settled result.
    Ran {
        ending: Ending,
        /// `Some` under [`RunIo::Capture`].
        captured: Option<Captured>,
        /// The dispatch's own trail, projected; empty unless [`Run::trail`]
        /// asked. Unbounded by declaration — the wire's frame fuse is the
        /// shared backstop, exactly as it is for `captured`.
        trail: Vec<FOValue>,
    },
}

/// The status of an ending that *failed*: never zero, so an error can never
/// be reported with the code that means success.
///
/// The clamp lives in the type rather than at any one site: `From<i32>` is
/// the only way in, the wire form is the plain integer, and `serde` routes a
/// decoded status back through the same door, so no peer can smuggle a
/// success code into a failure either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "i32", into = "i32")]
pub struct FailureStatus(i32);

impl From<i32> for FailureStatus {
    fn from(status: i32) -> Self {
        Self(status.clamp(1, 255))
    }
}

impl From<FailureStatus> for i32 {
    fn from(status: FailureStatus) -> Self {
        status.0
    }
}

impl FailureStatus {
    /// The exit code a host reports.
    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }
}

/// How a run left evaluation, wire-shaped.
///
/// The protocol projection of the engine's [`Ending`](crate::run::Ending),
/// rendered against a [`SourceDb`](crate::source::SourceDb) — a live `Error`
/// cannot cross the protocol, so [`Self::Raised`]/[`Self::Walled`] carry the
/// string it rendered to, and the record `try` would hand its handler.
/// `status` is carried explicitly wherever it
/// is not the whole of the arm's payload, since a renderer that has already
/// discarded the engine `Error` for its `rendered` string has no other way
/// back to it; on the two failing arms it is a [`FailureStatus`], which has
/// no success code to carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Ending {
    Settled {
        value: FOValue,
        status: i32,
    },
    /// `command_exit` says whether the status was an external command's
    /// non-zero exit rather than a raised error — the one classification a
    /// host's didactics need from a `Status` that never crosses the protocol.
    /// `single_command` picks between the exit-code remedy's two wordings.
    Raised {
        rendered: String,
        record: FOValue,
        command_exit: bool,
        single_command: bool,
        status: FailureStatus,
    },
    /// A raise the wall itself caused. No `command_exit`/`single_command`:
    /// the timeout remedy is unconditional, so no renderer reads them here.
    Walled {
        rendered: String,
        record: FOValue,
        status: FailureStatus,
    },
    /// The run settled, but on a value that is not data — a handle, a block,
    /// a function — which the protocol cannot carry. Nothing failed, yet the
    /// value is lost, so it ends as a failure of its own kind, with its own
    /// record.
    Unreturnable {
        rendered: String,
        record: FOValue,
    },
    Exited(i32),
}

impl Ending {
    /// The flat status view: the `status` an arm carries, or the whole of
    /// [`Self::Exited`]'s payload.
    #[must_use]
    pub fn status(&self) -> i32 {
        match self {
            Self::Settled { status, .. } | Self::Exited(status) => *status,
            Self::Raised { status, .. } | Self::Walled { status, .. } => status.get(),
            Self::Unreturnable { .. } => 1,
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
            record: FOValue::Unit,
            command_exit: true,
            single_command: false,
            status: 7.into(),
        }));
    }

    #[test]
    fn walled_round_trips() {
        round_trips(&ran(Ending::Walled {
            rendered: "error: timed out\n".into(),
            record: FOValue::Unit,
            status: 124.into(),
        }));
    }

    #[test]
    fn exited_round_trips() {
        round_trips(&ran(Ending::Exited(3)));
    }

    /// A failure never reports success, however it was built and whatever a
    /// peer sends.
    #[test]
    fn a_failing_ending_never_carries_a_success_status() {
        let raised = Ending::Raised {
            rendered: "error: boom\n".into(),
            record: FOValue::Unit,
            command_exit: false,
            single_command: false,
            status: 0.into(),
        };
        assert_eq!(raised.status(), 1, "a raise reported success");
        round_trips(&ran(raised));

        let smuggled: Ending = serde_json::from_str(
            r#"{"Walled":{"rendered":"error: timed out\n","record":"unit","status":0}}"#,
        )
        .expect("a Walled ending must decode");
        assert_eq!(smuggled.status(), 1, "a decoded raise reported success");
    }

    #[test]
    fn static_round_trips() {
        round_trips(&Report::Static {
            rendered: "Error: boom\n".into(),
            status: 1,
        });
    }
}

/// The build-time half of the `Attach` check: what stops a stale engine
/// reaching a user at all.
#[cfg(test)]
mod media_protocol {
    use super::{MEDIA_KEY, PROTOCOL_VERSION, check_media};

    /// A manifest as `build-boot.sh` writes it: the line is found by its key
    /// among the others, and media that agrees is packaged in silence.
    #[test]
    fn media_speaking_this_protocol_is_fit_to_package() {
        let manifest = format!(
            "arch=amd64\nboot_contract=2\n{MEDIA_KEY}={PROTOCOL_VERSION}\n\
             rust_target=x86_64-unknown-linux-musl\n"
        );
        check_media(&manifest, "out/boot/boot-manifest.txt").expect("matching media must pass");
    }

    /// The skew this exists for — the one that shipped a synod whose engine
    /// spoke 4 to a host speaking 7. Both numbers and the manifest, or a
    /// reader cannot tell which side is old.
    #[test]
    fn an_engine_behind_the_host_names_both_numbers() {
        let stale = format!("arch=amd64\n{MEDIA_KEY}={}\n", PROTOCOL_VERSION - 3);
        let err = check_media(&stale, "vm-image/out/boot/boot-manifest.txt")
            .expect_err("media from an older protocol must not be packaged");
        assert!(
            err.contains(&format!("protocol {}", PROTOCOL_VERSION - 3)),
            "{err}"
        );
        assert!(err.contains(&PROTOCOL_VERSION.to_string()), "{err}");
        assert!(err.contains("vm-image/out/boot/boot-manifest.txt"), "{err}");
        assert!(err.contains("just guest-boot amd64"), "{err}");
    }

    /// Media older than this mechanism records nothing, and says so rather
    /// than blaming a version it cannot know.
    #[test]
    fn media_predating_the_line_gets_its_own_sentence() {
        let err = check_media(
            "arch=amd64\nral_git_hash=7c1c8ae6\n",
            "out/boot-manifest.txt",
        )
        .expect_err("a manifest with no protocol line must not be packaged");
        assert!(err.contains(&format!("no `{MEDIA_KEY}=` line")), "{err}");
        assert!(err.contains("just guest-boot"), "{err}");
    }

    /// A broken manifest is refused as one, never silently read as zero.
    #[test]
    fn a_line_that_is_not_a_number_is_refused_as_such() {
        let err = check_media(&format!("{MEDIA_KEY}=seven\n"), "m.txt")
            .expect_err("an unparsable protocol version must be refused");
        assert!(err.contains("not a protocol version"), "{err}");
    }
}
