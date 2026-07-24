//! `ral` — the one tool the provider is offered: evaluate ral source
//! against the session's live shell.
//!
//! Runs synchronously on the dispatching thread.  Owns its own JSON
//! parsing: a missing `cmd` or a non-object input each becomes an inline
//! error block on the rail with no detour through the session.

use crate::agent::Agent;
use crate::agent::event::ToolResult as SessionToolResult;
use crate::bus::{Emitter, Kind};
use serde_json::{Value, json};
use std::sync::OnceLock;

/// Stable identifier the model and the wire schema use to name this tool.
pub(crate) const NAME: &str = "ral";

/// One-paragraph description handed to the provider.
const DESC: &str = "Run a ral shell command in the sandboxed working directory.";

/// Parsed ral-tool input, split from `dispatch` so the parser stays
/// trivially testable in isolation.
#[cfg_attr(test, derive(Debug))]
struct RalArgs {
    cmd: String,
    /// Short, one-line summary of what this call does.  Shown to the
    /// user on the rail; the full `cmd` is revealed when they open the
    /// call.
    description: String,
    /// Wall-clock limit in seconds for this call.  Absent or `null` in
    /// the input defaults to [`CALL_TIMEOUT_SECS`]; a caller raises it
    /// for a command known to run long.
    timeout_secs: u64,
}

/// Default wall-clock bound on a single inline `ral` call, applied when
/// the call omits `timeout_secs`.  It is the default, not a cap: a call
/// may set `timeout_secs` higher for work known to run long, and there
/// is no upper clamp.  Work you can overlap with other progress still
/// belongs in a `spawn`ed block whose handle is polled and awaited
/// across turns.
const CALL_TIMEOUT_SECS: u64 = 60;

/// Soft cap on `description` length.  The field is a one-line label
/// for the user-facing rail, not a paragraph; an oversize input is
/// truncated to this many characters with a trailing `...` rather than
/// rejected, so a verbose call still succeeds.
const DESCRIPTION_MAX: usize = 60;

/// Read the JSON object the model emitted into a [`RalArgs`], or a
/// short reason string suitable for the rail's error block.
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
    // `timeout_secs` is optional. Inspect the raw value directly rather than
    // a bare `as_u64` lookup: `as_u64` alone cannot tell absent (→ default)
    // from a present-but-invalid value (→ reject). Absent or `null` takes the
    // default; anything else must parse as a `u64` (so floats like `5.0`,
    // strings, and negatives all reject) and be ≥1. No upper clamp.
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

/// JSON schema for `ral`'s input object.  Built once and cached in a
/// `static`; cheap to call.
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

/// The wire-facing definition `provider.complete` advertises when this
/// agent's tool is enabled — a plain data value, not a `dyn` trait object:
/// there is exactly one tool, so no vtable or registry earns its keep.
pub(crate) fn wire_tool() -> genai::chat::Tool {
    genai::chat::Tool::new(NAME)
        .with_description(DESC)
        .with_schema(schema().clone())
}

/// The placeholder a malformed call passes for `display` when the JSON did
/// not even parse into args — a sentinel the frontend reads to route such a
/// call to an invisible boundary rather than render a stand-in token: there
/// is nothing meaningful to show, only the error the model receives through
/// the result body.  A call that *did* recover a real offending value (a bad
/// cron string, say) passes that value instead, and it renders.
pub(crate) const INVALID_INPUT: &str = "<invalid input>";

/// Render the rail header and an error block for a malformed `ral` call,
/// returning the [`SessionToolResult`] the dispatcher commits. `display` is
/// the partial label the rail should show — typically the field that did
/// parse, or [`INVALID_INPUT`] when nothing did.
pub(crate) fn invalid_input(
    id: String,
    display: &str,
    reason: &str,
    emit: &Emitter,
) -> SessionToolResult {
    emit.emit(Kind::ToolCall {
        tool: NAME,
        cmd: display.to_string(),
        summary: None,
    });
    let msg = format!("tool input error: {reason}\nexpected an object matching the tool's schema");
    emit.emit(Kind::ToolResult(msg.clone()));
    SessionToolResult { id, content: msg }
}

/// Read `input`, render the rail header, run the call, and return its
/// result.  Malformed input is reported by [`invalid_input`], so this
/// always produces a result.
pub(crate) fn dispatch(
    id: String,
    input: &Value,
    session: &mut Agent,
    emit: &Emitter,
) -> SessionToolResult {
    let args = match parse_args(input) {
        Ok(a) => a,
        Err(reason) => return invalid_input(id, INVALID_INPUT, &reason, emit),
    };
    emit.emit(Kind::ToolCall {
        tool: NAME,
        cmd: args.cmd.clone(),
        summary: Some(args.description.clone()),
    });
    session.run_shell(id, &args.cmd, args.timeout_secs, emit)
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

    /// An absent `timeout_secs` takes the default bound.
    #[test]
    fn parse_timeout_defaults_when_absent() {
        let a = parse_args(&json!({
            "cmd": "pwd",
            "description": "noop",
        }))
        .unwrap();
        assert_eq!(a.timeout_secs, CALL_TIMEOUT_SECS);
    }

    /// An explicit positive integer is taken verbatim, with no upper clamp.
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

    /// An explicit `null` is treated as absent and takes the default.
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

    /// Zero parses as a `u64` but fails the ≥1 floor.
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

    /// A negative number does not parse as a `u64`.
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

    /// A float — even one with an integral value like `5.0` — does not
    /// parse as a `u64`.
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

    /// A string does not parse as a `u64`.
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
