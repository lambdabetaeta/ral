use super::*;
use crate::agent::Agent;
use crate::agent::engine::EngineKind;
use crate::agent::fleet::{Fleet, Unborn};
use crate::agent::outcome::Outcome;
use crate::agent::testkit::{
    TestAgent, TestAgentSpec, open_next, pop, pop_outcome, test_agent, trunk,
};
use crate::bus::{AgentOutcome, Emitter, Item, Next, ParkMode, Queue, Sink};
use crate::headless::SettledConversation;
use crate::record::{Forensic, Protocol, Record, Voice};
use std::sync::Barrier;

fn root(fleet: &Arc<Fleet>, name: &str) -> TestAgent {
    test_agent(fleet, TestAgentSpec::new(name)).unwrap()
}

fn child(fleet: &Arc<Fleet>, parent: &TestAgent, name: &str) -> TestAgent {
    test_agent(
        fleet,
        TestAgentSpec::child(name, parent, parent.inbox.mailbox()),
    )
    .unwrap()
}

/// A root "parent" and its child "worker".
fn family() -> (Arc<Fleet>, TestAgent, TestAgent) {
    let fleet = Fleet::for_test();
    let parent = root(&fleet, "parent");
    let worker = child(&fleet, &parent, "worker");
    (fleet, parent, worker)
}

fn heard(worker: &TestAgent) -> bool {
    matches!(pop(&worker.inbox), Some(Next::Item(Item::Message(_))))
}

/// The worker replies; the parent pops `Replied` and owes nothing more.
fn replied(parent: &TestAgent, worker: &TestAgent) -> Lease {
    let lease = worker
        .inbox
        .deposit_reply(FOValue::Unit)
        .expect("no input is queued");
    assert!(matches!(pop_outcome(&parent.inbox), AgentOutcome::Replied));
    assert!(!parent.has_busy_children());
    lease
}

#[test]
fn a_message_posts_a_marked_note_to_a_descendant() {
    let (_fleet, parent, worker) = family();
    let to = parent.descendant(&worker).unwrap();
    to.message(&parent, "check the lexer".into()).unwrap();
    let Some(Next::Item(Item::Message(message))) = pop(&worker.inbox) else {
        panic!("expected a peer message");
    };
    assert_eq!(message.from, parent.id());
    assert_eq!(message.from_name, "parent");
    assert_eq!(message.text, "check the lexer");
    assert!(parent.has_busy_children());
    assert!(worker.status().last_exchange.is_some());
}

#[test]
fn the_scope_climb_refuses_self_and_non_descendants() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet, "parent");
    let branch = child(&fleet, &parent, "branch");
    let sibling = child(&fleet, &parent, "sibling");
    let grandchild = child(&fleet, &branch, "grandchild");
    assert!(branch.descendant(&branch).is_none());
    assert!(branch.descendant(&sibling).is_none());
    assert!(branch.descendant(&parent).is_none());
    assert_eq!(
        parent.descendant(&grandchild).unwrap().id(),
        grandchild.id()
    );
    assert_eq!(grandchild.root().id(), parent.id());
    assert_eq!(parent.root().id(), parent.id());
}

#[test]
fn message_provenance_survives_normal_end_but_not_abandoned_work() {
    for abandon in [false, true] {
        let fleet = Fleet::for_test();
        let parent = root(&fleet, "parent");
        let sender = child(&fleet, &parent, "sender");
        let receiver = root(&fleet, "receiver");
        receiver.message(&sender, "already said".into()).unwrap();
        if abandon {
            parent.inbox.abandon(Queue::Keep);
        } else {
            sender.end();
        }
        let next = pop(&receiver.inbox);
        if abandon {
            assert!(next.is_none());
        } else {
            assert!(matches!(next, Some(Next::Item(Item::Message(m))) if m.text == "already said"));
        }
    }
}

