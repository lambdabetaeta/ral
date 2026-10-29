//! Reports, follow-ups, quiet human work, Stop and structured teardown.

use super::*;
use crate::agent::Admission;
use crate::agent::nursery::Joined;
use crate::bus::{ENDED_BEFORE_REPORTING, Refused};
use crate::record::{ChildStatus, RECORD, Settlement, read_records};
use std::sync::Arc;

fn next_child(parent: &Agent) -> (crate::record::AgentId, ChildStatus) {
    let handle = parent.handle();
    let _timeout = ral_core::process::arm_callback(Duration::from_secs(10), move || handle.end());
    match parent.mailbox.next(&parent.rest()) {
        Next::Take {
            input: Input::Child { id, status, .. },
            ..
        } => (id, status),
        _ => panic!("the parent's next input is its helper's report"),
    }
}

#[test]
fn a_returned_helper_accepts_a_second_human_message_without_focus() {
    let mut parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    set_provider(
        &child,
        scripted(
            "test-model",
            Script::new().then(reply("1")).then(reply("2")),
        ),
    );
    let (emit, _rx) = emitter(&parent);
    let helper = launch_child(&mut parent, child, Some("first task".into()), &emit);
    wait_until("initial report", || {
        !helper.status().working() && helper.status().report.is_some()
    });
    assert!(matches!(
        next_child(&parent),
        (_, ChildStatus::Replied(FOValue::Int { value: 1 }))
    ));
    helper.steer("second task".into()).unwrap();
    wait_until("human follow-up report", || {
        matches!(
            helper.status().report,
            Some(ChildStatus::Replied(FOValue::Int { value: 2 }))
        ) && !helper.status().working()
    });
    assert!(
        parent.mailbox.drain().is_empty(),
        "human follow-ups owe the parent nothing"
    );
    assert_eq!(
        parent.body.state.nursery.dismiss_all(),
        Vec::<String>::new()
    );
}

