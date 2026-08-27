//! The exarch enquiry desk: the host half of `shell.enquire(class)`.
//!
//! [`HostServices`] is all `&Avatar` can lend a handler without lending
//! `&mut Avatar`/`&mut Shell`, since the reentrancy law bars a handler from
//! taking the session lock. [`ExarchDesk`] answers one enquiry against that
//! capture, paired with its [`SurfaceApplier`] in one [`HostSeam`];
//! [`DeskBinding`] wraps the seam for the identity transport, where a
//! handler's chrome would otherwise outrun the run's earlier surface output.

use crate::agent::event::{CACHE_SENTENCE, ContextOp, EditAuthority};
use crate::agent::{Avatar, Build, LogCell, ProviderHandle, ReplyCell};
use crate::bus::{AgentId, Emitter, Mailbox};
use crate::egress;
use crate::fleet::registry::{AgentRegistry, MessageError, NotADescendant, check_name};
use crate::fleet::schedule::{CronSchedule, ScheduleRegistry, Trigger, parse_duration};
use crate::record::commit::SurfaceBuffer;
use crate::shell_eval::{self, PinDigests, Surface};
use crate::sync::LockExt;
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::transport::{Event, EventReceiver};
use ral_core::types::{
    CallSite, Capabilities, EnquiryDesk, Error, NO_DESK, NO_DESK_STATUS, Nursery, NurseryId,
    Observation, Observed,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A hatched child's cwd — `vm_manager::MachineSpec::GUEST_WORKSPACE`
/// restated as a literal, since the desk may not depend on `vm_manager`.
const HATCHED_CWD: &str = "/work";
/// A hatched child's home — the guest's disposable scratch tmpfs, restated
/// for the same reason (`synod::grant::GUEST_SCRATCH`): `$HOME` pointed at
/// the workspace would litter it with whatever XDG-defaulting tools drop.
const HATCHED_HOME: &str = "/tmp";

/// The two words that bracket one hatch, in opposite directions: the host
/// writes the eight token bytes the guest's listener is waiting for, and reads
/// back the single byte that listener writes only once the child's `spawn` has
/// returned. Both precede the first frame; neither is one.
///
/// The clock is the transport's own, so a dial carries no second deadline —
/// and it is lifted again before the stream is adopted, since the reader
/// thread must then park in `read_frame` for as long as the child lives.
fn greet_hatch(stream: &mut ral_core::wire::WireStream, token: u64) -> Result<(), String> {
    use std::io::{Read, Write};
    let patience = ral_core::transport::Liveness::default().deadline;
    stream
        .set_write_timeout(Some(patience))
        .and_then(|()| stream.set_read_timeout(Some(patience)))
        .map_err(|e| format!("could not put the hatch deadline on the dialled wire: {e}"))?;
    stream
        .write_all(&token.to_le_bytes())
        .map_err(|e| format!("could not send the hatch token to the guest's listener: {e}"))?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            "the guest closed the connection before acknowledging the hatch".to_string()
        }
        _ => format!("could not read the guest's hatch acknowledgement: {e}"),
    })?;
    if ack[0] != ral_core::transport::HATCH_ACK {
        return Err(format!(
            "the guest answered the hatch with byte {:#04x} rather than the acknowledgement — \
             whatever is listening on that port is not a ral engine waiting to be hatched",
            ack[0]
        ));
    }
    stream
        .set_read_timeout(None)
        .map_err(|e| format!("could not lift the hatch deadline from the wire: {e}"))
}

/// The acts a desk verb can commit. One vocabulary for two readers: the rail's
/// `verb` column and the audit prose an unwind owes the model are both derived
/// from it, so a new act cannot reach one and miss the other.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum DeskAct {
    Spawn,
    Cancel,
    Message,
    Schedule,
    Unschedule,
    Reply,
    ContextDrop,
    ContextFold,
}

impl DeskAct {
    /// The name the rail draws in its verb column.
    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Cancel => "cancel",
            Self::Message => "message",
            Self::Schedule => "schedule",
            Self::Unschedule => "unschedule",
            Self::Reply => "reply",
            Self::ContextDrop => "context-drop",
            Self::ContextFold => "context-fold",
        }
    }

    /// The inverse of [`Self::verb`]: every string this fragment ever carries
    /// was minted from it, so this never meets a name it does not know.
    fn from_verb(verb: &str) -> Self {
        match verb {
            "spawn" => Self::Spawn,
            "cancel" => Self::Cancel,
            "message" => Self::Message,
            "schedule" => Self::Schedule,
            "unschedule" => Self::Unschedule,
            "reply" => Self::Reply,
            "context-drop" => Self::ContextDrop,
            "context-fold" => Self::ContextFold,
            other => unreachable!("desk act fragment carries an unknown verb `{other}`"),
        }
    }

    /// What the act did, past tense, for an audit read after the fact.
    fn done(self, subject: Option<&str>) -> String {
        let named = |what: &str| match subject {
            Some(s) => format!("{what} '{s}'"),
            None => what.to_string(),
        };
        match self {
            Self::Spawn => named("started agent"),
            Self::Cancel => named("cancelled agent"),
            Self::Message => named("delivered a message to agent"),
            Self::Schedule => named("armed the wakeup"),
            Self::Unschedule => named("removed the wakeup"),
            // The one act with no addressee: a returning agent replies to its
            // parent and to nobody else.
            Self::Reply => "staged your reply".to_string(),
            Self::ContextDrop => named("dropped context"),
            Self::ContextFold => named("folded context"),
        }
    }
}

/// What this `ral` call has committed, in the order it landed: one
/// [`Observation`] per attempt that landed, minted at [`HostServices::commit_act`]
/// — the door every acting handler funnels through — and read back once a
/// raise discards the call's bindings but not its acts.
///
/// Effects persist across an unwind: a call the wall cut short still started
/// what it started and delivered what it delivered, while every binding it made
/// is gone. The fragment is how the raise says so, since the model's only other
/// in-band signal — the diagnostic — speaks of the failure and not of the acts.
///
/// Only committed acts are recorded. A refused act changed nothing, so it
/// leaves no entry: this fragment answers *what stands*.
#[derive(Clone, Default)]
pub(crate) struct ActFragment(Arc<Mutex<Vec<Observation>>>);

impl ActFragment {
    /// A fragment seeded with `acts` directly, bypassing `commit_act` — for a
    /// renderer test that needs a settled fragment with no live desk behind it.
    #[cfg(test)]
    pub(crate) fn from_acts(acts: Vec<Observation>) -> Self {
        Self(Arc::new(Mutex::new(acts)))
    }

