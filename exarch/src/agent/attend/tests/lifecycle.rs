//! The engaged child's lifecycle: parked on its deposit, steered, and reaped
//! by the fleet's idle lease.

use super::*;
use crate::agent::Agent;
use crate::agent::fleet::Fleet;
use crate::agent::testkit::wait_for_settle;
use crate::provider::Provider;
use std::sync::Arc;

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
    spec.role = crate::agent::Role::child(parent);
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
    let log_dir = child.log_dir().path().to_path_buf();
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

#[test]
fn a_child_working_past_the_reply_lease_is_not_reaped() {
    let parent = Avatar::for_test_with(crate::agent::TestTrunk {
        lease: Duration::from_millis(25),
        installers: crate::agent::testkit::dressed(|runtime| {
            runtime.install_builtins(crate::agent::testkit::WORKER_REGISTRY_TEST_BUILTINS);
        }),
        ..crate::agent::TestTrunk::new("system")
    })
    .unwrap();
    let child = crate::agent::testkit::started_child(&parent, "child-a", "`inherit");
    child.agent.provider.swap(Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "block",
            "test-clear-block-forever",
        )])),
    )));
    child.seed("go".into());
    let child_agent = child.agent.clone();
    let handle = attend_and_deliver(child);
    std::thread::sleep(Duration::from_millis(150));
    let reaped = child_agent.token.terminated();
    child_agent.cancel_tree(ral_core::process::CancelCause::Cancelled);
    handle.join().expect("worker thread must not panic");
    assert!(!reaped, "the reply lease must not limit working time");
    assert!(matches!(
        wait_for_settle(&parent.inbox()),
        crate::bus::Next::Item(crate::bus::Item::Agent(crate::bus::AgentResult {
            outcome: crate::bus::AgentOutcome::Cancelled,
            ..
        }))
    ));
}

#[test]
fn child_is_reaped_after_its_reply_not_before_its_seed() {
    let parent = Avatar::for_test_with(crate::agent::TestTrunk {
        lease: Duration::from_millis(50),
        ..crate::agent::TestTrunk::new("system")
    })
    .unwrap();
    let child = parent.fork_named("child-a").expect("fork child");
    child.agent.provider.swap(Arc::new(Provider::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![ral_call(
            "reply",
            "exarch-agents `reply 42",
        )])),
    )));
    child.seed("go".into());
    let child_agent = child.agent.clone();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !child_agent.token.terminated(),
        "birth does not arm the reply lease"
    );
    let handle = attend_and_deliver(child);
    match wait_for_settle(&parent.inbox()) {
        crate::bus::Next::Item(crate::bus::Item::Agent(result)) => {
            assert!(matches!(result.outcome, crate::bus::AgentOutcome::Replied));
        }
        other => panic!("expected the child's reply, got {other:?}"),
    }
    assert_eq!(child_agent.reply(), Some(FOValue::Int { value: 42 }));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !child_agent.token.terminated() {
        assert!(
            std::time::Instant::now() < deadline,
            "reply lease must expire"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.join().expect("worker thread must not panic");
    assert!(
        parent.inbox().next_item().is_none(),
        "expiry must not duplicate the reply notice"
    );
}
