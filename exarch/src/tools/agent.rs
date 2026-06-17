//! `agent` tool — fork a child session and run a sub-agent prompt.
//!
//! [`Staged::Spawned`]; the parent joins same-batch children before the next
//! provider request, so sibling agents can overlap but cannot outlive the
//! parent turn.
//! The full prompt — every line — is rendered to the rail before spawn, so the
//! user can see what the child was asked to do.

use super::{Tool, invalid_input};
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::{Provider, ProviderError};
use crate::session::{Session, Staged, TurnOutcome};
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

pub(super) struct AgentTool;

/// Monotonic dispatch counter, used to mint `sub-{N}` fallback titles
/// when the model omits one.  Lives as long as the process — the TUI
/// only needs each child's label to be distinguishable from its
/// siblings on screen.
static DISPATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Parsed Agent tool input: the child's prompt and a validated tab
/// label.  Splitting the parse from dispatch keeps validation testable
/// in isolation.
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
spawn sub-agents."
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
        provider: &'env Provider,
        token: &'env crate::cancel::Token,
        emit: &Emitter,
        scope: &'scope thread::Scope<'scope, 'env>,
    ) -> Staged<'scope> {
        let Args { prompt, title } = match parse_args(&input) {
            Ok(a) => a,
            Err(reason) => return invalid_input(id, "agent", "<invalid input>", &reason, emit),
        };
        let mut child = match session.fork() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not fork child session: {e}");
                session.note_error(msg.clone(), emit);
                return Staged::Done(SessionToolResult { id, content: msg });
            }
        };
        emit.emit(Kind::ToolCall {
            tool: "agent",
            cmd: prompt.clone(),
            summary: Some(title.clone()),
        });
        let child_emit = emit.child(child.id);
        child_emit.emit(Kind::Born {
            log_dir: child.log_dir().to_path_buf(),
            title: title.clone(),
        });
        // Children do not nudge — the top-level driver owns that.
        // Collapse the child outcome to a reply string here so the
        // parent's join phase sees a single shape, and emit a final
        // `SubagentDone` on the parent's channel so the TUI can land a
        // completion breadcrumb in root's scrollback (where subagent
        // tabs leave no trace once their linger window expires).
        let parent_emit = emit.clone();
        let started = Instant::now();
        // The child shares the parent's cancellation token rather than
        // minting its own — an Esc cancels the whole spawn tree.
        let child_token = token.clone();
        let handle = scope.spawn(move || {
            // A child's `apply` runs on this scoped thread, outside the root
            // turn's `catch_unwind` (`bus::pump`), so a mid-eval panic here
            // would otherwise skip the two emits below — leaking the child's
            // TUI tab (no `Died` to age it out) and dropping the breadcrumb.
            // Recover the panic into an `Err` so `Died`/`SubagentDone` always
            // fire and the parent's join sees the usual single shape.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                child.apply(provider, Some(prompt), &child_token, &child_emit)
            }))
            .unwrap_or_else(|p| {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
                    .unwrap_or_else(|| "non-string payload".into());
                Err(ProviderError::Other(format!("sub-agent panicked: {msg}")))
            });
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
                TurnOutcome::Stopped { reason } => {
                    format!("(child stopped: {reason})")
                }
                TurnOutcome::Cancelled => "(child cancelled)".into(),
                TurnOutcome::Capped => "(child stopped: step cap reached)".into(),
            })
        });
        Staged::Spawned { id, handle }
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
