use super::*;
use crate::agent::testkit::{
    WORKER_REGISTRY_TEST_BUILTINS, dressed_trunk, scheme_test_clear_block_forever, workers,
};
use crate::bus::Emitter;
use crate::card::testkit::card_value;
use crate::record::{ChildStatus, Display, Record};
use ral_core::carrier::Severed;
use ral_core::process::DurableRoot;
use ral_core::shell::runtime::{
    BuiltinBody, BuiltinEntry, Done, DoneEvent, Mooring, Settled, WorkerId,
};
use ral_core::{Shell, Value};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ARRIVAL_STOP_ENTERED: AtomicBool = AtomicBool::new(false);
static ARRIVAL_STOP_RELEASE: AtomicBool = AtomicBool::new(false);

struct ReleaseGuard(&'static AtomicBool);
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn builtin_arrival_hold_until_released(
    _args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    ARRIVAL_STOP_ENTERED.store(true, Ordering::Release);
    while !ARRIVAL_STOP_RELEASE.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(2));
    }
    shell.poll()?;
    Ok(Value::Unit)
}

static ARRIVAL_TEST_BUILTINS: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed("test-arrival-hold"),
    scheme_test_clear_block_forever,
    "test-only: hold until ARRIVAL_STOP_RELEASE, then poll.",
    BuiltinBody::Static(builtin_arrival_hold_until_released),
)];

fn attach_sink(session: &Agent) -> crate::bus::BusReceiver {
    let (tx, rx) = crate::bus::channel();
    session.recorder().attach(Box::new(crate::bus::FleetSink {
        id: session.id(),
        tx: tx.downgrade(),
        meter: crate::bus::UsageMeter::default(),
    }));
    rx
}

fn display_facts(rx: &crate::bus::BusReceiver) -> Vec<Display> {
    crate::bus::drain_records(rx)
        .into_iter()
        .filter_map(|record| match record {
            Record::Display(display) => Some(display),
            Record::Transcript(_) | Record::Control(_) | Record::Forensic(_) => None,
        })
        .collect()
}

fn done_surface(id: u64, title: &str) -> ral_core::data::FOValue {
    DoneEvent {
        id: WorkerId(id),
        title: title.into(),
        elapsed_ms: 25,
        outcome: Done::Ok,
    }
    .to_surface()
}

/// An arrival records only what no protocol record speaks for: a subagent's
/// breadcrumb commits `Display::SubagentDone`, and a prompt commits nothing,
/// since its `Transcript` record is the one fact of it.
#[test]
fn arrival_records_display_facts() {
    let mut session = Agent::for_test("system").unwrap();
    let rx = attach_sink(&session);
    let helper = session.fork_named("helper").unwrap();
    assert!(
        session
            .admit(Input::Child {
                id: helper.id(),
                name: "helper".into(),
                status: ChildStatus::Failed("boom".into()),
                elapsed: Duration::from_millis(1500),
            })
            .is_none()
    );
    assert!(display_facts(&rx).is_empty());
    session.body.nursery.adopt(
        helper.handle(),
        crate::agent::nursery::Joined::new(std::thread::spawn(|| {
            crate::agent::nursery::Teardown { failures: vec![] }
        })),
    );

    assert!(session.admit(Input::Human("hello".into())).is_some());
    assert!(
        session
            .admit(Input::Child {
                id: helper.id(),
                name: "helper".into(),
                status: ChildStatus::Failed("boom".into()),
                elapsed: Duration::from_millis(1500),
            })
            .is_some()
    );

    let mut facts = display_facts(&rx).into_iter();
    let Some(Display::SubagentDone {
        name,
        outcome,
        elapsed_ms,
    }) = facts.next()
    else {
        panic!("the subagent's breadcrumb is the first display record");
    };
    assert_eq!(name, "helper");
    assert_eq!(
        outcome,
        crate::record::Settlement::Failed {
            reason: "boom".into()
        }
    );
    assert_eq!(elapsed_ms, 1500);
    assert!(facts.next().is_none(), "the prompt commits no display fact");
}

