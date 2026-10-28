use super::*;
use crate::agent::testkit::{TestAgent, TestAgentSpec, test_agent};
use crate::agent::{Birth, Role};
use crate::bus::{Inbox, Stamp};
use std::sync::mpsc;
use std::time::Instant;

fn root(fleet: &Arc<Fleet>) -> TestAgent {
    test_agent(fleet, TestAgentSpec::new("root")).unwrap()
}

fn prepared(
    fleet: &Arc<Fleet>,
    parent: &Arc<Agent>,
    consumer: Stamp,
    name: &str,
) -> (Arc<Agent>, Claim) {
    let spec = TestAgentSpec::new(name);
    let claim = fleet.claim(name).unwrap();
    let child = Agent::new(Birth {
        id: crate::agent::fresh_id(),
        name: name.into(),
        log_dir: parent.log_dir.clone(),
        run_dir: parent.run_dir().clone(),
        started: Instant::now(),
        role: Role::Child {
            parent: parent.clone(),
            consumer,
        },
        fuel: 0,
        provider: parent.provider.clone(),
        search: false,
        reach: spec.reach,
        mailbox: spec.mailbox,
    });
    (child, claim)
}

#[test]
fn construction_pending_enrollment_retains_the_original_delegation_stamp() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet);
    let captured = parent.mailbox.stamp();
    let (ready, waiting) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let worker_fleet = fleet.clone();
    let worker_parent = parent.clone();
    let worker = std::thread::spawn(move || {
        let (child, claim) = prepared(&worker_fleet, &worker_parent, captured, "pending");
        ready.send(()).unwrap();
        resumed.recv().unwrap();
        assert!(matches!(
            worker_fleet.enrol(&child, claim),
            Err(Unborn::SessionDead)
        ));
        assert!(worker_parent.children().is_empty());
    });
    waiting.recv().unwrap();
    parent.stop();
    assert!(parent.mailbox.stamp().reset(&parent.token));
    resume.send(()).unwrap();
    worker.join().unwrap();
    assert!(fleet.resolve("pending").is_none());
    let (fresh, claim) = prepared(&fleet, &parent, parent.mailbox.stamp(), "fresh");
    let _fresh_membership = fleet.enrol(&fresh, claim).unwrap();
    assert!(parent.has_busy_children());
}

#[test]
fn enrollment_before_stop_is_included_and_its_late_result_is_fenced() {
    let fleet = Fleet::for_test();
    let inbox = Inbox::new();
    let mut spec = TestAgentSpec::new("root");
    spec.mailbox = inbox.mailbox();
    let parent = test_agent(&fleet, spec).unwrap();
    let (child, claim) = prepared(&fleet, &parent, parent.mailbox.stamp(), "child");
    let _child_membership = fleet.enrol(&child, claim).unwrap();
    parent.stop();
    assert!(child.token.terminated());
    child.report(crate::bus::AgentOutcome::Cancelled);
    assert!(inbox.next_item().is_none());
    assert!(!parent.has_busy_children());
}

#[test]
fn end_closes_each_node_before_its_direct_child_snapshot() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet);
    let (child, claim) = prepared(&fleet, &parent, parent.mailbox.stamp(), "child");
    let _child_membership = fleet.enrol(&child, claim).unwrap();
    let (grandchild, claim) = prepared(&fleet, &child, child.mailbox.stamp(), "grandchild");
    let _grandchild_membership = fleet.enrol(&grandchild, claim).unwrap();
    parent.end();
    assert!(parent.token.terminated());
    assert!(child.token.terminated());
    assert!(grandchild.token.terminated());
    let (late, claim) = prepared(&fleet, &child, child.mailbox.stamp(), "late");
    assert!(matches!(
        fleet.enrol(&late, claim),
        Err(Unborn::SessionDead)
    ));
}

#[test]
fn pending_grandchild_cannot_escape_an_ancestor_end() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet);
    let (child, claim) = prepared(&fleet, &parent, parent.mailbox.stamp(), "child");
    let _child_membership = fleet.enrol(&child, claim).unwrap();
    let (prepared_tx, prepared_rx) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let worker_fleet = fleet;
    let worker_child = child;
    let worker = std::thread::spawn(move || {
        let (grandchild, claim) = prepared(
            &worker_fleet,
            &worker_child,
            worker_child.mailbox.stamp(),
            "pending",
        );
        prepared_tx.send(()).unwrap();
        resumed.recv().unwrap();
        assert!(matches!(
            worker_fleet.enrol(&grandchild, claim),
            Err(Unborn::SessionDead)
        ));
    });
    prepared_rx.recv().unwrap();
    parent.end();
    resume.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn descendant_cancellation_preserves_queued_human_followups() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet);
    parent.steer("followup".into());
    parent.cancel_descendants(CancelCause::Cancelled);
    assert_eq!(parent.queued(), vec!["followup"]);
}
