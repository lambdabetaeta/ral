//! Sub-agent tools — fork a child session and launch a sub-agent.
//!
//! Launch-only and always asynchronous: every call forks a child, runs it on a
//! detached thread through the same [`Agent::drive`] loop, and returns a
//! start receipt immediately.  The child's single result is delivered later to
//! the parent's inbox at quiescence.
//!
//! The full prompt — every line — is rendered to the rail before spawn, so the
//! user can see what the child was asked to do.

use super::{INVALID_INPUT, Tool, invalid_input, u64_field};
use crate::agent::Agent;
use crate::agent_registry::MessageError;
use crate::bus::{AgentOutcome, AgentResult, Emitter, InboxMsg, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

pub(super) struct SpawnTool {
    kind: SpawnKind,
}

impl SpawnTool {
    pub(super) fn amnemon() -> Self {
        Self {
            kind: SpawnKind::Amnemon,
        }
    }

    pub(super) fn mnemon() -> Self {
        Self {
            kind: SpawnKind::Mnemon,
        }
    }
}

#[derive(Copy, Clone)]
enum SpawnKind {
    /// Tabula rasa: fresh model context, inherited shell values only.
    Amnemon,
    /// Remembrance: inherited model context, fresh prompt at the end.
    Mnemon,
}

impl SpawnKind {
    fn tool(self) -> &'static str {
        match self {
            Self::Amnemon => "amnemon",
            Self::Mnemon => "mnemon",
        }
    }

    fn desc(self) -> &'static str {
        match self {
            Self::Amnemon => {
                "Spawn an amnemon sub-agent: tabula rasa.  This is launch-only \
and always asynchronous: the call forks a child session and returns immediately \
with a start receipt `{id, title, status, log_dir}`.  The child runs the same \
agent loop off your critical path; its single reply is delivered to you LATER \
as its own marked turn when it finishes.  To fan out independent work, spawn \
several and combine their results when they land; poll live ones with `agents` \
and stop one with `agent_cancel`.  A sub-agent may itself spawn sub-agents, but \
delegation depth is finite: each spawn spends one unit of your own spawn budget \
on the child, and once a descendant's budget reaches zero this tool disappears \
from its view, so a chain cannot recurse forever — `agent_cancel` on any node \
stops its whole live subtree regardless of depth.\n\n\
This is expensive: the child is a fresh model session that re-pays the entire \
system prompt and does not share your conversation — it sees a value-snapshot \
of your shell (your `let` bindings, cwd, env) but none of your reasoning or \
message history.  Its own shell mutations (cd, env, new bindings) do NOT \
propagate back; you receive a string, not its bindings or intermediate \
findings.  File edits it makes to the working tree are real and persist.  \
Because each call costs a whole session boot plus a round-trip to relay the \
result back as text, delegate only a substantial, self-contained unit of work: \
a focused exploration whose intermediate detail you do not want to carry, or a \
task whose execution would otherwise flood your own context.  NEVER delegate a \
single grep/view/read/edit you can run inline — running it yourself is cheaper \
and keeps the result addressable (e.g. the witness `edit` needs).  \
`permissions` bounds the child to at most your own authority."
            }
            Self::Mnemon => {
                "Spawn a mnemon sub-agent: remembrance.  This is launch-only \
and always asynchronous: the call forks a child session and returns immediately \
with a start receipt `{id, title, status, log_dir}`.  The child reuses your \
current provider selection and inherits your model-visible context, with the \
tool call's `prompt` appended as a fresh final user prompt.  That makes it a \
good fit when the child needs the conversation you already built and the \
provider can hit its prompt cache.  Its single reply is delivered to you LATER \
as its own marked turn when it finishes.\n\n\
The shell is still forked: the child sees a value-snapshot of your shell (your \
`let` bindings, cwd, env), but its own shell mutations (cd, env, new bindings) \
do NOT propagate back; you receive a string, not its bindings or intermediate \
findings.  File edits it makes to the working tree are real and persist.  Use \
mnemon when context reuse matters; use `amnemon` when isolation and a blank \
conversation are better.  `permissions` bounds the child to at most your own \
authority; delegation depth is likewise finite — each spawn spends one unit of \
your own spawn budget on the child, so a chain cannot recurse forever."
            }
        }
    }

    fn fork(self, session: &Agent, caps: ral_core::types::Capabilities) -> std::io::Result<Agent> {
        match self {
            Self::Amnemon => session.fork(caps),
            Self::Mnemon => session.fork_remembering(caps),
        }
    }
}

