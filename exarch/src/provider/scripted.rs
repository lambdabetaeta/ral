//! Canned replies for the `Backend::Scripted` arm, so tests never dial out.

use super::{CutShort, Delta, ProviderError, StepOut, SummaryOut, Usage};
use genai::chat::{ChatMessage, StopReason, ToolCall};
use std::collections::VecDeque;
use std::sync::Mutex;

/// One scripted completion reply.
pub struct Reply {
    text: String,
    outcome: Result<StepOut, ProviderError>,
    /// Set by [`Self::panicking`]: the turn unwinds as it is consumed, rather
    /// than answering.
    panics: bool,
}

impl Reply {
    pub fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            panics: false,
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(text.to_string()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: Some(StopReason::Completed("end_turn".into())),
                cut_short: None,
            }),
        }
    }

    /// A turn whose stream broke after `text` reached the caller.  The cause is
    /// terminal — genai's shape for a frame it could not parse, which a retry
    /// only re-loses — so the driver must report it rather than resend.
    pub fn stalled(text: &str) -> Self {
        Self {
            text: text.to_string(),
            panics: false,
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(text.to_string()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: None,
                cut_short: Some(CutShort::Stalled(ProviderError::Other(
                    "Failed to parse stream data for model 'test-model'".into(),
                ))),
            }),
        }
    }

    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        let parts = calls
            .iter()
            .cloned()
            .map(genai::chat::ContentPart::ToolCall)
            .collect::<Vec<_>>();
        Self {
            text: String::new(),
            panics: false,
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(parts),
                tool_calls: calls,
                usage: Usage::default(),
                stop_reason: Some(StopReason::ToolCall("tool_use".into())),
                cut_short: None,
            }),
        }
    }

    /// An output-capped turn carrying tool calls, which the driver still runs.
    pub fn truncated_with_tool_calls(calls: Vec<ToolCall>) -> Self {
        let parts = calls
            .iter()
            .cloned()
            .map(genai::chat::ContentPart::ToolCall)
            .collect::<Vec<_>>();
        Self {
            text: String::new(),
            panics: false,
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(parts),
                tool_calls: calls,
                usage: Usage::default(),
                stop_reason: Some(StopReason::MaxTokens("max_tokens".into())),
                cut_short: Some(CutShort::OutputCap),
            }),
        }
    }

    /// A reply with no content parts, unlike `text("")` and its one empty part:
    /// the turn `admit_assistant` in `agent/deliberate.rs` must fill with a stub.
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            panics: false,
            outcome: Ok(StepOut {
                assistant_message: ChatMessage::assistant(genai::chat::MessageContent::default()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: Some(StopReason::Completed("end_turn".into())),
                cut_short: None,
            }),
        }
    }

    /// A turn that unwinds host-side as it is consumed, standing in for a
    /// transport or decode crash — the arm `Agent::attend`'s `catch_unwind`
    /// exists for, which no eval-side panic can reach.
    pub fn panicking() -> Self {
        Self {
            panics: true,
            ..Self::empty()
        }
    }

    pub fn error(error: ProviderError) -> Self {
        Self {
            text: String::new(),
            panics: false,
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

    /// The scripted counterpart to `Engine::complete`: no stream, so the whole
    /// reply fires as one [`Delta::Say`] and no reasoning ever arrives. Panics
    /// once the queue runs dry — a test drove more turns than it scripted —
    /// and on a [`Reply::panicking`] turn, by construction.
    pub(super) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        _model: &str,
        on_delta: &mut F,
    ) -> Result<StepOut, ProviderError> {
        let reply = self
            .completes
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted provider ran out of `complete` replies");
        assert!(!reply.panics, "scripted host panic");
        if !reply.text.is_empty() {
            on_delta(Delta::Say(&reply.text));
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
