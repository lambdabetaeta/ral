//! `ral` — the one tool the provider is offered: evaluate ral source against
//! the session's live shell, synchronously on the dispatching thread.  Input
//! that does not parse never reaches the session; it becomes an error block.

use crate::agent::Agent;
use crate::agent::event::ToolResult as SessionToolResult;
use crate::bus::{Emitter, Kind};
use crate::record::{BlockId, Display};
use serde_json::{Value, json};
use std::sync::OnceLock;

/// The name on the wire; `Agent::invoke` recognises no other.
pub(crate) const NAME: &str = "ral";

const DESC: &str = "Run a ral shell command in the sandboxed working directory.";

#[cfg_attr(test, derive(Debug))]
struct RalArgs {
    cmd: String,
    /// The rail's collapsed label, with the full `cmd` behind it.
    description: String,
    timeout_secs: u64,
}

/// Default wall bound on one call.  A default, not a cap: a call may raise it,
/// and nothing clamps the value it names.
const CALL_TIMEOUT_SECS: u64 = 60;

/// Character cap on `description`; oversize input truncates rather than rejects.
const DESCRIPTION_MAX: usize = 60;

/// Parse the model's JSON, or a reason short enough for the rail's error block.
fn parse_args(input: &Value) -> Result<RalArgs, String> {
    let obj = input
        .as_object()
        .ok_or_else(|| "tool input is not a JSON object".to_string())?;
    let cmd = obj
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `cmd`".to_string())?
        .to_string();
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string field `description`".to_string())?
        .trim()
        .to_string();
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
    let timeout_secs = match obj.get("timeout_secs") {
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

fn schema() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "The ral source to evaluate." },
                "description": {
                    "type": "string",
                    "maxLength": DESCRIPTION_MAX,
                    "description": "One line (≤60 chars) stating the script's \
        intent — what it is for, not what it types. Present continuous, e.g. \
        \"Counting TODOs across src/.\". Shown on the rail; no newlines. Do not echo the source, or the mechanics of ral.",
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Wall-clock limit in seconds; default 60. \
        Raise it for a command known to run long, rather than defer-then-wait. \
        Keep defer for work you can overlap.",
                },
            },
            "required": ["cmd", "description"],
        })
    })
}

/// The definition `tool_defs` in `provider/request.rs` puts on the wire.
pub(crate) fn wire_tool() -> genai::chat::Tool {
    genai::chat::Tool::new(NAME)
        .with_description(DESC)
        .with_schema(schema().clone())
}

/// Sentinel `cmd` for a call that did not parse.  `tui::app` renders no
/// block for it — it exists only as the boundary its error result attaches to.
pub(crate) const INVALID_INPUT: &str = "<invalid input>";

/// Rail header and error block for a malformed call, and the result to commit.
fn invalid_input(id: String, reason: &str, session: &Agent, emit: &Emitter) -> SessionToolResult {
    let call = record_call(session, INVALID_INPUT.to_string(), None, emit);
    emit.emit(Kind::ToolCall {
        tool: NAME,
        cmd: INVALID_INPUT.to_string(),
        summary: None,
    });
    let msg = format!("tool input error: {reason}\nexpected an object matching the tool's schema");
    record_result(session, &msg, call, emit);
    emit.emit(Kind::ToolResult(msg.clone()));
    SessionToolResult { id, content: msg }
}

/// Record the call's display commit through the seam, keeping the witnessed
/// [`BlockId`] so the paired result can address it directly — the mechanism
/// that retires the view's tail-walk.  A failed append surfaces as an error
/// row; the paired result is then simply not recorded, having no target.
fn record_call(
    session: &Agent,
    cmd: String,
    summary: Option<String>,
    emit: &Emitter,
) -> Option<BlockId> {
    match session.recorder().emit(Display::ToolCall {
        tool: NAME.to_string(),
        cmd,
        summary,
    }) {
        Ok(recorded) => Some(BlockId::new(recorded.stamp().seq())),
        Err(error) => {
            emit.emit(Kind::Error(format!(
                "a tool-call commit was not recorded in record.jsonl: {error}"
            )));
            None
        }
    }
}