/// Monotonic dispatch counter, used to mint `sub-{N}` fallback titles
/// when the model omits one.  Lives as long as the process — the TUI
/// only needs each child's label to be distinguishable from its
/// siblings on screen.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Parsed Agent tool input: the child's prompt, a validated tab label, and
/// the mandatory base to which the child's permissions are narrowed.
/// Splitting the parse from dispatch keeps validation testable in isolation.
#[cfg_attr(test, derive(Debug))]
struct Args {
    prompt: String,
    title: String,
    permissions: String,
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
    // `permissions` is mandatory: every spawn states the child's ceiling
    // explicitly.  The base name is validated against the bake-ins at
    // dispatch, where the resolver's diagnostic can be surfaced.
    let permissions = obj
        .get("permissions")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `permissions`".to_string())?
        .to_string();
    Ok(Args {
        prompt,
        title,
        permissions,
    })
}

impl Tool for SpawnTool {
    fn name(&self) -> &'static str {
        self.kind.tool()
    }

    fn desc(&self) -> &'static str {
        self.kind.desc()
    }

    fn gate(&self) -> super::Gate {
        super::Gate::Spawns
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
                    "permissions": {
                        "type": "string",
                        "enum": ["confined", "minimal", "read-only", "edit-only", "reasonable", "dangerous"],
                        "description": "Narrow agent's permissions: confined (offline, no home \
            reads), minimal (working tree + /tmp + network), read-only (writes only to scratch), \
            edit-only (edits working tree, no build tooling), \
            reasonable (everyday tooling), dangerous (no narrowing).",
                    },
                },
                "required": ["prompt", "title", "permissions"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        dispatch_spawn(self.kind, id, input, session, emit)
    }
}

/// Start a bounded two-agent discussion from a host slash command.
///
/// This deliberately reuses the ordinary spawn API: the discussion chair is an
/// `mnemon` child, so it remembers the focused conversation; the chair is
/// instructed to spawn one `amnemon` partner and consume that partner's normal
/// `reply`.  There is no special peer channel and no pretend-human delivery.
pub(crate) fn spawn_discussion(session: &mut Agent, topic: &str, emit: &Emitter) -> String {
    let title = format!("discuss-{}", DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed));
    let input = json!({
        "prompt": discussion_prompt(topic),
        "title": title,
        "permissions": "read-only",
    });
    dispatch_spawn(
        SpawnKind::Mnemon,
        "/discuss".to_string(),
        input,
        session,
        emit,
    )
    .content
}

fn discussion_prompt(topic: &str) -> String {
    format!(
        "\
You are the chair of a lively two-way discussion. You will debate a single discussant via `message`.

Topic:
{topic}

Protocol:
- Spawn exactly one `amnemon` partner with a title that includes the topic (e.g. `debate-<topic>`) and permissions `read-only`.
  Give it a prompt that says: you are the discussant in a two-way debate, wait for the chair's messages, respond to each via `message` with both (a) a defense or refinement of your position and (b) a sharp critique of the chair's position, continue for at least 10 exchanges, then call `reply` with a closing statement.
- Once spawned, send the partner a `message` with the topic and ask for an opening position.
- When the partner responds, formulate (a) your sharpest challenge and (b) your own position on the topic. Send both back via `message`.
- When the partner responds again, update your own position — concede where they landed a hit, sharpen where you still disagree — and send a new challenge plus your updated position via `message`.
- Engage for at least 10 exchanges. Stop when the debate has genuinely matured: no new arguments appear or positions have converged.
- Call `reply` exactly once with a single `result` field containing the final report.

The final report should be concise and include:
- recommendation
- main reasoning (the strongest thread that survived the debate)
- strongest objection or dissent (what the discussant could not answer)
- what would change your mind
"
    )
}

fn dispatch_spawn(
    kind: SpawnKind,
    id: String,
    input: Value,
    session: &mut Agent,
    emit: &Emitter,
) -> SessionToolResult {
    let Args {
        prompt,
        title,
        permissions,
    } = match parse_args(&input) {
        Ok(a) => a,
        Err(reason) => return invalid_input(id, kind.tool(), INVALID_INPUT, &reason, emit),
    };
    // The child's authority is the parent's narrowed to the requested base
    // (`parent ⊓ base`), frozen against the child's cwd.  Meet only ever
    // removes authority, so a spawn never escalates.
    let cwd = session.cwd();
    let child_caps =
        match crate::policy::narrow(session.caps(), &permissions, &cwd.to_string_lossy()) {
            Ok(c) => c,
            Err(reason) => return invalid_input(id, kind.tool(), INVALID_INPUT, &reason, emit),
        };
    let child = match kind.fork(session, child_caps) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("could not fork child session: {e}");
            session.note_error(msg.clone(), emit);
            return SessionToolResult { id, content: msg };
        }
    };
    spawn_async(
        session,
        id,
        child,
        AsyncSpawn {
            tool: kind.tool(),
            title,
            prompt,
            commitment: None,
        },
        emit,
    )
}