#[test]
fn an_old_reply_cannot_pay_a_newer_follow_up() {
    let (_fleet, parent, worker) = family();
    worker.inbox.deposit_reply(FOValue::Unit);
    worker.message(&parent, "follow up".into()).unwrap();
    assert!(matches!(pop_outcome(&parent.inbox), AgentOutcome::Replied));
    assert!(parent.has_busy_children());
    assert!(heard(&worker));
    worker.inbox.deposit_reply(FOValue::Unit);
    assert!(matches!(pop_outcome(&parent.inbox), AgentOutcome::Replied));
    assert!(!parent.has_busy_children());
}

#[test]
fn a_reply_pays_every_heard_debt_in_one_result() {
    let (_fleet, parent, worker) = family();
    for text in ["one", "two"] {
        worker.message(&parent, text.into()).unwrap();
        assert!(heard(&worker));
    }
    assert!(parent.has_busy_children());
    worker.inbox.deposit_reply(FOValue::Unit);
    assert!(matches!(pop_outcome(&parent.inbox), AgentOutcome::Replied));
    assert!(!parent.has_busy_children());
    assert!(pop(&parent.inbox).is_none());
}

#[test]
fn an_accepted_follow_up_is_paid_when_the_child_retires() {
    let (_fleet, parent, worker) = family();
    replied(&parent, &worker);
    worker.message(&parent, "follow up".into()).unwrap();
    assert!(parent.has_busy_children());
    worker.inbox.retire(AgentOutcome::Replied);
    assert!(matches!(
        pop_outcome(&parent.inbox),
        AgentOutcome::Cancelled
    ));
    assert!(!parent.has_busy_children());
    assert!(pop(&parent.inbox).is_none());
}

#[test]
fn a_refused_message_withdraws_its_debt_silently() {
    let (_fleet, parent, worker) = family();
    replied(&parent, &worker);
    worker.end();
    assert_eq!(worker.message(&parent, "too late".into()), Err(Ended));
    assert!(!parent.has_busy_children());
    assert!(pop(&parent.inbox).is_none());
}

#[test]
fn a_debt_paid_into_abandoned_work_posts_nothing() {
    let (_fleet, parent, worker) = family();
    parent.inbox.abandon(Queue::Keep);
    assert!(!worker.is_live());
    worker.inbox.retire(AgentOutcome::Failed("late".into()));
    assert!(pop(&parent.inbox).is_none());
    assert!(!parent.has_busy_children());
}

#[test]
fn a_childs_clear_pays_a_discarded_follow_up_as_cancelled() {
    let (_fleet, parent, worker) = family();
    replied(&parent, &worker);
    worker.message(&parent, "follow up".into()).unwrap();
    worker.inbox.abandon(Queue::Discard);
    assert!(matches!(
        pop_outcome(&parent.inbox),
        AgentOutcome::Cancelled
    ));
    assert!(!parent.has_busy_children());
}

#[test]
fn a_newer_message_breaks_the_lease() {
    let (_fleet, parent, worker) = family();
    let lease = replied(&parent, &worker);
    worker.message(&parent, "follow up".into()).unwrap();
    assert!(worker.reply_since().is_none());
    worker.lapse(&lease);
    assert!(worker.is_live());
}

#[test]
fn an_unbroken_lease_lapses_to_timed_out() {
    let (_fleet, parent, worker) = family();
    let lease = replied(&parent, &worker);
    worker.lapse(&lease);
    assert!(!worker.is_live());
    assert_eq!(
        worker.0.mailbox.scope().cause(),
        Some(CancelCause::TimedOut)
    );
}

#[test]
fn an_unsolicited_reply_still_notifies_the_parent() {
    let (_fleet, parent, worker) = family();
    replied(&parent, &worker);
    replied(&parent, &worker);
}

#[test]
fn ended_handle_refuses_steering_and_exposes_no_queue() {
    let fleet = Fleet::for_test();
    let owner = root(&fleet, "owner");
    let sender = root(&fleet, "sender");
    owner.steer("queued before end".into()).unwrap();
    owner.end();
    let before = owner.inbox.source_depths();
    assert_eq!(owner.steer("late".into()), Err(Ended));
    assert_eq!(owner.message(&sender, "late message".into()), Err(Ended));
    assert_eq!(owner.queued(), [] as [String; 0]);
    assert_eq!(owner.recall(), [] as [String; 0]);
    assert_eq!(owner.stop(1.into()), None);
    owner.post_read(Read::Resources);
    owner.post_rewrite(Rewrite::Evict);
    owner.interrupt();
    assert_eq!(owner.inbox.source_depths(), before);
    assert!(pop(&owner.inbox).is_none());
}

