//! The one vocabulary exarch's enquiries speak: every class, tag and field a
//! harness builtin's door sends and [`super::agent::desk`] answers, and the answers
//! owed back.
//!
//! The door builds a [`Request`] and the desk decodes one, so an
//! ill-shaped request is refused in the same words at either end.

use crate::card::{Card, encode_card, value_to_card};
use crate::record::model::ContextSurvey;
use crate::record::{Location, Memory, Span, SpanRow, TranscriptPart};
use crate::schedule::{CronSchedule, ScheduleInfo, Trigger, fmt_duration, parse_duration};
use ral_core::SpawnGrant;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::{Datum, exact_keys, field, tag, untag};
use ral_core::text::code;
use ral_core::ty::{Ty, Typed, closed_record};
use ral_core::types::NurseryId;
use ral_core::{record, variant};
use regex::Regex;
use std::path::PathBuf;
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
            "{} takes a tag naming what to do ({}); got {}",
            code(&format!("exarch-{class}")),
            offered(),
            word.shape()
        )
    })?;
    decode(label, payload)
        .ok_or_else(|| {
            format!(
                "unrecognised tag in {}: {} takes {}",
                code(&format!("exarch-{class} `{label}")),
                code(&format!("exarch-{class}")),
                offered()
            )
        })?
        .map_err(|why| format!("{}: {why}", code(&format!("exarch-{class} `{label}"))))
}

fn offer(tags: &[&str]) -> String {
    let tags: Vec<String> = tags.iter().map(|t| code(&format!("`{t}"))).collect();
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
                format!(
                    "{} takes a tag naming what to do, got no payload at all",
                    code(&format!("exarch-{label}"))
                )
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
            Self::Pins(Pins::Set(_) | Pins::Clear(_)) => unit,
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

/// `` `some `` a slot's card, or `` `none `` for an empty one.
fn pinned(v: &FOValue) -> Result<(), String> {
    match v {
        FOValue::Variant {
            label,
            payload: None,
        } if label == "none" => Ok(()),
        FOValue::Variant {
            label,
            payload: Some(card),
        } if label == "some" => is::<Card>(card),
        other => Err(format!(
            "expected `none or `some card, got {}",
            other.shape()
        )),
    }
}

// ── `exarch-agents` ──────────────────────────────────────────────────────

pub(crate) enum Agents {
    List,
    Start(Start),
    Message(Message),
    Cancel(String),
    Reply(FOValue),
    Read(String),
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
    pub(crate) spec: Spec,
    pub(crate) fork: ForkClaim,
}

record!(Start {
    spec: "spec",
    fork: "fork",
});

/// The model's own spawn record: a flat record read as its grant and the rest.
pub(crate) struct Launch {
    pub(crate) grant: Grant,
    pub(crate) spec: Spec,
}

impl Datum for Launch {
    fn encode(self) -> FOValue {
        let FOValue::Map { mut entries } = self.spec.encode() else {
            unreachable!("a spec encodes to a record")
        };
        entries.push(("grant".into(), self.grant.encode()));
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        FOValue::Map { entries }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let FOValue::Map { entries } = v else {
            return Err(format!("expected a record, got {}", v.shape()));
        };
        let rest = entries.iter().filter(|(k, _)| k != "grant").cloned();
        Ok(Self {
            grant: field(v, "grant")?,
            spec: Spec::decode(&FOValue::Map {
                entries: rest.collect(),
            })?,
        })
    }
}

/// A [`Launch`] as the desk sees it: the grant rides in the sealed bequest.
pub(crate) struct Spec {
    pub(crate) prompt: String,
    pub(crate) name: Name,
    pub(crate) memory: Memory,
    pub(crate) model: Selection,
}

record!(Spec {
    prompt: "prompt",
    name: "name",
    memory: "type",
    model: "model",
});

/// The tab-bar contract every agent name must meet.
///
/// Non-empty, at most 24 characters, ASCII alphanumeric plus `-`/`_`, since the
/// tab bar lays a name out as a single token.  Stated here because this is where
/// a name becomes identity — `Fleet::claim` refuses whatever this refuses,
/// so no door can mint an agent around it, however a name reached that door.
///
/// # Errors
/// The sentence to show, naming the rule and the name that broke it.
pub fn check_name(name: &str) -> Result<(), String> {
    if !name.is_empty()
        && name.len() <= 24
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Ok(());
    }
    Err(format!(
        "`name` must be non-empty, at most 24 characters, and only ASCII letters, digits, `-`, \
         or `_` (the tab-bar contract); got {name:?}"
    ))
}

