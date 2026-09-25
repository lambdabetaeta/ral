//! The engine protocol: the frame algebra between a front-end and the ral
//! engine, and the two transports that carry it.
//!
//! Attach/Detach/Ping/Pong bracket and keep alive one connection — the
//! connection *is* the session — while Dispatch carries one whole run, Event
//! flows engine→front-end, and Control flows front-end→engine.
//!
//! Two transports realise the algebra over one [`Engine`]. `IdentityTransport`
//! calls it in one address space: frames move, never encode. `WireTransport`
//! encodes the same frames as length-prefixed JSON over a socket, answered by
//! the engine process in `engine/wire.rs`.
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::engine::{Engine, EngineInstaller, Rails, Scopes};
use crate::serial::{FOValue, NotData, Opaque};
use crate::spawn_grant::SpawnGrant;
use crate::sync::{CondvarExt as _, LockExt};
use crate::types::{CapturePolicy, DeferredSink, Fork, Nursery, NurseryId, Observation, Shell};
use std::sync::OnceLock;

pub mod reading;

/// The frame algebra's generation, checked at `Attach` and refused on
/// mismatch.  Public because a build has to compare it against the engine
/// sitting in the guest media beside it: see [`check_media`].
pub const PROTOCOL_VERSION: u32 = 10;

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
    const REMEDY: &str = "Rebuild the media from this checkout — `just guest-boot amd64` for the \
                          Windows guest, `just guest-boot` for the Mac's — and package again.";

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
    /// answered on the `Event::Report` rail under the same `DispatchId`. The
    /// `FOValue` is a `Variant` naming a [`reading`] class.
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
    pub terminal: crate::io::TerminalState,
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
            terminal: crate::io::TerminalState::default(),
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
    pub caps: crate::types::GrantStack,
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
    pub(crate) fn hearing(ambient: crate::process::Ambient) -> Self {
        use crate::process::{Ambient, CancelCause};
        match ambient {
            Ambient::Interrupt => Self::Interrupt,
            Ambient::Root(CancelCause::RootAbort) => Self::Abort,
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
        /// `Some` under [`RunIo::Capture`](crate::run::RunIo).
        captured: Option<crate::run::Captured>,
        /// The dispatch's own trail, projected; empty unless [`Run::trail`]
        /// asked. Unbounded by declaration — the wire's frame fuse is the
        /// shared backstop, exactly as it is for `captured`.
        trail: Vec<FOValue>,
    },
}

