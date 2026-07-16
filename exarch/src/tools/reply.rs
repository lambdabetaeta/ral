//! `reply` tool — the JSON-schema surface for a returning agent's
//! deliberate return value, kept installed beside the `reply` *builtin*
//! (`crate::agent_builtins::harness`) as a measurement-window coexistence:
//! the builtin is the product surface, this tool the retiring one
//! (`dev/docs/260715_tools_to_builtins.md`).
//!
//! A returning agent — every sub-agent, and the headless root — hands its
//! result back by *constructing* it: it calls `reply` with the value and the
//! run hard-terminates.  There is no scrape of the final prose step; `reply` is
//! the sole return path, and so it is withheld only from the *interactive* root
//! (which talks to the user across turns and never "returns").
//!
//! The dispatch converts the model's JSON `result` through [`json_fo`] —
//! the coexistence projection into [`ral_core::serial::FOValue`], the type
//! [`Agent::set_reply`] and [`crate::agent::TurnOutcome::Replied`] carry
//! end to end — and stashes it for the drive loop to turn into a `Replied`
//! terminal once the whole tool-call batch drains.  Each consumer renders
//! it at its own edge by the shared value→text rule
//! ([`shell_eval::ral_value_to_text`] over [`ral_core::Value::from`]: a
//! top-level string passes through raw, so a markdown report keeps real
//! newlines; a structured value renders in ral surface syntax) — except the
//! headless harness, which projects the structure faithfully through
//! [`shell_eval::user_json`].  This tool's own rail display stays JSON-typed
//! ([`shell_eval::json_to_text`]), since its input arrives as JSON.

use super::Tool;
use crate::agent::Agent;
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use crate::shell_eval::{self, json_fo};
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

    fn gate(&self) -> super::Gate {
        super::Gate::Returns
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
        session: &mut Agent,
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
            summary: Some("Replied to parent.".to_string()),
        });
        // Stash the faithful value for `apply` to lift into a `Replied`
        // terminal once the batch drains.  The tool result itself is never seen
        // by the model (the run ends here); it exists only so the transcript
        // stays role-alternating (every dispatched call gets a result).  The
        // `reply` builtin's payload arrives already first-order; this JSON
        // tool's own arrives as JSON and crosses the coexistence seam through
        // `json_fo` — the mandated transitional projection that dies with
        // this tool at retirement.
        session.set_reply(json_fo(&value));
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
}
