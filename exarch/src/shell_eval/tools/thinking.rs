//! `thinking` — a relay: the model's thought, recorded on the deliberation
//! lane and answered with nothing, so it may narrate between calls without
//! ending its turn.

use super::{input_error, required_str};
use crate::agent::Avatar;
use crate::agent::event::ToolResult;
use crate::bus::Emitter;
use crate::record::Display;
use serde_json::{Value, json};

pub(crate) const NAME: &str = "thinking";

pub(crate) const DESC: &str = "Use this to record your thinking.";

const ACK: &str = "relayed";

pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "thought": {
                "type": "string",
                "description": "Thinking.",
            },
        },
        "required": ["thought"],
    })
}

fn thought(input: &Value) -> Result<&str, String> {
    let text = required_str(input, "thought")?;
    if text.trim().is_empty() {
        return Err("`thought` must be non-empty".to_string());
    }
    Ok(text)
}

/// Recorded on the deliberation lane, so a thought wears the `∴` rail and the
/// drained ink the provider's own reasoning does.  Newline-terminated: the
/// fold grows one thinking block by every thinking record that follows, so the
/// next stretch would otherwise join mid-line.
pub(crate) fn dispatch(
    id: String,
    input: &Value,
    session: &mut Avatar,
    _emit: &Emitter,
) -> ToolResult {
    let content = match thought(input) {
        Ok(text) => {
            let recorder = session.recorder();
            let text = format!("{}\n", text.trim_end());
            if let Err(error) = recorder.emit(Display::Thinking { text }) {
                recorder.report_fault(&error);
            }
            ACK.to_string()
        }
        Err(reason) => {
            let msg = input_error(&reason);
            session.note_error(msg.clone());
            msg
        }
    };
    ToolResult { id, content }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_thought() {
        assert_eq!(
            thought(&json!({ "thought": "one step back" })).unwrap(),
            "one step back"
        );
    }

    #[test]
    fn rejects_a_missing_thought() {
        let e = thought(&json!({ "text": "wrong key" })).unwrap_err();
        assert!(e.contains("`thought`"));
    }

    #[test]
    fn rejects_a_blank_thought() {
        let e = thought(&json!({ "thought": "   \n" })).unwrap_err();
        assert!(e.contains("`thought`"));
    }
}
