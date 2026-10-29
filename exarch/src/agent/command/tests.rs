use crate::agent::{Admission, Agent, TestTrunk};
use crate::bus::{Command, Emitter, Read, Rewrite};
use crate::record::{Forensic, Record};

use crate::agent::Trunk;
use crate::agent::testkit::{self, ral_call, scripted, set_provider};
use crate::host::Host;
use crate::port::{Port, Signal};
use crate::provider::scripted::{Reply, Script};
use crate::provider::{Account, Bureau, Choice, Login, Pick, Tuning, Via};
use crate::record::{AgentId, ChildStatus, Protocol, Voice};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn commands_after_a_display_first_failure_do_not_open_or_rewrite_sessions() {
    let session = Agent::for_test("system").unwrap();
    let recorder = session.recorder();
    recorder.fail_after(3);
    recorder
        .emit(crate::record::Display::Answer {
            text: "failed display".into(),
        })
        .unwrap_err();
    let dir = session.log_dir().clone();
    let _fleet = session.fleet.clone();
    let sessions = session.fleet.launch.sessions_root.clone();
    let id = session.id();
    let handle = session.handle();
    let before = dir.read("record.jsonl").unwrap();
    let _posted = handle.command(Command::Read(Read::Hatch {
        memory: crate::record::Memory::Amnemon,
        name: Some("must-not-open".into()),
    }));
    let _posted = handle.command(Command::Rewrite(Rewrite::Clear));
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, id);
    let _teardown = session.run(&emit);
    assert!(!handle.is_live());
    assert_eq!(dir.read("record.jsonl").unwrap(), before);
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|signal| matches!(signal, crate::bus::Signal::Born(..)))
    );
    assert_eq!(
        crate::record::AgentLog::session_ids(&sessions).unwrap(),
        vec![id]
    );
}

#[test]
fn resources_command_runs_at_gated_rest_once_without_opening_a_job() {
    let session = Agent::for_test_with(TestTrunk {
        admission: Admission::Gated,
        ..TestTrunk::new("system")
    })
    .unwrap();
    let _posted = session.handle().command(Command::Read(Read::Resources));
    let _posted = session.handle().command(Command::Rewrite(Rewrite::Close));
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    assert_eq!(session.run(&emit).failures, [] as [String; 0]);
    let mut folds = Vec::new();
    let mut records = Vec::new();
    for signal in std::iter::from_fn(|| rx.try_recv().ok()) {
        match signal {
            crate::bus::Signal::Transient(_, crate::record::Transient::Resources { card }) => {
                folds.push(card);
            }
            crate::bus::Signal::Commit(_, commit) => records.extend(
                commit
                    .into_records()
                    .into_iter()
                    .map(crate::record::Recorded::into_value),
            ),
            _ => {}
        }
    }
    assert_eq!(folds.len(), 1);
    let card = &folds[0];
    assert_eq!(card.marks().len(), 2);
    let crate::card::Mark::Fields { rows } = &card.marks()[1] else {
        panic!("the agent's resource matrix")
    };
    assert!(rows.iter().any(|row| row.label == "workers.running"));
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Forensic(Forensic::JobOpened | Forensic::TurnStarted { .. })
    )));
}

#[test]
fn rewind_past_the_last_turn_reports_the_bad_anchor() {
    let session = Agent::for_test("system").unwrap();
    let _posted = session
        .handle()
        .command(Command::Rewrite(Rewrite::Rewind(7)));
    let _posted = session.handle().command(Command::Rewrite(Rewrite::Close));
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    assert_eq!(session.run(&emit).failures, [] as [String; 0]);
    let records = crate::bus::drain_records(&rx);
    assert!(records.iter().any(|record| matches!(record,
        Record::Forensic(Forensic::Error { text }) if text.contains("turn 7"))));
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Forensic(Forensic::JobOpened | Forensic::TurnStarted { .. })
    )));
}

const WAIT: Duration = Duration::from_secs(10);

fn with_minting(mut session: Agent, scripts: Vec<Script>) -> Agent {
    Arc::get_mut(&mut session.fleet)
        .expect("the unlaunched root alone owns its fleet")
        .launch
        .bureau = Arc::new(Bureau::scripted(scripts));
    session
}

fn routed_pick() -> Pick {
    Pick {
        choice: Choice {
            account: Account::built_in("openrouter"),
            model: "switched-model".into(),
            via: Some(Via::parse("deepinfra/fp8").unwrap()),
        },
        tuning: Tuning {
            temperature: Some(0.7),
            top_p: Some(0.9),
            effort: Some(genai::chat::ReasoningEffort::Medium),
        },
    }
}

