use super::*;
use crate::agent::testkit::*;
use crate::bus::{Emitter, Input};
use crate::card::FieldVal;
use crate::record::Voice;
use ral_core::process::DurableRoot;

/// The card is a heading plus one matrix, one field per row, the cap
/// rendered into the figure only when armed.
#[test]
fn resources_card_renders_one_field_per_row() {
    let rows = vec![
        ProbeRow::new("workers.running", 3, Some(64), Policy::Reject, None),
        ProbeRow::new("bindings.count", 7, None, Policy::Reap, Some("x".into())),
    ];
    let card = resources_card(&rows);
    assert_eq!(card.marks().len(), 2, "a heading and one matrix");
    let Mark::Fields { rows: fields } = &card.marks()[1] else {
        panic!("the second mark must be the fields matrix");
    };
    assert_eq!(fields.len(), 2);
    let FieldVal::Inline(spans) = &fields[0].value else {
        panic!("a probe field renders inline spans");
    };
    assert_eq!(spans[0].text, "3/64", "an armed cap rides the figure");
    let FieldVal::Inline(spans) = &fields[1].value else {
        panic!("a probe field renders inline spans");
    };
    assert_eq!(spans[0].text, "7", "an unarmed cap adds nothing");
}

fn row<'a>(rows: &'a [ProbeRow], name: &str) -> &'a ProbeRow {
    rows.iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("the fold must emit a `{name}` row"))
}

/// `/context`'s survey records a `Display::Context` commit — what lets a
/// resumed scrollback rebuild the survey card the user saw.
#[test]
fn context_survey_records_a_display_commit() {
    use crate::record::{Display, Record};

    let mut session = Agent::for_test("system").unwrap();
    {
        let log = &mut session.body.log;
        log.append_user("one".into(), None, Voice::Human).unwrap();
        log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
            .unwrap();
    }
    let (tx, rx) = crate::bus::channel();
    session.recorder().attach(Box::new(crate::bus::FleetSink {
        id: session.id(),
        tx: tx.downgrade(),
        meter: crate::bus::UsageMeter::default(),
    }));
    session.emit_context_survey();

    let fact = crate::bus::drain_records(&rx)
        .into_iter()
        .find_map(|rec| match rec {
            Record::Display(Display::Context { rows }) => Some(rows),
            _ => None,
        })
        .expect("the survey records a Display::Context commit");
    assert_eq!(
        (fact[0].span, fact[0].role),
        (crate::record::Span::single(1), crate::record::Role::User)
    );
    assert_eq!(
        fact[0].kind,
        crate::record::ItemKind::Turn(crate::record::TurnKind::Own)
    );
}

static RESOURCES_STOP_ENTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static RESOURCES_STOP_RELEASE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct ResourcesReleaseGuard(&'static std::sync::atomic::AtomicBool);
impl Drop for ResourcesReleaseGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn builtin_resources_hold_until_released(
    _args: &[ral_core::Value],
    _mooring: &ral_core::shell::runtime::Mooring,
    shell: &mut ral_core::Shell,
) -> ral_core::shell::runtime::Settled<ral_core::Value> {
    RESOURCES_STOP_ENTERED.store(true, std::sync::atomic::Ordering::Release);
    while !RESOURCES_STOP_RELEASE.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    shell.poll()?;
    Ok(ral_core::Value::Unit)
}

static RESOURCES_TEST_BUILTINS: [ral_core::shell::runtime::BuiltinEntry; 1] =
    [ral_core::shell::runtime::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("test-resources-hold"),
        scheme_test_clear_block_forever,
        "test-only: hold until RESOURCES_STOP_RELEASE, then poll.",
        ral_core::shell::runtime::BuiltinBody::Static(builtin_resources_hold_until_released),
    )];

