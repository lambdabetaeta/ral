use super::*;
use crate::agent::testkit::{TestAgent, TestAgentSpec, test_agent};
use crate::agent::{AgentHandle, roster};
use crate::bus::{AgentOutcome, Item, Next, ParkMode, Queue};
use crate::enquiry::Spawner;
use ral_core::data::FOValue;
use ral_core::test_helper::eventually;

fn agent(fleet: &Arc<Fleet>, name: &str, parent: Option<&TestAgent>) -> TestAgent {
    let spec = parent.map_or_else(
        || TestAgentSpec::new(name),
        |parent| TestAgentSpec::child(name, parent, parent.inbox.mailbox()),
    );
    test_agent(fleet, spec).expect("a fresh agent under live work")
}

fn deposit(agent: &TestAgent, fleet: &Fleet, value: i64) -> Instant {
    let since = agent.inbox.deposit_reply(FOValue::Int { value });
    fleet.lease_reply(agent, since);
    since
}

#[test]
fn a_claim_counts_construction_until_it_is_dropped() {
    let fleet = Fleet::for_test();
    assert!(fleet.is_empty());
    let claim = fleet.claim("building").unwrap();
    assert!(!fleet.is_empty());
    assert!(fleet.live().is_empty());
    assert!(matches!(fleet.claim("building"), Err(Unborn::NameTaken(_))));
    drop(claim);
    assert!(fleet.is_empty());
    assert!(fleet.claim("building").is_ok());
}

#[test]
fn owner_drop_ends_descendants_and_releases_its_name() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let retained = AgentHandle::clone(&root);
    let report = root.inbox.mailbox();
    drop(root);
    assert!(!retained.is_live());
    assert!(!child.is_live());
    assert!(fleet.resolve("root").is_none());
    assert!(fleet.claim("root").is_ok());
    assert_eq!(
        test_agent(&fleet, TestAgentSpec::child("late", &retained, report)).err(),
        Some(Unborn::SessionDead)
    );
    assert!(!fleet.is_empty());
    drop(child);
    assert!(fleet.is_empty());
}

#[test]
fn a_lease_is_armed_only_after_a_child_replies() {
    let fleet = Fleet::leased_for_test(Duration::from_millis(50));
    let trunk = agent(&fleet, "trunk", None);
    let branch = agent(&fleet, "branch", None);
    let worker = agent(&fleet, "worker", Some(&trunk));
    std::thread::sleep(Duration::from_millis(150));
    assert!(worker.is_live(), "working children have no reply lease");
    assert!(fleet.nearest_reap().is_none());
    deposit(&worker, &fleet, 1);
    deposit(&branch, &fleet, 1);
    assert!(fleet.nearest_reap().is_some());
    assert!(eventually(Duration::from_secs(2), || (!worker.is_live()).then_some(())).is_some());
    assert_eq!(
        worker.0.mailbox.scope().cause(),
        Some(CancelCause::TimedOut)
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(branch.is_live(), "a root arms no reaper deadline");
}

#[test]
fn abandoning_work_cancels_descendants_but_spares_the_owner() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let grandchild = agent(&fleet, "grandchild", Some(&child));
    trunk.inbox.abandon(Queue::Discard);
    assert!(!child.is_live());
    assert!(!grandchild.is_live());
    assert!(trunk.is_live());
    trunk.steer("fresh work".into()).unwrap();
    trunk.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    trunk.inbox.open_exchange();
    assert!(!trunk.inbox.exchange().is_cancelled());
}

#[test]
fn ending_a_subtree_spares_its_parent_and_sibling() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let grandchild = agent(&fleet, "grandchild", Some(&child));
    let sibling = agent(&fleet, "sibling", Some(&root));
    child.end();
    assert!(!child.is_live());
    assert!(!grandchild.is_live());
    assert!(sibling.is_live());
    assert!(root.is_live());
}

