//! The desk's own tests.

use super::*;
use crate::agent::roster::summary;
use crate::agent::testkit::ral_call;
use crate::bus::FleetSink;
use crate::bus::{Inbox, Signal, channel};
use crate::card::Card;
use crate::enquiry::AgentInfo;
use crate::enquiry::{
    Add, Evict, ForkClaim, Grant, Grep, Launch, Memory, Message, Name, Note, Pin, Reading,
    Selection, Start, Turns,
};
use crate::provider::{
    Provider,
    scripted::{Reply, Script},
};
use crate::record::AgentId;
use crate::record::AgentLog;
use crate::record::EditAuthority;
use crate::record::{Display, Record, Transient};
use crate::schedule::{Trigger, parse_duration};
use ral_core::SpawnGrant;
use ral_core::first_order::datum::tag;
use ral_core::types::NurseryId;
use regex::Regex;
use std::time::Duration;

fn fresh_log() -> AgentLog {
    AgentLog::for_test(
        AgentId::new(0),
        "test",
        &crate::record::RecordedAccount::for_test("test"),
    )
    .expect("session log")
}

/// A fresh fleet's trunk and the [`HostServices`] a desk running as it
/// captures, with `configure` free to narrow the trunk's own config
/// before it is born — `search: false` for the one case that must be
/// narrowed rather than granted, a custom `system_base`/`index` for the
/// bookend test, `returns: false` for the one refusal that keys on it.
/// Returns the fleet and the trunk's own inbox too, since a spawn test
/// needs both to drive `` `start `` end to end.
fn services_with(
    fuel: u32,
    launch: crate::agent::fleet::Launch,
    configure: impl FnOnce(&mut crate::agent::testkit::TestAgentSpec),
) -> (HostServices, Arc<Fleet>, Inbox) {
    let parent_inbox = Inbox::new();
    let fleet = Fleet::new(launch, crate::agent::fleet::AGENT_LEASE_IDLE);
    let mut spec = crate::agent::testkit::TestAgentSpec::new("parent");
    spec.mailbox = parent_inbox.mailbox();
    spec.fuel = fuel;
    spec.returns = true;
    spec.search = true;
    configure(&mut spec);
    let agent = crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh fleet's trunk");
    let (emit, _rx) = crate::bus::dummy_emitter();
    let services = HostServices {
        fleet: fleet.clone(),
        kind: SeatKind::Identity(Arc::new(crate::boot::test_transport())),
        stamp: agent.mailbox.stamp(),
        agent,
        emit,
        reply: ReplyCell::default(),
        log: LogCell::new(fresh_log()),
        branch: None,
        acts: ActFragment::default(),
        principal: ral_core::host::user(),
    };
    (services, fleet, parent_inbox)
}

/// The base capture every desk below builds on, so growing [`HostServices`]
/// means touching one literal, not three.
fn base_services() -> HostServices {
    services_with(3, crate::agent::fleet::Launch::for_test(), |_| {}).0
}

fn desk() -> ExarchDesk {
    ExarchDesk {
        services: base_services(),
    }
}

fn append_prompt_and_answer(log: &mut AgentLog, prompt: &str, answer: &str) {
    log.append_user(prompt.to_string(), None).expect("prompt");
    log.append_assistant(
        genai::chat::ChatMessage::assistant(answer),
        Vec::new(),
        None,
    )
    .expect("answer");
}

impl ExarchDesk {
    /// `request` as its door sends it: encoded by the vocabulary, and
    /// decoded again on arrival.
    pub(super) fn ask(&self, request: Request) -> Result<FOValue, Error> {
        self.handle(&request.encode())
    }
}

fn context_evict_request(turns: &[u64], note: Option<&str>) -> Request {
    Request::Context(Context::Evict(Evict {
        turns: turns.to_vec(),
        note: note.map(|note| Note(note.to_string())),
    }))
}

/// An empty address is well-typed, so a read naming no turn is the shape
/// the fold refuses rather than one the encoder cannot build.
fn transcript_read_request(turns: &[u64]) -> Request {
    Request::Transcript(Transcript::Read(Reading {
        turns: turns.to_vec(),
    }))
}

fn transcript_grep_request(pattern: &str, turns: Option<&[u64]>) -> Request {
    Request::Transcript(Transcript::Grep(Grep {
        pattern: Regex::new(pattern).expect("a test pattern compiles"),
        turns: turns.map_or(Turns::All, |t| Turns::Only(t.to_vec())),
    }))
}

fn int_field(value: &FOValue, key: &str) -> i64 {
    value
        .field(key)
        .and_then(FOValue::as_int)
        .unwrap_or_else(|| panic!("record has no Int field `{key}`"))
}

fn str_field<'a>(row: &'a FOValue, key: &str) -> Option<&'a str> {
    row.field(key).and_then(FOValue::as_str)
}

fn text(value: &str) -> FOValue {
    value.to_string().encode()
}

/// A malformed request, spelt by hand because the vocabulary will not
/// build it: the family names the class, the tag what to do.
fn family_req(family: &str, label: &str, payload: Option<FOValue>) -> FOValue {
    tag(family, Some(tag(label, payload)))
}

/// The model's plainest spawn record: `` `amnemon ``, inheriting
/// provider and model.
pub(super) fn spec(prompt: &str, name: &str, grant: SpawnGrant, search: bool) -> Launch {
    Launch {
        prompt: prompt.to_string(),
        name: Name(name.to_string()),
        memory: Memory::Amnemon,
        grant: Grant(grant),
        search,
        provider: Selection::Inherit,
        model: Selection::Inherit,
    }
}

pub(super) fn confined() -> SpawnGrant {
    SpawnGrant::Base("confined".to_string())
}

pub(super) fn start(fork: ForkClaim, spec: Launch) -> Request {
    Request::Agents(Agents::Start(Start { spec, fork }))
}

/// The in-process shape: the fork waits in this host's own nursery.
pub(super) fn start_req(session: NurseryId, prompt: &str, name: &str, search: bool) -> Request {
    start(
        ForkClaim::Parked(session),
        spec(prompt, name, confined(), search),
    )
}

pub(super) fn message_req(to: &str, text: &str) -> Request {
    Request::Agents(Agents::Message(Message {
        to: to.to_string(),
        text: text.to_string(),
    }))
}

fn reply_req(value: FOValue) -> Request {
    Request::Agents(Agents::Reply(value))
}

/// `` `exarch-schedules `add `` with an `` `after `` trigger — the only
/// kind these tests arm, since a cron's first fire is not a fixed delay.
fn add_req(after: &str, label: &str, prompt: &str) -> Request {
    Request::Schedules(Schedules::Add(Add {
        trigger: Trigger::After(parse_duration(after).expect("a test duration parses")),
        label: label.to_string(),
        prompt: prompt.to_string(),
    }))
}

fn remove_req(label: &str) -> Request {
    Request::Schedules(Schedules::Remove(label.to_string()))
}

/// Unwrap a summary answer into `(live, replied)`.
pub(super) fn summary_counts(answer: &FOValue) -> (usize, usize) {
    let counts = crate::enquiry::Summary::decode(answer).expect("a summary answer");
    (counts.live, counts.replied)
}

/// The rows `` `list `` answers, for a test whose transition no longer
/// carries them.
pub(super) fn listed(desk: &ExarchDesk) -> Vec<AgentInfo> {
    let rows = desk
        .ask(Request::Agents(Agents::List))
        .expect("`list answers the rows");
    Vec::decode(&rows).expect("`list answers roster rows")
}

