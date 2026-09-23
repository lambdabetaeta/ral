//! The one vocabulary exarch's enquiries speak: every class, tag and field a
//! harness builtin's door sends and [`super::desk`] answers, and the answers
//! owed back. The door builds a [`Request`] and the desk decodes one, so an
//! ill-shaped request is refused in the same words at either end.

use crate::agent::event::{ContextSurvey, TranscriptPart};
use crate::bus::card::{Card, encode_card, value_to_card};
use crate::fleet::check_name;
use crate::fleet::roster::{AgentInfo, RosterState, Spawner, Summary};
use crate::fleet::schedule::{CronSchedule, ScheduleInfo, Trigger, fmt_duration, parse_duration};
use ral_core::SpawnGrant;
use ral_core::record;
use ral_core::serial::FOValue;
use ral_core::serial::datum::{Datum, exact_keys, field, tag, untag};
use ral_core::types::NurseryId;
use regex::Regex;
use std::time::Duration;

/// One enquiry, as it crosses from a harness builtin's door to the desk.
pub(crate) enum Request {
    Agents(Agents),
    Schedules(Schedules),
    Pins(Pins),
    Context(Context),
    Transcript(Transcript),
}

/// The tags beneath one enquiry class, which the model calls `exarch-<class>`.
pub(crate) trait Family: Sized {
    const CLASS: &'static str;
    /// The tags a model may write, as a refusal offers them.
    const TAGS: &'static [&'static str];
    fn request(self) -> Request;
    fn encode(self) -> FOValue;
    /// `None` for a tag the family does not know.
    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, String>>;
}

/// A model's argument to `exarch-<class>`, read as the request it makes.
///
/// # Errors
/// The refusal, naming the builtin and tag it was written against.
pub(crate) fn family<F: Family>(word: &FOValue) -> Result<F, String> {
    read_tag::<F, F>(word, F::decode)
}

fn read_tag<F: Family, T>(
    word: &FOValue,
    decode: impl FnOnce(&str, Option<&FOValue>) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let class = F::CLASS;
    let offered = || offer(F::TAGS);
    let (label, payload) = untag(word).ok_or_else(|| {
        format!(
            "`exarch-{class}` takes a tag naming what to do — {} — got {}",
            offered(),
            word.shape()
        )
    })?;
    decode(label, payload)
        .ok_or_else(|| {
            format!(
                "unrecognised tag in `exarch-{class} `{label}` — `exarch-{class}` takes {}",
                offered()
            )
        })?
        .map_err(|why| format!("`exarch-{class} `{label}`: {why}"))
}

fn offer(tags: &[&str]) -> String {
    let tags: Vec<String> = tags.iter().map(|t| format!("`{t}")).collect();
    match tags.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => tags.concat(),
    }
}

fn bare(payload: Option<&FOValue>) -> Result<(), String> {
    payload.map_or(Ok(()), |p| {
        Err(format!("takes no payload, got {}", p.shape()))
    })
}

fn carried(payload: Option<&FOValue>) -> Result<&FOValue, String> {
    payload.ok_or_else(|| "requires a payload".to_string())
}

fn datum<T: Datum>(payload: Option<&FOValue>) -> Result<T, String> {
    carried(payload).and_then(T::decode)
}

fn class<F: Family>(family: F) -> FOValue {
    tag(F::CLASS, Some(family.encode()))
}

impl Datum for Request {
    fn encode(self) -> FOValue {
        match self {
            Self::Agents(f) => class(f),
            Self::Schedules(f) => class(f),
            Self::Pins(f) => class(f),
            Self::Context(f) => class(f),
            Self::Transcript(f) => class(f),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let (label, word) = untag(v).ok_or_else(|| {
            format!(
                "an enquiry must be a variant naming its class, got {}",
                v.shape()
            )
        })?;
        let word = || {
            word.ok_or_else(|| {
                format!("`exarch-{label}` takes a tag naming what to do, got no payload at all")
            })
        };
        match label {
            Agents::CLASS => family(word()?).map(Self::Agents),
            Schedules::CLASS => family(word()?).map(Self::Schedules),
            Pins::CLASS => family(word()?).map(Self::Pins),
            Context::CLASS => family(word()?).map(Self::Context),
            Transcript::CLASS => family(word()?).map(Self::Transcript),
            other => Err(format!("unrecognised enquiry class `{other}`")),
        }
    }
}

impl Request {
    /// How the answer this request is owed is checked, named before the
    /// request crosses.
    pub(crate) fn owed(&self) -> fn(&FOValue) -> Result<(), String> {
        match self {
            Self::Agents(Agents::List) => is::<Vec<AgentInfo>>,
            Self::Agents(Agents::Read(_)) => is::<Deposit>,
            Self::Agents(Agents::Branch(_)) | Self::Pins(Pins::Set(_) | Pins::Clear(_)) => unit,
            Self::Agents(_) => is::<Summary>,
            Self::Schedules(_) => is::<Vec<ScheduleInfo>>,
            Self::Pins(Pins::Read(_)) => pinned,
            Self::Pins(Pins::List) => is::<Vec<String>>,
            Self::Context(_) => is::<Survey>,
            Self::Transcript(Transcript::Index) => is::<Vec<Indexed>>,
            Self::Transcript(Transcript::Read(_)) => is::<Vec<Material>>,
            Self::Transcript(Transcript::Grep(_)) => is::<Hits>,
        }
    }
}

fn is<T: Datum>(v: &FOValue) -> Result<(), String> {
    T::decode(v).map(drop)
}

fn unit(v: &FOValue) -> Result<(), String> {
    match v {
        FOValue::Unit => Ok(()),
        other => Err(format!("expected (), got {}", other.shape())),
    }
}

/// A slot's card, or `()` for an empty one.
fn pinned(v: &FOValue) -> Result<(), String> {
    unit(v).or_else(|_| is::<Card>(v))
}

// ── `exarch-agents` ──────────────────────────────────────────────────────

pub(crate) enum Agents {
    List,
    Start(Start),
    Message(Message),
    Cancel(String),
    Reply(FOValue),
    Read(String),
    /// The host's own `/branch`, whose door is `_exarch-branch`; no model
    /// is offered it.
    Branch(ForkClaim),
}

impl Family for Agents {
    const CLASS: &'static str = "agents";
    const TAGS: &'static [&'static str] = &["list", "start", "message", "cancel", "reply", "read"];