    /// Never waits: the only thread that could hold this guard is the asker —
    /// a desk handler runs on the attend thread parked in `run_shell`, and the
    /// audit is read on that same thread once the run is back.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Observation>> {
        match self.0.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => panic!(
                "act fragment contended: a desk handler may only run while the attend thread is \
                 parked in run_shell"
            ),
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("act fragment poisoned"),
        }
    }

    /// The sentence an unwind owes the model, or `None` when this call committed
    /// nothing at all — where silence is the whole truth.
    pub(crate) fn audit(&self) -> Option<String> {
        let done = {
            let acts = self.lock();
            if acts.is_empty() {
                return None;
            }
            acts.iter()
                .filter_map(|obs| match &obs.what {
                    Observed::Act { verb, subject, .. } => {
                        Some(DeskAct::from_verb(verb).done(subject.as_deref()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        Some(format!(
            "audit: this call had already {done}; that work stands — do not repeat it.\n"
        ))
    }
}

/// Everything a desk handler may read off `&Avatar`, snapshotted fresh at every
/// [`crate::agent::Avatar::run_shell`] install so no capture goes stale mid-call.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent axis captured off the agent (interactive, returns, allow_schedule, search); not a candidate for a combined enum"
)]
pub(crate) struct HostServices {
    pub registry: AgentRegistry,
    /// `None` on a wire seat, whose real scratch lives in the guest it dials.
    pub scratch: Option<Arc<crate::bootstrap::Scratch>>,
    /// This run's own agent: parent of what it spawns, root of the descendant
    /// check `` `message ``/`` `cancel `` enforce.
    pub parent: AgentId,
    pub mailbox: Mailbox,
    pub emit: Emitter,
    pub provider: ProviderHandle,
    /// The ceiling [`crate::policy::narrow`] attenuates a `grant` against.
    pub caps: Capabilities,
    /// Frozen at install: the path root `narrow` and every handler resolves from.
    pub cwd: PathBuf,
    /// Immutable for this run's extent; `` `start `` refuses once it reads zero.
    pub fuel: u32,
    /// Whether this agent holds `reply`; the desk's refusal keys on this bit,
    /// never on trunk-ness.
    pub returns: bool,
    pub allow_schedule: bool,
    /// A ceiling like `caps`: a spawn may narrow this bit, never widen it.
    pub search: bool,
    /// Where the `reply` handler stages its value, holding no `&mut Avatar` to
    /// write it any other way.
    pub reply: ReplyCell,
    pub schedules: ScheduleRegistry,
    /// A `mnemon` spawn forks its inherited context off this.
    pub log: LogCell,
    /// Still carrying [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`]: the index is
    /// filtered per agent, so only an unresolved template is shareable.
    pub system_template: String,
    /// Resolves `system_template` with no live `Shell` at the desk.
    pub index: std::sync::Arc<crate::prompt::BuiltinIndex>,
    pub interactive: bool,
    /// The adoption end of the body's own [`ral_core::Shell::fork_into_nursery`];
    /// `` `start `` redeems a [`NurseryId`] here.
    pub nursery: Nursery,
    /// The calling agent's own generation, read at install, so a desk older
    /// than *its* `/clear` refuses to spawn.
    pub generation: u64,
    pub disk_warn_bytes: Option<u64>,
    /// A spawn shares its parent's egress policy, never a fresh one.
    pub egress: egress::Egress,
    /// What this call has committed so far: minted per `ral` call, and read
    /// back by [`crate::shell_eval::report::render`] when a raise discards
    /// the bindings but not the acts.
    pub acts: ActFragment,
    /// Who the acts are committed on behalf of, read once at install: the
    /// desk holds no `Shell` to ask, and a host act's principal is the host's.
    /// `None` where the host names nobody, the same fact `Context::principal`
    /// reports for an unbound `$USER` in the same record.
    pub principal: Option<String>,
    /// The agent's own pin register, shared verbatim so `pin-read`/`pin-list`
    /// answer from the same mirror the nudge already reads. `None` on a seat
    /// with no mirror of its own — the wire test seat above all.
    pub pins: Option<PinDigests>,
    /// Stated, never inferred from `scratch`'s incidental absence:
    /// `` `start `` chooses its identity or wire arm on this one fact.
    pub wire_seat: bool,
    /// The dial-side capability a wire trunk's `` `start `` reaches its
    /// child's listener through; `None` on every identity trunk.
    pub dial: Option<Arc<dyn crate::agent::Dial>>,
}

impl HostServices {
    /// The one door every acting handler commits an attempt through: one
    /// [`Observation`] built at the arm where the outcome is known, fanned to
    /// both its readers off that single datum — the rail row always, the
    /// fragment only when `refused` is false. A verb that reaches here cannot
    /// draw one reader's row and miss the other's, since there is only the
    /// one construction either draws from.
    fn commit_act(&self, act: DeskAct, subject: Option<&str>, payload: String, refused: bool) {
        let obs = Observation::instant(
            CallSite::default(),
            self.principal.clone(),
            Observed::Act {
                verb: act.verb().to_string(),
                subject: subject.map(str::to_string),
                payload: payload.clone(),
                refused,
            },
        );
        self.record_display(crate::record::Display::HarnessCall {
            verb: act.verb().to_string(),
            subject: subject.map(str::to_string),
            payload,
            failed: refused,
        });
        if !refused {
            self.acts.lock().push(obs);
        }
    }

    /// Author one display commit through the session's record seam.  A desk
    /// handler runs while the attend thread is parked in `run_shell`, so the
    /// log cell is free to lend the seam; the append failure a handler cannot
    /// propagate surfaces as its own error row instead of a shrug.
    fn record_display(&self, commit: crate::record::Display) {
        let recorder = self.log.lock().record_emitter();
        if let Err(error) = recorder.emit(commit) {
            recorder.report_fault(&error);
        }
    }

    /// [`Self::record_display`] for the forensic class — the harness-result
    /// breadcrumbs that pair with an act's row.
    fn record_forensic(&self, fact: crate::record::Forensic) {
        let recorder = self.log.lock().record_emitter();
        if let Err(error) = recorder.emit(fact) {
            recorder.report_fault(&error);
        }
    }
}

/// Answers one [`FOValue`] enquiry against a captured [`HostServices`]. Fresh
/// per `ral` call; wrapped in [`DeskBinding`] for the identity transport and
/// passed bare to `shell_eval::run_shell` for the wire path.
pub(crate) struct ExarchDesk {
    pub(crate) services: HostServices,
}

/// Whole seconds, saturating: nothing this desk answers comes near `i64::MAX`,
/// and saturation stays total where an `as` cast would wrap in silence.
fn secs_to_i64(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// Decode a payload as an `N`-element list, or a didactic error naming the
/// shape. The builtin's door checked already; the desk trusts nothing that
/// crossed the wire.
fn payload_list<const N: usize>(
    payload: Option<Box<FOValue>>,
    class: &str,
    shape: &str,
) -> Result<[FOValue; N], Error> {
    let Some(payload) = payload else {
        return Err(Error::new(
            format!("`{class}` requires a payload {shape}"),
            1,
        ));
    };
    let FOValue::List { items } = *payload else {
        return Err(Error::new(
            format!("`{class}` payload must be a list {shape}"),
            1,
        ));
    };
    <[FOValue; N]>::try_from(items).map_err(|items| {
        Error::new(
            format!(
                "`{class}` payload must have exactly {N} element(s) {shape}, got {}",
                items.len()
            ),
            1,
        )
    })
}

fn payload_int(v: FOValue, class: &str, field: &str) -> Result<i64, Error> {
    match v {
        FOValue::Int { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be an Int, got {}", other.shape()),
            1,
        )),
    }
}

fn payload_exchange(v: FOValue, class: &str, field: &str) -> Result<u64, Error> {
    let value = payload_int(v, class, field)?;
    u64::try_from(value).map_err(|_| {
        Error::new(
            format!("`{class}`: `{field}` must be a non-negative exchange number"),
            1,
        )
    })
}

/// The exchange list *is* the payload, not an argument wrapping one.
fn payload_exchanges(payload: Option<Box<FOValue>>, class: &str) -> Result<Vec<u64>, Error> {
    let Some(FOValue::List { items }) = payload.map(|payload| *payload) else {
        return Err(Error::new(
            format!("`{class}`: `exchanges` must be a list of non-negative Ints"),
            1,
        ));
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| payload_exchange(item, class, &format!("exchanges[{index}]")))
        .collect()
}

/// The rail subject `` `context-read ``/`` `context-drop `` both mint from an
/// exchange list: `"exchanges [1, 2, 3]"`.
fn exchanges_subject(exchanges: &[u64]) -> String {
    format!(
        "exchanges [{}]",
        exchanges
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn payload_string(v: FOValue, class: &str, field: &str) -> Result<String, Error> {
    match v {
        FOValue::String { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be a Str, got {}", other.shape()),
            1,
        )),
    }
}

fn payload_tag(v: FOValue, class: &str, field: &str) -> Result<String, Error> {
    match v {
        FOValue::Variant {
            label,
            payload: None,
        } => Ok(label),
        other => Err(Error::new(
            format!(
                "`{class}`: `{field}` must be a bare variant tag, got {}",
                other.shape()
            ),
            1,
        )),
    }
}

fn payload_bool(v: FOValue, class: &str, field: &str) -> Result<bool, Error> {
    match v {
        FOValue::Bool { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `{field}` must be a Bool, got {}", other.shape()),
            1,
        )),
    }
}

/// Split a family's payload into the tag it names and that tag's own payload.
fn family_tag(
    payload: Option<Box<FOValue>>,
    family: &str,
) -> Result<(String, Option<Box<FOValue>>), Error> {
    match payload.map(|payload| *payload) {
        Some(FOValue::Variant { label, payload }) => Ok((label, payload)),
        other => Err(Error::new(
            format!(
                "`{family}` requires a tag naming what to do, got {}",
                other.map_or_else(|| "no payload at all".to_string(), |v| v.shape())
            ),
            1,
        )),
    }
}

/// A tag this desk does not answer. Tags extend a family the way classes
/// extend the desk, so an unknown one is as loud one level down as it is at
/// the top — never a silent default.
fn unknown_tag(family: &str, tag: &str) -> Error {
    Error::new(format!("unrecognised `{family} tag `{tag}`"), 1)
}

/// A tag whose whole payload is one bare value rather than a record.
fn payload_value(
    payload: Option<Box<FOValue>>,
    class: &str,
    shape: &str,
) -> Result<FOValue, Error> {
    payload.map_or_else(
        || {
            Err(Error::new(
                format!("`{class}` requires a payload {shape}"),
                1,
            ))
        },
        |payload| Ok(*payload),
    )
}

/// One payload read by field name — the dual of the builtin door's `spec.get`
/// arms. A field the desk does not need is ignored rather than refused: the
/// surface's closed record types already fix the shape.
struct Fields<'a> {
    class: &'a str,
    entries: Vec<(String, FOValue)>,
}

impl<'a> Fields<'a> {
    /// The tag's whole payload, as a record.
    fn payload(payload: Option<Box<FOValue>>, class: &'a str, shape: &str) -> Result<Self, Error> {
        Self::of(
            payload_value(payload, class, shape)?,
            class,
            "the payload",
            shape,
        )
    }

    /// A named field that is itself a record — `` `start ``'s `spec`.
    fn of(v: FOValue, class: &'a str, subject: &str, shape: &str) -> Result<Self, Error> {
        match v {
            FOValue::Map { entries } => Ok(Self { class, entries }),
            other => Err(Error::new(
                format!(
                    "`{class}`: {subject} must be a record {shape}, got {}",
                    other.shape()
                ),
                1,
            )),
        }
    }

    /// Take one field by name; a missing one is told what it was for.
    fn take(&mut self, field: &str, purpose: &str) -> Result<FOValue, Error> {
        let at = self
            .entries
            .iter()
            .position(|(key, _)| key == field)
            .ok_or_else(|| {
                Error::new(
                    format!(
                        "`{}`: the record needs a `{field}` field — {purpose}",
                        self.class
                    ),
                    1,
                )
            })?;
        Ok(self.entries.swap_remove(at).1)
    }
}

/// `` `start ``'s `fork` field: the one part of that payload no model wrote,
/// minted engine-side because the reentrancy law bars a desk handler from
/// holding the `&mut Shell` a fork needs. Which tag is legal is the *host's*
/// fact, not the guest's — see [`ExarchDesk::launch`].
enum ForkClaim {
    /// The fork is parked in this host's own nursery, under this id.
    Parked(NurseryId),
    /// The fork's engine holds a listener on `port`, and will hand its
    /// connection to a child only for a dial that writes `token` first.
    Listening { port: u32, token: u64 },
}

fn payload_fork(v: FOValue, class: &str) -> Result<ForkClaim, Error> {
    match v {
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "parked" => {
            let id = payload_int(*payload, class, "fork")?;
            let id = u64::try_from(id).map_err(|_| {
                Error::new(
                    format!("`{class}`: `fork `parked` carries a nursery id, never a negative Int"),
                    1,
                )
            })?;
            Ok(ForkClaim::Parked(NurseryId(id)))
        }
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "listening" => {
            let mut fields = Fields::of(*payload, class, "`fork `listening`", "[port, token]")?;
            let port = payload_int(
                fields.take("port", "the guest port the host must dial")?,
                class,
                "port",
            )?;
            let port = u32::try_from(port).map_err(|_| {
                Error::new(
                    format!("`{class}`: `fork `listening`'s port must be a bound vsock port"),
                    1,
                )
            })?;
            // Bit-preserving: the eight bytes the host writes are the eight
            // the listener compares, never arithmetic on them.
            let token = payload_int(
                fields.take("token", "the eight bytes the guest's listener expects")?,
                class,
                "token",
            )?
            .cast_unsigned();
            Ok(ForkClaim::Listening { port, token })
        }
        other => Err(Error::new(
            format!(
                "`{class}`: `fork` must be `parked <nursery id>` or `listening [port, token]`, \
                 got {}",
                other.shape()
            ),
            1,
        )),
    }
}

/// Decode an `` `add `` trigger into a live [`Trigger`], re-running the parsers
/// the builtin's door already ran: that check is never the only line of defence.
fn payload_trigger(v: FOValue, class: &str) -> Result<Trigger, Error> {
    match v {
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "cron" => {
            let expr = payload_string(*payload, class, "trigger")?;
            let schedule =
                CronSchedule::parse(&expr).map_err(|e| Error::new(format!("`{class}`: {e}"), 1))?;
            Ok(Trigger::Cron { schedule, expr })
        }
        FOValue::Variant {
            label,
            payload: Some(payload),
        } if label == "after" => {
            let dur = payload_string(*payload, class, "trigger")?;
            let d = parse_duration(&dur).map_err(|e| Error::new(format!("`{class}`: {e}"), 1))?;
            Ok(Trigger::After(d))
        }
        other => Err(Error::new(
            format!(
                "`{class}`: `trigger` must be `cron <expr>` or `after <dur>`, got {}",
                other.shape()
            ),
            1,
        )),
    }
}

/// Decode an `` `add `` payload's `label`: a required `Str`, since every
/// caller now names its own schedule.
fn payload_label(v: FOValue, class: &str) -> Result<String, Error> {
    match v {
        FOValue::String { value } => Ok(value),
        other => Err(Error::new(
            format!("`{class}`: `label` must be a Str, got {}", other.shape()),
            1,
        )),
    }
}

/// All that varies across `` `start ``'s two spawn kinds (`amnemon`/`mnemon`,
/// the `agent` builtin's `type`); every other step of the spawn spine is
/// identical for both and lives once in [`ExarchDesk::launch`].
struct Launch<'a> {
    /// How the builtin body left its fork for this desk to reach.
    fork: ForkClaim,
    grant: &'a str,
    /// Import the parent's model-visible conversation — a `mnemon` spawn only.
    inherit_context: bool,
    /// Refused if any live agent already bears it.
    name: String,
    prompt: String,
    /// The caller's request, clamped rather than taken at face value.
    search: bool,
}

impl ExarchDesk {
    /// Decode one enquiry class and answer it. An unrecognised class draws the
    /// extension law's error — never a silent default.
    ///
    /// # Errors
    /// Returns `Err` if `req` is not a [`FOValue::Variant`], names a class this
    /// desk does not answer, or the named class itself refuses.
    pub(crate) fn handle(&self, req: FOValue) -> Result<FOValue, Error> {
        let FOValue::Variant { label, payload } = req else {
            return Err(Error::new(
                format!(
                    "enquiry request must be a variant naming its class, got {}",
                    req.shape()
                ),
                1,
            ));
        };
        match label.as_str() {
            "agents" => self.agents(payload),
            "schedules" => self.schedules(payload),
            "pin-read" => self.pin_read(payload),
            "pin-list" => Ok(self.pin_list()),
            "context" => Ok(self.context()),
            "context-read" => self.context_read(payload),
            "context-drop" => self.context_drop(payload),
            "context-fold" => self.context_fold(payload),
            other => Err(Error::new(
                format!("unrecognised enquiry class `{other}`"),
                1,
            )),
        }
    }

    /// `` `agents `` — the agent registry, one class for the whole family.
    /// Every tag answers the roster, both spawn arms included: a wire spawn
    /// does not answer until its child exists, so it has one to be listed on.
    fn agents(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let (tag, payload) = family_tag(payload, "agents")?;
        match tag.as_str() {
            "list" => Ok(self.roster()),
            "start" => self.agent_start(payload),
            "message" => self.message(payload),
            "cancel" => self.agent_cancel(payload),
            "reply" => self.agent_reply(payload),
            "read" => self.agent_read(payload),
            other => Err(unknown_tag("agents", other)),
        }
    }

    /// `` `schedules `` — the wakeup table, one class for the whole family.
    /// Every tag answers the table; the self-wakeup grant gates all three,
    /// each naming the tag the model typed.
    fn schedules(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let (tag, payload) = family_tag(payload, "schedules")?;
        match tag.as_str() {
            "list" => {
                self.require_schedule_grant("schedules `list")?;
                Ok(self.schedule_table())
            }
            "add" => self.schedule(payload),
            "remove" => self.unschedule(payload),
            other => Err(unknown_tag("schedules", other)),
        }
    }

    /// The spawn spine behind `` `start ``: adopt the parked fork, narrow its
    /// authority, fork its log off the parent's, assemble it at one less unit of
    /// fuel, hand it to `spawn_async`. Every cheap guard runs before
    /// [`Nursery::adopt`]; a refusal after it simply drops the adopted `Shell`.
    fn launch(&self, spec: Launch) -> Result<FOValue, Error> {
        let s = &self.services;

        // The captured fuel, caps, and grant are only as fresh as the
        // generation they were snapshotted under — this caller's own, so a
        // `/clear` in another tab does not refuse a spawn here.
        if s.generation != s.registry.generation(s.parent) {
            return Err(Error::new(
                "`agents `start` refused: the agent tree was cleared while this call was still in \
                 flight, so the fuel and permissions snapshot it captured are now stale — \
                 issue agent again on your next turn",
                1,
            ));
        }

        // Fuel bounds depth, not fan-out: the parent's own is never debited, so
        // siblings are free and only a deep enough chain bottoms out.
        if s.fuel == 0 {
            return Err(Error::new(
                "`agents `start` refused: no spawn fuel remains at this depth, so you cannot \
                 delegate any further here. Fuel bounds how deep a chain of spawns may \
                 recurse, never how many children you may start at any one depth — starting \
                 several agents here costs nothing extra. `agents `cancel` on any node stops \
                 its whole live subtree regardless of depth.",
                1,
            ));
        }

        // Before a log is forked or a listener dialled: the in-process door
        // checked this name, but a wire peer need not have come through one,
        // and `register` — the authority — only refuses after all that work.
        if let Err(why) = check_name(&spec.name) {
            return Err(Error::new(format!("`agents `start` refused: {why}"), 1));
        }

        // Didactic, not race-free: `register` below re-checks under its own
        // lock, and that is what closes a same-name race.
        if s.registry.name_live(&spec.name) {
            return Err(Error::new(
                format!(
                    "`agents `start` refused: a live agent already bears the name '{}' — pick \
                     another, or wait for it to settle. Names identify live agents; agents \
                     lists yours.",
                    spec.name
                ),
                1,
            ));
        }

        if s.wire_seat {
            return self.launch_wire(spec);
        }

        let ForkClaim::Parked(session) = spec.fork else {
            return Err(Error::new(
                "`agents `start` refused: this host runs its children in its own process, so the \
                 fork must be `parked <nursery id>` — a session that says it is listening for a \
                 dial is describing a wire this desk does not have",
                1,
            ));
        };
        let Some(shell) = s.nursery.adopt(session) else {
            return Err(Error::new(
                "`agents `start`: no forked session parked under this id — it may already have \
                 started, or the run that forked it has ended",
                1,
            ));
        };

        let (child_caps, child_log, system_prompt) = self.fork_child(&spec)?;

        // Mirrors `Avatar::fork_with`'s `Build` literal, with the adopted shell
        // and forked log standing in for `fork_session`'s fresh ones.
        let fuel = s.fuel - 1;
        // Unreachable: an identity seat always carries its own scratch.
        let Some(scratch) = s.scratch.clone() else {
            return Err(Error::new(
                "`agents `start` refused: this session has no host-side scratch to fork an \
                 in-process child into",
                1,
            ));
        };
        // No detach: the shell was forked, not booted, so it carries no policy.
        let seat =
            crate::agent::seat::Seat::identity(shell, scratch, s.cwd.clone(), false, &child_log);
        // Registered so a terminate-class cancel can unwind a `ral` eval
        // already in flight, and a per-tab interrupt can unwind just the
        // in-flight exchange without touching the root a later exchange
        // would inherit.
        let reach = seat.eval_reach();
        let child = Avatar::assemble(Build {
            name: spec.name.clone(),
            system: s.system_template.clone(),
            system_prompt,
            index: s.index.clone(),
            caps: child_caps,
            seat,
            log: child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tool_enabled: true,
            // A child may narrow its parent's search reach, never widen it.
            search: s.search && spec.search,
            agents: s.registry.clone(),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes: s.disk_warn_bytes,
            egress: s.egress.clone(),
            dial: s.dial.clone(),
            reach,
            // `s.generation` was just re-verified fresh against the live
            // registry above, on this same parent's own attend thread — the
            // only thread that could ever bump it.
            consumer: s.generation,
        });

        self.spawn_child(child, spec.name, spec.prompt)
    }

    /// The child-log/capability half of the spawn spine, shared by both arms:
    /// an identity spawn runs it once it has adopted its shell, a wire spawn
    /// before it dials, since the host has no shell of its own to gate on.
    fn fork_child(
        &self,
        spec: &Launch,
    ) -> Result<(Capabilities, crate::agent::event::AgentLog, String), Error> {
        let s = &self.services;
        // `narrow` names all six legal bases in its own diagnostic, so an
        // unknown `grant` needs no refusal text here.
        let cwd = s.cwd.to_string_lossy();
        let child_caps = crate::policy::narrow(&s.caps, spec.grant, &cwd)
            .map_err(|reason| Error::new(reason, 1))?;

        // On the raw `AgentLog`, not through `Avatar::inherit_context`, which
        // needs a `&Self` the child is not yet. The index resolves here, off the
        // same bits the `Build` below fixes, so the opening bookend records the
        // child's real system length rather than the template's.
        let child_id = crate::agent::fresh_id();
        let system_prompt = s.index.apply(
            &s.system_template,
            &crate::prompt::Grants {
                returns: true,
                allow_schedule: s.allow_schedule,
                spawns: s.fuel.saturating_sub(1) > 0,
            },
        );
        let child_log = {
            let parent_log = s.log.lock();
            let mut child_log = parent_log
                .fork(child_id, system_prompt.len())
                .map_err(|e| Error::new(format!("could not fork child session log: {e}"), 1))?;
            let inherited = spec
                .inherit_context
                .then(|| parent_log.inherited_context_messages());
            drop(parent_log);
            if let Some(messages) = inherited {
                child_log
                    .import_context(messages)
                    .map_err(|e| Error::new(e, 1))?;
            }
            child_log
        };
        Ok((child_caps, child_log, system_prompt))
    }

    /// `` `start ``'s wire arm: nothing is parked host-side, so the host
    /// dials the listener the guest opened for this one spawn, writes the
    /// token that listener is waiting for, and does not answer until the
    /// guest acknowledges — which it does only once the child process
    /// exists. The answer is the roster the identity arm gives; the builtin
    /// cannot tell which served it.
    fn launch_wire(&self, spec: Launch) -> Result<FOValue, Error> {
        let s = &self.services;
        let ForkClaim::Listening { port, token } = spec.fork else {
            return Err(Error::new(
                "`agents `start` refused: this host reaches its children across a wire, so the \
                 fork must be `listening [port, token]` — a session that says it is parked in \
                 process is naming a nursery this desk does not have",
                1,
            ));
        };
        let Some(dial) = s.dial.clone() else {
            return Err(Error::new(
                "`agents `start` refused: this wire session has no dialler installed to reach a \
                 helper engine's listener — a construction bug, since a fuelled wire trunk is \
                 refused at Avatar::root without one",
                1,
            ));
        };
        let (child_caps, child_log, system_prompt) = self.fork_child(&spec)?;

        let mut stream = dial.dial(port).map_err(|reason| {
            Error::new(
                format!(
                    "`agents `start` refused: could not dial the helper engine's listener on \
                     guest port {port} — {reason}"
                ),
                1,
            )
        })?;
        greet_hatch(&mut stream, token)
            .map_err(|reason| Error::new(format!("`agents `start` refused: {reason}"), 1))?;

        // Past the ack the child is alive, so a refusal from here on simply
        // drops the stream: the child reads EOF on fd 3 and the guest's own
        // table reaps it. Nothing to kill, and nothing to tell it.
        let transport = ral_core::transport::WireTransport::adopt(
            stream,
            ral_core::transport::Liveness::default(),
        )
        .map_err(|e| {
            Error::new(
                format!("`agents `start`: could not adopt the hatched wire: {e}"),
                1,
            )
        })?;
        // The same two paths synod's own trunk seat uses.
        let seat = crate::agent::seat::Seat::wire(
            transport,
            PathBuf::from(HATCHED_CWD),
            PathBuf::from(HATCHED_HOME),
        );

        let fuel = s.fuel - 1;
        let reach = seat.eval_reach();
        let child = Avatar::assemble(Build {
            name: spec.name.clone(),
            system: s.system_template.clone(),
            system_prompt,
            index: s.index.clone(),
            caps: child_caps,
            seat,
            log: child_log,
            parent: Some(s.parent),
            fuel,
            provider: ProviderHandle::new(s.provider.current()),
            interactive: s.interactive,
            returns: true,
            allow_schedule: s.allow_schedule,
            tool_enabled: true,
            search: s.search && spec.search,
            agents: s.registry.clone(),
            run_lock: None,
            resume_summary: None,
            disk_warn_bytes: s.disk_warn_bytes,
            egress: s.egress.clone(),
            dial: Some(dial),
            reach,
            // Re-verified fresh against the live registry in `launch`, before
            // this wire arm was ever reached — see its own comment.
            consumer: s.generation,
        });

        self.spawn_child(child, spec.name, spec.prompt)
    }

    /// Hand `child` to `spawn_async` and answer the roster it now appears in —
    /// the state, not a receipt, and the same answer from either arm.
    fn spawn_child(&self, child: Avatar, name: String, prompt: String) -> Result<FOValue, Error> {
        let s = &self.services;
        // Held past the move, so the commitment arm can still name the act.
        let acted_name = name.clone();
        let acted_prompt = prompt.clone();
        let spawned = crate::shell_eval::tools::agent::spawn_async(
            &s.registry,
            s.parent,
            s.mailbox.clone(),
            child,
            crate::shell_eval::tools::agent::AsyncSpawn {
                name,
                prompt: Some(prompt),
            },
            &s.emit,
        );
        s.commit_act(
            DeskAct::Spawn,
            Some(&acted_name),
            acted_prompt,
            spawned.is_err(),
        );
        match spawned {
            Ok(_) => Ok(self.roster()),
            Err(reason) => Err(Error::new(reason, 1)),
        }
    }

    /// `` `start `` — the desk half of the `agents` builtin. The `spec` record
    /// is the model's own, read field by field; `fork` is the engine's.
    fn agent_start(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        const CLASS: &str = "agents `start";
        let mut req = Fields::payload(payload, CLASS, "[spec: …, fork: …]")?;
        let mut spec = Fields::of(
            req.take("spec", "the model's own spawn record")?,
            CLASS,
            "`spec`",
            "[prompt: …, name: …, type: …, grant: …, search: …]",
        )?;
        let fork = payload_fork(
            req.take("fork", "how this spawn's forked session is to be reached")?,
            CLASS,
        )?;

        let prompt = payload_string(
            spec.take("prompt", "the instruction the child starts with")?,
            CLASS,
            "prompt",
        )?;
        let name = payload_string(spec.take("name", "the child's identity")?, CLASS, "name")?;
        let kind = payload_tag(spec.take("type", "`amnemon or `mnemon")?, CLASS, "type")?;
        let mnemon = match kind.as_str() {
            "amnemon" => false,
            "mnemon" => true,
            other => {
                return Err(Error::new(
                    format!(
                        "`{CLASS}`: `type` must be `amnemon` (blank context) or `mnemon` \
                         (inherits your conversation), got `{other}`"
                    ),
                    1,
                ));
            }
        };
        let grant = payload_tag(
            spec.take("grant", "one of the five permission bases")?,
            CLASS,
            "grant",
        )?;
        let search = payload_bool(
            spec.take(
                "search",
                "whether the child may use the provider's built-in web search",
            )?,
            CLASS,
            "search",
        )?;

        self.launch(Launch {
            fork,
            grant: &grant,
            inherit_context: mnemon,
            name,
            prompt,
            search,
        })
    }

    /// The registry listing scoped to this agent's descendants, tagged
    /// `` `roster `` — the answer to every `` `agents `` tag there is.
    fn roster(&self) -> FOValue {
        let s = &self.services;
        let rows = FOValue::List {
            items: s
                .registry
                .list(s.parent)
                .into_iter()
                .map(|a| {
                    let elapsed_s = secs_to_i64(a.elapsed);
                    let idle_s = secs_to_i64(a.idle);
                    FOValue::Map {
                        entries: vec![
                            ("name".to_string(), FOValue::String { value: a.name }),
                            (
                                "state".to_string(),
                                FOValue::Variant {
                                    label: a.state.tag().to_string(),
                                    payload: None,
                                },
                            ),
                            ("idle-s".to_string(), FOValue::Int { value: idle_s }),
                            ("elapsed-s".to_string(), FOValue::Int { value: elapsed_s }),
                            (
                                "log-dir".to_string(),
                                FOValue::String {
                                    value: a.log_dir.display().to_string(),
                                },
                            ),
                        ],
                    }
                })
                .collect(),
        };
        FOValue::Variant {
            label: "roster".to_string(),
            payload: Some(Box::new(rows)),
        }
    }

    /// `` `cancel `` — resolve a live descendant by name and cancel it, scoped
    /// as [`AgentRegistry::cancel_scoped`] enforces. A real cancel and a miss
    /// are both successful calls answering the roster; a name gone from it is
    /// the cancel, and only a scope violation raises.
    fn agent_cancel(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        const CLASS: &str = "agents `cancel";
        let s = &self.services;
        let name = payload_value(payload, CLASS, "naming the descendant to cancel")?;
        let name = payload_string(name, CLASS, "name")?;

        // The row is derived after the call: one claiming "cancelled" ahead of
        // it would assert an effect the world never saw. `cancel` takes no
        // argument, so its payload column carries the outcome instead.
        let cancelled = s
            .registry
            .resolve_name(&name)
            .map_or(Ok(false), |id| s.registry.cancel_scoped(s.parent, id));
        let landed = matches!(cancelled, Ok(true));
        let (payload, content, refused) = match cancelled {
            Ok(true) => (String::new(), format!("cancelling agent '{name}'"), false),
            Ok(false) => (
                "no live agent by that name".to_string(),
                format!("no live agent named '{name}'"),
                false,
            ),
            Err(NotADescendant(_)) => (
                "refused: not a descendant".to_string(),
                format!(
                    "agent '{name}' is not an agent you started; `agents `cancel` may only reach a descendant of yours"
                ),
                true,
            ),
        };
        s.commit_act(DeskAct::Cancel, Some(&name), payload, !landed);
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        // The raise is the model's only copy of a scope violation.
        if refused {
            Err(Error::new(content, 1))
        } else {
            Ok(self.roster())
        }
    }

    /// `` `message `` — resolve a live descendant by name and send it a note.
    fn message(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        const CLASS: &str = "agents `message";
        let s = &self.services;
        let mut spec = Fields::payload(payload, CLASS, "[to: …, text: …]")?;
        let name = payload_string(spec.take("to", "the descendant's name")?, CLASS, "to")?;
        let text = payload_string(spec.take("text", "what to send")?, CLASS, "text")?;

        // Unlike `cancel`, an unresolved name refuses rather than no-ops:
        // `message` promises delivery and there is nothing to deliver to. A
        // target settling between the name resolving and the send lands in the
        // same arm, one step later. The row goes up after the send, since a
        // delivery's payload is its text but a refusal's is why.
        let sent = text.clone();
        let (payload, content, ok) = match s
            .registry
            .resolve_name(&name)
            .map(|to| s.registry.message(s.parent, to, text))
        {
            None | Some(Err(MessageError::UnknownRecipient(_))) => (
                "refused: no live agent by that name".to_string(),
                format!("no live agent named '{name}'; did it finish already?"),
                false,
            ),
            Some(Ok(())) => (sent, format!("sent message to agent '{name}'"), true),
            Some(Err(MessageError::UnknownSender(n))) => (
                "refused: sender is no longer live".to_string(),
                format!("cannot send from agent {n}: it is no longer live"),
                false,
            ),
            Some(Err(MessageError::NotADescendant(_))) => (
                "refused: not a descendant".to_string(),
                format!(
                    "agent '{name}' is not an agent you started; `agents `message` may only reach a descendant of yours"
                ),
                false,
            ),
            Some(Err(MessageError::RecipientInboxFull(_, reject))) => (
                format!("refused: {reject}"),
                format!("agent '{name}' did not receive the message: {reject}"),
                false,
            ),
        };
        s.commit_act(DeskAct::Message, Some(&name), payload, !ok);
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        // A delivery answers the roster like every other tag; a refusal raises,
        // and the raise is the model's only copy of it.
        if ok {
            Ok(self.roster())
        } else {
            Err(Error::new(content, 1))
        }
    }

    /// The self-wakeup guard the whole schedule family runs first. It is
    /// handed the tag the model typed, so the refusal names that and not a
    /// vocabulary the model was never taught.
    fn require_schedule_grant(&self, verb: &str) -> Result<(), Error> {
        if self.services.allow_schedule {
            return Ok(());
        }
        Err(Error::new(
            format!(
                "`{verb}` refused: this agent does not hold the self-wakeup grant. An agent \
                 that can wake itself indefinitely holds real authority, so the grant is off \
                 by default; relaunch with `--allow-schedule` if this session genuinely needs \
                 to schedule its own wakeups."
            ),
            1,
        ))
    }

    /// `` `add `` — arm a self-wakeup through [`ScheduleRegistry::schedule`].
    /// The answer is the table it now appears in; its `next-s` column already
    /// says everything a receipt could.
    fn schedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        const CLASS: &str = "schedules `add";
        self.require_schedule_grant(CLASS)?;
        let s = &self.services;

        let mut spec = Fields::payload(payload, CLASS, "[trigger: …, label: …, prompt: …]")?;
        let trigger = payload_trigger(
            spec.take("trigger", "`cron '<expr>' or `after '<dur>'")?,
            CLASS,
        )?;
        let label = payload_label(spec.take("label", "a Str naming the wakeup")?, CLASS)?;
        let prompt = payload_string(
            spec.take("prompt", "the instruction delivered when the wakeup fires")?,
            CLASS,
            "prompt",
        )?;

        // The row goes up after the registry call, so a refusal tiers instead
        // of reading as one that landed.
        let described = trigger.describe();
        let result = s
            .schedules
            .schedule(trigger, prompt, label.clone(), &s.mailbox);
        let (payload, content) = match &result {
            Ok(receipt) => (
                described,
                format!(
                    "scheduled '{}' ({}s to first fire)",
                    receipt.label,
                    secs_to_i64(receipt.next_in)
                ),
            ),
            Err(e) => (format!("refused: {e}"), format!("could not schedule: {e}")),
        };
        s.commit_act(DeskAct::Schedule, Some(&label), payload, result.is_err());
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        match result {
            Ok(_) => Ok(self.schedule_table()),
            Err(_) => Err(Error::new(content, 1)),
        }
    }

    /// A snapshot of this agent's live wakeups — every `` `schedules `` tag's
    /// answer. Bare, unlike the roster: this family has only the one shape,
    /// so there is nothing for a tag to tell it apart from.
    fn schedule_table(&self) -> FOValue {
        let s = &self.services;
        FOValue::List {
            items: s
                .schedules
                .list()
                .into_iter()
                .map(|info| {
                    // A cron with nothing inside `next_delay`'s search horizon
                    // says "never" as `secs_to_i64`'s own saturated ceiling.
                    let next_s = info.next_in.map_or(i64::MAX, secs_to_i64);
                    let fires = i64::try_from(info.fires).unwrap_or(i64::MAX);
                    FOValue::Map {
                        entries: vec![
                            ("label".to_string(), FOValue::String { value: info.label }),
                            (
                                "trigger".to_string(),
                                FOValue::String {
                                    value: info.trigger,
                                },
                            ),
                            ("next-s".to_string(), FOValue::Int { value: next_s }),
                            ("fires".to_string(), FOValue::Int { value: fires }),
                        ],
                    }
                })
                .collect(),
        }
    }

    /// `` `remove `` — take one scheduled wakeup off the table by label. Only
    /// the grant refusal raises; a label that was never there is a successful
    /// call answering a table that does not carry it.
    fn unschedule(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        const CLASS: &str = "schedules `remove";
        self.require_schedule_grant(CLASS)?;
        let s = &self.services;
        let label = payload_value(payload, CLASS, "naming the wakeup to remove")?;
        let label = payload_string(label, CLASS, "label")?;

        // The rail's payload column spells out the miss the table only implies,
        // since the verb has no argument of its own to show there.
        let removed = s.schedules.unschedule(&label);
        let (payload, content) = if removed {
            (String::new(), format!("unscheduled '{label}'"))
        } else {
            (
                "no live schedule by that label".to_string(),
                format!("no live schedule labelled '{label}'"),
            )
        };
        s.commit_act(DeskAct::Unschedule, Some(&label), payload, !removed);
        s.record_forensic(crate::record::Forensic::HarnessResult { text: content });
        Ok(self.schedule_table())
    }

    /// `` `reply `` — stage the payload into the cell [`Avatar::deliberate`] lifts
    /// into a deposit on this agent's own registry entry once the batch drains:
    /// it parks the agent and hands the value to the parent's `` agents `read ``,
    /// rather than ending the run. Refused on every non-returning agent, keyed
    /// on `returns` and never on trunk-ness.
    fn agent_reply(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let s = &self.services;
        if !s.returns {
            return Err(Error::new(
                "agents `reply` refused: you converse with the user; you do not return. \
                 `reply` parks you and hands your value to your parent's agents `read — \
                 the interactive trunk and every /branch child instead keep talking, turn after \
                 turn, and hold no `reply` to call.",
                1,
            ));
        }
        let [value] = payload_list(payload, "agents `reply", "[value]")?;
        let display =
            shell_eval::ral_value_to_text(&RalValue::from(value.clone())).unwrap_or_default();
        let payload = if display.is_empty() {
            "(empty reply)".into()
        } else {
            display
        };
        // No subject: the parent is the only recipient a returning agent has.
        s.commit_act(DeskAct::Reply, None, payload, false);
        s.reply.set(value);
        Ok(self.roster())
    }

    /// `` `read `` — fetch the value the live descendant named by the payload
    /// last handed to `` `reply ``, scoped as [`AgentRegistry::message`] is. The
    /// one tag that does not answer the roster: it answers the fetched record
    /// instead, since that is the whole point of the call. A read is an
    /// observation, not an act, so nothing is committed to [`DeskAct`] — it
    /// changes nothing, exactly like `` `list ``.
    fn agent_read(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        const CLASS: &str = "agents `read";
        let s = &self.services;
        let name = payload_value(payload, CLASS, "naming the descendant to read")?;
        let name = payload_string(name, CLASS, "name")?;

        let content = match s.registry.resolve_name(&name) {
            None => Err(format!(
                "no live agent named '{name}'; did it finish, or was it never started?"
            )),
            Some(to) => match s.registry.reply_of(s.parent, to) {
                Err(NotADescendant(_)) => Err(format!(
                    "agent '{name}' is not an agent you started; `agents `read` may only reach \
                     a descendant of yours"
                )),
                Ok(None) => Err(format!(
                    "agent '{name}' has not replied yet — it is still working; wait for its \
                     notice instead of polling"
                )),
                Ok(Some(reply)) => Ok(reply),
            },
        };
        match content {
            Ok(reply) => {
                s.record_forensic(crate::record::Forensic::HarnessResult {
                    text: format!("read agent '{name}''s reply"),
                });
                Ok(FOValue::Map {
                    entries: vec![
                        ("name".to_string(), FOValue::String { value: name }),
                        ("reply".to_string(), reply),
                    ],
                })
            }
            Err(text) => {
                s.record_forensic(crate::record::Forensic::HarnessResult { text: text.clone() });
                Err(Error::new(text, 1))
            }
        }
    }

    /// `` `pin-read `` — the card stored under `key` on this agent's own
    /// register, canonically re-encoded, or `()` on a miss or an absent
    /// mirror. Read-after-write within one run is sound because
    /// [`DeskBinding::enquire`] drains queued surface frames before handling.
    fn pin_read(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let [key] = payload_list(payload, "pin-read", "[key]")?;
        let key = payload_string(key, "pin-read", "key")?;
        let Some(pins) = &self.services.pins else {
            return Ok(FOValue::Unit);
        };
        let m = pins.lock().expect("pin register poisoned");
        match m.get(&key) {
            // A card value is always first-order, so this conversion never fails.
            Some(digest) => FOValue::try_from(&crate::bus::card::encode_card(&digest.card)),
            None => Ok(FOValue::Unit),
        }
    }

    /// `` `pin-list `` — the keys currently occupied on this agent's own
    /// register, in `BTreeMap` order; an absent mirror answers empty.
    fn pin_list(&self) -> FOValue {
        FOValue::List {
            items: self.services.pins.as_ref().map_or_else(Vec::new, |pins| {
                pins.lock()
                    .expect("pin register poisoned")
                    .keys()
                    .map(|key| FOValue::String { value: key.clone() })
                    .collect()
            }),
        }
    }

    /// `` `context `` — a survey of this agent's own transcript: one span per
    /// digest/import/exchange, plus the running byte and step totals. Silent,
    /// like the roster: the answer is the survey.
    fn context(&self) -> FOValue {
        let survey = self.services.log.lock().context_survey();
        let spans = survey
            .items
            .into_iter()
            .map(|item| FOValue::Map {
                entries: vec![
                    (
                        "exchange".to_string(),
                        FOValue::Int {
                            value: u64_to_i64(item.exchange),
                        },
                    ),
                    (
                        "kind".to_string(),
                        FOValue::String {
                            value: item.kind.as_str().to_string(),
                        },
                    ),
                    (
                        "prompt".to_string(),
                        FOValue::String {
                            value: item.opening,
                        },
                    ),
                    (
                        "bytes".to_string(),
                        FOValue::Int {
                            value: usize_to_i64(item.bytes),
                        },
                    ),
                    (
                        "steps".to_string(),
                        FOValue::Int {
                            value: usize_to_i64(item.steps),
                        },
                    ),
                    ("live".to_string(), FOValue::Bool { value: item.live }),
                ],
            })
            .collect();
        FOValue::Map {
            entries: vec![
                ("spans".to_string(), FOValue::List { items: spans }),
                (
                    "total-bytes".to_string(),
                    FOValue::Int {
                        value: usize_to_i64(survey.total_bytes),
                    },
                ),
                (
                    "total-steps".to_string(),
                    FOValue::Int {
                        value: usize_to_i64(survey.total_steps),
                    },
                ),
                (
                    "cache".to_string(),
                    FOValue::String {
                        value: CACHE_SENTENCE.to_string(),
                    },
                ),
            ],
        }
    }

    /// `` `context-read `` — the rendered transcript of the named exchanges,
    /// probing rather than acting: a read commits no act, so the verb string
    /// below is a second mint outside [`DeskAct::verb`] on purpose.
    fn context_read(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let exchanges = payload_exchanges(payload, "context-read")?;
        let subject = exchanges_subject(&exchanges);
        let result = self.services.log.lock().read_context(&exchanges);
        self.services
            .record_display(crate::record::Display::HarnessCall {
                verb: "context-read".to_string(),
                subject: Some(subject.clone()),
                payload: subject,
                failed: result.is_err(),
            });
        result
            .map(|value| FOValue::String { value })
            .map_err(|error| Error::new(error, 1))
    }

    /// `` `context-drop `` — shed the named exchanges from the view, receipted
    /// by [`Self::context_edit`] with the resulting byte delta.
    fn context_drop(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let exchanges = payload_exchanges(payload, "context-drop")?;
        let subject = exchanges_subject(&exchanges);
        self.context_edit(
            DeskAct::ContextDrop,
            Some(&subject),
            subject.clone(),
            ContextOp::Drop { exchanges },
        )
    }

    /// `` `context-fold `` — collapse every exchange through `through` into
    /// one digest, receipted by [`Self::context_edit`] with the resulting byte
    /// delta.
    fn context_fold(&self, payload: Option<Box<FOValue>>) -> Result<FOValue, Error> {
        let [through, digest] = payload_list(payload, "context-fold", "[through, digest]")?;
        let through = payload_exchange(through, "context-fold", "through")?;
        let digest = payload_string(digest, "context-fold", "digest")?;
        let subject = format!("through {through}");
        let payload = format!("{subject}, digest {digest}");
        self.context_edit(
            DeskAct::ContextFold,
            Some(&subject),
            payload,
            ContextOp::Fold {
                through_exchange: through,
                digest,
            },
        )
    }

    /// The `` `context-drop ``/`` `context-fold `` tail both share: apply the
    /// edit, commit the act under `refused` on either outcome, and mirror the
    /// receipt or the refusal onto the trace before answering.
    fn context_edit(
        &self,
        act: DeskAct,
        subject: Option<&str>,
        payload: String,
        op: ContextOp,
    ) -> Result<FOValue, Error> {
        let result = self
            .services
            .log
            .lock()
            .apply_edit(op, EditAuthority::Model);
        match result {
            // `apply_edit` records `ContextEdited` through the seam, and the
            // live row derives from that published record: nothing separate
            // to emit here.
            Ok(receipt) => {
                self.services.commit_act(act, subject, payload, false);
                let text = format!(
                    "context changed by {:+} serialized bytes",
                    receipt.bytes_delta
                );
                self.services
                    .record_forensic(crate::record::Forensic::HarnessResult { text });
                Ok(FOValue::Map {
                    entries: vec![(
                        "bytes-delta".to_string(),
                        FOValue::Int {
                            value: receipt.bytes_delta,
                        },
                    )],
                })
            }
            Err(error) => {
                self.services.commit_act(act, subject, payload, true);
                self.services
                    .record_forensic(crate::record::Forensic::HarnessResult {
                        text: error.clone(),
                    });
                Err(Error::new(error, 1))
            }
        }
    }
}

