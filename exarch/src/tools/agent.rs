//! `agent` tool — fork a child session and launch a sub-agent.
//!
//! Launch-only and always asynchronous: every call forks a child, runs it on a
//! detached thread through the same [`Session::drive`] loop, and returns a
//! start receipt immediately.  The child's single result is delivered later to
//! the parent's inbox at quiescence.
//!
//! The full prompt — every line — is rendered to the rail before spawn, so the
//! user can see what the child was asked to do.

use super::{Tool, invalid_input, u64_field};
use crate::bus::{AgentOutcome, AgentResult, Emitter, InboxMsg, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use crate::session::Session;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

pub(super) struct AgentTool;

/// Monotonic dispatch counter, used to mint `sub-{N}` fallback titles
/// when the model omits one.  Lives as long as the process — the TUI
/// only needs each child's label to be distinguishable from its
/// siblings on screen.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Parsed Agent tool input: the child's prompt and a validated tab label.
/// Splitting the parse from dispatch keeps validation testable in isolation.
#[cfg_attr(test, derive(Debug))]
struct Args {
    prompt: String,
    title: String,
}

/// True for titles that fit the tab-bar contract — non-empty, ≤24
/// chars, ASCII alphanumeric plus `-` / `_`.  Spaces and punctuation
/// are excluded because the tab bar lays them out token-by-token.
fn valid_title(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Read the JSON object the model emitted into [`Args`], or a reason
/// explaining why the input is malformed.  A missing or syntactically
/// invalid `title` is *not* an error — it falls back to `sub-{N}`.
fn parse_args(input: &Value) -> Result<Args, String> {
    let obj = input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?;
    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `prompt`".to_string())?
        .to_string();
    let title = match obj.get("title").and_then(Value::as_str) {
        Some(t) if valid_title(t) => t.to_string(),
        _ => format!("sub-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed)),
    };
    Ok(Args { prompt, title })
}

impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn desc(&self) -> &'static str {
        "Spawn a sub-agent to carry out `prompt`.  This is launch-only and \
always asynchronous: the call forks a child session and returns immediately \
with a start receipt `{id, title, status, log_dir}`.  The child runs the same \
agent loop off your critical path; its single reply is delivered to you LATER \
as its own marked turn when it finishes.  To fan out independent work, spawn \
several and combine their results when they land; poll live ones with `agents` \
and stop one with `agent_cancel`.  A sub-agent cannot itself spawn \
sub-agents.\n\n\
This is expensive: the child is a fresh session that re-pays the entire system \
prompt and does not share your conversation — it sees a value-snapshot of your \
shell (your `let` bindings, cwd, env) but none of your reasoning or message \
history.  Its own shell mutations (cd, env, new bindings) do NOT propagate \
back; you receive a string, not its bindings or intermediate findings.  File \
edits it makes to the working tree are real and persist.  Because each call \
costs a whole session boot plus a round-trip to relay the result back as text, \
delegate only a substantial, self-contained unit of work: a focused \
exploration whose intermediate detail you do not want to carry, or a task \
whose execution would otherwise flood your own context.  NEVER delegate a \
single grep/view/read/edit you can run inline — running it yourself is cheaper \
and keeps the result addressable (e.g. the line-hash `edit` needs)."
    }

    fn spawns(&self) -> bool {
        true
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The instruction to give the child agent.  \
            The child runs the same agent loop on its own snapshot of the shell.",
                    },
                    "title": {
                        "type": "string",
                        "description": "A short label (≤24 chars, ASCII letters/digits/`-`/`_`) \
            naming the child's tab in the TUI — e.g. `refactor-output`, `audit-deps`.  \
            Choose a phrase the user will recognise at a glance.  Omitted or invalid \
            titles fall back to `sub-{N}`.",
                    },
                },
                "required": ["prompt", "title"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let Args { prompt, title } = match parse_args(&input) {
            Ok(a) => a,
            Err(reason) => return invalid_input(id, "agent", "<invalid input>", &reason, emit),
        };
        let mut child = match session.fork() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not fork child session: {e}");
                session.note_error(msg.clone(), emit);
                return SessionToolResult { id, content: msg };
            }
        };
        // Capture everything off the child before it moves into the worker
        // thread: its identity, log directory, and own cancellation token, the
        // registry entry's generation, the parent's mailbox (the child's
        // upward result edge), an owned registry handle and provider clone.
        let agent_id = child.id;
        let log_dir = child.log_dir().to_path_buf();
        let log_dir_str = log_dir.display().to_string();
        let cancel = child.cancel_token().clone();
        let generation = session
            .agents
            .register(agent_id, title.clone(), log_dir.clone(), cancel);
        let parent_mailbox = session.mailbox();
        let registry = session.agents.clone();
        let provider = Arc::clone(provider);
        // A live tab whenever the bus outlives the turn (the TUI): a real
        // emitter cloned off the session sender, stamped with the child's id
        // and carrying the child's own mailbox.  Off a per-turn bus (headless)
        // the child is muted — an emitter whose receiver is already dropped —
        // so it never streams; its persistent record is its own forked
        // `SessionLog` and its reply returns through the inbox.
        let child_emit = if emit.is_session_lived() {
            emit.child(agent_id, child.mailbox())
        } else {
            let (tx, _rx) = std::sync::mpsc::channel();
            Emitter::new(tx, agent_id)
        };
        let started = Instant::now();
        let born_title = title.clone();
        let worker_title = title.clone();
        // Seed the launch turn into the child's own inbox: the only downward
        // edge is this one write.
        child.seed(prompt.clone());
        // The dispatch shows on the rail before the spawn, so the user can see
        // exactly what the child was asked to do.
        emit.emit(Kind::ToolCall {
            tool: "agent",
            cmd: prompt,
            summary: Some(title.clone()),
        });
        thread::Builder::new()
            .name(format!("exarch-agent-{agent_id}"))
            .spawn(move || {
                // `Born`/`Died` bracket the child's run so its tab is
                // registered before the first token and ages out after the
                // last; on a muted emitter both are no-ops.  The id routes
                // every event to the child's own tab through the TUI's existing
                // draw path.
                child_emit.emit(Kind::Born {
                    log_dir: log_dir.clone(),
                    title: born_title,
                });
                let (outcome, text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // The peer wraps a snapshot of the provider at spawn, so a
                    // later root `/model` never disturbs an already-running child.
                    child.drive(
                        crate::session::ProviderHandle::new(provider),
                        &mut crate::session::NoControl,
                        &child_emit,
                    )
                }))
                .unwrap_or_else(|_| (AgentOutcome::Failed("sub-agent panicked".into()), String::new()));
                child_emit.emit(Kind::Died);
                // Deliver only if still the live worker of the current
                // generation; a result from before a `/clear` is dropped, not
                // posted into a rebuilt context.
                if registry.settle(agent_id, generation) {
                    parent_mailbox.push(InboxMsg::AgentResult(AgentResult {
                        id: agent_id,
                        title: worker_title,
                        outcome,
                        text,
                        log_dir,
                        elapsed: started.elapsed(),
                        generation,
                    }));
                }
            })
            .expect("spawn async agent worker");
        let receipt = json!({
            "id": agent_id,
            "title": title,
            "status": "started",
            "log_dir": log_dir_str,
        });
        SessionToolResult {
            id,
            content: receipt.to_string(),
        }
    }
}

