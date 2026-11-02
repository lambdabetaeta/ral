//! `dispatch`'s refusals: a malformed call is recorded and answered, an
//! unrecordable one is answered unrun.

use super::*;
use crate::record::Record;

fn emitter(session: &Agent) -> (Emitter, crate::bus::BusReceiver) {
    let (tx, rx) = crate::bus::channel();
    (Emitter::new(tx, session.id()), rx)
}

fn call(session: &mut Agent, input: &Value, emit: &Emitter) -> SessionToolResult {
    dispatch(
        "paired".into(),
        input,
        session,
        &ral_core::process::DurableRoot::default().child(),
        emit,
    )
}

#[test]
fn a_malformed_call_records_a_failed_row_pair_and_still_answers() {
    let mut session = Agent::for_test("system").unwrap();
    let (emit, rx) = emitter(&session);
    session.couple(&emit);
    let result = call(&mut session, &json!({ "description": "no command" }), &emit);
    assert_eq!(result.id, "paired");
    assert_ne!(result.content, "");
    let rows: Vec<Display> = crate::bus::drain_records(&rx)
        .into_iter()
        .filter_map(|record| match record {
            Record::Display(row) => Some(row),
            _ => None,
        })
        .collect();
    assert!(matches!(
        rows.as_slice(),
        [
            Display::ToolCall { cmd, summary: None, .. },
            Display::Verdict { verdict, .. },
        ] if cmd == crate::record::INVALID_INPUT && verdict.failed
    ));
    assert_eq!(session.body.reply, None, "nothing ran");
}

/// A call whose row cannot be recorded is not made, valid or not.
#[test]
fn an_unrecordable_call_is_answered_without_running() {
    for input in [
        json!({ "description": "no command" }),
        json!({
            "command": "exarch-agents `reply 'unrun'",
            "description": "Stage the answer.",
        }),
    ] {
        let mut session = Agent::for_test("system").unwrap();
        let (emit, _rx) = emitter(&session);
        session.recorder().fail_after(0);
        let result = call(&mut session, &input, &emit);
        assert_eq!(result.id, "paired");
        assert_ne!(result.content, "");
        assert_eq!(session.body.reply, None, "nothing ran");
    }
}