impl Report {
    /// A refusal the engine raises on its own behalf — a panicked worker, a
    /// busy engine — rendered through the same door as a run's own static
    /// failure, so a host never has to know which of the two it is printing.
    #[cfg(unix)]
    pub(crate) fn host_fault(message: impl Into<String>) -> Self {
        let diagnostics = crate::run::StaticDiagnostics::Host(crate::types::Error::new(message, 1));
        let (rendered, status) = crate::diagnostic::format_static_diagnostics(&diagnostics);
        Self::Static { rendered, status }
    }
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

/// A settled result that cannot cross, said with what the author likely
/// meant.
fn unreturnable(message: String, hint: &str, shell: &Shell) -> Ending {
    let mut error = crate::types::Error::new(message, 1);
    error.hint = Some(hint.into());
    Ending::Unreturnable {
        rendered: crate::diagnostic::format_runtime_error_compact(&error),
        record: record_of(&error, shell),
    }
}

/// The record `try` hands its handler for `error`.
fn record_of(error: &crate::types::Error, shell: &Shell) -> FOValue {
    FOValue::try_from(&crate::types::error_record_of(error, shell))
        .expect("an error record is data")
}

fn not_data(NotData { leaf, nested }: NotData, shell: &Shell) -> Ending {
    let (verb, hint) = match (leaf, nested) {
        (Opaque::Handle, false) => (
            "is",
            "did you mean to keep it? bind it — `let h = defer { … }` — and `await $h` \
             when you need its value",
        ),
        (Opaque::Block, false) => ("is", "did you mean to run it? force it with `!{ … }`"),
        (Opaque::Function, false) => ("is", "did you mean to apply it? pass it its arguments"),
        (Opaque::Handle, true) => (
            "holds",
            "bind the handle with `let h = …`, and return only data",
        ),
        (Opaque::Block | Opaque::Function, true) => (
            "holds",
            "return only data: force a block with `!{ … }`, or bind it with `let`",
        ),
    };
    unreturnable(
        format!("the result {verb} {leaf}, and a run can return only data"),
        hint,
        shell,
    )
}

/// Project an engine [`Ending`](crate::run::Ending) onto the wire, rendering
/// a caught runtime error against `shell` — the one lossy step between the
/// engine and the protocol: the live `Error` renders to the string the host
/// prints verbatim, and to its record.
fn render_ending(ending: crate::run::Ending, shell: &Shell) -> Ending {
    use crate::run::Ending as Raw;
    match ending {
        // A top-level result is not an `Observation` — it has no placeholder
        // vocabulary of its own — so a value the wire cannot carry is
        // reported, never silently dropped.
        Raw::Settled { value, status } => match FOValue::try_from(&value) {
            Ok(fo) => Ending::Settled {
                value: fo,
                status: status.clamp(0, 255),
            },
            Err(found) => not_data(found, shell),
        },
        Raw::Raised {
            error,
            single_command,
            root,
        } => render_raise(&error, single_command, root, shell, false),
        Raw::Walled {
            error,
            single_command,
            root,
        } => render_raise(&error, single_command, root, shell, true),
        Raw::Exited(code) => Ending::Exited(code.clamp(0, 255)),
    }
}

/// `Raised` and `Walled` differ only in tag: same rendering, same
/// `command_exit`/`single_command` inputs, distinguished by `walled` alone.
fn render_raise(
    error: &crate::types::Error,
    single_command: bool,
    root: crate::source::FileId,
    shell: &Shell,
    walled: bool,
) -> Ending {
    let status = FailureStatus::from(error.exit_code());
    let command_exit = error.status.exited().is_some();
    let rendered = crate::diagnostic::format_runtime_error_auto(
        shell.sources(),
        error,
        single_command.then_some(root),
    );
    let record = record_of(error, shell);
    if walled {
        Ending::Walled {
            rendered,
            record,
            status,
        }
    } else {
        Ending::Raised {
            rendered,
            record,
            command_exit,
            single_command,
            status,
        }
    }
}

impl crate::run::RunReport {
    /// Project into the protocol [`Report`] — the one lossy step between the
    /// engine and the protocol: the live `Value` becomes an [`FOValue`], and rich
    /// diagnostics render to strings. Rendering belongs here because this is
    /// the last point at which the engine's shell is in hand; the host
    /// receives the full string — prefix, status, hint, caret — and only has
    /// to print it.
    pub(crate) fn into_report(self, shell: &Shell) -> Report {
        match self {
            // Not the shell's sources: a static failure carries the text its
            // carets point into, so nothing about it was ever registered.
            Self::Static { diagnostics } => {
                let (rendered, status) = crate::diagnostic::format_static_diagnostics(&diagnostics);
                Report::Static { rendered, status }
            }
            Self::Ran {
                ending,
                captured,
                trail,
            } => Report::Ran {
                ending: render_ending(ending, shell),
                captured,
                trail: trail.iter().map(Observation::to_wire).collect(),
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

/// First cause wins.
fn sever(severance: &OnceLock<Severed>, cause: Severed) {
    let _ = severance.set(cause);
}

/// Why a probe has no reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The engine answered but would not read: an unknown class, a malformed
    /// payload, or the rendezvous held by an in-flight run. A program error
    /// on the caller's side — probes are legal only at a run boundary and
    /// only for the classes [`reading`] knows.
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

// ── Transport trait ───────────────────────────────────────────────────

/// The front-end side of the engine protocol. Construction is attach: a
/// transport in hand is one its engine accepted.
pub trait Transport: Send + Sync {
    /// Run a dispatch synchronously. The `Report` arrives as the final `Event`,
    /// after any `Surface` events, which may be drained concurrently. `host`
    /// is the host's side of this run; see [`dispatch_to_report`] for how each
    /// transport uses it.
    fn dispatch(&self, id: DispatchId, run: Run, host: &Arc<dyn Host>);

    /// Read session state at a run boundary, synchronously. [`reading`]'s
    /// typed doors are the way to ask.
    ///
    /// # Errors
    /// [`ProbeError::Rejected`] is a program error on the caller's side:
    /// `probe` is legal only at a run boundary, so a caller that could see it
    /// has already broken that rule. A wire caller cannot extend that trust
    /// to the far side, and treats a `Rejected` there as the protocol fault
    /// it would then be. [`ProbeError::Severed`] is the engine's death, never
    /// a program error.
    fn probe(&self, reading: FOValue) -> Result<FOValue, ProbeError>;

    /// The out-of-band control sender — writable while a dispatch is in
    /// flight.
    fn control(&self) -> &ControlSender;

    /// The event stream the front-end drains.
    fn events(&self) -> &EventReceiver;

    /// Why no further frame will cross the protocol, if that has happened.
    fn severed(&self) -> Option<Severed>;

    /// Declare the engine dead for a cause the front-end observed itself — an
    /// answer outside the protocol, say — and answer the cause that stands,
    /// which is the first one recorded.
    fn sever(&self, cause: Severed) -> Severed;

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
         same transport — the identity transport would deadlock on its own session lock, and \
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
            Event::Report(report) => return Ok(report),
        }
    }
    Err(transport.severed().expect(READER_SEVERS_FIRST))
}

/// A closed event stream always has a cause: every reader exit path severs
/// before it drops the sender.
const READER_SEVERS_FIRST: &str = "the reader severs before it closes the event stream";

/// Dispatches and probes share this mint: both are answered by an
/// [`Event::Report`] under a [`DispatchId`], so two counters would let a
/// probe's Report be mistaken for a dispatch's.
fn mint_dispatch_id() -> DispatchId {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    DispatchId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

// ── Senders and receivers ─────────────────────────────────────────────

/// A wire sender's write door and the severance cell a failed write records
/// into.
type WireControl = (Arc<Mutex<crate::wire::WireChannel>>, Arc<OnceLock<Severed>>);

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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
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

// ── Identity transport ────────────────────────────────────────────────

/// A session lock that cannot poison-panic, named for the one state it
/// guards: a run that unwinds drops its guard mid-mutation, and recovering
/// the poison (via [`LockExt`]) rather than unwrapping it means such a run
/// can never wedge the session for whatever runs next.
struct SessionLock(std::sync::Mutex<Engine>);

impl SessionLock {
    fn lock(&self) -> std::sync::MutexGuard<'_, Engine> {
        self.0.lock_ignore_poison()
    }
}

/// The in-process carrier: an [`Engine`] behind the session lock, run on the
/// calling thread, its rails calling the `Host` directly.
pub struct IdentityTransport {
    engine: SessionLock,
    /// The engine's scopes, outside the lock, so a `Control` lands on a
    /// dispatch still waiting to take it.
    scopes: Arc<Scopes>,
    installer: &'static EngineInstaller,
    /// Where this transport's runs park their forks — outside the lock, so a
    /// handler can adopt one mid-dispatch.
    nursery: Nursery,
    control: ControlSender,
    /// `Arc`-wrapped so a drain-then-handle adapter can hold its own clone
    /// alongside the transport rather than a borrow tied to `&self`.
    events_recv: Arc<EventReceiver>,
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    /// `None` until a host installs one, and while it is `None` a settling
    /// worker's batch is dropped.
    deferred_sink: Mutex<Option<Arc<dyn DeferredSink>>>,
    severance: OnceLock<Severed>,
}

impl IdentityTransport {
    /// Boot an engine from `attach`, in this process.
    ///
    /// # Errors
    /// [`Severed::Refused`], in the refusing step's own words — the verdict
    /// [`WireTransport::await_attached`] gives.
    pub fn boot(installers: &'static [EngineInstaller], attach: &Attach) -> Result<Self, Severed> {
        Engine::boot(installers, attach, None)
            .map(Self::over)
            .map_err(Severed::Refused)
    }

    fn over(engine: Engine) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let scopes = engine.scopes().clone();
        Self {
            installer: engine.installer(),
            control: ControlSender(Door::Identity(scopes.clone())),
            scopes,
            engine: SessionLock(std::sync::Mutex::new(engine)),
            nursery: Nursery::default(),
            events_recv: Arc::new(EventReceiver::new(event_rx)),
            event_tx,
            deferred_sink: Mutex::new(None),
            severance: OnceLock::new(),
        }
    }

    /// Adopt the fork a run parked under `id` as an engine of its own, its
    /// grant layer narrowed against the fork's own cwd under this transport's
    /// installer.
    ///
    /// # Errors
    /// No fork parked under `id`, or whatever narrowing `grant` refuses.
    pub fn adopt_parked(&self, id: NurseryId, grant: &SpawnGrant) -> Result<Self, String> {
        let mut shell = self.nursery.adopt(id).ok_or_else(|| {
            format!(
                "no forked session is parked under nursery id {}: was it adopted already, or \
                 has the run that forked it ended?",
                id.0
            )
        })?;
        grant.narrow_onto(&mut shell, self.installer.narrow)?;
        Ok(Self::over(Engine::new(shell, self.installer, Box::new(()))))
    }

    /// Read the engine's shell under the session lock.
    #[cfg(feature = "test-util")]
    pub(crate) fn inspect<R>(&self, read: impl FnOnce(&Shell) -> R) -> R {
        forbid_reentry(self);
        read(&self.engine.lock().shell)
    }
}

/// The identity transport's enquiry desk: a direct, same-thread wrapper
/// around the host's `Host`, installed onto each dispatch's `RunRequest`.
///
/// Draining `events` before `host.enquire` is what keeps a handler from
/// running ahead of the run's own surface output.
struct IdentityDesk {
    host: Arc<dyn Host>,
    events: Arc<EventReceiver>,
}

impl crate::types::EnquiryDesk for IdentityDesk {
    /// `_cancel` goes unpolled: [`EnquiryDesk::enquire`] is contractually
    /// blocking and short, so a `Host` answers at once and leaves nothing
    /// here to park on.
    fn enquire(
        &self,
        req: FOValue,
        _cancel: &crate::process::CancelScope,
    ) -> Result<FOValue, crate::types::Error> {
        let mut carried = std::collections::VecDeque::new();
        while let Some((did, event)) = self.events.try_recv() {
            match event {
                Event::Surface(val) => self.host.surface(&val),
                other => carried.push_back((did, other)),
            }
        }
        for item in carried {
            self.events.stash.lock_ignore_poison().push_back(item);
        }
        self.host
            .enquire(req)
            .map_err(|e| crate::types::Error::new(e.message, e.status))
    }
}

impl Transport for IdentityTransport {
    fn dispatch(&self, id: DispatchId, run: Run, host: &Arc<dyn Host>) {
        if self.severed().is_some() {
            return;
        }
        let scope = self.scopes.open(id);
        let events = self.event_tx.clone();
        let rails = Rails {
            outlet: Arc::new(move |id, event| events.send((id, event)).is_ok()),
            deferred: self.deferred_sink.lock_ignore_poison().clone(),
            desk: Arc::new(IdentityDesk {
                host: host.clone(),
                events: self.events_recv.clone(),
            }),
            fork: Fork::Park(self.nursery.clone()),
        };
        let report = self.engine.lock().run(id, run, rails, &scope);
        let _ = self.event_tx.send((id, Event::Report(report)));
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "Transport::probe signature is fixed by the trait; the sibling impl consumes `reading` into a Frame"
    )]
    fn probe(&self, reading: FOValue) -> Result<FOValue, ProbeError> {
        forbid_reentry(self);
        if let Some(cause) = self.severed() {
            return Err(ProbeError::Severed(cause));
        }
        self.engine
            .lock()
            .probe(&reading)
            .map_err(ProbeError::Rejected)
    }