#[test]
fn interrupt_unwinds_only_the_exchange() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let grandchild = agent(&fleet, "grandchild", Some(&child));
    child.steer("work".into()).unwrap();
    child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    child.inbox.open_exchange();
    child.interrupt();
    assert_eq!(
        child.inbox.exchange().cause(),
        Some(CancelCause::Interrupted)
    );
    assert!(child.is_live());
    assert!(grandchild.is_live());
}

#[cfg_attr(
    ctor_without_argv,
    ignore = "musl gives bundled tools no argv before main"
)]
#[test]
fn interrupt_never_poisons_the_next_run() {
    use crate::agent::testkit::bare_transport;
    use ral_core::carrier::dispatch_to_report;
    use ral_core::protocol::{Ending, Report, Run};
    use ral_core::source::SourceOrigin;

    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    child.steer("work".into()).unwrap();
    child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    child.inbox.open_exchange();
    let exchange = child.inbox.exchange();
    let transport = Arc::new(bare_transport());
    let running = {
        let transport = transport.clone();
        std::thread::spawn(move || {
            dispatch_to_report(
                &*transport,
                Run::captured("sleep 30", SourceOrigin::named("<test>")),
                Arc::new(()),
                Some(&exchange),
            )
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !running.is_finished() {
        assert!(
            Instant::now() < deadline,
            "interrupt must unwind the dispatch"
        );
        child.interrupt();
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = running.join().unwrap().expect("identity never severs");
    assert!(
        matches!(
            first,
            Report::Ran {
                ending: Ending::Stopped {
                    cause: CancelCause::Interrupted,
                    ..
                },
                ..
            }
        ),
        "{first:?}"
    );
    child.steer("next".into()).unwrap();
    child.inbox.draw(|_| ParkMode::Quiesce).unwrap();
    child.inbox.open_exchange();
    let next = dispatch_to_report(
        &*transport,
        Run::captured("$[1 + 1]", SourceOrigin::named("<test>")),
        Arc::new(()),
        Some(&child.inbox.exchange()),
    )
    .expect("identity never severs");
    assert!(
        matches!(
            next,
            Report::Ran {
                ending: Ending::Settled { .. },
                ..
            }
        ),
        "{next:?}"
    );
}

#[test]
fn ended_agents_leave_resolve_and_the_roster_before_teardown() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let names: Vec<_> = roster::listing(&trunk, &fleet)
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, ["trunk", "child"]);
    child.end();
    assert!(fleet.resolve("child").is_none());
    assert_eq!(roster::listing(&trunk, &fleet).len(), 1);
    assert!(!fleet.is_empty());
    assert_eq!(
        fleet.claim("child").err(),
        Some(Unborn::NameTaken("child".into()))
    );
    drop(child);
    assert!(fleet.claim("child").is_ok());
}

#[test]
fn every_row_names_who_started_it() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let mid = agent(&fleet, "mid", Some(&trunk));
    let _leaf = agent(&fleet, "leaf", Some(&mid));
    let edges: Vec<_> = roster::listing(&trunk, &fleet)
        .into_iter()
        .map(|row| {
            (
                row.name,
                match row.spawner {
                    Spawner::Root => None,
                    Spawner::Agent(up) => Some(up),
                },
            )
        })
        .collect();
    assert_eq!(
        edges,
        vec![
            ("trunk".into(), None),
            ("mid".into(), Some("trunk".into())),
            ("leaf".into(), Some("mid".into())),
        ]
    );
}

#[test]
fn the_summary_counts_company_and_what_is_owed() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let mid = agent(&fleet, "mid", Some(&trunk));
    let _sibling = agent(&fleet, "sibling", Some(&trunk));
    let child = agent(&fleet, "child", Some(&mid));
    let alone = roster::summary(&mid, &fleet);
    assert_eq!(alone.live, 3);
    assert_eq!(alone.replied, 0);
    deposit(&child, &fleet, 1);
    let owed = roster::summary(&mid, &fleet);
    assert_eq!(owed.live, 3, "a reply does not end its owner");
    assert_eq!(owed.replied, 1);
    assert_eq!(
        roster::summary(&trunk, &fleet).replied,
        0,
        "owed only to the spawner"
    );
}

