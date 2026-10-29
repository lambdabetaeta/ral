use super::*;
use crate::agent::{AgentHandle, roster};
use crate::bus::{Input, Next, Rest};
use crate::enquiry::{RosterState, Spawner};
use crate::record::ChildStatus;
use ral_core::data::FOValue;
use ral_core::process::CancelCause;
use std::sync::Barrier;

const REST: Rest = Rest {
    pending: false,
    schedules: false,
};

#[test]
fn a_reserved_name_counts_construction_until_it_is_dropped() {
    let fleet = Fleet::for_test();
    assert!(fleet.is_empty());
    let name = fleet.name("building").unwrap();
    assert_eq!(name.name(), "building");
    assert!(!fleet.is_empty());
    assert!(fleet.live().is_empty());
    assert!(fleet.roots().is_empty());
    assert!(fleet.resolve("building").is_none());
    assert_eq!(
        fleet.name("building").err(),
        Some(Unborn::NameTaken("building".into()))
    );
    drop(name);
    assert!(fleet.is_empty());
    assert!(fleet.name("building").is_ok());
}

#[test]
fn malformed_names_leave_no_reservation() {
    let fleet = Fleet::for_test();
    for name in ["", "two words", "a/b", "é", "abcdefghijklmnopqrstuvwxy"] {
        let Some(Unborn::NameMalformed(why)) = fleet.name(name).err() else {
            panic!("invalid name {name:?} must be refused");
        };
        assert!(why.contains("ASCII"), "{why}");
        assert!(fleet.is_empty());
    }
    let name = fleet.name("Ab_9-abcdefghijklmnopqrs").unwrap();
    assert_eq!(name.name().len(), 24);
}

#[test]
fn racing_reservations_have_exactly_one_owner() {
    let fleet = Fleet::for_test();
    let start = Barrier::new(2);
    let names = std::thread::scope(|scope| {
        let reserve = || {
            start.wait();
            fleet.name("contended")
        };
        let first = scope.spawn(reserve);
        let second = scope.spawn(reserve);
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(names.iter().filter(|name| name.is_ok()).count(), 1);
    assert_eq!(
        names.iter().find_map(|name| name.as_ref().err()),
        Some(&Unborn::NameTaken("contended".into()))
    );
    drop(names);
    assert!(fleet.name("contended").is_ok());
}

#[test]
fn a_name_does_not_retain_its_fleet() {
    let fleet = Fleet::for_test();
    let name = fleet.name("reserved").unwrap();
    let weak = Arc::downgrade(&fleet);
    drop(fleet);
    assert!(weak.upgrade().is_none());
    drop(name);
}

#[test]
fn a_bound_name_refuses_a_second_owner() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let helper = agent(&fleet, "helper", Some(&root));
    assert_eq!(
        test_agent(
            &fleet,
            TestAgentSpec::child("helper", &root, root.mailbox.life().child())
        )
        .err(),
        Some(Unborn::NameTaken("helper".into()))
    );
    assert_eq!(fleet.resolve("helper").unwrap().id(), helper.id());
    assert_eq!(fleet.live().len(), 2);
}

#[test]
fn ended_agents_leave_the_directory_views_but_reserve_their_names_until_drop() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    assert_eq!(roster::listing(&root, &fleet).len(), 2);
    child.end();
    assert!(fleet.resolve("child").is_none());
    assert_eq!(roster::listing(&root, &fleet).len(), 1);
    assert_eq!(fleet.live().len(), 1);
    assert!(!fleet.is_empty());
    assert_eq!(
        fleet.name("child").err(),
        Some(Unborn::NameTaken("child".into()))
    );
    drop(child);
    assert!(fleet.name("child").is_ok());
}

#[test]
fn retained_handles_do_not_reserve_a_dropped_owners_name() {
    let fleet = Fleet::for_test();
    let owner = agent(&fleet, "reused", None);
    let retained = AgentHandle::clone(&owner);
    drop(owner);
    assert!(!retained.is_live());
    assert!(fleet.is_empty());
    let replacement = agent(&fleet, "reused", None);
    assert_ne!(retained.id(), replacement.id());
    drop(retained);
    assert_eq!(fleet.resolve("reused").unwrap().id(), replacement.id());
    assert!(matches!(fleet.name("reused"), Err(Unborn::NameTaken(_))));
}

#[test]
fn owner_drop_ends_descendants_and_releases_only_its_own_name() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let grandchild = agent(&fleet, "grandchild", Some(&child));
    let retained = AgentHandle::clone(&root);
    drop(root);
    assert!(!retained.is_live());
    assert!(!child.is_live());
    assert!(!grandchild.is_live());
    assert!(fleet.name("root").is_ok());
    assert!(matches!(fleet.name("child"), Err(Unborn::NameTaken(_))));
    assert!(!fleet.is_empty());
    drop((child, grandchild));
    assert!(fleet.is_empty());
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
    assert!(root.is_live());
    assert!(sibling.is_live());
}