    fn request(self) -> Request {
        Request::Agents(self)
    }

    fn encode(self) -> FOValue {
        match self {
            Self::List => tag("list", None),
            Self::Start(start) => tag("start", Some(start.encode())),
            Self::Message(message) => tag("message", Some(message.encode())),
            Self::Cancel(name) => tag("cancel", Some(name.encode())),
            Self::Reply(value) => tag("reply", Some(value)),
            Self::Read(name) => tag("read", Some(name.encode())),
            Self::Branch(fork) => tag("branch", Some(fork.encode())),
        }
    }

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, String>> {
        Some(match label {
            "list" => bare(payload).map(|()| Self::List),
            "start" => datum(payload).map(Self::Start),
            "message" => datum(payload).map(Self::Message),
            "cancel" => datum(payload).map(Self::Cancel),
            "reply" => carried(payload).cloned().map(Self::Reply),
            "read" => datum(payload).map(Self::Read),
            "branch" => datum(payload).map(Self::Branch),
            _ => return None,
        })
    }
}

/// A model's own `exarch-agents` argument: a whole request, or a spawn's
/// spec alone, since the fork it needs is its door's to mint.
pub(crate) enum Word {
    Ask(Agents),
    Spawn(Launch),
}

impl Word {
    /// # Errors
    /// The refusal, in the words the desk would use.
    pub(crate) fn decode(word: &FOValue) -> Result<Self, String> {
        read_tag::<Agents, _>(word, |label, payload| match label {
            "start" => Some(datum(payload).map(Self::Spawn)),
            label => <Agents as Family>::decode(label, payload).map(|ask| ask.map(Self::Ask)),
        })
    }
}

/// `` `start ``: the model's spec, and how its forked session is reached.
pub(crate) struct Start {
    pub(crate) spec: Launch,
    pub(crate) fork: ForkClaim,
}

record!(Start {
    spec: "spec",
    fork: "fork",
});

/// The model's own spawn record.
pub(crate) struct Launch {
    pub(crate) prompt: String,
    pub(crate) name: Name,
    pub(crate) memory: Memory,
    pub(crate) grant: Grant,
    pub(crate) search: bool,
    pub(crate) provider: Selection,
    pub(crate) model: Selection,
}

record!(Launch {
    prompt: "prompt",
    name: "name",
    memory: "type",
    grant: "grant",
    search: "search",
    provider: "provider",
    model: "model",
});

/// An agent's name, admitted by [`check_name`]; [`crate::fleet::Fleet::enrol`]
/// is what makes that unskippable.
pub(crate) struct Name(pub(crate) String);

impl Datum for Name {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let name = String::decode(v)?;
        check_name(&name)?;
        Ok(Self(name))
    }
}

/// Whether a child starts blank or inherits its parent's conversation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Memory {
    Amnemon,
    Mnemon,
}

impl Datum for Memory {
    fn encode(self) -> FOValue {
        match self {
            Self::Amnemon => tag("amnemon", None),
            Self::Mnemon => tag("mnemon", None),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("amnemon", None)) => Ok(Self::Amnemon),
            Some(("mnemon", None)) => Ok(Self::Mnemon),
            _ => Err(format!(
                "expected `amnemon (blank context) or `mnemon (inherits your conversation), got {}",
                v.shape()
            )),
        }
    }
}

/// The one layer a child's stack gains. `` `restrict ``'s record is carried
/// as data: the capability vocabulary is the engine's, and every form only
/// ever narrows.
pub(crate) struct Grant(pub(crate) SpawnGrant);

