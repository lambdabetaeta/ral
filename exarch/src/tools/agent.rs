//! `agent` tool — fork a child session and run a sub-agent prompt.
//!
//! [`Staged::Spawned`]; the parent joins same-batch children before the next
//! provider request, so sibling agents can overlap but cannot outlive the
//! parent turn.
//! The full prompt — every line — is rendered to the rail before spawn, so the
//! user can see what the child was asked to do.

use super::{Tool, invalid_input, u64_field};
use crate::bus::{AgentOutcome, AgentResult, Emitter, InboxMsg, Kind};
use crate::cancel::Token;
use crate::event::ToolResult as SessionToolResult;
use crate::provider::{Provider, ProviderError};
use crate::session::{Session, Staged, TurnOutcome};
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

/// How the parent relates to the child: a dependency edge or an
/// orchestration edge.
#[cfg_attr(test, derive(Debug, PartialEq))]
enum Mode {
    /// Run the child because the parent needs its answer now — fork-join on
    /// the dispatch scope, the answer returns in this `tool_result`.
    Sync,
    /// Start the child because the parent is orchestrating independent work
    /// — a detached worker that outlives the turn; this call returns a start
    /// receipt and the child's reply arrives later through the inbox.
    Async,
}

/// Parsed Agent tool input: the child's prompt, a validated tab label, and
/// the dependency/orchestration mode.  Splitting the parse from dispatch
/// keeps validation testable in isolation.
#[cfg_attr(test, derive(Debug))]
struct Args {
    prompt: String,
    title: String,
    mode: Mode,
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
    // `mode` is optional; sync is the compatibility default, since today's
    // `agent` result is the child's final answer, not a start receipt.
    let mode = match obj.get("mode").and_then(Value::as_str) {
        None | Some("sync") => Mode::Sync,
        Some("async") => Mode::Async,
        Some(other) => return Err(format!("`mode` must be \"sync\" or \"async\", got {other:?}")),
    };
    Ok(Args {
        prompt,
        title,
        mode,
    })
}

impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn desc(&self) -> &'static str {
        "Spawn a sub-agent to carry out `prompt`, returning only its final \
assistant text.  This is expensive: the child is a fresh session that \
re-pays the entire system prompt and does not share your conversation — it \
sees a value-snapshot of your shell (your `let` bindings, cwd, env) but \
none of your reasoning or message history.  Its own shell mutations (cd, \
env, new bindings) do NOT propagate back; you receive a string, not its \
bindings or intermediate findings.  File edits it makes to the working tree \
are real and persist.  Because each call costs a whole session boot plus a \
round-trip to relay the result back as text, delegate only a substantial, \
self-contained unit of work: a focused exploration whose intermediate \
detail you do not want to carry, or a task whose execution would otherwise \
flood your own context.  NEVER delegate a single grep/view/read/edit you \
can run inline — running it yourself is cheaper and keeps the result \
addressable (e.g. the line-hash `edit` needs).  A sub-agent cannot itself \
spawn sub-agents.\n\n\
`mode` chooses the dependency edge.  `\"sync\"` (the default) blocks: the \
child's final text returns in this call, the right shape when you need the \
answer before you can continue.  `\"async\"` starts the child as background \
work and returns immediately with a start receipt `{id, title, status, \
log_dir}`; the child runs off your critical path and its reply is delivered \
to you on its own as a marked turn when it finishes.  Use async to fan out \
independent work you do not need to wait on now; list live ones with \
`agents` and stop one with `agent_cancel`."
    }

    fn root_only(&self) -> bool {
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
                    "mode": {
                        "type": "string",
                        "enum": ["sync", "async"],
                        "description": "`sync` (default) blocks and returns the child's final \
            text in this call — a dependency edge.  `async` starts the child as background \
            work, returns a start receipt now, and delivers the reply later as a marked \
            turn — an orchestration edge.",
                    },
                },
                "required": ["prompt", "title"],
            })
        })
    }

    fn dispatch<'scope, 'env: 'scope>(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        provider: &'env Arc<Provider>,
        token: &'env crate::cancel::Token,
        emit: &Emitter,
        scope: &'scope thread::Scope<'scope, 'env>,
    ) -> Staged<'scope> {
        let Args {
            prompt,
            title,
            mode,
        } = match parse_args(&input) {
            Ok(a) => a,
            Err(reason) => return invalid_input(id, "agent", "<invalid input>", &reason, emit),
        };
        let child = match session.fork() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not fork child session: {e}");
                session.note_error(msg.clone(), emit);
                return Staged::Done(SessionToolResult { id, content: msg });
            }
        };
        // The dispatch shows on the rail in both modes; only sync gives the
        // child a live tab (`Born`/tokens/`Died`).  An async child is muted
        // to its own log, so it never streams to the parent bus.
        emit.emit(Kind::ToolCall {
            tool: "agent",
            cmd: prompt.clone(),
            summary: Some(title.clone()),
        });
        match mode {
            Mode::Sync => dispatch_sync(id, prompt, title, child, provider, token, emit, scope),
            Mode::Async => dispatch_async(id, prompt, title, child, provider, session, emit),
        }
    }
}