/// Decodes a surfaced value onto the bus, folding a `` `pin ``/`` `unpin ``
/// into the pin mirror and the io/diff surfaces into the commit producer's
/// [`SurfaceBuffer`], so what reaches the record is the deduped, coalesced
/// form the user saw. [`HostSeam::apply`] is the one instance
/// `shell_eval::run_shell`'s own drain and [`DeskBinding`]'s both reach
/// through, so the two paths render off the same applier rather than two that
/// could drift.
pub(crate) struct SurfaceApplier {
    pub(crate) pins: Option<PinDigests>,
    /// The owning session's id and record seam — what the buffered commits
    /// stamp through when they flush.
    pub(crate) id: AgentId,
    pub(crate) recorder: crate::record::Emitter,
    /// Per-call, like the applier itself: `run_shell` flushes it at the call
    /// boundary, so a call's effects record contiguously.
    pub(crate) surface: Mutex<SurfaceBuffer>,
}

impl SurfaceApplier {
    /// Apply one live [`Event::Surface`] value.
    pub(crate) fn live(&self, val: FOValue) {
        if let Some(surface) = shell_eval::accepted_surface(&RalValue::from(val), &self.recorder) {
            if let Some(pins) = &self.pins {
                // Fatal, never skipped: dropping a disposition here would
                // desync the mirror from the stream with no signal at all.
                let mut m = pins.lock().expect("pin register poisoned");
                match &surface {
                    Surface::Pin { key, card } => {
                        m.insert(key.clone(), shell_eval::PinDigest::new(card.clone()));
                    }
                    Surface::Unpin { key } => {
                        m.remove(key);
                    }
                    _ => {}
                }
            }
            let mut buf = self.surface.lock_ignore_poison();
            if let Err(error) = absorb_surface(&mut buf, &self.recorder, self.id, &surface) {
                drop(buf);
                self.recorder.report_fault(&error);
            }
        }
    }