impl Datum for Grant {
    fn encode(self) -> FOValue {
        match self.0 {
            SpawnGrant::Inherit => tag("inherit", None),
            SpawnGrant::Base(base) => tag(&base, None),
            SpawnGrant::Restrict(record) => tag("restrict", Some(record)),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("inherit", None)) => Ok(Self(SpawnGrant::Inherit)),
            Some((base, None)) if crate::policy::SPAWN_BASES.contains(&base) => {
                Ok(Self(SpawnGrant::Base(base.to_string())))
            }
            Some(("restrict", Some(record @ FOValue::Map { .. }))) => {
                Ok(Self(SpawnGrant::Restrict(record.clone())))
            }
            Some(("restrict", Some(other))) => Err(format!(
                "`restrict must carry a capability record [exec, fs, net, detach, editor, shell], got {}",
                other.shape()
            )),
            _ => Err(format!(
                "expected `inherit, {}, or `restrict [exec: …, fs: …, net: …, detach: …, editor: …, \
                 shell: …] — `inherit is how you decline to narrow at all, and `restrict must carry \
                 a capability record of the same shape `grant [...]` takes, every key optional — got {}",
                crate::policy::SPAWN_BASES
                    .map(|b| format!("`{b}"))
                    .join(", "),
                v.shape()
            )),
        }
    }
}

/// One half of a child's model selection: the spawner's own, or one named
/// outright. ral has no optional field, so the absence of a choice is data.
pub(crate) enum Selection {
    Inherit,
    Named(String),
}

impl Datum for Selection {
    fn encode(self) -> FOValue {
        match self {
            Self::Inherit => tag("inherit", None),
            Self::Named(name) => tag("named", Some(name.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("inherit", None)) => Ok(Self::Inherit),
            Some(("named", Some(FOValue::String { value }))) if !value.is_empty() => {
                Ok(Self::Named(value.clone()))
            }
            Some(("named", payload)) => Err(format!(
                "`named must carry a non-empty Str, got {}",
                payload.map_or_else(|| "nothing".to_string(), FOValue::shape)
            )),
            _ => Err(format!(
                "expected `inherit (whatever you are running on) or `named '<name>', got {}",
                v.shape()
            )),
        }
    }
}

/// How a fork its door minted reaches the desk. Which arm is legal is the
/// host's fact, not the guest's.
pub(crate) enum ForkClaim {
    /// Parked in this host's own nursery, under this id.
    Parked(NurseryId),
    /// Its engine listens on `port`, handing its connection only to a dial
    /// that writes `token` first.
    Listening { port: u32, token: u64 },
}

impl Datum for ForkClaim {
    fn encode(self) -> FOValue {
        match self {
            Self::Parked(id) => tag("parked", Some(id.0.encode())),
            Self::Listening { port, token } => tag(
                "listening",
                Some(FOValue::Map {
                    entries: vec![
                        ("port".into(), u64::from(port).encode()),
                        // Bit-preserving: the eight bytes the host writes are
                        // the eight the listener compares.
                        (
                            "token".into(),
                            FOValue::Int {
                                value: token.cast_signed(),
                            },
                        ),
                    ],
                }),
            ),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("parked", Some(id))) => u64::decode(id).map(|id| Self::Parked(NurseryId(id))),
            Some(("listening", Some(dial))) => {
                exact_keys(dial, &["port", "token"])?;
                let port: u64 = field(dial, "port")?;
                let port =
                    u32::try_from(port).map_err(|_| format!("`port: {port} is no vsock port"))?;
                let Some(FOValue::Int { value }) = dial.field("token") else {
                    return Err(format!("`token: expected an Int, in {}", dial.shape()));
                };
                Ok(Self::Listening {
                    port,
                    token: value.cast_unsigned(),
                })
            }
            _ => Err(format!(
                "expected `parked <nursery id> or `listening [port, token], got {}",
                v.shape()
            )),
        }
    }
}

pub(crate) struct Message {
    pub(crate) to: String,
    pub(crate) text: String,
}

record!(Message {
    to: "to",
    text: "text",
});

/// What `` `read `` fetches: a descendant's name and the value it replied.
pub(crate) struct Deposit {
    pub(crate) name: String,
    pub(crate) reply: FOValue,
}

impl Datum for Deposit {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("name".into(), self.name.encode()),
                ("reply".into(), self.reply),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["name", "reply"])?;
        Ok(Self {
            name: field(v, "name")?,
            reply: v.field("reply").cloned().ok_or("no `reply field")?,
        })
    }
}

record!(Summary {
    live: "live",
    replied: "replied",
});

impl Datum for AgentInfo {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("name".into(), self.name.encode()),
                ("spawner".into(), self.spawner.encode()),
                ("state".into(), self.state.encode()),
                ("idle-s".into(), self.idle.as_secs().encode()),
                ("elapsed-s".into(), self.elapsed.as_secs().encode()),
                (
                    "log-dir".into(),
                    self.log_dir.display().to_string().encode(),
                ),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(
            v,
            &["name", "spawner", "state", "idle-s", "elapsed-s", "log-dir"],
        )?;
        Ok(Self {
            name: field(v, "name")?,
            spawner: field(v, "spawner")?,
            state: field(v, "state")?,
            idle: Duration::from_secs(field(v, "idle-s")?),
            elapsed: Duration::from_secs(field(v, "elapsed-s")?),
            log_dir: field::<String>(v, "log-dir")?.into(),
        })
    }
}

