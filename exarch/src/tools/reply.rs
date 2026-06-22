//! `reply` tool — a sub-agent's deliberate return value.
//!
//! A child hands its parent a value by *constructing* it: it calls `reply`
//! with its result and the run hard-terminates.  There is no scrape of the
//! final prose step — `reply` is the only way a sub-agent returns, and so it
//! is withheld from the root (which talks to the user across turns and never
//! "returns").  Finishing-and-returning is an agent-protocol act, a peer of
//! spawning (`agent`), which is why it is a tool and not a ral builtin.
//!
//! The argument is rendered by the shared value→text rule
//! ([`shell_eval::json_to_text`]): a JSON string passes through raw (a markdown
//! report keeps real newlines), an object/array is pretty-printed.  The
//! dispatch stashes the rendered payload on the session and the drive loop
//! turns it into a `Replied` terminal once the whole tool-call batch drains.

use super::Tool;
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use crate::session::Session;
use crate::shell_eval;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

pub(super) struct ReplyTool;

impl Tool for ReplyTool {
    fn name(&self) -> &'static str {
        "reply"
    }

    fn desc(&self) -> &'static str {
        "Return your result to whoever spawned you, and end your run.  This is \
the ONLY way you hand work back: your parent receives the `result` you pass \
here and nothing else — not your reasoning, your shell bindings, or the prose \
you streamed along the way.  Call it exactly once, when you are done.\n\n\
`result` is normally a markdown report (a string; newlines are preserved).  \
For structured findings — a list of files, a table of results — pass a JSON \
object or array instead and it is rendered legibly.  Calling `reply` \
hard-terminates you the moment the current batch of tool calls finishes, so do \
any remaining work in earlier calls; there is no turn after it.  If you finish \
without calling `reply`, your parent receives nothing."
    }

    fn replies(&self) -> bool {
        true
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "result": {
                        "description": "Your result for the parent.  A markdown \
            report (string) for prose, or a JSON object/array for structured \
            findings.  This is the entire value your parent receives.",
                    },
                },
                "required": ["result"],
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
        // The payload is the `result` field, rendered by the shared value→text
        // rule.  A missing field, a JSON null, or an empty string all render
        // to nothing and settle as `AgentOutcome::Empty` downstream.
        let payload = input
            .get("result")
            .and_then(shell_eval::json_to_text)
            .unwrap_or_default();
        // Show the reply on the child's own rail before it terminates; the
        // full payload opens in the collapsible block.
        emit.emit(Kind::ToolCall {
            tool: "reply",
            cmd: if payload.is_empty() {
                "(empty reply)".into()
            } else {
                payload.clone()
            },
            summary: None,
        });
        // Stash it for `apply` to lift into a `Replied` terminal once the
        // batch drains.  The tool result itself is never seen by the model
        // (the run ends here); it exists only so the transcript stays
        // role-alternating (every dispatched call gets a result).
        session.set_reply(payload);
        let content = "reply recorded; ending turn".to_string();
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema declares a required `result` with no type constraint, so the
    /// model may pass a string or a structured value.
    #[test]
    fn schema_requires_result() {
        let s = ReplyTool.schema();
        assert_eq!(s["required"][0], "result");
        assert!(s["properties"]["result"].is_object());
    }

    /// `reply` is the one tool that flips the `replies` axis; it does not
    /// spawn.
    #[test]
    fn reply_flips_only_the_reply_axis() {
        assert!(ReplyTool.replies());
        assert!(!ReplyTool.spawns());
    }
}
