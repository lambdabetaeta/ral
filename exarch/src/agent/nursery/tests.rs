use super::*;
use crate::agent::Trunk;
use crate::agent::testkit::{TestAgentSpec, test_agent, trunk};
use crate::record::ChildStatus;
use ral_core::data::FOValue;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn pending(nursery: &Nursery) -> bool {
    crate::bus::Rest {
        owing: nursery.owing(),
        lives: nursery.lives(),
        schedules: false,
    }
    .pending()
}

#[test]
fn joined_waits_on_drop_even_during_unwind() {
    let completed = Arc::new(AtomicBool::new(false));
    let done = completed.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _owned = Joined::new(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            done.store(true, Ordering::Release);
        }));
        panic!("unwind the owner");
    }));
    assert!(result.is_err());
    assert!(completed.load(Ordering::Acquire));
}

#[test]
fn ended_helpers_remain_pending_until_joined_and_failures_are_taken_once() {
    let parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*helper).clone();
    let mut nursery = Nursery::default();
    let (release, receiver) = std::sync::mpsc::channel();
    nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(helper);
            Teardown {
                failures: vec!["helper teardown failed".into()],
            }
        })),
    );
    assert!(pending(&nursery), "birth owes a report");
    handle.report(ChildStatus::Replied(FOValue::Unit));
    assert!(!pending(&nursery), "a live resting helper owes nothing");
    handle.end();
    assert!(
        pending(&nursery),
        "an ended helper is pending even without a report debt"
    );
    assert!(!nursery.is_joined(handle.id()));
    release.send(()).unwrap();
    assert_eq!(nursery.join(handle.id()), vec!["helper teardown failed"]);
    assert!(nursery.is_joined(handle.id()));
    assert!(!pending(&nursery));
    assert_eq!(nursery.join(handle.id()), Vec::<String>::new());
    assert_eq!(nursery.dismiss_all(), Vec::<String>::new());
}

#[test]
fn dismiss_all_ends_every_helper_before_joining_any() {
    let parent = trunk(Trunk::Converses);
    let first = test_agent(
        &parent.fleet,
        TestAgentSpec::child("first", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let second = test_agent(
        &parent.fleet,
        TestAgentSpec::child("second", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let first_handle = (*first).clone();
    let second_handle = (*second).clone();
    let wait_for_both = [first_handle.clone(), second_handle.clone()];
    let mut nursery = Nursery::default();
    nursery.adopt(
        first_handle.clone(),
        Joined::new(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while wait_for_both.iter().any(crate::agent::AgentHandle::is_live) {
                assert!(
                    Instant::now() < deadline,
                    "all helpers must be ended before the first join"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(first);
            Teardown {
                failures: vec!["first failure".into()],
            }
        })),
    );
    nursery.adopt(
        second_handle.clone(),
        Joined::new(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while second.is_live() {
                assert!(
                    Instant::now() < deadline,
                    "dismiss must end the second helper before joining the first"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(second);
            Teardown {
                failures: vec!["second failure".into()],
            }
        })),
    );
    assert_eq!(
        nursery.dismiss_all(),
        vec!["first failure", "second failure"]
    );
    assert!(nursery.is_joined(first_handle.id()));
    assert!(nursery.is_joined(second_handle.id()));
    assert!(!pending(&nursery));
    assert_eq!(nursery.dismiss_all(), Vec::<String>::new());
}

#[test]
fn newest_name_remains_discoverable_after_join() {
    let parent = trunk(Trunk::Converses);
    let mut nursery = Nursery::default();
    let old = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let old_handle = (*old).clone();
    nursery.adopt(
        old_handle.clone(),
        Joined::new(std::thread::spawn(move || {
            drop(old);
            Teardown { failures: vec![] }
        })),
    );
    assert_eq!(nursery.join(old_handle.id()), Vec::<String>::new());
    let new = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let new_handle = (*new).clone();
    nursery.adopt(
        new_handle.clone(),
        Joined::new(std::thread::spawn(move || {
            drop(new);
            Teardown { failures: vec![] }
        })),
    );
    assert_eq!(nursery.named("helper").unwrap().id(), new_handle.id());
    assert_eq!(nursery.join(new_handle.id()), Vec::<String>::new());
    assert_eq!(nursery.named("helper").unwrap().id(), new_handle.id());
    assert!(nursery.named("missing").is_none());
}
