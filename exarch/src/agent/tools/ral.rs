//! `ral` — the tool every agent is offered: evaluate ral source against the
//! session's live shell, synchronously on the dispatching thread.  Input
//! that does not parse never reaches the session; it becomes an error block.

use super::{input_error, required_str};
use crate::agent::Agent;
use crate::bus::Emitter;
use crate::provider::tools::{CALL_TIMEOUT_SECS, COMMAND, DESCRIPTION_MAX, RAL};
use crate::record::ToolResult as SessionToolResult;
use crate::record::{BlockId, Display, Unavailable, Verdict};
use ral_core::process::Scope;
use serde_json::Value;

#[cfg_attr(test, derive(Debug))]
struct RalArgs {
    command: String,
    /// The rail's collapsed label, with the full `command` behind it.
    description: String,
    timeout_secs: u64,
}

/// Parse the model's JSON, or a reason short enough for the rail's error block.
fn parse_args(input: &Value) -> Result<RalArgs, String> {
    let command = required_str(input, COMMAND)?.to_string();
    let description = required_str(input, "description")?.trim().to_string();
    if description.is_empty() {
        return Err("`description` must be non-empty".to_string());
    }
    if description.contains('\n') {
        return Err("`description` must be a single line (no newlines)".to_string());
    }
    let description = if description.chars().count() > DESCRIPTION_MAX {
        description
            .chars()
            .take(DESCRIPTION_MAX)
            .collect::<String>()
            + "..."
    } else {
        description
    };
    // Absent or `null` takes the default, anything else must be a `u64`: a bare
    // `as_u64` lookup could not tell those apart from present-but-junk.
    let timeout_secs = match input.get("timeout_secs") {
        None | Some(Value::Null) => CALL_TIMEOUT_SECS,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| "`timeout_secs` must be a positive integer".to_string())?;
            if n < 1 {
                return Err("`timeout_secs` must be ≥1".to_string());
            }
            n
        }
    };
    Ok(RalArgs {
        command,
        description,
        timeout_secs,
    })
}

/// Sentinel `cmd` for a call that did not parse.  `tui::app` renders no
/// block for it — it exists only as the boundary its error result attaches to.
pub(crate) const INVALID_INPUT: &str = "<invalid input>";

/// Rail header and error block for a malformed call, and the result to commit.
fn invalid_input(id: String, reason: &str, session: &Agent) -> SessionToolResult {
    let msg = input_error(reason);
    if let Ok(call) = record_call(session, INVALID_INPUT.to_string(), None) {
        record_result(session, &msg, true, call);
    }
    SessionToolResult { id, content: msg }
}

/// Record the call's display record; the witnessed [`BlockId`] addresses its
/// paired result. A call whose row cannot be recorded is not made.
fn record_call(
    session: &Agent,
    cmd: String,
    summary: Option<String>,
) -> Result<BlockId, Unavailable> {
    session
        .recorder()
        .emit(Display::ToolCall {
            tool: RAL.name.to_string(),
            cmd,
            summary,
        })
        .map(|recorded| BlockId::new(recorded.locus().seq()))
}

/// The paired half: what the result told, addressed at the call record it
/// answers.
fn record_result(session: &Agent, content: &str, failed: bool, call: BlockId) {
    session.recorder().emit_or_end(Display::Verdict {
        call,
        verdict: Verdict::of(content, failed),
    });
}

/// Parse, announce on the rail, run.  Always yields a result: the provider
/// protocol pairs one with every call, malformed or not.
pub(crate) fn dispatch(
    id: String,
    input: &Value,
    session: &Agent,
    exchange: &Scope,
    emit: &Emitter,
) -> SessionToolResult {
    let RalArgs {
        command,
        description,
        timeout_secs,
    } = match parse_args(input) {
        Ok(args) => args,
        Err(reason) => return invalid_input(id, &reason, session),
    };
    let call = match record_call(session, command.clone(), Some(description)) {
        Ok(call) => call,
        Err(failure) => {
            return SessionToolResult {
                id,
                content: failure.to_string(),
            };
        }
    };
    let crate::agent::Evaluated { text, failed } =
        session.ral(exchange, &command, timeout_secs, emit);
    record_result(session, &text, failed, call);
    SessionToolResult { id, content: text }
}

#[cfg(test)]
mod tests;
