use super::*;
use crate::agent::testkit::{
    WORKER_REGISTRY_TEST_BUILTINS, dressed_trunk, scheme_test_clear_block_forever, workers,
};
use crate::bus::{AgentOutcome, Emitter};
use crate::card::testkit::card_value;
use crate::record::{Display, Record};
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
            Record::Protocol(_) | Record::Forensic(_) => None,
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
/// since its `Protocol` record is the one fact of it.
#[test]
fn arrival_records_display_facts() {
    use crate::bus::AgentResult;

    let session = Agent::for_test("system").unwrap();
    let rx = attach_sink(&session);

    assert_eq!(session.arrive(&Item::Human("hello".into())), Ok(true));
    assert_eq!(
        session.arrive(&Item::Agent(AgentResult {
            name: "helper".into(),
            outcome: AgentOutcome::Failed("boom".into()),
            elapsed: Duration::from_millis(1500),
        })),
        Ok(true)
    );

    let mut facts = display_facts(&rx).into_iter();
    let Some(Display::SubagentDone {
        name,
        outcome,
        elapsed_ms,
    }) = facts.next()
    else {
        panic!("the subagent's breadcrumb is the first display commit");
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

    let item = Item::Surface {
        id: session.id(),
        values: vec![card_value(vec![])],
    };
    assert_eq!(session.arrive(&item), Ok(true));

    let facts = display_facts(&rx);
    assert!(
        matches!(facts.as_slice(), [Display::Card { .. }]),
        "the card must be replayed once, got {facts:?}"
    );
}

/// When a worker's result was already claimed by `await` (or its row is
/// absent), `arrive` suppresses the redundant model wakeup while still
/// replaying historical cards and the done breadcrumb.
#[test]
fn consumed_worker_done_event_suppresses_wakeup_but_replays_cards() {
    let session = Agent::for_test("system").unwrap();
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

    let item = Item::Surface {
        id: session.id(),
        values: vec![card_value(vec![]), done_surface(1, "claimed")],
    };
    assert_eq!(
        session.arrive(&item),
        Ok(false),
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
    let session = dressed_trunk(|runtime| {
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
        let rows = session.engine.read(|t| t.workers()).unwrap();
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

    let stopping_item = Item::Surface {
        id: session.id(),
        values: vec![card_value(vec![]), done_surface(stop_id, "stopping")],
    };
    assert_eq!(
        session.arrive(&stopping_item),
        Ok(false),
        "a stopping worker's result is already claimed by cancel"
    );
    let stopping_facts = display_facts(&rx);
    assert!(
        stopping_facts
            .iter()
            .any(|d| matches!(d, Display::Card { .. })),
        "cards still replay when stopping suppresses model wakeup"
    );

    let ready_item = Item::Surface {
        id: session.id(),
        values: vec![done_surface(ready_id, "ready")],
    };
    assert_eq!(
        session.arrive(&ready_item),
        Ok(true),
        "an unclaimed ready worker must wake the model"
    );

    let running_item = Item::Surface {
        id: session.id(),
        values: vec![done_surface(run_id, "running")],
    };
    assert_eq!(
        session.arrive(&running_item),
        Ok(true),
        "a running row must not panic and delivers the conditional notice"
    );

    for entry in workers(&session) {
        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Cancelled);
    }
}

/// A severed `workers()` probe during `arrive` still replays the surface cards
/// first and then propagates `Severed`.
#[test]
fn severed_worker_probe_propagates_after_replaying_cards() {
    let session = Agent::for_test("system").unwrap();
    let rx = attach_sink(&session);
    let cause = Severed::Closed("engine gone".into());
    session.engine.transport().sever(cause.clone());

    let item = Item::Surface {
        id: session.id(),
        values: vec![card_value(vec![]), done_surface(9, "orphaned")],
    };
    assert_eq!(session.arrive(&item), Err(cause));

    let facts = display_facts(&rx);
    assert!(
        facts.iter().any(|d| matches!(d, Display::Card { .. })),
        "cards replay before the worker probe fails: {facts:?}"
    );
}
