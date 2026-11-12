//! The one vocabulary exarch's enquiries speak: every class, tag and field a
//! harness builtin sends and [`super::agent::desk`] answers, and the answers
//! owed back.
//!
//! The builtin builds a [`Request`] and the desk decodes one, so an
//! ill-shaped request is refused in the same words at either end.

use crate::card::{Card, encode_card, value_to_card};
use crate::record::context::ContextSurvey;
use crate::record::{Location, Memory, Span, SpanRow, TranscriptPart};
use crate::schedule::{Asked, CronSchedule, ScheduleInfo, fmt_duration, parse_duration};
use ral_core::SpawnGrant;
use ral_core::data::FOValue;
use ral_core::data::datum::{Datum, exact_keys, field, tag, untag};
use ral_core::fact::{
    AgentNameFailure, CardFailure, DecodeCause, DecodeFailure, Expected, Found, NoteFailure,
    NoteFailureTooLong, RegexCause, RegexFailure, ShapeFailure, UnexpectedPayload, UnknownTag,
    ValueShape,
};
use ral_core::marshal::Marshalled;
use ral_core::ty::{Ty, Typed, closed_record};
use ral_core::{memoized_ty, record, variant};
use regex::Regex;
use std::path::PathBuf;
use std::time::Duration;

/// One enquiry, as it crosses from a harness builtin to the desk.
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
    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, DecodeFailure>>;
}

/// A model's argument to `exarch-<class>`, read as the request it makes.
///
/// # Errors
/// The typed refusal at its argument path.
pub(crate) fn family<F: Family>(word: &FOValue) -> Result<F, DecodeFailure> {
    read_tag::<F, F>(word, F::decode)
}

fn read_tag<F: Family, T>(
    word: &FOValue,
    decode: impl FnOnce(&str, Option<&FOValue>) -> Option<Result<T, DecodeFailure>>,
) -> Result<T, DecodeFailure> {
    let (label, payload) = tagged(word, F::TAGS)?;
    decode(label, payload).ok_or_else(|| unknown(label, F::TAGS))?
}

fn tagged<'a>(
    v: &'a FOValue,
    tags: &[&str],
) -> Result<(&'a str, Option<&'a FOValue>), DecodeFailure> {
    untag(v).ok_or_else(|| {
        DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
            expected: Expected::Tags(tags.iter().map(|tag| (*tag).into()).collect()),
            found: Found::of(v),
        }))
    })
}

fn unknown(tag: &str, tags: &[&str]) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
        tag: tag.into(),
        offered: tags.iter().map(|tag| (*tag).into()).collect(),
    }))
}

fn bare(tag: &str, payload: Option<&FOValue>) -> Result<(), DecodeFailure> {
    payload.map_or(Ok(()), |p| {
        Err(DecodeFailure::new(DecodeCause::UnexpectedPayload(
            UnexpectedPayload {
                tag: tag.into(),
                found: Found::of(p),
            },
        )))
    })
}

fn carried<'a>(tag: &str, payload: Option<&'a FOValue>) -> Result<&'a FOValue, DecodeFailure> {
    payload.ok_or_else(|| DecodeFailure::new(DecodeCause::MissingPayload(tag.into())))
}

fn datum<T: Datum>(tag: &str, payload: Option<&FOValue>) -> Result<T, DecodeFailure> {
    T::decode(carried(tag, payload)?).map_err(|why| why.payload(tag))
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const CLASSES: &[&str] = &[
            Agents::CLASS,
            Schedules::CLASS,
            Pins::CLASS,
            Context::CLASS,
            Transcript::CLASS,
        ];
        let (label, word) = tagged(v, CLASSES)?;
        let decode = |word| match label {
            Agents::CLASS => family(word).map(Self::Agents),
            Schedules::CLASS => family(word).map(Self::Schedules),
            Pins::CLASS => family(word).map(Self::Pins),
            Context::CLASS => family(word).map(Self::Context),
            Transcript::CLASS => family(word).map(Self::Transcript),
            _ => Err(unknown(label, CLASSES)),
        };
        if !CLASSES.contains(&label) {
            return Err(unknown(label, CLASSES));
        }
        decode(carried(label, word)?).map_err(|why| why.payload(label))
    }
}

