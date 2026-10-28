use super::*;
use crate::agent::build::Build;
use crate::agent::engine::EngineKind;
use crate::agent::fleet::{Fleet, Unborn};
use crate::agent::outcome::Outcome;
use crate::agent::testkit::{TestAgent, TestAgentSpec, scripted, test_agent, trunk};
use crate::agent::{Agent, Role};
use crate::bus::{AgentOutcome, Emitter, Item, Next, ParkMode, Queue, Scoped, Sink};
use crate::headless::SettledConversation;
use crate::provider::scripted::Script;
use crate::record::{AgentLog, Forensic, Protocol, Record, RecordedAccount, RecordedModel, Voice};
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

fn pop(agent: &TestAgent) -> Option<Next> {
    agent.inbox.draw(|_| ParkMode::Quiesce)
}

#[test]
fn a_message_posts_a_marked_note_to_a_descendant() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet, "parent");
    let worker = child(&fleet, &parent, "worker");
    let to = parent.descendant(&worker).unwrap();
    to.message(&parent, "check the lexer".into()).unwrap();
    let Some(Next::Item(Item::Message(message))) = pop(&worker) else {
        panic!("expected a peer message");
    };
    assert_eq!(message.from, parent.id());
    assert_eq!(message.from_name, "parent");
    assert_eq!(message.text, "check the lexer");
    assert!(message.seq > 0);
    assert_eq!(worker.0.mailbox.answers(), message.seq);
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
        let next = pop(&receiver);
        if abandon {
            assert!(next.is_none());
        } else {
            assert!(matches!(next, Some(Next::Item(Item::Message(m))) if m.text == "already said"));
        }
    }
}

#[test]
fn reply_queued_before_followup_does_not_discharge_it() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet, "parent");
    let worker = child(&fleet, &parent, "worker");
    worker
        .report
        .as_ref()
        .unwrap()
        .post(Scoped::Result(crate::bus::AgentResult {
            id: worker.id(),
            name: "worker".into(),
            outcome: AgentOutcome::Replied,
            elapsed: Duration::ZERO,
            answers: 0,
        }));
    worker.message(&parent, "follow up".into()).unwrap();
    assert!(matches!(pop(&parent), Some(Next::Item(Item::Agent(r))) if r.answers == 0));
    assert!(parent.has_busy_children());
    assert!(matches!(pop(&worker), Some(Next::Item(Item::Message(_)))));
    worker
        .report
        .as_ref()
        .unwrap()
        .post(Scoped::Result(crate::bus::AgentResult {
            id: worker.id(),
            name: "worker".into(),
            outcome: AgentOutcome::Replied,
            elapsed: Duration::ZERO,
            answers: worker.0.mailbox.answers(),
        }));
    assert!(matches!(pop(&parent), Some(Next::Item(Item::Agent(_)))));
    assert!(!parent.has_busy_children());
}

#[test]
fn refused_message_rolls_back_its_expectation() {
    let fleet = Fleet::for_test();
    let parent = root(&fleet, "parent");
    let worker = child(&fleet, &parent, "worker");
    worker.end();
    assert_eq!(worker.message(&parent, "too late".into()), Err(Ended));
    assert!(!parent.has_busy_children());
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
    assert!(pop(&owner).is_none());
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
fn abandoned_mailbox_refuses_construction_without_enrolling() {
    let parent = trunk(false);
    let fleet = parent.fleet.clone();
    let mailbox = parent.inbox.mailbox();
    let provider = scripted("test-model", Script::new());
    let engine = fleet.launch.engine().unwrap();
    let EngineKind::Identity(transport) = engine.kind() else {
        unreachable!()
    };
    let weak = Arc::downgrade(&transport);
    drop(transport);
    let claim = fleet.claim("refused").unwrap();
    let log = AgentLog::root(
        &fleet.launch.sessions_root,
        crate::agent::fresh_id(),
        "refused",
        &RecordedModel::of(&provider),
        &RecordedAccount::of(provider.account(), &fleet.launch.bureau.available()),
        "system",
        None,
    )
    .unwrap();
    parent.abandon(Queue::Keep);
    let result = Agent::assemble(Build {
        claim,
        engine,
        log,
        role: Role::Child {
            parent: parent.handle(),
        },
        fuel: 0,
        provider: super::ProviderHandle::new(provider),
        search: false,
        fleet: fleet.clone(),
        report: Some(mailbox),
    });
    assert_eq!(result.err(), Some(Unborn::Abandoned));
    assert!(
        weak.upgrade().is_none(),
        "refused construction drops its engine"
    );
    assert!(fleet.claim("refused").is_ok());
    assert_eq!(fleet.live().len(), 1);
    assert!(!parent.handle().has_busy_children());
    drop(parent);
    assert!(fleet.is_empty(), "refusal acquired no membership");
}

#[test]
fn child_dropped_before_reporting_posts_failed_with_its_answer_sequence() {
    let parent = trunk(false);
    let child = parent.fork_named("worker").unwrap();
    let id = child.id();
    child
        .handle()
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    let next = child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    let Next::Item(Item::Message(message)) = next else {
        panic!("expected message")
    };
    drop(child);
    assert!(parent.handle().has_busy_children());
    let Some(Next::Item(Item::Agent(result))) = parent.inbox.draw(|_| ParkMode::Quiesce) else {
        panic!("drop must report failure");
    };
    assert_eq!(result.id, id);
    assert_eq!(result.name, "worker");
    assert_eq!(result.answers, message.seq);
    assert!(matches!(result.outcome, AgentOutcome::Failed(why) if why == "ended before reporting"));
    assert!(!parent.handle().has_busy_children());
}

#[test]
fn human_drawn_before_stop_is_recorded_once_without_a_provider_turn() {
    let mut owner = trunk(false);
    owner.handle().steer("taken up".into()).unwrap();
    let job = owner.await_job().unwrap();
    let Next::Item(item) = owner.inbox.draw(|_| ParkMode::Quiesce).unwrap() else {
        panic!("expected the human item");
    };
    assert_eq!(owner.handle().stop(job.id()), Some(Vec::new()));
    assert!(owner.arrive(&item).unwrap());
    owner.inbox.open_exchange();
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
    assert!(!job.close_if_running());
    drop(job);
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
    owner.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    owner.inbox.open_exchange();
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
    owner.inbox.decline_opening();
    assert_eq!(owner.inbox.exchange().id(), exchange.id());
    assert!(!exchange.is_cancelled());
    assert!(matches!(owner.inbox.draw(|_| ParkMode::Quiesce),
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
        let fleet = Fleet::for_test();
        let parent = root(&fleet, "parent");
        let worker = child(&fleet, &parent, "worker");
        let since = worker.inbox.deposit_reply(FOValue::Unit);
        let barrier = Arc::new(Barrier::new(2));
        let lapser = {
            let handle = AgentHandle::clone(&worker);
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                handle.lapse(since);
            })
        };
        barrier.wait();
        let sent = worker.message(&parent, "follow up".into());
        lapser.join().unwrap();
        if sent == Ok(()) {
            assert!(worker.is_live());
            assert!(worker.reply_since().is_none());
            assert!(matches!(pop(&worker), Some(Next::Item(Item::Message(_)))));
            assert!(parent.has_busy_children());
        } else {
            assert!(!worker.is_live());
            assert_eq!(
                worker.0.mailbox.scope().cause(),
                Some(CancelCause::TimedOut)
            );
            assert!(!parent.has_busy_children());
            assert!(pop(&worker).is_none());
        }
    }
}