#[test]
fn a_child_constructed_beneath_an_ended_parent_is_born_cancelled() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    root.end();
    let late = agent(&fleet, "late", Some(&root));
    assert!(!late.is_live());
    assert!(fleet.resolve("late").is_none());
    assert_eq!(late.mailbox.life().cause(), Some(CancelCause::Cancelled));
    drop(late);
    assert!(fleet.name("late").is_ok());
}

#[test]
fn stopping_an_exchange_spares_the_helper_subtree_and_the_next_exchange() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let grandchild = agent(&fleet, "grandchild", Some(&child));
    child.steer("work".into()).unwrap();
    let Next::Take { exchange, .. } = child.mailbox.next(REST) else {
        panic!("a human prompt starts an exchange");
    };
    assert!(child.mailbox.begin());
    child.steer("queued".into()).unwrap();
    assert!(child.stop_current());
    assert_eq!(child.queued(), ["queued"]);
    assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
    assert!(root.is_live());
    assert!(child.is_live());
    assert!(grandchild.is_live());
    child.steer("next".into()).unwrap();
    let Next::Take { exchange: next, .. } = child.mailbox.next(REST) else {
        panic!("the next prompt starts a fresh exchange");
    };
    assert!(!next.is_cancelled());
}

#[test]
fn every_roster_row_names_who_started_it() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let mid = agent(&fleet, "mid", Some(&root));
    let _leaf = agent(&fleet, "leaf", Some(&mid));
    let edges: Vec<_> = roster::listing(&root, &fleet)
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
            ("root".into(), None),
            ("mid".into(), Some("root".into())),
            ("leaf".into(), Some("mid".into())),
        ]
    );
}

#[test]
fn the_summary_counts_only_resting_replies_to_the_reader() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let mid = agent(&fleet, "mid", Some(&root));
    let _sibling = agent(&fleet, "sibling", Some(&root));
    let child = agent(&fleet, "child", Some(&mid));
    let before = roster::summary(&mid, &fleet);
    assert_eq!(before.live, 3);
    assert_eq!(before.replied, 0);
    child.report(ChildStatus::Replied(FOValue::Int { value: 1 }));
    let answered = roster::summary(&mid, &fleet);
    assert_eq!(answered.live, 3, "a report does not end its owner");
    assert_eq!(answered.replied, 1);
    assert_eq!(roster::summary(&root, &fleet).replied, 0);
    child.steer("a quiet human follow-up".into()).unwrap();
    assert!(matches!(child.mailbox.next(REST), Next::Take { .. }));
    assert!(child.mailbox.begin());
    assert_eq!(roster::summary(&mid, &fleet).replied, 0);
    assert!(matches!(
        roster::listing(&mid, &fleet).last().unwrap().state,
        RosterState::Busy
    ));
    assert!(matches!(child.mailbox.next(REST), Next::Settled { .. }));
    child.report(ChildStatus::Stopped);
    assert_eq!(roster::summary(&mid, &fleet).replied, 0);
}

#[test]
fn a_second_root_is_outside_the_first_roots_tree_and_roots_are_sorted() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let branch = agent(&fleet, "branch", None);
    let names: Vec<_> = roster::listing(&root, &fleet)
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, ["root", "child"]);
    assert!(root.descendant(&branch).is_none());
    assert_eq!(
        fleet.live().iter().map(AgentHandle::id).collect::<Vec<_>>(),
        [root.id(), child.id(), branch.id()]
    );
    assert_eq!(
        fleet
            .roots()
            .iter()
            .map(AgentHandle::id)
            .collect::<Vec<_>>(),
        [root.id(), branch.id()]
    );
    branch.end();
    assert_eq!(fleet.roots().len(), 1);
}

#[test]
fn beginning_an_exchange_stamps_its_clock_without_observation_renewing_rest() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let resting = root.status().rest;
    assert!(root.status().last_exchange.is_none());
    root.steer("hi".into()).unwrap();
    assert!(root.status().last_exchange.is_none());
    assert_eq!(root.status().rest, resting);
    assert!(matches!(
        root.mailbox.next(REST),
        Next::Take { input: Input::Human(text), .. } if text == "hi"
    ));
    assert!(
        root.status().last_exchange.is_none(),
        "a quiet input begins nothing"
    );
    assert!(root.mailbox.begin());
    assert!(root.status().rest.is_none());
    assert!(root.status().last_exchange.is_some());
}