/// A surface batch without a `DoneEvent` replays its cards and always enters
/// model context without probing for a worker row.
#[test]
fn surface_batch_without_done_event_replays_cards_and_wakes_model() {
    let session = Agent::for_test("system").unwrap();
    let rx = attach_sink(&session);

    let item = Input::Surface {
        generation: session.generation,
        values: vec![card_value(vec![])],
    };
    assert!(session.admit(item).is_some());

    let facts = display_facts(&rx);
    assert!(
        matches!(facts.as_slice(), [Display::Card { .. }]),
        "the card must be replayed once, got {facts:?}"
    );
}

/// When a worker's result was already claimed by `await` (or its row is
/// absent), `admit` suppresses the redundant model wakeup while still
/// replaying historical cards and the done breadcrumb.
#[test]
fn consumed_worker_done_event_suppresses_wakeup_but_replays_cards() {
    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);

    session.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"claimed\" { 7 }\nawait $h",
        10,
        &emit,
    );
    let _ = display_facts(&rx);

    let item = Input::Surface {
        generation: session.generation,
        values: vec![card_value(vec![]), done_surface(1, "claimed")],
    };
    assert!(
        session.admit(item).is_none(),
        "an already-claimed worker must not wake the model a second time"
    );

    let facts = display_facts(&rx);
    assert!(
        facts.iter().any(|d| matches!(d, Display::Card { .. })),
        "cards in the batch must still replay when model wakeup is suppressed: {facts:?}"
    );
    assert!(
        facts
            .iter()
            .any(|d| matches!(d, Display::Done { id: Some(1), .. })),
        "the historical done line still replays to the scrollback: {facts:?}"
    );
}

/// A worker in `Stopping` has surrendered its result to cancellation, so a
/// queued `DoneEvent` for its id suppresses model wakeup while still replaying
/// cards; an unclaimed `Ready` worker (and a still-`Running` worker) delivers.
#[test]
fn stopping_suppresses_wakeup_while_ready_and_running_deliver() {
    ARRIVAL_STOP_ENTERED.store(false, Ordering::Release);
    ARRIVAL_STOP_RELEASE.store(false, Ordering::Release);
    let mut session = dressed_trunk(|runtime| {
        runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        runtime.install_builtins(&ARRIVAL_TEST_BUILTINS);
    });
    // Declared after the session, so the hold ends before its teardown.
    let _release = ReleaseGuard(&ARRIVAL_STOP_RELEASE);
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.couple(&emit);

    session.ral(
        &DurableRoot::default().child(),
        "let h_run = spawn \"running\" { test-clear-block-forever }\n\
         let h_stop = spawn \"stopping\" { test-arrival-hold }\n\
         let h_ready = spawn \"ready\" { 42 }",
        10,
        &emit,
    );
    while !ARRIVAL_STOP_ENTERED.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(2));
    }
    session.ral(&DurableRoot::default().child(), "cancel $h_stop", 10, &emit);

    let deadline = Instant::now() + Duration::from_secs(10);
    let (run_id, stop_id, ready_id) = loop {
        let rows = session.engine().read(|t| t.workers()).unwrap();
        let run = rows
            .iter()
            .find(|r| r.title == "running" && matches!(r.state, WorkerState::Running))
            .map(|r| r.id);
        let stop = rows
            .iter()
            .find(|r| r.title == "stopping" && matches!(r.state, WorkerState::Stopping))
            .map(|r| r.id);
        let ready = rows
            .iter()
            .find(|r| r.title == "ready" && matches!(r.state, WorkerState::Ready(_)))
            .map(|r| r.id);
        if let (Some(run_id), Some(stop_id), Some(ready_id)) = (run, stop, ready) {
            break (run_id, stop_id, ready_id);
        }
        assert!(
            Instant::now() < deadline,
            "workers did not reach expected states: {rows:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let _ = display_facts(&rx);

    let stopping_item = Input::Surface {
        generation: session.generation,
        values: vec![card_value(vec![]), done_surface(stop_id, "stopping")],
    };
    assert!(
        session.admit(stopping_item).is_none(),
        "a stopping worker's result is already claimed by cancel"
    );
    let stopping_facts = display_facts(&rx);
    assert!(
        stopping_facts
            .iter()
            .any(|d| matches!(d, Display::Card { .. })),
        "cards still replay when stopping suppresses model wakeup"
    );

    let ready_item = Input::Surface {
        generation: session.generation,
        values: vec![done_surface(ready_id, "ready")],
    };
    assert!(
        session.admit(ready_item).is_some(),
        "an unclaimed ready worker must wake the model"
    );

    let running_item = Input::Surface {
        generation: session.generation,
        values: vec![done_surface(run_id, "running")],
    };
    assert!(
        session.admit(running_item).is_some(),
        "a running row must not panic and delivers the conditional notice"
    );

    for entry in workers(&session) {
        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Cancelled);
    }
}