    /// The call boundary: record whatever the buffers still hold, in their
    /// barrier order.
    pub(crate) fn flush(&self) {
        let mut buf = self.surface.lock_ignore_poison();
        if let Err(error) = buf.flush_surfaces(&self.recorder) {
            drop(buf);
            self.recorder.report_fault(&error);
        }
    }
}

/// Feed one decoded [`Surface`] to the commit producer — an io observation
/// into its bucket, a lone diff card into the patch buffer, and every other
/// surface class as its own commit — shared by [`SurfaceApplier::live`] and
/// the deferred-batch path in `agent::attend::announce`, so the two cannot
/// drift on what records.
///
/// Only the bucketed observations — read, exec, grep, write — enter the
/// buffer; a worker birth or capability check joins no group and records at
/// once as its own commit, exactly the line the buffer's own `unreachable!`
/// arms draw.  A done, notice, or generic card flushes the buffer first:
/// those close or interrupt the run around them, so the record keeps the
/// order the user saw.
///
/// A pin publishes twice, per the mirror it also feeds in
/// [`SurfaceApplier::live`]: [`crate::record::Forensic::Pin`]/`Unpin` is the
/// durable breadcrumb a resume replays, [`crate::record::Transient::Pin`]/
/// `Unpin` is the register the live process is holding, which a resume does
/// not restore.
pub(crate) fn absorb_surface(
    buf: &mut SurfaceBuffer,
    recorder: &crate::record::Emitter,
    id: AgentId,
    surface: &Surface,
) -> std::io::Result<()> {
    match surface {
        Surface::Observation(event) => match &event.what {
            Observed::Read { .. }
            | Observed::Command { .. }
            | Observed::Grep { .. }
            | Observed::Write { .. } => buf.absorb_observation(recorder, id, (**event).clone()),
            Observed::Worker { .. } | Observed::Capability { .. } | Observed::Act { .. } => {
                let value = crate::bus::card::observation_wire(event);
                let _recorded = recorder.emit(crate::record::Display::Observation { value })?;
                Ok(())
            }
        },
        Surface::Card(card) => match card.clone().into_single_diff() {
            Ok((path, hunks)) => buf.absorb_patch(recorder, id, path, hunks),
            // A richer card is the kit's own communication: the mark tree is
            // the fact, so it records whole and opaquely.  The buffers flush
            // first, keeping the record in the order the user saw — a card
            // never joins a group, so nothing coalesced is torn by the flush.
            Err(card) => {
                buf.flush_surfaces(recorder)?;
                let marks =
                    serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
                let _recorded = recorder.emit(crate::record::Display::Card { marks })?;
                Ok(())
            }
        },
        Surface::Done(outcome) => {
            buf.flush_surfaces(recorder)?;
            let _recorded = recorder.emit(crate::record::Display::Done {
                outcome: done_fact(outcome),
            })?;
            Ok(())
        }
        Surface::Notice(notice) => {
            buf.flush_surfaces(recorder)?;
            let _recorded = recorder.emit(crate::record::Display::Notice {
                notice: notice_fact(notice),
            })?;
            Ok(())
        }
        // Register state, not scrollback: the history informs the resume note,
        // and the buffer is left unflushed on purpose — a pin joins no group
        // and must not split the one it is never offered to.
        Surface::Pin { key, card } => {
            let _recorded = recorder.emit(crate::record::Forensic::Pin { key: key.clone() })?;
            recorder.transient(crate::record::Transient::Pin {
                key: key.clone(),
                card: card.clone(),
            });
            Ok(())
        }
        Surface::Unpin { key } => {
            let _recorded = recorder.emit(crate::record::Forensic::Unpin { key: key.clone() })?;
            recorder.transient(crate::record::Transient::Unpin { key: key.clone() });
            Ok(())
        }
    }
}

/// The data half of a settled worker's `` `done `` card, in the record's own
/// spelling of the same three outcomes.
fn done_fact(outcome: &crate::bus::card::DoneOutcome) -> crate::record::DoneOutcome {
    match outcome {
        crate::bus::card::DoneOutcome::Ok => crate::record::DoneOutcome::Ok,
        crate::bus::card::DoneOutcome::Err { message, status } => crate::record::DoneOutcome::Err {
            message: message.clone(),
            status: *status,
        },
        crate::bus::card::DoneOutcome::Panic { message } => crate::record::DoneOutcome::Panic {
            message: message.clone(),
        },
    }
}

/// The data half of a `` `notice `` card, the reap cause carried as the three
/// spellings the record's serde surface names.
fn notice_fact(notice: &crate::bus::card::Notice) -> crate::record::NoticeFact {
    match notice {
        crate::bus::card::Notice::Reap { cmd, cause } => crate::record::NoticeFact::Reap {
            cmd: cmd.clone(),
            cause: match cause {
                ral_core::types::ReapCause::Idle => "idle",
                ral_core::types::ReapCause::Backstop => "backstop",
                ral_core::types::ReapCause::Retention => "retention",
            }
            .to_string(),
        },
        crate::bus::card::Notice::Prune { names, idle_calls } => crate::record::NoticeFact::Prune {
            names: names.clone(),
            idle_calls: idle_calls.clone(),
        },
    }
}

/// Installed by [`crate::agent::seat::RunGuard`] once a call's real desk
/// retires, so nothing holds that call's [`HostServices`] — its [`Emitter`]
/// above all — past the call that owns it. Never observed in practice: nothing
/// dispatches without a fresh real desk installed first.
pub(crate) struct AbsentDesk;

impl EnquiryDesk for AbsentDesk {
    /// `_cancel` goes unpolled: every answer here is immediate and
    /// unconditional, so there is no wait for a poll point to interrupt.
    fn enquire(
        &self,
        _req: FOValue,
        _cancel: &ral_core::process::CancelScope,
    ) -> Result<FOValue, Error> {
        Err(Error::new(NO_DESK, NO_DESK_STATUS))
    }
}

/// One `ral` call's whole host seam: the desk that answers its enquiries and
/// the applier that renders its surfaced values, built once per call and
/// shared by `Arc` between [`DeskBinding`] (the identity install) and
/// `shell_eval::run_shell`'s own drain — so the two paths through one call can
/// never be handed different desks, or different appliers, by mistake.
pub(crate) struct HostSeam {
    pub(crate) desk: ExarchDesk,
    pub(crate) apply: SurfaceApplier,
}

/// The identity transport's drain-then-handle adapter.
///
/// Under [`ral_core::transport::IdentityTransport`], [`ral_core::Shell::enquire`]
/// calls into the installed desk synchronously, mid-`dispatch` — before
/// `dispatch_to_report`'s drain loop, which starts only once `dispatch` returns.
/// The run's earlier surface frames are therefore still queued, so
/// [`Self::enquire`] applies them first: answering ahead of them would render a
/// handler's chrome before output that preceded it.
pub(crate) struct DeskBinding {
    pub(crate) seam: Arc<HostSeam>,
    pub(crate) events: Arc<EventReceiver>,
}

impl EnquiryDesk for DeskBinding {
    /// `_cancel` goes unpolled: `ExarchDesk::handle` answers every class from
    /// its captured `HostServices` without parking on another party — a spawn
    /// starts a detached worker and returns, a cancel signals a token and
    /// returns, neither waits for the far side to react. The one arm that
    /// grows rather than finishing at once is `` `start ``'s `mnemon` copy of
    /// the parent's inherited context (`ExarchDesk::launch`): still bounded,
    /// by the conversation so far, but the largest uncancellable window this
    /// desk ever holds open.
    fn enquire(
        &self,
        req: FOValue,
        _cancel: &ral_core::process::CancelScope,
    ) -> Result<FOValue, Error> {
        while let Some((_, event)) = self.events.try_recv() {
            if let Event::Surface(val) = event {
                self.seam.apply.live(val);
            }
        }
        self.seam.desk.handle(req)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::event::AgentLog;
    use crate::agent::testkit::ral_call;
    use crate::bus::{Inbox, Signal, channel};
    use crate::egress::Egress;
    use crate::fleet::registry::{EvalReach, InterruptTarget};
    use crate::provider::{
        Provider,
        scripted::{Reply, Script},
    };
    use crate::record::{Display, FleetSink, Record, Transient};
    use crate::shell_eval::builtins::harness;
    use ral_core::transport::{DispatchId, IdentityTransport, Program, Run, Transport};
    use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};

    fn fresh_log() -> AgentLog {
        AgentLog::for_test(0, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    fn scripted_provider() -> Arc<Provider> {
        Arc::new(Provider::scripted("test-model", Script::new()))
    }

    /// The base capture every desk below builds on, so growing [`HostServices`]
    /// means touching one literal, not three.
    fn base_services() -> HostServices {
        let (emit, _rx) = crate::bus::dummy_emitter();
        HostServices {
            registry: AgentRegistry::new(),
            scratch: Some(Arc::new(
                crate::bootstrap::Scratch::for_test(
                    crate::bootstrap::EXARCH,
                    &format!("desk-{}", crate::agent::fresh_id()),
                )
                .expect("scratch dir"),
            )),
            parent: 0,
            mailbox: Inbox::new().mailbox(),
            emit,
            provider: ProviderHandle::new(scripted_provider()),
            caps: Capabilities::root(),
            cwd: PathBuf::from("/"),
            fuel: 3,
            returns: true,
            allow_schedule: false,
            search: true,
            reply: ReplyCell::default(),
            schedules: ScheduleRegistry::new(),
            log: LogCell::new(fresh_log()),
            system_template: String::new(),
            index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                ral_core::io::TerminalState::default(),
            )),
            interactive: false,
            nursery: Nursery::default(),
            generation: 0,
            disk_warn_bytes: None,
            egress: Egress::for_test(),
            acts: ActFragment::default(),
            principal: ral_core::host::user(),
            pins: None,
            wire_seat: false,
            dial: None,
        }
    }

    fn desk() -> ExarchDesk {
        ExarchDesk {
            services: base_services(),
        }
    }

    fn complete_exchange(log: &mut AgentLog, prompt: &str, answer: &str) {
        log.append_user(prompt.to_string(), None).expect("prompt");
        log.append_assistant(
            genai::chat::ChatMessage::assistant(answer),
            Vec::new(),
            None,
        )
        .expect("answer");
    }

    /// Every context-verb test request below goes through the same encoders
    /// [`harness::builtin_context_drop`] and its siblings call, so a change to
    /// an encoder breaks these tests instead of leaving them to hand-build a
    /// payload that has quietly drifted from what the builtin actually sends.
    fn exchanges_value(exchanges: &[i64]) -> RalValue {
        RalValue::list(
            exchanges
                .iter()
                .map(|value| RalValue::Int(*value))
                .collect(),
        )
    }

    fn context_drop_request(exchanges: &[i64]) -> FOValue {
        let payload =
            harness::context_exchanges_payload(&exchanges_value(exchanges), "context-drop")
                .expect("valid exchange list");
        FOValue::Variant {
            label: "context-drop".to_string(),
            payload: Some(Box::new(payload)),
        }
    }

    fn context_fold_request(through: i64, digest: &str) -> FOValue {
        let spec = RalValue::map(vec![
            ("through".to_string(), RalValue::Int(through)),
            ("digest".to_string(), RalValue::String(digest.to_string())),
        ]);
        let payload = harness::context_fold_payload(&spec).expect("valid fold spec");
        FOValue::Variant {
            label: "context-fold".to_string(),
            payload: Some(Box::new(payload)),
        }
    }

    /// The `` `context `` survey call, shaped exactly as `builtin_context`
    /// sends it: no payload to encode, since the answer is the whole survey.
    fn context_survey_request() -> FOValue {
        FOValue::Variant {
            label: "context".to_string(),
            payload: None,
        }
    }

    fn context_read_request(exchanges: &[i64]) -> FOValue {
        let payload =
            harness::context_exchanges_payload(&exchanges_value(exchanges), "context-read")
                .expect("valid exchange list");
        FOValue::Variant {
            label: "context-read".to_string(),
            payload: Some(Box::new(payload)),
        }
    }

    fn int_field(value: FOValue, key: &str) -> i64 {
        let FOValue::Map { entries } = value else {
            panic!("expected a record")
        };
        entries
            .into_iter()
            .find_map(|(name, value)| (name == key).then_some(value))
            .and_then(|value| match value {
                FOValue::Int { value } => Some(value),
                _ => None,
            })
            .unwrap_or_else(|| panic!("record has no Int field `{key}`"))
    }

    fn str_field(row: &FOValue, key: &str) -> Option<String> {
        let FOValue::Map { entries } = row else {
            panic!("expected a record")
        };
        entries.iter().find_map(|(name, value)| match value {
            FOValue::String { value } if name == key => Some(value.clone()),
            _ => None,
        })
    }

    fn bare(label: &str) -> FOValue {
        FOValue::Variant {
            label: label.to_string(),
            payload: None,
        }
    }

    fn text(value: &str) -> FOValue {
        FOValue::String {
            value: value.to_string(),
        }
    }

    /// The nested request both registries now take: the family names the
    /// class, the tag names what to do, and the payload is the model's value.
    pub(super) fn family_req(family: &str, tag: &str, payload: Option<FOValue>) -> FOValue {
        FOValue::Variant {
            label: family.to_string(),
            payload: Some(Box::new(FOValue::Variant {
                label: tag.to_string(),
                payload: payload.map(Box::new),
            })),
        }
    }

    /// `` `agents `start ``: the model's own spec record beside whichever
    /// `fork` tag the engine minted. Always `` `amnemon ``, the only kind the
    /// desk tests exercise.
    pub(super) fn start_req_forked(
        fork: FOValue,
        prompt: &str,
        name: &str,
        grant: &str,
        search: bool,
    ) -> FOValue {
        let spec = FOValue::Map {
            entries: vec![
                ("prompt".to_string(), text(prompt)),
                ("name".to_string(), text(name)),
                ("type".to_string(), bare("amnemon")),
                ("grant".to_string(), bare(grant)),
                ("search".to_string(), FOValue::Bool { value: search }),
            ],
        };
        family_req(
            "agents",
            "start",
            Some(FOValue::Map {
                entries: vec![("spec".to_string(), spec), ("fork".to_string(), fork)],
            }),
        )
    }

    /// The in-process shape: the fork waits in this host's own nursery.
    pub(super) fn start_req(
        session: NurseryId,
        prompt: &str,
        name: &str,
        grant: &str,
        search: bool,
    ) -> FOValue {
        let fork = FOValue::Variant {
            label: "parked".to_string(),
            payload: Some(Box::new(FOValue::Int {
                value: i64::try_from(session.0).expect("small test id"),
            })),
        };
        start_req_forked(fork, prompt, name, grant, search)
    }

