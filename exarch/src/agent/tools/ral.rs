//! `ral` — the tool every agent is offered: evaluate ral source against the
//! session's live shell, synchronously on the dispatching thread.  Input
//! that does not parse never reaches the session; it becomes an error block.

use super::{input_error, required_str};
use crate::agent::Avatar;
use crate::bus::Emitter;
use crate::provider::tools::{CALL_TIMEOUT_SECS, DESCRIPTION_MAX, RAL};
use crate::record::ToolResult as SessionToolResult;
use crate::record::{BlockId, Display};
use serde_json::Value;

#[cfg_attr(test, derive(Debug))]
struct RalArgs {
    cmd: String,
    /// The rail's collapsed label, with the full `cmd` behind it.
    description: String,
    timeout_secs: u64,
}

/// Parse the model's JSON, or a reason short enough for the rail's error block.
fn parse_args(input: &Value) -> Result<RalArgs, String> {
    let cmd = required_str(input, "cmd")?.to_string();
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
        cmd,
        description,
        timeout_secs,
    })
}

/// Sentinel `cmd` for a call that did not parse.  `tui::app` renders no
/// block for it — it exists only as the boundary its error result attaches to.
pub(crate) const INVALID_INPUT: &str = "<invalid input>";

/// Rail header and error block for a malformed call, and the result to commit.
fn invalid_input(id: String, reason: &str, session: &Avatar) -> SessionToolResult {
    let call = record_call(session, INVALID_INPUT.to_string(), None);
    let msg = input_error(reason);
    record_result(session, &msg, true, call);
    SessionToolResult { id, content: msg }
}

/// Record the call's display commit through the seam, keeping the witnessed
/// [`BlockId`] so the paired result can address it directly — the mechanism
/// that retires the view's tail-walk.  A failed append surfaces as an error
/// row; the paired result is then simply not recorded, having no target.
fn record_call(session: &Avatar, cmd: String, summary: Option<String>) -> Option<BlockId> {
    match session.recorder().emit(Display::ToolCall {
        tool: RAL.name.to_string(),
        cmd,
        summary,
    }) {
        Ok(recorded) => Some(BlockId::new(recorded.locus().seq())),
        Err(error) => {
            session.recorder().report_fault(&error);
            None
        }
    }
}

/// The paired half: the byte-identical result string, addressed at the call
/// commit it answers.
fn record_result(session: &Avatar, content: &str, failed: bool, call: Option<BlockId>) {
    let Some(call) = call else { return };
    if let Err(error) = session.recorder().emit(Display::Result {
        text: content.to_string(),
        failed,
        call,
    }) {
        session.recorder().report_fault(&error);
    }
}

/// Parse, announce on the rail, run.  Always yields a result: the provider
/// protocol pairs one with every call, malformed or not.
pub(crate) fn dispatch(
    id: String,
    input: &Value,
    session: &Avatar,
    emit: &Emitter,
) -> SessionToolResult {
    let args = match parse_args(input) {
        Ok(a) => a,
        Err(reason) => return invalid_input(id, &reason, session),
    };
    let call = record_call(session, args.cmd.clone(), Some(args.description.clone()));
    let crate::agent::Evaluated { text, failed } = session.ral(&args.cmd, args.timeout_secs, emit);
    record_result(session, &text, failed, call);
    SessionToolResult { id, content: text }
}

#[cfg(test)]
mod tests;