impl Request {
    /// How the answer this request is owed is checked, named before the
    /// request crosses.
    pub(crate) fn owed(&self) -> fn(&FOValue) -> Result<(), DecodeFailure> {
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

fn is<T: Datum>(v: &FOValue) -> Result<(), DecodeFailure> {
    T::decode(v).map(drop)
}

fn unit(v: &FOValue) -> Result<(), DecodeFailure> {
    <()>::decode(v)
}

/// `` `some `` a slot's card, or `` `none `` for an empty one.
fn pinned(v: &FOValue) -> Result<(), DecodeFailure> {
    const TAGS: &[&str] = &["none", "some"];
    let (label, payload) = tagged(v, TAGS)?;
    match label {
        "none" => bare(label, payload),
        "some" => datum::<Card>(label, payload).map(drop),
        _ => Err(unknown(label, TAGS)),
    }
}

// ── `exarch-agents` ──────────────────────────────────────────────────────

pub(crate) enum Agents {
    List,
    Start(Start),
    Message(Message),
    Stop(String),
    Dismiss(String),
    Reply(FOValue),
    Read(String),
}

impl Family for Agents {
    const CLASS: &'static str = "agents";
    const TAGS: &'static [&'static str] = &[
        "list", "start", "message", "stop", "dismiss", "reply", "read",
    ];

    fn request(self) -> Request {
        Request::Agents(self)
    }

    fn encode(self) -> FOValue {
        match self {
            Self::List => tag("list", None),
            Self::Start(start) => tag("start", Some(start.encode())),
            Self::Message(message) => tag("message", Some(message.encode())),
            Self::Stop(name) => tag("stop", Some(name.encode())),
            Self::Dismiss(name) => tag("dismiss", Some(name.encode())),
            Self::Reply(value) => tag("reply", Some(value)),
            Self::Read(name) => tag("read", Some(name.encode())),
        }
    }

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, DecodeFailure>> {
        Some(match label {
            "list" => bare(label, payload).map(|()| Self::List),
            "start" => datum(label, payload).map(Self::Start),
            "message" => datum(label, payload).map(Self::Message),
            "stop" => datum(label, payload).map(Self::Stop),
            "dismiss" => datum(label, payload).map(Self::Dismiss),
            "reply" => Ok(Self::Reply(payload.cloned().unwrap_or(FOValue::Unit))),
            "read" => datum(label, payload).map(Self::Read),
            _ => return None,
        })
    }
}

/// A model's own `exarch-agents` argument: a whole request, or a spawn's
/// spec alone, since its builtin marshals the parent.
pub(crate) enum Word {
    Ask(Agents),
    Spawn(Launch),
}

impl Word {
    /// # Errors
    /// The same typed refusal the desk would return.
    pub(crate) fn decode(word: &FOValue) -> Result<Self, DecodeFailure> {
        read_tag::<Agents, _>(word, |label, payload| match label {
            "start" => Some(datum(label, payload).map(Self::Spawn)),
            label => <Agents as Family>::decode(label, payload).map(|ask| ask.map(Self::Ask)),
        })
    }
}

/// `` `start ``: the model's spec and its marshalled parent.
pub(crate) struct Start {
    pub(crate) spec: Spec,
    pub(crate) parent: Marshalled,
}

record!(Start {
    spec: "spec",
    parent: "parent",
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let FOValue::Map { entries } = v else {
            return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Map),
                found: Found::of(v),
            })));
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

/// A [`Launch`] as the desk sees it: the grant rides in the marshalled parent.
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
/// so no path can mint an agent around it, however a name reached it.
///
/// # Errors
/// The name that violates the tab-bar contract.
pub fn check_name(name: &str) -> Result<(), AgentNameFailure> {
    if !name.is_empty()
        && name.len() <= 24
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Ok(());
    }
    Err(AgentNameFailure { name: name.into() })
}

/// An agent's name, admitted by [`check_name`]; `Fleet::claim`
/// is what makes that unskippable.
pub(crate) struct Name(pub(crate) String);