/// Unwrap a `` `exarch-schedules `` answer into its rows.
fn table(answer: FOValue) -> Vec<FOValue> {
    let FOValue::List { items } = answer else {
        panic!("every `exarch-schedules tag answers the bare table")
    };
    items
}

/// [`desk`] holding the self-wakeup grant, so a schedule test reaches past it.
fn granted_desk() -> ExarchDesk {
    ExarchDesk {
        services: services_with(
            3,
            crate::agent::fleet::Launch {
                allow_schedule: true,
                ..crate::agent::fleet::Launch::for_test()
            },
            |_| {},
        )
        .0,
    }
}

/// A desk whose parent holds the very inbox this returns, so
/// `` `start ``/`` `cancel ``/`` `message `` run end to end and a child's
/// result is observable — unlike [`desk`].
fn spawnable_desk(fuel: u32) -> (Arc<ExarchDesk>, Arc<Fleet>, Inbox) {
    let (services, fleet, parent_inbox) =
        services_with(fuel, crate::agent::fleet::Launch::for_test(), |_| {});
    (Arc::new(ExarchDesk { services }), fleet, parent_inbox)
}

/// Poll `inbox` for the next exchange-boundary item — a spawned child's
/// settled [`crate::bus::AgentResult`] lands here.
fn wait_for_settle(inbox: &Inbox) -> crate::bus::Next {
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

type Parked<R> = Box<dyn FnOnce(&ExarchDesk, NurseryId) -> R + Send>;

/// Answers the one `` `branch `` a real run enquires by running its `f`
/// against the fork that run parked, while the fork is still in its pen.
struct Parking<R> {
    desk: Arc<ExarchDesk>,
    f: Mutex<Option<Parked<R>>>,
    out: Mutex<Option<R>>,
}

impl<R: Send> Host for Parking<R> {
    fn surface(&self, _val: &FOValue) {}

    fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
        let Ok(Request::Agents(Agents::Branch(ForkClaim::Parked(id)))) = Request::decode(&req)
        else {
            panic!("an identity run's `branch parks its fork")
        };
        let f = self.f.lock().unwrap().take().expect("one enquiry per run");
        *self.out.lock().unwrap() = Some(f(&self.desk, id));
        Ok(FOValue::Unit)
    }
}

/// `f`'s answer about a fork a real run parked in the desk's own parent
/// transport — the one door an identity fork reaches a desk by.
fn with_parked<R: Send + 'static>(
    desk: &Arc<ExarchDesk>,
    f: impl FnOnce(&ExarchDesk, NurseryId) -> R + Send + 'static,
) -> R {
    let SeatKind::Identity(parent) = &desk.services.kind else {
        panic!("an identity desk parks in process")
    };
    let host = Arc::new(Parking {
        desk: desk.clone(),
        f: Mutex::new(Some(Box::new(f))),
        out: Mutex::default(),
    });
    let report = ral_core::carrier::dispatch_to_report(
        &**parent,
        ral_core::protocol::Run::captured("_exarch-branch", "<test>"),
        host.clone(),
    )
    .expect("an identity engine never severs");
    host.out
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("the run never enquired: {report:?}"))
}

/// Whether the fork `id` still waits in the desk's parent's pen.
fn still_parked(desk: &ExarchDesk, id: NurseryId) -> bool {
    let SeatKind::Identity(parent) = &desk.services.kind else {
        panic!("an identity desk parks in process")
    };
    parent.adopt_parked(id, &SpawnGrant::Inherit).is_ok()
}

#[test]
fn unknown_class_answers_the_extension_error() {
    let err = desk()
        .handle(&FOValue::Variant {
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
    for class in ["agents", "schedules", "pins", "context", "transcript"] {
        let err = desk()
            .handle(&family_req(class, "no-such-tag", None))
            .expect_err("an unrecognised tag must not answer Ok");
        assert!(
            err.message.starts_with(&format!(
                "unrecognised tag in `exarch-{class} `no-such-tag`: "
            )),
            "got: {}",
            err.message
        );
    }
}

/// Names the expected shape rather than panicking or silently defaulting.
#[test]
fn non_variant_request_errors_didactically() {
    let err = desk()
        .handle(&FOValue::Unit)
        .expect_err("a non-variant request must not answer Ok");
    assert!(
        err.message.contains("must be a variant"),
        "error must name the expected shape, got: {}",
        err.message
    );
}

#[test]
fn context_survey_rows_every_resident_turn() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "closed\nturn", "answer");
        append_prompt_and_answer(&mut log, "evicted", "answer");
        log.evict(&[1, 2], None, EditAuthority::Model)
            .expect("evict");
        log.import_note(genai::chat::ChatMessage::user("inherited\ncontext"))
            .expect("import");
        log.append_user("live".into(), None).expect("live prompt");
    }
    let expected_bytes = desk.services.log.borrow().context().history_bytes();

    let answer = desk
        .ask(Request::Context(Context::Survey))
        .expect("context survey");
    let kinds = survey_rows(&answer)
        .iter()
        .map(|row| str_field(row, "kind").expect("survey kind"))
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["own", "own", "import", "own"]);
    assert_eq!(
        int_field(&answer, "total-bytes"),
        i64::try_from(expected_bytes).unwrap()
    );
}

/// One record per turn the read named, not one concatenated blob: the
/// list is the shape the doc's own "read in slices" advice needs to be
/// sayable.  A read is a listing, so it commits no act and draws no row.
#[test]
fn transcript_answers_one_record_per_turn_without_committing_an_act() {
    let mut desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "first prompt", "first answer");
    }
    let (tx, rx) = channel();
    desk.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    desk.services.emit = Emitter::new(tx, AgentId::new(0));

    let FOValue::List { items } = desk
        .ask(transcript_read_request(&[1, 2]))
        .expect("exarch-transcript `read")
    else {
        panic!("exarch-transcript `read must answer a list, one record per turn")
    };
    let [first, second] = items.as_slice() else {
        panic!("two named turns must answer exactly two records, got {items:?}")
    };
    assert_eq!(int_field(first, "turn"), 1);
    assert_eq!(str_field(first, "role"), Some("user"));
    let messages = first
        .field("messages")
        .and_then(FOValue::as_list)
        .unwrap_or_else(|| panic!("a read's messages are a list, got {first:?}"));
    assert_eq!(
        messages.len(),
        1,
        "the prompt turn holds one message, got {messages:?}"
    );
    assert_eq!(
        int_field(second, "turn"),
        2,
        "the second record is the turn asked for after it"
    );
    assert!(desk.services.acts.audit().is_none(), "a read has no act");

    desk.ask(transcript_read_request(&[]))
        .expect_err("a read that names no turn is not meaningful");
    let drawn = crate::bus::drain_records(&rx)
        .into_iter()
        .filter(|record| matches!(record, Record::Display(Display::HarnessCall { .. })))
        .count();
    assert_eq!(
        drawn, 0,
        "neither the read nor its refusal draws an act row: a listing's telling is its answer"
    );
}