#[test]
fn actual_owner_drop_ends_the_tree_and_frees_its_name() {
    let owner = trunk(false);
    let child = owner.fork_named("child").unwrap();
    let grandchild = child.fork_named("grandchild").unwrap();
    let fleet = owner.fleet.clone();
    let handle = owner.handle();
    drop(owner);
    assert!(!handle.is_live());
    assert!(!child.handle().is_live());
    assert!(!grandchild.handle().is_live());
    assert_eq!(handle.steer("late".into()), Err(Ended));
    assert!(fleet.claim("main").is_ok());
    assert!(fleet.resolve("child").is_none());
    assert!(!fleet.is_empty());
    drop((child, grandchild));
    assert!(fleet.is_empty());
}

#[test]
fn a_refused_adoption_certifies_its_occupying_engine() {
    let parent = trunk(false);
    let fleet = parent.fleet.clone();
    let mailbox = parent.inbox.mailbox();
    parent.abandon(Queue::Keep);
    let engine = fleet.launch.engine().unwrap();
    let EngineKind::Identity(transport) = engine.kind() else {
        unreachable!()
    };
    let weak = Arc::downgrade(&transport);
    drop(transport);
    let tenancy = fleet.claim("refused").unwrap().occupy(engine);
    let result = Agent::child_of(
        (&parent.handle, mailbox),
        &parent.log.borrow(),
        &fleet,
        tenancy,
        parent.handle.current_provider(),
        crate::record::Memory::Amnemon,
    );
    assert_eq!(result.err(), Some(Unborn::Abandoned.to_string()));
    assert!(
        weak.upgrade().is_none(),
        "refused construction drops its engine"
    );
    assert!(fleet.claim("refused").is_ok());
    assert_eq!(fleet.live().len(), 1);
    assert!(!parent.handle().has_busy_children());
    assert_eq!(
        fleet.wait_settled(parent.tenant(), &crate::bus::Pumping::for_test()),
        Ok(())
    );
    drop(parent);
    assert!(fleet.is_empty(), "refusal left no tenant behind");
}

#[test]
fn an_owner_drop_pays_failed() {
    let parent = trunk(false);
    let child = parent.fork_named("worker").unwrap();
    child
        .handle()
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    let next = pop(&child.inbox).unwrap();
    assert!(matches!(next, Next::Item(Item::Message(_))));
    drop(child);
    assert!(parent.handle().has_busy_children());
    let Some(Next::Item(Item::Agent(result))) = pop(&parent.inbox) else {
        panic!("drop must report failure");
    };
    assert_eq!(result.name, "worker");
    assert!(matches!(result.outcome, AgentOutcome::Failed(why) if why == "ended before reporting"));
    assert!(!parent.handle().has_busy_children());
}

#[test]
fn human_drawn_before_stop_is_recorded_once_without_a_provider_turn() {
    let mut owner = trunk(false);
    owner.handle().steer("taken up".into()).unwrap();
    let job = owner.await_job().unwrap();
    let drawn = owner.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    let Next::Item(item) = &drawn.next else {
        panic!("expected the human item");
    };
    assert_eq!(owner.handle().stop(job.id()), Some(Vec::new()));
    assert!(owner.arrive(item).unwrap());
    drawn.opening.unwrap().open();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, owner.id());
    let outcome = owner
        .deliberate(
            &owner.inbox.exchange(),
            &owner.handle().current_provider(),
            Some((item.text(), item.voice())),
            item.continues(),
            &emit,
        )
        .unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    assert!(matches!(job.close(), crate::bus::Closed::Stopped(_)));
    let records = crate::bus::drain_records(&rx);
    assert_eq!(records.iter().filter(|record| matches!(record,
        Record::Protocol(Protocol::UserPrompt { text, voice: Voice::Human, .. }) if text == "taken up"
    )).count(), 1);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::Cancelled)))
    );
    assert!(owner.is_ready());
}