/// `agents` — list the live async agents this session started.
pub(super) struct AgentsTool;

impl Tool for AgentsTool {
    fn name(&self) -> &'static str {
        "agents"
    }

    fn desc(&self) -> &'static str {
        "List the async agents you started this session that are still \
running — by id, with title, elapsed seconds, and log directory.  Use it to \
recover ids after a context compaction, then `agent_cancel` to stop a \
straggler.  Settled agents are not listed: their replies arrive on their own \
as marked turns."
    }

    fn spawns(&self) -> bool {
        true
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {},
                "required": [],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        _input: Value,
        session: &mut Session,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        emit.emit(Kind::ToolCall {
            tool: "agents",
            cmd: "agents".into(),
            summary: None,
        });
        let live = session.agents.list();
        let content = if live.is_empty() {
            "no live async agents".to_string()
        } else {
            live.iter()
                .map(|a| {
                    format!(
                        "{}  {}  {}s  {}",
                        a.id,
                        a.title,
                        a.elapsed.as_secs(),
                        a.log_dir.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

/// `agent_cancel` — stop one live async agent by id.
pub(super) struct AgentCancelTool;

impl Tool for AgentCancelTool {
    fn name(&self) -> &'static str {
        "agent_cancel"
    }

    fn desc(&self) -> &'static str {
        "Cancel a live async agent by its id (from `agents`).  The worker is \
asked to stop at its next checkpoint; it then delivers a cancelled result.  \
A no-op if no live agent has that id."
    }

    fn spawns(&self) -> bool {
        true
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The id of the live async agent to cancel, as shown by `agents`.",
                    },
                },
                "required": ["id"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let agent_id = match u64_field(&input, "id") {
            Some(n) => n,
            None => {
                return invalid_input(
                    id,
                    "agent_cancel",
                    "<invalid input>",
                    "missing required integer field `id`",
                    emit,
                );
            }
        };
        emit.emit(Kind::ToolCall {
            tool: "agent_cancel",
            cmd: format!("agent_cancel {agent_id}"),
            summary: None,
        });
        let content = if session.agents.cancel(agent_id) {
            format!("cancelling agent {agent_id}")
        } else {
            format!("no live agent with id {agent_id}")
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_prompt_and_title() {
        let a = parse_args(&json!({
            "prompt": "summarise SPEC.md",
            "title":  "summarise-spec",
        }))
        .unwrap();
        assert_eq!(a.prompt, "summarise SPEC.md");
        assert_eq!(a.title, "summarise-spec");
    }

    #[test]
    fn parse_rejects_missing_prompt() {
        let e = parse_args(&json!({ "title": "foo" })).unwrap_err();
        assert!(e.contains("`prompt`"));
    }

    #[test]
    fn parse_rejects_non_object() {
        let e = parse_args(&json!(42)).unwrap_err();
        assert!(e.contains("not a JSON object"));
    }

    /// Missing title falls back to a generated `sub-N` label.
    #[test]
    fn parse_synthesises_title_when_missing() {
        let a = parse_args(&json!({ "prompt": "x" })).unwrap();
        assert!(a.title.starts_with("sub-"));
    }

    /// An invalid title (spaces, punctuation, too long) is treated as
    /// missing and replaced with a generated label, never propagated.
    #[test]
    fn parse_rejects_invalid_title() {
        for bad in [
            "",
            "has spaces",
            "has/slash",
            "has.dot",
            "x".repeat(25).as_str(),
        ] {
            let a = parse_args(&json!({ "prompt": "x", "title": bad })).unwrap();
            assert!(a.title.starts_with("sub-"), "bad title {bad:?} leaked");
        }
    }

    #[test]
    fn valid_title_boundaries() {
        assert!(valid_title("a"));
        assert!(valid_title("refactor-output"));
        assert!(valid_title("audit_deps"));
        assert!(valid_title(&"x".repeat(24)));
        assert!(!valid_title(""));
        assert!(!valid_title(&"x".repeat(25)));
        assert!(!valid_title("has space"));
        assert!(!valid_title("non-ascii-é"));
    }
}