#[test]
fn a_second_root_is_outside_the_first_roots_tree() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let branch = agent(&fleet, "branch", None);
    let names: Vec<_> = roster::listing(&trunk, &fleet)
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, ["trunk"]);
    assert!(trunk.descendant(&branch).is_none());
    assert_eq!(
        fleet
            .roots()
            .iter()
            .map(AgentHandle::id)
            .collect::<Vec<_>>(),
        [trunk.id(), branch.id()]
    );
}

#[test]
fn an_ended_parent_bears_no_more_children() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let parent = agent(&fleet, "parent", Some(&trunk));
    parent.end();
    assert_eq!(
        test_agent(
            &fleet,
            TestAgentSpec::child("late", &parent, parent.inbox.mailbox())
        )
        .err(),
        Some(Unborn::SessionDead)
    );
    assert!(fleet.resolve("late").is_none());
    assert!(!parent.has_busy_children());
    assert!(fleet.claim("late").is_ok());
}

#[test]
fn a_name_borne_by_a_live_agent_refuses_a_second() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let helper = agent(&fleet, "helper", Some(&trunk));
    assert_eq!(
        test_agent(
            &fleet,
            TestAgentSpec::child("helper", &trunk, trunk.inbox.mailbox())
        )
        .err(),
        Some(Unborn::NameTaken("helper".into()))
    );
    assert_eq!(fleet.resolve("helper").unwrap().id(), helper.id());
    assert_eq!(fleet.live().len(), 2);
    assert!(trunk.has_busy_children());
}

#[test]
fn followups_disarm_the_old_reply_and_the_next_reply_gets_a_new_lease() {
    for peer_message in [false, true] {
        let fleet = Fleet::leased_for_test(Duration::from_secs(60));
        let trunk = agent(&fleet, "trunk", None);
        let child = agent(&fleet, "child", Some(&trunk));
        let first = deposit(&child, &fleet, 1);
        if peer_message {
            child.message(&trunk, "follow up".into()).unwrap();
        } else {
            child.steer("follow up".into()).unwrap();
        }
        child.lapse(first);
        assert!(child.is_live());
        assert!(fleet.nearest_reap().is_none());
        child.inbox.disarm_reply_lease();
        assert!(child.reply_since().is_none());
        child.inbox.draw(|_| ParkMode::Quiesce);
        let second = deposit(&child, &fleet, 2);
        assert!(second > first);
        child.lapse(first);
        assert!(child.is_live());
        child.lapse(second);
        assert!(!child.is_live());
    }
}

#[test]
fn resumed_work_disarms_the_reply_even_without_a_message() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let since = deposit(&child, &fleet, 1);
    child.inbox.disarm_reply_lease();
    child.lapse(since);
    assert!(child.is_live());
    assert!(fleet.nearest_reap().is_none());
}

#[test]
fn observing_status_does_not_disarm_the_reply_lease() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let since = deposit(&child, &fleet, 1);
    child.inbox.set_resting(false);
    child.inbox.set_resting(true);
    assert_eq!(child.reply_since(), Some(since));
    assert!(child.status().rest.is_some());
    assert!(fleet.nearest_reap().is_some());
    child.lapse(since);
    assert!(!child.is_live());
}

#[test]
fn settling_before_the_fire_ends_the_chain_silently() {
    let fleet = Fleet::leased_for_test(Duration::from_secs(60));
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let weak = child.0.mailbox.downgrade();
    let life = child.0.mailbox.scope();
    let since = deposit(&child, &fleet, 1);
    let retained = AgentHandle::clone(&child);
    assert!(fleet.nearest_reap().is_some());
    assert_eq!(life.cause(), None);
    drop(child);
    assert!(
        weak.upgrade().is_none(),
        "weak mailbox refuses a dead scope"
    );
    assert!(fleet.resolve("child").is_none());
    assert!(fleet.nearest_reap().is_none());
    assert_eq!(life.cause(), Some(CancelCause::Cancelled));
    retained.lapse(since);
    assert_eq!(life.cause(), Some(CancelCause::Cancelled));
}