/// A read answers the turns it named wherever they lie, each naming the
/// role it bears; a turn past what is recorded, and the turn
/// being written, are refused by the state they meet.
#[test]
fn transcript_read_names_the_role_of_every_turn_it_answers() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "first prompt", "first answer");
        append_prompt_and_answer(&mut log, "second prompt", "second answer");
    }
    let FOValue::List { items } = desk
        .ask(transcript_read_request(&[2, 3]))
        .expect("closed turns are readable")
    else {
        panic!("exarch-transcript `read must answer a list, one record per turn")
    };
    let reached = items
        .iter()
        .map(|item| {
            (
                int_field(item, "turn"),
                str_field(item, "role").expect("a read names the turn's role"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reached,
        vec![(2, "assistant"), (3, "user")],
        "an answer and the prompt after it, each naming its own role"
    );

    let error = desk
        .ask(transcript_read_request(&[9]))
        .expect_err("a read must not reach past what is recorded");
    assert_eq!(error.message, "turn 9 is not recorded: the latest is 4");

    desk.services
        .log
        .borrow_mut()
        .append_user("live".into(), None)
        .expect("live prompt");
    let error = desk
        .ask(transcript_read_request(&[5]))
        .expect_err("the turn being written is not readable");
    assert_eq!(
        error.message,
        "turn 5 is being written now: it is the one turn the transcript cannot read back yet"
    );
}

/// The pattern is compiled on decode, so an invalid one is refused in the
/// regex crate's own words rather than in a paraphrase of them.
#[test]
#[expect(
    clippy::invalid_regex,
    reason = "the pattern that will not compile is this test's subject"
)]
fn grep_refuses_a_bad_regex_with_the_crate_message() {
    const UNCLOSED: &str = "(unclosed";
    let desk = desk();
    let expected = Regex::new(UNCLOSED)
        .expect_err("an unclosed group is not a regex")
        .to_string();
    let error = desk
        .handle(&family_req(
            "transcript",
            "grep",
            Some(FOValue::Map {
                entries: vec![("pattern".to_string(), text(UNCLOSED))],
            }),
        ))
        .expect_err("an invalid regex is not searchable");
    assert!(error.message.ends_with(&expected), "got: {}", error.message);
}

/// `turn` for `turns` once searched the whole transcript in silence: the
/// reader accepted the payload and no one ever asked for that field.
#[test]
fn transcript_grep_refuses_a_field_it_does_not_read() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "first prompt", "first answer");
    }
    let err = desk
        .handle(&family_req(
            "transcript",
            "grep",
            Some(FOValue::Map {
                entries: vec![
                    ("pattern".to_string(), text("answer")),
                    (
                        "turn".to_string(),
                        FOValue::List {
                            items: vec![FOValue::Int { value: 1 }],
                        },
                    ),
                ],
            }),
        ))
        .expect_err("`turn` is not a field `grep` reads");
    assert_eq!(
        err.message,
        "`exarch-transcript `grep`: unknown field `turn`: did you mean `turns`?"
    );
    desk.ask(transcript_grep_request("answer", Some(&[1, 2])))
        .expect("the spelt field still narrows the search");
}

/// The desk hands the fold's refusal to the model verbatim.
#[test]
fn context_edit_refusals_surface_the_admissibility_sentence() {
    let live_desk = desk();
    {
        let mut live = live_desk.services.log.borrow_mut();
        live.append_user("live".into(), None).expect("live prompt");
    }
    let err = live_desk
        .ask(context_evict_request(&[1], None))
        .expect_err("the turn being written is not editable");
    assert_eq!(
        err.message,
        "turn 1 is being written now: an eviction keeps the work in hand"
    );

    let unknown_desk = desk();
    {
        let mut log = unknown_desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "answer");
    }
    let err = unknown_desk
        .ask(context_evict_request(&[7], None))
        .expect_err("an unrecorded turn is not editable");
    assert_eq!(err.message, "turn 7 is not recorded: the latest is 2");

    let evicted_desk = desk();
    {
        let mut log = evicted_desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "answer");
        append_prompt_and_answer(&mut log, "two", "answer");
        append_prompt_and_answer(&mut log, "three", "answer");
    }
    evicted_desk
        .ask(context_evict_request(&[1, 2], None))
        .expect("evict");
    let err = evicted_desk
        .ask(context_evict_request(&[1], None))
        .expect_err("a turn that has left is not addressable");
    assert_eq!(
        err.message,
        "turn 1 has already left your context: the earliest still in it is 3"
    );

    evicted_desk
        .ask(context_evict_request(&[], None))
        .expect_err("an empty address is not an edit");
}

/// The edit's answer is the survey the transition leaves behind, not a
/// receipt for the transition: the number that decides the next edit is
/// `total-bytes` now, against the budget. The act it commits is
/// [`DeskAct::ContextEvict`], and the audit sentence names it.
#[test]
fn context_evict_answers_the_survey_it_leaves_behind_and_commits_the_act() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "a longer answer");
        append_prompt_and_answer(&mut log, "two", "another answer");
    }
    let answer = desk
        .ask(context_evict_request(&[1, 2], Some("the parser is fixed")))
        .expect("context evict");
    assert_eq!(
        int_field(&answer, "total-bytes"),
        i64::try_from(desk.services.log.borrow().context().history_bytes()).unwrap(),
        "the survey's total is the context's own weight"
    );
    let rows = survey_rows(&answer);
    assert_eq!(
        rows.iter()
            .map(|row| (
                int_field(row, "id"),
                str_field(row, "role").expect("a survey row names its role")
            ))
            .collect::<Vec<_>>(),
        vec![(3, "user"), (4, "assistant")],
        "the evicted turns are gone from the answer"
    );
    assert_eq!(str_field(&rows[0], "kind"), Some("own"));
    let audit = desk
        .services
        .acts
        .audit()
        .expect("a landed eviction leaves an act");
    assert!(
        audit.contains("evicted context"),
        "the audit sentence must name the act, got: {audit}"
    );
}

/// The survey rows an answer carries, in id order.
fn survey_rows(answer: &FOValue) -> &[FOValue] {
    answer
        .field("rows")
        .and_then(FOValue::as_list)
        .expect("a context answer carries its survey rows")
}

/// An empty note would render `Your note at eviction: ""` in the marker,
/// so its decode refuses it rather than the shape allowing it.
#[test]
fn context_evict_refuses_an_empty_note() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "answer");
        append_prompt_and_answer(&mut log, "two", "answer");
    }
    let err = desk
        .ask(context_evict_request(&[1], Some("")))
        .expect_err("an empty note is not a note");
    assert_eq!(
        err.message,
        "`exarch-context `evict`: `note: text must not be empty: `none leaves no note"
    );
}

/// The marker keeps the note for the rest of the session, so the cap is
/// what stands between one short line and a summary.
#[test]
fn context_evict_refuses_a_note_over_the_cap() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "answer");
        append_prompt_and_answer(&mut log, "two", "answer");
    }
    let note = "x".repeat(241);
    let err = desk
        .ask(context_evict_request(&[1], Some(&note)))
        .expect_err("241 bytes is over the 240-byte cap");
    assert_eq!(
        err.message,
        "`exarch-context `evict`: `note: text is 241 bytes; the marker keeps one short line: 240 at most. What is the one thing your future self needs to know?"
    );
}

/// A line break in a note would draw an extra row in the marker, read as
/// one of the harness's own — the door refuses it rather than the
/// renderer alone standing between the two.
#[test]
fn context_evict_refuses_a_multiline_note() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "answer");
        append_prompt_and_answer(&mut log, "two", "answer");
    }
    for note in ["line one\nline two", "line one\rline two"] {
        let err = desk
            .ask(context_evict_request(&[1], Some(note)))
            .expect_err("a note that breaks a line is not one row");
        assert_eq!(
            err.message,
            "`exarch-context `evict`: `note: text must be a single line: the marker draws one row per turn the cut takes, and a line break in a note reads as one of them."
        );
    }
}

