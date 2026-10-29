//! Reply parks, follow-ups and the fleet's idle lease.

use super::*;
use crate::agent::AgentHandle;
use crate::agent::testkit::wait_for_settle;
use crate::bus::{CANCELLED, ENDED_BEFORE_ANSWERING, ENDED_BEFORE_REPORTING};
use crate::record::{ChildStatus, RECORD, read_records};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn attend_and_deliver(mut child: Agent) -> std::thread::JoinHandle<AgentOutcome> {
    let id = child.id();
    std::thread::spawn(move || {
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, id);
        let outcome = child.attend(&emit);
        assert!(child.is_ready());
        child.settle(outcome.clone(), &emit);
        outcome
    })
}

fn keepalive(parent: &Agent) -> TestAgent {
    test_agent(
        &parent.fleet,
        TestAgentSpec::child(
            &format!("keepalive-{}", parent.id()),
            &parent.handle(),
            parent.inbox.mailbox(),
        ),
    )
    .unwrap()
}

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

#[test]
fn engaged_child_answers_a_second_steer_with_no_focus_involved() {
    let parent = Agent::for_test("system").unwrap();
    let child = parent.fork_named("helper").unwrap();
    set_provider(
        &child,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::text("first response, no reply yet"))
                .then(Reply::tool_calls(vec![ral_call(
                    "r1",
                    "exarch-agents `reply 'second response arrived'",
                )])),
        ),
    );
    child.seed("say hi".into());
    let log_dir = child.log_dir().path().to_path_buf();
    let child_agent = child.handle();
    let _keepalive = keepalive(&child);
    let handle = attend_and_deliver(child);
    child_agent.steer("first message".into()).unwrap();
    assert!(child_agent.status().last_exchange.is_some());
    assert!(eventually_logged(
        &log_dir.join("record.jsonl"),
        "first response, no reply yet",
        Duration::from_secs(5)
    ));
    child_agent.steer("second message".into()).unwrap();
    match wait_for_settle(&parent.inbox) {
        Next::Item(Item::Agent { settled, .. }) => {
            assert!(matches!(settled, Settlement::Replied), "{settled:?}");
        }
        other => panic!("expected an Agent result, got {other:?}"),
    }
    assert_eq!(
        parent
            .handle()
            .descendant(&child_agent)
            .and_then(|c| c.reply()),
        Some(FOValue::String {
            value: "second response arrived".into()
        })
    );
    child_agent.end();
    handle.join().unwrap();
}

#[test]
fn a_child_working_past_the_reply_lease_is_not_reaped() {
    let parent = Agent::for_test_with(TestTrunk {
        lease: Duration::from_millis(25),
        installers: dressed(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS)),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let child = started_child(&parent, "child-a", "`inherit");
    set_provider(
        &child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    child.seed("go".into());
    let child_agent = child.handle();
    let handle = attend_and_deliver(child);
    std::thread::sleep(Duration::from_millis(150));
    let reaped = !child_agent.is_live();
    child_agent.end();
    handle.join().unwrap();
    assert!(!reaped, "the reply lease must not limit working time");
    assert!(matches!(
        wait_for_settle(&parent.inbox),
        Next::Item(Item::Agent {
            settled: Settlement::Failed { reason },
            ..
        }) if reason == CANCELLED
    ));
}

#[test]
fn child_is_reaped_after_its_reply_not_before_its_seed() {
    let parent = Agent::for_test_with(TestTrunk {
        lease: Duration::from_millis(50),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let child = parent.fork_named("child-a").unwrap();
    set_provider(
        &child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "reply",
                "exarch-agents `reply 42",
            )])),
        ),
    );
    child.seed("go".into());
    let child_agent = child.handle();
    std::thread::sleep(Duration::from_millis(150));
    assert!(child_agent.is_live(), "birth does not arm the reply lease");
    let handle = attend_and_deliver(child);
    assert!(matches!(
        wait_for_settle(&parent.inbox),
        Next::Item(Item::Agent {
            settled: Settlement::Replied,
            ..
        })
    ));
    assert_eq!(child_agent.reply(), Some(FOValue::Int { value: 42 }));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child_agent.is_live() {
        assert!(
            std::time::Instant::now() < deadline,
            "reply lease must expire"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.join().unwrap();
    assert!(
        parent.inbox.draw(|_| ParkMode::Quiesce).is_none(),
        "expiry must not duplicate the reply notice"
    );
}

struct CancelOnDrop(AgentHandle);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.end();
    }
}