fn until_record(host: &Host, id: AgentId, mut done: impl FnMut(&Record) -> bool) -> Vec<Record> {
    let deadline = Instant::now() + WAIT;
    let mut records = Vec::new();
    loop {
        let signal = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the agent publishes the expected record");
        if let Signal::Commit(agent, commit) = signal
            && agent == id
        {
            let finished = commit.records().iter().any(|record| done(record.value()));
            records.extend(
                commit
                    .into_records()
                    .into_iter()
                    .map(crate::record::Recorded::into_value),
            );
            if finished {
                return records;
            }
        }
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "the agent did not reach the expected boundary"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_pick(port: &Port, id: AgentId, pick: &Pick) {
    wait_until(|| port.row(id).is_some_and(|row| row.pick == *pick));
}

fn settled(record: &Record) -> bool {
    matches!(record, Record::Forensic(Forensic::JobSettled { .. }))
}

#[test]
fn model_at_rest_records_its_built_selection_in_its_own_log_then_runs_on_the_published_pick() {
    let routed = routed_pick();
    let plan = Pick {
        choice: Choice {
            account: Account::login(Login::Chatgpt, "plan", "plan@example.test"),
            model: "plan-model".into(),
            via: None,
        },
        tuning: routed.tuning.clone(),
    };
    let normalized = Tuning {
        effort: plan.tuning.effort.clone(),
        ..Tuning::default()
    };
    let routed_tuning = routed.tuning.clone();
    for (requested, tuning) in [(routed, routed_tuning), (plan, normalized)] {
        let expected = Pick {
            choice: requested.choice.clone(),
            tuning,
        };
        let session = with_minting(
            testkit::trunk(Trunk::Converses),
            vec![Script::new().then(Reply::text("minted answer"))],
        );
        let dir = session.log_dir().clone();
        let host = testkit::host(session);
        let id = host.roots()[0];
        let port = host.port();
        assert_ne!(port.row(id).unwrap().pick, expected);
        port.command(id, Command::Model(requested)).unwrap();
        let records = until_record(&host, id, |record| {
            matches!(record, Record::Forensic(Forensic::ModelChanged { .. }))
        });
        assert!(records.iter().any(|record| matches!(record,
            Record::Forensic(Forensic::ModelChanged { model, context_window, label, service, account, via })
            if model == &expected.choice.model && context_window.is_none()
                && label == &crate::provider::identity::label(&expected.choice.account, &[])
                && service == expected.choice.account.service.name.as_str()
                && account == expected.choice.account.id.as_str()
                && via == &expected.choice.via
        )));
        wait_for_pick(&port, id, &expected);
        port.submit(id, "use the new provider".into()).unwrap();
        let records = until_record(&host, id, settled);
        assert!(records.iter().any(|record| matches!(record,
            Record::Display(crate::record::Display::Answer { text }) if text == "minted answer"
        )));
        assert!(records.iter().any(|record| matches!(record,
            Record::Forensic(Forensic::TurnStarted { tuning, via, .. }) if tuning == &expected.tuning && via == &expected.choice.via
        )));
        let own_log = crate::record::read_records(&dir.path().join(crate::record::RECORD)).unwrap();
        assert_eq!(own_log.iter().filter(|record| matches!(record, Record::Forensic(Forensic::ModelChanged { model, via, .. }) if model == &expected.choice.model && via == &expected.choice.via)).count(), 1);
        let switched = own_log
            .iter()
            .position(|record| matches!(record, Record::Forensic(Forensic::ModelChanged { .. })))
            .unwrap();
        let job = own_log
            .iter()
            .position(|record| matches!(record, Record::Forensic(Forensic::JobOpened)))
            .unwrap();
        assert!(switched < job, "the rest-time switch opens no exchange");
        assert_eq!(host.end(), [] as [String; 0]);
    }
}

#[test]
fn a_refused_model_build_records_an_error_and_keeps_the_provider_and_published_pick() {
    let mut session = with_minting(testkit::trunk(Trunk::Converses), vec![]);
    set_provider(
        &mut session,
        scripted(
            "old-model",
            Script::new().then(Reply::text("old provider still runs")),
        ),
    );
    let before = Pick::of(&session.body.state.provider);
    let dir = session.log_dir().clone();
    let host = testkit::host(session);
    let id = host.roots()[0];
    let port = host.port();
    port.command(id, Command::Model(routed_pick())).unwrap();
    let records = until_record(&host, id, |record| {
        matches!(record, Record::Forensic(Forensic::Error { .. }))
    });
    assert!(records.iter().any(|record| matches!(record,
        Record::Forensic(Forensic::Error { text }) if text == "could not switch to switched-model: this scripted session has no script left to mint switched-model"
    )));
    assert_eq!(port.row(id).unwrap().pick, before);
    port.submit(id, "continue".into()).unwrap();
    let records = until_record(&host, id, settled);
    assert!(records.iter().any(|record| matches!(record,
        Record::Display(crate::record::Display::Answer { text }) if text == "old provider still runs"
    )));
    assert_eq!(port.row(id).unwrap().pick, before);
    let own_log = crate::record::read_records(&dir.path().join(crate::record::RECORD)).unwrap();
    assert!(
        !own_log
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::ModelChanged { .. })))
    );
    assert_eq!(host.end(), [] as [String; 0]);
}