impl Datum for Spawner {
    fn encode(self) -> FOValue {
        match self {
            Self::Root => tag("root", None),
            Self::Agent(name) => tag("agent", Some(name.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("root", None)) => Ok(Self::Root),
            Some(("agent", Some(name))) => String::decode(name).map(Self::Agent),
            _ => Err(format!(
                "expected `root or `agent <name>, got {}",
                v.shape()
            )),
        }
    }
}

impl Datum for RosterState {
    fn encode(self) -> FOValue {
        let label = match self {
            Self::Busy => "busy",
            Self::WaitingOnAgents => "waiting-on-agents",
            Self::Replied => "replied",
            Self::Waiting => "waiting",
        };
        tag(label, None)
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("busy", None)) => Ok(Self::Busy),
            Some(("waiting-on-agents", None)) => Ok(Self::WaitingOnAgents),
            Some(("replied", None)) => Ok(Self::Replied),
            Some(("waiting", None)) => Ok(Self::Waiting),
            _ => Err(format!(
                "expected `busy, `waiting-on-agents, `replied or `waiting, got {}",
                v.shape()
            )),
        }
    }
}

// ── `exarch-schedules` ───────────────────────────────────────────────────

pub(crate) enum Schedules {
    List,
    Add(Add),
    Remove(String),
}

impl Family for Schedules {
    const CLASS: &'static str = "schedules";
    const TAGS: &'static [&'static str] = &["list", "add", "remove"];

    fn request(self) -> Request {
        Request::Schedules(self)
    }

    fn encode(self) -> FOValue {
        match self {
            Self::List => tag("list", None),
            Self::Add(add) => tag("add", Some(add.encode())),
            Self::Remove(label) => tag("remove", Some(label.encode())),
        }
    }

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, String>> {
        Some(match label {
            "list" => bare(payload).map(|()| Self::List),
            "add" => datum(payload).map(Self::Add),
            "remove" => datum(payload).map(Self::Remove),
            _ => return None,
        })
    }
}

pub(crate) struct Add {
    pub(crate) trigger: Trigger,
    pub(crate) label: String,
    pub(crate) prompt: String,
}

record!(Add {
    trigger: "trigger",
    label: "label",
    prompt: "prompt",
});

/// Parsed on arrival at either end, so a malformed expression carries its
/// parser's own message home.
impl Datum for Trigger {
    fn encode(self) -> FOValue {
        match self {
            Self::Cron { expr, .. } => tag("cron", Some(expr.encode())),
            Self::After(delay) => tag("after", Some(fmt_duration(delay).encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("cron", Some(expr))) => {
                let expr = String::decode(expr)?;
                let schedule = CronSchedule::parse(&expr)?;
                Ok(Self::Cron { schedule, expr })
            }
            Some(("after", Some(delay))) => {
                parse_duration(&String::decode(delay)?).map(Self::After)
            }
            _ => Err(format!(
                "expected `cron '<5-field-cron-expr>' or `after '<n><unit>', got {}",
                v.shape()
            )),
        }
    }
}

/// A cron with nothing inside its search horizon says "never" as the
/// saturated ceiling.
impl Datum for ScheduleInfo {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("label".into(), self.label.encode()),
                ("trigger".into(), self.trigger.encode()),
                (
                    "next-s".into(),
                    self.next_in.map_or(u64::MAX, |d| d.as_secs()).encode(),
                ),
                ("fires".into(), self.fires.encode()),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["label", "trigger", "next-s", "fires"])?;
        Ok(Self {
            label: field(v, "label")?,
            trigger: field(v, "trigger")?,
            next_in: Some(Duration::from_secs(field(v, "next-s")?)),
            fires: field(v, "fires")?,
        })
    }
}

// ── `exarch-pins` ────────────────────────────────────────────────────────

pub(crate) enum Pins {
    Set(Pin),
    Clear(String),
    Read(String),
    List,
}

impl Family for Pins {
    const CLASS: &'static str = "pins";
    const TAGS: &'static [&'static str] = &["set", "clear", "read", "list"];

    fn request(self) -> Request {
        Request::Pins(self)
    }

    fn encode(self) -> FOValue {
        match self {
            Self::Set(pin) => tag("set", Some(pin.encode())),
            Self::Clear(key) => tag("clear", Some(key.encode())),
            Self::Read(key) => tag("read", Some(key.encode())),
            Self::List => tag("list", None),
        }
    }

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, String>> {
        Some(match label {
            "set" => datum(payload).map(Self::Set),
            "clear" => datum(payload).map(Self::Clear),
            "read" => datum(payload).map(Self::Read),
            "list" => bare(payload).map(|()| Self::List),
            _ => return None,
        })
    }
}

