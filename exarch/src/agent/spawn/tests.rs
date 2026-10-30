use super::*;
use crate::agent::testkit::*;
use crate::agent::{TestTrunk, Trunk};
use crate::bus::{Input, Next, Rest, Signal};
use crate::provider::scripted::{Reply, Script};
use crate::record::{RECORD, Record, read_records};
use ral_core::carrier::{Severed, Transport as _};
use std::time::{Duration, Instant};

fn wait_until(reason: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "{reason}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn next_input(parent: &Agent) -> Input {
    let handle = parent.handle();
    let _timeout = ral_core::process::arm_callback(Duration::from_secs(10), move || handle.end());
    let Next::Take { input, .. } = parent.mailbox.next(parent.rest()) else {
        panic!("a helper posts a report followed by Gone")
    };
    input
}

#[test]
fn a_helper_severed_mid_work_reports_failed_then_gone_and_records_the_cause() {
    let mut parent = Agent::for_test_with(TestTrunk {
        installers: dressed(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS)),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let mut child = forked_child(&parent, "helper", "`inherit");
    set_provider(
        &mut child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    let crate::agent::engine::EngineKind::Identity(engine) = child.engine().kind() else {
        panic!("identity")
    };
    let log = child.log_dir().path().join(RECORD);
    let cause = Severed::Closed("engine lost during helper work".into());
    let expected_reason = crate::agent::EngineLost::running(&cause, child.run_dir()).to_string();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    let helper = launch_child(&mut parent, child, Some("work".into()), &emit);
    wait_until("helper did not start work", || {
        helper.status().unwrap().job.is_some()
    });
    ral_core::carrier::Transport::sever(&*engine, cause.clone());
    assert!(
        matches!(next_input(&parent), Input::Child { id, status: ChildStatus::Failed(reason), .. } if id == helper.id() && reason == expected_reason)
    );
    assert!(matches!(parent.mailbox.next(parent.rest()), Next::Gone(id) if id == helper.id()));
    assert_eq!(parent.body.nursery.join(helper.id()), Vec::<String>::new());
    let records = read_records(&log).unwrap();
    assert_eq!(records.iter().filter(|record| matches!(record, Record::Forensic(Forensic::EngineSevered { cause: recorded }) if recorded == &cause)).count(), 1);
    let signals: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(signals.iter().any(
        |signal| matches!(signal, Signal::Transient(id, Transient::Died { .. }) if *id == helper.id())
    ));
    assert!(parent.fleet.registry.resolve("helper").is_none());
    assert!(matches!(
        helper.status().unwrap().report,
        Some(ChildStatus::Failed(_))
    ));
}

#[test]
fn panic_in_run_joins_a_working_helper_then_reports_failed_and_gone() {
    let mut parent = Agent::for_test_with(TestTrunk {
        installers: dressed(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS)),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let mut child = forked_child(&parent, "helper", "`inherit");
    let mut grandchild = forked_child(&child, "grandchild", "`inherit");
    set_provider(
        &mut grandchild,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    let grandchild = launch_child(&mut child, grandchild, Some("work".into()), &emit);
    wait_until("grandchild did not start work", || {
        grandchild.status().unwrap().job.is_some()
    });
    crate::agent::attend::hooks::panic_on_run(child.id());
    let life = child.mailbox.life();
    child
        .handle()
        .0
        .address
        .after_report_commit(move || assert!(life.is_cancelled()));
    let log = child.log_dir().path().join(RECORD);
    let helper = launch_child(&mut parent, child, None, &emit);
    let failures = parent.body.nursery.join(helper.id());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].starts_with(WORKER_PANIC_PREFIX));
    assert!(!grandchild.is_live());
    assert!(
        parent.fleet.registry.resolve("grandchild").is_none(),
        "unwind joined the working helper before returning"
    );
    assert!(
        matches!(next_input(&parent), Input::Child { id, status: ChildStatus::Failed(reason), .. } if id == helper.id() && reason.starts_with(WORKER_PANIC_PREFIX))
    );
    assert!(matches!(parent.mailbox.next(parent.rest()), Next::Gone(id) if id == helper.id()));
    assert!(
        matches!(helper.status().unwrap().report, Some(ChildStatus::Failed(reason)) if reason.starts_with(WORKER_PANIC_PREFIX))
    );
    assert!(!helper.status().unwrap().owes);
    assert_eq!(
        read_records(&log)
            .unwrap()
            .iter()
            .filter(|record| matches!(
                record,
                Record::Control(crate::record::Control::Status { .. })
            ))
            .count(),
        1
    );
    let answer = parent.ral(
        &parent.mailbox.life().child(),
        "exarch-agents `read 'helper'",
        5,
        &emit,
    );
    assert!(
        answer.failed,
        "a departed failure is an actionable desk refusal: {}",
        answer.text
    );
    assert!(answer.text.contains(WORKER_PANIC_PREFIX), "{}", answer.text);
    assert!(
        read_records(&log)
            .unwrap()
            .iter()
            .any(|record| matches!(record,
                Record::Forensic(Forensic::Error { text }) if text.starts_with(WORKER_PANIC_PREFIX)
            ))
    );
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok()).any(|signal| matches!(signal,
            Signal::Transient(id, Transient::Died { failures: reported })
                if id == helper.id() && reported == failures
        ))
    );
}