    fn control(&self) -> &ControlSender {
        &self.control
    }

    fn events(&self) -> &EventReceiver {
        &self.events_recv
    }

    fn severed(&self) -> Option<Severed> {
        self.severance.get().cloned()
    }

    fn sever(&self, cause: Severed) -> Severed {
        self.severance.get_or_init(|| cause).clone()
    }

    fn detach(&self) {
        self.scopes.strike(crate::process::CancelCause::Explicit);
    }

    fn set_deferred_sink(&self, sink: Arc<dyn DeferredSink>) {
        *self.deferred_sink.lock_ignore_poison() = Some(sink);
    }
}

#[cfg(test)]
mod identity_cancel_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Dispatch `sleep 30` from another thread while this one holds the
    /// session lock, so `dispatch` parks right after it opens its scope and
    /// before it ever reaches the run; strike the scope with `strike`, then let
    /// the run go. A failure here is the race, not an absence of wiring.
    fn race(strike: impl FnOnce(&ControlSender)) {
        let transport = Arc::new(crate::engine::testkit::boot(&crate::engine::testkit::BARE));
        let guard = transport.engine.lock();

        let worker = {
            let transport = transport.clone();
            std::thread::spawn(move || {
                transport.dispatch(
                    DispatchId(1),
                    crate::engine::testkit::run("sleep 30"),
                    &(Arc::new(()) as Arc<dyn Host>),
                );
            })
        };

        // `dispatch` opens its scope ahead of the session lock; wait for that
        // rather than guessing at a delay.
        let deadline = Instant::now() + Duration::from_secs(5);
        while transport.scopes.current() != Some(DispatchId(1)) {
            assert!(
                Instant::now() < deadline,
                "dispatch must open its scope before acquiring the session lock"
            );
            std::thread::yield_now();
        }

        strike(transport.control());
        drop(guard);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.join().expect("dispatch must not panic");
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a strike recorded before the run's frame exists must still settle it promptly, \
             not run `sleep 30` to completion"
        );
    }

    /// The run's frame hangs off the scope a strike lands on, even one struck
    /// before the frame exists.
    #[test]
    fn a_cancel_racing_a_fresh_dispatch_is_not_dropped() {
        race(|control| control.cancel(DispatchId(1)));
    }

    #[test]
    fn an_interrupt_racing_a_fresh_dispatch_is_not_dropped() {
        race(ControlSender::interrupt);
    }

    /// Dispatch a child under a forwarder, `raise` once the child is running,
    /// and return the run's status and whether the session then ended.
    #[cfg(unix)]
    fn forwarded(raise: fn()) -> (i32, Option<i32>) {
        let _serial = crate::process::cancel::REQUEST_SERIAL.lock();
        let dir = tempfile::tempdir().expect("a temp dir");
        let marker = dir.path().join("running");
        let transport = Arc::new(crate::engine::testkit::boot(&crate::engine::testkit::BARE));
        let _signals = transport.control().forward_signals();
        let worker = {
            let transport = transport.clone();
            let src = format!("sh -c 'touch {}; exec sleep 30'", marker.display());
            std::thread::spawn(move || crate::engine::testkit::eval(&transport, &src))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "the child never touched its marker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        raise();
        let report = worker.join().expect("dispatch must not panic");
        crate::process::cancel::clear_root_request();
        let Report::Ran { ending, .. } = report else {
            panic!("the child must reach evaluation, got {report:?}");
        };
        let ended = reading::session_ended(&*transport).expect("the probe answers");
        (ending.status(), ended)
    }

    /// A SIGINT reaches an identity engine only as its host's
    /// `Control::Interrupt`: the run unwinds, the session lives on.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_interrupt_unwinds_the_run_in_flight() {
        assert_eq!(
            forwarded(crate::process::request_interrupt),
            (130, None),
            "an interrupt ends the run, never the session"
        );
    }

    /// Ctrl-`\` arrives as `Control::Abort`, ending the session with its own
    /// cause: `RootAbort`, reported 131 rather than a SIGTERM's 143.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_root_abort_ends_the_session() {
        assert_eq!(
            forwarded(|| crate::process::request_root_cancel(
                crate::process::CancelCause::RootAbort
            )),
            (131, Some(131)),
        );
    }

    /// SIGTERM arrives as `Control::Terminate`.
    #[cfg(unix)]
    #[test]
    fn a_forwarded_terminate_ends_the_session() {
        assert_eq!(
            forwarded(|| crate::process::request_root_cancel(
                crate::process::CancelCause::Terminate
            )),
            (143, Some(143)),
        );
    }
}