static MODEL_TOOL_ENTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static MODEL_TOOL_RELEASE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct ReleaseModelTool;
impl Drop for ReleaseModelTool {
    fn drop(&mut self) {
        MODEL_TOOL_RELEASE.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn hold_model_tool(
    _args: &[ral_core::Value],
    _mooring: &ral_core::shell::runtime::Mooring,
    shell: &mut ral_core::Shell,
) -> ral_core::shell::runtime::Settled<ral_core::Value> {
    MODEL_TOOL_ENTERED.store(true, std::sync::atomic::Ordering::Release);
    while !MODEL_TOOL_RELEASE.load(std::sync::atomic::Ordering::Acquire) {
        shell.poll()?;
        std::thread::sleep(Duration::from_millis(2));
    }
    shell.poll()?;
    Ok(ral_core::Value::Unit)
}

static MODEL_TOOL_BUILTINS: [ral_core::shell::runtime::BuiltinEntry; 1] =
    [ral_core::shell::runtime::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("test-model-boundary"),
        testkit::scheme_test_clear_block_forever,
        "test-only: hold the tool boundary until released.",
        ral_core::shell::runtime::BuiltinBody::Static(hold_model_tool),
    )];

#[test]
fn model_posted_during_a_tool_applies_at_its_boundary_but_only_the_next_attempt_uses_it() {
    MODEL_TOOL_ENTERED.store(false, std::sync::atomic::Ordering::Release);
    MODEL_TOOL_RELEASE.store(false, std::sync::atomic::Ordering::Release);
    let mut session = with_minting(
        testkit::dressed_trunk(|runtime| runtime.install_builtins(&MODEL_TOOL_BUILTINS)),
        vec![Script::new().then(Reply::tool_calls(vec![ral_call(
            "new-reply",
            "exarch-agents `reply 'new attempt'",
        )]))],
    );
    let old = scripted(
        "old-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call(
                "held",
                "test-model-boundary",
            )]))
            .then(Reply::empty()),
    );
    let old_pick = Pick::of(&old);
    set_provider(&mut session, old);
    let host = testkit::host(session);
    let release = ReleaseModelTool;
    let id = host.roots()[0];
    let port = host.port();
    port.submit(id, "begin on the old provider".into()).unwrap();
    wait_until(|| MODEL_TOOL_ENTERED.load(std::sync::atomic::Ordering::Acquire));
    let new_pick = routed_pick();
    port.command(id, Command::Model(new_pick.clone())).unwrap();
    assert_eq!(port.queued(id).unwrap(), ["/model switched-model"]);
    assert_eq!(port.row(id).unwrap().pick, old_pick);
    drop(release);
    let records = until_record(&host, id, settled);
    wait_for_pick(&port, id, &new_pick);
    let turns: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Forensic(Forensic::TurnStarted { tuning, via, .. }) => Some((tuning, via)),
            _ => None,
        })
        .collect();
    assert_eq!(
        turns,
        [
            (&old_pick.tuning, &old_pick.choice.via),
            (&old_pick.tuning, &old_pick.choice.via),
            (&new_pick.tuning, &new_pick.choice.via)
        ]
    );
    let switched = records
        .iter()
        .position(|record| matches!(record, Record::Forensic(Forensic::ModelChanged { .. })))
        .unwrap();
    let turn_positions: Vec<_> = records
        .iter()
        .enumerate()
        .filter_map(|(i, record)| {
            matches!(record, Record::Forensic(Forensic::TurnStarted { .. })).then_some(i)
        })
        .collect();
    assert!(turn_positions[0] < switched && switched < turn_positions[1]);
    assert!(
        records[turn_positions[1]..turn_positions[2]]
            .iter()
            .any(|record| matches!(
                record,
                Record::Protocol(Protocol::Steering {
                    voice: Voice::Harness,
                    ..
                })
            ))
    );
    assert!(records.iter().any(|record| matches!(record,
        Record::Forensic(Forensic::Status { status: ChildStatus::Replied(value) }) if value.as_str() == Some("new attempt")
    )));
    assert_eq!(host.end(), [] as [String; 0]);
}
