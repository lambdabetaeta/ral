use crate::agent::{Admission, Agent, TestTrunk};
use crate::bus::{Command, Emitter, Read, Rewrite};
use crate::record::{Forensic, Record};

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
    handle.post_command(Command::Read(Read::Hatch {
        memory: crate::record::Memory::Amnemon,
        name: Some("must-not-open".into()),
    }));
    handle.post_command(Command::Rewrite(Rewrite::Clear));
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
    session
        .handle()
        .post_command(Command::Read(Read::Resources));
    session
        .handle()
        .post_command(Command::Rewrite(Rewrite::Close));
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
    session
        .handle()
        .post_command(Command::Rewrite(Rewrite::Rewind(7)));
    session
        .handle()
        .post_command(Command::Rewrite(Rewrite::Close));
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
