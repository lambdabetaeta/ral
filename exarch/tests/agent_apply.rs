#![allow(clippy::disallowed_methods)]

//! End-to-end tests for [`exarch::agent::Agent::apply`] driven through
//! a scripted provider backend (`exarch::provider::Provider::scripted`).
//! This is the composition the protocol machine (`event.rs`) is exercised
//! under in production: `apply` renders the transcript, calls the provider,
//! commits the reply, dispatches tools, drains tool-boundary steering, and
//! continues until the turn completes — the path `event.rs`'s unit tests
//! never see.
//!
//! The transcript-admission invariant (every committed message
//! serialises to a request every provider accepts) is asserted by
//! round-tripping the committed messages through the same genai
//! `ChatMessage` serialisation the live request uses.

use exarch::agent::{Agent, TurnOutcome};
use exarch::bus::{AgentId, Emitter, Kind, channel};
use exarch::provider::scripted::{Reply, Script};
use exarch::provider::{Provider, ProviderError, ProviderKind};
use genai::chat::{ChatRole, ContentPart, ToolCall};
use std::sync::Arc;

/// A scripted provider behind the `Arc` the turn driver threads — the same
/// shape the live driver holds so an async `agent` worker could capture a
/// clone.
fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, ProviderKind::Openai, script))
}

/// Mirror the binary's pre-`main` re-exec dispatch — helper re-exec
/// dispatch, then the OS-sandbox stage — before libtest sees the flags
/// either would reject; see [`exarch::dispatch_pre_main`].
#[ctor::ctor(unsafe)]
fn init_test_binary() {
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(i32::from(code));
    }
}

/// A unique scratch directory per test so concurrent runs don't share
/// session logs.
fn tmp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("exarch-apply-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Run one `apply` against a fresh `Emitter`/collector pair, returning
/// the outcome plus every event the worker emitted.
fn drive_apply(
    session: &mut Agent,
    provider: &Arc<Provider>,
    prompt: Option<&str>,
) -> (Result<TurnOutcome, ProviderError>, Vec<Kind>) {
    let id: AgentId = session.id;
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, id);
    let token = exarch::cancel::Token::new();
    let _slot = exarch::cancel::publish(&token);
    let outcome = session.apply(provider, prompt.map(str::to_string), &token, &emit);
    drop(emit);
    let kinds = rx.into_iter().map(|e| e.kind).collect();
    (outcome, kinds)
}

/// A `ral` tool call invoking `cmd`.
fn ral_call(id: &str, cmd: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: "ral".into(),
        fn_arguments: serde_json::json!({
            "cmd": cmd,
            "description": "test command",
        }),
        thought_signatures: None,
    }
}

/// Assert every committed model-view message serialises (the proxy for
/// "every provider accepts the request") and that no assistant message
/// is empty.
fn assert_admissible(session: &Agent) {
    for m in session.rendered_messages() {
        let json = serde_json::to_string(&m).expect("committed message must serialise");
        if m.role == ChatRole::Assistant {
            assert!(
                !m.content.is_empty(),
                "committed assistant message is empty (rejected by strict backends): {json}"
            );
        }
    }
}

#[test]
fn plain_text_turn_completes() {
    let dir = tmp("plain-text");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let provider = scripted("test-model", Script::new().then(Reply::text("hello")));

    let (outcome, _kinds) = drive_apply(&mut session, &provider, Some("hi"));

    match outcome {
        Ok(TurnOutcome::Complete(s)) => assert_eq!(s, "hello"),
        other => panic!("expected Complete, got {other:?}"),
    }
    assert!(session.is_ready());
    assert_admissible(&session);
}

#[test]
fn tool_call_then_completion() {
    let dir = tmp("tool-then-complete");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let x = 41")]))
            .then(Reply::text("done")),
    );

    let (outcome, kinds) = drive_apply(&mut session, &provider, Some("set x then report"));

    match outcome {
        Ok(TurnOutcome::Complete(s)) => assert_eq!(s, "done"),
        other => panic!("expected Complete, got {other:?}"),
    }
    // The tool call was dispatched and its result committed.
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Kind::ToolCall { tool: "ral", .. })),
        "ral tool call should reach the bus"
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

/// A `let` binding committed by an earlier tool call survives into the
/// next tool call — the persistent-shell contract `apply` relies on.
#[test]
fn bindings_persist_across_tool_calls() {
    let dir = tmp("bindings-persist");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let xv = 41")]))
            .then(Reply::tool_calls(vec![ral_call("c2", "echo $[$xv + 1]")]))
            .then(Reply::text("done")),
    );

    let (outcome, _kinds) = drive_apply(&mut session, &provider, Some("compute"));
    assert!(matches!(outcome, Ok(TurnOutcome::Complete(_))));

    // The second tool result must show 42 — the binding survived.
    let results: Vec<_> = session
        .rendered_messages()
        .into_iter()
        .filter(|m| m.role == ChatRole::Tool)
        .collect();
    let joined: String = results
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|p| match p {
                ContentPart::ToolResponse(r) => Some(r.content.clone()),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("42"),
        "expected 42 in tool results, got: {joined}"
    );
}

