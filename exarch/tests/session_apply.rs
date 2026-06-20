#![allow(clippy::disallowed_methods)]

//! End-to-end tests for [`exarch::session::Session::apply`] and
//! [`Session::run_turn`] driven through a scripted provider backend
//! (`exarch::provider::Provider::scripted`).  This is the composition
//! the protocol machine (`event.rs`) is exercised under in production:
//! `apply` renders the transcript, calls the provider, commits the
//! reply, dispatches tools, and loops — the loop `event.rs`'s unit
//! tests never see.
//!
//! The transcript-admission invariant (every committed message
//! serialises to a request every provider accepts) is asserted by
//! round-tripping the committed messages through the same genai
//! `ChatMessage` serialisation the live request uses.

use exarch::bus::{Emitter, Event, Inbox, Kind, SessionId, Sink};
use exarch::provider::scripted::{Reply, Script};
use exarch::provider::{Provider, ProviderError};
use exarch::session::{Session, TurnOutcome};
use genai::chat::{ChatRole, ContentPart, ToolCall};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

/// A scripted provider behind the `Arc` the turn driver threads — the same
/// shape the live driver holds so an async `agent` worker could capture a
/// clone.
fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// Mirror the binary's pre-`main` re-exec dispatch — helper re-exec
/// dispatch, then the OS-sandbox stage — before libtest sees the flags
/// either would reject; see [`exarch::dispatch_pre_main`].
#[ctor::ctor(unsafe)]
fn init_test_binary() {
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(code as i32);
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

/// A sink that records every event it handles, for `run_turn` tests
/// that need to inspect what reached the frontend.
#[derive(Default)]
#[allow(dead_code)]
struct Recorder {
    kinds: Vec<Kind>,
}

impl Sink for Recorder {
    fn handle(&mut self, e: Event) {
        self.kinds.push(e.kind);
    }
}

struct SteeringSink {
    inbox: Inbox,
    kinds: Vec<Kind>,
    queued: bool,
}

impl Default for SteeringSink {
    fn default() -> Self {
        Self {
            inbox: Inbox::new(),
            kinds: Vec::new(),
            queued: false,
        }
    }
}

impl Sink for SteeringSink {
    fn handle(&mut self, e: Event) {
        let should_queue = !self.queued
            && matches!(&e.kind, Kind::ToolCall { cmd, .. } if cmd.contains("sleep 0.1"));
        if should_queue {
            self.inbox
                .push_user("steer: revise after the current batch".into());
            self.queued = true;
        }
        self.kinds.push(e.kind);
    }

    fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }
}

/// Run one `apply` against a fresh `Emitter`/collector pair, returning
/// the outcome plus every event the worker emitted.
fn drive_apply(
    session: &mut Session,
    provider: &Arc<Provider>,
    prompt: Option<&str>,
) -> (Result<TurnOutcome, ProviderError>, Vec<Kind>) {
    let id: SessionId = session.id;
    let (tx, rx) = channel();
    let emit = Emitter::new(tx, id);
    let root = exarch::cancel::mint_root();
    let outcome = session.apply(provider, prompt.map(str::to_string), root.token(), &emit);
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

/// An `agent` tool call invoking a child prompt.
fn agent_call(id: &str, title: &str, prompt: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: "agent".into(),
        fn_arguments: serde_json::json!({
            "title": title,
            "prompt": prompt,
        }),
        thought_signatures: None,
    }
}

/// An async `agent` tool call — the orchestration-edge mode.
fn agent_async_call(id: &str, title: &str, prompt: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: "agent".into(),
        fn_arguments: serde_json::json!({
            "title": title,
            "prompt": prompt,
            "mode": "async",
        }),
        thought_signatures: None,
    }
}

