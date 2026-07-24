//! Scripted provider backend for deterministic agent tests.

use super::{CutShort, ProviderError, StepOut, SummaryOut, Usage};
use genai::chat::{ChatMessage, StopReason, ToolCall};
use std::collections::VecDeque;
use std::sync::Mutex;

/// One scripted completion reply.
pub struct Reply {
    text: String,
    outcome: Result<StepOut, ProviderError>,
}

impl Reply {
    /// A plain assistant turn with no tool calls.
    pub fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(text.to_string()),
                tool_calls: Vec::new(),
                reasoning: None,
                usage: Usage::default(),
                stop_reason: Some(StopReason::Completed("end_turn".into())),
                cut_short: None,
            }),
        }
    }

    /// A turn whose stream stalled after `text` rendered, with a cause
    /// string shaped like a real genai stream-timeout error.
    pub fn stalled(text: &str) -> Self {
        Self {
            text: text.to_string(),
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(text.to_string()),
                tool_calls: Vec::new(),
                reasoning: None,
                usage: Usage::default(),
                stop_reason: None,
                cut_short: Some(CutShort::Stalled(
                    "Web stream error: operation timed out".into(),
                )),
            }),
        }
    }

    /// An assistant turn that requests tool calls.
    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        let parts = calls
            .iter()
            .cloned()
            .map(genai::chat::ContentPart::ToolCall)
            .collect::<Vec<_>>();
        Self {
            text: String::new(),
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(parts),
                tool_calls: calls,
                reasoning: None,
                usage: Usage::default(),
                stop_reason: Some(StopReason::ToolCall("tool_use".into())),
                cut_short: None,
            }),
        }
    }

    /// A token-capped turn carrying tool calls the model was mid-emitting.
    pub fn truncated_with_tool_calls(calls: Vec<ToolCall>) -> Self {
        let parts = calls
            .iter()
            .cloned()
            .map(genai::chat::ContentPart::ToolCall)
            .collect::<Vec<_>>();
        Self {
            text: String::new(),
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(parts),
                tool_calls: calls,
                reasoning: None,
                usage: Usage::default(),
                stop_reason: Some(StopReason::MaxTokens("max_tokens".into())),
                cut_short: Some(CutShort::OutputCap),
            }),
        }
    }

    /// A reply with zero content parts — distinct from `text("")`, whose
    /// `ChatMessage` still carries one empty text part.  For exercising the
    /// driver's stub-substitution path on a truly empty assistant turn.
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(genai::chat::MessageContent::default()),
                tool_calls: Vec::new(),
                reasoning: None,
                usage: Usage::default(),
                stop_reason: Some(StopReason::Completed("end_turn".into())),
                cut_short: None,
            }),
        }
    }

    /// A reply that surfaces `error`.
    pub fn error(error: ProviderError) -> Self {
        Self {
            text: String::new(),
            outcome: Err(error),
        }
    }
}

/// A queue of deterministic completion replies.
pub struct Script {
    completes: Mutex<VecDeque<Reply>>,
}

impl Script {
    pub fn new() -> Self {
        Self {
            completes: Mutex::new(VecDeque::new()),
        }
    }

    /// Append one completion reply.
    ///
    /// # Panics
    /// Panics if the script mutex is poisoned.
    pub fn then(self, reply: Reply) -> Self {
        self.completes.lock().unwrap().push_back(reply);
        self
    }

    /// The `Backend::Scripted` counterpart to `Engine::complete`: there is
    /// no real stream, so the whole reply fires through `on_text` once and
    /// the queued outcome returns synchronously.  `on_think` is never
    /// driven — a scripted `reasoning` rides directly on the outcome
    /// instead of streaming incrementally.
    ///
    /// # Panics
    /// Panics if the script is empty (a test drove more turns than it
    /// scripted) or the mutex is poisoned.
    pub(super) fn complete<F: FnMut(&str), G: FnMut(&str)>(
        &self,
        _model: &str,
        on_text: &mut F,
        _on_think: &mut G,
    ) -> Result<StepOut, ProviderError> {
        let reply = self
            .completes
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted provider ran out of `complete` replies");
        if !reply.text.is_empty() {
            on_text(&reply.text);
        }
        reply.outcome
    }

    #[allow(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "the scripted and live backend arms deliberately share one shape"
    )]
    pub(super) fn summarize(&self, _model: &str) -> Result<SummaryOut, ProviderError> {
        Ok(SummaryOut {
            summary: "scripted summary".to_string(),
            usage: Usage::default(),
        })
    }
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}
