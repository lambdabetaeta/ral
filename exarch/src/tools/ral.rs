//! `ral` tool — evaluate ral source against the session's live shell.
//!
//! Runs synchronously on the dispatching thread.  Owns its own JSON
//! parsing: a missing `cmd` or a non-object input each becomes an
//! inline error block on the rail with no detour through the session.

use super::{INVALID_INPUT, Tool, invalid_input};
use crate::agent::Agent;
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

pub(super) struct RalTool;

/// Parsed ral-tool input.  Public to the tool so the parser stays
/// trivially testable without going through `dispatch`.
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

/// Hard cap on `description` length.  The field is a one-line label
/// for the user-facing rail, not a paragraph; oversize inputs are
/// rejected at parse time so the model fixes the call rather than
/// silently flooding the transcript.
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
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(format!(
            "`description` must be ≤{DESCRIPTION_MAX} characters (got {})",
            description.chars().count()
        ));
    }
    if description.contains('\n') {
        return Err("`description` must be a single line (no newlines)".to_string());
    }
    // `timeout_secs` is optional. Inspect the raw value rather than using
    // `u64_field`: `as_u64` alone cannot tell absent (→ default) from a
    // present-but-invalid value (→ reject). Absent or `null` takes the
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

impl Tool for RalTool {
    fn name(&self) -> &'static str {
        "ral"
    }

    fn desc(&self) -> &'static str {
        "Run a ral shell command in the sandboxed working directory."
    }

    fn schema(&self) -> &'static Value {
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

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let args = match parse_args(&input) {
            Ok(a) => a,
            Err(reason) => return invalid_input(id, "ral", INVALID_INPUT, &reason, emit),
        };
        emit.emit(Kind::ToolCall {
            tool: "ral",
            cmd: args.cmd.clone(),
            summary: Some(args.description.clone()),
        });
        session.run_shell(id, &args.cmd, args.timeout_secs, emit)
    }
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
    fn parse_rejects_oversize_description() {
        let big = "x".repeat(DESCRIPTION_MAX + 1);
        let e = parse_args(&json!({
            "cmd": "pwd",
            "description": big,
        }))
        .unwrap_err();
        assert!(e.contains("`description`"));
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
