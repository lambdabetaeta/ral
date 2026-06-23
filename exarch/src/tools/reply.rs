//! `reply` tool — a returning agent's deliberate return value.
//!
//! A returning agent — every sub-agent, and the headless root — hands its
//! result back by *constructing* it: it calls `reply` with the value and the
//! run hard-terminates.  There is no scrape of the final prose step; `reply` is
//! the sole return path, and so it is withheld only from the *interactive* root
//! (which talks to the user across turns and never "returns").
//! Finishing-and-returning is an agent-protocol act, a peer of spawning
//! (`agent`), which is why it is a tool and not a ral builtin.
//!
//! The dispatch stashes the *faithful* value the model passed and the drive
//! loop turns it into a `Replied` terminal once the whole tool-call batch
//! drains.  Each consumer renders it at its own edge by the shared value→text
//! rule ([`shell_eval::json_to_text`]: a JSON string passes through raw, so a
//! markdown report keeps real newlines; an object/array is pretty-printed) —
//! except the headless harness, which writes the structure faithfully.  The
//! rail shows that render as the call's display line.

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
        // The payload is the `result` field, stashed as the faithful value the
        // model passed so each consumer renders it at its own edge (a missing
        // field is a JSON null).  The rail display is the value→text render of
        // it; a null or empty render settles as `AgentOutcome::Empty`.
        let value = input.get("result").cloned().unwrap_or(Value::Null);
        let display = shell_eval::json_to_text(&value).unwrap_or_default();
        // Show the reply on the child's own rail before it terminates; the
        // full payload opens in the collapsible block.
        emit.emit(Kind::ToolCall {
            tool: "reply",
            cmd: if display.is_empty() {
                "(empty reply)".into()
            } else {
                display
            },
            summary: None,
        });
        // Stash the faithful value for `apply` to lift into a `Replied`
        // terminal once the batch drains.  The tool result itself is never seen
        // by the model (the run ends here); it exists only so the transcript
        // stays role-alternating (every dispatched call gets a result).
        session.set_reply(value);
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