/// Fork-join sync child: runs on the dispatch `thread::scope`, the parent
/// joins it before the next provider request, and its final text returns in
/// this `tool_result`.  The child shares the parent's cancellation token, so
/// one Esc halts the whole spawn tree.
#[allow(clippy::too_many_arguments)]
fn dispatch_sync<'scope, 'env: 'scope>(
    id: String,
    prompt: String,
    title: String,
    mut child: Session,
    provider: &'env Arc<Provider>,
    token: &'env Token,
    emit: &Emitter,
    scope: &'scope thread::Scope<'scope, 'env>,
) -> Staged<'scope> {
    let child_emit = emit.child(child.id);
    child_emit.emit(Kind::Born {
        log_dir: child.log_dir().to_path_buf(),
        title: title.clone(),
    });
    // Children do not nudge — the top-level driver owns that.  Collapse the
    // child outcome to a reply string here so the parent's join phase sees a
    // single shape, and emit a final `SubagentDone` on the parent's channel
    // so the TUI can land a completion breadcrumb in root's scrollback (where
    // subagent tabs leave no trace once their linger window expires).
    let parent_emit = emit.clone();
    let started = Instant::now();
    let child_token = token.clone();
    let handle = scope.spawn(move || {
        // A child's `apply` runs on this scoped thread, outside the root
        // turn's `catch_unwind` (`bus::pump`), so a mid-eval panic here would
        // otherwise skip the two emits below — leaking the child's TUI tab (no
        // `Died` to age it out) and dropping the breadcrumb.  Recover the
        // panic into an `Err` so `Died`/`SubagentDone` always fire.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            child.apply(provider, Some(prompt), &child_token, &child_emit)
        }))
        .unwrap_or_else(|p| Err(ProviderError::Other(format!("sub-agent panicked: {}", panic_msg(&p)))));
        child_emit.emit(Kind::Died);
        let (text, error) = match &r {
            Ok(TurnOutcome::Complete(s)) => (s.clone(), None),
            Ok(TurnOutcome::Empty) => (String::new(), None),
            Ok(TurnOutcome::Stopped { reason }) => (String::new(), Some(reason.clone())),
            Ok(TurnOutcome::Cancelled) => (String::new(), Some("cancelled".into())),
            Ok(TurnOutcome::Capped) => (String::new(), Some("step cap reached".into())),
            Err(e) => (String::new(), Some(e.to_string())),
        };
        parent_emit.emit(Kind::SubagentDone {
            title,
            text,
            error,
            elapsed: started.elapsed(),
        });
        r.map(|outcome| match outcome {
            TurnOutcome::Complete(s) => s,
            TurnOutcome::Empty => "(child returned empty reply)".into(),
            TurnOutcome::Stopped { reason } => format!("(child stopped: {reason})"),
            TurnOutcome::Cancelled => "(child cancelled)".into(),
            TurnOutcome::Capped => "(child stopped: step cap reached)".into(),
        })
    });
    Staged::Spawned { id, handle }
}

