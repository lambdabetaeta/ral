use super::*;
use serde_json::json;

mod invalid;

#[test]
fn parse_accepts_command_and_description() {
    let a = parse_args(&json!({
        "command": "pwd",
        "description": "Printing the working directory.",
    }))
    .unwrap();
    assert_eq!(a.command, "pwd");
    assert_eq!(a.description, "Printing the working directory.");
}

#[test]
fn parse_truncates_oversize_description() {
    let big = "x".repeat(DESCRIPTION_MAX + 1);
    let a = parse_args(&json!({
        "command": "pwd",
        "description": big,
    }))
    .unwrap();
    assert_eq!(a.description, "x".repeat(DESCRIPTION_MAX) + "...");
}

#[test]
fn parse_timeout_defaults_when_absent() {
    let a = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
    }))
    .unwrap();
    assert_eq!(a.timeout_secs, CALL_TIMEOUT_SECS);
}

/// No upper clamp: an explicit bound is taken verbatim.
#[test]
fn parse_timeout_accepts_explicit() {
    let a = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
        "timeout_secs": 300,
    }))
    .unwrap();
    assert_eq!(a.timeout_secs, 300);
}

#[test]
fn parse_timeout_null_takes_default() {
    let a = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
        "timeout_secs": null,
    }))
    .unwrap();
    assert_eq!(a.timeout_secs, CALL_TIMEOUT_SECS);
}

#[test]
fn malformed_arguments_are_rejected() {
    for input in [
        json!({ "description": "x" }),
        json!({ "cmd": "pwd", "description": "x" }),
        json!({ "command": 3, "description": "x" }),
        json!("oops"),
        json!({ "command": "pwd" }),
        json!({ "command": "pwd", "description": "   " }),
        json!({ "command": "pwd", "description": "first\nsecond" }),
        // Zero parses as a `u64`, and an integral float is not one to serde.
        json!({ "command": "pwd", "description": "x", "timeout_secs": 0 }),
        json!({ "command": "pwd", "description": "x", "timeout_secs": -5 }),
        json!({ "command": "pwd", "description": "x", "timeout_secs": 5.0 }),
        json!({ "command": "pwd", "description": "x", "timeout_secs": "30" }),
    ] {
        assert!(parse_args(&input).is_err(), "{input}");
    }
}

#[test]
fn dispatch_stages_a_reply_through_the_loan_and_returns_the_body() {
    use crate::agent::Agent;
    use crate::bus::Emitter;
    use ral_core::data::FOValue;

    let mut session = Agent::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    let result = dispatch(
        "reply".into(),
        &json!({
            "command": "exarch-agents `reply 'loaned'", "description": "Stage the answer.",
        }),
        &mut session,
        &ral_core::process::DurableRoot::default().child(),
        &emit,
    );
    assert_eq!(result.id, "reply");
    assert!(result.content.contains("EXIT: 0"), "{}", result.content);
    assert_eq!(
        session.body.reply,
        Some(FOValue::String {
            value: "loaned".into()
        })
    );
    assert!(
        session.handle().status().unwrap().report.is_none(),
        "only the batch boundary publishes a reply"
    );
    assert!(crate::bus::drain_records(&rx).iter().any(|record| matches!(
        record,
        crate::record::Record::Display(Display::Verdict { .. })
    )));
}