#[test]
fn a_panicked_helper_that_no_longer_owes_its_parent_posts_only_gone() {
    let mut parent = trunk(Trunk::Converses);
    let mut child = parent.fork_named("helper").unwrap();
    child.report(ChildStatus::Replied(ral_core::data::FOValue::Unit));
    assert!(matches!(next_input(&parent), Input::Child { .. }));
    crate::agent::attend::hooks::panic_on_run(child.id());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    let helper = launch_child(&mut parent, child, None, &emit);
    let failures = parent.body.nursery.join(helper.id());
    assert_eq!(failures.len(), 1);
    assert!(matches!(parent.mailbox.next(parent.rest()), Next::Gone(id) if id == helper.id()));
    assert!(parent.mailbox.drain().is_empty());
    assert!(matches!(
        helper.status().unwrap().report,
        Some(ChildStatus::Failed(_))
    ));
}

#[test]
fn the_name_leaves_the_directory_before_died_is_published() {
    let mut parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    child.handle().end();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    let helper = launch_child(&mut parent, child, None, &emit);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let signal = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("Died is bounded");
        if matches!(signal, Signal::Transient(id, Transient::Died { .. }) if id == helper.id()) {
            assert!(parent.fleet.registry.resolve("helper").is_none());
            break;
        }
    }
    assert!(matches!(next_input(&parent), Input::Child { .. }));
    assert!(matches!(parent.mailbox.next(parent.rest()), Next::Gone(id) if id == helper.id()));
    assert_eq!(parent.body.nursery.join(helper.id()), Vec::<String>::new());
}

#[test]
fn final_session_end_writer_failure_is_in_died_and_the_join_result() {
    let mut agent = trunk(Trunk::Converses);
    let recorder = agent.recorder();
    let handle = agent.handle();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, agent.id());
    let mut terminal = Terminal {
        emit,
        handle,
        recorder: recorder.clone(),
        completion: None,
    };
    let teardown = agent.teardown();
    recorder.fail_after(0);
    drop(agent);
    let teardown = terminal.complete(teardown);
    assert_eq!(teardown.failures.len(), 1);
    assert!(teardown.failures[0].contains("injected record write failure"));
    drop(terminal);
    let signals = rx.drain_pending();
    assert!(signals.iter().any(|signal| matches!(signal,
        Signal::Transient(_, Transient::Died { failures }) if *failures == teardown.failures
    )));
}