/// Assert every committed model-view message serialises (the proxy for
/// "every provider accepts the request") and that no assistant message
/// is empty.
fn assert_admissible(session: &Session) {
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
    let mut session = Session::for_test(&dir, "system").unwrap();
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
    let mut session = Session::for_test(&dir, "system").unwrap();
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

#[test]
fn queued_prompt_steers_after_current_tool_batch() {
    let dir = tmp("tool-steering");
    let mut session = Session::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![
                ral_call("c1", "sleep 0.1; return first"),
                ral_call("c2", "return second"),
            ]))
            .then(Reply::text("steered")),
    );
    let mut sink = SteeringSink::default();

    session
        .run_turn(&mut sink, &provider, Some("start".into()))
        .unwrap();

    let ral_calls = sink
        .kinds
        .iter()
        .filter(|k| matches!(k, Kind::ToolCall { tool: "ral", .. }))
        .count();
    assert_eq!(
        ral_calls, 2,
        "batch-boundary steering lets every already-issued tool call run"
    );
    assert!(
        sink.kinds
            .iter()
            .any(|k| matches!(k, Kind::UserPromptEcho(s) if s.contains("after the current batch"))),
        "drained steering prompt should echo when it becomes model-visible"
    );

    let messages = session.rendered_messages();
    let users: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == ChatRole::User)
        .flat_map(|m| {
            m.content.iter().filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
        })
        .collect();
    assert!(
        users.contains(&"steer: revise after the current batch"),
        "steering prompt must be committed before the next assistant step: {users:?}"
    );

    let tool_text = messages
        .iter()
        .filter(|m| m.role == ChatRole::Tool)
        .flat_map(|m| {
            m.content.iter().filter_map(|p| match p {
                ContentPart::ToolResponse(r) => Some(r.content.as_str()),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_text.contains("first"),
        "first call should run before steering is drained: {tool_text}"
    );
    assert!(
        tool_text.contains("second"),
        "second call should run before batch-boundary steering is drained: {tool_text}"
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

#[test]
fn same_batch_agents_run_concurrently() {
    let dir = tmp("same-batch-agents");
    let mut session = Session::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![
                agent_call("a1", "left", "sleep, then report left"),
                agent_call("a2", "right", "sleep, then report right"),
            ]))
            .then(Reply::tool_calls(vec![ral_call("left-sleep", "sleep 0.3")]))
            .then(Reply::tool_calls(vec![ral_call(
                "right-sleep",
                "sleep 0.3",
            )]))
            .then(Reply::text("left done"))
            .then(Reply::text("right done"))
            .then(Reply::text("root done")),
    );

    let started = Instant::now();
    let (outcome, kinds) = drive_apply(&mut session, &provider, Some("fan out"));
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Ok(TurnOutcome::Complete(_))));
    let child_calls = kinds
        .iter()
        .filter(|k| matches!(k, Kind::ToolCall { tool: "agent", .. }))
        .count();
    assert_eq!(child_calls, 2, "the root should dispatch both child agents");
    assert!(
        elapsed < Duration::from_millis(550),
        "two 300 ms child sleeps should overlap; elapsed {elapsed:?}"
    );
    assert!(session.is_ready());
    assert_admissible(&session);
}

/// An async `agent` dispatch returns a start receipt now (not the child's
/// answer) and the child's reply arrives later through the session inbox as
/// a marked turn — the orchestration edge.  The root turn does not wait for
/// the child.
#[test]
fn async_agent_returns_a_receipt_and_delivers_through_the_inbox() {
    let dir = tmp("async-agent");
    let mut session = Session::for_test(&dir, "system").unwrap();
    // Three replies, consumed across the root and the detached child: the
    // root's async dispatch, then a text reply each for whichever of the two
    // turns reaches the provider next.  The assertions do not depend on that
    // order — only that the root completes and the child posts a result.
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![agent_async_call(
                "bg",
                "background",
                "do background work",
            )]))
            .then(Reply::text("first"))
            .then(Reply::text("second")),
    );

    // Hold the inbox so we can observe the worker's delivery; `drive_apply`
    // builds its own, so drive `apply` directly here.
    let inbox = Inbox::new();
    let (tx, _rx) = channel();
    let emit = Emitter::with_inbox(tx, session.id, inbox.clone());
    let root = exarch::cancel::mint_root();
    let outcome = session.apply(&provider, Some("dispatch async".into()), root.token(), &emit);
    assert!(
        matches!(outcome, Ok(TurnOutcome::Complete(_))),
        "the root turn completes without waiting for the child"
    );

    // The detached worker settles on its own thread; poll the inbox for its
    // delivery, then drain it as the driver would at the turn boundary.
    let mut delivered = None;
    for _ in 0..200 {
        if let Some(text) = inbox.drain_turn() {
            delivered = Some(text);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let text = delivered.expect("the async agent must post its result to the inbox");
    assert!(
        text.starts_with("[agent 'background'"),
        "delivered as a marked agent turn, got {text:?}"
    );
    assert!(session.is_ready());
}

/// A `let` binding committed by an earlier tool call survives into the
/// next tool call — the persistent-shell contract `apply` relies on.
#[test]
fn bindings_persist_across_tool_calls() {
    let dir = tmp("bindings-persist");
    let mut session = Session::for_test(&dir, "system").unwrap();
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::tool_calls(vec![ral_call("c1", "let x = 41")]))
            .then(Reply::tool_calls(vec![ral_call("c2", "echo $[$x + 1]")]))
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

/// X1: auto-compaction fires at the turn boundary once history crosses
/// `COMPACT_THRESHOLD`.  The prior call sat in
/// `AwaitingAssistantAfterToolResults`, where `can_compact()` is always
/// false, so it never fired; the call now lives at the top of `apply`,
/// `ReadyForUser`, where the gate holds.  Turn 1 commits an over-threshold
/// reply; turn 2's boundary compaction collapses the history to the
/// summary.
#[test]
fn compaction_fires_at_the_threshold() {
    let dir = tmp("compaction-threshold");
    let mut session = Session::for_test(&dir, "system").unwrap();

    // A reply that serialises well past the 500 KiB threshold.
    let huge = "x".repeat(exarch::digest::COMPACT_THRESHOLD + 64 * 1024);
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::text(&huge))
            .then_summary("the work so far, summarised")
            .then(Reply::text("second turn done")),
    );

    let (o1, _k1) = drive_apply(&mut session, &provider, Some("produce a lot"));
    assert!(matches!(o1, Ok(TurnOutcome::Complete(_))));
    assert!(
        session.history_bytes() > exarch::digest::COMPACT_THRESHOLD,
        "turn 1 must leave history over the threshold so turn 2's boundary can compact"
    );

    let (o2, kinds) = drive_apply(&mut session, &provider, Some("continue"));
    assert!(matches!(o2, Ok(TurnOutcome::Complete(_))));
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Kind::Dim(t) if t.contains("compacted"))),
        "turn 2's boundary compaction must fire and emit a `[compacted …]` breadcrumb"
    );
    assert!(
        session.history_bytes() < exarch::digest::COMPACT_THRESHOLD,
        "compaction must collapse the history below the threshold"
    );
    assert_admissible(&session);
}