#[test]
fn helper_idle_limits_rest_not_work() {
    let mut parent = Agent::for_test_with(TestTrunk {
        helper_idle: Duration::from_millis(25),
        installers: dressed(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS)),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let child = forked_child(&parent, "helper", "`inherit");
    set_provider(
        &child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    let (emit, _rx) = emitter(&parent);
    let helper = launch_child(&mut parent, child, Some("work".into()), &emit);
    wait_until("helper did not open a job", || {
        helper.status().job.is_some()
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(helper.is_live(), "idle must not bound working time");
    assert_eq!(
        parent.body.state.nursery.dismiss_all(),
        Vec::<String>::new()
    );
    assert!(matches!(next_child(&parent), (_, ChildStatus::Failed(_))));
}

#[test]
fn helper_ends_after_resting_and_does_not_repeat_its_report() {
    let mut parent = Agent::for_test_with(TestTrunk {
        helper_idle: Duration::from_millis(50),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let child = parent.fork_named("helper").unwrap();
    set_provider(
        &child,
        scripted("test-model", Script::new().then(reply("42"))),
    );
    let id = child.id();
    let (emit, _rx) = emitter(&parent);
    let helper = launch_child(&mut parent, child, Some("work".into()), &emit);
    assert!(matches!(
        next_child(&parent),
        (_, ChildStatus::Replied(FOValue::Int { value: 42 }))
    ));
    wait_until("resting helper did not time out", || !helper.is_live());
    let Next::Take { input, .. } = parent.mailbox.next(&parent.rest()) else {
        panic!("Gone")
    };
    assert!(matches!(input, Input::Gone(gone) if gone == id));
    assert!(parent.admit(input).is_none());
    assert!(parent.body.state.nursery.is_joined(id));
    assert!(parent.mailbox.drain().is_empty());
}

#[test]
fn a_previous_reply_does_not_suppress_followup_repairs() {
    for repair in [
        Reply::text("prose without a return"),
        Reply::empty(),
        Reply::stopped("early-stop"),
        Reply::truncated_with_tool_calls(vec![]),
        Reply::stalled("partial response"),
    ] {
        let parent = trunk(Trunk::Converses);
        let mut child = parent.fork_named("helper").unwrap();
        let (emit, _rx) = emitter(&child);
        set_provider(
            &child,
            scripted("test-model", Script::new().then(reply("1"))),
        );
        let first = human_exchange(&mut child, "first task", &emit);
        settle(&mut child, first);
        assert!(matches!(
            next_child(&parent),
            (_, ChildStatus::Replied(FOValue::Int { value: 1 }))
        ));
        set_provider(
            &child,
            scripted("test-model", Script::new().then(repair).then(reply("2"))),
        );
        child
            .handle()
            .message(&parent.handle(), "second task".into())
            .unwrap();
        let Next::Take { input, exchange } = child.mailbox.next(&child.rest()) else {
            panic!("follow-up")
        };
        let prompt = child.admit(input).unwrap();
        assert!(child.mailbox.begin());
        child.body.state.readings.nudges.reset();
        let outcome = child.exchange(&exchange, prompt, &emit);
        assert!(matches!(outcome, AgentOutcome::Replied));
        settle(&mut child, outcome);
        assert!(matches!(
            next_child(&parent),
            (_, ChildStatus::Replied(FOValue::Int { value: 2 }))
        ));
        assert!(!child.handle().status().working());
        assert!(child.is_ready());
    }
}

#[test]
fn nonreply_followups_publish_new_failure_instead_of_the_previous_reply() {
    for failure in [
        (0..4).fold(Script::new(), |script, _| {
            script.then(Reply::text("no reply"))
        }),
        (0..4).fold(Script::new(), |script, _| {
            script.then(Reply::stopped("early-stop"))
        }),
        Script::new().then(Reply::error(ProviderError::Api {
            status: Some(400),
            model: "test-model".into(),
            message: "terminal failure".into(),
            body: None,
        })),
        Script::new().then(Reply::panicking()),
    ] {
        let parent = trunk(Trunk::Converses);
        let mut child = parent.fork_named("helper").unwrap();
        let (emit, _rx) = emitter(&child);
        set_provider(
            &child,
            scripted("test-model", Script::new().then(reply("1"))),
        );
        let first = human_exchange(&mut child, "first task", &emit);
        settle(&mut child, first);
        let _report = next_child(&parent);
        set_provider(&child, scripted("test-model", failure));
        child
            .handle()
            .message(&parent.handle(), "follow up".into())
            .unwrap();
        let Next::Take { input, exchange } = child.mailbox.next(&child.rest()) else {
            panic!("follow-up")
        };
        let prompt = child.admit(input).unwrap();
        assert!(child.mailbox.begin());
        child.body.state.readings.nudges.reset();
        let outcome = child.exchange(&exchange, prompt, &emit);
        let reason = outcome.clone().reason();
        settle(&mut child, outcome);
        let (_, status) = next_child(&parent);
        assert!(matches!(status, ChildStatus::Failed(ref failure) if failure == &reason));
        assert!(
            matches!(child.handle().status().report, Some(ChildStatus::Failed(failure)) if failure == reason)
        );
        assert!(!child.handle().status().working());
        assert!(child.is_ready());
    }
}

#[test]
fn status_records_publish_reports_without_a_working_compatibility_record() {
    let parent = trunk(Trunk::Converses);
    let mut child = parent.fork_named("helper").unwrap();
    let log = child.log_dir().path().join(RECORD);
    assert!(child.handle().status().working());
    child.report(ChildStatus::Replied(FOValue::Int { value: 1 }));
    assert!(matches!(
        next_child(&parent),
        (_, ChildStatus::Replied(FOValue::Int { value: 1 }))
    ));
    child
        .handle()
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    assert!(child.handle().status().working());
    assert!(matches!(
        child.handle().status().report,
        Some(ChildStatus::Replied(FOValue::Int { value: 1 }))
    ));
    let _teardown = child.teardown();
    let (_, status) = next_child(&parent);
    assert!(matches!(status, ChildStatus::Failed(ref why) if why == ENDED_BEFORE_REPORTING));
    assert!(
        matches!(child.handle().status().report, Some(ChildStatus::Failed(ref why)) if why == ENDED_BEFORE_REPORTING)
    );
    drop(child);
    let records = read_records(&log).unwrap();
    let reports: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Forensic(Forensic::Status { status }) => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(reports.len(), 2);
    assert!(matches!(reports[1], ChildStatus::Failed(why) if why == ENDED_BEFORE_REPORTING));
    let last_report = records
        .iter()
        .rposition(|record| matches!(record, Record::Forensic(Forensic::Status { .. })))
        .unwrap();
    let ended = records
        .iter()
        .position(|record| matches!(record, Record::Forensic(Forensic::SessionEnded)))
        .unwrap();
    assert!(last_report < ended);
}

#[test]
fn a_followup_crossing_a_reply_keeps_the_parent_pending_and_the_report_unread() {
    let mut parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    let helper = child.handle();
    let (release, ready) = std::sync::mpsc::channel();
    parent.body.state.nursery.adopt(
        helper.clone(),
        Joined::new(std::thread::spawn(move || {
            ready.recv_timeout(Duration::from_secs(10)).unwrap();
            let mut child = child;
            child.teardown()
        })),
    );
    assert!(parent.mailbox.begin());
    let job = parent.handle().status().job.unwrap();
    helper
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    helper.report(ChildStatus::Replied(FOValue::Unit));
    assert!(
        helper.status().owes_parent,
        "queued parent input is still owed"
    );
    assert!(parent.rest().pending());
    let Next::Take { input, .. } = parent.mailbox.next(&parent.rest()) else {
        panic!("report, not settlement")
    };
    assert!(matches!(input, Input::Child { .. }));
    assert!(parent.admit(input).is_some());
    assert_eq!(parent.handle().status().job, Some(job));
    release.send(()).unwrap();
    let failures = parent.body.state.nursery.join(helper.id());
    assert_eq!(failures, Vec::<String>::new());
    assert!(matches!(next_child(&parent), (_, ChildStatus::Failed(_))));
    assert!(parent.handle().is_live());
}

#[test]
fn a_parent_waking_after_a_report_cannot_settle_without_reading_it() {
    let mut parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*helper).clone();
    parent.body.state.nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            wait_until("helper ended", || !helper.is_live());
            drop(helper);
            Teardown { failures: vec![] }
        })),
    );
    assert!(parent.mailbox.begin());
    handle.report(ChildStatus::Replied(FOValue::Unit));
    assert!(
        !parent.rest().pending(),
        "report is already queued when the debt clears"
    );
    let Next::Take { input, .. } = parent.mailbox.next(&parent.rest()) else {
        panic!("report before settlement")
    };
    assert!(parent.admit(input).is_some());
    assert!(matches!(
        parent.mailbox.next(&parent.rest()),
        Next::Settled { stopped: false }
    ));
}

#[test]
fn human_work_on_a_resting_helper_is_quiet_under_every_parent_admission() {
    for admission in [
        Admission::Free,
        Admission::Gated,
        Admission::Parent {
            idle: Duration::from_hours(5),
        },
    ] {
        let parent = Agent::for_test_with(TestTrunk {
            trunk: Trunk::Converses,
            admission,
            ..TestTrunk::new("system")
        })
        .unwrap();
        let mut child = parent.fork_named("helper").unwrap();
        child.report(ChildStatus::Replied(FOValue::Int { value: 1 }));
        let _report = next_child(&parent);
        let (emit, rx) = emitter(&parent);
        parent.couple(&emit);
        let (child_emit, _child_rx) = emitter(&child);
        child.handle().steer("human task".into()).unwrap();
        let Next::Take { input, exchange } = child.mailbox.next(&child.rest()) else {
            panic!("human input")
        };
        let prompt = child.admit(input).unwrap();
        assert!(child.mailbox.begin());
        assert!(child.handle().status().working());
        assert!(!child.handle().status().owes_parent);
        assert!(parent.mailbox.drain().is_empty());
        assert!(parent.handle().status().job.is_none());
        set_provider(
            &child,
            scripted("test-model", Script::new().then(reply("2"))),
        );
        let outcome = child.exchange(&exchange, prompt, &child_emit);
        settle(&mut child, outcome);
        assert!(matches!(
            child.handle().status().report,
            Some(ChildStatus::Replied(FOValue::Int { value: 2 }))
        ));
        assert!(!child.handle().status().working());
        assert!(parent.mailbox.drain().is_empty());
        assert!(parent.rendered_messages().is_empty());
        assert!(
            !crate::bus::drain_records(&rx).iter().any(|record| matches!(
                record,
                Record::Forensic(Forensic::JobOpened)
                    | Record::Display(Display::SubagentDone { .. })
            )),
            "the parent's recorder receives nothing about human-only work"
        );
    }
}

#[test]
fn stop_recalls_human_text_but_helpers_wake_the_same_parent_job() {
    let mut parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*helper).clone();
    parent.body.state.nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            wait_until("parent teardown ends helper", || !helper.is_live());
            drop(helper);
            Teardown { failures: vec![] }
        })),
    );
    parent.handle().steer("work".into()).unwrap();
    let Next::Take { input, exchange } = parent.mailbox.next(&parent.rest()) else {
        panic!("work")
    };
    assert!(parent.admit(input).is_some());
    assert!(parent.mailbox.begin());
    let job = parent.handle().status().job.unwrap();
    parent.handle().steer("queued one".into()).unwrap();
    parent.handle().steer("queued two".into()).unwrap();
    assert_eq!(
        parent.handle().stop(job),
        Some(vec!["queued one\nqueued two".into()])
    );
    assert!(exchange.is_cancelled());
    assert!(handle.is_live());
    assert!(handle.status().owes_parent);
    handle.report(ChildStatus::Replied(FOValue::Int { value: 42 }));
    let Next::Take {
        input,
        exchange: resumed,
    } = parent.mailbox.next(&parent.rest())
    else {
        panic!("helper wakes parent")
    };
    let prompt = parent.admit(input).unwrap();
    assert!(parent.mailbox.begin());
    assert_eq!(parent.handle().status().job, Some(job));
    assert!(!resumed.is_cancelled());
    set_provider(
        &parent,
        scripted(
            "test-model",
            Script::new().then(Reply::text("read helper report")),
        ),
    );
    let (emit, _rx) = emitter(&parent);
    let outcome = parent.exchange(&resumed, prompt, &emit);
    assert!(matches!(outcome, AgentOutcome::Failed(_)));
    assert!(
        serde_json::to_string(&parent.rendered_messages())
            .unwrap()
            .contains("helper")
    );
    assert!(matches!(
        parent.mailbox.next(&parent.rest()),
        Next::Settled { stopped: true }
    ));
}