// ── Wire transport ────────────────────────────────────────────────────

/// The front-end's half of the severance law: [`crate::wire::write_or_sever`]
/// recording into `severance`. Shared by [`WireTransport::write`] and the wire
/// arm of `ControlSender`; `Err` is handed back so a caller that must
/// react further — the heartbeat's `Ping` arm breaking its loop — can, without
/// repeating the severing itself.
fn write_through(
    ch: &Mutex<crate::wire::WireChannel>,
    severance: &OnceLock<Severed>,
    frame: &Frame,
) -> io::Result<()> {
    crate::wire::write_or_sever(ch, frame, |e| {
        sever(severance, Severed::Closed(e.to_string()));
    })
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

/// How many whole `tick`s `span` is worth, never none.
fn ticks(span: Duration, tick: Duration) -> u32 {
    let count = span.as_nanos().checked_div(tick.as_nanos()).unwrap_or(1);
    u32::try_from(count).unwrap_or(u32::MAX).max(1)
}

impl Liveness {
    /// How many unanswered `Ping`s amount to the deadline.
    ///
    /// The deadline is *configured* as a span but *judged* as this count: a
    /// clock measures the host's absence as readily as the peer's silence, so
    /// a suspended laptop would wake to find its engine long dead of a silence
    /// nobody was running to observe.  A probe is evidence because this
    /// process authored it; a duration is evidence of nothing.
    fn probes(self) -> u32 {
        ticks(self.deadline, self.interval)
    }
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            deadline: Duration::from_secs(25),
        }
    }
}

