//! A call to a tool the request did not advertise.

use super::*;
use crate::agent::testkit::{chat_trunk, ral_call};
use crate::record::{Forensic, Record};
use ral_core::process::DurableRoot;

#[test]
fn an_unoffered_tool_is_answered_as_unknown_and_noted() {
    let bash = ToolCall {
        call_id: "c1".into(),
        fn_name: "bash".into(),
        fn_arguments: serde_json::json!({}),
        thought_signatures: None,
    };
    // A chat agent offers nothing, `ral` included.
    for (mut session, call) in [
        (Agent::for_test("system").unwrap(), bash),
        (chat_trunk(), ral_call("c1", "pwd")),
    ] {
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id());
        session.couple(&emit);
        let result = session.invoke(&DurableRoot::default().child(), call, &emit);
        assert_eq!(result.id, "c1");
        let records = crate::bus::drain_records(&rx);
        let noted: Vec<&String> = records
            .iter()
            .filter_map(|record| match record {
                Record::Forensic(Forensic::Error { text }) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            noted,
            [&result.content],
            "the error noted is the one answered"
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, Record::Display(_))),
            "nothing was dispatched"
        );
    }
}
