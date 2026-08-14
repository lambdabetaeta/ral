#![allow(clippy::disallowed_methods)]

//! End-to-end tests for [`exarch::agent::Agent::deliberate`] driven through
//! a scripted provider backend (`exarch::provider::Provider::scripted`).
//! This is the composition the protocol machine (`event.rs`) is exercised
//! under in production: `deliberate` renders the transcript, calls the
//! provider, commits the reply, runs the tool batch, drains tool-boundary
//! steering, and continues until it reaches quiescence — the path
//! `event.rs`'s unit tests never see.
//!
//! The transcript-admission invariant (every committed message
//! serialises to a request every provider accepts) is asserted by
//! round-tripping the committed messages through the same genai
//! `ChatMessage` serialisation the live request uses.

use exarch::agent::event::{ContextOp, EditAuthority, ProviderErrorRecord};
use exarch::agent::{Agent, deliberate};
use exarch::bus::{AgentId, AgentState, Emitter, Kind, channel};
use exarch::provider::scripted::{Reply, Script};
use exarch::provider::{Provider, ProviderError, ProviderKind};
use exarch::record::{Protocol, Record};
use genai::chat::{ChatRole, ContentPart, ToolCall};
use std::sync::Arc;

/// A scripted provider behind the `Arc` the attend loop threads — the same
/// shape the live driver holds so an async `agent` worker could capture a
/// clone.
fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, ProviderKind::Openai, script))
}

// Mirror the binary's pre-`main` re-exec dispatch — helper re-exec dispatch,
// then the OS-sandbox stage — before libtest sees the flags either would
// reject; see [`exarch::dispatch_pre_main`].
exarch::pre_main_ctor!();

/// Run one `deliberate` against a fresh `Emitter`/collector pair, returning
/// the outcome plus every event the worker emitted.
fn drive_deliberate(
    session: &mut Agent,
    provider: &Arc<Provider>,
    prompt: Option<&str>,
) -> (Result<deliberate::Outcome, ProviderError>, Vec<Kind>) {
    let id: AgentId = session.id;
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, id);
    let token = exarch::agent::cancel::Token::new();
    let outcome = session.deliberate(provider, prompt.map(str::to_string), None, &token, &emit);
    drop(emit);
    let kinds = rx
        .into_iter()
        .filter_map(exarch::bus::Signal::into_event)
        .map(|e| e.kind)
        .collect();
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
fn plain_text_reaches_quiescence() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted("test-model", Script::new().then(Reply::text("hello")));

    let (outcome, _kinds) = drive_deliberate(&mut session, &provider, Some("hi"));

    match outcome {
        Ok(deliberate::Outcome::Complete(s)) => assert_eq!(s, "hello"),
        other => panic!("expected Complete, got {other:?}"),
    }
    assert!(session.is_ready());
    assert_admissible(&session);
}

#[test]
fn tool_call_then_completion() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let x = 41")]))
            .then(Reply::text("done")),
    );

    let (outcome, kinds) = drive_deliberate(&mut session, &provider, Some("set x then report"));

    match outcome {
        Ok(deliberate::Outcome::Complete(s)) => assert_eq!(s, "done"),
        other => panic!("expected Complete, got {other:?}"),
    }
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
/// next tool call — the persistent-shell contract `deliberate` relies on.
#[test]
fn bindings_persist_across_tool_calls() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let xv = 41")]))
            .then(Reply::tool_calls(vec![ral_call("c2", "echo $[$xv + 1]")]))
            .then(Reply::text("done")),
    );

    let (outcome, _kinds) = drive_deliberate(&mut session, &provider, Some("compute"));
    assert!(matches!(outcome, Ok(deliberate::Outcome::Complete(_))));

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

/// X6: a `MaxTokens` reply carrying captured tool calls must run them
/// and continue, not return `Truncated` while the session is
/// `AwaitingToolResults` (which strands the protocol and fails the next
/// `append_user`).  The truncated-with-tools reply runs the tool; the next
/// reply reaches quiescence.
#[test]
fn truncated_with_tool_calls_runs_and_continues() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::truncated_with_tool_calls(vec![ral_call(
                "c1",
                "let truncated_marker = 1",
            )]))
            .then(Reply::text("resumed after truncation")),
    );

    let (outcome, kinds) =
        drive_deliberate(&mut session, &provider, Some("do work then get cut off"));
    match outcome {
        Ok(deliberate::Outcome::Complete(s)) => assert_eq!(s, "resumed after truncation"),
        other => panic!("truncated-with-tools must run and complete, got {other:?}"),
    }
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Kind::ToolCall { tool: "ral", .. })),
        "the captured tool call must be run, not dropped"
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