#[test]
fn abandoned_work_outranks_the_lease() {
    let ttl = Duration::from_millis(80);
    let fleet = Fleet::leased_for_test(ttl);
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let life = child.0.mailbox.scope();
    deposit(&child, &fleet, 1);
    trunk.inbox.abandon(Queue::Discard);
    assert_eq!(life.cause(), Some(CancelCause::Cancelled));
    drop(child);
    std::thread::sleep(ttl * 4);
    assert_eq!(life.cause(), Some(CancelCause::Cancelled));
}

#[test]
fn steering_stamps_the_exchange_clock_and_resets_idle() {
    let fleet = Fleet::for_test();
    let mut spec = TestAgentSpec::new("root");
    spec.idle = Duration::from_secs(30);
    let root = test_agent(&fleet, spec).unwrap();
    assert!(root.status().last_exchange.is_none());
    assert!(root.idle() >= Duration::from_secs(30));
    root.steer("hi".into()).unwrap();
    assert!(root.status().last_exchange.is_some());
    assert!(root.idle() < Duration::from_secs(1));
    assert!(
        matches!(root.inbox.draw(|_| ParkMode::Quiesce), Some(Next::Item(Item::Human(s))) if s == "hi")
    );
}

#[test]
fn a_settling_childs_result_outruns_the_quiesce_verdict() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    assert!(trunk.has_busy_children());
    drop(child);
    assert!(trunk.has_busy_children(), "ended is not heard");
    let next = trunk.inbox.draw(|state| {
        if state.has_busy_children() {
            ParkMode::HeldByChildren
        } else {
            ParkMode::Quiesce
        }
    });
    assert!(
        matches!(next, Some(Next::Item(Item::Agent(r))) if r.name == "child" && matches!(r.outcome, AgentOutcome::Failed(_)))
    );
    assert!(!trunk.has_busy_children());
    assert!(trunk.inbox.draw(|_| ParkMode::Quiesce).is_none());
}

#[test]
fn fleet_end_ends_every_agent_and_future_roots_are_born_cancelled() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let other = agent(&fleet, "other", None);
    fleet.end(CancelCause::Terminated);
    for handle in [&*root, &*child, &*other] {
        assert!(!handle.is_live());
        assert_eq!(
            handle.0.mailbox.scope().cause(),
            Some(CancelCause::Terminated)
        );
    }
    let late = agent(&fleet, "late", None);
    assert!(!late.is_live());
    assert!(fleet.live().is_empty());
    assert!(fleet.roots().is_empty());
    assert!(fleet.resolve("late").is_none());
    assert!(!fleet.is_empty(), "ending is not teardown");
    drop((root, child, other, late));
    assert!(fleet.is_empty());
}

#[test]
fn wait_empty_counts_child_teardown_after_its_root_retires() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    drop(root);
    assert!(!child.is_live());
    assert!(!fleet.is_empty());
    let (waiting, parked) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let waiter = {
        let fleet = fleet.clone();
        std::thread::spawn(move || {
            fleet.wait_empty_after(|| waiting.send(()).unwrap());
            done.send(()).unwrap();
        })
    };
    parked.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        finished.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(child);
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
    assert!(fleet.is_empty());
}

#[test]
fn wait_settled_waits_for_ended_members_but_not_live_ones() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    root.inbox.abandon(Queue::Recall);
    let (done, finished) = std::sync::mpsc::channel();
    let waiter = {
        let fleet = fleet.clone();
        let handle = root.clone();
        std::thread::spawn(move || {
            fleet.wait_settled(&handle).unwrap();
            done.send(()).unwrap();
        })
    };
    assert!(matches!(
        finished.recv_timeout(Duration::from_millis(20)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    drop(child);
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
    assert!(root.is_live());
    assert!(!fleet.is_empty());
}
