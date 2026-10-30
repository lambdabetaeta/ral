use super::*;
use crate::agent::{Admission, AgentHandle, TestTrunk};
use crate::record::AgentState;

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "agent did not reach the expected state"
        );
        std::thread::yield_now();
    }
}

fn blocking_root() -> Agent {
    let mut root = testkit::dressed_trunk(|runtime| {
        runtime.install_builtins(testkit::WORKER_REGISTRY_TEST_BUILTINS);
    });
    set_provider(
        &mut root,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "block",
                "test-clear-block-forever",
            )])),
        ),
    );
    root
}

#[test]
fn headless_reply_runs_through_the_host_feed_in_both_formats() {
    const CHILD: &str = "EXARCH_HEADLESS_REPLY_CHILD";
    const EXPECTED_LOG: &str = "Expected root log: ";
    if std::env::var_os(CHILD).is_none() {
        // Capture process stdio without redirecting parallel tests' writers.
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "headless::tests::host::headless_reply_runs_through_the_host_feed_in_both_formats",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run the headless reply test in isolation");
        let stdout = String::from_utf8(output.stdout).expect("test stdout is UTF-8");
        let stderr = String::from_utf8(output.stderr).expect("headless stderr is UTF-8");
        assert!(output.status.success(), "{stdout}\n{stderr}");
        let expected: Vec<_> = stdout
            .lines()
            .filter_map(|line| line.strip_prefix(EXPECTED_LOG))
            .collect();
        let actual: Vec<_> = stderr
            .lines()
            .filter_map(|line| line.strip_prefix("Agent log: "))
            .collect();
        assert_eq!(expected.len(), 2, "one root directory per format: {stdout}");
        assert_eq!(actual, expected, "{stderr}");
        return;
    }
    for format in [OutputFormat::Text, OutputFormat::Json] {
        let host = frontend_host(reply_script("[answer: 42]"), Trunk::Returns);
        let root_handle = host
            .fleet()
            .registry
            .get(host.roots()[0])
            .expect("a live root");
        println!("\n{EXPECTED_LOG}{}", root_handle.log_dir());
        run_frontend(&host, format).expect("a deliberate reply succeeds");
        assert!(matches!(
            root_handle.status().expect("the cell is available").report,
            Some(ChildStatus::Replied(FOValue::Map { .. }))
        ));
        assert_eq!(host.end(), [] as [String; 0]);
    }
}

#[test]
fn headless_no_reply_runs_through_the_host_feed_in_both_formats() {
    for format in [OutputFormat::Text, OutputFormat::Json] {
        let host = frontend_host(no_reply_script(), Trunk::Returns);
        let root_handle = host
            .fleet()
            .registry
            .get(host.roots()[0])
            .expect("a live root");
        assert_eq!(
            run_frontend(&host, format),
            Err("ended without calling `reply`".into())
        );
        assert_eq!(
            root_handle.status().expect("the cell is available").report,
            Some(ChildStatus::Failed("ended without calling `reply`".into()))
        );
        assert_eq!(host.end(), [] as [String; 0]);
    }
}

#[test]
fn headless_stopped_runs_through_the_host_feed_in_both_formats() {
    for format in [OutputFormat::Text, OutputFormat::Json] {
        let host = testkit::host(blocking_root());
        let root_handle = host
            .fleet()
            .registry
            .get(host.roots()[0])
            .expect("a live root");
        let handle = root_handle.clone();
        let watchdog = ral_core::process::arm_callback(WAIT, {
            let handle = handle.clone();
            move || handle.end()
        });
        let stopper = std::thread::spawn(move || {
            until(|| {
                handle
                    .status()
                    .expect("the cell is available")
                    .job
                    .is_some()
            });
            let job = handle
                .status()
                .expect("the cell is available")
                .job
                .expect("the blocking exchange stays open");
            assert_eq!(handle.stop(job), Some(Vec::new()));
        });
        assert_eq!(
            run_frontend(&host, format),
            Err("the agent was stopped".into())
        );
        stopper.join().unwrap();
        drop(watchdog);
        assert_eq!(
            root_handle.status().expect("the cell is available").report,
            Some(ChildStatus::Stopped)
        );
        assert!(root_handle.is_live(), "Stop does not end the root's life");
        assert_eq!(host.end(), [] as [String; 0]);
    }
}