impl Datum for Name {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let name = String::decode(v)?;
        check_name(&name).map_err(|cause| DecodeFailure::new(DecodeCause::AgentName(cause)))?;
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let (label, payload) = tagged(v, &["amnemon", "mnemon"])?;
        match label {
            "amnemon" => bare(label, payload).map(|()| Self::Amnemon),
            "mnemon" => bare(label, payload).map(|()| Self::Mnemon),
            _ => Err(unknown(label, &["amnemon", "mnemon"])),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let tags: Vec<_> = std::iter::once("inherit")
            .chain(crate::policy::spawn_bases())
            .chain(["restrict"])
            .collect();
        let (label, payload) = tagged(v, &tags)?;
        match label {
            "inherit" => bare(label, payload).map(|()| Self(SpawnGrant::Inherit)),
            "restrict" => {
                let record = carried(label, payload)?;
                if !matches!(record, FOValue::Map { .. }) {
                    return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                        expected: Expected::Shape(ValueShape::Map),
                        found: Found::of(record),
                    }))
                    .payload(label));
                }
                Ok(Self(SpawnGrant::Restrict(record.clone())))
            }
            base if crate::policy::spawn_bases().any(|b| b == base) => {
                bare(label, payload).map(|()| Self(SpawnGrant::Base(base.into())))
            }
            _ => Err(unknown(label, &tags)),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const TAGS: &[&str] = &["inherit", "role"];
        let (label, payload) = tagged(v, TAGS)?;
        match label {
            "inherit" => bare(label, payload).map(|()| Self::Inherit),
            "role" => {
                let name: String = datum(label, payload)?;
                if name.is_empty() {
                    return Err(DecodeFailure::new(DecodeCause::EmptyProviderRole).payload(label));
                }
                Ok(Self::Role(name))
            }
            _ => Err(unknown(label, TAGS)),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        exact_keys(v, &["name", "reply"])?;
        Ok(Self {
            name: field(v, "name")?,
            reply: field(v, "reply")?,
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
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
/// tree. The human has no agent name to give.
pub enum Spawner {
    Human,
    Agent(String),
}

impl Datum for Spawner {
    fn encode(self) -> FOValue {
        match self {
            Self::Human => tag("human", None),
            Self::Agent(name) => tag("agent", Some(name.encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const TAGS: &[&str] = &["human", "agent"];
        let (label, payload) = tagged(v, TAGS)?;
        match label {
            "human" => bare(label, payload).map(|()| Self::Human),
            "agent" => datum(label, payload).map(Self::Agent),
            _ => Err(unknown(label, TAGS)),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        ral_core::data::datum::labelled(
            v,
            &[
                Self::Busy,
                Self::WaitingOnAgents,
                Self::Replied,
                Self::Waiting,
            ],
            |state| match state {
                Self::Busy => "busy",
                Self::WaitingOnAgents => "waiting-on-agents",
                Self::Replied => "replied",
                Self::Waiting => "waiting",
            },
        )
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

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, DecodeFailure>> {
        Some(match label {
            "list" => bare(label, payload).map(|()| Self::List),
            "add" => datum(label, payload).map(Self::Add),
            "remove" => datum(label, payload).map(Self::Remove),
            _ => return None,
        })
    }
}

pub(crate) struct Add {
    pub(crate) trigger: Asked,
    pub(crate) label: String,
    pub(crate) prompt: String,
}

record!(Add {
    trigger: "trigger",
    label: "label",
    prompt: "prompt",
});

/// Parsed on arrival at either end, so a malformed expression carries its
/// parser's typed cause home.
impl Datum for Asked {
    fn encode(self) -> FOValue {
        match self {
            Self::Cron { expr, .. } => tag("cron", Some(expr.encode())),
            Self::After(delay) => tag("after", Some(fmt_duration(delay).encode())),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const TAGS: &[&str] = &["cron", "after"];
        let (label, payload) = tagged(v, TAGS)?;
        match label {
            "cron" => {
                let expr: String = datum(label, payload)?;
                let schedule = CronSchedule::parse(&expr)
                    .map_err(|cause| DecodeFailure::new(DecodeCause::Cron(cause)).payload(label))?;
                Ok(Self::Cron { schedule, expr })
            }
            "after" => {
                let delay: String = datum(label, payload)?;
                parse_duration(&delay).map(Self::After).map_err(|cause| {
                    DecodeFailure::new(DecodeCause::Duration(cause)).payload(label)
                })
            }
            _ => Err(unknown(label, TAGS)),
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

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, DecodeFailure>> {
        Some(match label {
            "set" => datum(label, payload).map(Self::Set),
            "clear" => datum(label, payload).map(Self::Clear),
            "read" => datum(label, payload).map(Self::Read),
            "list" => bare(label, payload).map(|()| Self::List),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let card = value_to_card(v).ok_or_else(|| {
            DecodeFailure::new(DecodeCause::Card(CardFailure::NotCard(Found::of(v))))
        })?;
        if card.marks().is_empty() {
            return Err(DecodeFailure::new(DecodeCause::Card(CardFailure::Empty)));
        }
        Ok(card)
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

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, DecodeFailure>> {
        Some(match label {
            "survey" => bare(label, payload).map(|()| Self::Survey),
            "evict" => datum(label, payload).map(Self::Evict),
            _ => return None,
        })
    }
}

/// The run `[from, to]` that leaves, both ends inclusive, and the note its
/// eviction keeps in its place. An inverted run is refused here, once.
pub(crate) struct Evict {
    pub(crate) span: Span,
    pub(crate) note: Note,
}

impl Typed for Evict {
    fn ty() -> Ty {
        memoized_ty!(closed_record(&[
            ("from", u64::ty()),
            ("to", u64::ty()),
            ("note", Note::ty())
        ]))
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        exact_keys(v, &["from", "to", "note"])?;
        Ok(Self {
            span: Span::new(field(v, "from")?, field(v, "to")?)
                .map_err(|e| DecodeFailure::new(DecodeCause::TurnSpan(e.into())))?,
            note: field(v, "note")?,
        })
    }
}

/// An eviction's note: one short line, since the eviction keeps it in place
/// of the run for the rest of the session.
pub(crate) struct Note(pub(crate) String);

impl Note {
    const CAP: usize = 240;
}

impl Typed for Note {
    fn ty() -> Ty {
        Ty::string()
    }
}

impl Datum for Note {
    fn encode(self) -> FOValue {
        self.0.encode()
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let note = String::decode(v)?;
        let cause = if note.is_empty() {
            Some(NoteFailure::Empty)
        } else if note.len() > Self::CAP {
            Some(NoteFailure::TooLong(NoteFailureTooLong {
                bytes: note.len().into(),
                limit: Self::CAP.into(),
            }))
        } else if note.contains(['\n', '\r']) {
            Some(NoteFailure::Multiline)
        } else {
            None
        };
        if let Some(cause) = cause {
            return Err(DecodeFailure::new(DecodeCause::Note(cause)));
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

/// One item, turn or eviction, by its span.
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

    fn decode(label: &str, payload: Option<&FOValue>) -> Option<Result<Self, DecodeFailure>> {
        Some(match label {
            "index" => bare(label, payload).map(|()| Self::Index),
            "read" => datum(label, payload).map(Self::Read),
            "grep" => datum(label, payload).map(Self::Grep),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        exact_keys(v, &["pattern", "turns"])?;
        let pattern: String = field(v, "pattern")?;
        Ok(Self {
            pattern: Regex::new(&pattern).map_err(|e| {
                let cause = match e {
                    regex::Error::Syntax(detail) => RegexCause::Syntax(detail),
                    regex::Error::CompiledTooBig(limit) => RegexCause::CompiledTooBig(limit.into()),
                    other => RegexCause::Unclassified(other.to_string()),
                };
                DecodeFailure::new(DecodeCause::Regex(RegexFailure { pattern, cause }))
                    .field("pattern")
            })?,
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        exact_keys(v, &["from", "to"])?;
        Self::new(field(v, "from")?, field(v, "to")?)
            .map_err(|e| DecodeFailure::new(DecodeCause::TurnSpan(e.into())))
    }
}

/// `` `resident ``, `` `evicted [from: Int, to: Int] `` naming the smallest
/// later eviction over the item, or `` `withdrawn ``.
impl Datum for Location {
    fn encode(self) -> FOValue {
        match self {
            Self::Evicted(span) => tag("evicted", Some(span.encode())),
            location => tag(location.as_str(), None),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const TAGS: &[&str] = &["resident", "withdrawn", "evicted"];
        let (label, payload) = tagged(v, TAGS)?;
        match label {
            "resident" => bare(label, payload).map(|()| Self::Resident),
            "withdrawn" => bare(label, payload).map(|()| Self::Withdrawn),
            "evicted" => datum(label, payload).map(Self::Evicted),
            _ => Err(unknown(label, TAGS)),
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let (label, payload) = untag(v).ok_or_else(|| {
            DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Variant),
                found: Found::of(v),
            }))
        })?;
        bare(label, payload).map(|()| Self(label.into()))
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        const TAGS: &[&str] = &["text", "program", "result", "reasoning", "binary", "custom"];
        let (label, payload) = tagged(v, TAGS)?;
        if !TAGS.contains(&label) {
            return Err(unknown(label, TAGS));
        }
        let r = carried(label, payload)?;
        (|| match label {
            "text" | "result" | "reasoning" => {
                exact_keys(r, &["content"])?;
                let content = field(r, "content")?;
                Ok(match label {
                    "text" => Self::Text(content),
                    "result" => Self::Result(content),
                    _ => Self::Reasoning(content),
                })
            }
            "program" => {
                exact_keys(r, &["tool", "source", "keys"])?;
                Ok(Self::Program {
                    tool: field(r, "tool")?,
                    source: field(r, "source")?,
                    keys: field(r, "keys")?,
                })
            }
            "binary" => {
                exact_keys(r, &["content-type", "name", "bytes"])?;
                Ok(Self::Binary {
                    content_type: field(r, "content-type")?,
                    name: field(r, "name")?,
                    bytes: field(r, "bytes")?,
                })
            }
            "custom" => {
                exact_keys(r, &["provider", "model"])?;
                Ok(Self::Custom {
                    provider: field(r, "provider")?,
                    model: field(r, "model")?,
                })
            }
            _ => Err(unknown(label, TAGS)),
        })()
        .map_err(|why: DecodeFailure| why.payload(label))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::environ::EnvVars;

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
        let parent = ral_core::HostSurface::default()
            .shell(
                ral_core::io::terminal::TerminalState::default(),
                ral_core::test_helper::process_cwd(),
            )
            .expect("a test host resolves its baseline")
            .marshal(SpawnGrant::Inherit)
            .expect("a test parent marshals");
        let start = |spec, parent| Request::Agents(Agents::Start(Start { spec, parent }));
        round_trips([
            Request::Agents(Agents::List),
            start(spec(Memory::Amnemon, Selection::Inherit), parent.clone()),
            start(
                spec(Memory::Mnemon, Selection::Role("other-model".into())),
                parent,
            ),
            Request::Agents(Agents::Message(Message {
                to: "parent".into(),
                text: "hi".into(),
            })),
            Request::Agents(Agents::Stop("scout".into())),
            Request::Agents(Agents::Dismiss("scout".into())),
            Request::Agents(Agents::Reply(FOValue::Unit)),
            Request::Agents(Agents::Reply(restriction())),
            Request::Agents(Agents::Read("scout".into())),
        ]);
    }

    #[test]
    fn an_absent_or_unit_reply_payload_is_unit() {
        for payload in [None, Some(FOValue::Unit)] {
            let word = tag("reply", payload);
            assert!(matches!(
                Word::decode(&word),
                Ok(Word::Ask(Agents::Reply(FOValue::Unit)))
            ));
            let reply = family::<Agents>(&word).unwrap_or_else(|why| panic!("{why}"));
            assert_eq!(reply.encode(), tag("reply", None));
        }
    }

    #[test]
    fn non_unit_agent_requests_still_require_their_payloads() {
        for label in ["start", "message", "stop", "dismiss", "read"] {
            for payload in [None, Some(FOValue::Unit)] {
                assert!(family::<Agents>(&tag(label, payload)).is_err(), "{label}");
            }
        }
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
            add(Asked::After(parse_duration("2h").unwrap())),
            add(Asked::Cron {
                schedule: CronSchedule::parse(expr).unwrap(),
                expr: expr.into(),
            }),
            Request::Schedules(Schedules::Remove("nightly".into())),
        ]);
    }

    #[test]
    fn the_harness_only_at_trigger_has_no_wire_spelling() {
        let at = tag("at", Some(jiff::Timestamp::now().to_string().encode()));
        assert!(Asked::decode(&at).is_err());
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

    /// Each family's own answer passes the validator its request names, and
    /// no record or scalar of the wrong shape does.
    #[test]
    fn every_request_owes_the_answer_of_its_own_shape() {
        let text = |t: &str| t.to_string();
        let agent = AgentInfo {
            name: text("scout"),
            spawner: Spawner::Human,
            log_dir: PathBuf::from("/log"),
            elapsed: Duration::from_secs(3),
            state: RosterState::Busy,
            idle: Duration::ZERO,
        };
        let summary = Summary {
            live: 1,
            replied: 0,
        };
        let schedule = ScheduleInfo {
            label: text("nightly"),
            trigger: text("after 2h"),
            next_s: 7,
            fires: 0,
        };
        let card = Card(vec![crate::card::Mark::Text {
            spans: vec![crate::card::Span::plain("hi")],
        }]);
        let evict = || Evict {
            span: Span::new(1, 3).unwrap(),
            note: Note("n".into()),
        };
        let row = || Row {
            from: 1,
            to: 1,
            role: text("user"),
            kind: text("turn"),
            label: text("l"),
            bytes: 2,
        };
        let indexed = Indexed {
            from: 1,
            to: 1,
            role: text("user"),
            kind: text("turn"),
            label: text("l"),
            bytes: 2,
            location: Location::Resident,
        };
        let material = Material {
            turn: 1,
            role: text("user"),
            messages: vec![Said {
                role: Bare(text("user")),
                parts: vec![TranscriptPart::Text(text("x"))],
            }],
        };
        let hits = Hits {
            hits: vec![Hit {
                from: 1,
                to: 1,
                kind: text("turn"),
                role: text("user"),
                line: 1,
                text: text("x"),
            }],
            total: 1,
        };
        let survey = || Survey {
            rows: vec![row()],
            total_bytes: 2,
        };
        let table = [
            (Request::Agents(Agents::List), vec![agent].encode()),
            (
                Request::Agents(Agents::Read(text("scout"))),
                Deposit {
                    name: text("scout"),
                    reply: FOValue::Unit,
                }
                .encode(),
            ),
            (
                Request::Agents(Agents::Stop(text("scout"))),
                summary.encode(),
            ),
            (Request::Schedules(Schedules::List), vec![schedule].encode()),
            (Request::Pins(Pins::Clear(text("k"))), FOValue::Unit),
            (Request::Pins(Pins::Read(text("k"))), tag("none", None)),
            (
                Request::Pins(Pins::Read(text("k"))),
                tag("some", Some(card.encode())),
            ),
            (Request::Pins(Pins::List), vec![text("k")].encode()),
            (Request::Context(Context::Survey), survey().encode()),
            (Request::Context(Context::Evict(evict())), survey().encode()),
            (
                Request::Transcript(Transcript::Index),
                vec![indexed].encode(),
            ),
            (
                Request::Transcript(Transcript::Read(Reading { turns: vec![1] })),
                vec![material].encode(),
            ),
            (
                Request::Transcript(Transcript::Grep(Grep {
                    pattern: Regex::new("x|y").unwrap(),
                    turns: Turns::All,
                })),
                hits.encode(),
            ),
        ];
        for (request, answer) in table {
            let owed = request.owed();
            assert_eq!(owed(&answer), Ok(()), "{answer:?}");
            for foil in [FOValue::Int { value: 1 }, FOValue::Map { entries: vec![] }] {
                assert!(owed(&foil).is_err(), "{foil:?}");
            }
        }
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

    /// A model's `start` names its spec alone: its builtin marshals the parent.
    #[test]
    fn a_models_start_word_is_the_spec_awaiting_its_parent() {
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
        std::iter::once("inherit").chain(crate::policy::spawn_bases())
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
            DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Map),
                found: Found::of(&"everything".to_string().encode()),
            }))
            .payload("restrict")
        );
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
        let cwd = ral_core::path::CwdPath::new(std::env::current_dir().unwrap()).unwrap();
        let ctx = ral_core::authority::grant::freeze::FreezeCtx::at(EnvVars::new(), cwd)
            .with_scratch(Some(std::env::temp_dir().to_string_lossy()));
        for label in crate::policy::spawn_bases() {
            crate::policy::base_layer(label, &ctx)
                .unwrap_or_else(|e| panic!("grant `{label} must name a bake-in base: {e}"));
        }
        let err =
            crate::policy::base_layer("bogus", &ctx).expect_err("an unknown base must be refused");
        let ral_core::fact::Reason::Host(host) = err.reason else {
            panic!("host failure")
        };
        let ral_core::fact::HostFailure::Exarch(ral_core::fact::ExarchFailure::Policy(
            ral_core::fact::PolicyFailure::UnknownBase(base),
        )) = *host
        else {
            panic!("unknown policy base")
        };
        assert_eq!(base.name, "bogus");
        assert!(
            crate::policy::spawn_bases()
                .all(|label| base.offered.iter().any(|offered| offered == label))
        );
    }

    fn evict_with(entries: &[(&str, FOValue)]) -> Result<Context, DecodeFailure> {
        let entries = entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect();
        family::<Context>(&tag("evict", Some(FOValue::Map { entries })))
    }

    /// An eviction stands on its note, so an eviction without one never reaches
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
            DecodeFailure::new(DecodeCause::MissingField("note".into())).payload("evict")
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

    /// `location` is a variant, and `evicted` names the holder's span.
    #[test]
    fn an_index_row_encodes_its_span_and_holder() {
        let row = Indexed::from(SpanRow {
            span: Span::single(3),
            role: crate::record::Role::User,
            kind: crate::record::ItemKind::Turn(crate::record::TurnKind::Own),
            label: "first".into(),
            bytes: 7,
            location: Location::Evicted(Span::new(3, 4).unwrap()),
        })
        .encode();
        let int = |value| FOValue::Int { value };
        assert_eq!(row.field("from"), Some(&int(3)));
        assert_eq!(row.field("to"), Some(&int(3)));
        assert_eq!(
            row.field("location"),
            Some(&tag(
                "evicted",
                Some(FOValue::Map {
                    entries: vec![("from".into(), int(3)), ("to".into(), int(4))]
                })
            ))
        );
        let [back] = <[Indexed; 1]>::try_from(
            Vec::<Indexed>::decode(&FOValue::List {
                items: vec![row.clone()],
            })
            .unwrap(),
        )
        .unwrap_or_else(|_| panic!("one row in, one row out"));
        assert_eq!(back.encode(), row);
    }

    #[test]
    fn transcript_index_matches_the_context_guide_example() {
        use crate::record::{AgentId, AgentLog, EditAuthority, Voice};
        use genai::chat::ChatMessage;

        let mut log = AgentLog::for_test(AgentId::new(0), "m").unwrap().0;
        for _ in 0..30 {
            log.import_note(ChatMessage::user("earlier")).unwrap();
        }
        let label = "Reject duplicate flags in parse_options";
        let mut prompt = format!("{label}\n");
        let bytes = serde_json::to_vec(&ChatMessage::user(prompt.clone()))
            .unwrap()
            .len();
        prompt.push_str(&"x".repeat(1208 - bytes));
        for (prompt, answer) in [
            (prompt, "fixed"),
            ("check".into(), "checked"),
            ("review".into(), "reviewed"),
        ] {
            log.append_user(prompt, None, Voice::Human).unwrap();
            log.append_assistant(ChatMessage::assistant(answer), vec![], None)
                .unwrap();
        }
        let note =
            "Duplicate check added in parse_options. Regression test needed; checks not run.";
        log.evict(
            Span::new(31, 36).unwrap(),
            Some(note.into()),
            EditAuthority::Model,
        )
        .unwrap();
        let rows: Vec<_> = log
            .context()
            .transcript_index()
            .into_iter()
            .filter(|row| row.span.from() == 31)
            .map(Indexed::from)
            .collect::<Vec<_>>();
        // No FOValue printer exists: spell each real row by hand and require the guide to contain it.
        let guide = include_str!("../data/context.md");
        let doc_rows: String = guide
            .lines()
            .skip_while(|line| !line.contains("exarch-transcript `index"))
            .skip(1)
            .take_while(|line| line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rows.len(), 2, "the fixture yields the guide's two rows");
        for row in &rows {
            let location = match &row.location {
                Location::Resident => "`resident".to_string(),
                Location::Evicted(s) => format!("`evicted [from: {}, to: {}]", s.from(), s.to()),
                Location::Withdrawn => "`withdrawn".to_string(),
            };
            let text = format!(
                "[from: {}, to: {}, role: #'{}'#, kind: #'{}'#, label: #'{}'#, bytes: {}, location: {location}]",
                row.from, row.to, row.role, row.kind, row.label, row.bytes
            );
            assert!(doc_rows.contains(&text), "context.md lacks `{text}`");
        }
    }

    /// A tag that reads nothing refuses a payload rather than dropping it.
    #[test]
    fn a_payload_on_a_bare_tag_is_refused() {
        let Err(why) = family::<Pins>(&tag("list", Some(FOValue::Int { value: 1 }))) else {
            panic!("`list takes nothing");
        };
        assert_eq!(
            why,
            DecodeFailure::new(DecodeCause::UnexpectedPayload(UnexpectedPayload {
                tag: "list".into(),
                found: Found::Int(1),
            }))
        );
    }

    #[test]
    fn nested_schedule_requests_preserve_domain_causes_and_payload_paths() {
        let request = |trigger| {
            tag(
                "schedules",
                Some(tag(
                    "add",
                    Some(FOValue::Map {
                        entries: vec![
                            ("trigger".into(), trigger),
                            ("label".into(), "nightly".to_string().encode()),
                            ("prompt".into(), "wake".to_string().encode()),
                        ],
                    }),
                )),
            )
        };
        for (label, text, cause) in [
            (
                "cron",
                "* * * *",
                DecodeCause::Cron(CronSchedule::parse("* * * *").unwrap_err()),
            ),
            (
                "after",
                "0m",
                DecodeCause::Duration(parse_duration("0m").unwrap_err()),
            ),
        ] {
            let Err(error) = Request::decode(&request(tag(label, Some(text.to_string().encode()))))
            else {
                panic!("malformed trigger")
            };
            assert_eq!(
                error,
                DecodeFailure::new(cause)
                    .payload(label)
                    .field("trigger")
                    .payload("add")
                    .payload("schedules")
            );
        }
    }

    #[test]
    fn name_note_and_regex_validation_keep_typed_causes_at_their_present_fields() {
        let mut start = launch(Memory::Amnemon, Selection::Inherit);
        start.spec.name = Name("bad name".into());
        let Err(error) = Word::decode(&tag("start", Some(start.encode()))) else {
            panic!("invalid name")
        };
        assert_eq!(
            error,
            DecodeFailure::new(DecodeCause::AgentName(AgentNameFailure {
                name: "bad name".into()
            }))
            .field("name")
            .payload("start")
        );
        let Err(error) = Evict::decode(&FOValue::Map {
            entries: vec![
                ("from".into(), 1u64.encode()),
                ("to".into(), 2u64.encode()),
                ("note".into(), String::new().encode()),
            ],
        }) else {
            panic!("empty note")
        };
        assert_eq!(
            error,
            DecodeFailure::new(DecodeCause::Note(NoteFailure::Empty)).field("note")
        );
        let value = FOValue::Map {
            entries: vec![
                ("pattern".into(), "[".to_string().encode()),
                ("turns".into(), tag("all", None)),
            ],
        };
        let Err(error) = Grep::decode(&value) else {
            panic!("invalid regex")
        };
        assert_eq!(
            error.path,
            [ral_core::fact::DataStep::Field("pattern".into())]
        );
        assert!(
            matches!(*error.cause, DecodeCause::Regex(RegexFailure { pattern, cause: RegexCause::Syntax(_) }) if pattern == "[")
        );
    }
}
