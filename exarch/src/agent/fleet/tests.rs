use super::*;
use crate::agent::testkit::{TestAgent, TestAgentSpec, open_next, pop, test_agent};
use crate::agent::{AgentHandle, roster};
use crate::bus::{Item, Lease, Next, ParkMode, Pumping, Queue};
use crate::enquiry::Spawner;
use crate::record::Settlement;
use ral_core::data::FOValue;
use ral_core::test_helper::eventually;
use std::time::Instant;

fn agent(fleet: &Arc<Fleet>, name: &str, parent: Option<&TestAgent>) -> TestAgent {
    let spec = parent.map_or_else(
        || TestAgentSpec::new(name),
        |parent| TestAgentSpec::child(name, parent, parent.inbox.mailbox()),
    );
    test_agent(fleet, spec).expect("a fresh agent under live work")
}

fn deposit(agent: &TestAgent, fleet: &Fleet, value: i64) -> Lease {
    let lease = agent
        .inbox
        .deposit_reply(FOValue::Int { value })
        .expect("no input is queued");
    fleet.lease_reply(agent, lease.clone());
    lease
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
fn death_is_published_after_the_engine_drops_but_before_the_last_claim_releases() {
    use crate::agent::engine::EngineKind;
    use crate::agent::testkit::bare_transport;
    use crate::record::{Commit, Publish, Transient};
    use ral_core::carrier::IdentityTransport;
    use std::sync::{Weak, mpsc};

    const NAME: &str = "departing";
    const TIMEOUT: Duration = Duration::from_secs(5);

    struct DeathWitness {
        transport: Weak<IdentityTransport>,
        census: Arc<Census>,
        tenant: TenantId,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Publish for DeathWitness {
        fn commit(&self, commit: Commit) {
            panic!("claim release must publish no commit: {commit:?}");
        }

        fn transient(&self, transient: Transient) {
            assert!(matches!(transient, Transient::Died), "{transient:?}");
            assert!(
                self.transport.upgrade().is_none(),
                "Died must follow the actual transport drop, not merely engine.end()"
            );
            {
                let registry = self
                    .census
                    .registry
                    .try_lock()
                    .expect("Died publication must not hold the census registry lock");
                assert!(registry.tenants.contains_key(&self.tenant));
                assert_eq!(registry.names.get(NAME), Some(&self.tenant));
            }
            assert!(!self.census.is_empty());
            assert_eq!(
                self.census.claim(NAME).err(),
                Some(Unborn::NameTaken(NAME.into()))
            );
            self.entered.send(()).unwrap();
            self.release.recv_timeout(TIMEOUT).unwrap();
        }
    }

    let fleet = Fleet::for_test();
    let census = fleet.census();
    let claim = fleet.claim(NAME).unwrap();
    let tenant = claim.id();
    let mut tenancy = claim.occupy(Engine::adopted(bare_transport()));
    let transport = match tenancy.kind() {
        EngineKind::Identity(transport) => Arc::downgrade(&transport),
        EngineKind::Wire { .. } => panic!("a bare engine uses identity transport"),
    };
    assert!(transport.upgrade().is_some());
    let (entered, publishing) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    tenancy.publish_death_on_release(Box::new(DeathWitness {
        transport,
        census: census.clone(),
        tenant,
        entered,
        release: resume,
    }));
    let worker = std::thread::spawn(move || drop(tenancy));

    publishing.recv_timeout(TIMEOUT).unwrap();
    assert!(!fleet.is_empty());
    assert_eq!(
        fleet.claim(NAME).err(),
        Some(Unborn::NameTaken(NAME.into()))
    );
    drop(fleet);
    assert!(
        !census.is_empty(),
        "the final census still counts publication"
    );
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(census.is_empty());
    assert!(census.claim(NAME).is_ok());
}

#[test]
fn rearming_death_publication_replaces_the_sink_and_publishes_once() {
    use crate::agent::testkit::bare_transport;
    use crate::record::{Commit, Publish, Transient};
    use std::sync::mpsc;

    struct DeathCount {
        sink: u8,
        deaths: mpsc::Sender<u8>,
    }

    impl Publish for DeathCount {
        fn commit(&self, commit: Commit) {
            panic!("claim release must publish no commit: {commit:?}");
        }

        fn transient(&self, transient: Transient) {
            assert!(matches!(transient, Transient::Died), "{transient:?}");
            self.deaths.send(self.sink).unwrap();
        }
    }

    let fleet = Fleet::for_test();
    let mut tenancy = fleet
        .claim("rearmed")
        .unwrap()
        .occupy(Engine::adopted(bare_transport()));
    let (deaths, received) = mpsc::channel();
    for sink in [1, 2] {
        tenancy.publish_death_on_release(Box::new(DeathCount {
            sink,
            deaths: deaths.clone(),
        }));
    }
    assert!(matches!(
        received.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    drop(tenancy);
    assert_eq!(received.try_iter().collect::<Vec<_>>(), [2]);
    assert!(fleet.is_empty());
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
    trunk.inbox.abandon(Queue::Keep);
    assert!(!child.is_live());
    assert!(!grandchild.is_live());
    assert!(trunk.is_live());
    trunk.steer("fresh work".into()).unwrap();
    open_next(&trunk.inbox);
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
    open_next(&child.inbox);
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
    open_next(&child.inbox);
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
    open_next(&child.inbox);
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
        child.lapse(&first);
        assert!(child.is_live());
        assert!(fleet.nearest_reap().is_none());
        child.inbox.disarm_reply_lease();
        assert!(child.reply_since().is_none());
        child.inbox.draw(|_| ParkMode::Quiesce);
        let second = deposit(&child, &fleet, 2);
        child.lapse(&first);
        assert!(child.is_live());
        child.lapse(&second);
        assert!(!child.is_live());
    }
}

#[test]
fn resumed_work_disarms_the_reply_even_without_a_message() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let lease = deposit(&child, &fleet, 1);
    child.inbox.disarm_reply_lease();
    child.lapse(&lease);
    assert!(child.is_live());
    assert!(fleet.nearest_reap().is_none());
}

#[test]
fn observing_status_does_not_disarm_the_reply_lease() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let lease = deposit(&child, &fleet, 1);
    child.inbox.set_resting(false);
    child.inbox.set_resting(true);
    assert!(child.reply_since().is_some());
    assert!(child.status().rest.is_some());
    assert!(fleet.nearest_reap().is_some());
    child.lapse(&lease);
    assert!(!child.is_live());
}

#[test]
fn settling_before_the_fire_ends_the_chain_silently() {
    let fleet = Fleet::leased_for_test(Duration::from_secs(60));
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    let weak = child.0.mailbox.downgrade();
    let life = child.0.mailbox.scope();
    let lease = deposit(&child, &fleet, 1);
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
    retained.lapse(&lease);
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
    trunk.inbox.abandon(Queue::Keep);
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
    assert!(matches!(pop(&root.inbox), Some(Next::Item(Item::Human(s))) if s == "hi"));
}

#[test]
fn a_finished_childs_notice_outruns_the_quiesce_verdict() {
    let fleet = Fleet::for_test();
    let trunk = agent(&fleet, "trunk", None);
    let child = agent(&fleet, "child", Some(&trunk));
    assert!(trunk.has_busy_children());
    drop(child);
    assert!(!trunk.has_busy_children());
    let next = trunk
        .inbox
        .draw(|state| {
            if state.has_busy_children() {
                ParkMode::HeldByChildren
            } else {
                ParkMode::Quiesce
            }
        })
        .map(|drawn| drawn.next);
    assert!(
        matches!(next, Some(Next::Item(Item::Agent { name, settled: Settlement::Failed { .. }, .. })) if &*name == "child")
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
            fleet
                .census()
                .wait_empty_after(|| waiting.send(()).unwrap());
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

/// `wait_settled` for `waiter` on another thread; its verdict arrives on the receiver.
fn settling(fleet: &Arc<Fleet>, waiter: TenantId) -> std::sync::mpsc::Receiver<Result<(), String>> {
    let (done, verdict) = std::sync::mpsc::channel();
    let fleet = fleet.clone();
    std::thread::spawn(move || done.send(fleet.wait_settled(waiter, &Pumping::for_test())));
    verdict
}

#[test]
fn wait_settled_waits_for_ended_members_but_not_live_ones() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    root.inbox.abandon(Queue::Recall);
    let verdict = settling(&fleet, root.tenant());
    assert!(matches!(
        verdict.recv_timeout(Duration::from_millis(20)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    drop(child);
    assert_eq!(
        verdict.recv_timeout(Duration::from_secs(5)).unwrap(),
        Ok(())
    );
    assert!(root.is_live());
    assert!(!fleet.is_empty());
}

#[test]
fn the_waiter_is_counted_once_under_racing_cancellation() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let helper = agent(&fleet, "helper", Some(&root));
    helper.end();
    let verdict = settling(&fleet, root.tenant());
    let handle = AgentHandle::clone(&root);
    std::thread::spawn(move || handle.end()).join().unwrap();
    assert!(!root.is_live());
    assert!(matches!(
        verdict.recv_timeout(Duration::from_millis(350)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    drop(helper);
    assert_eq!(
        verdict.recv_timeout(Duration::from_secs(5)).unwrap(),
        Ok(())
    );
}

#[test]
fn a_bare_claim_releases_silently() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let claim = fleet.claim("bare").unwrap();
    assert!(!fleet.is_empty());
    drop(claim);
    assert_eq!(
        fleet.wait_settled(root.tenant(), &Pumping::for_test()),
        Ok(())
    );
    assert!(fleet.claim("bare").is_ok());
}