/// A field no tag reads is refused, not dropped: an eviction that silently
/// ignored `through` would cut turns the model never named.
#[test]
fn context_evict_refuses_a_field_it_does_not_read() {
    let desk = desk();
    {
        let mut log = desk.services.log.borrow_mut();
        append_prompt_and_answer(&mut log, "one", "answer");
        append_prompt_and_answer(&mut log, "two", "answer");
    }
    let err = desk
        .handle(&family_req(
            "context",
            "evict",
            Some(FOValue::Map {
                entries: vec![
                    (
                        "turns".to_string(),
                        FOValue::List {
                            items: vec![FOValue::Int { value: 1 }],
                        },
                    ),
                    ("through".to_string(), FOValue::Int { value: 2 }),
                ],
            }),
        ))
        .expect_err("`through` is not a field `evict` reads");
    assert_eq!(
        err.message,
        "`exarch-context `evict`: unknown field `through`: expected `turns`, `note`"
    );
    desk.ask(context_evict_request(&[1, 2], Some("the parser is fixed")))
        .expect("the fields the tag does read still evict");
}

/// `` `exarch-pins `set ``: a one-span text card under `key`.
fn pin_set_req(key: &str, text: &str) -> Request {
    let body = FOValue::Map {
        entries: vec![(
            "spans".into(),
            FOValue::List {
                items: vec![FOValue::Map {
                    entries: vec![("text".into(), text.to_string().encode())],
                }],
            },
        )],
    };
    let body = Card::decode(&tag("text", Some(body))).expect("a one-span text card");
    Request::Pins(Pins::Set(Pin {
        key: key.to_string(),
        body,
    }))
}

fn pin_clear_req(key: &str) -> Request {
    Request::Pins(Pins::Clear(key.to_string()))
}

fn pin_read_req(key: &str) -> Request {
    Request::Pins(Pins::Read(key.to_string()))
}

/// What a read answered: the card of a `some`, or `None` for a `none`.
fn slot(answer: FOValue) -> Option<FOValue> {
    match answer {
        FOValue::Variant { label, payload } if label == "some" => payload.map(|card| *card),
        FOValue::Variant {
            label,
            payload: None,
        } if label == "none" => None,
        other => panic!("a read answers `some or `none, got {other:?}"),
    }
}

/// A pin written through `` `exarch-pins `set `` comes back from
/// `` `exarch-pins `read `` as the canonical card — the readback and the
/// pinned mark agree on shape, which is the whole point of a readable
/// register.
#[test]
fn pin_read_returns_the_canonical_card() {
    let d = desk();
    d.ask(pin_set_req("tasks", "hi"))
        .expect("`exarch-pins `set` must answer Ok");

    let answer = slot(d.ask(pin_read_req("tasks")).expect("a hit must answer Ok"))
        .expect("a pinned key reads `some");
    let card = crate::card::value_to_card(&answer).expect("the readback must decode as a card");
    assert!(
        matches!(
            card.marks(),
            [crate::card::Mark::Text { spans }]
                if spans.len() == 1 && spans[0].role.is_none() && spans[0].text == "hi"
        ),
        "the canonical card must round-trip the pinned text mark, got {card:?}"
    );
}

/// `` `set ``/`` `clear `` write and empty the register mirror
/// `` `read ``/`` `list `` answer from.
#[test]
fn pin_set_and_clear_round_trip_through_the_desk() {
    let d = desk();

    assert!(
        slot(d.ask(pin_read_req("tasks")).expect("a read answers")).is_none(),
        "an unset key must read `none"
    );

    d.ask(pin_set_req("tasks", "hi"))
        .expect("`exarch-pins `set` must answer Ok");
    let answer = slot(
        d.ask(pin_read_req("tasks"))
            .expect("a set key must read back"),
    )
    .expect("a set key reads `some");
    let card = crate::card::value_to_card(&answer).expect("the readback must decode as a card");
    assert!(
        matches!(
            card.marks(),
            [crate::card::Mark::Text { spans }]
                if spans.len() == 1 && spans[0].text == "hi"
        ),
        "`set` must write the canonical card, got {card:?}"
    );

    d.ask(pin_clear_req("tasks"))
        .expect("`exarch-pins `clear` must answer Ok");
    assert!(
        slot(d.ask(pin_read_req("tasks")).expect("a read answers")).is_none(),
        "`clear` must empty the slot `set` wrote"
    );
}

/// A key never pinned, and a key unpinned after being pinned, both answer
/// `` `none `` — a miss and a clear are the same absence to `` `exarch-pins `read ``.
#[test]
fn pin_read_answers_none_on_miss_and_after_unpin() {
    let d = desk();

    assert!(
        slot(d.ask(pin_read_req("tasks")).expect("a read answers")).is_none(),
        "a key never pinned must read `none"
    );

    d.ask(pin_set_req("tasks", "hi"))
        .expect("`exarch-pins `set` must answer Ok");
    d.ask(pin_clear_req("tasks"))
        .expect("`exarch-pins `clear` must answer Ok");
    assert!(
        slot(d.ask(pin_read_req("tasks")).expect("a read answers")).is_none(),
        "an unpinned key must read `none"
    );
}