/// Which commitment-register operation, if any, a settled spawn's structured
/// reply should be checked against — computed by
/// [`commitment::commitment_settle`](super::commitment::commitment_settle) on
/// the worker thread while it still holds the raw payload.
pub(super) enum CommitmentIntent {
    /// A `commit` writer: on a matching, well-formed card, open this key.
    Write(String),
    /// A `verify_commitment` verifier: on a matching pass, clear this key.
    Verify(String),
}

/// The tool-specific half of an async spawn: everything that varies between
/// `amnemon`/`mnemon` and `commit`/`verify_commitment`, as opposed to the
/// fork-detach-register mechanics every one of them shares ([`spawn_async`]).
pub(super) struct AsyncSpawn {
    pub tool: &'static str,
    pub title: String,
    pub prompt: String,
    /// Set only for a `commit`/`verify_commitment` spawn. `None` for every
    /// ordinary spawn, which can never tag a result for the pin register.
    pub commitment: Option<CommitmentIntent>,
}

/// Fork-then-detach: hand an already-forked, already-capped `child` to a
/// worker thread that drives it to completion off the parent's critical
/// path, and return the immediate "started" receipt every spawn tool
/// answers its dispatch with.  This is the one launch-only, always-
/// asynchronous shape `amnemon`, `mnemon`, `commit`, and `verify_commitment`
/// share; each caller differs only in how it built `child` and `spec`.
///
/// `spec.commitment`, when `Some`, marks this spawn as a `commit`/
/// `verify_commitment` check: the worker computes — while it still holds the
/// child's raw reply, before that detail is discarded — what the structured
/// reply decides, and tags the settled [`AgentResult`] so the parent applies
/// it to the protected pin register when the result drains
/// ([`Agent::drive`](crate::agent::Agent::drive)).
pub(super) fn spawn_async(
    session: &mut Agent,
    id: String,
    mut child: Agent,
    spec: AsyncSpawn,
    emit: &Emitter,
) -> SessionToolResult {
    let AsyncSpawn {
        tool,
        title,
        prompt,
        commitment,
    } = spec;
    // Capture everything off the child before it moves into the worker
    // thread: its identity, log directory, own cancellation token and
    // provider handle, the registry entry's generation, and the parent's
    // mailbox (the child's upward result edge).  The child is registered
    // under *this* agent as its parent, so the subtree cascade reaches it.
    let agent_id = child.id;
    let log_dir = child.log_dir().to_path_buf();
    let log_dir_str = log_dir.display().to_string();
    let cancel = child.cancel_token().clone();
    // The child's eval-layer cancel handle, registered so the cascade can
    // unwind a `ral` eval already in flight when the cancel lands.
    let eval_root = child.eval_root();
    // The child's inbox sender, registered so the frontend can steer or
    // wake this tab.  Cheap-clone, taken off `child` before it moves into
    // the worker thread (alongside the one the streaming `child_emit`
    // carries).
    let child_mailbox = child.mailbox();
    // The child's own provider handle (seeded at `fork` from this agent's
    // current provider), registered so a `/model` on this tab swaps the
    // child's provider alone.
    let child_provider = child.provider_handle();
    let generation = session.agents.register(
        agent_id,
        Some(session.id),
        title.clone(),
        log_dir.clone(),
        cancel,
        Some(eval_root),
        child_mailbox,
        child_provider,
    );
    let parent_mailbox = session.mailbox();
    let registry = session.agents.clone();
    // A live tab whenever the bus outlives the turn (the TUI): a real
    // emitter cloned off the session sender, stamped with the child's id
    // and carrying the child's own mailbox.  Off a per-turn bus (headless)
    // the child is muted *on the display* — an emitter whose receiver is
    // already dropped — so it never streams; but either way it carries the
    // child's own `Transcript`, so its operational trace is recorded
    // regardless of whether anyone is watching.  Its model view returns
    // through its forked `AgentLog`, its reply through the inbox.
    let child_emit = if emit.is_session_lived() {
        emit.child(agent_id, child.mailbox(), child.transcript())
    } else {
        emit.muted_child(agent_id, child.transcript())
    };
    let started = Instant::now();
    let born_title = title.clone();
    let born_parent = session.id;
    let worker_title = title.clone();
    // Seed the launch turn into the child's own inbox: the only downward
    // edge is this one write.
    child.seed(prompt.clone());
    // The dispatch shows on the rail before the spawn, so the user can see
    // exactly what the child was asked to do.
    emit.emit(Kind::ToolCall {
        tool,
        cmd: prompt,
        summary: Some(format!("Agent {title} spawned ({tool}).")),
    });
    thread::Builder::new()
        .name(format!("exarch-agent-{agent_id}"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // registered before the first token and ages out after the
            // last; on a muted emitter both are no-ops.  The id routes
            // every event to the child's own tab through the TUI's existing
            // draw path.
            child_emit.emit(Kind::Born {
                log_dir: log_dir.clone(),
                title: born_title,
                parent: born_parent,
            });
            let (outcome, payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // The child drives on its own provider handle, seeded at
                // `fork` from this agent's current provider, so a later
                // `/model` on either never disturbs the other.
                child.drive(&mut crate::agent::NoControl, &child_emit)
            }))
            .unwrap_or_else(|_| (AgentOutcome::Failed("sub-agent panicked".into()), None));
            child_emit.emit(Kind::Died);
            // A model parent reads the reply as prose in its context, so the
            // peer edge renders the faithful payload to text here.
            let text = payload
                .as_ref()
                .map(crate::agent::render_reply)
                .unwrap_or_default();
            // A commitment writer/verifier's structured reply decides here,
            // on the worker thread, while the raw payload is still in hand —
            // the parent only ever sees the tag, never the payload itself.
            let commitment_settle = commitment
                .and_then(|i| super::commitment::commitment_settle(i, &outcome, &payload));
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
                    commitment_settle,
                }));
            }
        })
        .expect("spawn async agent worker");
    let receipt = json!({
        "id": agent_id,
        "title": title,
        "status": "started",
        "log_dir": log_dir_str,
        "note": "If you have nothing to do before the agent returns, please yield to the user.",
    });
    SessionToolResult {
        id,
        content: receipt.to_string(),
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
        session: &mut Agent,
        _provider: &Arc<Provider>,
        _emit: &Emitter,
    ) -> SessionToolResult {
        // A silent tool: an argument-less recovery poll the orchestrator runs
        // to recover ids after a compaction, not a user-facing action.  It
        // emits no `ToolCall`/`ToolResult`, so it leaves no trace on the rail
        // (or the headless frontend, or `transcript.jsonl`) — the model still
        // receives the listing through the returned result below.
        let live = session.agents.list(session.id);
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
        SessionToolResult { id, content }
    }
}