fn initial_reply() -> Reply {
    Reply::tool_calls(vec![ral_call(
        "initial",
        "exarch-agents `reply 'old payload'",
    )])
}

fn consume_reply(parent: &Agent) {
    let Next::Item(item @ Item::Agent { .. }) = wait_for_settle(&parent.inbox) else {
        panic!("expected reply notice")
    };
    assert!(matches!(
        &item,
        Item::Agent {
            settled: Settlement::Replied,
            ..
        }
    ));
    parent.arrive(&item).unwrap();
}

#[test]
fn a_previous_reply_does_not_suppress_followup_repairs() {
    for repair in [
        Reply::text("prose without a return"),
        Reply::empty(),
        Reply::stopped("early-stop"),
        Reply::truncated_with_tool_calls(vec![]),
        Reply::stalled("partial response"),
    ] {
        let parent = trunk(false);
        let child = parent.fork_named("helper").unwrap();
        set_provider(
            &child,
            scripted("test-model", Script::new().then(initial_reply())),
        );
        child.seed("first task".into());
        let child_agent = child.handle();
        let _cancel = CancelOnDrop(child_agent.clone());
        let handle = attend_and_deliver(child);
        consume_reply(&parent);
        assert_eq!(
            child_agent.reply(),
            Some(FOValue::String {
                value: "old payload".into()
            })
        );
        child_agent.0.provider.swap(scripted(
            "test-model",
            Script::new()
                .then(repair)
                .then(Reply::tool_calls(vec![ral_call(
                    "new",
                    "exarch-agents `reply 'new payload'",
                )])),
        ));
        child_agent
            .message(&parent.handle(), "second task".into())
            .unwrap();
        consume_reply(&parent);
        assert_eq!(
            child_agent.reply(),
            Some(FOValue::String {
                value: "new payload".into()
            })
        );
        assert!(!parent.handle().has_busy_children());
        child_agent.end();
        assert!(matches!(handle.join().unwrap(), AgentOutcome::Replied));
        assert!(parent.inbox.draw(|_| ParkMode::Quiesce).is_none());
    }
}