#[test]
fn a_stopped_helper_reports_stopped_even_after_a_later_exchange_runs() {
    for followup in [false, true] {
        let parent = trunk(Trunk::Converses);
        let mut child = parent.fork_named("helper").unwrap();
        assert!(child.mailbox.begin());
        child.handle().steer("first".into()).unwrap();
        let Next::Take { input, exchange } = child.mailbox.next(&child.rest()) else {
            panic!("first input")
        };
        assert!(child.admit(input).is_some());
        let job = child.handle().status().job.unwrap();
        if followup {
            child
                .handle()
                .message(&parent.handle(), "queued follow-up".into())
                .unwrap();
        }
        assert_eq!(child.handle().stop(job), Some(vec![]));
        assert!(exchange.is_cancelled());
        let (emit, _rx) = emitter(&child);
        let outcome = if followup {
            set_provider(
                &child,
                scripted(
                    "test-model",
                    Script::new().then(Reply::error(ProviderError::Other("no reply".into()))),
                ),
            );
            let Next::Take { input, exchange } = child.mailbox.next(&child.rest()) else {
                panic!("Stop preserves the queued parent message")
            };
            assert!(matches!(input, Input::Message(_)));
            let prompt = child.admit(input).unwrap();
            assert!(child.mailbox.begin());
            assert!(!exchange.is_cancelled());
            child.body.state.readings.nudges.reset();
            child.exchange(&exchange, prompt, &emit)
        } else {
            AgentOutcome::Cancelled
        };
        settle(&mut child, outcome);
        assert!(matches!(next_child(&parent), (_, ChildStatus::Stopped)));
        assert!(matches!(
            child.handle().status().report,
            Some(ChildStatus::Stopped)
        ));
    }
}