/// `message` — send a marked model-visible note to another live agent.
pub(super) struct MessageTool;

impl Tool for MessageTool {
    fn name(&self) -> &'static str {
        "message"
    }

    fn desc(&self) -> &'static str {
        "Send a short message to another live agent by id.  The recipient sees \
it as a marked turn, not as human input, at its next tool boundary.  Use ids \
from `agents`, spawn receipts, or ids you deliberately included in a child's \
prompt.  This is for coordination only: it does not return the recipient's \
answer; use `reply` for a returning agent's final result."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The id of the live agent to receive the message.",
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to deliver to that agent.",
                    },
                },
                "required": ["id", "message"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let recipient_id = match u64_field(&input, "id") {
            Some(n) => n,
            None => {
                return invalid_input(
                    id,
                    "message",
                    INVALID_INPUT,
                    "missing required integer field `id`",
                    emit,
                );
            }
        };
        let Some(message) = input
            .as_object()
            .and_then(|o| o.get("message"))
            .and_then(Value::as_str)
        else {
            return invalid_input(
                id,
                "message",
                INVALID_INPUT,
                "missing required string field `message`",
                emit,
            );
        };
        let recipient_title = session.agents.title_for(recipient_id);
        emit.emit(Kind::ToolCall {
            tool: "message",
            cmd: message.to_string(),
            summary: Some(match &recipient_title {
                Some(t) => format!("Messaged agent {t}."),
                None => format!("Messaged agent {recipient_id}."),
            }),
        });
        let content = match session
            .agents
            .message(session.id, recipient_id, message.to_string())
        {
            Ok(()) => match &recipient_title {
                Some(t) => format!("sent message to agent {recipient_id} ({t})"),
                None => format!("sent message to agent {recipient_id}"),
            },
            Err(MessageError::UnknownRecipient(n)) => {
                format!("no live agent with id {n}; did it finish already?")
            }
            Err(MessageError::UnknownSender(n)) => {
                format!("cannot send from agent {n}: it is no longer live")
            }
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
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let agent_id = match u64_field(&input, "id") {
            Some(n) => n,
            None => {
                return invalid_input(
                    id,
                    "agent_cancel",
                    INVALID_INPUT,
                    "missing required integer field `id`",
                    emit,
                );
            }
        };
        let agent_title = session.agents.title_for(agent_id);
        emit.emit(Kind::ToolCall {
            tool: "agent_cancel",
            cmd: format!("cancelling agent {agent_id}"),
            summary: Some(match &agent_title {
                Some(t) => format!("Agent {t} cancelled."),
                None => format!("Agent {agent_id} cancelled."),
            }),
        });
        let content = if session.agents.cancel(agent_id) {
            match &agent_title {
                Some(t) => format!("cancelling agent {agent_id} ({t})"),
                None => format!("cancelling agent {agent_id}"),
            }
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