/// X6: a `MaxTokens` reply carrying captured tool calls must dispatch them
/// and continue, not return `Truncated` while the session is
/// `AwaitingToolResults` (which strands the protocol and fails the next
/// `append_user`).  The truncated-with-tools reply runs the tool; the next
/// reply completes the turn.
#[test]
fn truncated_with_tool_calls_dispatches_and_continues() {
    let dir = tmp("truncated-with-tools");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::truncated_with_tool_calls(vec![ral_call(
                "c1",
                "let truncated_marker = 1",
            )]))
            .then(Reply::text("resumed after truncation")),
    );

    let (outcome, kinds) = drive_apply(&mut session, &provider, Some("do work then get cut off"));
    match outcome {
        Ok(TurnOutcome::Complete(s)) => assert_eq!(s, "resumed after truncation"),
        other => panic!("truncated-with-tools must dispatch and complete, got {other:?}"),
    }
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Kind::ToolCall { tool: "ral", .. })),
        "the captured tool call must be dispatched, not dropped"
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

/// A stream that stalls after some text has streamed must not discard the
/// work and end the run: `apply` commits the streamed prefix as the
/// assistant message and returns `Truncated` (carrying the stall cause) so
/// the nudge re-drives the turn with `continue`.  The recovery is an
/// operational `SystemNote`, not an `Error`.
#[test]
fn stalled_stream_commits_partial_and_truncates() {
    let dir = tmp("stalled-stream");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::stalled("partial answer before the stall")),
    );

    let (outcome, kinds) = drive_apply(&mut session, &provider, Some("answer at length"));
    match outcome {
        Err(ProviderError::Truncated { reason }) => {
            assert_eq!(reason, "Web stream error: operation timed out");
        }
        other => panic!("a committed stall must surface as Truncated, got {other:?}"),
    }
    // The streamed prefix is committed verbatim, so the re-driven turn
    // continues from exactly what the user already saw.
    let committed: Vec<_> = session
        .rendered_messages()
        .into_iter()
        .filter(|m| m.role == ChatRole::Assistant)
        .filter_map(|m| m.content.first_text().map(str::to_string))
        .collect();
    assert!(
        committed
            .iter()
            .any(|t| t == "partial answer before the stall"),
        "the streamed prefix must be committed, got {committed:?}",
    );
    // A recovered stall is operational, not a misconfiguration: it surfaces
    // as a slate note, never the red error chrome.
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Kind::SystemNote(t) if t.contains("Stream stalled"))),
        "the stall recovery must surface as a SystemNote",
    );
    assert!(
        !kinds.iter().any(|k| matches!(k, Kind::Error(_))),
        "a recovered stall must not surface an Error",
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

/// X7: an empty assistant reply (zero content parts) must never be
/// committed empty — a stub is substituted so the transcript stays
/// admissible — while the turn still surfaces as `Empty` for the nudge.
#[test]
fn empty_reply_commits_a_stub_not_empty_content() {
    let dir = tmp("empty-reply");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let provider = scripted("test-model", Script::new().then(Reply::empty()));

    let (outcome, _kinds) = drive_apply(&mut session, &provider, Some("say nothing"));
    assert!(
        matches!(outcome, Ok(TurnOutcome::Empty)),
        "an empty reply still surfaces as Empty for the nudge"
    );
    // The committed assistant message must not be empty — this is the
    // invariant `assert_admissible` enforces.
    assert_admissible(&session);
    let assistants: Vec<_> = session
        .rendered_messages()
        .into_iter()
        .filter(|m| m.role == ChatRole::Assistant)
        .collect();
    assert!(
        !assistants.is_empty() && assistants.iter().all(|m| !m.content.is_empty()),
        "the empty reply must have been replaced with a non-empty stub"
    );
}

/// X2: a tool call whose `fn_arguments` is not a JSON object is repaired to
/// `{}` at the commit boundary, so the re-serialised transcript is one a
/// strict backend accepts.  The repaired-to-`{}` call still reaches the
/// tool (which reports the missing fields), and the turn completes.
#[test]
fn malformed_tool_arguments_are_normalised_to_object() {
    let dir = tmp("malformed-tool-args");
    let mut session = Agent::for_test(&dir, "system").unwrap();
    let bad_call = ToolCall {
        call_id: "c1".into(),
        fn_name: "ral".into(),
        fn_arguments: serde_json::json!("not an object"),
        thought_signatures: None,
    };
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![bad_call]))
            .then(Reply::text("recovered")),
    );

    let (outcome, _kinds) = drive_apply(&mut session, &provider, Some("emit a bad call"));
    assert!(matches!(outcome, Ok(TurnOutcome::Complete(_))));

    // Every committed tool call's arguments must be a JSON object.
    for m in session.rendered_messages() {
        for part in &m.content {
            if let ContentPart::ToolCall(tc) = part {
                assert!(
                    tc.fn_arguments.is_object(),
                    "a committed tool call must carry object arguments, got {:?}",
                    tc.fn_arguments
                );
            }
        }
    }
    assert_admissible(&session);
}