/// The out-of-process transport: the engine runs across a duplex stream, frames
/// crossing on a `WireChannel` as length-prefixed JSON.
///
/// [`WireTransport::adopt`] drives an *existing* stream, in production the
/// virtual socket into a guest VM, whose failure mode is silence rather than
/// EOF, and so runs a ticker.
///
/// A reader thread forwards `Event` frames into the `EventReceiver`, swallows
/// `Pong`s, and timestamps every frame it reads. All writes share one
/// `Mutex<WireChannel>`, the ticker's `Ping` included.
pub struct WireTransport {
    events_recv: EventReceiver,
    control: ControlSender,
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    /// Never joined: the thread exits when the channel closes. `Mutex` only
    /// for `Sync`.
    _reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set once, first cause wins — a write error, a read EOF, a refused
    /// `Attach`, or the ticker's silence deadline. Reader and ticker both
    /// check it to break early, and it is what [`WireTransport::severed`]
    /// reports.
    severance: Arc<OnceLock<Severed>>,
    /// `Ping`s sent with no frame back since — what the ticker counts silence
    /// in. Any frame read resets it; see [`Liveness::probes`].
    unanswered: Arc<AtomicU32>,
    /// The heartbeat still owed, parked by [`WireTransport::adopt`] and taken
    /// by the first [`WireTransport::attach`].
    pending_heartbeat: Mutex<Option<Liveness>>,
    /// Where the reader thread hands a `Frame::Session` batch. Shared with the
    /// reader so `set_deferred_sink` takes effect on the next batch without
    /// restarting anything; `None` drops a batch that arrives before a host
    /// installs one.
    deferred_sink: Arc<Mutex<Option<Arc<dyn DeferredSink>>>>,
    /// A shutdown-only duplicate of the socket, minted alongside `write_tx`.
    /// `Drop` shuts this down directly rather than taking
    /// `write_tx`'s lock, so tearing the transport down never parks behind a
    /// write some other thread holds the lock over.
    shutdown: crate::wire::WireChannel,
    /// Set true by the reader on `SessionEvent::Attached`; awaited by
    /// [`WireTransport::await_attached`].
    attached: Arc<(Mutex<bool>, Condvar)>,
    /// How long [`WireTransport::await_attached`] waits for the verdict before
    /// declaring the engine [`Severed::Silent`].
    patience: Duration,
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
    mut reader_ch: crate::wire::WireChannel,
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    deferred_sink: Arc<Mutex<Option<Arc<dyn DeferredSink>>>>,
    severance: Arc<OnceLock<Severed>>,
    unanswered: Arc<AtomicU32>,
    attached: Arc<(Mutex<bool>, Condvar)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Severs on every exit, panics included (e.g. from `DeferredSink::deliver`),
        // so `dispatch_to_report`'s `expect` always finds a cause. Declared first so
        // it drops after the loop's own locals but before the captured `event_tx`.
        struct SeverOnExit<'a> {
            severance: &'a OnceLock<Severed>,
            attached: &'a (Mutex<bool>, Condvar),
        }
        impl Drop for SeverOnExit<'_> {
            fn drop(&mut self) {
                sever(
                    self.severance,
                    Severed::Closed("the reader thread ended".into()),
                );
                self.attached.1.notify_all();
            }
        }
        let _sever_on_exit = SeverOnExit {
            severance: &severance,
            attached: &attached,
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
                            *attached.0.lock_ignore_poison() = true;
                            attached.1.notify_all();
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
/// The deadline arm never takes `write_tx`'s lock: severing from silence must
/// not itself be capable of parking behind a write some other thread is
/// stalled on, so a severed transport always implies `shutdown` runs in
/// bounded time. The `Ping` arm goes through [`write_through`], which severs
/// on failure the same way any other front-end write does.
fn spawn_heartbeat(
    write_tx: Arc<Mutex<crate::wire::WireChannel>>,
    severance: Arc<OnceLock<Severed>>,
    unanswered: Arc<AtomicU32>,
    liveness: Liveness,
    shutdown: crate::wire::WireChannel,
) {
    let patience = liveness.probes();
    std::thread::spawn(move || {
        let mut seq: u64 = 0;
        loop {
            std::thread::sleep(liveness.interval);
            if severance.get().is_some() {
                break;
            }
            // Only probes this thread actually sent can condemn the peer, so a
            // host that was asleep — sending none — wakes accusing nobody.
            if unanswered.load(Ordering::Relaxed) >= patience {
                sever(&severance, Severed::Silent(liveness.deadline));
                shutdown.shutdown();
                break;
            }
            seq += 1;
            if write_through(&write_tx, &severance, &Frame::Ping(seq)).is_err() {
                break;
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
    /// socket under Hyper-V — adopted through [`crate::wire::WireStream`],
    /// whose docs say why std's stream types can carry either. Such a stream
    /// can fall silent without ever tearing, so the first
    /// [`WireTransport::attach`] spawns a heartbeat ticker under `liveness`.
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

        let severance = Arc::new(OnceLock::new());
        let unanswered = Arc::new(AtomicU32::new(0));
        let (event_tx, event_rx) = mpsc::channel();
        let deferred_sink = Arc::new(Mutex::new(None));
        let attached = Arc::new((Mutex::new(false), Condvar::new()));
        let reader = spawn_wire_reader(
            reader_ch,
            event_tx,
            deferred_sink.clone(),
            severance.clone(),
            unanswered.clone(),
            attached.clone(),
        );

        let write_tx = Arc::new(Mutex::new(writer));
        let control = ControlSender(Door::Wire((write_tx.clone(), severance.clone())));

        Ok(Self {
            events_recv: EventReceiver::new(event_rx),
            control,
            write_tx,
            _reader: Mutex::new(Some(reader)),
            severance,
            unanswered,
            pending_heartbeat: Mutex::new(Some(liveness)),
            deferred_sink,
            shutdown,
            attached,
            patience: liveness.deadline,
        })
    }

    /// Write `attach`, the only legal first frame, and only then start the
    /// heartbeat: no `Ping` may precede the handshake. The verdict is
    /// [`Self::await_attached`]'s.
    ///
    /// # Panics
    /// If the already-open socket cannot be duplicated for the heartbeat.
    pub fn attach(&self, attach: Attach) {
        self.write(&Frame::Attach(attach));
        let pending = self.pending_heartbeat.lock_ignore_poison().take();
        if let Some(liveness) = pending {
            // Silence is counted from here, not from `adopt`: a front-end that
            // adopted long before attaching must not find its booting engine
            // already declared `Silent` on the heartbeat's first tick.
            self.unanswered.store(0, Ordering::Relaxed);
            spawn_heartbeat(
                self.write_tx.clone(),
                self.severance.clone(),
                self.unanswered.clone(),
                liveness,
                self.shutdown
                    .try_clone()
                    .expect("dup an already-open socket"),
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
    ///
    /// # Panics
    /// Never: the `expect` fires only after this call has just severed the
    /// transport itself, on the same line above it.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard is the loop's own state, retaken by every wait"
    )]
    pub fn await_attached(&self) -> Result<(), Severed> {
        const TICK: Duration = Duration::from_millis(100);
        let mut left = ticks(self.patience, TICK);
        let mut guard = self.attached.0.lock_ignore_poison();
        loop {
            // Severance first: an `Attached` that raced a death must not grant
            // a seat on a transport already known dead.
            if let Some(cause) = self.severed() {
                return Err(cause);
            }
            if *guard {
                return Ok(());
            }
            if left == 0 {
                sever(&self.severance, Severed::Silent(self.patience));
                self.shutdown.shutdown();
                return Err(self
                    .severed()
                    .expect("sever just recorded the cause this call names"));
            }
            let (next, timed_out) = self.attached.1.wait_timeout_ignore_poison(guard, TICK);
            guard = next;
            if timed_out.timed_out() {
                left -= 1;
            }
        }
    }

    /// Any write error is fatal, `ECONNRESET` no less than `BrokenPipe`: a
    /// dropped frame means no `Report` will ever arrive, so the host's `recv`
    /// must learn of the severance rather than block forever.
    fn write(&self, frame: &Frame) {
        let _ = write_through(&self.write_tx, &self.severance, frame);
    }
}

impl Drop for WireTransport {
    fn drop(&mut self) {
        // `self.shutdown` never takes `write_tx`'s lock, so this wakes the
        // reader and the ticker — which share the one socket —
        // without ever parking behind a write in progress.
        sever(
            &self.severance,
            Severed::Closed("the front-end dropped the transport".into()),
        );
        self.shutdown.shutdown();
    }
}

impl Transport for WireTransport {
    fn dispatch(&self, id: DispatchId, run: Run, _host: &Arc<dyn Host>) {
        // The host is consulted by the drain loop, not here: an enquiry
        // crossing a wire is a frame on `events()`, answered by whoever drains
        // it — `dispatch_to_report`.
        self.write(&Frame::Dispatch(id, Box::new(run)));
    }

    fn probe(&self, reading: FOValue) -> Result<FOValue, ProbeError> {
        forbid_reentry(self);
        let id = mint_dispatch_id();
        self.write(&Frame::Probe(id, reading));
        // Foreign events read past on the way to this probe's own Report are
        // held here rather than restashed mid-loop: `recv`'s stash-first law
        // would hand a mid-loop stash straight back on the next iteration,
        // spinning forever on a foreign event.
        let mut carried = std::collections::VecDeque::new();
        let outcome = loop {
            match self.events_recv.recv() {
                Some((did, Event::Report(report))) if did == id => {
                    break reading::unreport(report);
                }
                Some(item) => carried.push_back(item),
                None => {
                    break Err(ProbeError::Severed(
                        self.severed().expect(READER_SEVERS_FIRST),
                    ));
                }
            }
        };
        // Anything that was not this probe's Report — another dispatch's, a
        // worker's batch settling mid-probe — is stashed for the ordinary
        // drain rather than dropped.
        let mut stash = self.events_recv.stash.lock_ignore_poison();
        stash.extend(carried);
        drop(stash);
        outcome
    }

    fn control(&self) -> &ControlSender {
        &self.control
    }

    fn events(&self) -> &EventReceiver {
        &self.events_recv
    }

    /// Why no further frame will cross the protocol, if that has happened — a
    /// write error, a read EOF, a refused `Attach`, or the heartbeat's silence
    /// deadline. This is how a front-end tells *detached* from merely failed:
    /// once severed, no further frame will ever cross.
    fn severed(&self) -> Option<Severed> {
        self.severance.get().cloned()
    }

    /// Also shuts the connection, so the engine sees EOF.
    fn sever(&self, cause: Severed) -> Severed {
        let standing = self.severance.get_or_init(|| cause).clone();
        self.shutdown.shutdown();
        standing
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
}

// ── Enquiry desk tests ────────────────────────────────────────────────
//
// Core installs no builtin that enquires, so these install a test builtin
// that does — the shape a real enquiring door runs under.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
mod enquiry_tests {
    use super::*;
    use crate::run::tests::{capture_req, install_act};
    use crate::run::{RunReport, RunRequest};
    use crate::types::{Desk, Mooring};
    use std::sync::Mutex;

    /// No desk installed answers the honest absence error, verbatim.
    #[test]
    fn absent_desk_answers_the_honest_error() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let err = shell
            .enquire(&Mooring::adrift(), FOValue::Unit)
            .expect_err("no desk is installed");
        assert_eq!(err.message, crate::types::NO_DESK);
    }

    /// A no-op receiver, for a desk built without a real dispatch behind it.
    fn no_events() -> Arc<EventReceiver> {
        let (_tx, rx) = mpsc::channel();
        Arc::new(EventReceiver::new(rx))
    }

    /// A stub host that maps `Int{n}` to `Int{n+1}`, otherwise echoes.
    struct IncrementSeam;
    impl Host for IncrementSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            match req {
                FOValue::Int { value } => Ok(FOValue::Int { value: value + 1 }),
                other => Ok(other),
            }
        }
    }

    /// The host's transform reaches `enquire`'s caller, not just `IdentityDesk`.
    #[test]
    fn enquire_round_trips_through_a_stub_desk() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let answer = std::sync::Arc::new(Mutex::new(None));
        let asked = answer.clone();
        install_act(&mut shell, "ask-desk", move |mooring, shell| {
            *asked.lock().unwrap() = Some(shell.enquire(mooring, FOValue::Int { value: 41 }));
        });
        let desk: Desk = Arc::new(IdentityDesk {
            host: Arc::new(IncrementSeam),
            events: no_events(),
        });

        match shell.run(RunRequest {
            desk: Some(desk),
            ..capture_req("ask-desk")
        }) {
            RunReport::Ran { .. } => {}
            RunReport::Static { .. } => panic!("`ask-desk` must reach evaluation"),
        }

        let answer = answer
            .lock()
            .unwrap()
            .take()
            .expect("`ask-desk` must enquire");
        match answer {
            Ok(v) => assert_eq!(
                v,
                FOValue::Int { value: 42 },
                "enquire must return the host's transformed answer"
            ),
            Err(e) => panic!("enquire must succeed through the stub desk, got {e:?}"),
        }
    }

    /// A handler reaching back into `probe` would take the session lock its
    /// own stack already holds. Lacking a core builtin that enquires, the test
    /// enters `dispatch_to_report`'s claim by hand and exercises the same guard.
    struct ReentrantProbeSeam(std::sync::Arc<IdentityTransport>);
    impl Host for ReentrantProbeSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            let _ = reading::cwd(&*self.0);
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_probe_panics_never_hangs() {
        reenter(|transport| Arc::new(ReentrantProbeSeam(transport)));
    }

    /// Enquire, from inside a dispatch's claim, of the host `seam` builds.
    fn reenter(seam: impl FnOnce(Arc<IdentityTransport>) -> Arc<dyn Host>) {
        let transport = Arc::new(crate::engine::testkit::boot(&crate::engine::testkit::BARE));
        let _dispatching = Dispatching::enter(&*transport);
        let shell = Shell::new(crate::io::TerminalState::default());
        let mut mooring = Mooring::adrift();
        mooring.desk = Some(Arc::new(IdentityDesk {
            host: seam(transport),
            events: no_events(),
        }) as Desk);
        let _ = shell.enquire(&mooring, FOValue::Unit);
    }

    /// The same guard on the other door.
    struct ReentrantDispatchSeam(std::sync::Arc<IdentityTransport>);
    impl Host for ReentrantDispatchSeam {
        fn surface(&self, _val: &FOValue) {}
        fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
            let _ = dispatch_to_report(&*self.0, crate::engine::testkit::run(""), Arc::new(()));
            Ok(req)
        }
    }

    #[test]
    #[should_panic(expected = "reentrant session access")]
    fn desk_reentering_dispatch_panics_never_hangs() {
        reenter(|transport| Arc::new(ReentrantDispatchSeam(transport)));
    }
}

// ── Run-door durability, seen through the protocol ────────────────────
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
        std::borrow::Cow::Borrowed("protocol-panic-now"),
        scheme_panic_now,
        "test-only: panic the evaluator mid-run.",
        crate::types::BuiltinBody::Static(builtin_panic_now),
    )];

    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
    )]
    fn panicky(_attach: &Attach) -> Result<crate::engine::Booted, String> {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(&PANIC_BUILTINS_ARR);
        Ok(crate::engine::Booted {
            shell,
            keep: Box::new(()),
        })
    }

    static PANICKY: [EngineInstaller; 1] = [crate::engine::testkit::installer("panicky", panicky)];

    /// A panicking run arrives as an ordinary `Report::Static{Host}` — the run
    /// door caught it and rolled the shell back — and the same transport
    /// dispatches the next run cleanly.
    #[test]
    fn panicking_dispatch_reports_and_the_session_survives() {
        let transport = crate::engine::testkit::boot(&PANICKY);

        let report = crate::engine::testkit::eval(&transport, "protocol-panic-now");
        match report {
            Report::Static { rendered, .. } => {
                assert!(rendered.contains("run panicked"), "got {rendered:?}");
            }
            other @ Report::Ran { .. } => {
                panic!("a panicking run must report Static Host, got {other:?}")
            }
        }

        let report = crate::engine::testkit::eval(&transport, "$[1 + 1]");
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
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod probe_tests {
    use super::*;
    use crate::engine::testkit::{BARE, attach, boot, boot_at, eval};

    fn fresh() -> IdentityTransport {
        boot(&BARE)
    }

    /// A bare engine seated at `dir`.
    fn at(dir: &std::path::Path) -> IdentityTransport {
        boot_at(
            &BARE,
            &Attach {
                cwd: dir.to_path_buf(),
                ..attach(&BARE)
            },
        )
    }

    fn raw(label: &str, payload: Option<FOValue>) -> FOValue {
        FOValue::Variant {
            label: label.into(),
            payload: payload.map(Box::new),
        }
    }

    fn rejection(transport: &IdentityTransport, req: FOValue) -> String {
        match transport.probe(req) {
            Err(ProbeError::Rejected(msg)) => msg,
            other => panic!("a malformed probe is a rejection, got {other:?}"),
        }
    }

    /// A fresh shell's ledger is unarmed and nothing is spawned or bound.
    #[test]
    fn a_fresh_shell_reads_empty() {
        let transport = fresh();
        assert!(reading::binding_count(&transport).is_ok());
        assert_eq!(reading::leased_binding_count(&transport), Ok(0));
        assert_eq!(reading::largest_binding_bytes(&transport), Ok(0));
        assert_eq!(reading::workers(&transport), Ok(Vec::new()));
        assert!(reading::cwd(&transport).is_ok());
        assert!(reading::home(&transport).is_ok());
        assert!(reading::builtin_names(&transport).is_ok());
    }

    #[cfg(feature = "test-util")]
    #[test]
    fn the_test_only_classes_answer() {
        let transport = fresh();
        assert_eq!(
            reading::read::<u64>(&transport, reading::Class::WorkerCount, None),
            Ok(0)
        );
        assert!(
            reading::read::<u64>(&transport, reading::Class::GrantDepth, None)
                .is_ok_and(|depth| depth >= 1),
            "a fresh shell still carries the ambient root frame"
        );
    }

    #[test]
    fn largest_binding_bytes_measures_what_is_bound() {
        let transport = fresh();
        eval(&transport, "let probe_sized = 'a dozen bytes or so'");
        assert!(reading::largest_binding_bytes(&transport).is_ok_and(|n| n > 0));
    }

    #[test]
    fn env_var_reads_the_overlay() {
        let mut attach = attach(&BARE);
        attach.env.push(("PROBE_TEST_VAR".into(), "42".into()));
        let transport = boot_at(&BARE, &attach);
        assert_eq!(
            reading::env_var(&transport, "PROBE_TEST_VAR_ABSENT"),
            Ok(None)
        );
        assert_eq!(
            reading::env_var(&transport, "PROBE_TEST_VAR"),
            Ok(Some("42".to_string()))
        );
    }

    /// Relative to the engine's own cwd, and recursive.
    #[test]
    fn path_bytes_sums_a_tree_under_the_engine_cwd() {
        let dir = tempfile::tempdir().expect("a tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("a subdirectory");
        std::fs::write(dir.path().join("sub/five"), b"12345").expect("a file");
        let transport = at(dir.path());
        assert_eq!(
            reading::path_bytes(&transport, std::path::Path::new(".")),
            Ok(5)
        );
    }

    /// Ended only once its durable root is cancelled, with that cause's status.
    #[test]
    fn session_ended_reads_the_root_cause() {
        let transport = fresh();
        assert_eq!(reading::session_ended(&transport), Ok(None));
        transport.control().terminate();
        assert_eq!(reading::session_ended(&transport), Ok(Some(143)));
    }

    #[test]
    fn spine_stages_a_pipeline_and_locates_a_type_error() {
        let transport = fresh();
        assert_eq!(reading::spine(&transport, " "), Ok(reading::Spine::Empty));
        let Ok(reading::Spine::Stages(stages)) =
            reading::spine(&transport, "/bin/echo hi | /bin/cat")
        else {
            panic!("a pipeline stages");
        };
        assert_eq!(stages[0].src, "/bin/echo hi");
        let Ok(reading::Spine::TypeError(error)) =
            reading::spine(&transport, "if \"x\" { 1 } else { 2 }")
        else {
            panic!("an ill-typed buffer locates its error");
        };
        assert_eq!(error.span, Some((3, 6)));
    }

    #[test]
    fn bind_effects_tells_an_exec_from_arithmetic() {
        let effects = reading::bind_effects(&fresh(), "let n = $[1 + 2]\nlet p = /bin/echo hi");
        let effectful = |name: &str, effectful: bool| reading::BindEffect {
            name: name.into(),
            effectful,
        };
        assert_eq!(
            effects,
            Ok(vec![effectful("n", false), effectful("p", true)])
        );
    }

    #[test]
    fn bindings_and_completion_names_read_the_scope() {
        let transport = fresh();
        eval(&transport, "let probe_row = 7");
        let rows = reading::bindings(&transport).expect("a reading");
        let row = rows
            .iter()
            .find(|r| r.name == "probe_row")
            .expect("the row");
        assert_eq!((row.preview.as_str(), &row.handle), ("7", &None));
        let names = reading::completion_names(&transport).expect("a reading");
        assert!(names.bindings.iter().any(|n| n == "probe_row"));
    }

    #[test]
    fn path_entries_lists_under_the_engine_cwd() {
        let dir = tempfile::tempdir().expect("a tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("a subdirectory");
        let transport = at(dir.path());
        assert_eq!(
            reading::path_entries(&transport, std::path::Path::new(".")),
            Ok(vec![reading::PathEntry {
                name: "sub".into(),
                dir: true,
                exec: false,
            }])
        );
    }

    #[test]
    fn an_unknown_class_names_itself() {
        let msg = rejection(&fresh(), raw("not-a-real-class", None));
        assert!(msg.contains("not-a-real-class"), "{msg}");
    }

    #[test]
    fn a_request_that_is_not_a_variant_is_refused() {
        let msg = rejection(&fresh(), FOValue::Unit);
        assert!(msg.contains("variant"), "{msg}");
    }

    /// Each class states its payload, and a request breaking that is refused
    /// naming the class.
    #[test]
    fn a_payload_its_class_does_not_take_is_refused() {
        let transport = fresh();
        let msg = rejection(&transport, raw("cwd", Some(FOValue::Unit)));
        assert!(msg.contains("`cwd probe takes no payload"), "{msg}");
        let msg = rejection(&transport, raw("env-var", None));
        assert!(
            msg.contains("`env-var probe reads a string payload"),
            "{msg}"
        );
    }

    /// First cause wins, and a severed identity is as over as a wire.
    #[test]
    fn a_severed_identity_refuses_what_follows() {
        let transport = fresh();
        let first = Severed::Faulted("the first".into());
        assert_eq!(transport.sever(first.clone()), first);
        assert_eq!(
            transport.sever(Severed::Faulted("the second".into())),
            first
        );
        assert_eq!(
            reading::cwd(&transport),
            Err(ProbeError::Severed(first.clone()))
        );
        let refused = dispatch_to_report(
            &transport,
            Run {
                program: Program::Source("$[1 + 1]".into()),
                script_name: "<test>".into(),
                caps: crate::types::GrantStack::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: crate::run::RunIo::Capture,
                terminal: crate::run::RequestedTerminalAccess::Denied,
                stdin: crate::run::RunStdin::Empty,
                trail: None,
            },
            Arc::new(()),
        );
        assert_eq!(refused, Err(first));
    }

    /// A parked fork is adopted once, and narrowed under the parent's own
    /// installer — which, for a test engine, has no base lexicon to narrow by.
    #[test]
    fn a_parked_fork_is_adopted_once_under_the_parent_installer() {
        let transport = fresh();
        let park = || {
            transport
                .nursery
                .park(Shell::new(crate::io::TerminalState::default()))
        };
        let id = park();
        assert!(transport.adopt_parked(id, &SpawnGrant::Inherit).is_ok());
        let again = transport
            .adopt_parked(id, &SpawnGrant::Inherit)
            .err()
            .expect("a fork adopts once");
        assert!(again.contains("no forked session is parked"), "{again}");
        let based = transport
            .adopt_parked(park(), &SpawnGrant::Base("confined".into()))
            .err()
            .expect("a bare engine has no base lexicon");
        assert!(based.contains("`confined`"), "{based}");
    }
}

// ── Runtime-error protocol tests ───────────────────────────────────────
//
// Rendering at the protocol is what gives every front-end, the REPL included,
// batch-host parity: it prints the string verbatim and renders nothing itself.
#[cfg(test)]
mod runtime_error_seam_tests {
    use super::*;
    use crate::types::Error;

    #[test]
    fn runtime_error_projects_to_a_full_diagnostic_string() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let err = Error::new("boom", 3).with_hint("try harder");
        // A single command whose error carries no span: the compact one-liner.
        let Ending::Raised {
            rendered, record, ..
        } = render_raise(&err, true, crate::source::FileId(0), &shell, false)
        else {
            panic!("an engine runtime error must project to a wire Ending::Raised");
        };
        assert_eq!(
            record.field("message"),
            Some(&FOValue::String {
                value: "boom".into()
            }),
            "the record carries the bare message: {record:?}"
        );
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
            "the protocol must supply the trailing newline the host prints verbatim: {rendered:?}"
        );
    }
}

// ── The static seam ──────────────────────────────────────────────────
//
// The counterpart law for a run that never reached evaluation: the caret, the
// code and the hint survive the projection, and the registry never learns of
// text no live span can index.
#[cfg(test)]
mod static_diagnostic_seam_tests {
    use super::*;
    use crate::run::{RunReport, StaticDiagnostics, tests::capture_req};
    use crate::types::Shell;

    /// The wire report for `src`, and how many registry ids the run minted.
    fn project(src: &str) -> (String, i32, u32) {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let before = shell.sources().next_id().0;
        let report = shell.run(capture_req(src));
        assert!(
            matches!(report, RunReport::Static { .. }),
            "{src:?} must fail before evaluation"
        );
        let minted = shell.sources().next_id().0 - before;
        let Report::Static { rendered, status } = report.into_report(&shell) else {
            panic!("a static run must project to Report::Static");
        };
        (rendered, status, minted)
    }

    #[test]
    fn a_parse_failure_projects_to_a_caret_report() {
        let (rendered, status, _) = project("let = ");
        assert_eq!(status, 2, "a parse failure exits 2: {rendered:?}");
        assert!(
            rendered.contains("[P0001]"),
            "the code must survive: {rendered:?}"
        );
        assert!(
            rendered.contains("<test>:1:5"),
            "the resolved position must survive: {rendered:?}"
        );
        assert!(
            rendered.contains('╰'),
            "the caret must survive: {rendered:?}"
        );
    }

    /// `code()`, `render_label()` and `hint()` are exactly what `Display` drops.
    #[test]
    fn a_type_failure_projects_with_its_code_label_and_hint() {
        let (rendered, status, _) = project("$[1 + true]");
        assert_eq!(status, 1, "a type failure exits 1: {rendered:?}");
        assert!(
            rendered.contains("[T0010]"),
            "the code must survive: {rendered:?}"
        );
        assert!(
            rendered.contains("Integer doesn't match Bool"),
            "the under-caret label must survive: {rendered:?}"
        );
        assert!(
            rendered.contains("Help:"),
            "the hint must survive: {rendered:?}"
        );
        assert!(
            !rendered.contains("@0.."),
            "raw byte offsets must not reach a host: {rendered:?}"
        );
    }

    /// A failed compile leaves no live span, so its text has no slot.
    #[test]
    fn a_static_failure_registers_no_source() {
        for src in ["let = ", "$[1 + true]"] {
            let (_, _, minted) = project(src);
            assert_eq!(minted, 0, "{src:?} must not grow the registry");
        }
    }

    /// The `Host` arm is spanless, so it renders as the one-liner.
    #[test]
    fn a_host_fault_renders_without_a_caret() {
        let (rendered, status) = crate::diagnostic::format_static_diagnostics(
            &StaticDiagnostics::Host(crate::types::Error::new("hook 'x' is not registered", 1)),
        );
        assert_eq!(status, 1);
        assert!(rendered.contains("hook 'x' is not registered"));
        assert!(!rendered.contains('╰'), "no span, no caret: {rendered:?}");
        assert!(rendered.ends_with('\n'));
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
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod wire_liveness_tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

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
        transport.attach(Attach::new("repl", PathBuf::from("/"), PathBuf::from("/")));
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
            Frame::Attach(attach) => assert_eq!(attach.proto_version, PROTOCOL_VERSION),
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
            Frame::Attach(_) => {}
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
            transport.severed().is_none(),
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
        assert!(
            transport.severed().is_some(),
            "silence past the deadline is death"
        );

        drop(back);
    }

    /// Silence is counted in probes, not in clock, so the peer is condemned
    /// only after it has ignored a full deadline's worth of `Ping`s this
    /// process actually sent. A host asleep sends none and so accuses nobody —
    /// the sleep/wake false death this replaced.
    #[test]
    fn a_peer_is_condemned_by_unanswered_probes_not_by_elapsed_time() {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, brisk()).unwrap();
        attach(&transport);

        // The peer reads but never answers, so every ping stays unanswered.
        let mut peer = crate::wire::WireChannel::from_stream(back);
        let mut pings = 0;
        while transport.severed().is_none() {
            match peer.read_frame() {
                Ok(Some(Frame::Ping(_))) => pings += 1,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            pings >= brisk().probes(),
            "death must follow a deadline's worth of probes, got {pings}"
        );
    }

    /// The reader severs before dropping its sender, so `severed()` is honest
    /// the instant `recv` unblocks on a hangup.
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
            transport.severed().is_some(),
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

        transport.control().cancel(DispatchId(1));

        assert!(
            transport.severed().is_some(),
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
            Frame::Attach(_) => {}
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
                caps: crate::types::GrantStack::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: crate::run::RunIo::Capture,
                terminal: crate::run::RequestedTerminalAccess::Denied,
                stdin: crate::run::RunStdin::Empty,
                trail: None,
            },
            &(Arc::new(()) as Arc<dyn Host>),
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

/// The severance codes are load-bearing text: a front-end prints them beside
/// its one-sentence failure so that a person who cannot read a log still has
/// something exact to quote.  These assertions exist to make a rename a
/// deliberate act with a failing test attached, rather than a tidy-up nobody
/// notices until a support thread stops matching.
#[cfg(test)]
mod severance_codes {
    use super::Severed;
    use std::time::Duration;

    #[test]
    fn every_severance_has_its_own_settled_code() {
        let codes = [
            (Severed::Refused("v9".into()).code(), "engine-refused"),
            (Severed::Closed("eof".into()).code(), "engine-closed"),
            (
                Severed::Silent(Duration::from_secs(30)).code(),
                "engine-silent",
            ),
            (Severed::Faulted("junk".into()).code(), "engine-faulted"),
        ];
        for (got, want) in codes {
            assert_eq!(got, want, "a severance code may not be renamed in place");
        }
        let mut distinct: Vec<_> = codes.iter().map(|(got, _)| *got).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), codes.len(), "two severances share a code");
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
