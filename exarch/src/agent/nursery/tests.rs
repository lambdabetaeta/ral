use super::*;
use crate::agent::Trunk;
use crate::agent::testkit::{TestAgentSpec, test_agent, trunk};
use crate::record::ChildStatus;
use ral_core::data::FOValue;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
fn ended_helpers_without_debt_are_not_pending_and_failures_are_taken_once() {
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
    assert!(nursery.owing(), "birth owes a report");
    handle.report(ChildStatus::Replied(FOValue::Unit)).unwrap();
    assert!(!nursery.owing(), "a live resting helper owes nothing");
    handle.end();
    assert!(
        !nursery.owing(),
        "an ended helper without debt is not pending"
    );
    assert!(nursery.holds(handle.id()));
    release.send(()).unwrap();
    assert_eq!(nursery.join(handle.id()), vec!["helper teardown failed"]);
    assert!(!nursery.holds(handle.id()));
    assert!(!nursery.owing());
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
    assert!(!nursery.holds(first_handle.id()));
    assert!(!nursery.holds(second_handle.id()));
    assert!(!nursery.owing());
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

#[test]
fn report_commit_holds_the_helper_cell_until_its_parent_notice_is_posted() {
    use crate::record::{Control, RECORD, Record, Voice, read_records};
    use std::sync::mpsc;
    const WAIT: Duration = Duration::from_secs(5);
    for crossing_message in [false, true] {
        let parent = trunk(Trunk::Converses);
        let helper = parent.fork_named("barrier-helper").unwrap();
        let handle = helper.handle();
        let address = handle.0.address.clone();
        let mut nursery = Nursery::default();
        let (finish, finishing) = mpsc::channel();
        nursery.adopt(
            handle.clone(),
            Joined::new(std::thread::spawn(move || {
                finishing.recv_timeout(WAIT).unwrap();
                Teardown { failures: vec![] }
            })),
        );
        let (paused, pausing) = mpsc::channel();
        let (resume, resuming) = mpsc::channel();
        address.after_report_commit(move || {
            paused.send(()).unwrap();
            resuming.recv_timeout(WAIT).unwrap();
        });
        nursery = std::thread::scope(|scope| {
            let report =
                scope.spawn(|| handle.report(ChildStatus::Replied(FOValue::Unit)).unwrap());
            pausing.recv_timeout(WAIT).unwrap();
            let records = read_records(&handle.log_dir().path().join(RECORD)).unwrap();
            assert!(matches!(
                records.last(),
                Some(Record::Control(Control::Status { .. }))
            ));
            assert_eq!(
                parent.handle().0.address.queued_posts(),
                Vec::<crate::record::PostId>::new()
            );
            let (started, starting) = mpsc::channel();
            let (status_done, status_finished) = mpsc::channel();
            let (owing_done, owing_finished) = mpsc::channel();
            let (post_done, post_finished) = mpsc::channel();
            let started_status = started.clone();
            let parent_address = parent.handle().0.address.clone();
            let status_address = &address;
            let status = scope.spawn(move || {
                started_status.send(()).unwrap();
                let status = status_address.status().unwrap();
                status_done.send(()).unwrap();
                (status, parent_address.queued_posts().len())
            });
            let started_owing = started.clone();
            let owing_parent = parent.handle().0.address.clone();
            let owing = scope.spawn(move || {
                started_owing.send(()).unwrap();
                let pending = nursery.owing();
                owing_done.send(()).unwrap();
                (nursery, pending, owing_parent.queued_posts().len())
            });
            let handle_ref = &handle;
            let parent_handle = parent.handle();
            let post = crossing_message.then(|| {
                scope.spawn(move || {
                    started.send(()).unwrap();
                    handle_ref
                        .message(&parent_handle, "crossing follow up".into())
                        .unwrap();
                    post_done.send(()).unwrap();
                })
            });
            for _ in 0..(2 + usize::from(crossing_message)) {
                starting.recv_timeout(WAIT).unwrap();
            }
            for finished in [&status_finished, &owing_finished] {
                assert!(matches!(
                    finished.recv_timeout(Duration::from_millis(50)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
            }
            if crossing_message {
                assert!(matches!(
                    post_finished.recv_timeout(Duration::from_millis(50)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
            }
            resume.send(()).unwrap();
            report.join().unwrap();
            let (status, delivered) = status.join().unwrap();
            assert_eq!(status.report, Some(ChildStatus::Replied(FOValue::Unit)));
            assert_eq!(
                delivered, 1,
                "status cannot observe payment before delivery"
            );
            let (nursery, pending, delivered) = owing.join().unwrap();
            assert_eq!(delivered, 1, "owing cannot observe payment before delivery");
            if !crossing_message {
                assert!(!pending);
                assert!(!status.owes);
            }
            if let Some(post) = post {
                post.join().unwrap();
            }
            nursery
        });
        assert_eq!(handle.status().unwrap().owes, crossing_message);
        if crossing_message {
            let records = read_records(&handle.log_dir().path().join(RECORD)).unwrap();
            let status = records
                .iter()
                .position(|record| matches!(record, Record::Control(Control::Status { .. })))
                .unwrap();
            let message = records
                .iter()
                .position(|record| {
                    matches!(record, Record::Control(Control::Posted(post))
                if post.voice == Some(Voice::Peer) && post.text.contains("crossing follow up"))
                })
                .unwrap();
            assert!(
                status < message,
                "the crossing message records after the report"
            );
        }
        finish.send(()).unwrap();
        assert_eq!(nursery.dismiss_all(), Vec::<String>::new());
    }
}