pub(crate) struct Pin {
    pub(crate) key: String,
    pub(crate) body: Card,
}

record!(Pin {
    key: "key",
    body: "body",
});

impl Datum for Card {
    fn encode(self) -> FOValue {
        encode_card(&self)
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        value_to_card(v)
            .filter(|card| !card.marks().is_empty())
            .ok_or_else(|| {
                format!(
                    "expected a card with at least one mark, got {} — did you mean `exarch-pins \
                     `clear` to empty the slot?",
                    v.shape()
                )
            })
    }
}

// ── `exarch-context` ─────────────────────────────────────────────────────

pub(crate) enum Context {
    Survey,
    Evict(Evict),
}

impl Family for Context {
    const CLASS: &'static str = "context";
    const TAGS: &'static [&'static str] = &["survey", "evict"];

    fn request(self) -> Request {
        Request::Context(self)
    }

    fn encode(self) -> FOValue {
        match self {
            Self::Survey => tag("survey", None),
            Self::Evict(evict) => tag("evict", Some(evict.encode())),
        }
    }

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, String>> {
        Some(match label {
            "survey" => bare(payload).map(|()| Self::Survey),
            "evict" => datum(payload).map(Self::Evict),
            _ => return None,
        })
    }
}

/// Which turns leave, and the note the marker keeps in their place.
pub(crate) struct Evict {
    pub(crate) turns: Vec<u64>,
    pub(crate) note: Option<Note>,
}

impl Datum for Evict {
    fn encode(self) -> FOValue {
        let mut entries = vec![("turns".into(), self.turns.encode())];
        entries.extend(self.note.map(|note| ("note".into(), note.0.encode())));
        FOValue::Map { entries }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["turns", "note"])?;
        Ok(Self {
            turns: field(v, "turns")?,
            note: optional(v, "note")?,
        })
    }
}

/// A field a record may leave out.
fn optional<T: Datum>(v: &FOValue, key: &str) -> Result<Option<T>, String> {
    v.field(key).is_some().then(|| field(v, key)).transpose()
}

/// An eviction's note: one short line, since the marker keeps it for the
/// rest of the session and draws one row per turn the cut takes.
pub(crate) struct Note(pub(crate) String);

impl Note {
    const CAP: usize = 240;
}

impl Datum for Note {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let note = String::decode(v)?;
        if note.is_empty() {
            return Err("must not be empty — omit it to leave none".into());
        }
        if note.len() > Self::CAP {
            return Err(format!(
                "is {} bytes; the marker keeps one short line — {} at most. What is the one \
                 thing your future self needs to know?",
                note.len(),
                Self::CAP
            ));
        }
        if note.contains(['\n', '\r']) {
            return Err(
                "must be a single line — the marker draws one row per turn the cut \
                        takes, and a line break in a note reads as one of them."
                    .into(),
            );
        }
        Ok(Self(note))
    }
}

/// The context as `` `survey `` rows it: one line per resident turn, then
/// what the whole of it weighs.
pub(crate) struct Survey {
    pub(crate) rows: Vec<Line>,
    pub(crate) total_bytes: usize,
}

record!(Survey {
    rows: "rows",
    total_bytes: "total-bytes",
});

pub(crate) struct Line {
    pub(crate) id: u64,
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) bytes: usize,
}

record!(Line {
    id: "id",
    role: "role",
    kind: "kind",
    label: "label",
    bytes: "bytes",
});

impl From<ContextSurvey> for Survey {
    fn from(survey: ContextSurvey) -> Self {
        Self {
            rows: survey.rows.iter().map(Line::from).collect(),
            total_bytes: survey.total_bytes,
        }
    }
}

impl From<&crate::record::TurnRow> for Line {
    fn from(turn: &crate::record::TurnRow) -> Self {
        Self {
            id: turn.id,
            role: turn.role.as_str().into(),
            kind: turn.kind.as_str().into(),
            label: turn.label.clone(),
            bytes: turn.bytes,
        }
    }
}

// ── `exarch-transcript` ──────────────────────────────────────────────────

pub(crate) enum Transcript {
    Index,
    Read(Reading),
    Grep(Grep),
}

impl Family for Transcript {
    const CLASS: &'static str = "transcript";
    const TAGS: &'static [&'static str] = &["index", "read", "grep"];

    fn request(self) -> Request {
        Request::Transcript(self)
    }

    fn encode(self) -> FOValue {
        match self {
            Self::Index => tag("index", None),
            Self::Read(read) => tag("read", Some(read.encode())),
            Self::Grep(grep) => tag("grep", Some(grep.encode())),
        }
    }

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, String>> {
        Some(match label {
            "index" => bare(payload).map(|()| Self::Index),
            "read" => datum(payload).map(Self::Read),
            "grep" => datum(payload).map(Self::Grep),
            _ => return None,
        })
    }
}

pub(crate) struct Reading {
    pub(crate) turns: Vec<u64>,
}

record!(Reading { turns: "turns" });

/// A Rust regex over the transcript's text, narrowed to `turns` if given.
pub(crate) struct Grep {
    pub(crate) pattern: Regex,
    pub(crate) turns: Option<Vec<u64>>,
}