#[test]
fn an_uncompleted_terminal_cannot_publish_success() {
    let agent = trunk(Trunk::Converses);
    let (tx, rx) = crate::bus::channel();
    let terminal = Terminal {
        emit: Emitter::new(tx, agent.id()),
        handle: agent.handle(),
        recorder: agent.recorder(),
        completion: None,
    };
    drop(terminal);
    assert!(
        matches!(rx.try_recv().unwrap(), Signal::Transient(_, Transient::Died { failures }) if !failures.is_empty())
    );
}

#[test]
fn an_unwinding_helper_publishes_failure_before_its_parent_settles() {
    let mut parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    crate::agent::attend::hooks::panic_on_run(child.id());
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    parent.couple(&emit);
    assert!(parent.mailbox.begin());
    let helper = launch_child(&mut parent, child, None, &emit);
    assert!(parent.rest().pending);
    let child_report = next_input(&parent);
    assert!(
        matches!(&child_report, Input::Child { id, status: ChildStatus::Failed(_), .. } if *id == helper.id())
    );
    assert!(parent.admit(child_report).is_some());
    let Next::Gone(id) = parent.mailbox.next(parent.rest()) else {
        panic!("Gone")
    };
    assert_eq!(id, helper.id());
    parent.gone(id);
    assert!(matches!(
        parent.mailbox.next(Rest { pending: false }),
        Next::Settled { .. }
    ));
    let signals = rx.drain_pending();
    let died = signals.iter().position(|signal| matches!(signal,
        Signal::Transient(id, Transient::Died { failures }) if *id == helper.id() && !failures.is_empty()
    )).expect("failed terminal outcome");
    let settled = signals.iter().position(|signal| matches!(signal,
        Signal::Commit(id, commit) if *id == parent.id() && commit.records().iter().any(|record| matches!(record.value(), Record::Control(crate::record::Control::JobSettled { .. })))
    )).expect("parent settlement");
    assert!(died < settled);
}

#[test]
fn a_poisoned_helper_owner_tears_down_delivers_one_failure_and_lets_the_parent_settle() {
    let mut parent = Agent::for_test_with(TestTrunk {
        installers: dressed(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS)),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let mut child = forked_child(&parent, "poisoned-helper", "`inherit");
    set_provider(
        &mut child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    let handle = child.handle();
    let crate::agent::engine::EngineKind::Identity(engine) = child.engine().kind() else {
        panic!("identity")
    };
    assert!(parent.mailbox.begin());
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, parent.id());
    let helper = launch_child(&mut parent, child, Some("work".into()), &emit);
    wait_until("helper did not start", || {
        handle.status().unwrap().job.is_some()
    });
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _result = handle
                .0
                .address
                .transact(|_| panic!("poison the live helper cell"));
        }))
        .is_err()
    );
    let expected = format!(
        "a thread panicked inside agent {}'s transaction, so its state may no longer match its log; it has ended, and resuming the run rebuilds its state from the log",
        handle.id()
    );
    assert_eq!(
        handle.status().err().expect("poisoned status").to_string(),
        expected
    );
    wait_until("poisoned helper failed to depart", || {
        parent
            .mailbox
            .source_depths()
            .iter()
            .any(|(source, depth)| source.to_string() == "agent" && *depth == 2)
    });
    assert!(!helper.is_live());
    assert!(
        engine.session_ended().unwrap().is_some(),
        "the actual owner's teardown ended its engine"
    );
    assert!(
        matches!(next_input(&parent), Input::Child { id, status: ChildStatus::Failed(reason), .. } if id == helper.id() && reason == expected)
    );
    assert!(matches!(parent.mailbox.next(parent.rest()), Next::Gone(id) if id == helper.id()));
    parent.gone(helper.id());
    assert!(!parent.body.nursery.holds(helper.id()));
    assert!(!parent.rest().pending);
    assert!(matches!(
        parent.mailbox.next(parent.rest()),
        Next::Settled { stopped: false }
    ));
    assert!(
        parent.mailbox.drain().is_empty(),
        "no second failure delivery"
    );
    assert!(
        parent
            .body
            .failures
            .iter()
            .any(|failure| failure == &expected)
    );
}
