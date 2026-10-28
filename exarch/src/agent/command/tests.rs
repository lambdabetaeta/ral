use crate::agent::Agent;
use crate::bus::{Emitter, Read, Rewrite};
use crate::record::{Forensic, Record};

#[test]
fn commands_after_a_display_first_failure_do_not_open_or_rewrite_sessions() {
    let mut session = Agent::for_test("system").unwrap();
    let recorder = session.recorder();
    recorder.fail_after(3);
    recorder
        .emit(crate::record::Display::Answer {
            text: "failed display".into(),
        })
        .unwrap_err();
    let before = session.log_dir().read("record.jsonl").unwrap();
    session.handle().post_read(Read::Hatch {
        memory: crate::record::Memory::Amnemon,
        name: Some("must-not-open".into()),
    });
    session.handle().post_rewrite(Rewrite::Clear);
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    assert!(matches!(
        session.attend(&emit),
        crate::bus::AgentOutcome::Cancelled
    ));
    assert!(!session.handle().is_live());
    assert_eq!(session.log_dir().read("record.jsonl").unwrap(), before);
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|signal| matches!(signal, crate::bus::Signal::Born(..)))
    );
    assert_eq!(
        crate::record::AgentLog::session_ids(session.log.borrow().sessions_root()).unwrap(),
        vec![session.id()]
    );
}

/// `/resources` rides the inbox to the attend loop and folds into exactly
/// one event with no provider round-trip.
#[test]
fn resources_command_routes_through_attend_and_emits_once() {
    let mut session = Agent::for_test("system").unwrap();
    session.handle().post_read(Read::Resources);

    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let _ = session.attend(&emit);

    // The fold is a transient; the session's own head bookend rides the
    // same channel, so the claim below is about the live half alone.
    let mut transients = std::iter::from_fn(|| rx.try_recv().ok()).filter_map(|sig| match sig {
        crate::bus::Signal::Transient(_, t) => Some(t),
        crate::bus::Signal::Fact(..) | crate::bus::Signal::Born(..) => None,
    });
    match transients
        .next()
        .expect("the /resources command must publish its fold")
    {
        crate::record::Transient::Resources { card } => {
            assert_eq!(card.marks().len(), 2, "a heading and one matrix");
            let crate::card::Mark::Fields { rows } = &card.marks()[1] else {
                panic!("the second mark is the agent's matrix");
            };
            assert!(
                rows.iter().any(|r| r.label == "workers.running"),
                "the registry chapter is surveyed"
            );
        }
        other => panic!("expected Transient::Resources, got {other:?}"),
    }
    // The park the loop settles into announces itself, and nothing else
    // follows: a command is not a turn, so no state ran before the fold.
    assert!(
        matches!(
            transients.next(),
            Some(crate::record::Transient::State(
                crate::record::AgentState::Ready
            ))
        ),
        "the fold is followed by the ready-boundary state alone"
    );
    assert!(
        transients.next().is_none(),
        "one /resources command, exactly one fold"
    );
}

#[test]
fn rewind_past_the_last_turn_reports_the_bad_anchor() {
    let mut session = Agent::for_test("system").unwrap();
    session.handle().post_rewrite(Rewrite::Rewind(7));

    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let _ = session.attend(&emit);

    assert!(
        crate::bus::drain_records(&rx).iter().any(|rec| matches!(
            rec,
            Record::Forensic(Forensic::Error { text }) if text.contains("turn 7")
        )),
        "a /rewind past the last turn reports the bad anchor"
    );
}