    /// `` `agents `message ``: the model's record, by the surface's own names.
    pub(super) fn message_req(to: &str, body: &str) -> FOValue {
        family_req(
            "agents",
            "message",
            Some(FOValue::Map {
                entries: vec![
                    ("to".to_string(), text(to)),
                    ("text".to_string(), text(body)),
                ],
            }),
        )
    }

    /// `` `schedules `add `` with an `` `after `` trigger — the only kind these
    /// tests arm, since a cron's first fire is not a fixed delay.
    fn add_req(after: &str, label: &str, prompt: &str) -> FOValue {
        family_req(
            "schedules",
            "add",
            Some(FOValue::Map {
                entries: vec![
                    (
                        "trigger".to_string(),
                        FOValue::Variant {
                            label: "after".to_string(),
                            payload: Some(Box::new(text(after))),
                        },
                    ),
                    ("label".to_string(), text(label)),
                    ("prompt".to_string(), text(prompt)),
                ],
            }),
        )
    }

    /// Unwrap a `` `roster `` answer into its rows.
    pub(super) fn roster(answer: FOValue) -> Vec<FOValue> {
        let FOValue::Variant {
            label,
            payload: Some(payload),
        } = answer
        else {
            panic!("every `agents tag answers a tagged variant")
        };
        assert_eq!(label, "roster", "the answer must be the roster");
        let FOValue::List { items } = *payload else {
            panic!("a roster carries a list of rows")
        };
        items
    }

    /// The names a `` `roster `` answer lists, in registry order.
    pub(super) fn roster_names(answer: FOValue) -> Vec<String> {
        roster(answer)
            .iter()
            .map(|row| str_field(row, "name").expect("every roster row names its agent"))
            .collect()
    }

    /// Unwrap a `` `schedules `` answer into its rows.
    fn table(answer: FOValue) -> Vec<FOValue> {
        let FOValue::List { items } = answer else {
            panic!("every `schedules tag answers the bare table")
        };
        items
    }

    /// [`desk`] holding the self-wakeup grant, so a schedule test reaches past it.
    fn granted_desk() -> ExarchDesk {
        ExarchDesk {
            services: HostServices {
                allow_schedule: true,
                ..base_services()
            },
        }
    }