#[test]
fn claimed_surface_declines_without_closing_the_exchange_or_its_nudge() {
    use crate::agent::testkit::wait_for_settle;

    let owner = trunk(false);
    owner.handle().steer("opening".into()).unwrap();
    open_next(&owner.inbox);
    let exchange = owner.inbox.exchange();
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, owner.id());
    let report = owner.ral(
        &exchange,
        "let h = spawn \"claimed\" { 7 }\nawait $h",
        10,
        &emit,
    );
    assert!(report.text.contains("EXIT: 0"), "{}", report.text);
    let Next::Item(surface @ Item::Surface { .. }) = wait_for_settle(&owner.inbox) else {
        panic!("expected the claimed worker's surface");
    };
    owner.inbox.nudge(1, "continue".into());
    assert!(!owner.arrive(&surface).unwrap());
    assert_eq!(owner.inbox.exchange().id(), exchange.id());
    assert!(!exchange.is_cancelled());
    assert!(matches!(pop(&owner.inbox),
        Some(Next::Item(Item::Nudge { text, .. })) if text == "continue"
    ));
}

struct Discard;
impl Sink for Discard {}

#[test]
fn stop_closes_the_job_stopped_and_text_after_stop_opens_the_next_job() {
    let owner = trunk(false);
    let mut conversation = SettledConversation::new(owner).unwrap();
    let handle = conversation.handle();
    handle.steer("running".into()).unwrap();
    let job = conversation.await_job().unwrap();
    let id = job.id();
    handle.post_read(Read::Resources);
    handle.steer("first".into()).unwrap();
    handle.post_read(Read::Resources);
    handle.steer("second".into()).unwrap();
    assert_eq!(
        handle.stop(id),
        Some(vec!["running".into(), "first".into(), "second".into()])
    );
    assert_eq!(handle.stop(id), None, "Stop is idempotent");
    handle.steer("next job".into()).unwrap();
    let end = conversation.run(job, &mut Discard);
    assert_eq!(end.id, id);
    assert!(end.stopped);
    assert!(end.outcome.is_ok());
    assert_eq!(handle.queued(), ["/resources", "/resources", "next job"]);
    let next = conversation.await_job().unwrap();
    assert!(next.id().get() > id.get());
    let queued = handle.queued();
    assert_eq!(handle.stop(id), None);
    assert_eq!(
        handle.queued(),
        queued,
        "stale Stop cannot touch the next job"
    );
    assert_eq!(handle.stop(next.id()), Some(vec!["next job".into()]));
    assert!(conversation.run(next, &mut Discard).stopped);
}

#[test]
fn dropped_job_closes_without_touching_queued_text() {
    let owner = trunk(false);
    owner.handle().steer("queued".into()).unwrap();
    let job = owner.await_job().unwrap();
    let id = job.id();
    drop(job);
    assert_eq!(owner.handle().stop(id), None);
    assert_eq!(owner.handle().queued(), ["queued"]);
    let next = owner.await_job().unwrap();
    assert!(next.id().get() > id.get());
}

#[test]
fn lease_lapse_is_atomic_with_a_followup_message() {
    for _ in 0..32 {
        let (_fleet, parent, worker) = family();
        let lease = replied(&parent, &worker);
        let barrier = Arc::new(Barrier::new(2));
        let lapser = {
            let handle = AgentHandle::clone(&worker);
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                handle.lapse(&lease);
            })
        };
        barrier.wait();
        let sent = worker.message(&parent, "follow up".into());
        lapser.join().unwrap();
        if sent == Ok(()) {
            assert!(worker.is_live());
            assert!(worker.reply_since().is_none());
            assert!(heard(&worker));
            assert!(parent.has_busy_children());
        } else {
            assert!(!worker.is_live());
            assert_eq!(
                worker.0.mailbox.scope().cause(),
                Some(CancelCause::TimedOut)
            );
            assert!(!parent.has_busy_children());
            assert!(pop(&worker.inbox).is_none());
        }
    }
}