/// Orchestration-edge async child: a detached worker owned by the session's
/// agent registry, surviving this turn.  Returns a start receipt now; the
/// child's reply is posted to the inbox at settle and delivered as a marked
/// turn.  The worker is muted — the per-turn bus closes when the spawning
/// turn ends — so it writes only its own `SessionLog`.
fn dispatch_async<'scope>(
    id: String,
    prompt: String,
    title: String,
    mut child: Session,
    provider: &Arc<Provider>,
    session: &mut Session,
    emit: &Emitter,
) -> Staged<'scope> {
    let agent_id = child.id;
    let log_dir = child.log_dir().to_path_buf();
    let log_dir_str = log_dir.display().to_string();
    // A background worker holds its own, unpublished cancellation token: an
    // Esc targets the foreground turn, never these children.  They are
    // cancelled by `agent_cancel`, `/clear`, or their reaper ceiling.
    let cancel = Token::new();
    let generation =
        session
            .agents
            .register(agent_id, title.clone(), log_dir.clone(), cancel.clone());
    // An owned provider clone: the worker outlives the dispatch frame and so
    // cannot hold the borrowed `&Provider`.  A later `/model` switch replaces
    // the driver's Arc but leaves this already-running child on its clone.
    let provider = Arc::clone(provider);
    let registry = session.agents.clone();
    let inbox = emit.inbox();
    let started = Instant::now();
    let worker_title = title.clone();
    thread::Builder::new()
        .name(format!("exarch-agent-{agent_id}"))
        .spawn(move || {
            // Muted: a background child cannot stream to the per-turn bus, so
            // it gets an emitter whose receiver is dropped.  Its persistent
            // record is its own forked `SessionLog`; its reply returns through
            // the inbox.
            let (tx, _rx) = std::sync::mpsc::channel();
            let muted = Emitter::new(tx, agent_id);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                child.apply(&provider, Some(prompt), &cancel, &muted)
            }))
            .unwrap_or_else(|p| {
                Err(ProviderError::Other(format!(
                    "sub-agent panicked: {}",
                    panic_msg(&p)
                )))
            });
            let (outcome, text) = to_outcome(r);
            // Deliver only if still the live worker of the current generation;
            // a result from before a `/clear` is dropped, not posted into a
            // rebuilt context.
            if registry.settle(agent_id, generation) {
                inbox.push(InboxMsg::AgentResult(AgentResult {
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
    Staged::Done(SessionToolResult {
        id,
        content: receipt.to_string(),
    })
}

/// Reduce a child's turn outcome to the inbox digest: the outcome tag and,
/// for a completed run, its final text.
fn to_outcome(r: Result<TurnOutcome, ProviderError>) -> (AgentOutcome, String) {
    match r {
        Ok(TurnOutcome::Complete(s)) => (AgentOutcome::Complete, s),
        Ok(TurnOutcome::Empty) => (AgentOutcome::Empty, String::new()),
        Ok(TurnOutcome::Stopped { reason }) => (AgentOutcome::Stopped(reason), String::new()),
        Ok(TurnOutcome::Cancelled) => (AgentOutcome::Cancelled, String::new()),
        Ok(TurnOutcome::Capped) => (AgentOutcome::Stopped("step cap reached".into()), String::new()),
        Err(e) => (AgentOutcome::Failed(e.to_string()), String::new()),
    }
}

/// The message of a recovered panic payload, for either string shape.
fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
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

    fn root_only(&self) -> bool {
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

    fn dispatch<'scope, 'env: 'scope>(
        &self,
        id: String,
        _input: Value,
        session: &mut Session,
        _provider: &'env Arc<Provider>,
        _token: &'env Token,
        emit: &Emitter,
        _scope: &'scope thread::Scope<'scope, 'env>,
    ) -> Staged<'scope> {
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
        Staged::Done(SessionToolResult { id, content })
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

    fn root_only(&self) -> bool {
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

    fn dispatch<'scope, 'env: 'scope>(
        &self,
        id: String,
        input: Value,
        session: &mut Session,
        _provider: &'env Arc<Provider>,
        _token: &'env Token,
        emit: &Emitter,
        _scope: &'scope thread::Scope<'scope, 'env>,
    ) -> Staged<'scope> {
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
        Staged::Done(SessionToolResult { id, content })
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
        assert_eq!(a.mode, Mode::Sync, "sync is the default mode");
    }

    #[test]
    fn parse_reads_async_mode_and_rejects_unknown() {
        let a = parse_args(&json!({ "prompt": "x", "title": "t", "mode": "async" })).unwrap();
        assert_eq!(a.mode, Mode::Async);
        let s = parse_args(&json!({ "prompt": "x", "title": "t", "mode": "sync" })).unwrap();
        assert_eq!(s.mode, Mode::Sync);
        let e = parse_args(&json!({ "prompt": "x", "title": "t", "mode": "later" })).unwrap_err();
        assert!(e.contains("mode"), "unknown mode is rejected: {e}");
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
