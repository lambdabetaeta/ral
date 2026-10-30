//! The desk's own tests.

use super::*;
use crate::agent::loan::Loanable;
use crate::agent::nursery::Nursery;
use crate::agent::roster::summary;
use crate::agent::testkit::{TestAgent, ral_call};
use crate::agent::{Body, Readings};
use crate::bus::FleetSink;
use crate::bus::{Input, Next, Rest};
use crate::bus::{Signal, channel};
use crate::card::Card;
use crate::enquiry::AgentInfo;
use crate::enquiry::{
    Add, Evict, ForkClaim, Grep, Message, Name, Note, Pin, Reading, Selection, Spec, Start, Turns,
};
use crate::provider::{
    Provider,
    scripted::{Reply, Script},
};
use crate::record::AgentLog;
use crate::record::ChildStatus;
use crate::record::EditAuthority;
use crate::record::{AgentId, Memory, Voice};
use crate::record::{Display, Record, Transient};
use crate::schedule::{Trigger, parse_duration};
use ral_core::data::datum::tag;
use ral_core::shell::runtime::NurseryId;
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn fresh_log() -> AgentLog {
    AgentLog::for_test(AgentId::new(0), "test")
        .expect("session log")
        .0
}

fn with_fleet_desk<R>(
    fuel: u32,
    launch: crate::agent::fleet::Launch,
    f: impl FnOnce(ExarchDesk, Arc<Fleet>, TestAgent) -> R,
) -> R {
    let fleet = Fleet::new(launch);
    let mut spec = crate::agent::testkit::TestAgentSpec::new("parent");
    spec.fuel = fuel;
    spec.returns = true;
    let agent = crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh fleet's trunk");
    let mut owner = Loanable::new(Body {
        log: fresh_log(),
        client: Arc::new(Provider::scripted("test-model", Script::new())),
        nursery: Nursery::default(),
        reply: None,
        failures: Vec::new(),
        pins: BTreeMap::default(),
        schedules: crate::schedule::ScheduleRegistry::new(),
        readings: Readings::new(true),
    });
    let (emit, _rx) = crate::bus::dummy_emitter();
    owner.lend(|loan| {
        let desk = ExarchDesk {
            loan: loan.clone(),
            call: Call {
                handle: (*agent).clone(),
                life: agent.mailbox.life(),
                fleet: fleet.clone(),
                kind: EngineKind::Identity(Arc::new(crate::boot::test_transport())),
                emit,
                acts: AtomicRefCell::new(Vec::new()),
            },
        };
        let answer = f(desk, fleet, agent);
        let mut body = loan.get().expect("the test still owns its dispatch");
        let failures = body.nursery.dismiss_all();
        assert!(failures.is_empty(), "helper teardown failed: {failures:?}");
        answer
    })
}

fn with_desk<R>(f: impl FnOnce(ExarchDesk, TestAgent) -> R) -> R {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch::for_test(),
        |desk, _, agent| f(desk, agent),
    )
}

fn append_prompt_and_answer(log: &mut AgentLog, prompt: &str, answer: &str) {
    log.append_user(prompt.to_string(), None, Voice::Human)
        .expect("prompt");
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

/// The model's plainest spawn record: `` `amnemon ``, inheriting its model.
pub(super) fn spec(prompt: &str, name: &str) -> Spec {
    Spec {
        prompt: prompt.to_string(),
        name: Name(name.to_string()),
        memory: Memory::Amnemon,
        model: Selection::Inherit,
    }
}

pub(super) fn start(fork: ForkClaim, spec: Spec) -> Request {
    Request::Agents(Agents::Start(Start { spec, fork }))
}

/// The in-process shape: the fork waits in this host's own nursery.
pub(super) fn start_req(session: NurseryId, prompt: &str, name: &str) -> Request {
    start(ForkClaim::Parked(session), spec(prompt, name))
}

pub(super) fn message_req(to: &str, text: &str) -> Request {
    Request::Agents(Agents::Message(Message {
        to: to.to_string(),
        text: text.to_string(),
    }))
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

fn with_granted_desk<R>(f: impl FnOnce(ExarchDesk, TestAgent) -> R) -> R {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch {
            allow_schedule: true,
            ..crate::agent::fleet::Launch::for_test()
        },
        |desk, _, agent| f(desk, agent),
    )
}

fn with_spawnable_desk<R>(
    fuel: u32,
    f: impl FnOnce(Arc<ExarchDesk>, Arc<Fleet>, TestAgent) -> R,
) -> R {
    with_fleet_desk(
        fuel,
        crate::agent::fleet::Launch::for_test(),
        |desk, fleet, agent| f(Arc::new(desk), fleet, agent),
    )
}

fn next_input(mailbox: &crate::bus::AgentMailbox) -> Input {
    let address = mailbox.address();
    let (done, wait) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if wait.recv_timeout(Duration::from_secs(10)).is_err() {
            address.end(ral_core::process::CancelCause::TimedOut);
        }
    });
    let next = mailbox.next(Rest { pending: true });
    let _ = done.send(());
    watchdog.join().expect("the input watchdog joins");
    match next {
        Next::Take { input, .. } => input,
        _ => panic!("expected a queued input before timeout"),
    }
}