#[test]
fn nonreply_followups_release_the_embedded_parent() {
    enum Failure {
        NoReply,
        Stopped,
        Api,
        Panic,
    }
    for failure in [
        Failure::NoReply,
        Failure::Stopped,
        Failure::Api,
        Failure::Panic,
    ] {
        let mut parent = trunk(false);
        let child = parent.fork_named("helper").unwrap();
        set_provider(
            &child,
            scripted("test-model", Script::new().then(initial_reply())),
        );
        child.seed("first task".into());
        let child_agent = child.handle();
        let cancel_on_drop = CancelOnDrop(child_agent.clone());
        let handle = attend_and_deliver(child);
        consume_reply(&parent);
        let script = match failure {
            Failure::NoReply => (0..4).fold(Script::new(), |s, _| s.then(Reply::text("no reply"))),
            Failure::Stopped => {
                (0..4).fold(Script::new(), |s, _| s.then(Reply::stopped("early-stop")))
            }
            Failure::Api => Script::new().then(Reply::error(ProviderError::Api {
                status: Some(400),
                model: "test-model".into(),
                message: "terminal failure".into(),
                body: None,
            })),
            Failure::Panic => Script::new().then(Reply::panicking()),
        };
        child_agent.0.provider.swap(scripted("test-model", script));
        child_agent
            .message(&parent.handle(), "follow up".into())
            .unwrap();
        set_provider(
            &parent,
            scripted("test-model", Script::new().then(Reply::text("settled"))),
        );
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, parent.id());
        let timeout = {
            let handle = parent.handle();
            ral_core::process::arm_callback(Duration::from_secs(5), move || handle.end())
        };
        let outcome = parent.attend(&emit);
        drop(timeout);
        assert!(
            parent.handle().is_live(),
            "parent must settle before the deadline: {outcome:?}"
        );
        let terminal = handle.join().unwrap();
        match failure {
            Failure::NoReply => assert!(
                matches!(&terminal, AgentOutcome::Failed(reason) if reason == NO_REPLY_REASON)
            ),
            Failure::Stopped => assert!(
                matches!(&terminal, AgentOutcome::Stopped(reason) if reason == "early-stop")
            ),
            Failure::Api => assert!(
                matches!(&terminal, AgentOutcome::Failed(reason) if reason.contains("terminal failure"))
            ),
            Failure::Panic => assert!(
                matches!(&terminal, AgentOutcome::Failed(reason) if reason.starts_with(WORKER_PANIC_PREFIX))
            ),
        }
        let outcomes: Vec<_> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|r| match r {
                Record::Display(crate::record::Display::SubagentDone { outcome, .. }) => {
                    Some(outcome)
                }
                _ => None,
            })
            .collect();
        assert_eq!(outcomes.len(), 1, "one followup result: {outcomes:?}");
        let crate::record::Settlement::Failed { reason } = &outcomes[0] else {
            panic!("nonreply followup: {outcomes:?}")
        };
        match failure {
            Failure::NoReply => assert_eq!(reason, NO_REPLY_REASON),
            Failure::Stopped => assert_eq!(reason, "early-stop"),
            Failure::Api => assert!(reason.contains("terminal failure")),
            Failure::Panic => assert!(reason.starts_with(WORKER_PANIC_PREFIX)),
        }
        assert!(!parent.handle().has_busy_children());
        drop(cancel_on_drop);
        drop(child_agent);
        assert!(
            parent
                .fleet
                .live()
                .iter()
                .all(|h| parent.handle().descendant(h).is_none())
        );
        assert!(parent.is_ready());
        parent.seed("fresh prompt".into());
        set_provider(
            &parent,
            scripted(
                "test-model",
                Script::new().then(Reply::text("fresh answer")),
            ),
        );
        parent.attend(&emit);
        assert!(parent.is_ready());
    }
}

#[test]
fn a_childs_log_folds_to_its_live_status() {
    let parent = trunk(false);
    let child = parent.fork_named("helper").unwrap();
    let log = child.log_dir().path().join(RECORD);
    let assert_status = |expected, count| {
        let records = read_records(&log).unwrap();
        assert_eq!(ChildStatus::fold(&records), expected);
        assert_eq!(
            parent.inbox.mailbox().child_status("helper"),
            Some(expected)
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::Forensic(Forensic::Status { .. })))
                .count(),
            count
        );
    };
    assert_status(ChildStatus::Working, 0);
    child.inbox.deposit_reply(FOValue::Int { value: 1 });
    assert_status(ChildStatus::Replied(FOValue::Int { value: 1 }), 1);
    child
        .handle()
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    assert_status(ChildStatus::Working, 2);
    child.inbox.deposit_reply(FOValue::Int { value: 2 });
    assert_status(ChildStatus::Working, 2);
    assert!(matches!(
        pop(&child.inbox),
        Some(Next::Item(Item::Message(_)))
    ));
    child.inbox.deposit_reply(FOValue::Int { value: 3 });
    assert_status(ChildStatus::Replied(FOValue::Int { value: 3 }), 3);
    child
        .handle()
        .message(&parent.handle(), "another follow up".into())
        .unwrap();
    assert_status(ChildStatus::Working, 4);
    drop(child);
    assert_status(ChildStatus::Failed(ENDED_BEFORE_REPORTING.into()), 5);
    let records = read_records(&log).unwrap();
    let last_status = records
        .iter()
        .rposition(|record| matches!(record, Record::Forensic(Forensic::Status { .. })))
        .unwrap();
    let ended = records
        .iter()
        .position(|record| matches!(record, Record::Forensic(Forensic::SessionEnded)))
        .unwrap();
    assert!(last_status < ended);
}