    /// A desk whose parent is itself registered — the precondition
    /// [`AgentRegistry::register`] enforces for any child — so
    /// `` `start ``/`` `cancel ``/`` `message `` run end to end, unlike [`desk`].
    fn spawnable_desk(fuel: u32) -> (ExarchDesk, AgentRegistry, Inbox) {
        let parent_inbox = Inbox::new();
        let registry = AgentRegistry::new();
        // Minted, never a literal: children draw from the same process-wide
        // counter, so a hardcoded id collides with the first child this desk
        // spawns — overwriting the parent's entry, and hanging rather than
        // failing.
        let parent_id = crate::agent::fresh_id();
        let parent_agent = crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
            id: parent_id,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: parent_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider()),
            consumer: 0,
        });
        let _ = registry.register(None, parent_agent);
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                parent: parent_id,
                mailbox: parent_inbox.mailbox(),
                fuel,
                generation: registry.generation(parent_id),
                ..base_services()
            },
        };
        (desk, registry, parent_inbox)
    }

    /// Poll `inbox` for the next exchange-boundary item — a spawned child's
    /// settled [`crate::bus::AgentResult`] lands here.
    fn wait_for_settle(inbox: &Inbox) -> crate::bus::Item {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(item) = inbox.next_item() {
                return item;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child did not settle within the timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A scratch directory for one test, deleted when the guard falls.
    fn tmp(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("exarch-desk-full-{tag}-"))
            .tempdir()
            .expect("test scratch dir")
    }

    /// Booted exactly once per test. `boot_shell` resets process-global signal
    /// state, the ceremony the fleet runs once at its root; booting again per
    /// spawn races that reset against a sibling's in-flight run and deadlocks.
    fn root_shell() -> ral_core::Shell {
        crate::bootstrap::boot_shell()
    }

    /// The fork [`ral_core::Shell::fork_into_nursery`] would perform on a live
    /// session, off the shared [`root_shell`] rather than a freshly booted one.
    fn forkable_child_shell(root: &ral_core::Shell) -> ral_core::Shell {
        root.fork_session()
    }

    #[test]
    fn unknown_class_answers_the_extension_error() {
        let err = desk()
            .handle(FOValue::Variant {
                label: "no-such-class".into(),
                payload: None,
            })
            .expect_err("an unrecognised class must not answer Ok");
        assert_eq!(err.message, "unrecognised enquiry class `no-such-class`");
    }

    /// Nesting the tag under the family must not open a silent hole one level
    /// down: a tag extends a family the way a class extends the desk.
    #[test]
    fn unknown_tag_answers_the_extension_error_too() {
        for family in ["agents", "schedules"] {
            let err = desk()
                .handle(family_req(family, "no-such-tag", None))
                .expect_err("an unrecognised tag must not answer Ok");
            assert_eq!(
                err.message,
                format!("unrecognised `{family} tag `no-such-tag`")
            );
        }
    }

    /// Names the expected shape rather than panicking or silently defaulting.
    #[test]
    fn non_variant_request_errors_didactically() {
        let err = desk()
            .handle(FOValue::Unit)
            .expect_err("a non-variant request must not answer Ok");
        assert!(
            err.message.contains("must be a variant"),
            "error must name the expected shape, got: {}",
            err.message
        );
    }

    #[test]
    fn context_survey_lists_digest_import_and_live_exchange() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "closed\nexchange", "answer");
            complete_exchange(&mut log, "folded", "answer");
            log.apply_edit(
                ContextOp::Fold {
                    through_exchange: 1,
                    digest: "kept summary".into(),
                },
                EditAuthority::Model,
            )
            .expect("fold");
            log.import_context(vec![genai::chat::ChatMessage::user("inherited\ncontext")])
                .expect("import");
            log.append_user("live".into(), None).expect("live prompt");
        }
        let expected_bytes = desk.services.log.lock().history_bytes();

        let FOValue::Map { entries } = desk
            .handle(context_survey_request())
            .expect("context survey")
        else {
            panic!("context must answer a record")
        };
        let spans = entries
            .iter()
            .find_map(|(key, value)| (key == "spans").then_some(value))
            .expect("survey spans");
        let FOValue::List { items } = spans else {
            panic!("survey spans must be a list")
        };
        assert_eq!(items.len(), 4);
        let kinds = items
            .iter()
            .map(|item| {
                let FOValue::Map { entries } = item else {
                    panic!("survey item must be a record")
                };
                entries
                    .iter()
                    .find_map(|(key, value)| {
                        (key == "kind").then(|| match value {
                            FOValue::String { value } => value.as_str(),
                            _ => panic!("survey kind must be a string"),
                        })
                    })
                    .expect("survey kind")
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec!["digest", "exchange", "import", "exchange"]);
        let total_bytes = entries
            .iter()
            .find_map(|(key, value)| {
                (key == "total-bytes").then(|| match value {
                    FOValue::Int { value } => *value,
                    _ => panic!("survey total bytes must be an Int"),
                })
            })
            .expect("survey total bytes");
        assert_eq!(total_bytes, i64::try_from(expected_bytes).unwrap());
        let live = items.last().expect("live exchange");
        assert!(matches!(
            live,
            FOValue::Map { entries }
                if entries.iter().any(|(key, value)| key == "live" && matches!(value, FOValue::Bool { value: true }))
        ));
    }

    #[test]
    fn context_read_returns_one_transcript_without_committing_an_act() {
        let mut desk = desk();
        {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "first prompt", "first answer");
        }
        let (tx, rx) = channel();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);

        let FOValue::String { value } = desk
            .handle(context_read_request(&[1]))
            .expect("context-read")
        else {
            panic!("context-read must answer one Str")
        };
        assert!(value.contains("[user]\nfirst prompt"));
        assert!(value.contains("[assistant]\nfirst answer"));
        assert!(desk.services.acts.audit().is_none(), "a read has no act");
        let record = crate::bus::drain_records(&rx)
            .into_iter()
            .next()
            .expect("the read must reach the trace");
        assert!(matches!(
            record,
            Record::Display(Display::HarnessCall {
                verb,
                subject: Some(subject),
                payload,
                failed: false,
            }) if verb == "context-read" && subject == "exchanges [1]" && payload == "exchanges [1]"
        ));

        let error = desk
            .handle(context_read_request(&[]))
            .expect_err("an empty read is not meaningful");
        assert_eq!(
            error.message,
            "context-read must name at least one exchange"
        );
        let record = crate::bus::drain_records(&rx)
            .into_iter()
            .next()
            .expect("a refused read still reaches the trace");
        assert!(matches!(
            record,
            Record::Display(Display::HarnessCall {
                verb,
                failed: true,
                ..
            }) if verb == "context-read"
        ));
    }

    #[test]
    fn context_edit_refusals_surface_the_admissibility_sentence() {
        let live_desk = desk();
        {
            let mut log = live_desk.services.log.lock();
            log.append_user("live".into(), None).expect("live prompt");
        }
        let err = live_desk
            .handle(context_drop_request(&[1]))
            .expect_err("the live exchange is not editable");
        assert_eq!(
            err.message,
            "exchange 1 is the one you are in — a context edit may only name closed exchanges"
        );

        let unknown_desk = desk();
        {
            let mut log = unknown_desk.services.log.lock();
            complete_exchange(&mut log, "one", "answer");
        }
        let err = unknown_desk
            .handle(context_drop_request(&[7]))
            .expect_err("an unknown exchange is not editable");
        assert_eq!(err.message, "exchange 7 is not present in the current view");

        let folded_desk = desk();
        {
            let mut log = folded_desk.services.log.lock();
            complete_exchange(&mut log, "one", "answer");
            complete_exchange(&mut log, "two", "answer");
        }
        folded_desk
            .handle(context_fold_request(2, "kept summary"))
            .expect("fold");
        let err = folded_desk
            .handle(context_drop_request(&[1]))
            .expect_err("an exchange inside the digest is not addressable");
        assert_eq!(
            err.message,
            "exchange 1 is folded into the digest through 2 — name 2 to drop the digest whole, or fold further"
        );

        let err = folded_desk
            .handle(context_drop_request(&[]))
            .expect_err("an empty drop is not an edit");
        assert_eq!(
            err.message,
            "a context edit must name at least one exchange"
        );
    }

    #[test]
    fn context_drop_receipt_reports_the_view_byte_delta() {
        let desk = desk();
        let before = {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "one", "a longer answer");
            complete_exchange(&mut log, "two", "another answer");
            log.history_bytes()
        };
        let delta = int_field(
            desk.handle(context_drop_request(&[1]))
                .expect("context drop"),
            "bytes-delta",
        );
        let after = desk.services.log.lock().history_bytes();
        assert_eq!(
            delta,
            i64::try_from(before).unwrap() - i64::try_from(after).unwrap()
        );
        assert!(delta > 0, "dropping a visible exchange should shed bytes");
    }

    /// The fold counterpart to [`context_drop_receipt_reports_the_view_byte_delta`]:
    /// a fold receipts its byte delta and commits [`DeskAct::ContextFold`], not
    /// [`DeskAct::ContextDrop`] — the audit sentence names the act it actually
    /// took, which nothing else in this suite pins.
    #[test]
    fn context_fold_receipt_commits_the_fold_act() {
        let desk = desk();
        {
            let mut log = desk.services.log.lock();
            complete_exchange(&mut log, "one", "a longer answer");
            complete_exchange(&mut log, "two", "another answer");
        }
        let delta = int_field(
            desk.handle(context_fold_request(2, "kept summary"))
                .expect("context fold"),
            "bytes-delta",
        );
        assert_ne!(
            delta, 0,
            "folding two exchanges into a digest should change the byte count"
        );
        let audit = desk
            .services
            .acts
            .audit()
            .expect("a landed fold leaves an act");
        assert!(
            audit.contains("folded context"),
            "the audit sentence must name the fold, got: {audit}"
        );
    }

    /// `enquire` is called bare, off the dispatching thread: the adapter's drain
    /// is what is under test, not [`ral_core::Shell::enquire`]'s routing, which
    /// core's own suite covers.
    #[test]
    fn desk_binding_drains_pending_surfaces_before_handling() {
        let shell = ral_core::Shell::new(ral_core::io::TerminalState::default());
        let transport = IdentityTransport::new(shell);
        transport.dispatch(
            DispatchId(1),
            Run {
                program: Program::Source(r#"surface `unpin [key: "test-marker"]"#.into()),
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

        let (tx, rx) = crate::bus::channel();
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        let binding = DeskBinding {
            seam: Arc::new(HostSeam {
                desk: desk(),
                apply: SurfaceApplier {
                    pins: None,
                    id: 0,
                    recorder,
                    surface: Mutex::new(SurfaceBuffer::new()),
                },
            }),
            events: transport.events_shared(),
        };

        let answer = binding
            .enquire(
                FOValue::Variant {
                    label: "probe".into(),
                    payload: None,
                },
                &ral_core::process::CancelScope::default(),
            )
            .expect_err("the stub desk answers every class with the extension-law error");
        assert_eq!(answer.message, "unrecognised enquiry class `probe`");

        // Proof `apply` ran, not just that `handle` did.
        let saw_unpin = crate::bus::drain_transients(&rx)
            .into_iter()
            .any(|t| matches!(t, Transient::Unpin { key } if key == "test-marker"));
        assert!(
            saw_unpin,
            "the drained surface frame must render onto the bus"
        );

        // Nothing pending survives, so no straggler is left for a later drain to
        // reorder ahead of.
        assert!(
            binding.events.try_recv().is_none(),
            "enquire must drain the transport's event queue fully"
        );
    }

    fn fresh_mirror() -> PinDigests {
        Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()))
    }

    /// A `` `pin [key, body] `` surface value carrying a one-span text card —
    /// the minimal shape [`SurfaceApplier::live`] folds into the mirror.
    fn pin_value(key: &str, text: &str) -> FOValue {
        FOValue::Variant {
            label: "pin".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    ("key".into(), FOValue::String { value: key.into() }),
                    (
                        "body".into(),
                        FOValue::Variant {
                            label: "text".into(),
                            payload: Some(Box::new(FOValue::Map {
                                entries: vec![(
                                    "spans".into(),
                                    FOValue::List {
                                        items: vec![FOValue::Map {
                                            entries: vec![(
                                                "text".into(),
                                                FOValue::String { value: text.into() },
                                            )],
                                        }],
                                    },
                                )],
                            })),
                        },
                    ),
                ],
            })),
        }
    }

    fn unpin_value(key: &str) -> FOValue {
        FOValue::Variant {
            label: "unpin".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![("key".into(), FOValue::String { value: key.into() })],
            })),
        }
    }

    fn pin_read_req(key: &str) -> FOValue {
        FOValue::Variant {
            label: "pin-read".into(),
            payload: Some(Box::new(FOValue::List {
                items: vec![FOValue::String { value: key.into() }],
            })),
        }
    }

    /// A pin applied through [`SurfaceApplier::live`] comes back from
    /// `pin-read` as the canonical card — the readback and the pinned mark
    /// agree on shape, which is the whole point of a readable register.
    #[test]
    fn pin_read_returns_the_canonical_card() {
        let mirror = fresh_mirror();
        SurfaceApplier {
            pins: Some(mirror.clone()),
            id: 0,
            recorder: crate::record::Emitter::none(),
            surface: Mutex::new(SurfaceBuffer::new()),
        }
        .live(pin_value("tasks", "hi"));

        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };
        let answer = d
            .handle(pin_read_req("tasks"))
            .expect("a hit must answer Ok");
        let card = crate::bus::card::value_to_card(&RalValue::from(answer))
            .expect("the readback must decode as a card");
        assert!(
            matches!(
                card.marks(),
                [crate::bus::card::Mark::Text { spans }]
                    if spans.len() == 1 && spans[0].role.is_none() && spans[0].text == "hi"
            ),
            "the canonical card must round-trip the pinned text mark, got {card:?}"
        );
    }

    /// A key never pinned, and a key unpinned after being pinned, both answer
    /// `()` — a miss and a clear are the same absence to `pin-read`.
    #[test]
    fn pin_read_answers_unit_on_miss_and_after_unpin() {
        let mirror = fresh_mirror();
        let applier = SurfaceApplier {
            pins: Some(mirror.clone()),
            id: 0,
            recorder: crate::record::Emitter::none(),
            surface: Mutex::new(SurfaceBuffer::new()),
        };
        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };

        assert!(
            matches!(d.handle(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "a key never pinned must answer unit"
        );

        applier.live(pin_value("tasks", "hi"));
        applier.live(unpin_value("tasks"));
        assert!(
            matches!(d.handle(pin_read_req("tasks")), Ok(FOValue::Unit)),
            "an unpinned key must answer unit"
        );
    }

    /// `pin-list` names exactly the occupied keys, in `BTreeMap` order, and
    /// tracks a set/clear pair.
    #[test]
    fn pin_list_tracks_set_and_clear() {
        let mirror = fresh_mirror();
        let applier = SurfaceApplier {
            pins: Some(mirror.clone()),
            id: 0,
            recorder: crate::record::Emitter::none(),
            surface: Mutex::new(SurfaceBuffer::new()),
        };
        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };
        let keys = |d: &ExarchDesk| match d
            .handle(FOValue::Variant {
                label: "pin-list".into(),
                payload: None,
            })
            .expect("pin-list must answer Ok")
        {
            FOValue::List { items } => items
                .into_iter()
                .map(|v| match v {
                    FOValue::String { value } => value,
                    other => panic!("pin-list must answer strings, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("pin-list must answer a list, got {other:?}"),
        };

        assert!(keys(&d).is_empty(), "an empty register lists no keys");

        applier.live(pin_value("b", "one"));
        applier.live(pin_value("a", "two"));
        assert_eq!(
            keys(&d),
            vec!["a", "b"],
            "keys list in BTreeMap (lexicographic) order"
        );

        applier.live(unpin_value("b"));
        assert_eq!(keys(&d), vec!["a"], "a clear drops its key from the list");
    }

    /// Every surface class the live applier renders also records through the
    /// seam — a `Display` record (or, for the pin register, `Forensic`) —
    /// and a generic card records *behind* the io group buffered before it,
    /// keeping the record
    /// in the order the user saw.  A pin/unpin additionally publishes a
    /// `Transient` beside its `Forensic` twin: the durable breadcrumb and the
    /// live register the process is holding, two records for one act.
    #[test]
    fn live_surfaces_record_their_seam_twins() {
        use crate::bus::Signal;
        use crate::record::{Display, Forensic, Record, Transient};

        let (tx, rx) = channel();
        let recorder = crate::record::Emitter::none();
        recorder.attach(crate::record::FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        let applier = SurfaceApplier {
            pins: None,
            id: 0,
            recorder,
            surface: Mutex::new(SurfaceBuffer::new()),
        };

        let read = ral_core::types::Observation::instant(
            ral_core::types::CallSite::default(),
            None,
            ral_core::types::Observed::Read {
                path: "a.rs".into(),
            },
        );
        applier.live(crate::bus::card::observation_wire(&read));
        applier.live(FOValue::Variant {
            label: "card".into(),
            payload: Some(Box::new(FOValue::List { items: vec![] })),
        });
        applier.live(FOValue::Variant {
            label: "done".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    (
                        "cmd".into(),
                        FOValue::String {
                            value: "<block>".into(),
                        },
                    ),
                    (
                        "outcome".into(),
                        FOValue::Variant {
                            label: "ok".into(),
                            payload: Some(Box::new(FOValue::Unit)),
                        },
                    ),
                ],
            })),
        });
        applier.live(FOValue::Variant {
            label: "notice".into(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    (
                        "kind".into(),
                        FOValue::Variant {
                            label: "reap".into(),
                            payload: None,
                        },
                    ),
                    (
                        "cmd".into(),
                        FOValue::String {
                            value: "sleep 10".into(),
                        },
                    ),
                    (
                        "cause".into(),
                        FOValue::String {
                            value: "idle".into(),
                        },
                    ),
                ],
            })),
        });
        applier.live(pin_value("tasks", "hi"));
        applier.live(unpin_value("tasks"));

        let mut facts: Vec<&'static str> = Vec::new();
        let mut transients: Vec<&'static str> = Vec::new();
        while let Ok(sig) = rx.try_recv() {
            match sig {
                Signal::Fact(_, fact) => facts.push(match fact.value() {
                    Record::Display(Display::Observation { .. }) => "io",
                    Record::Display(Display::Card { .. }) => "card",
                    Record::Display(Display::Done {
                        outcome: crate::record::DoneOutcome::Ok,
                    }) => "done",
                    Record::Display(Display::Notice {
                        notice: crate::record::NoticeFact::Reap { cause, .. },
                    }) if cause == "idle" => "notice",
                    Record::Forensic(Forensic::Pin { key }) if key == "tasks" => "pin",
                    Record::Forensic(Forensic::Unpin { key }) if key == "tasks" => "unpin",
                    _ => continue,
                }),
                Signal::Transient(_, t) => transients.push(match t {
                    Transient::Pin { key, .. } if key == "tasks" => "pin",
                    Transient::Unpin { key } if key == "tasks" => "unpin",
                    _ => continue,
                }),
            }
        }
        assert_eq!(
            transients,
            ["pin", "unpin"],
            "the pin register's live copy also publishes as a Transient beside its Forensic twin"
        );
        assert_eq!(
            facts,
            ["io", "card", "done", "notice", "pin", "unpin"],
            "every class records its twin, the buffered read flushed ahead of the card"
        );
    }

    /// The protected-`services` guard blocks a model's `` `pin `` write,
    /// never a read: `` `pin-read `` answers a host-authored `"services"`
    /// slot exactly like any other key.
    #[test]
    fn pin_read_of_services_answers() {
        let mirror = fresh_mirror();
        mirror.lock().expect("pin register poisoned").insert(
            "services".to_string(),
            shell_eval::PinDigest::new(crate::bus::card::Card(vec![
                crate::bus::card::Mark::Text {
                    spans: vec![crate::bus::card::Span {
                        role: None,
                        text: "svc".into(),
                    }],
                },
            ])),
        );
        let d = ExarchDesk {
            services: HostServices {
                pins: Some(mirror),
                ..base_services()
            },
        };
        assert!(
            d.handle(pin_read_req("services")).is_ok(),
            "reads are not writes: the protected-pin guard must not reach `pin-read`"
        );
    }

    /// The receipt names the child, and its reply notice lands in the parent's
    /// inbox once it settles — the value itself is fetched with `` `read ``.
    #[test]
    fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
        let (desk, registry, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "agents `reply 'hi from child'",
            )])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let answer = desk
            .handle(start_req(session, "say hi", "helper", "confined", true))
            .expect("a valid `start must succeed");

        let rows = roster(answer);
        assert_eq!(rows.len(), 1);
        assert_eq!(str_field(&rows[0], "name").as_deref(), Some("helper"));
        assert!(
            str_field(&rows[0], "log-dir").is_some(),
            "a roster row carries the agent's log directory"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "the child's reply notice must reach the parent's inbox, got: {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        assert_eq!(
            registry.reply_of(
                desk.services.parent,
                registry.resolve_name("helper").unwrap()
            ),
            Ok(Some(FOValue::String {
                value: "hi from child".into()
            })),
            "the deposited reply must be fetchable off the child's entry"
        );
    }

    /// `` `read `` is the one tag that answers a record rather than the
    /// roster: after a scripted child replies, the parent fetches exactly
    /// what it deposited.
    #[test]
    fn agent_read_answers_the_childs_deposited_reply() {
        let (desk, _registry, parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "agents `reply 'read me'",
            )])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        desk.handle(start_req(session, "say hi", "helper", "confined", true))
            .expect("a valid `start must succeed");
        let _ = wait_for_settle(&parent_inbox);

        let answer = desk
            .handle(family_req("agents", "read", Some(text("helper"))))
            .expect("a descendant that has replied must answer its reply");
        assert_eq!(str_field(&answer, "name").as_deref(), Some("helper"));
        let FOValue::Map { entries } = &answer else {
            panic!("expected a record")
        };
        let reply = entries
            .iter()
            .find_map(|(k, v)| (k == "reply").then(|| v.clone()));
        assert_eq!(
            reply,
            Some(FOValue::String {
                value: "read me".into()
            }),
            "`` `read `` must answer the very value the child handed to `reply"
        );
    }

    /// The state *is* the answer, not a receipt about the child just started:
    /// the roster a spawn answers with carries a sibling that spawn never
    /// touched, so nothing has to ask again to see what the fleet now is.
    #[test]
    fn start_answers_the_whole_roster_not_a_receipt() {
        let (desk, registry, parent_inbox) = spawnable_desk(3);
        desk.services.provider.swap(Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call("r1", "agents `reply 'a'")])),
        )));
        // Registered without a worker, so it never settles out from under the
        // assertion below.
        let already_there =
            crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
                id: crate::agent::fresh_id(),
                name: "already-there".into(),
                log_dir: PathBuf::from("/tmp/already-there"),
                cancel: crate::agent::cancel::Token::new(),
                reach: EvalReach::Identity {
                    eval_root: Some(ral_core::process::DurableRoot::default()),
                    interrupt_target: InterruptTarget::default(),
                },
                mailbox: Inbox::new().mailbox(),
                provider: ProviderHandle::new(scripted_provider()),
                consumer: registry.generation(desk.services.parent),
            });
        let _ = registry.register(Some(desk.services.parent), already_there);

        let root = root_shell();
        let session = desk.services.nursery.park(forkable_child_shell(&root));
        let mut names = roster_names(
            desk.handle(start_req(session, "go", "helper", "confined", false))
                .expect("the spawn must succeed"),
        );
        names.sort();
        assert_eq!(
            names,
            vec!["already-there".to_string(), "helper".to_string()],
            "the spawn answers the registry's state, siblings and all"
        );

        let _ = wait_for_settle(&parent_inbox);
    }

    /// The desk reads the model's record by field name, so a missing field is
    /// named and told what it was for — never a position the model never wrote.
    #[test]
    fn start_refuses_a_spec_missing_a_field_by_name() {
        let err = desk()
            .handle(family_req(
                "agents",
                "start",
                Some(FOValue::Map {
                    entries: vec![
                        (
                            "spec".to_string(),
                            FOValue::Map {
                                entries: vec![
                                    ("prompt".to_string(), text("go")),
                                    ("type".to_string(), bare("amnemon")),
                                    ("grant".to_string(), bare("confined")),
                                    ("search".to_string(), FOValue::Bool { value: false }),
                                ],
                            },
                        ),
                        (
                            "fork".to_string(),
                            FOValue::Variant {
                                label: "parked".to_string(),
                                payload: Some(Box::new(FOValue::Int { value: 0 })),
                            },
                        ),
                    ],
                }),
            ))
            .expect_err("a spec missing `name` must be refused");
        assert!(
            err.message.contains("`name`") && err.message.contains("the child's identity"),
            "the refusal must name the field and say what it is for, got: {}",
            err.message
        );
    }

    /// A request above the parent's ceiling is narrowed, never refused — the
    /// shape [`crate::policy::narrow`] gives `grant`. Only that half is visible
    /// here: the clamped bit lands in a private `Agent` field, so
    /// `agent::build`'s fork test asserts the narrowing itself.
    #[test]
    fn agent_start_admits_a_search_request_above_the_parents_ceiling() {
        let (mut desk, _registry, parent_inbox) = spawnable_desk(3);
        desk.services.search = false;
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "agents `reply 'done'",
            )])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let answer = desk.handle(start_req(session, "go", "searcher", "confined", true));
        assert!(
            answer.is_ok(),
            "a spawn asking for more search reach than its parent holds is narrowed, not refused"
        );
        let _ = wait_for_settle(&parent_inbox);
    }

    /// Read `system_prompt_bytes` off a session's opening bookend — the first
    /// record in its `record.jsonl`.
    fn recorded_system_prompt_bytes(log_dir: &std::path::Path) -> usize {
        let records = crate::record::read_records(&log_dir.join("record.jsonl")).unwrap();
        let first = records
            .into_iter()
            .next()
            .expect("record.jsonl must have at least one record");
        match first {
            crate::record::Record::Protocol(crate::record::Protocol::SessionStarted {
                system_prompt_bytes,
                ..
            }) => system_prompt_bytes,
            other => panic!("first record must be SessionStarted, got {other:?}"),
        }
    }

    /// The recorded length is the child's own resolved system prompt, not the
    /// raw `system_template` [`ExarchDesk::launch`] forks its log from.
    #[test]
    fn agent_start_bookend_records_the_childs_resolved_length() {
        let (mut desk, _registry, parent_inbox) = spawnable_desk(3);
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        desk.services.system_template = template.clone();
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "agents `reply 'hi from child'",
            )])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        // The production seam: the index table resolves from the same booted
        // surface the parked child shells fork from.
        desk.services.index = crate::prompt::BuiltinIndex::resolve(&root);
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);

        let answer = desk
            .handle(start_req(session, "say hi", "helper", "confined", true))
            .expect("a valid `start must succeed");

        let rows = roster(answer);
        let log_dir = str_field(&rows[0], "log-dir").expect("a roster row carries its log dir");

        let expected = desk
            .services
            .index
            .apply(
                &template,
                &crate::prompt::Grants {
                    returns: true,
                    allow_schedule: desk.services.allow_schedule,
                    spawns: desk.services.fuel.saturating_sub(1) > 0,
                },
            )
            .len();
        assert_eq!(
            recorded_system_prompt_bytes(std::path::Path::new(&log_dir)),
            expected,
            "the bookend must record the spawned child's own resolved \
             system, not the unresolved template HostServices captured"
        );

        // Drain the settle, so no background thread outlives this test.
        let _ = wait_for_settle(&parent_inbox);
    }

    #[test]
    fn agent_start_refuses_at_zero_fuel_with_the_exhaustion_text() {
        let (desk, _registry, _parent_inbox) = spawnable_desk(0);
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(start_req(session, "hi", "helper", "confined", true))
            .expect_err("zero fuel must refuse");
        assert!(
            err.message.contains("no spawn fuel remains"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("Fuel bounds how deep"),
            "must state that fuel bounds depth, not fan-out, got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session).is_some(),
            "a fuel refusal happens before adopt, so the parked fork must \
             stay for the run guard to reap, never claimed by a refused call"
        );
    }

    /// [`AgentRegistry::name_live`] catches the ordinary case before the parked
    /// fork is ever adopted, so the refused spawn registers no child.
    #[test]
    fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
        let (desk, registry, _parent_inbox) = spawnable_desk(3);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call("r1", "agents `reply 'a'")])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        let shell1 = forkable_child_shell(&root);
        let session1 = desk.services.nursery.park(shell1);
        let answer = desk.handle(start_req(session1, "go", "helper", "confined", true));
        assert!(answer.is_ok(), "the first spawn must succeed");

        let shell2 = forkable_child_shell(&root);
        let session2 = desk.services.nursery.park(shell2);
        let err = desk
            .handle(start_req(session2, "go again", "helper", "confined", true))
            .expect_err("a second spawn naming a live agent must be refused");
        assert!(
            err.message.contains("already bears the name 'helper'"),
            "got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session2).is_some(),
            "a name-collision refusal happens before adopt, so the parked \
             fork must stay for the run guard to reap, never claimed by a \
             refused call"
        );
        assert_eq!(
            registry.list(desk.services.parent).len(),
            1,
            "the second, refused spawn must register no child"
        );
    }

    /// The in-process door checks a name, but a wire peer need not have come
    /// through one — so the spawn spine checks it too, before a log is forked
    /// or a listener dialled, rather than leaving it all to `register`.
    #[test]
    fn agent_start_refuses_a_malformed_name_before_it_adopts_the_fork() {
        let (desk, registry, _parent_inbox) = spawnable_desk(3);

        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(start_req(session, "go", "help/er", "confined", true))
            .expect_err("a malformed name must be refused");
        assert!(
            err.message.contains("ASCII letters"),
            "the refusal must carry the name rule; got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session).is_some(),
            "the name is refused before adopt, so the parked fork must stay \
             for the run guard to reap"
        );
        assert!(
            registry.list(desk.services.parent).is_empty(),
            "a refused spawn registers no child"
        );
    }

    /// Siblings in one turn all succeed off the same captured fuel, since the
    /// parent's own is never debited.
    #[test]
    fn fuel_bounds_depth_not_fanout() {
        let (desk, _registry, parent_inbox) = spawnable_desk(1);
        let provider = Arc::new(Provider::scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("r1", "agents `reply 'a'")]))
                .then(Reply::tool_calls(vec![ral_call("r2", "agents `reply 'b'")]))
                .then(Reply::tool_calls(vec![ral_call("r3", "agents `reply 'c'")])),
        ));
        desk.services.provider.swap(provider);

        let root = root_shell();
        for i in 0..3 {
            let shell = forkable_child_shell(&root);
            let session = desk.services.nursery.park(shell);
            let answer = desk.handle(start_req(session, "go", &format!("t{i}"), "confined", true));
            assert!(
                answer.is_ok(),
                "sibling {i} must not be refused for lack of fuel — fuel \
                 bounds depth, not fan-out"
            );
        }

        for _ in 0..3 {
            match wait_for_settle(&parent_inbox) {
                crate::bus::Item::Agent(result) => assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "every sibling must settle by replying, got: {:?}",
                    result.outcome
                ),
                other => panic!("expected an Agent result item, got {other:?}"),
            }
        }
    }

    /// The capture went stale the instant the *calling* session's generation
    /// moved past what was snapshotted at install.
    #[test]
    fn agent_start_refuses_after_clear() {
        let (desk, registry, _parent_inbox) = spawnable_desk(3);
        registry.clear_subtree(desk.services.parent); // the /clear gesture, on this caller
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(start_req(session, "hi", "helper", "confined", true))
            .expect_err("a stale generation must refuse");
        assert!(
            err.message.contains("cleared"),
            "must name the /clear cause, got: {}",
            err.message
        );
    }

    /// A refusal after [`Nursery::adopt`] — here, a bad grant label — drops the
    /// adopted `Shell` rather than leaving it to be adopted twice.
    #[test]
    fn refused_enquiry_leaves_the_nursery_empty() {
        let (desk, _registry, _parent_inbox) = spawnable_desk(3);
        let root = root_shell();
        let shell = forkable_child_shell(&root);
        let session = desk.services.nursery.park(shell);
        let err = desk
            .handle(start_req(session, "hi", "helper", "bogus", true))
            .expect_err("an unknown permissions label must refuse");
        assert!(
            err.message.contains("confined"),
            "must name the legal bases, got: {}",
            err.message
        );
        assert!(
            desk.services.nursery.adopt(session).is_none(),
            "a refusal downstream of adopt must not leave the fork \
             re-adoptable — it was already claimed and simply drops"
        );
    }

    /// Never a sibling, an ancestor, or itself: what [`AgentRegistry`]'s own
    /// tests check inside, checked here at the desk entry point.
    #[test]
    fn message_and_cancel_scope_to_descendants() {
        let (desk_root, registry, _root_inbox) = spawnable_desk(3);
        // Minted, never literals — see `spawnable_desk` on why. The trunk here
        // is the agent `spawnable_desk` named "parent".
        let root_id = desk_root.services.parent;
        let mid = crate::agent::fresh_id();
        let sibling = crate::agent::fresh_id();
        let grandchild = crate::agent::fresh_id();
        // root -> mid -> grandchild, and root -> sibling (mid's sibling).
        for (id, parent, name) in [
            (mid, root_id, "mid"),
            (sibling, root_id, "sibling"),
            (grandchild, mid, "grandchild"),
        ] {
            let agent = crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
                id,
                name: name.to_string(),
                log_dir: PathBuf::from(format!("/tmp/{id}")),
                cancel: crate::agent::cancel::Token::new(),
                reach: EvalReach::Identity {
                    eval_root: Some(ral_core::process::DurableRoot::default()),
                    interrupt_target: InterruptTarget::default(),
                },
                mailbox: Inbox::new().mailbox(),
                provider: ProviderHandle::new(scripted_provider()),
                consumer: registry.generation(parent),
            });
            let _ = registry.register(Some(parent), agent);
        }

        let mut desk1 = desk_root;
        desk1.services.parent = mid;

        let err = desk1
            .handle(message_req("sibling", "hi"))
            .expect_err("a message to a sibling must be refused");
        assert_eq!(
            err.message,
            "agent 'sibling' is not an agent you started; `agents `message` may only reach a descendant of yours"
        );

        assert!(
            desk1.handle(message_req("grandchild", "hi")).is_ok(),
            "a message to a proper descendant must succeed"
        );

        let cancel_err = desk1
            .handle(family_req("agents", "cancel", Some(text("parent"))))
            .expect_err("cancelling an ancestor must be refused");
        assert_eq!(
            cancel_err.message,
            "agent 'parent' is not an agent you started; `agents `cancel` may only reach a descendant of yours"
        );

        assert!(
            desk1
                .handle(family_req("agents", "cancel", Some(text("grandchild"))))
                .is_ok(),
            "cancelling a proper descendant must succeed"
        );
    }

    /// `desk_binding_drains_pending_surfaces_before_handling`'s law again, but
    /// through a real spawn rather than a stub class.
    #[test]
    fn surface_then_spawn_observes_the_surface_first() {
        let mut session = crate::agent::Avatar::for_test("system").unwrap();
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, session.agent.id);
        let _ = session.run_shell(
            "call-1".to_string(),
            r#"surface `unpin [key: "test-marker"]; agents `start [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined, search: true]"#,
            5,
            &emit,
        );
        drop(emit);

        let mut saw_unpin = false;
        let mut saw_spawn = false;
        while let Ok(sig) = rx.try_recv() {
            match sig {
                Signal::Transient(_, Transient::Unpin { .. }) => saw_unpin = true,
                Signal::Fact(_, fact) => {
                    if let Record::Display(Display::HarnessCall { verb, .. }) = fact.value()
                        && verb == "spawn"
                    {
                        assert!(
                            saw_unpin,
                            "the surfaced unpin must be observed before the spawn's HarnessCall line"
                        );
                        saw_spawn = true;
                    }
                }
                Signal::Transient(..) => {}
            }
        }
        assert!(
            saw_spawn,
            "the agent spawn's HarnessCall chrome must have been emitted"
        );
    }

    /// Refused before the payload is even decoded, naming the flag that grants
    /// — and naming the tag the model typed, not a wire word it never saw.
    #[test]
    fn every_schedule_tag_is_refused_in_the_models_own_vocabulary() {
        for (request, tag) in [
            (add_req("1s", "nightly", "wake"), "schedules `add"),
            (family_req("schedules", "list", None), "schedules `list"),
            (
                family_req("schedules", "remove", Some(text("sched-0"))),
                "schedules `remove",
            ),
        ] {
            let err = desk()
                .handle(request)
                .expect_err("every schedule tag is refused without the grant");
            assert!(
                err.message.starts_with(&format!("`{tag}` refused:")),
                "the refusal must name the tag the model typed, got: {}",
                err.message
            );
            assert!(
                err.message.contains("--allow-schedule"),
                "must name the grant flag, got: {}",
                err.message
            );
        }
    }

    /// A schedule with no label is refused, naming the missing field, and
    /// registers nothing. The field is missing by *name*: the desk reads the
    /// model's record the way the door wrote it.
    #[test]
    fn schedule_without_a_label_is_refused() {
        let desk = granted_desk();
        let err = desk
            .handle(family_req(
                "schedules",
                "add",
                Some(FOValue::Map {
                    entries: vec![
                        (
                            "trigger".to_string(),
                            FOValue::Variant {
                                label: "after".to_string(),
                                payload: Some(Box::new(text("1s"))),
                            },
                        ),
                        ("prompt".to_string(), text("wake")),
                    ],
                }),
            ))
            .expect_err("a schedule with no label must be refused");
        assert!(
            err.message.contains("`label`"),
            "must name the missing field, got: {}",
            err.message
        );
        assert!(
            desk.services.schedules.list().is_empty(),
            "the refused attempt registers nothing"
        );
    }

    /// Arm one, see the answer already list it with the fields it was given,
    /// remove it by label, see the table empty again — every tag answers the
    /// table, so no tag needs a second call to learn what it did.
    #[test]
    fn schedules_lists_what_add_registered() {
        let desk = granted_desk();
        let rows = table(
            desk.handle(add_req("2h", "nightly", "wake"))
                .expect("a valid `add must succeed"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(str_field(&rows[0], "label").as_deref(), Some("nightly"));
        assert_eq!(str_field(&rows[0], "trigger").as_deref(), Some("after 2h"));
        assert!(
            !matches!(&rows[0], FOValue::Map { entries } if entries.iter().any(|(k, _)| k == "id")),
            "the table carries no id"
        );

        let listed = table(
            desk.handle(family_req("schedules", "list", None))
                .expect("`list must succeed"),
        );
        assert_eq!(listed.len(), 1, "`list sees what `add armed");

        let after_removal = table(
            desk.handle(family_req("schedules", "remove", Some(text("nightly"))))
                .expect("`remove must succeed"),
        );
        assert!(
            after_removal.is_empty(),
            "the wakeup must be gone from the very table `remove answers"
        );
    }

    /// Refused before a second schedule is registered.
    #[test]
    fn schedule_at_the_desk_refuses_a_duplicate_label() {
        let desk = granted_desk();
        desk.handle(add_req("1s", "nightly", "wake"))
            .expect("the first schedule must succeed");
        let err = desk
            .handle(add_req("1s", "nightly", "wake"))
            .expect_err("a duplicate label must be refused");
        assert!(err.message.contains("nightly"), "got: {}", err.message);
        assert_eq!(
            desk.services.schedules.list().len(),
            1,
            "the duplicate registers nothing"
        );
    }

    /// The row goes up after the registry call precisely so `failed` can carry
    /// the outcome; emitted before, every schedule would read as one that
    /// landed.
    #[test]
    fn a_refused_schedule_tiers_its_act_row() {
        let (tx, rx) = channel();
        let mut desk = granted_desk();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);
        desk.handle(add_req("1s", "nightly", "wake"))
            .expect("the first schedule must succeed");
        desk.handle(add_req("1s", "nightly", "wake"))
            .expect_err("a duplicate label must be refused");

        let acts: Vec<(String, bool)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall {
                    verb,
                    subject,
                    payload,
                    failed,
                }) if verb == "schedule" => {
                    assert_eq!(subject.as_deref(), Some("nightly"));
                    Some((payload, failed))
                }
                _ => None,
            })
            .collect();
        assert_eq!(acts.len(), 2, "both attempts draw a row: {acts:?}");
        assert!(!acts[0].1, "the schedule that landed is not tiered");
        assert_eq!(acts[0].0, "after 1s", "a landed row carries its trigger");
        assert!(acts[1].1, "the refused schedule is tiered hot");
        assert!(
            acts[1].0.starts_with("refused: "),
            "a refusal states itself in the payload: {:?}",
            acts[1].0
        );
    }

    // ── `reply` ───────────────────────────────────────────────────────────

    /// Refused before the payload is decoded, and the cell is left empty.
    #[test]
    fn reply_refused_without_returns() {
        let (emit, _rx) = crate::bus::dummy_emitter();
        let mut d = desk();
        d.services.emit = emit;
        d.services.returns = false;
        let err = d
            .handle(family_req(
                "agents",
                "reply",
                Some(FOValue::List {
                    items: vec![FOValue::Int { value: 1 }],
                }),
            ))
            .expect_err("a non-returning agent's reply must be refused");
        assert!(
            err.message
                .contains("you converse with the user; you do not return"),
            "got: {}",
            err.message
        );
        assert!(
            d.services.reply.take().is_none(),
            "a refused reply must never reach the cell"
        );
    }

    /// Each call also puts a subject-less act on the rail, carrying its value.
    #[test]
    fn reply_stages_the_payload_last_write_wins() {
        let (tx, rx) = channel();
        let mut d = desk();
        d.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        d.services.emit = Emitter::new(tx, 0);

        for text in ["first", "second"] {
            d.handle(family_req(
                "agents",
                "reply",
                Some(FOValue::List {
                    items: vec![FOValue::String { value: text.into() }],
                }),
            ))
            .expect("a returning agent's reply must succeed");
        }

        assert_eq!(
            d.services.reply.take(),
            Some(FOValue::String {
                value: "second".into()
            }),
            "the last staged reply wins"
        );

        let acts: Vec<String> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall {
                    verb,
                    subject,
                    payload,
                    failed,
                }) if verb == "reply" => {
                    assert_eq!(subject, None, "`reply` addresses no named subject");
                    assert!(!failed, "a staged reply is not a refusal");
                    Some(payload)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            acts,
            ["first", "second"],
            "each reply emits its own act row, carrying the value it staged"
        );
    }

    /// The audit is the model's only account of what survived an unwind, so it
    /// must name every committed act, in the order the call committed them.
    #[test]
    fn the_fragment_keeps_committed_acts_in_the_order_they_landed() {
        let desk = granted_desk();
        desk.handle(add_req("2h", "nightly", "wake"))
            .expect("a valid schedule must succeed");
        desk.handle(family_req("schedules", "remove", Some(text("nightly"))))
            .expect("unscheduling the label just armed must remove it");
        desk.handle(family_req(
            "agents",
            "reply",
            Some(FOValue::List {
                items: vec![FOValue::String {
                    value: "done".into(),
                }],
            }),
        ))
        .expect("a returning agent's reply must succeed");

        assert_eq!(
            desk.services.acts.audit().as_deref(),
            Some(
                "audit: this call had already armed the wakeup 'nightly'; removed the wakeup \
                 'nightly'; staged your reply; that work stands — do not repeat it.\n"
            )
        );
    }

    /// A refused act changed nothing, so the fragment stays silent: an entry
    /// here would tell the model to leave standing work it never did.
    #[test]
    fn a_refused_act_leaves_the_fragment_empty() {
        let desk = desk();
        desk.handle(message_req("nobody", "hi"))
            .expect_err("a message to an unknown name must be refused");
        assert!(
            desk.services.acts.audit().is_none(),
            "a call that committed nothing owes the model no audit"
        );
    }

    /// The rail parity a seventh act cannot break by construction: a landed
    /// and a refused attempt both draw their row, but only the landed one
    /// reaches the fragment.
    #[test]
    fn rail_draws_every_attempt_the_fragment_holds_only_what_landed() {
        let (tx, rx) = channel();
        let mut desk = granted_desk();
        desk.services.log.lock().record_emitter().attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        desk.services.emit = Emitter::new(tx, 0);

        desk.handle(add_req("1s", "nightly", "wake"))
            .expect("a valid schedule must land");
        desk.handle(message_req("nobody", "hi"))
            .expect_err("a message to an unknown name must be refused");

        let rows: Vec<(String, bool)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall { verb, failed, .. }) => Some((verb, failed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            rows.iter()
                .map(|(v, f)| (v.as_str(), *f))
                .collect::<Vec<_>>(),
            vec![("schedule", false), ("message", true)],
            "the rail draws one row per attempt, landed or refused"
        );

        let audit = desk
            .services
            .acts
            .audit()
            .expect("the landed schedule owes an audit");
        assert!(
            audit.contains("armed the wakeup") && !audit.contains("message"),
            "the fragment carries the landed schedule alone, got: {audit}"
        );
    }

    // ── engaged-child lifecycle ────────────────────────────────────────────

    /// The registration half of `spawn_async`: a real spawn bakes `name` into
    /// `child`'s own construction, which a plain [`Avatar::fork`] does not,
    /// so this stamps it on directly first — the test-only escape hatch
    /// [`Agent::set_name`] exists for.
    fn register_child(parent_id: AgentId, name: &str, child: &mut Avatar) -> u64 {
        child.agent_mut().set_name(name);
        child
            .agents
            .register(Some(parent_id), child.agent.clone())
            .expect("a fresh child of a live parent always registers");
        child.agent.consumer()
    }

    /// Attend `child` to completion on a detached thread, in `spawn_async`'s own
    /// worker-epilogue order: a `` `reply ``'s notice already rode `attend`'s own
    /// deposit, so only a non-reply outcome is delivered here before the settle.
    fn attend_and_deliver(
        mut child: Avatar,
        name: &str,
        generation: u64,
        parent_mailbox: Mailbox,
    ) -> std::thread::JoinHandle<()> {
        let id = child.agent.id;
        let registry = child.agents.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            let (tx, _rx) = crate::bus::channel();
            let emit = Emitter::new(tx, id);
            let (outcome, _payload) = child.attend(&mut crate::agent::NoControl, &emit);
            if !matches!(outcome, crate::bus::AgentOutcome::Replied) {
                let _ =
                    parent_mailbox.push(crate::bus::Post::AgentResult(crate::bus::AgentResult {
                        name,
                        outcome,
                        elapsed: Duration::default(),
                        generation,
                    }));
            }
            registry.settle(id);
        })
    }

    /// A live, never-attended child: just enough for
    /// [`AgentRegistry::has_children`] to read true, so `parent_id` parks
    /// [`crate::bus::ParkMode::HeldByChildren`] instead of quiescing.
    fn register_keepalive(
        registry: &AgentRegistry,
        parent_id: AgentId,
        log_dir: &std::path::Path,
    ) -> AgentId {
        let id = crate::agent::fresh_id();
        let agent = crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
            id,
            name: format!("keepalive-{parent_id}"),
            log_dir: log_dir.to_path_buf(),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: ProviderHandle::new(scripted_provider()),
            consumer: registry.generation(parent_id),
        });
        let _ = registry.register(Some(parent_id), agent);
        id
    }

    /// Poll `path` until it contains `needle` or `timeout` elapses. A child's
    /// `record.jsonl` records every turn, and it is the only channel into a
    /// thread the test does not otherwise touch mid-flight.
    fn eventually_logged(path: &std::path::Path, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Ok(body) = std::fs::read_to_string(path)
                && body.contains(needle)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// The lifecycle turns on [`AgentRegistry::steer`]'s delivery alone and knows
    /// nothing of TUI focus. The keepalive grandchild stops the child quiescing
    /// before it is engaged, so both steers land instead of racing its loop.
    #[test]
    fn engaged_child_answers_a_second_steer_with_no_focus_involved() {
        let dir = tmp("engaged-second-steer");
        let parent = Avatar::for_test("system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.provider_handle().swap(Arc::new(Provider::scripted(
            "test-model",
            Script::new()
                .then(Reply::text("first response, no reply yet"))
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "agents `reply 'second response arrived'",
                )])),
        )));
        child.seed("say hi".into());
        let log_dir = child.log_dir();
        let child_id = child.agent.id;
        let registry = child.agents.clone();

        let generation = register_child(parent.agent.id, "helper", &mut child);
        register_keepalive(&registry, child_id, &dir.path().join("keepalive"));
        let handle = attend_and_deliver(child, "helper", generation, parent.mailbox());

        assert!(
            registry.steer(child_id, "first message".into()),
            "the freshly registered, parked child accepts a steer"
        );
        assert!(
            registry.messageable(child_id),
            "steer renews the exchange clock"
        );
        assert!(
            eventually_logged(
                &log_dir.join("record.jsonl"),
                "first response, no reply yet",
                Duration::from_secs(5),
            ),
            "the child must answer the first steer before the second is sent"
        );

        assert!(
            registry.steer(child_id, "second message".into()),
            "the parked, engaged child still accepts a second steer"
        );
        match wait_for_settle(&parent.inbox()) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "the second steer's reply must notify the parent, got: {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        assert_eq!(
            registry.reply_of(parent.agent.id, child_id),
            Ok(Some(FOValue::String {
                value: "second response arrived".into()
            })),
            "the reply the second steer produced must be the one deposited"
        );
        // A replied child parks for a follow-up; only a terminate ends it.
        registry.cancel(child_id);
        handle.join().expect("worker thread must not panic");
    }

    /// The reaped child delivers exactly one
    /// [`crate::bus::AgentOutcome::Cancelled`] to the parent inbox.
    #[test]
    fn ms_lease_child_never_renewed_is_cancelled_but_a_renewed_one_survives() {
        let dir = tmp("ms-lease-integration");
        let parent = Avatar::for_test("system").unwrap();
        // Child A's ttl must expire well inside the round-trip loop's own
        // `MAX_STEPS` cap, so the lease and not the step cap ends its exchange.
        // Child B's is generous instead: its renewal is paced by `thread::sleep`
        // on the test thread, where jitter stretches a short sleep past nominal.
        let ttl_a = Duration::from_millis(25);
        let ttl_b = Duration::from_millis(300);

        let mut child_a = parent.fork(parent.caps().clone()).expect("fork child a");
        let mut long_script = Script::new();
        for i in 0..2_000u32 {
            long_script = long_script.then(Reply::tool_calls(vec![ral_call(&i.to_string(), "1")]));
        }
        child_a
            .provider_handle()
            .swap(Arc::new(Provider::scripted("test-model", long_script)));
        child_a.seed("go".into());
        let id_a = child_a.agent.id;
        let registry = child_a.agents.clone();
        registry.set_lease(ttl_a);
        let gen_a = register_child(parent.agent.id, "child-a", &mut child_a);
        let handle_a = attend_and_deliver(child_a, "child-a", gen_a, parent.mailbox());

        match wait_for_settle(&parent.inbox()) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Cancelled),
                    "a never-renewed lease reaps mid-exchange with Cancelled, got {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        assert!(!registry.is_live(id_a), "the reaped entry settles");
        handle_a.join().expect("worker thread must not panic");

        // Child B is parked by a keepalive grandchild and given no script to
        // race, so `renew` alone must be what defers its reap.
        let mut child_b = parent.fork(parent.caps().clone()).expect("fork child b");
        let id_b = child_b.agent.id;
        registry.set_lease(ttl_b);
        let gen_b = register_child(parent.agent.id, "child-b", &mut child_b);
        let keepalive_b = register_keepalive(&registry, id_b, &dir.path().join("keepalive-b"));
        let handle_b = attend_and_deliver(child_b, "child-b", gen_b, parent.mailbox());

        std::thread::sleep(ttl_b / 2);
        // A bare stamp, not `steer`: child B's script is empty, so an actual
        // delivery would give its attend loop work it cannot answer.
        registry
            .mailbox(id_b)
            .expect("a live entry renews")
            .stamp_exchange();
        registry
            .mailbox(keepalive_b)
            .expect("so does its keepalive")
            .stamp_exchange();

        std::thread::sleep(ttl_b / 2 + Duration::from_millis(150));
        assert!(
            registry.is_live(id_b),
            "renewed at half the ttl, still alive past the original bound"
        );

        // Wind child B down rather than leave its thread parked forever: per
        // `ParkMode`, a terminate-cause cancel ends an unengaged park at once.
        registry.cancel(id_b);
        let _ = wait_for_settle(&parent.inbox());
        handle_b.join().expect("worker thread must not panic");
    }
}