/// Drive the frontend over `root` after a recorded channel fault, while
/// `interfere` plays the outside world; the verdict it returns.
fn verdict_after_fault(
    root: Agent,
    fault: Option<&str>,
    interfere: impl FnOnce(AgentHandle) + Send + 'static,
) -> Result<(), String> {
    let recorder = root.recorder();
    let host = testkit::host(root);
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    wait_ready(&host);
    if let Some(text) = fault {
        recorder.transient(Transient::Fault { text: text.into() });
    }
    let handle = root_handle;
    let watchdog = ral_core::process::arm_callback(WAIT, {
        let handle = handle.clone();
        move || handle.end()
    });
    let outside = std::thread::spawn(move || interfere(handle));
    let verdict = run_frontend(&host, OutputFormat::Json);
    outside.join().unwrap();
    drop(watchdog);
    assert_eq!(host.end(), [] as [String; 0]);
    verdict
}

#[test]
fn a_report_takes_precedence_over_a_fault_and_a_fault_over_the_missing_reply() {
    let fault = Some("earlier channel fault");
    let refused = Script::new().then(Reply::error(crate::provider::ProviderError::Other(
        "request refused".into(),
    )));
    assert_eq!(
        verdict_after_fault(
            frontend_root(reply_script("42"), Trunk::Returns),
            fault,
            |_| {}
        ),
        Ok(())
    );
    assert_eq!(
        verdict_after_fault(frontend_root(refused, Trunk::Returns), fault, |_| {}),
        Err("request refused".to_string())
    );
    assert_eq!(
        verdict_after_fault(blocking_root(), fault, |handle| {
            until(|| {
                handle
                    .status()
                    .expect("the cell is available")
                    .job
                    .is_some()
            });
            assert!(handle.stop_current());
        }),
        Err("the agent was stopped".to_string())
    );
    for returns in [true, false] {
        for fault in [None, Some("channel lost its facts")] {
            let gated = Agent::for_test_with(TestTrunk {
                trunk: if returns {
                    Trunk::Returns
                } else {
                    Trunk::Converses
                },
                admission: Admission::Gated,
                ..TestTrunk::new("system")
            })
            .unwrap();
            assert_eq!(
                verdict_after_fault(gated, fault, |handle| {
                    until(|| !handle.queued().is_empty());
                    handle.end();
                }),
                Err(if returns {
                    "ended before reporting"
                } else {
                    fault.unwrap_or("the agent ended without a reply")
                }
                .to_string())
            );
        }
    }
}