impl Datum for Grep {
    fn encode(self) -> FOValue {
        let mut entries = vec![("pattern".into(), self.pattern.as_str().to_string().encode())];
        entries.extend(self.turns.map(|turns| ("turns".into(), turns.encode())));
        FOValue::Map { entries }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["pattern", "turns"])?;
        let pattern: String = field(v, "pattern")?;
        Ok(Self {
            pattern: Regex::new(&pattern).map_err(|e| format!("`pattern: {e}"))?,
            turns: optional(v, "turns")?,
        })
    }
}

/// One row of `` `index ``: a survey's line, and whether the turn is held.
pub(crate) struct Indexed {
    pub(crate) id: u64,
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) bytes: usize,
    pub(crate) held: String,
}

record!(Indexed {
    id: "id",
    role: "role",
    kind: "kind",
    label: "label",
    bytes: "bytes",
    held: "held",
});

impl From<crate::record::TurnRow> for Indexed {
    fn from(turn: crate::record::TurnRow) -> Self {
        let Line {
            id,
            role,
            kind,
            label,
            bytes,
        } = Line::from(&turn);
        Self {
            id,
            role,
            kind,
            label,
            bytes,
            held: turn.held.as_str().into(),
        }
    }
}

/// `` `grep ``'s hits, and how many there were in all.
pub(crate) struct Hits {
    pub(crate) hits: Vec<Hit>,
    pub(crate) total: usize,
}

record!(Hits {
    hits: "hits",
    total: "total",
});

pub(crate) struct Hit {
    pub(crate) turn: u64,
    pub(crate) role: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}

record!(Hit {
    turn: "turn",
    role: "role",
    line: "line",
    text: "text",
});

impl From<crate::agent::event::GrepAnswer> for Hits {
    fn from(answer: crate::agent::event::GrepAnswer) -> Self {
        Self {
            hits: answer
                .hits
                .into_iter()
                .map(|hit| Hit {
                    turn: hit.turn,
                    role: crate::agent::event::role_label(&hit.role).into(),
                    line: hit.line,
                    text: hit.text,
                })
                .collect(),
            total: answer.total,
        }
    }
}

/// One turn a `` `read `` named, its messages narrowed to variant parts.
pub(crate) struct Material {
    pub(crate) turn: u64,
    pub(crate) role: String,
    pub(crate) messages: Vec<Said>,
}

record!(Material {
    turn: "turn",
    role: "role",
    messages: "messages",
});

pub(crate) struct Said {
    pub(crate) role: Bare,
    pub(crate) parts: Vec<TranscriptPart>,
}

record!(Said {
    role: "role",
    parts: "parts",
});

impl From<crate::agent::event::TranscriptTurn> for Material {
    fn from(read: crate::agent::event::TranscriptTurn) -> Self {
        Self {
            turn: read.turn,
            role: read.role.as_str().into(),
            messages: read
                .messages
                .into_iter()
                .map(|message| Said {
                    role: Bare(crate::agent::event::role_label(&message.role).into()),
                    parts: message.parts,
                })
                .collect(),
        }
    }
}

/// A bare tag whose alphabet is the record's, not this vocabulary's.
pub(crate) struct Bare(pub(crate) String);

impl Datum for Bare {
    fn encode(self) -> FOValue {
        tag(&self.0, None)
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some((label, None)) => Ok(Self(label.into())),
            _ => Err(format!("expected a bare tag, got {}", v.shape())),
        }
    }
}

