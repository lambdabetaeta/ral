//! Dispatch of the tools a request advertises ([`crate::provider::tools`]):
//! `ral`, and `thinking` under `--thinking-tool`.
//!
//! Everything else the model reaches — spawning a sub-agent, messaging one,
//! scheduling a wakeup, replying — is an ordinary ral builtin in
//! `shell_eval/builtins/harness.rs`, answered by [`crate::agent::desk`].

use crate::agent::Agent;
use crate::bus::Emitter;
use crate::provider::tools::{RAL, THINKING};
use crate::record::ToolResult;
use ral_core::process::Scope;
use serde_json::Value;

pub(crate) mod ral;
pub(crate) mod thinking;

/// Answer one call to a tool this agent was offered; `None` for a name it was not.
pub(crate) fn dispatch(
    name: &str,
    id: String,
    input: &Value,
    session: &Agent,
    exchange: &Scope,
    emit: &Emitter,
) -> Option<ToolResult> {
    match name {
        n if n == RAL.name => Some(ral::dispatch(id, input, session, exchange, emit)),
        n if n == THINKING.name => Some(thinking::dispatch(id, input, session, emit)),
        _ => None,
    }
}

/// A required string field of the model's input, or the reason it is not one.
/// The reason says what was received, so the model can correct the call: the
/// fields it sent when the field is absent, the field's name when it is not a
/// string.
pub(crate) fn required_str<'a>(input: &'a Value, field: &str) -> Result<&'a str, String> {
    let object = input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?;
    match object.get(field) {
        None => {
            let received = if object.is_empty() {
                "no fields".to_string()
            } else {
                object
                    .keys()
                    .map(|key| format!("`{key}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Err(format!(
                "missing required string field `{field}`; got {received}"
            ))
        }
        Some(value) => value
            .as_str()
            .ok_or_else(|| format!("field `{field}` must be a string")),
    }
}

/// The result a malformed call earns.
pub(crate) fn input_error(reason: &str) -> String {
    format!("tool input error: {reason}\nexpected an object matching the tool's schema")
}