/// `` `exarch-pins `list `` names exactly the occupied keys, in
/// `BTreeMap` order, and tracks a set/clear pair.
#[test]
fn pin_list_tracks_set_and_clear() {
    let d = desk();
    let keys = |d: &ExarchDesk| match d
        .ask(Request::Pins(Pins::List))
        .expect("`exarch-pins `list` must answer Ok")
    {
        FOValue::List { items } => items
            .into_iter()
            .map(|v| match v {
                FOValue::String { value } => value,
                other => panic!("`exarch-pins `list` must answer strings, got {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("`exarch-pins `list` must answer a list, got {other:?}"),
    };

    assert!(keys(&d).is_empty(), "an empty register lists no keys");

    d.ask(pin_set_req("b", "one"))
        .expect("`exarch-pins `set` must answer Ok");
    d.ask(pin_set_req("a", "two"))
        .expect("`exarch-pins `set` must answer Ok");
    assert_eq!(
        keys(&d),
        vec!["a", "b"],
        "keys list in BTreeMap (lexicographic) order"
    );

    d.ask(pin_clear_req("b"))
        .expect("`exarch-pins `clear` must answer Ok");
    assert_eq!(keys(&d), vec!["a"], "a clear drops its key from the list");
}

/// Every surface class the live applier renders also records through the
/// seam — a `Display` record (or, for the pin register, `Forensic`) — in
/// the order it arrived.  A pin/unpin additionally publishes a `Transient`
/// beside its `Forensic` twin: the durable breadcrumb and the live
/// register the process is holding, two records for one act.
#[test]
fn live_surfaces_record_their_seam_twins() {
    use crate::bus::Signal;
    use crate::record::{Display, Forensic, Record, Transient};

    let d = desk();
    let (tx, rx) = channel();
    d.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(crate::bus::FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    let applier = SurfaceApplier::new(d.services.log.borrow().record_emitter());

    let read = ral_core::types::Observation::instant(
        None,
        None,
        ral_core::types::Observed::Read(ral_core::fact::Read {
            path: "a.rs".into(),
        }),
    );
    applier.live(&read.to_surface());
    applier.live(&FOValue::Variant {
        label: "card".into(),
        payload: Some(Box::new(FOValue::List { items: vec![] })),
    });
    applier.live(
        &ral_core::types::DoneEvent {
            cmd: "<block>".into(),
            outcome: ral_core::types::Done::Ok,
        }
        .to_surface(),
    );
    applier.live(
        &ral_core::types::Notice::Reap(ral_core::types::ReapNotice {
            id: ral_core::types::WorkerId(1),
            cmd: "sleep 10".into(),
            class: ral_core::types::LeaseClass::Worker,
            cause: ral_core::types::ReapCause::Idle,
        })
        .to_surface(),
    );
    d.ask(pin_set_req("tasks", "hi"))
        .expect("`exarch-pins `set` must answer Ok");
    d.ask(pin_clear_req("tasks"))
        .expect("`exarch-pins `clear` must answer Ok");

    let mut facts: Vec<&'static str> = Vec::new();
    let mut transients: Vec<&'static str> = Vec::new();
    while let Ok(sig) = rx.try_recv() {
        match sig {
            Signal::Fact(_, fact) => facts.push(match fact.value() {
                Record::Display(Display::Observation { .. }) => "io",
                Record::Display(Display::Card { .. }) => "card",
                Record::Display(Display::Done {
                    outcome: crate::card::DoneOutcome::Ok,
                    ..
                }) => "done",
                Record::Forensic(Forensic::Reap { cause, .. }) if cause == "idle" => "notice",
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
        "every class records its twin, in the order it surfaced"
    );
}

/// The receipt names the child, and its reply notice lands in the parent's
/// inbox once it settles — the value itself is fetched with `` `read ``.
#[test]
fn agent_start_spawns_and_delivers_result_to_parent_inbox() {
    let (desk, fleet, parent_inbox) = spawnable_desk(3);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'hi from child'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    let answer = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "say hi", "helper", true))
    })
    .expect("a valid `start must succeed");

    let (live, _) = summary_counts(&answer);
    assert_eq!(live, 1, "the child is the one other agent alive");
    let rows = listed(&desk);
    let child = rows
        .iter()
        .find(|row| row.name == "helper")
        .expect("the child stands on the listing");
    assert!(
        !child.log_dir.as_os_str().is_empty(),
        "a roster row carries the agent's log directory"
    );

    match wait_for_settle(&parent_inbox) {
        crate::bus::Next::Item(crate::bus::Item::Agent(result)) => {
            assert!(
                matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                "the child's reply notice must reach the parent's inbox, got: {:?}",
                result.outcome
            );
        }
        other => panic!("expected an Agent result item, got {other:?}"),
    }
    let helper = fleet.resolve("helper").expect("the child is still live");
    assert_eq!(
        desk.services
            .agent
            .descendant(&helper)
            .and_then(|child| child.reply()),
        Some(FOValue::String {
            value: "hi from child".into()
        }),
        "the deposited reply must be fetchable off the child"
    );
}

/// `` `read `` is the one tag that answers a record rather than the
/// roster: after a scripted child replies, the parent fetches exactly
/// what it deposited.
#[test]
fn agent_read_answers_the_childs_deposited_reply() {
    let (desk, _fleet, parent_inbox) = spawnable_desk(3);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'read me'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "say hi", "helper", true))
    })
    .expect("a valid `start must succeed");
    let _ = wait_for_settle(&parent_inbox);

    let answer = desk
        .ask(Request::Agents(Agents::Read("helper".into())))
        .expect("a descendant that has replied must answer its reply");
    assert_eq!(str_field(&answer, "name"), Some("helper"));
    assert_eq!(
        str_field(&answer, "reply"),
        Some("read me"),
        "`` `read `` must answer the very value the child handed to `reply"
    );
}

/// The state *is* the answer, not a receipt about the child just started:
/// the roster a spawn answers with carries a sibling that spawn never
/// touched, so nothing has to ask again to see what the fleet now is.
#[test]
fn start_answers_the_fleets_state_not_a_receipt() {
    let (desk, fleet, parent_inbox) = spawnable_desk(3);
    desk.services
        .agent
        .provider
        .swap(Arc::new(Provider::scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'a'",
            )])),
        )));
    // Held for the whole test, and with no worker behind it, so it never
    // settles out from under the assertion below.
    let mut sibling = crate::agent::testkit::TestAgentSpec::new("already-there");
    sibling.parent = Some(desk.services.agent.clone());
    let _already_there =
        crate::agent::testkit::test_agent(&fleet, sibling).expect("a fresh child of a live parent");

    let (live, _) = summary_counts(
        &with_parked(&desk, |desk, session| {
            desk.ask(start_req(session, "go", "helper", false))
        })
        .expect("the spawn must succeed"),
    );
    assert_eq!(
        live, 2,
        "the spawn counts the fleet's state, the sibling it did not start included"
    );
    let rows = listed(&desk);
    let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["already-there", "helper", "parent"],
        "and `list names them, the reader among them"
    );

    let _ = wait_for_settle(&parent_inbox);
}

/// Naming neither half is an inheritance, not a decision: the child gets
/// the parent's own `Arc<Provider>`, allocating nothing and asking the
/// bureau nothing.
#[test]
fn an_inheriting_spawn_hands_the_child_the_parents_own_provider() {
    let desk = desk();
    let parent = desk.services.agent.current_provider();
    let child = desk
        .child_provider(&Selection::Inherit, &Selection::Inherit)
        .expect("an inheriting spawn resolves nothing");
    assert!(
        Arc::ptr_eq(&parent, &child),
        "the child must share the parent's provider, not a rebuild of it"
    );
}

/// Spelling out the parent's own model is the same pair, so the same `Arc`.
#[test]
fn naming_the_parents_own_pair_hands_the_child_its_provider() {
    let desk = desk();
    let parent = desk.services.agent.current_provider();
    let child = desk
        .child_provider(
            &Selection::Inherit,
            &Selection::Named(parent.model().into()),
        )
        .expect("the parent's own pair needs no minting");
    assert!(Arc::ptr_eq(&parent, &child));
}

/// A scripted session mints nothing, so a spawn that names a selection is
/// refused saying so rather than silently inheriting.
#[test]
fn a_named_selection_under_a_scripted_bureau_is_refused() {
    let desk = desk();
    let Err(err) =
        desk.child_provider(&Selection::Inherit, &Selection::Named("other-model".into()))
    else {
        panic!("a scripted bureau must refuse to mint");
    };
    assert!(
        err.message.contains("scripted") && err.message.contains("mints no others"),
        "the refusal must say this session mints nothing, got: {}",
        err.message
    );
}

/// And it is refused before anything is adopted: no child is registered,
/// exactly as a missing spec field leaves none.
#[test]
fn a_refused_selection_never_registers_a_child() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    // Refused before any fork is taken up, so none need be parked.
    let err = desk
        .ask(start(
            ForkClaim::Parked(NurseryId(0)),
            Launch {
                model: Selection::Named("other-model".into()),
                ..spec("go", "helper", confined(), false)
            },
        ))
        .expect_err("a scripted bureau must refuse to mint");
    assert!(
        err.message.contains("`exarch-agents `start` refused"),
        "the refusal must name the tag it refused, got: {}",
        err.message
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        0,
        "a refused selection must never register a child"
    );
}