#[test]
fn ending_a_parent_joins_helper_engines_before_ending_its_own() {
    let mut parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    let helper = child.handle();
    let crate::agent::engine::EngineKind::Identity(parent_engine) = parent.engine().kind() else {
        panic!("identity")
    };
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ended = observed.clone();
    parent.body.state.nursery.adopt(
        helper.clone(),
        Joined::new(std::thread::spawn(move || {
            wait_until("parent life did not end helper", || {
                !child.handle().is_live()
            });
            let mut child = child;
            let teardown = child.teardown();
            assert!(
                parent_engine.session_ended().unwrap().is_none(),
                "parent engine must outlive the helper's teardown"
            );
            ended.store(true, std::sync::atomic::Ordering::Release);
            teardown
        })),
    );
    let crate::agent::engine::EngineKind::Identity(engine) = parent.engine().kind() else {
        panic!("identity")
    };
    let teardown = parent.teardown();
    assert_eq!(teardown.failures, Vec::<String>::new());
    assert!(observed.load(std::sync::atomic::Ordering::Acquire));
    assert!(!helper.is_live());
    assert!(engine.session_ended().unwrap().is_some());
    assert!(parent.body.state.nursery.is_joined(helper.id()));
}

#[test]
fn gone_joins_failures_without_recording_them_again_in_the_parent() {
    let mut parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let id = helper.id();
    parent.body.state.nursery.adopt(
        (*helper).clone(),
        Joined::new(std::thread::spawn(move || {
            drop(helper);
            Teardown {
                failures: vec!["helper's certified failure".into()],
            }
        })),
    );
    assert!(parent.admit(Input::Gone(id)).is_none());
    assert!(parent.body.state.nursery.is_joined(id));
    assert_eq!(
        parent.body.state.failures,
        vec!["helper's certified failure"]
    );
    assert!(
        parent
            .admit(Input::Child {
                id,
                name: Arc::from("helper"),
                status: ChildStatus::Stopped,
                elapsed: Duration::ZERO
            })
            .is_none()
    );
    let teardown = parent.teardown();
    assert_eq!(teardown.failures, vec!["helper's certified failure"]);
    assert!(
        !read_records(&parent.log_dir().path().join(RECORD))
            .unwrap()
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::TeardownFailed { .. })))
    );
}

