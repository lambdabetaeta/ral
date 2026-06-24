//! `ral` tool — evaluate ral source against the session's live shell.
//!
//! Runs synchronously on the dispatching thread.  Owns its own JSON
//! parsing: a missing `cmd` or a non-object input each becomes an
//! inline error block on the rail with no detour through the session.

use super::{Tool, invalid_input};
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
}

/// Hard wall-clock bound on a single inline `ral` call. There is no
/// per-call knob: work that outruns this belongs in a `spawn`ed block
/// whose handle is polled and awaited across turns.
const CALL_TIMEOUT_SECS: u64 = 30;

/// Hard cap on `description` length.  The field is a one-line label
/// for the user-facing rail, not a paragraph; oversize inputs are
/// rejected at parse time so the model fixes the call rather than
/// silently flooding the transcript.
const DESCRIPTION_MAX: usize = 240;

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
    Ok(RalArgs { cmd, description })
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
                        "description": "One-line summary (≤240 chars) of what this call \
            does, written in the present continuous, e.g. \"Counting TODOs in src/ and \
            sampling the first five hits.\" or \"Appending the next section to \
            docs/SPEC.md.\". This is what the user sees on the rail (the full ral source \
            is one click away), so it must stand on its own — name the files touched, \
            the action taken, the metric reported. No newlines. Do not echo the ral \
            source; paraphrase the intent.",
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
            Err(reason) => return invalid_input(id, "ral", "<invalid input>", &reason, emit),
        };
        emit.emit(Kind::ToolCall {
            tool: "ral",
            cmd: args.cmd.clone(),
            summary: Some(args.description.clone()),
        });
        session.run_shell(id, &args.cmd, CALL_TIMEOUT_SECS, emit)
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
}