/// A `` `start `` whose spec record `edit` has spoilt, as no door sends it.
fn spoilt_start(edit: impl FnOnce(&mut Vec<(String, FOValue)>)) -> FOValue {
    let mut spec = spec("go", "scout", confined(), false).encode();
    let FOValue::Map { entries } = &mut spec else {
        unreachable!("a spec is a record")
    };
    edit(entries);
    let fork = ForkClaim::Parked(NurseryId(0)).encode();
    family_req(
        "agents",
        "start",
        Some(FOValue::Map {
            entries: vec![("spec".into(), spec), ("fork".into(), fork)],
        }),
    )
}

/// The desk reads the model's record by field name, so a missing field is
/// named — never a position the model never wrote.
#[test]
fn start_refuses_a_spec_missing_a_field_by_name() {
    let err = desk()
        .handle(&spoilt_start(|spec| spec.retain(|(key, _)| key != "name")))
        .expect_err("a spec missing `name` must be refused");
    assert_eq!(
        err.message,
        "`exarch-agents `start`: `spec: no `name field in a record of 6 fields"
    );
}

/// A misspelt spec field is refused with the field it most likely meant.
#[test]
fn start_refuses_a_misspelt_spec_field() {
    let err = desk()
        .handle(&spoilt_start(|spec| spec[1].0 = "nmae".into()))
        .expect_err("`nmae` is not a field the spec carries");
    assert_eq!(
        err.message,
        "`exarch-agents `start`: `spec: unknown field `nmae`: did you mean `name`?"
    );
}

/// A request above the parent's ceiling is narrowed, never refused — the
/// child's stack gains one more layer from [`crate::policy::base_layer`].
/// Only that half is visible here: the clamped bit lands in a private
/// `Agent` field, so `agent::build`'s fork test asserts the narrowing
/// itself.
#[test]
fn agent_start_admits_a_search_request_above_the_parents_ceiling() {
    let (services, _fleet, parent_inbox) =
        services_with(3, crate::agent::fleet::Launch::for_test(), |spec| {
            spec.search = false;
        });
    let desk = Arc::new(ExarchDesk { services });
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'done'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    let answer = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "go", "searcher", true))
    });
    assert!(
        answer.is_ok(),
        "a spawn asking for more search reach than its parent holds is narrowed, not refused"
    );
    let _ = wait_for_settle(&parent_inbox);
}

/// A `` `restrict `` carrying anything but a record is refused by the
/// desk's own decoder, before a base is named or a path is frozen.
#[test]
fn start_refuses_a_restriction_that_is_not_a_record() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    let err = desk
        .ask(start(
            ForkClaim::Parked(NurseryId(0)),
            spec(
                "go",
                "helper",
                SpawnGrant::Restrict(text("everything")),
                false,
            ),
        ))
        .expect_err("a `restrict that carries no record at all");
    assert_eq!(
        err.message,
        "`exarch-agents `start`: `spec: `grant: `restrict must carry a capability record \
         [exec, fs, net, detach, editor, shell], got a Str"
    );
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
        crate::record::Record::Forensic(crate::record::Forensic::SessionStarted {
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
    let template = format!(
        "persona\n\n# Builtins\n\n{}",
        crate::prompt::BUILTIN_INDEX_PLACEHOLDER
    );
    // The production seam: the index table resolves from the same booted
    // surface the parked child shells fork from.
    let index = crate::prompt::BuiltinIndex::resolve(
        crate::boot::test_shell()
            .builtin_names()
            .map(str::to_string)
            .collect(),
    );
    let (services, _fleet, parent_inbox) = services_with(
        3,
        crate::agent::fleet::Launch {
            system: template.as_str().into(),
            index,
            ..crate::agent::fleet::Launch::for_test()
        },
        |_| {},
    );
    let desk = Arc::new(ExarchDesk { services });
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'hi from child'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    let answer = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "say hi", "helper", true))
    })
    .expect("a valid `start must succeed");

    let _ = summary_counts(&answer);
    let rows = listed(&desk);
    let child = rows
        .iter()
        .find(|row| row.name == "helper")
        .expect("the child stands on the listing");

    let expected = desk
        .services
        .fleet
        .launch
        .index
        .apply(
            &template,
            &crate::prompt::Grants {
                returns: true,
                allow_schedule: desk.services.fleet.launch.allow_schedule,
                spawns: desk.services.agent.fuel.saturating_sub(1) > 0,
            },
            "helper",
        )
        .len();
    assert_eq!(
        recorded_system_prompt_bytes(&child.log_dir),
        expected,
        "the bookend must record the spawned child's own resolved \
         system, not the unresolved template HostServices captured"
    );

    // Drain the settle, so no background thread outlives this test.
    let _ = wait_for_settle(&parent_inbox);
}

#[test]
fn agent_start_refuses_at_zero_fuel_with_the_exhaustion_text() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(0);
    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let answer = desk.ask(start_req(session, "hi", "helper", true));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("zero fuel must refuse");
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
        left_parked,
        "a fuel refusal happens before adopt, so the parked fork must \
         stay for the run guard to reap, never claimed by a refused call"
    );
}

/// [`Fleet::name_live`] catches the ordinary case before the parked
/// fork is ever adopted, so the refused spawn registers no child.
#[test]
fn agent_start_refuses_a_name_already_borne_by_a_live_agent() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "r1",
            "exarch-agents `reply 'a'",
        )])),
    ));
    desk.services.agent.provider.swap(provider);

    let answer = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "go", "helper", true))
    });
    assert!(answer.is_ok(), "the first spawn must succeed");

    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let answer = desk.ask(start_req(session, "go again", "helper", true));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("a second spawn naming a live agent must be refused");
    assert!(
        err.message.contains("already bears the name 'helper'"),
        "got: {}",
        err.message
    );
    assert!(
        left_parked,
        "a name-collision refusal happens before adopt, so the parked \
         fork must stay for the run guard to reap, never claimed by a \
         refused call"
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        1,
        "the second, refused spawn must leave no child behind"
    );
}

/// The in-process door checks a name, but a wire peer need not have come
/// through one — so the spawn spine checks it too, before a log is forked
/// or a listener dialled, rather than leaving it all to the enrolment.
#[test]
fn agent_start_refuses_a_malformed_name_before_it_adopts_the_fork() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);

    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let answer = desk.ask(start_req(session, "go", "help/er", true));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("a malformed name must be refused");
    assert!(
        err.message.contains("ASCII letters"),
        "the refusal must carry the name rule; got: {}",
        err.message
    );
    assert!(
        left_parked,
        "the name is refused before adopt, so the parked fork must stay \
         for the run guard to reap"
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        0,
        "a refused spawn leaves no child behind"
    );
}

/// Siblings in one turn all succeed off the same captured fuel, since the
/// parent's own is never debited.
#[test]
fn fuel_bounds_depth_not_fanout() {
    let (desk, _fleet, parent_inbox) = spawnable_desk(1);
    let provider = Arc::new(Provider::scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'a'",
            )]))
            .then(Reply::tool_calls(vec![ral_call(
                "r2",
                "exarch-agents `reply 'b'",
            )]))
            .then(Reply::tool_calls(vec![ral_call(
                "r3",
                "exarch-agents `reply 'c'",
            )])),
    ));
    desk.services.agent.provider.swap(provider);

    for i in 0..3 {
        let answer = with_parked(&desk, move |desk, session| {
            desk.ask(start_req(session, "go", &format!("t{i}"), true))
        });
        assert!(
            answer.is_ok(),
            "sibling {i} must not be refused for lack of fuel: fuel \
             bounds depth, not fan-out"
        );
    }

    for _ in 0..3 {
        match wait_for_settle(&parent_inbox) {
            crate::bus::Next::Item(crate::bus::Item::Agent(result)) => assert!(
                matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                "every sibling must settle by replying, got: {:?}",
                result.outcome
            ),
            other => panic!("expected an Agent result item, got {other:?}"),
        }
    }
}

