//! Dispatch of the tools a request advertises ([`crate::provider::tools`]):
//! `ral`, and `thinking` under `--thinking-tool`.
//!
//! Everything else the model reaches — spawning a sub-agent, messaging one,
//! scheduling a wakeup, replying — is an ordinary ral builtin in
//! `shell_eval/builtins/harness.rs`, answered by [`crate::agent::desk`].

use crate::agent::Avatar;
use crate::bus::Emitter;
use crate::provider::tools::{RAL, THINKING};
use crate::record::ToolResult;
use serde_json::Value;

pub(crate) mod ral;
pub(crate) mod thinking;

/// Answer one call to a tool this agent was offered; `None` for a name it was not.
pub(crate) fn dispatch(
    name: &str,
    id: String,
    input: &Value,
    session: &Avatar,
    emit: &Emitter,
) -> Option<ToolResult> {
    match name {
        n if n == RAL.name => Some(ral::dispatch(id, input, session, emit)),
        n if n == THINKING.name => Some(thinking::dispatch(id, input, session, emit)),
        _ => None,
    }
}

/// A required string field of the model's input, or the reason it is not one.
pub(crate) fn required_str<'a>(input: &'a Value, field: &str) -> Result<&'a str, String> {
    input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string field `{field}`"))
}

/// The result a malformed call earns.
pub(crate) fn input_error(reason: &str) -> String {
    format!("tool input error: {reason}\nexpected an object matching the tool's schema")
}