#[test]
fn a_stopped_notice_has_the_new_settlement_not_a_legacy_failure() {
    let mut parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    let (emit, rx) = emitter(&parent);
    parent.couple(&emit);
    child.handle().report(ChildStatus::Stopped);
    let Next::Take { input, .. } = parent.mailbox.next(&parent.rest()) else {
        panic!("report")
    };
    let prompt = parent.admit(input).unwrap();
    assert!(
        prompt.text.contains("message it to continue"),
        "{}",
        prompt.text
    );
    assert!(crate::bus::drain_records(&rx).iter().any(|record| matches!(
        record,
        Record::Display(Display::SubagentDone {
            outcome: Settlement::Stopped,
            ..
        })
    )));
}

#[test]
fn an_uncertified_helper_end_is_recorded_once_and_travels_in_parent_teardown() {
    use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry};
    use std::borrow::Cow;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let mut parent = Agent::for_test_with(TestTrunk {
        installers: dressed(|_| {}),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let release = Release(Arc::new(AtomicBool::new(false)));
    let ready = release.0.clone();
    let held: Arc<[BuiltinEntry]> = Arc::new([BuiltinEntry::new(
        Cow::Borrowed("test-uncertified-helper-worker"),
        scheme_test_clear_block_forever,
        "test-only: hold a worker past engine teardown.",
        BuiltinBody::Captured(Arc::new(move |_, _, shell| {
            loop {
                if ready.load(Ordering::Acquire) {
                    shell.poll()?;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })),
    )]);
    let _installers = dressed_over(
        move |runtime| runtime.install_captured_builtins(&held),
        |shell| shell.set_worker_drain_grace(Duration::from_millis(20)),
    );
    let mut child = parent.fork_named("helper").unwrap();
    let log = child.log_dir().path().join(RECORD);
    let (emit, _rx) = emitter(&parent);
    let result = child.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"held\" { test-uncertified-helper-worker }",
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    let entries = workers(&child);
    assert_eq!(entries.len(), 1);
    let helper = launch_child(&mut parent, child, None, &emit);
    let teardown = parent.teardown();
    assert_eq!(teardown.failures.len(), 1);
    assert!(teardown.failures[0].contains("ended without certified engine teardown"));
    assert!(!helper.is_live());
    let records = read_records(&log).unwrap();
    let failures: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Forensic(Forensic::TeardownFailed { failure }) => Some(failure),
            _ => None,
        })
        .collect();
    assert_eq!(failures, vec![&teardown.failures[0]]);
    assert!(
        !read_records(&parent.log_dir().path().join(RECORD))
            .unwrap()
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::TeardownFailed { .. })))
    );
    release.0.store(true, Ordering::Release);
    wait_until("held worker did not finish after release", || {
        entries
            .iter()
            .all(|entry| entry.handle.sample().has_ended())
    });
}