/// X10: a compaction summary that itself hit the token budget is surfaced
/// as `Truncated`, so `Session::compact` keeps the un-summarised history
/// rather than committing a half summary that silently drops everything
/// past the cut-off.  Turn 1 crosses the threshold; turn 2's boundary
/// compaction gets a truncated summary and must leave history intact.
#[test]
fn truncated_summary_preserves_history() {
    let dir = tmp("truncated-summary");
    let mut session = Session::for_test(&dir, "system").unwrap();

    let huge = "x".repeat(exarch::digest::COMPACT_THRESHOLD + 64 * 1024);
    let provider = scripted(
        "test-model",
        Script::new()
            .then(Reply::text(&huge))
            .then_summary_truncated()
            .then(Reply::text("second turn done")),
    );

    let (o1, _k1) = drive_apply(&mut session, &provider, Some("produce a lot"));
    assert!(matches!(o1, Ok(TurnOutcome::Complete(_))));
    let before = session.history_bytes();
    assert!(before > exarch::digest::COMPACT_THRESHOLD);

    let (o2, _k2) = drive_apply(&mut session, &provider, Some("continue"));
    assert!(matches!(o2, Ok(TurnOutcome::Complete(_))));
    // The truncated summary was rejected: history was not collapsed (it
    // only grew by turn 2's small prompt + reply).
    assert!(
        session.history_bytes() > exarch::digest::COMPACT_THRESHOLD,
        "a truncated summary must not replace the history"
    );
    assert_admissible(&session);
}

/// X6: a `MaxTokens` reply carrying captured tool calls must dispatch them
/// and continue, not return `Truncated` while the session is
/// `AwaitingToolResults` (which strands the protocol and fails the next
/// `append_user`).  The truncated-with-tools reply runs the tool; the next
/// reply completes the turn.
#[test]
fn truncated_with_tool_calls_dispatches_and_continues() {
    let dir = tmp("truncated-with-tools");
    let mut session = Session::for_test(&dir, "system").unwrap();
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

/// X7: an empty assistant reply (zero content parts) must never be
/// committed empty — a stub is substituted so the transcript stays
/// admissible — while the turn still surfaces as `Empty` for the nudge.
#[test]
fn empty_reply_commits_a_stub_not_empty_content() {
    let dir = tmp("empty-reply");
    let mut session = Session::for_test(&dir, "system").unwrap();
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
    let mut session = Session::for_test(&dir, "system").unwrap();
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
        for part in m.content.iter() {
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