fn wait_at_rest(helper: &crate::agent::AgentHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while helper.status().unwrap().working() {
        assert!(helper.is_live(), "the helper ended before resting");
        assert!(Instant::now() < deadline, "the helper never rested");
        std::thread::sleep(Duration::from_millis(2));
    }
}

type Parked<R> = Box<dyn FnOnce(&ExarchDesk, NurseryId) -> R + Send>;

/// Answers the one fork a real run enquires of, `` `start ``,
/// by running its `f` against the bequest that run parked, while it is still
/// in its pen.
struct Parking<R> {
    desk: Arc<ExarchDesk>,
    f: Mutex<Option<Parked<R>>>,
    out: Mutex<Option<R>>,
}

impl<R: Send> Host for Parking<R> {
    fn surface(&self, _val: &FOValue) {}

    fn enquire(&self, req: FOValue) -> Result<FOValue, EnquiryError> {
        let Ok(Request::Agents(Agents::Start(Start {
            fork: ForkClaim::Parked(id),
            ..
        }))) = Request::decode(&req)
        else {
            panic!("an identity run parks its bequest")
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
    parked_by(
        desk,
        r"exarch-agents `start [prompt: #'go'#, name: 'parked', type: `amnemon, grant: `inherit, model: `inherit]",
        f,
    )
}

/// [`with_parked`], the fork made by running `source`.
fn parked_by<R: Send + 'static>(
    desk: &Arc<ExarchDesk>,
    source: &str,
    f: impl FnOnce(&ExarchDesk, NurseryId) -> R + Send + 'static,
) -> R {
    let EngineKind::Identity(parent) = &desk.call.kind else {
        panic!("an identity desk parks in process")
    };
    let host = Arc::new(Parking {
        desk: desk.clone(),
        f: Mutex::new(Some(Box::new(f))),
        out: Mutex::default(),
    });
    let report = ral_core::carrier::dispatch_to_report(
        &**parent,
        ral_core::protocol::Run::captured(source, ral_core::source::SourceOrigin::named("<test>")),
        host.clone(),
        None,
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
    let EngineKind::Identity(parent) = &desk.call.kind else {
        panic!("an identity desk parks in process")
    };
    parent.adopt_parked(id).is_ok()
}

#[test]
fn unknown_class_answers_the_extension_error() {
    with_desk(|desk, _agent| {
        let err = desk
            .handle(&FOValue::Variant {
                label: "no-such-class".into(),
                payload: None,
            })
            .expect_err("an unrecognised class must not answer Ok");
        assert_eq!(err.message, "unrecognised enquiry class `no-such-class`");
    });
}

/// Nesting the tag under the family must not open a silent hole one level
/// down: a tag extends a family the way a class extends the desk.
#[test]
fn unknown_tag_answers_the_extension_error_too() {
    for class in ["agents", "schedules", "pins", "context", "transcript"] {
        with_desk(|desk, _agent| {
            let err = desk
                .handle(&family_req(class, "no-such-tag", None))
                .expect_err("an unrecognised tag must not answer Ok");
            assert!(
                err.message.starts_with(&format!(
                    "unrecognised tag in `` exarch-{class} `no-such-tag ``: "
                )),
                "got: {}",
                err.message
            );
        });
    }
}

/// Names the expected shape rather than panicking or silently defaulting.
#[test]
fn non_variant_request_errors_didactically() {
    with_desk(|desk, _agent| {
        let err = desk
            .handle(&FOValue::Unit)
            .expect_err("a non-variant request must not answer Ok");
        assert!(
            err.message.contains("must be a variant"),
            "error must name the expected shape, got: {}",
            err.message
        );
    });
}

/// The audit is the model's only account of what survived an unwind, so it
/// must name every committed act, in the order the call committed them.
#[test]
fn the_fragment_keeps_committed_acts_in_the_order_they_landed() {
    with_granted_desk(|desk, _agent| {
        desk.ask(add_req("2h", "nightly", "wake"))
            .expect("a valid schedule must succeed");
        desk.ask(remove_req("nightly"))
            .expect("unscheduling the label just armed must remove it");
        desk.ask(reply_req(FOValue::String {
            value: "done".into(),
        }))
        .expect("a returning agent's reply must succeed");

        assert_eq!(
            desk.call.audit().as_deref(),
            Some(
                "audit: this call had already armed the wakeup 'nightly'; removed the wakeup \
             'nightly'; staged your reply; that work stands; do not repeat it.\n"
            )
        );
    });
}

/// The rail parity a seventh act cannot break by construction: a landed
/// and a refused attempt both draw their row, but only the landed one
/// reaches the fragment.
#[test]
fn rail_draws_every_attempt_the_fragment_holds_only_what_landed() {
    let (tx, rx) = channel();
    with_granted_desk(|mut desk, _agent| {
        desk.loan
            .get()
            .unwrap()
            .log
            .record_emitter()
            .attach(Box::new(FleetSink {
                id: AgentId::new(0),
                tx: tx.downgrade(),
                meter: crate::bus::UsageMeter::default(),
            }));
        desk.call.emit = Emitter::new(tx, AgentId::new(0));

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
            .call
            .audit()
            .expect("the landed schedule owes an audit");
        assert!(
            audit.contains("armed the wakeup") && !audit.contains("message"),
            "the fragment carries the landed schedule alone, got: {audit}"
        );
    });
}

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

fn reply_req(value: FOValue) -> Request {
    Request::Agents(Agents::Reply(value))
}

mod agents;
mod context;
mod pins;
mod schedules;
#[cfg(unix)]
mod wire;

#[test]
fn a_desk_dispatch_returns_its_state_and_retained_enquiries_answer_no_desk() {
    let mut agent = crate::agent::Agent::for_test("system").unwrap();
    let EngineKind::Identity(transport) = agent.engine().kind() else {
        panic!("the test dispatch runs in process")
    };
    let recorder = agent.recorder();
    let call = Call {
        handle: agent.handle(),
        life: agent.mailbox.life(),
        fleet: agent.fleet.clone(),
        kind: agent.engine().kind(),
        emit: crate::bus::dummy_emitter().0,
        acts: AtomicRefCell::new(Vec::new()),
    };
    let (report, host) = agent.body.lend(|loan| {
        let host = Arc::new(RunHost {
            desk: ExarchDesk {
                loan: loan.clone(),
                call,
            },
            apply: SurfaceApplier::new(recorder),
        });
        let report = crate::shell_eval::run_shell(
            &*transport,
            "loan test",
            "exarch-agents `reply 'returned state'",
            5,
            host.clone(),
            None,
        )
        .unwrap();
        (report, host)
    });
    assert!(
        matches!(report, ral_core::protocol::Report::Ran { ending, .. } if ending.status() == 0)
    );
    assert_eq!(agent.body.reply, Some(text("returned state")));
    let error = host
        .enquire(Request::Agents(Agents::List).encode())
        .unwrap_err();
    assert_eq!(error.status, EnquiryError::no_desk().status);
    assert_eq!(error.message, EnquiryError::no_desk().message);
}

#[test]
fn an_unwinding_desk_dispatch_returns_the_mutated_body() {
    let mut agent = crate::agent::Agent::for_test("system").unwrap();
    let call = Call {
        handle: agent.handle(),
        life: agent.mailbox.life(),
        fleet: agent.fleet.clone(),
        kind: agent.engine().kind(),
        emit: crate::bus::dummy_emitter().0,
        acts: AtomicRefCell::new(Vec::new()),
    };
    let mut retained = None;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        agent.body.lend(|loan| {
            let desk = ExarchDesk {
                loan: loan.clone(),
                call,
            };
            desk.ask(reply_req(text("before panic"))).unwrap();
            retained = Some(desk);
            panic!("the dispatch panicked");
        });
    }));
    assert!(panic.is_err());
    assert_eq!(agent.body.reply, Some(text("before panic")));
    let error = retained
        .unwrap()
        .ask(Request::Agents(Agents::List))
        .unwrap_err();
    assert_eq!(error.message, EnquiryError::no_desk().message);
}