#[test]
fn an_owner_unwinding_shows_failed_and_its_log_reads_working() {
    let parent = trunk(false);
    let child = parent.fork_named("helper").unwrap();
    let log = child.log_dir().path().join(RECORD);
    assert!(
        catch_unwind(AssertUnwindSafe(move || {
            let _owner = child;
            panic!("unwind");
        }))
        .is_err()
    );
    assert_eq!(
        parent.inbox.mailbox().child_status("helper"),
        Some(ChildStatus::Failed(ENDED_BEFORE_REPORTING.into()))
    );
    assert_eq!(
        ChildStatus::fold(&read_records(&log).unwrap()),
        ChildStatus::Working
    );
}

#[test]
fn settling_a_working_child_that_cannot_record_announces_and_reports_the_failure() {
    let parent = trunk(false);
    let child = parent.fork_named("helper").unwrap();
    let log = child.log_dir().path().join(RECORD);
    let recorder = child.recorder();
    recorder.fail_after(0);
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    child.settle(AgentOutcome::Failed("gave up".into()), &emit);
    let failure = recorder.check_available().unwrap_err().to_string();
    let announced: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|signal| match signal {
            crate::bus::Signal::Transient(_, Transient::Fault { text }) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(announced, std::slice::from_ref(&failure));
    assert!(matches!(
        wait_for_settle(&parent.inbox),
        Next::Item(Item::Agent { settled: Settlement::Failed { reason }, .. }) if reason == failure
    ));
    assert_eq!(
        parent.inbox.mailbox().child_status("helper"),
        Some(ChildStatus::Failed(failure))
    );
    assert_eq!(
        ChildStatus::fold(&read_records(&log).unwrap()),
        ChildStatus::Working
    );
}

#[test]
fn a_crossing_reply_settles_only_at_retirement() {
    let parent = trunk(false);
    let child = parent.fork_named("helper").unwrap();
    child
        .handle()
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    child.inbox.deposit_reply(FOValue::Unit);
    assert!(parent.inbox.is_empty());
    assert!(parent.handle().has_busy_children());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    child.settle(AgentOutcome::Replied, &emit);
    assert!(
        matches!(wait_for_settle(&parent.inbox), Next::Item(Item::Agent { settled: Settlement::Failed { reason }, .. })
        if reason == ENDED_BEFORE_ANSWERING)
    );
    assert!(!parent.handle().has_busy_children());
}

#[test]
fn cancellation_before_followup_pop_reports_new_work_once() {
    let parent = trunk(false);
    let mut child = parent.fork_named("helper").unwrap();
    set_provider(
        &child,
        scripted("test-model", Script::new().then(initial_reply())),
    );
    child.seed("first task".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, child.id());
    let first = child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    child.take_up(first, &emit).unwrap();
    consume_reply(&parent);
    child
        .handle()
        .message(&parent.handle(), "cancel this accepted task".into())
        .unwrap();
    child.handle().end();
    let outcome = child.attend(&emit);
    assert!(matches!(outcome, AgentOutcome::Cancelled));
    assert!(child.is_ready());
    assert_eq!(
        child
            .inbox
            .source_depths()
            .iter()
            .map(|(_, count)| count)
            .sum::<u64>(),
        1,
        "cancellation won before dequeue"
    );
    child.settle(outcome, &emit);
    let Next::Item(item @ Item::Agent { .. }) = wait_for_settle(&parent.inbox) else {
        panic!("cancel result")
    };
    assert!(matches!(
        &item,
        Item::Agent {
            settled: Settlement::Failed { reason },
            ..
        } if reason == CANCELLED
    ));
    parent.arrive(&item).unwrap();
    assert!(!parent.handle().has_busy_children());
    assert!(parent.inbox.draw(|_| ParkMode::Quiesce).is_none());
}