/// The capture went stale the instant the *calling* session's inbox
/// epoch moved past what was snapshotted at install.
#[test]
fn agent_start_refuses_after_clear() {
    let (desk, _fleet, parent_inbox) = spawnable_desk(3);
    parent_inbox.clear(); // the /clear gesture, on this caller
    let err = with_parked(&desk, |desk, session| {
        desk.ask(start_req(session, "hi", "helper", true))
    })
    .expect_err("a stale epoch must refuse");
    assert!(
        err.message.contains("cleared"),
        "must name the /clear cause, got: {}",
        err.message
    );
}

/// A refusal at adoption — here, a restriction record the capability
/// decoder will not read, which only the engine's own freeze discovers —
/// drops the adopted fork rather than leaving it to be adopted twice.
#[test]
fn refused_enquiry_leaves_the_nursery_empty() {
    let (desk, _fleet, _parent_inbox) = spawnable_desk(3);
    let (err, left_parked) = with_parked(&desk, |desk, session| {
        let restriction = FOValue::Map {
            entries: vec![("net".to_string(), text("yes"))],
        };
        let answer = desk.ask(start(
            ForkClaim::Parked(session),
            spec("go", "helper", SpawnGrant::Restrict(restriction), false),
        ));
        (answer, still_parked(desk, session))
    });
    let err = err.expect_err("a `net axis that is not a Bool must refuse");
    assert!(
        err.message.contains("net"),
        "must name the axis it could not read, got: {}",
        err.message
    );
    assert!(
        !left_parked,
        "a refusal downstream of adopt must not leave the fork \
         re-adoptable: it was already claimed and simply drops"
    );
}

/// `` `cancel `` may only reach what this agent started; `` `message ``
/// reaches any live agent but this one.
#[test]
fn cancel_scopes_to_descendants_and_message_does_not() {
    let (services, fleet, _root_inbox) =
        services_with(3, crate::agent::fleet::Launch::for_test(), |_| {});
    let desk_root = ExarchDesk { services };
    // root -> mid -> grandchild, and root -> sibling (mid's sibling).
    let under = |name: &str, parent: &Arc<Agent>| {
        let mut spec = crate::agent::testkit::TestAgentSpec::new(name);
        spec.parent = Some(parent.clone());
        crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh child of a live parent")
    };
    let root = desk_root.services.agent.clone();
    let mid = under("mid", &root);
    let _sibling = under("sibling", &root);
    let _grandchild = under("grandchild", &mid);

    let mut desk1 = desk_root;
    desk1.services.agent = mid;

    for who in ["sibling", "parent", "grandchild"] {
        assert!(
            desk1.ask(message_req(who, "hi")).is_ok(),
            "a message must reach {who}, whichever way across the tree it runs"
        );
    }

    let err = desk1
        .ask(message_req("mid", "hi"))
        .expect_err("a message to oneself must be refused");
    assert_eq!(
        err.message,
        "agent 'mid' is you; `exarch-agents `message` reaches another agent: to wake yourself, arm a `exarch-schedules` fire"
    );

    let cancel_err = desk1
        .ask(Request::Agents(Agents::Cancel("parent".into())))
        .expect_err("cancelling an ancestor must be refused");
    assert_eq!(
        cancel_err.message,
        "agent 'parent' is not an agent you started; `exarch-agents `cancel` may only reach a descendant of yours"
    );

    assert!(
        desk1
            .ask(Request::Agents(Agents::Cancel("grandchild".into())))
            .is_ok(),
        "cancelling a proper descendant must succeed"
    );
}

