use super::*;
use serde_json::json;

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
fn parse_rejects_missing_command() {
    let e = parse_args(&json!({
        "description": "noop",
    }))
    .unwrap_err();
    assert!(e.contains("`command`"));
}

#[test]
fn parse_names_the_fields_received() {
    let e = parse_args(&json!({
        "cmd": "pwd",
        "description": "x",
    }))
    .unwrap_err();
    assert!(e.contains("`command`"));
    assert!(e.contains("got `cmd`, `description`"));
}

#[test]
fn parse_rejects_non_string_command() {
    let e = parse_args(&json!({
        "command": 3,
        "description": "x",
    }))
    .unwrap_err();
    assert!(e.contains("must be a string"));
}

#[test]
fn parse_rejects_non_object() {
    let e = parse_args(&json!("oops")).unwrap_err();
    assert!(e.contains("not a JSON object"));
}

#[test]
fn parse_rejects_missing_description() {
    let e = parse_args(&json!({ "command": "pwd" })).unwrap_err();
    assert!(e.contains("`description`"));
}

#[test]
fn parse_rejects_empty_description() {
    let e = parse_args(&json!({
        "command": "pwd",
        "description": "   ",
    }))
    .unwrap_err();
    assert!(e.contains("`description`"));
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
fn parse_rejects_multiline_description() {
    let e = parse_args(&json!({
        "command": "pwd",
        "description": "first\nsecond",
    }))
    .unwrap_err();
    assert!(e.contains("`description`"));
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

/// Zero parses as a `u64`, so the floor has to be a check of its own.
#[test]
fn parse_timeout_rejects_zero() {
    let e = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
        "timeout_secs": 0,
    }))
    .unwrap_err();
    assert!(e.contains("`timeout_secs`"));
    assert!(e.contains("≥1"));
}

#[test]
fn parse_timeout_rejects_negative() {
    let e = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
        "timeout_secs": -5,
    }))
    .unwrap_err();
    assert!(e.contains("`timeout_secs`"));
    assert!(e.contains("positive integer"));
}

/// Even an integral float like `5.0` is not a `u64` to serde.
#[test]
fn parse_timeout_rejects_float() {
    let e = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
        "timeout_secs": 5.0,
    }))
    .unwrap_err();
    assert!(e.contains("`timeout_secs`"));
    assert!(e.contains("positive integer"));
}

#[test]
fn parse_timeout_rejects_string() {
    let e = parse_args(&json!({
        "command": "pwd",
        "description": "noop",
        "timeout_secs": "30",
    }))
    .unwrap_err();
    assert!(e.contains("`timeout_secs`"));
    assert!(e.contains("positive integer"));
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
        session.body.state.reply,
        Some(FOValue::String {
            value: "loaned".into()
        })
    );
    assert!(
        session.handle().status().report.is_none(),
        "only the batch boundary publishes a reply"
    );
    assert!(crate::bus::drain_records(&rx).iter().any(|record| matches!(
        record,
        crate::record::Record::Display(Display::Verdict { .. })
    )));
}