#[test]
fn a_job_cannot_settle_while_an_ended_helper_is_unjoined() {
    let mut parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*helper).clone();
    parent.body.state.nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            wait_until("helper ends", || !helper.is_live());
            drop(helper);
            Teardown { failures: vec![] }
        })),
    );
    assert!(parent.mailbox.begin());
    handle.report(ChildStatus::Replied(FOValue::Unit));
    let _report = next_child(&parent);
    let sampled = parent.rest();
    assert!(!sampled.pending());
    handle.end();
    assert!(sampled.pending(), "a snapshot reads each life afresh");
    let parent_handle = parent.handle();
    let owner_address = parent_handle.0.address.clone();
    let helper_id = handle.id();
    let _timeout =
        ral_core::process::arm_callback(Duration::from_secs(10), move || parent_handle.end());
    let owner = std::thread::spawn(move || {
        let Next::Take { input, .. } = parent.mailbox.next(&sampled) else {
            panic!("Gone must precede settlement")
        };
        assert!(matches!(input, Input::Gone(id) if id == helper_id));
        assert!(parent.admit(input).is_none());
        assert!(parent.body.state.nursery.is_joined(helper_id));
        assert!(matches!(
            parent.mailbox.next(&parent.rest()),
            Next::Settled { stopped: false }
        ));
    });
    // `next` can return only what was posted: Gone, never a settlement.
    owner_address.post(Input::Gone(helper_id)).unwrap();
    owner.join().unwrap();
}

#[test]
fn read_uses_a_helpers_published_status_even_before_its_report_is_taken_up() {
    let mut parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*helper).clone();
    parent.body.state.nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            wait_until("parent must end the helper", || !helper.is_live());
            drop(helper);
            Teardown { failures: vec![] }
        })),
    );
    let (emit, _rx) = emitter(&parent);
    let working = parent.ral(
        &DurableRoot::default().child(),
        "exarch-agents `read 'helper'",
        5,
        &emit,
    );
    assert!(working.failed);
    assert!(working.text.contains("working"), "{}", working.text);
    handle.report(ChildStatus::Replied(FOValue::String {
        value: "published answer".into(),
    }));
    let answer = parent.ral(
        &DurableRoot::default().child(),
        "exarch-agents `read 'helper'",
        5,
        &emit,
    );
    assert!(!answer.failed, "{}", answer.text);
    assert!(answer.text.contains("published answer"), "{}", answer.text);
    assert!(matches!(next_child(&parent), (_, ChildStatus::Replied(_))));
}

#[test]
fn clear_joins_a_retiring_helper_before_discarding_its_notification() {
    let mut parent = trunk(Trunk::Converses);
    let helper = test_agent(
        &parent.fleet,
        TestAgentSpec::child("helper", &parent.handle(), parent.mailbox.life().child()),
    )
    .unwrap();
    let handle = (*helper).clone();
    let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = joined.clone();
    let (release, waiting) = std::sync::mpsc::channel();
    parent.body.state.nursery.adopt(
        handle.clone(),
        Joined::new(std::thread::spawn(move || {
            waiting.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(helper);
            seen.store(true, std::sync::atomic::Ordering::Release);
            Teardown { failures: vec![] }
        })),
    );
    handle.end();
    release.send(()).unwrap();
    parent.clear().unwrap();
    assert!(
        joined.load(std::sync::atomic::Ordering::Acquire),
        "clear returned before the retiring helper's thread finished"
    );
    assert!(parent.mailbox.drain().is_empty());
}

