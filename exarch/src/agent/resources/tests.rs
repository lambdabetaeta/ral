use super::*;
use crate::agent::testkit::*;
use crate::bus::{Emitter, Post};

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

/// A missing directory reads zero rather than failing the fold.
#[test]
fn dir_size_sums_files_recursively() {
    let root = std::env::temp_dir().join(format!("exarch-dirsize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a"), b"12345").unwrap();
    std::fs::write(root.join("sub/b"), b"123").unwrap();
    assert_eq!(dir_size(&root), 8);
    assert_eq!(dir_size(&root.join("missing")), 0);
    let _ = std::fs::remove_dir_all(&root);
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

    let session = Avatar::for_test("system").unwrap();
    {
        let mut log = session.log.borrow_mut();
        log.append_user("one".into(), None).unwrap();
        log.append_assistant(genai::chat::ChatMessage::assistant("answer"), vec![], None)
            .unwrap();
    }
    let (tx, rx) = crate::bus::channel();
    session.recorder().attach(Box::new(crate::bus::FleetSink {
        id: session.agent.id,
        tx: tx.downgrade(),
        meter: crate::bus::UsageMeter::default(),
    }));
    session.emit_context_survey();

    let fact = crate::bus::drain_records(&rx)
        .into_iter()
        .find_map(|rec| match rec {
            Record::Display(Display::Context { turns, .. }) => Some(turns),
            _ => None,
        })
        .expect("the survey records a Display::Context commit");
    assert_eq!((fact[0].id, fact[0].role), (1, crate::record::Role::User));
    assert_eq!(fact[0].kind, crate::record::TurnKind::Own);
}

/// The agent half surveys what this thread owns: the worker registry's
/// running/settled split with its time-to-reap notes, the binding count,
/// the inbox depths (counted, never drained), and the idle lease's
/// fallback when nothing has forked.
#[test]
fn resource_rows_survey_the_agents_accumulators() {
    let session = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

    session.ral("spawn { test-clear-block-forever }", 30, &emit);
    session.ral("spawn { return 7 }", 30, &emit);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !session
        .seat
        .read(|t| t.workers())
        .expect("an identity seat never severs")
        .iter()
        .any(|w| !w.running)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the instant worker must settle within the budget"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let before = row(
        &session
            .resource_rows()
            .expect("an identity seat never severs"),
        "bindings.count",
    )
    .current;
    session.ral("let probe_marker = 1", 30, &emit);
    let rows = session
        .resource_rows()
        .expect("an identity seat never severs");
    assert_eq!(
        row(&rows, "bindings.count").current,
        before + 1,
        "a `let` adds exactly one binding to the probe figure"
    );

    let running = row(&rows, "workers.running");
    assert_eq!(running.current, 1);
    assert_eq!(
        running.cap,
        Some(shell_eval::LIVE_WORKER_CAP as u64),
        "the admission cap is armed"
    );
    let running_worker = row(&rows, "workers.running[worker]");
    assert_eq!(running_worker.current, 1);
    assert!(
        running_worker
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with("nearest reap in ")),
        "a running worker carries its time-to-reap"
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
    assert!(
        row(&rows, "disk.log_dir").current > 0,
        "a session dir with a written record.jsonl probes nonzero"
    );

    assert_eq!(
        row(&rows, "agents.lease").current,
        AGENT_LEASE_IDLE.as_secs(),
        "no live children: the lease row reports the full idle window"
    );

    // The settled spawn's deferred `Surface` batch may also sit queued —
    // a legitimate arrival, not the probe's doing — so stability
    // compares snapshots rather than pinning the whole vector.
    session.inbox.push(Post::UserSteering("hold".into()));
    session.inbox.push(Post::Nudge {
        prompt: 1,
        text: "go on".into(),
    });
    let depths_before = session.inbox.source_depths();
    let rows = session
        .resource_rows()
        .expect("an identity seat never severs");
    assert_eq!(row(&rows, "inbox[user]").current, 1);
    assert_eq!(row(&rows, "inbox[nudge]").current, 1);
    assert_eq!(
        row(&rows, "inbox[agent]").current,
        0,
        "an idle source still emits its zero row: the row set is stable"
    );
    assert_eq!(
        session.inbox.source_depths(),
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

/// Probing renews nothing: assembling the rows reads a running worker's
/// `last_observed` cell without touching it.
#[test]
fn resource_rows_renew_no_lease() {
    let session = dressed_trunk(|shell| shell.install_builtins(WORKER_REGISTRY_TEST_BUILTINS));
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
    session.ral("spawn { test-clear-block-forever }", 30, &emit);

    let entry = workers(&session)
        .pop()
        .expect("the spawn registered its worker");
    let before = entry.handle.last_observed();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let _ = session
        .resource_rows()
        .expect("an identity seat never severs");
    let after = entry.handle.last_observed();
    assert_eq!(after, before, "the probe must not renew the lease");

    entry
        .handle
        .cancel
        .cancel(ral_core::process::CancelCause::Cancelled);
}

/// The scripted test provider's model has no pricing-catalog entry (the
/// catalog is fetched over the network, never populated in a test
/// process), so its window reads `None` — the fallback arm nearly every
/// other test in this module exercises without knowing it. `log.bytes`
/// alone carries the cap, and no `context.tokens` row appears.
#[test]
fn resource_rows_unknown_window_falls_back_to_capped_log_bytes() {
    let session = Avatar::for_test("system").unwrap();
    let rows = session
        .resource_rows()
        .expect("an identity seat never severs");
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

/// A known window is the pressure `Avatar::evict` actually fires on, so
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
        "the same trigger `Avatar::evict` fires auto-eviction on"
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