/// The agent half surveys what this thread owns: the worker registry's
/// running/settled split (counting `Stopping` against the running cap and
/// `Ready` under settled with its retention note), the binding count,
/// the mailbox depths (counted, never drained), and the idle
/// fallback when nothing has forked.
#[test]
fn resource_rows_survey_the_agents_accumulators() {
    RESOURCES_STOP_ENTERED.store(false, std::sync::atomic::Ordering::Release);
    RESOURCES_STOP_RELEASE.store(false, std::sync::atomic::Ordering::Release);
    let mut session = dressed_trunk(|runtime| {
        runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        runtime.install_builtins(&RESOURCES_TEST_BUILTINS);
    });
    // Declared after the session, so the hold ends before its teardown.
    let _release = ResourcesReleaseGuard(&RESOURCES_STOP_RELEASE);
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());

    session.ral(
        &DurableRoot::default().child(),
        "spawn \"blocker\" { test-clear-block-forever }",
        30,
        &emit,
    );
    session.ral(
        &DurableRoot::default().child(),
        "let h_stop = spawn \"stopping\" { test-resources-hold }",
        30,
        &emit,
    );
    while !RESOURCES_STOP_ENTERED.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    session.ral(&DurableRoot::default().child(), "cancel $h_stop", 30, &emit);
    session.ral(
        &DurableRoot::default().child(),
        "spawn \"seven\" { return 7 }",
        30,
        &emit,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !{
        let ws = session
            .engine()
            .read(|t| t.workers())
            .expect("an identity engine never severs");
        ws.iter().any(|w| matches!(w.state, WorkerState::Running))
            && ws.iter().any(|w| matches!(w.state, WorkerState::Stopping))
            && ws.iter().any(|w| matches!(w.state, WorkerState::Ready(_)))
    } {
        assert!(
            std::time::Instant::now() < deadline,
            "the instant worker must settle within the budget"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let before = row(
        &session
            .resource_rows()
            .expect("an identity engine never severs"),
        "bindings.count",
    )
    .current;
    session.ral(
        &DurableRoot::default().child(),
        "let probe_marker = 1",
        30,
        &emit,
    );
    let rows = session
        .resource_rows()
        .expect("an identity engine never severs");
    assert_eq!(
        row(&rows, "bindings.count").current,
        before + 1,
        "a `let` adds exactly one binding to the probe figure"
    );

    let running = row(&rows, "workers.running");
    assert_eq!(
        running.current, 2,
        "both Running and Stopping count against LIVE_WORKER_CAP"
    );
    assert_eq!(
        running.cap,
        Some(shell_eval::LIVE_WORKER_CAP as u64),
        "the admission cap is armed"
    );
    let settled = row(&rows, "workers.settled");
    assert_eq!(settled.current, 1);
    assert!(
        settled
            .note
            .as_deref()
            .is_some_and(|n| n.contains("ral calls")),
        "a settled entry carries the retention the engine says it has left"
    );

    assert_eq!(
        row(&rows, "log.events").current,
        0,
        "shell-only work has not entered the model view"
    );
    let record_bytes = session
        .body
        .log
        .dir()
        .open(crate::record::RECORD)
        .expect("the session record exists")
        .metadata()
        .expect("the open record has metadata")
        .len();
    assert_eq!(
        row(&rows, "disk.log_dir").current,
        record_bytes,
        "the disk probe counts committed bytes while the record writer stays open"
    );

    assert_eq!(
        row(&rows, "agents.idle").current,
        crate::agent::fleet::HELPER_IDLE.as_secs(),
        "no resting helpers: the idle row reports the full idle window"
    );

    // The settled spawn's deferred `Surface` batch may also sit queued —
    // a legitimate arrival, not the probe's doing — so stability
    // compares snapshots rather than pinning the whole vector.
    session.handle().steer("hold".into()).unwrap();
    session
        .mailbox
        .address()
        .post(Input::Message(crate::bus::AgentMessage {
            from: crate::record::AgentId::new(90),
            from_name: "peer".into(),
            text: "go on".into(),
            from_parent: false,
        }))
        .unwrap();
    let depths_before = session.mailbox.source_depths();
    let rows = session
        .resource_rows()
        .expect("an identity engine never severs");
    assert_eq!(row(&rows, "inbox[user]").current, 1);
    assert_eq!(row(&rows, "inbox[message]").current, 1);
    assert_eq!(
        row(&rows, "inbox[agent]").current,
        0,
        "an idle source still emits its zero row: the row set is stable"
    );
    assert_eq!(
        session.mailbox.source_depths(),
        depths_before,
        "probing drained nothing"
    );

    // End the blocked worker so the test does not leak a live thread.
    for entry in workers(&session) {
        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Cancelled);
    }
}

/// The scripted test provider's model has no pricing-catalog entry (the
/// catalog is fetched over the network, never populated in a test
/// process), so its window reads `None` — the fallback arm nearly every
/// other test in this module exercises without knowing it. `log.bytes`
/// alone carries the cap, and no `context.tokens` row appears.
#[test]
fn resource_rows_unknown_window_falls_back_to_capped_log_bytes() {
    let session = Agent::for_test("system").unwrap();
    let rows = session
        .resource_rows()
        .expect("an identity engine never severs");
    let bytes = row(&rows, "log.bytes");
    assert_eq!(
        bytes.cap,
        Some(EVICT_THRESHOLD as u64),
        "an unknown window falls back to the byte threshold `evict` itself uses"
    );
    assert_eq!(bytes.policy, Policy::Evict);
    assert!(
        !rows.iter().any(|r| r.name == "context.tokens"),
        "no window means no token-pressure row to show"
    );
}

/// A known window is the pressure `Agent::evict` actually fires on, so
/// that is what the fold must show: `context.tokens` capped at
/// `eviction_trigger(w)`, and `log.bytes` demoted to an uncapped
/// fallback gauge rather than faking a second, unenforced ceiling.
#[test]
fn known_window_reports_context_tokens_not_bytes() {
    let rows = pressure_rows(Some(12_345), 4_096, Some(200_000));
    let tokens = row(&rows, "context.tokens");
    assert_eq!(tokens.current, 12_345, "the live input-token numerator");
    assert_eq!(
        tokens.cap,
        Some(eviction_trigger(200_000)),
        "the same trigger `Agent::evict` forces an eviction on"
    );
    assert_eq!(tokens.policy, Policy::Evict);
    assert!(
        tokens.note.as_deref().is_some_and(|n| n.contains("200000")),
        "the note names the window the trigger was computed from"
    );

    let bytes = row(&rows, "log.bytes");
    assert_eq!(bytes.current, 4_096, "the byte gauge still reports");
    assert_eq!(
        bytes.cap, None,
        "log.bytes is no longer where the eviction pressure lives"
    );
    assert_eq!(
        rows.iter().filter(|r| r.name == "log.bytes").count(),
        1,
        "log.bytes still rides along as an uncapped gauge, exactly once"
    );
}

/// A stale measure must read as unknown, never as a number that could sit
/// at or over the trigger: right after a context edit the token count is
/// stale, so `context.tokens` drops out entirely rather than showing a
/// figure the design forbids ("stale reads unknown, never high").
#[test]
fn stale_measure_omits_context_tokens() {
    let rows = pressure_rows(None, 4_096, Some(200_000));
    assert!(
        !rows.iter().any(|r| r.name == "context.tokens"),
        "a stale token measure must not surface as a number"
    );
    let bytes = row(&rows, "log.bytes");
    assert_eq!(bytes.current, 4_096);
    assert_eq!(
        bytes.cap,
        Some(EVICT_THRESHOLD as u64),
        "falls back to the same byte threshold as an unknown window"
    );
}