#[test]
fn a_helper_writer_failure_at_teardown_failed_reaches_died_and_parent() {
    use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry};
    use std::borrow::Cow;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let mut parent = Agent::for_test_with(TestTrunk {
        installers: dressed(|_| {}),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let release = Release(Arc::new(AtomicBool::new(false)));
    let ready = release.0.clone();
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let held: Arc<[BuiltinEntry]> = Arc::new([BuiltinEntry::new(
        Cow::Borrowed("test-uncertified-writer-worker"),
        scheme_test_clear_block_forever,
        "test-only: hold a worker past engine teardown.",
        BuiltinBody::Captured(Arc::new(move |_, _, shell| {
            worker_started.store(true, Ordering::Release);
            loop {
                if ready.load(Ordering::Acquire) {
                    shell.poll()?;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })),
    )]);
    let _installers = dressed_over(
        move |runtime| runtime.install_captured_builtins(&held),
        |shell| shell.set_worker_drain_grace(Duration::from_millis(20)),
    );
    let mut child = parent.fork_named("helper").unwrap();
    let (emit, rx) = emitter(&parent);
    let result = child.ral(
        &DurableRoot::default().child(),
        "let h = spawn \"held\" { test-uncertified-writer-worker }",
        5,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    let entries = workers(&child);
    assert_eq!(entries.len(), 1);
    wait_until("held worker did not start", || {
        started.load(Ordering::Acquire)
    });
    child.report(ChildStatus::Replied(FOValue::Unit));
    let recorder = child.recorder();
    recorder.fail_after(0);
    let helper = launch_child(&mut parent, child, None, &emit);
    let failures = parent.teardown().failures;
    assert_eq!(failures.len(), 2);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("ended without certified engine teardown"))
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("injected record write failure"))
    );
    assert!(rx.drain_pending().iter().any(|signal| matches!(signal,
        Signal::Transient(id, Transient::Died { failures: reported }) if *id == helper.id() && *reported == failures
    )));
    release.0.store(true, Ordering::Release);
    wait_until("held worker did not finish after release", || {
        entries
            .iter()
            .all(|entry| entry.handle.sample().has_ended())
    });
}

#[test]
fn a_parent_messaging_at_the_idle_deadline_gets_a_report_or_a_refusal_never_silence() {
    let idle = Duration::from_millis(25);
    let mut parent = Agent::for_test_with(TestTrunk {
        helper_idle: idle,
        ..TestTrunk::new("system")
    })
    .unwrap();
    let child = parent.fork_named("helper").unwrap();
    set_provider(
        &child,
        scripted(
            "test-model",
            (0..200).fold(Script::new(), |script, _| script.then(reply("1"))),
        ),
    );
    let (emit, _rx) = emitter(&parent);
    let helper = launch_child(&mut parent, child, Some("work".into()), &emit);
    assert!(matches!(next_child(&parent), (_, ChildStatus::Replied(_))));
    let parent_handle = parent.handle();
    let mut accepted = 0;
    while helper.message(&parent_handle, "again".into()).is_ok() {
        accepted += 1;
        assert!(accepted < 200, "the helper never timed out");
        assert!(
            matches!(next_child(&parent), (_, ChildStatus::Replied(_))),
            "an accepted message owes a report"
        );
        std::thread::sleep(idle);
    }
    assert!(matches!(
        helper.message(&parent_handle, "late".into()),
        Err(Refused::Ended)
    ));
    assert!(matches!(
        parent.mailbox.next(&parent.rest()),
        Next::Take { input: Input::Gone(id), .. } if id == helper.id()
    ));
    assert!(parent.fleet.resolve("helper").is_none());
}

#[test]
fn dropping_an_unrun_parent_joins_its_working_helper_before_ending_its_engine() {
    let root = Agent::for_test_with(TestTrunk {
        installers: dressed(|runtime| runtime.install_builtins(WORKER_REGISTRY_TEST_BUILTINS)),
        ..TestTrunk::new("system")
    })
    .unwrap();
    let mut parent = forked_child(&root, "parent", "`inherit");
    let grandchild = forked_child(&parent, "grandchild", "`inherit");
    set_provider(
        &grandchild,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    let (emit, _rx) = emitter(&root);
    let crate::agent::engine::EngineKind::Identity(grand_engine) = grandchild.engine().kind()
    else {
        panic!("identity")
    };
    let grandchild = launch_child(&mut parent, grandchild, Some("work".into()), &emit);
    wait_until("grandchild did not start work", || {
        grandchild.status().job.is_some()
    });
    let crate::agent::engine::EngineKind::Identity(engine) = parent.engine().kind() else {
        panic!("identity")
    };
    let order = {
        let (engine, grand_engine) = (engine.clone(), grand_engine.clone());
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let parent_ended = engine.session_ended().unwrap().is_some();
                let grandchild_ended = grand_engine.session_ended().unwrap().is_some();
                if parent_ended {
                    return grandchild_ended;
                }
                assert!(Instant::now() < deadline, "the parent never ended");
                std::thread::yield_now();
            }
        })
    };
    drop(parent);
    assert!(!grandchild.is_live());
    assert!(root.fleet.resolve("grandchild").is_none());
    assert!(grand_engine.session_ended().unwrap().is_some());
    assert!(engine.session_ended().unwrap().is_some());
    assert!(
        order.join().unwrap(),
        "the parent's engine ended before its helper's"
    );
}
