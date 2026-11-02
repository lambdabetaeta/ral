//! The refusal arms of the session commands: each leaves the context as it
//! was and says so once.

use crate::agent::Agent;
use crate::agent::testkit::{close_exchange, ral_call};
use crate::bus::{Emitter, Read, Rewrite};
use crate::record::{AgentLog, Forensic, Memory, Record, Transcript, Voice};
use genai::chat::{ChatMessage, ContentPart};

fn emitter(session: &Agent) -> (Emitter, crate::bus::BusReceiver) {
    let (tx, rx) = crate::bus::channel();
    (Emitter::new(tx, session.id()), rx)
}

fn count(records: &[Record], mut is: impl FnMut(&Record) -> bool) -> usize {
    records.iter().filter(|record| is(record)).count()
}

fn is_error(record: &Record) -> bool {
    matches!(record, Record::Forensic(Forensic::Error { .. }))
}

fn owe_a_tool_result(session: &mut Agent) {
    let log = &mut session.body.log;
    log.append_user("unfinished".into(), None, Voice::Human)
        .unwrap();
    log.append_assistant(
        ChatMessage::assistant(vec![ContentPart::ToolCall(ral_call("owed", "pwd"))]),
        vec!["owed".into()],
        None,
    )
    .unwrap();
}

#[test]
fn clear_and_evict_are_refused_while_a_tool_result_is_owed() {
    for command in [Rewrite::Clear, Rewrite::Evict] {
        let mut session = Agent::for_test("system").unwrap();
        owe_a_tool_result(&mut session);
        let before = session.rendered_messages().len();
        let (emit, rx) = emitter(&session);
        session.couple(&emit);
        assert!(session.rewrite(&command, &emit).is_continue());
        let records = crate::bus::drain_records(&rx);
        assert_eq!(count(&records, is_error), 1);
        assert_eq!(
            count(&records, |record| matches!(
                record,
                Record::Transcript(Transcript::Cleared | Transcript::Evicted(_))
            )),
            0
        );
        assert_eq!(session.rendered_messages().len(), before);
        assert_eq!(session.generation, 0);
    }
}

#[test]
fn evict_sheds_the_older_context_once_the_session_is_ready() {
    let mut session = Agent::for_test("system").unwrap();
    for n in 0..4 {
        close_exchange(
            &mut session,
            &format!("question {n} {}", "x".repeat(2000)),
            &"y".repeat(2000),
        );
    }
    let before = session.body.log.context().history_bytes();
    let (emit, rx) = emitter(&session);
    session.couple(&emit);
    assert!(session.rewrite(&Rewrite::Evict, &emit).is_continue());
    let records = crate::bus::drain_records(&rx);
    assert_eq!(count(&records, is_error), 0);
    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::Transcript(Transcript::Evicted(_))
        )),
        1
    );
    assert!(session.body.log.context().history_bytes() < before);
}

/// The human's own name is taken, so the session cannot open: it says so and
/// adds no agent.
#[test]
fn a_hatch_that_cannot_open_a_session_reports_and_opens_nothing() {
    let mut session = Agent::for_test("system").unwrap();
    let sessions = session.fleet.launch.sessions_root.clone();
    let (emit, rx) = emitter(&session);
    session.couple(&emit);
    session.read(
        &Read::Hatch {
            memory: Memory::Amnemon,
            name: Some(session.handle().0.name.clone()),
        },
        &emit,
    );
    let records = crate::bus::drain_records(&rx);
    assert_eq!(count(&records, is_error), 1);
    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::Forensic(Forensic::SystemNote { .. })
        )),
        0
    );
    assert_eq!(AgentLog::human_children(&sessions).unwrap().len(), 1);
    assert_eq!(session.fleet.registry.live().len(), 1);
}