#[test]
fn converse_carries_context_from_one_job_into_the_next() {
    let host = frontend_host(
        Script::new()
            .then(Reply::text("hello back"))
            .then(Reply::tool_calls(vec![ral_call(
                "read-context",
                r#"let material = exarch-transcript `read [turns: [1, 2]]
                   let say-part = { |p| case $p [
                     `text: { |[content: c]| echo "text=$c" },
                     `program: { |_| echo "part=program" },
                     `result: { |_| echo "part=result" },
                     `reasoning: { |_| echo "part=reasoning" },
                     `binary: { |_| echo "part=binary" },
                     `custom: { |_| echo "part=custom" },
                   ] }
                   say-part $material[0][messages][0][parts][0]
                   say-part $material[1][messages][0][parts][0]"#,
            )]))
            .then(Reply::text("and again")),
        Trunk::Converses,
    );
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    root_handle.steer("first message".into()).unwrap();
    let first = wait_settled(&host);
    assert_eq!(
        root_handle.status().expect("the cell is available").report,
        None
    );
    assert!(root_handle.is_live());
    assert!(!first.iter().any(|signal| {
        has_fact(signal, root_handle.id(), |record| {
            matches!(record, Record::Control(Control::Status { .. }))
        })
    }));
    root_handle.steer("second message".into()).unwrap();
    let second = wait_settled(&host);
    let context = second
        .iter()
        .filter_map(|signal| match signal {
            Signal::Commit(_, commit) => Some(commit.records()),
            _ => None,
        })
        .flatten()
        .filter_map(|rec| match rec.value() {
            Record::Transcript(Transcript::ToolResults { results }) => Some(results),
            _ => None,
        })
        .flatten()
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(context.contains("text=first message"), "{context}");
    assert!(context.contains("text=hello back"), "{context}");
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn ordinary_failed_conversation_remains_usable_for_the_next_job() {
    let host = frontend_host(
        Script::new()
            .then(Reply::error(crate::provider::ProviderError::Other(
                "request refused".into(),
            )))
            .then(Reply::text("next job succeeds")),
        Trunk::Converses,
    );
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    root_handle.steer("first".into()).unwrap();
    wait_settled(&host);
    assert!(root_handle.is_live());
    root_handle.steer("next".into()).unwrap();
    let next = wait_settled(&host);
    assert!(next.iter().any(|signal| {
        has_fact(signal, root_handle.id(), |record| {
            matches!(record, Record::Display(Display::Answer { text }) if text == "next job succeeds")
        })
    }));
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn an_ended_root_refuses_the_headless_prompt() {
    let host = frontend_host(Script::new(), Trunk::Returns);
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    root_handle.end();
    assert_eq!(
        run_frontend(&host, OutputFormat::Text),
        Err("the agent ended before it could take the prompt".into())
    );
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn a_helper_without_a_reply_reports_failure_before_the_parent_settles() {
    let mut root = frontend_root(
        Script::new()
            .then(Reply::text("waiting for help"))
            .then(Reply::text("thanks for the update")),
        Trunk::Converses,
    );
    let mut child = testkit::forked_child(&root, "flaky", "`inherit");
    set_provider(&mut child, scripted("test-model", no_reply_script()));
    let (emit, _unused_feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    let host = testkit::host(root);
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    let root_id = root_handle.id();
    root_handle.steer("please help".into()).unwrap();
    receive_until(&host, |signal| {
        matches!(signal, Signal::Transient(id, Transient::State(AgentState::WaitingOnAgents))
            if *id == root_id)
    });
    helper.steer("go".into()).unwrap();
    let signals = wait_settled(&host);
    assert!(signals.iter().any(|signal| {
        has_fact(signal, root_id, |record| {
            matches!(record, Record::Display(Display::SubagentDone {
                name, outcome: Settlement::Failed { reason }, ..
            }) if name == "flaky" && reason == "ended without calling `reply`")
        })
    }));
    assert!(
        helper.is_live(),
        "an answered helper rests rather than ending"
    );
    assert_eq!(host.end(), [] as [String; 0]);
    assert!(!helper.is_live());
}

#[test]
fn stop_keeps_helpers_working_and_their_report_in_the_same_job() {
    let mut root = frontend_root(
        Script::new()
            .then(Reply::text("waiting for help"))
            .then(Reply::text("thanks for the update")),
        Trunk::Converses,
    );
    let mut child = testkit::forked_child(&root, "helper", "`inherit");
    set_provider(&mut child, scripted("test-model", reply_script("42")));
    let (emit, _unused_feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    let host = testkit::host(root);
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    let root = root_handle;
    root.steer("please help".into()).unwrap();
    let opened = receive_until(&host, |signal| {
        matches!(signal, Signal::Transient(id, Transient::State(AgentState::WaitingOnAgents))
            if *id == root.id())
    });
    let job = root
        .status()
        .expect("the cell is available")
        .job
        .expect("pending helper keeps the job open");
    assert_eq!(root.stop(job), Some(Vec::new()));
    assert!(helper.is_live(), "Stop cancels no helper life");
    helper.steer("finish your task".into()).unwrap();
    let tail = wait_settled(&host);
    assert!(tail.iter().any(|signal| {
        has_fact(signal, root.id(), |record| {
            matches!(record, Record::Display(Display::SubagentDone {
                name, outcome: Settlement::Replied, ..
            }) if name == "helper")
        })
    }));
    assert!(tail.iter().any(|signal| {
        has_fact(signal, root.id(), |record| {
            matches!(record, Record::Control(Control::JobSettled { job: settled, stopped: true })
                if *settled == job)
        })
    }));
    assert_eq!(
        opened
            .iter()
            .chain(&tail)
            .filter(|signal| {
                has_fact(signal, root.id(), |record| {
                    matches!(record, Record::Control(Control::JobOpened))
                })
            })
            .count(),
        1
    );
    assert_eq!(
        helper.status().expect("the cell is available").report,
        Some(ChildStatus::Replied(FOValue::Int { value: 42 }))
    );
    assert_eq!(
        root.stop(job),
        None,
        "a settled job cannot be stopped again"
    );
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn host_end_joins_other_roots_and_helpers_and_releases_their_names() {
    let mut root = frontend_root(Script::new(), Trunk::Converses);
    let other = crate::agent::spawn::hatch_root(
        &root,
        crate::record::Memory::Amnemon,
        Some("other-root".into()),
    )
    .unwrap();
    let child = testkit::forked_child(&root, "helper", "`inherit");
    let (emit, _unused_feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    let fleet = root.fleet.clone();
    let handles = [root.handle(), other.handle(), helper];
    let host = Host::adopt(vec![(root, None), (other, None)]).unwrap();
    assert_eq!(host.end(), [] as [String; 0]);
    assert!(handles.iter().all(|handle| !handle.is_live()));
    assert!(
        fleet.registry.is_empty(),
        "end returns only after every name leaves"
    );
    for handle in handles {
        assert!(handle.steer("too late".into()).is_err());
    }
}

#[test]
fn a_stale_stop_leaves_the_next_jobs_queue_untouched() {
    let mut root = Agent::for_test_with(TestTrunk {
        trunk: Trunk::Converses,
        admission: Admission::Gated,
        ..TestTrunk::new("system")
    })
    .unwrap();
    set_provider(
        &mut root,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::text("done"))
                .then(Reply::text("again")),
        ),
    );
    let host = testkit::host(root);
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    let handle = root_handle;
    handle.steer("first".into()).unwrap();
    assert!(handle.release());
    let first = wait_settled(&host);
    assert_eq!(
        handle.status().expect("the cell is available").report,
        None,
        "a conversing root returns no report"
    );
    let job = first
        .iter()
        .filter_map(|signal| match signal {
            Signal::Commit(id, commit) if *id == handle.id() => Some(commit.records()),
            _ => None,
        })
        .flatten()
        .find_map(|record| match record.value() {
            Record::Control(Control::JobSettled { job, .. }) => Some(*job),
            _ => None,
        })
        .expect("the first job settled");
    handle.steer("next".into()).unwrap();
    assert_eq!(handle.stop(job), None);
    assert_eq!(handle.queued(), ["next"]);
    assert!(handle.release());
    let next = wait_settled(&host);
    assert!(next.iter().any(|signal| has_fact(signal, handle.id(), |record| matches!(record,
        Record::Control(Control::JobSettled { job: next, stopped: false }) if next.get() > job.get()
    ))));
    assert_eq!(host.end(), [] as [String; 0]);
}

#[test]
fn a_reused_helper_keeps_its_thread_and_live_feed() {
    let mut parent_script = Script::new()
        .then(Reply::text("waiting for help"))
        .then(Reply::text("thanks for the first reply"))
        .then(Reply::tool_calls(vec![ral_call(
            "follow-up",
            "exarch-agents `message [to: 'helper', text: 'again']",
        )]));
    for _ in 0..3 {
        parent_script = parent_script.then(Reply::text("thanks for the second reply"));
    }
    let mut root = frontend_root(parent_script, Trunk::Converses);
    let mut child = testkit::forked_child(&root, "helper", "`inherit");
    set_provider(
        &mut child,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call(
                    "first",
                    "exarch-agents `reply 1",
                )]))
                .then(Reply::tool_calls(vec![ral_call(
                    "again",
                    "exarch-agents `reply 2",
                )])),
        ),
    );
    let (emit, child_feed) = crate::bus::dummy_emitter();
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    let host = testkit::host(root);
    let root_handle = host
        .fleet()
        .registry
        .get(host.roots()[0])
        .expect("a live root");
    let root_id = root_handle.id();
    root_handle.steer("first".into()).unwrap();
    receive_until(&host, |signal| {
        matches!(signal,
            Signal::Transient(id, Transient::State(AgentState::WaitingOnAgents)) if *id == root_id
        )
    });
    helper.steer("help".into()).unwrap();
    wait_settled(&host);
    assert_eq!(
        helper.status().expect("the cell is available").report,
        Some(ChildStatus::Replied(FOValue::Int { value: 1 }))
    );
    root_handle.steer("follow up".into()).unwrap();
    wait_settled(&host);
    assert_eq!(
        helper.status().expect("the cell is available").report,
        Some(ChildStatus::Replied(FOValue::Int { value: 2 }))
    );
    assert!(helper.is_live());
    let signals: Vec<_> = std::iter::from_fn(|| child_feed.try_recv().ok()).collect();
    assert_eq!(
        signals
            .iter()
            .filter(|signal| matches!(signal,
                Signal::Born(id, _) if *id == helper.id()
            ))
            .count(),
        1
    );
    for call in ["first", "again"] {
        assert!(
            signals
                .iter()
                .any(|signal| has_fact(signal, helper.id(), |record| {
                    matches!(record, Record::Transcript(Transcript::ToolResults { results })
                if results.iter().any(|result| result.id == call))
                })),
            "both exchanges use the original helper feed: {call}"
        );
    }
    assert!(
        signals
            .iter()
            .any(|signal| has_fact(signal, helper.id(), |record| {
                matches!(record, Record::Forensic(Forensic::UsageDelta { .. }))
            }))
    );
    assert_eq!(host.end(), [] as [String; 0]);
}