/// A stream that stalls after some text has streamed must not discard the
/// work and end the run: `deliberate` commits the streamed prefix as the
/// assistant message and returns `Truncated` (carrying the stall cause) so
/// the nudge continues the exchange with `continue`.  Salvaging the prefix
/// does not soften the failure — it surfaces as `Kind::Stalled`, carrying the
/// classified cause under the error chrome — but neither does it end the run,
/// which is why the kind is its own and not `Kind::ProviderError`.
#[test]
fn stalled_stream_commits_partial_and_truncates() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new().then(Reply::stalled("partial answer before the stall")),
    );

    let (outcome, kinds) = drive_deliberate(&mut session, &provider, Some("answer at length"));
    match outcome {
        Err(ProviderError::Truncated { reason }) => {
            assert_eq!(reason, "Failed to parse stream data for model 'test-model'");
        }
        other => panic!("a committed stall must surface as Truncated, got {other:?}"),
    }
    // The streamed prefix is committed verbatim, so the exchange the nudge
    // continues resumes from exactly what the user already saw.
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
    // The provider's own words reach the user whole and at error weight: a
    // ` | `-joined slate note beside a model switch reads as housekeeping, and a
    // stall is a refusal or a dropped connection the user has to act on.
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            Kind::Stalled(ProviderErrorRecord::Other { cause })
                if cause == "Failed to parse stream data for model 'test-model'"
        )),
        "the stall cause must surface as Kind::Stalled",
    );
    // Not the kind that ends an exchange: this run continues.
    assert!(
        !kinds.iter().any(|k| matches!(k, Kind::ProviderError(_))),
        "a survived stall is not a ProviderError",
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

/// X7: an empty assistant reply (zero content parts) must never be
/// committed empty — a stub is substituted so the transcript stays
/// admissible — while the outcome still surfaces as `Empty` for the nudge.
#[test]
fn empty_reply_commits_a_stub_not_empty_content() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted("test-model", Script::new().then(Reply::empty()));

    let (outcome, _kinds) = drive_deliberate(&mut session, &provider, Some("say nothing"));
    assert!(
        matches!(outcome, Ok(deliberate::Outcome::Empty)),
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

/// Auto-compaction fires at the one boundary `deliberate` checks — its own
/// entry — and rewrites the model view to the summary plus the recent
/// exchange, verbatim.  `test-model` has no pricing-catalog entry, so the
/// window is unknown and the byte fallback (`digest::COMPACT_THRESHOLD`, 500
/// KiB) is the live trigger: the older exchange is the larger, so half the
/// history holds the newer one whole.
#[test]
fn compaction_fires_at_the_entry_boundary_and_keeps_the_recent_exchange() {
    let mut session = Agent::for_test("system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::text("ack one"))
            .then(Reply::text("ack two"))
            .then(Reply::text("done")),
    );

    let first = format!("EXCHANGE1 {}", "x".repeat(450_000));
    let second = format!("EXCHANGE2 {}", "y".repeat(150_000));
    for prompt in [&first, &second] {
        let (acked, _) = drive_deliberate(&mut session, &provider, Some(prompt));
        assert!(matches!(acked, Ok(deliberate::Outcome::Complete(_))));
    }
    let (outcome, kinds) = drive_deliberate(&mut session, &provider, Some("after"));

    match outcome {
        Ok(deliberate::Outcome::Complete(s)) => assert_eq!(s, "done"),
        other => panic!("expected Complete, got {other:?}"),
    }
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Kind::State(AgentState::Compacting))),
        "the compaction must announce its own state"
    );
    let view = session.rendered_messages();
    assert_eq!(view[0].role, ChatRole::User);
    let summary = view[0].content.first_text().unwrap_or_default();
    assert!(
        summary.contains("scripted summary"),
        "the model view must open on the summary, got {summary:?}"
    );
    let rendered = serde_json::to_string(&view).unwrap();
    assert!(
        !rendered.contains("EXCHANGE1"),
        "the summarised prefix must leave the model view"
    );
    assert!(
        rendered.contains("EXCHANGE2") && rendered.contains("ack two"),
        "the recent exchange is kept verbatim, not summarised"
    );
    assert!(session.is_ready());

    // Reclamation is heap-only: the one seam's log keeps every byte.
    #[derive(serde::Deserialize)]
    struct Entry {
        record: Record,
    }
    let records: Vec<Record> = serde_json::Deserializer::from_reader(std::io::BufReader::new(
        std::fs::File::open(session.log_dir().join("record.jsonl")).unwrap(),
    ))
    .into_iter::<Entry>()
    .map(|entry| entry.map(|e| e.record))
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::Protocol(Protocol::UserPrompt { text, .. }) if text.contains("EXCHANGE1")
        )),
        "compaction must not touch the durable record log"
    );
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::Protocol(Protocol::ContextEdited {
                op: ContextOp::Fold { .. },
                by: EditAuthority::Harness,
            })
        )),
        "auto-compaction must record its fold as a harness-authored context edit"
    );
}

/// X2: a tool call whose `fn_arguments` is not a JSON object is repaired to
/// `{}` at the commit boundary, so the re-serialised transcript is one a
/// strict backend accepts.  The repaired-to-`{}` call still reaches the
/// tool (which reports the missing fields), and it reaches quiescence.
#[test]
fn malformed_tool_arguments_are_normalised_to_object() {
    let mut session = Agent::for_test("system").unwrap();
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

    let (outcome, _kinds) = drive_deliberate(&mut session, &provider, Some("emit a bad call"));
    assert!(matches!(outcome, Ok(deliberate::Outcome::Complete(_))));

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