/// The desk's wire arm: `` `start `` dialling a guest that is listening for
/// exactly one connection, over a fake dialler and a re-exec'd `--engine`
/// child standing in for the hatched guest — the same vehicle the wire seat's
/// own tests already use, since a genuine vsock dial only means anything
/// inside a real guest.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod wire_tests {
    use super::tests::{message_req, roster_names, start_req_forked};
    use super::*;
    use crate::agent::event::AgentLog;
    use crate::agent::testkit::ral_call;
    use crate::bus::Inbox;
    use crate::egress::Egress;
    use crate::fleet::registry::{EvalReach, InterruptTarget};
    use crate::provider::{
        Provider,
        scripted::{Reply, Script},
    };
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex as StdMutex;

    /// Each log takes the next session id, so two of them are never the same
    /// session to the wire.
    fn fresh_log() -> AgentLog {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        AgentLog::for_test(n, "test", &crate::agent::RecordedAccount::for_test("test"))
            .expect("session log")
    }

    fn scripted_provider(script: Script) -> Arc<Provider> {
        Arc::new(Provider::scripted("test-model", script))
    }

    /// Poll `inbox` for the next exchange-boundary item — a spawned child's
    /// settled result lands here.
    fn wait_for_settle(inbox: &Inbox) -> crate::bus::Item {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(item) = inbox.next_item() {
                return item;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child did not settle within the timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Re-exec this binary as a bare `--engine` child holding `guest` on fd 3,
    /// exactly [`crate::agent::seat::tests::spawn_engine`]'s vehicle. The
    /// caller drops its own copy afterwards, as `hatch_over` does, so the
    /// child's death reads as EOF on the host's end.
    fn spawn_engine_on(guest: &UnixStream) -> std::process::Child {
        let guest_fd = guest.as_raw_fd();
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
        cmd.arg("--engine");
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        // SAFETY: runs between fork and exec, calling only async-signal-safe
        // `dup2`/`close`, with no allocation and no locking.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::dup2(guest_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if guest_fd != 3 {
                    libc::close(guest_fd);
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn engine child")
    }

    /// What the guest at the far end of a [`FakeDial`] does with the dial.
    enum Guest {
        /// The whole spine: read the token, spawn the child, then ack — the
        /// order [`ral_core::hatch::hatch_over`] keeps, since the ack is the
        /// claim that the child exists.
        Hatches,
        /// A guest-side hatch that failed: read the token and close, saying
        /// nothing. The host's ack read sees EOF.
        ClosesWithoutAcking,
        /// Nothing is listening on that port.
        Refuses(String),
    }

    /// A fake [`crate::agent::Dial`] that plays the guest. `dial` hands back
    /// one end of a socketpair and leaves a thread on the other end doing
    /// whatever this fake's [`Guest`] says, so the desk under test drives a
    /// real handshake against a real peer.
    struct FakeDial {
        guest: Guest,
        /// Every port `dial` was asked for, in order.
        ports: StdMutex<Vec<u32>>,
        /// Every token a guest thread actually read off the wire.
        tokens: Arc<StdMutex<Vec<u64>>>,
        /// Engine children the guest threads spawned, for the test to reap.
        children: Arc<StdMutex<Vec<std::process::Child>>>,
    }

    impl FakeDial {
        fn new(guest: Guest) -> Arc<Self> {
            Arc::new(Self {
                guest,
                ports: StdMutex::new(Vec::new()),
                tokens: Arc::new(StdMutex::new(Vec::new())),
                children: Arc::new(StdMutex::new(Vec::new())),
            })
        }

        fn ports(&self) -> Vec<u32> {
            self.ports.lock().unwrap().clone()
        }

        /// Recorded before the ack, so a caller that has the ack has these.
        fn tokens(&self) -> Vec<u64> {
            self.tokens.lock().unwrap().clone()
        }

        fn reap(&self) {
            for mut child in self.children.lock().unwrap().drain(..) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl crate::agent::Dial for FakeDial {
        fn dial(&self, port: u32) -> Result<ral_core::wire::WireStream, String> {
            self.ports.lock().unwrap().push(port);
            let hatches = match &self.guest {
                Guest::Refuses(reason) => return Err(reason.clone()),
                Guest::Hatches => true,
                Guest::ClosesWithoutAcking => false,
            };
            let (host, guest) =
                UnixStream::pair().map_err(|e| format!("socketpair for the fake dial: {e}"))?;
            let tokens = self.tokens.clone();
            let children = self.children.clone();
            std::thread::spawn(move || {
                let mut guest = guest;
                let mut claim = [0u8; 8];
                if guest.read_exact(&mut claim).is_err() {
                    return;
                }
                tokens.lock().unwrap().push(u64::from_le_bytes(claim));
                if !hatches {
                    return;
                }
                let child = spawn_engine_on(&guest);
                guest
                    .write_all(&[ral_core::transport::HATCH_ACK])
                    .expect("ack the hatch");
                children.lock().unwrap().push(child);
            });
            Ok(host)
        }
    }

    /// A [`EvalReach::Wire`] with a genuine `ControlSender` behind it — a
    /// disposable `--engine` child adopted and killed at once, since these
    /// fixtures need a real reach value's *shape* but never actually cancel
    /// or interrupt through it.
    fn fake_wire_reach() -> EvalReach {
        let (host, guest) = UnixStream::pair().expect("socketpair standing in for the dial");
        let mut child = spawn_engine_on(&guest);
        drop(guest);
        let transport = ral_core::transport::WireTransport::adopt(
            host,
            ral_core::transport::Liveness::default(),
        )
        .expect("adopt host stream");
        let control = ral_core::transport::Transport::control(&transport).clone();
        let _ = child.kill();
        let _ = child.wait();
        EvalReach::Wire(control)
    }

    /// A wire-seat desk fixture, its parent registered so `` `start ``'s
    /// spawn spine runs end to end, exactly [`super::tests::spawnable_desk`]'s
    /// identity shape with `wire_seat: true` and a dialler installed.
    fn wire_spawnable_desk(fuel: u32, dial: Arc<FakeDial>) -> (ExarchDesk, AgentRegistry, Inbox) {
        let parent_inbox = Inbox::new();
        let registry = AgentRegistry::new();
        let parent_id = crate::agent::fresh_id();
        let parent_agent = crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
            id: parent_id,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/wire-parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: fake_wire_reach(),
            mailbox: parent_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
            consumer: 0,
        });
        let _ = registry.register(None, parent_agent);
        let (emit, _rx) = crate::bus::dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                scratch: None,
                parent: parent_id,
                mailbox: parent_inbox.mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider(Script::new())),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/work"),
                fuel,
                returns: true,
                allow_schedule: false,
                search: true,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                log: LogCell::new(fresh_log()),
                system_template: String::new(),
                index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                    ral_core::io::TerminalState::default(),
                )),
                interactive: false,
                nursery: Nursery::default(),
                generation: registry.generation(parent_id),
                disk_warn_bytes: None,
                egress: Egress::for_test(),
                acts: ActFragment::default(),
                principal: ral_core::host::user(),
                pins: None,
                wire_seat: true,
                dial: Some(dial),
            },
        };
        (desk, registry, parent_inbox)
    }

    /// `` `agents `start `` as a listening engine sends it: the port its
    /// listener is bound to, and the token that listener will check the
    /// host's dial against.
    fn wire_start_req(name: &str, port: u32, token: u64) -> FOValue {
        let fork = FOValue::Variant {
            label: "listening".to_string(),
            payload: Some(Box::new(FOValue::Map {
                entries: vec![
                    (
                        "port".to_string(),
                        FOValue::Int {
                            value: i64::from(port),
                        },
                    ),
                    (
                        "token".to_string(),
                        FOValue::Int {
                            value: token.cast_signed(),
                        },
                    ),
                ],
            })),
        };
        start_req_forked(fork, "go", name, "confined", false)
    }

    /// The whole spine in one exchange: the desk dials the port the enquiry
    /// named, writes the token that enquiry carried, waits for the guest's
    /// ack, and answers a roster the child is already on — after which the
    /// model-driven helper's `reply` lands in the parent's inbox exactly as
    /// an identity spawn's would.
    #[test]
    fn wire_spawn_dials_the_guest_and_delivers_its_result_to_the_parent_inbox() {
        let port = 41_731;
        // High bits set: the token crosses as an `Int` and must come back
        // bit-for-bit, not as an arithmetic value.
        let token = 0xF0DE_BA5E_1234_5678;
        let dial = FakeDial::new(Guest::Hatches);
        let (desk, registry, parent_inbox) = wire_spawnable_desk(3, dial.clone());

        // The assembled child seeds its own provider from the desk's captured
        // handle, so swap it before `` `start `` assembles.
        desk.services
            .provider
            .swap(scripted_provider(Script::new().then(Reply::tool_calls(
                vec![ral_call("r1", "agents `reply 'hi from wire'")],
            ))));

        let answer = desk
            .handle(wire_start_req("helper", port, token))
            .expect("`start must dial, hatch and spawn");
        assert_eq!(
            roster_names(answer),
            vec!["helper".to_string()],
            "the hatched child is on the roster the spawn answers with"
        );
        assert_eq!(
            dial.ports(),
            vec![port],
            "the desk dials the port it was told"
        );
        assert_eq!(
            dial.tokens(),
            vec![token],
            "the guest's listener reads back the very bits the enquiry carried"
        );

        match wait_for_settle(&parent_inbox) {
            crate::bus::Item::Agent(result) => {
                assert!(
                    matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                    "the hatched child's reply notice must reach the parent's inbox, got: {:?}",
                    result.outcome
                );
            }
            other => panic!("expected an Agent result item, got {other:?}"),
        }
        assert_eq!(
            registry.reply_of(
                desk.services.parent,
                registry.resolve_name("helper").unwrap()
            ),
            Ok(Some(FOValue::String {
                value: "hi from wire".into()
            })),
            "the hatched child's deposited reply must be fetchable off its entry"
        );

        dial.reap();
    }

    /// A guest whose own hatch failed closes without acking. The host has a
    /// connection and no child, and must say so rather than register one.
    #[test]
    fn wire_spawn_refuses_when_the_guest_closes_without_acking() {
        let dial = FakeDial::new(Guest::ClosesWithoutAcking);
        let (desk, registry, _parent_inbox) = wire_spawnable_desk(3, dial);
        let parent = desk.services.parent;

        let err = desk
            .handle(wire_start_req("unacked", 41_731, 7))
            .expect_err("an unacknowledged hatch must be refused");
        assert!(
            err.message
                .contains("the guest closed the connection before acknowledging the hatch"),
            "got: {}",
            err.message
        );
        assert!(
            registry.list(parent).is_empty(),
            "a hatch that was never acknowledged names no child on the roster"
        );
    }

    /// A dial the guest refuses — nothing bound on that port — is refused
    /// with a sentence naming the dial, so the builtin's own listener thread
    /// learns why it was never reached.
    #[test]
    fn wire_spawn_refuses_naming_the_dial_when_the_guest_refuses_it() {
        let dial = FakeDial::new(Guest::Refuses("connection reset by peer".to_string()));
        let (desk, _registry, _parent_inbox) = wire_spawnable_desk(3, dial);

        let err = desk
            .handle(wire_start_req("never-reached", 41_731, 7))
            .expect_err("a refused dial must be refused");
        assert!(
            err.message.contains("could not dial") && err.message.contains("41731"),
            "must name the dial and the port, got: {}",
            err.message
        );
    }

    /// The identity tag on a wire desk is a guest claiming to be in process.
    /// Refused, and before anything is dialled.
    #[test]
    fn wire_spawn_refuses_a_parked_fork_without_dialling() {
        let dial = FakeDial::new(Guest::Hatches);
        let (desk, _registry, _parent_inbox) = wire_spawnable_desk(3, dial.clone());

        let err = desk
            .handle(super::tests::start_req(
                NurseryId(0),
                "go",
                "in-process",
                "confined",
                false,
            ))
            .expect_err("a wire desk has no nursery to adopt a parked fork from");
        assert!(
            err.message.contains("`listening [port, token]`"),
            "must name the tag this host does take, got: {}",
            err.message
        );
        assert!(
            dial.ports().is_empty(),
            "a fork this desk cannot reach is refused before any dial"
        );
    }

    /// The peer-messaging pin: one identity-reach and one wire-reach entry in
    /// the same registry exchange marked notes through the desk's `message`
    /// handler, which touches only the registry and a mailbox — never a
    /// seat — so sender and recipient never learn each other's transport.
    #[test]
    fn identity_and_wire_peers_exchange_messages_through_one_desk() {
        let registry = AgentRegistry::new();
        let parent_id = crate::agent::fresh_id();
        let parent_agent = crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
            id: parent_id,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/peer-parent"),
            cancel: crate::agent::cancel::Token::new(),
            reach: EvalReach::Identity {
                eval_root: Some(ral_core::process::DurableRoot::default()),
                interrupt_target: InterruptTarget::default(),
            },
            mailbox: Inbox::new().mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
            consumer: 0,
        });
        let _ = registry.register(None, parent_agent);

        let identity_inbox = Inbox::new();
        let identity_id = crate::agent::fresh_id();
        let identity_agent =
            crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
                id: identity_id,
                name: "identity-peer".into(),
                log_dir: PathBuf::from("/tmp/peer-identity"),
                cancel: crate::agent::cancel::Token::new(),
                reach: EvalReach::Identity {
                    eval_root: Some(ral_core::process::DurableRoot::default()),
                    interrupt_target: InterruptTarget::default(),
                },
                mailbox: identity_inbox.mailbox(),
                provider: ProviderHandle::new(scripted_provider(Script::new())),
                consumer: registry.generation(parent_id),
            });
        let _ = registry.register(Some(parent_id), identity_agent);

        let wire_inbox = Inbox::new();
        let wire_id = crate::agent::fresh_id();
        let wire_agent = crate::agent::testkit::test_agent(crate::agent::testkit::TestAgentSpec {
            id: wire_id,
            name: "wire-peer".into(),
            log_dir: PathBuf::from("/tmp/peer-wire"),
            cancel: crate::agent::cancel::Token::new(),
            reach: fake_wire_reach(),
            mailbox: wire_inbox.mailbox(),
            provider: ProviderHandle::new(scripted_provider(Script::new())),
            consumer: registry.generation(parent_id),
        });
        let _ = registry.register(Some(parent_id), wire_agent);

        let (emit, _rx) = crate::bus::dummy_emitter();
        let desk = ExarchDesk {
            services: HostServices {
                registry: registry.clone(),
                scratch: None,
                parent: parent_id,
                mailbox: Inbox::new().mailbox(),
                emit,
                provider: ProviderHandle::new(scripted_provider(Script::new())),
                caps: Capabilities::root(),
                cwd: PathBuf::from("/"),
                fuel: 3,
                returns: true,
                allow_schedule: false,
                search: true,
                reply: ReplyCell::default(),
                schedules: ScheduleRegistry::new(),
                log: LogCell::new(fresh_log()),
                system_template: String::new(),
                index: crate::prompt::BuiltinIndex::resolve(&ral_core::Shell::new(
                    ral_core::io::TerminalState::default(),
                )),
                interactive: false,
                nursery: Nursery::default(),
                generation: registry.generation(parent_id),
                disk_warn_bytes: None,
                egress: Egress::for_test(),
                acts: ActFragment::default(),
                principal: ral_core::host::user(),
                pins: None,
                wire_seat: false,
                dial: None,
            },
        };

        desk.handle(message_req("identity-peer", "note for identity"))
            .expect("the parent may message its identity-reach descendant");
        desk.handle(message_req("wire-peer", "note for wire"))
            .expect("the parent may message its wire-reach descendant");

        match identity_inbox.next_item() {
            Some(crate::bus::Item::Message(m)) => {
                assert_eq!(m.text, "note for identity");
            }
            other => panic!("expected an AgentMessage item, got {other:?}"),
        }
        match wire_inbox.next_item() {
            Some(crate::bus::Item::Message(m)) => {
                assert_eq!(m.text, "note for wire");
            }
            other => panic!("expected an AgentMessage item, got {other:?}"),
        }
    }
}