/// An agent's name, admitted by [`check_name`]; `Fleet::claim`
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
                "`restrict must carry a capability record [exec, fs, net, editor, shell], got {}",
                other.shape()
            )),
            _ => Err(format!(
                "expected `inherit, {}, or `restrict [exec: …, fs: …, net: …, editor: …, \
                 shell: …]: `inherit is how you decline to narrow at all, and `restrict must carry \
                 a capability record of the same shape `grant [...]` takes, every key optional: got {}",
                crate::policy::SPAWN_BASES
                    .map(|b| format!("`{b}"))
                    .join(", "),
                v.shape()
            )),
        }
    }
}

/// A child's model: the spawner's own, or a role the human bound at launch.
/// ral has no optional field, so the absence of a choice is data.
pub(crate) enum Selection {
    Inherit,
    Role(String),
}

impl Datum for Selection {
    fn encode(self) -> FOValue {
        match self {
            Self::Inherit => tag("inherit", None),
            Self::Role(name) => tag("role", Some(name.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("inherit", None)) => Ok(Self::Inherit),
            Some(("role", Some(FOValue::String { value }))) if !value.is_empty() => {
                Ok(Self::Role(value.clone()))
            }
            Some(("role", payload)) => Err(format!(
                "`role must carry a non-empty Str, got {}",
                payload.map_or_else(|| "nothing".to_string(), FOValue::shape)
            )),
            _ => Err(format!(
                "expected `inherit (your own model) or `role '<name>' (one the human bound at launch), got {}",
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
    /// Sealed by a wire engine: the bytes the host builds a child engine
    /// around without reading them.
    Sealed(ral_core::seed::Sealed),
}

impl Datum for ForkClaim {
    fn encode(self) -> FOValue {
        match self {
            Self::Parked(id) => tag("parked", Some(id.0.encode())),
            Self::Sealed(seal) => tag("sealed", Some(seal.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("parked", Some(id))) => u64::decode(id).map(|id| Self::Parked(NurseryId(id))),
            Some(("sealed", Some(bytes))) => {
                ral_core::seed::Sealed::decode(bytes).map(Self::Sealed)
            }
            _ => Err(format!(
                "expected `parked <nursery id> or `sealed <bytes>, got {}",
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

/// What every tag but `` `list `` and `` `read `` answers: the world after the
/// transition, at O(1) to read rather than O(fleet) to carry.
pub struct Summary {
    /// Other live agents in `reader`'s tree — company, not bookkeeping.  Zero
    /// is the honest "you are alone here".
    pub live: usize,
    /// Direct children holding a reply; reading does not consume it.
    pub replied: usize,
}

record!(Summary {
    live: "live",
    replied: "replied",
});

/// One live agent, for the `exarch-agents` listing.
pub struct AgentInfo {
    pub name: String,
    pub spawner: Spawner,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
    pub state: RosterState,
    /// How long it has been parked; zero while busy.
    pub idle: Duration,
}

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
                    ral_core::path::shown(&self.log_dir).encode(),
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

/// Who started a listed agent — the edge that keeps a flat fleet listing a
/// tree.  A root was started by a human, and there is no name to give.
pub enum Spawner {
    Root,
    Agent(String),
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

/// What a listed agent is doing, as a roster row states it.  Derived at
/// `listing` time from an agent's rest and deposit and from its own children;
/// nothing stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RosterState {
    /// Working: not parked for a message.
    Busy,
    /// Working only in the sense of holding for a busy child of its own.
    WaitingOnAgents,
    /// Parked holding a reply its spawner has yet to fetch.
    Replied,
    /// Parked with no reply — a child a human is talking to.
    Waiting,
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
            Self::At(at) => tag("at", Some(at.to_string().encode())),
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

record!(typed ScheduleInfo {
    label: "label",
    trigger: "trigger",
    next_s: "next-s",
    fires: "fires",
});

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
                    "expected a card with at least one mark, got {}: did you mean `` exarch-pins \
                     `clear `` to empty the slot?",
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

/// The run `[from, to]` that leaves, both ends inclusive, and the note its
/// residue keeps in its place. An inverted run is refused here, once.
pub(crate) struct Evict {
    pub(crate) span: Span,
    pub(crate) note: Note,
}

impl Typed for Evict {
    fn ty() -> Ty {
        closed_record(&[("from", u64::ty()), ("to", u64::ty()), ("note", Note::ty())])
    }
}

impl Datum for Evict {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("from".into(), self.span.from().encode()),
                ("note".into(), self.note.encode()),
                ("to".into(), self.span.to().encode()),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["from", "to", "note"])?;
        Ok(Self {
            span: Span::new(field(v, "from")?, field(v, "to")?).map_err(|e| e.to_string())?,
            note: field(v, "note")?,
        })
    }
}

/// An eviction's note: one short line, since the residue keeps it in place
/// of the run for the rest of the session.
pub(crate) struct Note(pub(crate) String);

impl Note {
    const CAP: usize = 240;
}

impl Typed for Note {
    fn ty() -> Ty {
        Ty::String
    }
}

impl Datum for Note {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let note = String::decode(v)?;
        if note.is_empty() {
            return Err(
                "text must not be empty: the residue keeps one short line, and it is the only \
                 trace of the run left in your context"
                    .into(),
            );
        }
        if note.len() > Self::CAP {
            return Err(format!(
                "text is {} bytes; the residue keeps one short line: {} at most. What is the one \
                 thing your future self needs to know?",
                note.len(),
                Self::CAP
            ));
        }
        if note.contains(['\n', '\r']) {
            return Err("text must be a single line: the residue keeps one short line".into());
        }
        Ok(Self(note))
    }
}

/// The context as `` `survey `` rows it: one row per item in it, then what
/// the whole of it weighs.
pub(crate) struct Survey {
    pub(crate) rows: Vec<Row>,
    pub(crate) total_bytes: usize,
}

record!(typed Survey {
    rows: "rows",
    total_bytes: "total-bytes",
});

/// One item, turn or residue, by its span.
pub(crate) struct Row {
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) bytes: usize,
}

record!(typed Row {
    from: "from",
    to: "to",
    role: "role",
    kind: "kind",
    label: "label",
    bytes: "bytes",
});

impl From<ContextSurvey> for Survey {
    fn from(survey: ContextSurvey) -> Self {
        Self {
            rows: survey.rows.iter().map(Row::from).collect(),
            total_bytes: survey.total_bytes,
        }
    }
}

impl From<&SpanRow> for Row {
    fn from(row: &SpanRow) -> Self {
        Self {
            from: row.span.from(),
            to: row.span.to(),
            role: row.role.as_str().into(),
            kind: row.kind.as_str().into(),
            label: row.label.clone(),
            bytes: row.bytes,
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

record!(typed Reading { turns: "turns" });

/// Which turns a `` `grep `` covers.
pub(crate) enum Turns {
    All,
    Only(Vec<u64>),
}

variant!(typed Turns {
    All: "all",
    Only(Vec<u64>): "only",
});

impl Turns {
    pub(crate) fn only(&self) -> Option<&[u64]> {
        match self {
            Self::All => None,
            Self::Only(turns) => Some(turns),
        }
    }
}

/// A Rust regex over the transcript's text, narrowed to `` `only turns ``.
pub(crate) struct Grep {
    pub(crate) pattern: Regex,
    pub(crate) turns: Turns,
}

impl Datum for Grep {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("pattern".into(), self.pattern.as_str().to_string().encode()),
                ("turns".into(), self.turns.encode()),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["pattern", "turns"])?;
        let pattern: String = field(v, "pattern")?;
        Ok(Self {
            pattern: Regex::new(&pattern).map_err(|e| format!("`pattern: {e}"))?,
            turns: field(v, "turns")?,
        })
    }
}

/// One row of `` `index ``: a survey's row, and where the item is.
pub(crate) struct Indexed {
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) bytes: usize,
    pub(crate) location: Location,
}

record!(Indexed {
    from: "from",
    to: "to",
    role: "role",
    kind: "kind",
    label: "label",
    bytes: "bytes",
    location: "location",
});

impl From<SpanRow> for Indexed {
    fn from(row: SpanRow) -> Self {
        let Row {
            from,
            to,
            role,
            kind,
            label,
            bytes,
        } = Row::from(&row);
        Self {
            from,
            to,
            role,
            kind,
            label,
            bytes,
            location: row.location,
        }
    }
}

/// `[from: Int, to: Int]`, as every span reads at the surface.
impl Datum for Span {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("from".into(), self.from().encode()),
                ("to".into(), self.to().encode()),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        exact_keys(v, &["from", "to"])?;
        Self::new(field(v, "from")?, field(v, "to")?).map_err(|e| e.to_string())
    }
}

/// `` `resident ``, `` `under [from: Int, to: Int] `` naming the smallest
/// later residue over the item, or `` `withdrawn ``.
impl Datum for Location {
    fn encode(self) -> FOValue {
        match self {
            Self::Under(span) => tag("under", Some(span.encode())),
            location => tag(location.as_str(), None),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("resident", None)) => Ok(Self::Resident),
            Some(("withdrawn", None)) => Ok(Self::Withdrawn),
            Some(("under", Some(span))) => Span::decode(span).map(Self::Under),
            _ => Err(format!(
                "expected `resident, `under [from: Int, to: Int] or `withdrawn, got {}",
                v.shape()
            )),
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

/// One matching line, naming the item it lies in by `(from, to, kind)`.
pub(crate) struct Hit {
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) kind: String,
    pub(crate) role: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}

record!(Hit {
    from: "from",
    to: "to",
    kind: "kind",
    role: "role",
    line: "line",
    text: "text",
});

impl From<crate::record::GrepAnswer> for Hits {
    fn from(answer: crate::record::GrepAnswer) -> Self {
        Self {
            hits: answer
                .hits
                .into_iter()
                .map(|hit| Hit {
                    from: hit.span.from(),
                    to: hit.span.to(),
                    kind: hit.kind.as_str().into(),
                    role: crate::record::role_label(&hit.role).into(),
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

impl From<crate::record::TranscriptTurn> for Material {
    fn from(read: crate::record::TranscriptTurn) -> Self {
        Self {
            turn: read.turn,
            role: read.role.as_str().into(),
            messages: read
                .messages
                .into_iter()
                .map(|message| Said {
                    role: Bare(crate::record::role_label(&message.role).into()),
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

    fn launch(memory: Memory, model: Selection) -> Launch {
        Launch {
            grant: Grant(SpawnGrant::Inherit),
            spec: Spec {
                prompt: "go".into(),
                name: Name("scout".into()),
                memory,
                model,
            },
        }
    }

    fn spec(memory: Memory, model: Selection) -> Spec {
        launch(memory, model).spec
    }

    fn restriction() -> FOValue {
        FOValue::Map {
            entries: vec![("net".into(), tag("offline", None))],
        }
    }

    #[test]
    fn every_agents_tag_round_trips() {
        let start = |spec, fork| Request::Agents(Agents::Start(Start { spec, fork }));
        round_trips([
            Request::Agents(Agents::List),
            start(
                spec(Memory::Amnemon, Selection::Inherit),
                ForkClaim::Parked(NurseryId(3)),
            ),
            start(
                spec(Memory::Mnemon, Selection::Role("other-model".into())),
                ForkClaim::Sealed(
                    ral_core::seed::seal(
                        &ral_core::HostSurface::default()
                            .shell(
                                ral_core::terminal::TerminalState::default(),
                                ral_core::test_helper::process_cwd(),
                            )
                            .expect("a test host resolves its floor")
                            .bequeath(SpawnGrant::Inherit),
                    )
                    .expect("a bequest seals"),
                ),
            ),
            Request::Agents(Agents::Message(Message {
                to: "parent".into(),
                text: "hi".into(),
            })),
            Request::Agents(Agents::Cancel("scout".into())),
            Request::Agents(Agents::Reply(restriction())),
            Request::Agents(Agents::Read("scout".into())),
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
                span: Span::new(1, 3).unwrap(),
                note: Note("the parser is fixed".into()),
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
                turns: Turns::All,
            })),
            Request::Transcript(Transcript::Grep(Grep {
                pattern: Regex::new("first|second").unwrap(),
                turns: Turns::Only(vec![2]),
            })),
        ]);
    }

    /// A model's `start` names its spec alone: the fork is its door's.
    #[test]
    fn a_models_start_word_is_the_spec_awaiting_its_fork() {
        let launch = launch(Memory::Amnemon, Selection::Inherit).encode();
        assert!(matches!(
            Word::decode(&tag("start", Some(launch))),
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
    fn a_grant_refuses_a_restriction_that_is_not_a_record() {
        let Err(why) = Grant::decode(&tag(
            "restrict",
            Some(FOValue::String {
                value: "everything".into(),
            }),
        )) else {
            panic!("a `restrict carrying no record is no grant");
        };
        assert_eq!(
            why,
            "`restrict must carry a capability record [exec, fs, net, editor, shell], got a Str"
        );
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

    fn evict_with(entries: &[(&str, FOValue)]) -> Result<Context, String> {
        let entries = entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect();
        family::<Context>(&tag("evict", Some(FOValue::Map { entries })))
    }

    /// A residue stands on its note, so an eviction without one never reaches
    /// the host; nor does the old `turns` address. The scheme closes the
    /// record, so the decoder is the second line of defence.
    #[test]
    fn an_eviction_without_a_note_or_by_turns_is_refused() {
        let int = |value| FOValue::Int { value };
        let Err(why) = evict_with(&[("from", int(1)), ("to", int(2))]) else {
            panic!("an eviction must carry its note");
        };
        assert_eq!(
            why,
            "`` exarch-context `evict ``: no `note field in a record of 2 fields"
        );
        let turns = FOValue::List {
            items: vec![int(1), int(2)],
        };
        let note = "the parser is fixed".to_string().encode();
        assert!(
            evict_with(&[("turns", turns), ("note", note)]).is_err(),
            "a run is named by from and to, not by a list of turns"
        );
    }

    /// `location` is a variant, and `under` names the holder's span.
    #[test]
    fn an_index_row_encodes_its_span_and_holder() {
        let row = Indexed::from(SpanRow {
            span: Span::single(3),
            role: crate::record::Role::User,
            kind: crate::record::ItemKind::Turn(crate::record::TurnKind::Own),
            label: "first".into(),
            bytes: 7,
            location: Location::Under(Span::new(3, 4).unwrap()),
        })
        .encode();
        let int = |value| FOValue::Int { value };
        assert_eq!(row.field("from"), Some(&int(3)));
        assert_eq!(row.field("to"), Some(&int(3)));
        assert_eq!(
            row.field("location"),
            Some(&tag(
                "under",
                Some(FOValue::Map {
                    entries: vec![("from".into(), int(3)), ("to".into(), int(4))]
                })
            ))
        );
        assert!(Vec::<Indexed>::decode(&FOValue::List { items: vec![row] }).is_ok());
    }

    /// A hit names its item, a residue's note by its whole span.
    #[test]
    fn a_hit_encodes_its_item() {
        let hits = Hits::from(crate::record::GrepAnswer {
            hits: vec![crate::record::GrepHit {
                span: Span::new(3, 4).unwrap(),
                kind: crate::record::ItemKind::Residue,
                role: genai::chat::ChatRole::User,
                line: 1,
                text: "the parser is fixed".into(),
            }],
            total: 1,
        })
        .encode();
        let hit = &hits.field("hits").and_then(FOValue::as_list).expect("hits")[0];
        assert_eq!(hit.field("from"), Some(&FOValue::Int { value: 3 }));
        assert_eq!(hit.field("to"), Some(&FOValue::Int { value: 4 }));
        assert_eq!(hit.field("kind"), Some(&"residue".to_string().encode()));
    }

    /// A tag that reads nothing refuses a payload rather than dropping it.
    #[test]
    fn a_payload_on_a_bare_tag_is_refused() {
        let Err(why) = family::<Pins>(&tag("list", Some(FOValue::Unit))) else {
            panic!("`list takes nothing");
        };
        assert_eq!(why, "`` exarch-pins `list ``: takes no payload, got ()");
    }
}