/// The read-after-write law once more, through a real spawn: a surfaced
/// value must render before an enquiry raised after it in the same run.
#[test]
fn surface_then_spawn_observes_the_surface_first() {
    let session = crate::agent::Avatar::for_test("system").unwrap();
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, session.agent.id);
    let _ = session.ral(r#"exarch-pins `clear "test-marker"; exarch-agents `start [prompt: #'go'#, name: 't', type: `amnemon, grant: `confined, search: true, provider: `inherit, model: `inherit]"#,
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

/// Refused before anything is armed, naming the flag that grants — and
/// naming the tag the model typed, not a wire word it never saw.
#[test]
fn every_schedule_tag_is_refused_in_the_models_own_vocabulary() {
    for (request, verb) in [
        (add_req("1s", "nightly", "wake"), "exarch-schedules `add"),
        (
            Request::Schedules(Schedules::List),
            "exarch-schedules `list",
        ),
        (remove_req("sched-0"), "exarch-schedules `remove"),
    ] {
        let err = desk()
            .ask(request)
            .expect_err("every schedule tag is refused without the grant");
        assert!(
            err.message.starts_with(&format!("`{verb}` refused:")),
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
        .handle(&family_req(
            "schedules",
            "add",
            Some(FOValue::Map {
                entries: vec![
                    ("trigger".to_string(), tag("after", Some(text("1s")))),
                    ("prompt".to_string(), text("wake")),
                ],
            }),
        ))
        .expect_err("a schedule with no label must be refused");
    assert!(
        err.message.contains("no `label field"),
        "must name the missing field, got: {}",
        err.message
    );
    assert!(
        desk.services.agent.schedules.list().is_empty(),
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
        desk.ask(add_req("2h", "nightly", "wake"))
            .expect("a valid `add must succeed"),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(str_field(&rows[0], "label"), Some("nightly"));
    assert_eq!(str_field(&rows[0], "trigger"), Some("after 2h"));
    assert!(rows[0].field("id").is_none(), "the table carries no id");

    let listed = table(
        desk.ask(Request::Schedules(Schedules::List))
            .expect("`list must succeed"),
    );
    assert_eq!(listed.len(), 1, "`list sees what `add armed");

    let after_removal = table(
        desk.ask(remove_req("nightly"))
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
    desk.ask(add_req("1s", "nightly", "wake"))
        .expect("the first schedule must succeed");
    let err = desk
        .ask(add_req("1s", "nightly", "wake"))
        .expect_err("a duplicate label must be refused");
    assert!(err.message.contains("nightly"), "got: {}", err.message);
    assert_eq!(
        desk.services.agent.schedules.list().len(),
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
    desk.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    desk.services.emit = Emitter::new(tx, AgentId::new(0));
    desk.ask(add_req("1s", "nightly", "wake"))
        .expect("the first schedule must succeed");
    desk.ask(add_req("1s", "nightly", "wake"))
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
    let (services, _fleet, _inbox) =
        services_with(3, crate::agent::fleet::Launch::for_test(), |spec| {
            spec.returns = false;
        });
    let mut d = ExarchDesk { services };
    d.services.emit = emit;
    let err = d
        .ask(reply_req(FOValue::Int { value: 1 }))
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
    d.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    d.services.emit = Emitter::new(tx, AgentId::new(0));

    for text in ["first", "second"] {
        d.ask(reply_req(FOValue::String { value: text.into() }))
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
    desk.ask(add_req("2h", "nightly", "wake"))
        .expect("a valid schedule must succeed");
    desk.ask(remove_req("nightly"))
        .expect("unscheduling the label just armed must remove it");
    desk.ask(reply_req(FOValue::String {
        value: "done".into(),
    }))
    .expect("a returning agent's reply must succeed");

    assert_eq!(
        desk.services.acts.audit().as_deref(),
        Some(
            "audit: this call had already armed the wakeup 'nightly'; removed the wakeup \
             'nightly'; staged your reply; that work stands; do not repeat it.\n"
        )
    );
}

/// A refused act changed nothing, so the fragment stays silent: an entry
/// here would tell the model to leave standing work it never did.
#[test]
fn a_refused_act_leaves_the_fragment_empty() {
    let desk = desk();
    desk.ask(message_req("nobody", "hi"))
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
    desk.services
        .log
        .borrow_mut()
        .record_emitter()
        .attach(Box::new(FleetSink {
            id: AgentId::new(0),
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        }));
    desk.services.emit = Emitter::new(tx, AgentId::new(0));

    desk.ask(add_req("1s", "nightly", "wake"))
        .expect("a valid schedule must land");
    desk.ask(message_req("nobody", "hi"))
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

/// Attend `child` to completion on a detached thread, in `spawn_async`'s own
/// worker-epilogue order: `settle` delivers only a non-reply outcome — a
/// `` `reply ``'s notice already rode `attend`'s own deposit — before
/// retiring it.
fn attend_and_deliver(mut child: Avatar) -> std::thread::JoinHandle<()> {
    let id = child.agent.id;
    std::thread::spawn(move || {
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, id);
        let outcome = child.attend(&emit);
        child.settle(outcome);
    })
}

/// A live, never-attended child of `parent`: just enough for
/// [`Agent::has_busy_children`] to read true, so `parent` parks
/// [`crate::bus::ParkMode::HeldByChildren`] instead of quiescing.  The
/// caller must hold what comes back — that is what keeps it live.
fn keepalive(fleet: &Arc<Fleet>, parent: &Arc<Agent>) -> Arc<Agent> {
    let mut spec = crate::agent::testkit::TestAgentSpec::new(&format!("keepalive-{}", parent.id));
    spec.parent = Some(parent.clone());
    crate::agent::testkit::test_agent(fleet, spec).expect("a fresh child of a live parent")
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

/// The lifecycle turns on a steer's delivery alone and knows nothing of TUI
/// focus. The keepalive grandchild stops the child quiescing before it is
/// engaged, so both steers land instead of racing its loop.
#[test]
fn engaged_child_answers_a_second_steer_with_no_focus_involved() {
    let parent = Avatar::for_test("system").unwrap();
    let child = parent.fork_named("helper").expect("fork child");
    child.agent.provider.swap(Arc::new(Provider::scripted(
        "test-model",
        Script::new()
            .then(Reply::text("first response, no reply yet"))
            .then(Reply::tool_calls(vec![ral_call(
                "r1",
                "exarch-agents `reply 'second response arrived'",
            )])),
    )));
    child.seed("say hi".into());
    let log_dir = child.log_dir();
    let child_agent = child.agent.clone();
    let _keepalive = keepalive(&parent.fleet, &child_agent);
    let handle = attend_and_deliver(child);

    child_agent.mailbox.steer("first message".into());
    assert!(child_agent.engaged(), "steer renews the exchange clock");
    assert!(
        eventually_logged(
            &log_dir.join("record.jsonl"),
            "first response, no reply yet",
            Duration::from_secs(5),
        ),
        "the child must answer the first steer before the second is sent"
    );

    child_agent.mailbox.steer("second message".into());
    match wait_for_settle(&parent.inbox()) {
        crate::bus::Next::Item(crate::bus::Item::Agent(result)) => {
            assert!(
                matches!(result.outcome, crate::bus::AgentOutcome::Replied),
                "the second steer's reply must notify the parent, got: {:?}",
                result.outcome
            );
        }
        other => panic!("expected an Agent result item, got {other:?}"),
    }
    assert_eq!(
        parent
            .agent
            .descendant(&child_agent)
            .and_then(|c| c.reply()),
        Some(FOValue::String {
            value: "second response arrived".into()
        }),
        "the reply the second steer produced must be the one deposited"
    );
    // A replied child parks for a follow-up; only a terminate ends it.
    child_agent.cancel_tree(ral_core::process::CancelCause::Cancelled);
    handle.join().expect("worker thread must not panic");
}

/// The reaped child delivers exactly one
/// [`crate::bus::AgentOutcome::Cancelled`] to the parent inbox.
#[test]
fn ms_lease_child_never_renewed_is_cancelled() {
    // The ttl must expire well inside the scripted round-trips, so the
    // lease and not the script's end finishes the exchange.
    let ttl = Duration::from_millis(25);
    let parent = Avatar::for_test_with(crate::agent::TestTrunk {
        lease: ttl,
        ..crate::agent::TestTrunk::new("system")
    })
    .unwrap();
    let child = parent.fork_named("child-a").expect("fork child a");
    let mut long_script = Script::new();
    for i in 0..2_000u32 {
        long_script = long_script.then(Reply::tool_calls(vec![ral_call(&i.to_string(), "1")]));
    }
    child
        .agent
        .provider
        .swap(Arc::new(Provider::scripted("test-model", long_script)));
    child.seed("go".into());
    let handle = attend_and_deliver(child);

    match wait_for_settle(&parent.inbox()) {
        crate::bus::Next::Item(crate::bus::Item::Agent(result)) => {
            assert!(
                matches!(result.outcome, crate::bus::AgentOutcome::Cancelled),
                "a never-renewed lease reaps mid-exchange with Cancelled, got {:?}",
                result.outcome
            );
        }
        other => panic!("expected an Agent result item, got {other:?}"),
    }
    handle.join().expect("worker thread must not panic");
    assert_eq!(
        crate::agent::roster::summary(&parent.agent).live,
        0,
        "the reaped child settles, and the walk that looked for it pruned it"
    );
}

#[test]
fn ms_lease_child_renewed_at_half_the_ttl_survives_the_bound() {
    // Generous, because the renewal is paced by `thread::sleep` on the test
    // thread, where jitter stretches a short sleep past nominal.
    let ttl = Duration::from_secs(1);
    let parent = Avatar::for_test_with(crate::agent::TestTrunk {
        lease: ttl,
        ..crate::agent::TestTrunk::new("system")
    })
    .unwrap();
    let fleet = parent.fleet.clone();
    // Parked by a keepalive grandchild and given no script to race, so the
    // renewal alone must be what defers its reap.
    let child = parent.fork_named("child-b").expect("fork child b");
    let agent = child.agent.clone();
    let keepalive = keepalive(&fleet, &agent);
    let handle = attend_and_deliver(child);

    std::thread::sleep(ttl / 2);
    // A bare stamp, not a steer: the script is empty, so an actual
    // delivery would give the attend loop work it cannot answer.
    agent.mailbox.stamp_exchange();
    keepalive.mailbox.stamp_exchange();

    std::thread::sleep(ttl / 2 + Duration::from_millis(150));
    assert!(
        !agent.token.is_cancelled(),
        "renewed at half the ttl, still alive past the original bound"
    );

    // Wind the child down rather than leave its thread parked forever: per
    // `ParkMode`, a terminate-cause cancel ends an unengaged park at once.
    agent.cancel_tree(ral_core::process::CancelCause::Cancelled);
    let _ = wait_for_settle(&parent.inbox());
    handle.join().expect("worker thread must not panic");
}