/// The paired half: the byte-identical result string, addressed at the call
/// commit it answers.
fn record_result(session: &Agent, content: &str, call: Option<BlockId>, emit: &Emitter) {
    let Some(call) = call else { return };
    if let Err(error) = session.recorder().emit(Display::Result {
        text: content.to_string(),
        call,
    }) {
        emit.emit(Kind::Error(format!(
            "a tool-result commit was not recorded in record.jsonl: {error}"
        )));
    }
}

/// Parse, announce on the rail, run.  Always yields a result: the provider
/// protocol pairs one with every call, malformed or not.
pub(crate) fn dispatch(
    id: String,
    input: &Value,
    session: &mut Agent,
    emit: &Emitter,
) -> SessionToolResult {
    let args = match parse_args(input) {
        Ok(a) => a,
        Err(reason) => return invalid_input(id, &reason, session, emit),
    };
    let call = record_call(session, args.cmd.clone(), Some(args.description.clone()), emit);
    emit.emit(Kind::ToolCall {
        tool: NAME,
        cmd: args.cmd.clone(),
        summary: Some(args.description.clone()),
    });
    let result = session.run_shell(id, &args.cmd, args.timeout_secs, emit);
    record_result(session, &result.content, call, emit);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_cmd_and_description() {
        let a = parse_args(&json!({
            "cmd": "pwd",
            "description": "Printing the working directory.",
        }))
        .unwrap();
        assert_eq!(a.cmd, "pwd");
        assert_eq!(a.description, "Printing the working directory.");
    }

    #[test]
    fn parse_rejects_missing_cmd() {
        let e = parse_args(&json!({
            "description": "noop",
        }))
        .unwrap_err();
        assert!(e.contains("`cmd`"));
    }

    #[test]
    fn parse_rejects_non_object() {
        let e = parse_args(&json!("oops")).unwrap_err();
        assert!(e.contains("not a JSON object"));
    }

    #[test]
    fn parse_rejects_missing_description() {
        let e = parse_args(&json!({ "cmd": "pwd" })).unwrap_err();
        assert!(e.contains("`description`"));
    }

    #[test]
    fn parse_rejects_empty_description() {
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": "   ",
        }))
        .unwrap_err();
        assert!(e.contains("`description`"));
    }

    #[test]
    fn parse_truncates_oversize_description() {
        let big = "x".repeat(DESCRIPTION_MAX + 1);
        let a = parse_args(&json!({
            "cmd": "pwd",
            "description": big,
        }))
        .unwrap();
        assert_eq!(a.description, "x".repeat(DESCRIPTION_MAX) + "...");
    }

    #[test]
    fn parse_rejects_multiline_description() {
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": "first\nsecond",
        }))
        .unwrap_err();
        assert!(e.contains("`description`"));
    }

    #[test]
    fn parse_timeout_defaults_when_absent() {
        let a = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
        }))
        .unwrap();
        assert_eq!(a.timeout_secs, CALL_TIMEOUT_SECS);
    }

    /// No upper clamp: an explicit bound is taken verbatim.
    #[test]
    fn parse_timeout_accepts_explicit() {
        let a = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
            "timeout_secs": 300,
        }))
        .unwrap();
        assert_eq!(a.timeout_secs, 300);
    }

    #[test]
    fn parse_timeout_null_takes_default() {
        let a = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
            "timeout_secs": null,
        }))
        .unwrap();
        assert_eq!(a.timeout_secs, CALL_TIMEOUT_SECS);
    }

    /// Zero parses as a `u64`, so the floor has to be a check of its own.
    #[test]
    fn parse_timeout_rejects_zero() {
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
            "timeout_secs": 0,
        }))
        .unwrap_err();
        assert!(e.contains("`timeout_secs`"));
        assert!(e.contains("≥1"));
    }

    #[test]
    fn parse_timeout_rejects_negative() {
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
            "timeout_secs": -5,
        }))
        .unwrap_err();
        assert!(e.contains("`timeout_secs`"));
        assert!(e.contains("positive integer"));
    }

    /// Even an integral float like `5.0` is not a `u64` to serde.
    #[test]
    fn parse_timeout_rejects_float() {
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
            "timeout_secs": 5.0,
        }))
        .unwrap_err();
        assert!(e.contains("`timeout_secs`"));
        assert!(e.contains("positive integer"));
    }

    #[test]
    fn parse_timeout_rejects_string() {
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
            "timeout_secs": "30",
        }))
        .unwrap_err();
        assert!(e.contains("`timeout_secs`"));
        assert!(e.contains("positive integer"));
    }
}