/// A severed `workers()` probe during `admit` still replays the surface cards
/// first, then declines the input and leaves the engine loss observable.
#[test]
fn severed_worker_probe_declines_after_replaying_cards() {
    let session = Agent::for_test("system").unwrap();
    let rx = attach_sink(&session);
    let cause = Severed::Closed("engine gone".into());
    session.engine().transport().sever(cause.clone());

    let item = Input::Surface {
        generation: session.generation,
        values: vec![card_value(vec![]), done_surface(9, "orphaned")],
    };
    assert!(session.admit(item).is_none());
    assert_eq!(session.severance(), Some(cause));

    let facts = display_facts(&rx);
    assert!(
        facts.iter().any(|d| matches!(d, Display::Card { .. })),
        "cards replay before the worker probe fails: {facts:?}"
    );
}

#[test]
fn admission_renders_human_and_peer_inputs_without_changing_debt() {
    use crate::agent::testkit::{forked_child, trunk};
    use crate::bus::AgentMessage;
    for kind in [false, true] {
        let root = trunk(kind);
        let prompt = root.admit(Input::Human("human task".into())).unwrap();
        assert_eq!(prompt.text, "human task");
        assert_eq!(prompt.voice, crate::record::Voice::Human);
        assert!(!root.handle().status().unwrap().owes);
    }
    let parent = trunk(false);
    let helper = forked_child(&parent, "helper", "`inherit");
    let prompt = helper
        .admit(Input::Message(AgentMessage {
            from: parent.id(),
            from_name: "parent".into(),
            text: "follow-up".into(),
        }))
        .unwrap();
    assert_eq!(prompt.voice, crate::record::Voice::Peer);
    assert!(prompt.text.contains("follow-up"));
    assert!(!helper.handle().status().unwrap().owes);
}

#[test]
fn stale_surface_generation_is_quiet_and_wakeups_render() {
    let session = Agent::for_test("system").unwrap();
    let rx = attach_sink(&session);
    let generation = session.generation;
    assert!(
        session
            .admit(Input::Surface {
                generation: generation + 1,
                values: vec![card_value(vec![])],
            })
            .is_none()
    );
    assert!(display_facts(&rx).is_empty(), "stale cards must not replay");

    let prompt = session
        .admit(Input::Wakeup {
            id: crate::bus::ScheduleId::new(2),

            text: "current wakeup".into(),
        })
        .unwrap();
    assert_eq!(prompt.text, "current wakeup");
    assert_eq!(prompt.voice, crate::record::Voice::Wakeup);
    assert!(
        session.handle().status().unwrap().job.is_none(),
        "admission alone opens no job"
    );
}

#[test]
fn gone_joins_and_collects_failures_without_a_model_prompt() {
    use crate::agent::nursery::{Joined, Teardown};
    use crate::agent::testkit::{TestAgentSpec, test_agent};

    let mut session = Agent::for_test("system").unwrap();
    let helper = test_agent(
        &session.fleet,
        TestAgentSpec::child("helper", &session.handle(), session.mailbox.life().child()),
    )
    .unwrap();
    crate::agent::testkit::take_task(&helper, &helper.mailbox);
    let joined = Joined::new(std::thread::spawn(|| Teardown {
        failures: vec!["helper teardown".into()],
    }));
    session.body.nursery.adopt((*helper).clone(), joined);
    helper.end();
    assert!(
        session.rest().pending,
        "taken task debt remains pending after the helper ends"
    );
    session.gone(helper.id());
    assert!(!session.body.nursery.holds(helper.id()));
    assert!(!session.rest().pending);
    assert_eq!(session.body.failures, ["helper teardown"]);
    session.gone(helper.id());
    assert_eq!(
        session.body.failures.len(),
        1,
        "a failure is collected once"
    );
    assert!(
        session
            .admit(Input::Child {
                id: helper.id(),
                name: "helper".into(),
                status: ChildStatus::Stopped,
                elapsed: Duration::ZERO,
            })
            .is_none(),
        "joined helpers' late reports stay quiet"
    );
    session.body.failures.clear();
}
