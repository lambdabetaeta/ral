use super::*;
use crate::agent::nursery::{Joined, Teardown};
use crate::agent::testkit::trunk;
use crate::agent::{AgentHandle, Trunk};
use crate::bus::{Emitter, Signal, WORKER_PANIC_PREFIX, channel};
use crate::record::{Born, Transient};
use ral_core::process::CancelCause;
use ral_core::sync::LockExt;
use ral_core::test_helper::eventually;
use std::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn fleet_end_ends_every_agent_and_future_roots_are_born_cancelled() {
    let fleet = Fleet::for_test();
    let root = agent(&fleet, "root", None);
    let child = agent(&fleet, "child", Some(&root));
    let other = agent(&fleet, "other", None);
    fleet.registry.end(CancelCause::Terminated);
    for owner in [&root, &child, &other] {
        assert!(!owner.is_live());
        assert_eq!(owner.mailbox.life().cause(), Some(CancelCause::Terminated));
    }
    let late = agent(&fleet, "late", None);
    assert!(!late.is_live());
    assert_eq!(late.mailbox.life().cause(), Some(CancelCause::Terminated));
    assert!(fleet.registry.live().is_empty());
    assert!(fleet.registry.roots().is_empty());
    assert!(fleet.registry.resolve("late").is_none());
    assert!(!fleet.registry.is_empty(), "ending is not teardown");
    drop((root, child, other, late));
    assert!(fleet.registry.is_empty());
}

#[test]
fn joining_roots_collects_each_failure_and_drains_only_once() {
    let fleet = Fleet::for_test();
    for failure in ["first", "second"] {
        fleet.adopt_root(Joined::new(std::thread::spawn(move || Teardown {
            failures: vec![failure.into()],
        })));
    }
    fleet.adopt_root(Joined::new(std::thread::spawn(|| {
        panic!("root panic");
    })));
    fleet.registry.end(CancelCause::Cancelled);
    assert_eq!(
        fleet.join_roots(),
        [
            "first",
            "second",
            &format!("{WORKER_PANIC_PREFIX}root panic")
        ]
    );
    assert_eq!(fleet.join_roots(), Vec::<String>::new());
    assert!(fleet.roots.lock_ignore_poison().is_empty());
}

#[test]
fn joining_waits_for_a_root_after_its_last_name_leaves() {
    let fleet = Fleet::for_test();
    let owner = agent(&fleet, "departing", None);
    let retained = AgentHandle::clone(&owner);
    let (left, departed) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    fleet.adopt_root(Joined::new(std::thread::spawn(move || {
        drop(owner);
        left.send(()).unwrap();
        resume.recv_timeout(TIMEOUT).unwrap();
        Teardown {
            failures: vec!["late teardown".into()],
        }
    })));
    departed.recv_timeout(TIMEOUT).unwrap();
    assert!(fleet.registry.is_empty());
    assert!(!retained.is_live());
    fleet.registry.end(CancelCause::Cancelled);
    let (done, finished) = mpsc::channel();
    std::thread::scope(|scope| {
        let joining = scope.spawn(|| {
            done.send(fleet.join_roots()).unwrap();
        });
        assert!(
            eventually(TIMEOUT, || fleet
                .roots
                .lock_ignore_poison()
                .is_empty()
                .then_some(()))
            .is_some(),
            "the joiner takes the root thread before waiting on it"
        );
        assert!(matches!(
            finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), ["late teardown"]);
        joining.join().unwrap();
    });
    assert_eq!(Arc::strong_count(&fleet), 1);
}

#[test]
fn joining_roots_also_joins_a_root_adopted_during_the_join() {
    let fleet = Fleet::for_test();
    let (release, resume) = mpsc::channel();
    let launching = fleet.clone();
    fleet.adopt_root(Joined::new(std::thread::spawn(move || {
        resume.recv_timeout(TIMEOUT).unwrap();
        launching.adopt_root(Joined::new(std::thread::spawn(|| Teardown {
            failures: vec!["racing root".into()],
        })));
        Teardown {
            failures: vec!["initial root".into()],
        }
    })));
    fleet.registry.end(CancelCause::Cancelled);
    std::thread::scope(|scope| {
        let joining = scope.spawn(|| fleet.join_roots());
        assert!(
            eventually(TIMEOUT, || fleet
                .roots
                .lock_ignore_poison()
                .is_empty()
                .then_some(()))
            .is_some(),
            "adopt the racing root only after the first drain"
        );
        release.send(()).unwrap();
        assert_eq!(joining.join().unwrap(), ["initial root", "racing root"]);
    });
    assert_eq!(fleet.join_roots(), Vec::<String>::new());
}

#[test]
fn joining_roots_preserves_teardown_failures() {
    let fleet = Fleet::for_test();
    fleet.adopt_root(Joined::new(std::thread::spawn(|| Teardown {
        failures: vec!["retained failure".into()],
    })));
    fleet.registry.end(CancelCause::Cancelled);
    assert_eq!(fleet.join_roots(), ["retained failure"]);
    assert_eq!(Arc::strong_count(&fleet), 1);
}

#[test]
fn a_launched_root_releases_its_name_and_announces_death_once() {
    let root = trunk(Trunk::Converses);
    let handle = root.handle();
    let fleet = root.fleet.clone();
    let born = Born {
        log_dir: root.log_dir().clone(),
        name: handle.0.name.clone(),
        parent: None,
        resumed: None,
        reader: root.recorder().reader(),
    };
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, handle.id());
    let thread = root.launch(born, None, &emit).expect("a root thread");
    fleet.adopt_root(thread);
    fleet.registry.end(CancelCause::Cancelled);
    assert_eq!(fleet.join_roots(), Vec::<String>::new());
    assert!(fleet.registry.is_empty());
    assert!(fleet.registry.resolve(&handle.0.name).is_none());
    assert!(fleet.registry.name(&handle.0.name).is_ok());
    let mut deaths = 0;
    while let Ok(signal) = rx.try_recv() {
        if matches!(signal, Signal::Transient(id, Transient::Died { .. }) if id == handle.id()) {
            deaths += 1;
        }
    }
    assert_eq!(deaths, 1);
}