impl Datum for TranscriptPart {
    fn encode(self) -> FOValue {
        let (label, entries) = match self {
            Self::Text(content) => ("text", vec![("content", content.encode())]),
            Self::Program { tool, source, keys } => (
                "program",
                vec![
                    ("tool", tool.encode()),
                    ("source", source.encode()),
                    ("keys", keys.encode()),
                ],
            ),
            Self::Result(content) => ("result", vec![("content", content.encode())]),
            Self::Reasoning(content) => ("reasoning", vec![("content", content.encode())]),
            Self::Binary {
                content_type,
                name,
                bytes,
            } => (
                "binary",
                vec![
                    ("content-type", content_type.encode()),
                    ("name", name.encode()),
                    ("bytes", bytes.encode()),
                ],
            ),
            Self::Custom { provider, model } => (
                "custom",
                vec![("provider", provider.encode()), ("model", model.encode())],
            ),
        };
        let entries = entries.into_iter().map(|(k, v)| (k.into(), v)).collect();
        tag(label, Some(FOValue::Map { entries }))
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        fn record<'a>(r: &'a FOValue, keys: &[&str]) -> Result<&'a FOValue, String> {
            exact_keys(r, keys).map(|()| r)
        }
        match untag(v) {
            Some(("text", Some(r))) => Ok(Self::Text(field(record(r, &["content"])?, "content")?)),
            Some(("program", Some(r))) => {
                let r = record(r, &["tool", "source", "keys"])?;
                Ok(Self::Program {
                    tool: field(r, "tool")?,
                    source: field(r, "source")?,
                    keys: field(r, "keys")?,
                })
            }
            Some(("result", Some(r))) => {
                Ok(Self::Result(field(record(r, &["content"])?, "content")?))
            }
            Some(("reasoning", Some(r))) => {
                Ok(Self::Reasoning(field(record(r, &["content"])?, "content")?))
            }
            Some(("binary", Some(r))) => {
                let r = record(r, &["content-type", "name", "bytes"])?;
                Ok(Self::Binary {
                    content_type: field(r, "content-type")?,
                    name: field(r, "name")?,
                    bytes: field(r, "bytes")?,
                })
            }
            Some(("custom", Some(r))) => {
                let r = record(r, &["provider", "model"])?;
                Ok(Self::Custom {
                    provider: field(r, "provider")?,
                    model: field(r, "model")?,
                })
            }
            _ => Err(format!(
                "expected `text, `program, `result, `reasoning, `binary or `custom, got {}",
                v.shape()
            )),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// `encode` then `decode` is the identity, read back through `encode`
    /// since a request holds types that compare no other way.
    fn round_trips(requests: impl IntoIterator<Item = Request>) {
        for request in requests {
            let wire = request.encode();
            let back = Request::decode(&wire).unwrap_or_else(|why| panic!("{why}: {wire:?}"));
            assert_eq!(back.encode(), wire);
        }
    }

    fn spec(grant: SpawnGrant, memory: Memory, model: Selection) -> Launch {
        Launch {
            prompt: "go".into(),
            name: Name("scout".into()),
            memory,
            grant: Grant(grant),
            search: true,
            provider: Selection::Inherit,
            model,
        }
    }

    fn restriction() -> FOValue {
        FOValue::Map {
            entries: vec![("net".into(), false.encode())],
        }
    }

    #[test]
    fn every_agents_tag_round_trips() {
        let start = |spec, fork| Request::Agents(Agents::Start(Start { spec, fork }));
        round_trips([
            Request::Agents(Agents::List),
            start(
                spec(SpawnGrant::Inherit, Memory::Amnemon, Selection::Inherit),
                ForkClaim::Parked(NurseryId(3)),
            ),
            start(
                spec(
                    SpawnGrant::Base("confined".into()),
                    Memory::Mnemon,
                    Selection::Named("other-model".into()),
                ),
                // A token past `i64::MAX` crosses bit for bit.
                ForkClaim::Listening {
                    port: 41_731,
                    token: u64::MAX,
                },
            ),
            start(
                spec(
                    SpawnGrant::Restrict(restriction()),
                    Memory::Amnemon,
                    Selection::Inherit,
                ),
                ForkClaim::Parked(NurseryId(0)),
            ),
            Request::Agents(Agents::Message(Message {
                to: "parent".into(),
                text: "hi".into(),
            })),
            Request::Agents(Agents::Cancel("scout".into())),
            Request::Agents(Agents::Reply(restriction())),
            Request::Agents(Agents::Read("scout".into())),
            Request::Agents(Agents::Branch(ForkClaim::Parked(NurseryId(1)))),
        ]);
    }

    #[test]
    fn every_schedules_tag_round_trips() {
        let add = |trigger| {
            Request::Schedules(Schedules::Add(Add {
                trigger,
                label: "nightly".into(),
                prompt: "wake".into(),
            }))
        };
        let expr = "0 9 * * 1-5";
        round_trips([
            Request::Schedules(Schedules::List),
            add(Trigger::After(parse_duration("2h").unwrap())),
            add(Trigger::Cron {
                schedule: CronSchedule::parse(expr).unwrap(),
                expr: expr.into(),
            }),
            Request::Schedules(Schedules::Remove("nightly".into())),
        ]);
    }

    #[test]
    fn every_pins_tag_round_trips() {
        let body = tag(
            "text",
            Some(FOValue::Map {
                entries: vec![(
                    "spans".into(),
                    FOValue::List {
                        items: vec![FOValue::Map {
                            entries: vec![("text".into(), "hi".to_string().encode())],
                        }],
                    },
                )],
            }),
        );
        round_trips([
            Request::Pins(Pins::Set(Pin {
                key: "tasks".into(),
                body: Card::decode(&body).unwrap(),
            })),
            Request::Pins(Pins::Clear("tasks".into())),
            Request::Pins(Pins::Read("tasks".into())),
            Request::Pins(Pins::List),
        ]);
    }

    #[test]
    fn every_context_tag_round_trips() {
        round_trips([
            Request::Context(Context::Survey),
            Request::Context(Context::Evict(Evict {
                turns: vec![3, 1, 2],
                note: None,
            })),
            Request::Context(Context::Evict(Evict {
                turns: vec![1],
                note: Some(Note("the parser is fixed".into())),
            })),
        ]);
    }

    #[test]
    fn every_transcript_tag_round_trips() {
        round_trips([
            Request::Transcript(Transcript::Index),
            Request::Transcript(Transcript::Read(Reading { turns: vec![1, 2] })),
            Request::Transcript(Transcript::Grep(Grep {
                pattern: Regex::new("first (prompt|answer)").unwrap(),
                turns: None,
            })),
            Request::Transcript(Transcript::Grep(Grep {
                pattern: Regex::new("first|second").unwrap(),
                turns: Some(vec![2]),
            })),
        ]);
    }

    /// A model's `start` names its spec alone: the fork is its door's.
    #[test]
    fn a_models_start_word_is_the_spec_awaiting_its_fork() {
        let spec = spec(SpawnGrant::Inherit, Memory::Amnemon, Selection::Inherit).encode();
        assert!(matches!(
            Word::decode(&tag("start", Some(spec))),
            Ok(Word::Spawn(_))
        ));
        assert!(matches!(
            Word::decode(&tag("list", None)),
            Ok(Word::Ask(Agents::List))
        ));
    }

    /// Every bare tag a spawn may grant: the bases, plus `` `inherit ``, which
    /// names none and so is the one tag policy has nothing to resolve.
    fn bare_grant_tags() -> impl Iterator<Item = &'static str> {
        std::iter::once("inherit").chain(crate::policy::SPAWN_BASES)
    }

    #[test]
    fn a_grant_admits_every_bare_tag() {
        for label in bare_grant_tags() {
            let grant = Grant::decode(&tag(label, None))
                .unwrap_or_else(|e| panic!("must admit `{label}: {e}"));
            let read_back = match (&grant.0, label) {
                (SpawnGrant::Inherit, "inherit") => true,
                (SpawnGrant::Base(base), _) => base == label,
                _ => false,
            };
            assert!(read_back, "`{label} must read back as its own grant");
        }
    }

    /// The record is carried, not decoded: the capability vocabulary is the
    /// engine's.
    #[test]
    fn a_grant_carries_a_restrict_record_verbatim() {
        let grant = Grant::decode(&tag("restrict", Some(restriction())))
            .unwrap_or_else(|e| panic!("must admit `restrict: {e}"));
        let SpawnGrant::Restrict(record) = grant.0 else {
            panic!("`restrict must carry its record through as first-order data");
        };
        assert_eq!(record, restriction(), "the record must cross unchanged");
    }

    #[test]
    fn a_grant_refuses_an_unknown_tag_naming_every_legal_shape() {
        let Err(why) = Grant::decode(&tag("bogus", None)) else {
            panic!("`bogus is no grant");
        };
        for label in bare_grant_tags().chain(["restrict"]) {
            assert!(why.contains(label), "must name `{label}`, got: {why}");
        }
    }

    /// A base carrying a payload is refused, never truncated to its label:
    /// `` `restrict `` is the only tag that takes one.
    #[test]
    fn a_grant_refuses_a_base_carrying_a_payload() {
        assert!(Grant::decode(&tag("confined", Some(FOValue::Int { value: 1 }))).is_err());
    }

    /// Every base a spawn may grant must resolve to a bake-in profile — which
    /// also parses and evaluates that profile's `data/*.exarch.ral` — so a
    /// label added here alone shows up. The spawn's table is the narrower of
    /// the two: the policy layer offers a launching human bases a child is
    /// not handed.
    #[test]
    fn every_grant_base_resolves_to_a_bake_in_base() {
        let cwd = std::env::current_dir().unwrap();
        for label in crate::policy::SPAWN_BASES {
            crate::policy::base_layer(label, &cwd)
                .unwrap_or_else(|e| panic!("grant `{label} must name a bake-in base: {e}"));
        }
        let err =
            crate::policy::base_layer("bogus", &cwd).expect_err("an unknown base must be refused");
        let offered: std::collections::BTreeSet<&str> = err
            .rsplit_once("expected one of: ")
            .unwrap_or_else(|| panic!("the refusal must enumerate the bases, got: {err}"))
            .1
            .split(", ")
            .collect();
        assert!(
            crate::policy::SPAWN_BASES
                .iter()
                .all(|l| offered.contains(l)),
            "every grant base must be one the policy layer offers, got: {offered:?}"
        );
    }

    /// The address is the edit, so a spec carrying only a note never reaches
    /// the host. The scheme anchors `turns`, so no program written in ral
    /// gets this far — the decoder is the second line of defence.
    #[test]
    fn an_eviction_naming_no_turns_is_refused() {
        let note = FOValue::Map {
            entries: vec![("note".into(), "nothing to say".to_string().encode())],
        };
        let Err(why) = family::<Context>(&tag("evict", Some(note))) else {
            panic!("an eviction must name the turns it takes");
        };
        assert_eq!(
            why,
            "`exarch-context `evict`: no `turns field in a record of 1 field"
        );
    }

    /// A tag that reads nothing refuses a payload rather than dropping it.
    #[test]
    fn a_payload_on_a_bare_tag_is_refused() {
        let Err(why) = family::<Pins>(&tag("list", Some(FOValue::Unit))) else {
            panic!("`list takes nothing");
        };
        assert_eq!(why, "`exarch-pins `list`: takes no payload, got ()");
    }
}
