//! Streaming turns, summaries, and partial-response projection.

use super::ProviderError;
use super::request::{Tuning, build_cached_request, complete_options, tool_defs};
use super::retry::{Attempt, idle_timeout, retry_with_backoff, wait_for_cancel};
use super::transport::{Engine, Transport};
use super::usage::{Usage, usage_from};
use crate::agent::cancel;
use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatStreamEvent, StopReason, StreamEnd, ToolCall};

/// One streamed assistant response.
pub struct StepOut {
    pub assistant_message: ChatMessage,
    pub tool_calls: Vec<ToolCall>,
    /// Reasoning captured for display and transcript round-tripping.
    pub reasoning: Option<String>,
    pub usage: Usage,
    /// The provider-normalised stop reason, if one arrived.
    pub stop_reason: Option<StopReason>,
    /// Why a partial assistant turn was committed.
    pub cut_short: Option<CutShort>,
}

/// Why an assistant turn ended before the model chose to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutShort {
    /// The output-token cap truncated the turn.
    OutputCap,
    /// The stream broke after visible text or reasoning had arrived.
    Stalled(String),
}

/// One compaction summary response.
pub struct SummaryOut {
    pub summary: String,
    pub usage: Usage,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn complete<F: FnMut(&str), G: FnMut(&str)>(
        &self,
        transport: &Transport,
        model: &str,
        max_tokens_override: Option<u32>,
        tuning: &Tuning,
        route: Option<&str>,
        system: &str,
        messages: Vec<ChatMessage>,
        tool_enabled: bool,
        on_text: &mut F,
        on_think: &mut G,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        self.refresh_if_stale(transport);
        let adapter = transport.adapter();
        let request_template = build_cached_request(adapter, system, messages);
        let tools = tool_defs(tool_enabled);
        let options = complete_options(self.cache_key(), max_tokens_override, tuning, route);

        self.block_on(retry_with_backoff(
            "before request",
            cancel,
            async |attempt| {
                let mut request = request_template.clone();
                request.tools = (!tools.is_empty()).then(|| tools.clone());
                let mut seen_streamed_content = false;
                let mut streamed = String::new();
                let mut streamed_reasoning = String::new();
                let attempt_result: Result<StreamEnd, ProviderError> = async {
                    // `idle_timeout` bounds only opening the stream (time to
                    // first response); once `response.stream` exists below, a
                    // connection gone silent is instead caught by the
                    // transport's own per-read timeout
                    // (`tls::STREAM_IDLE_TIMEOUT`), surfacing as an `Err`
                    // from `stream.next()`.
                    let mut response = tokio::select! {
                        biased;
                        () = wait_for_cancel(cancel) => {
                            return Err(ProviderError::Cancelled("before request"));
                        }
                        () = tokio::time::sleep(idle_timeout(attempt)) => {
                            return Err(ProviderError::Transient {
                                cause: "stream idle: no response within timeout".into(),
                                attempts: 1,
                                body: None,
                            });
                        }
                        result = transport.client().exec_chat_stream(
                            model,
                            request,
                            Some(&options),
                        ) => result.map_err(|error| ProviderError::from_genai(&error, model))?,
                    };
                    loop {
                        let event = tokio::select! {
                            biased;
                            () = wait_for_cancel(cancel) => {
                                return Err(ProviderError::Cancelled("mid-stream"));
                            }
                            event = response.stream.next() => match event {
                                Some(Ok(event)) => event,
                                Some(Err(error)) => {
                                    return Err(ProviderError::from_genai(&error, model));
                                }
                                None => break,
                            }
                        };
                        match event {
                            ChatStreamEvent::Chunk(chunk) => {
                                seen_streamed_content |= !chunk.content.is_empty();
                                streamed.push_str(&chunk.content);
                                on_text(&chunk.content);
                            }
                            ChatStreamEvent::End(end) => return Ok(end),
                            ChatStreamEvent::ReasoningChunk(chunk) => {
                                seen_streamed_content |= !chunk.content.is_empty();
                                streamed_reasoning.push_str(&chunk.content);
                                on_think(&chunk.content);
                            }
                            // Signatures and tool calls are captured whole in
                            // `End`. Keep this exhaustive so a new event cannot
                            // disappear silently.
                            ChatStreamEvent::Start
                            | ChatStreamEvent::ThoughtSignatureChunk(_)
                            | ChatStreamEvent::ToolCallChunk(_) => {}
                        }
                    }
                    Err(ProviderError::Transient {
                        cause: "stream ended without End event".into(),
                        attempts: 1,
                        body: None,
                    })
                }
                .await;

                // Once content has reached `on_text`/`on_think` there is no
                // clean retry: re-issuing would resend the whole prompt and
                // duplicate output already delivered live. A failure past
                // that point is committed here as a `Stalled` step
                // (`Attempt::Done`, ending the retry loop) instead of being
                // retried — except a cancellation, which always propagates
                // as failure regardless of streamed content: the caller
                // discards the whole `StepOut` on `Cancelled` rather than
                // committing a partial turn.
                match attempt_result {
                    Ok(end) => {
                        Attempt::Done(step_out_from_end(model, end, transport.metered(), adapter))
                    }
                    Err(error @ ProviderError::Cancelled(_)) => Attempt::Failed(error),
                    Err(error) if seen_streamed_content => Attempt::Done(stalled_step_out(
                        model,
                        &streamed,
                        &streamed_reasoning,
                        &error,
                        transport.metered(),
                        adapter,
                    )),
                    Err(error) => Attempt::Failed(error),
                }
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn summarize(
        &self,
        transport: &Transport,
        model: &str,
        system: &str,
        mut messages: Vec<ChatMessage>,
        max_tokens: u32,
        cancel: &cancel::Token,
    ) -> Result<SummaryOut, ProviderError> {
        self.refresh_if_stale(transport);
        let adapter = transport.adapter();
        messages.push(ChatMessage::user(
            "Summarise the conversation so far concisely: \
            the user's task, what has been tried, what worked, what state the \
            shell is in (cwd, env, defined names), and any open subtasks. \
            A prior summary may appear first — fold it in. \
            Return only the summary, no preamble.",
        ));
        let request_template = build_cached_request(adapter, system, messages);
        let options = ChatOptions::default()
            .with_max_tokens(max_tokens)
            .with_prompt_cache_key(self.cache_key());

        let response = self.block_on(retry_with_backoff(
            "during summary",
            cancel,
            async |attempt| {
                let request = request_template.clone();
                // Unlike `complete`'s streaming path, `exec_chat` only
                // returns once the whole reply is in, so this timeout bounds
                // the entire call rather than just its opening — there is no
                // later per-byte read to fall back on for a stalled
                // compaction request.
                let result = tokio::select! {
                    biased;
                    () = wait_for_cancel(cancel) => {
                        return Attempt::Failed(ProviderError::Cancelled("during summary"));
                    }
                    () = tokio::time::sleep(idle_timeout(attempt)) => {
                        return Attempt::Failed(ProviderError::Transient {
                            cause: "summary request: no response within timeout".into(),
                            attempts: 1,
                            body: None,
                        });
                    }
                    result = transport.client().exec_chat(model, request, Some(&options)) => result,
                };
                match result {
                    Ok(response) => Attempt::Done(response),
                    Err(error) => Attempt::Failed(ProviderError::from_genai(&error, model)),
                }
            },
        ))?;

        if matches!(response.stop_reason, Some(StopReason::MaxTokens(_))) {
            return Err(ProviderError::Truncated {
                reason: "summary exceeded the compaction token budget".into(),
            });
        }
        Ok(SummaryOut {
            summary: response.first_text().unwrap_or("").to_string(),
            usage: usage_from(model, &response.usage, transport.metered(), adapter),
        })
    }
}

fn step_out_from_end(model: &str, end: StreamEnd, metered: bool, adapter: AdapterKind) -> StepOut {
    let raw_usage = end.captured_usage.clone().unwrap_or_default();
    let stop_reason = end.captured_stop_reason.clone();
    let reasoning = end.captured_reasoning_content.clone();
    let content = end.captured_content.clone().unwrap_or_default();
    let tool_calls = end.captured_into_tool_calls().unwrap_or_default();
    let cut_short = stop_reason
        .as_ref()
        .filter(|reason| matches!(reason, StopReason::MaxTokens(_)))
        .map(|_| CutShort::OutputCap);
    StepOut {
        assistant_message: ChatMessage::assistant(content)
            .with_reasoning_content(reasoning.clone()),
        tool_calls,
        reasoning,
        usage: usage_from(model, &raw_usage, metered, adapter),
        stop_reason,
        cut_short,
    }
}

fn stalled_step_out(
    model: &str,
    streamed: &str,
    streamed_reasoning: &str,
    cause: &ProviderError,
    metered: bool,
    adapter: AdapterKind,
) -> StepOut {
    let reasoning = (!streamed_reasoning.is_empty()).then(|| streamed_reasoning.to_string());
    StepOut {
        assistant_message: ChatMessage::assistant(streamed.to_string())
            .with_reasoning_content(reasoning.clone()),
        tool_calls: Vec::new(),
        reasoning,
        usage: usage_from(model, &genai::chat::Usage::default(), metered, adapter),
        stop_reason: None,
        cut_short: Some(CutShort::Stalled(cause.brief())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::ModelIden;
    use genai::chat::{ContentPart, MessageContent};
    use reqwest::StatusCode;

    fn web_stream_http(status: StatusCode) -> genai::Error {
        genai::Error::WebStream {
            model_iden: ModelIden::new(AdapterKind::Anthropic, "m"),
            cause: "stream open failed".into(),
            error: Box::new(genai::Error::HttpError {
                status,
                canonical_reason: status.canonical_reason().unwrap_or("").into(),
                body: String::new(),
            }),
        }
    }

    #[test]
    fn step_out_preserves_reasoning_content() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("answer")),
            captured_reasoning_content: Some("let me think".into()),
            ..Default::default()
        };
        let out = step_out_from_end("deepseek-v4-pro", end, true, AdapterKind::DeepSeek);
        assert!(out.assistant_message.content.iter().any(|part| matches!(
            part,
            ContentPart::ReasoningContent(reasoning) if reasoning == "let me think"
        )));
    }

    #[test]
    fn step_out_without_reasoning_attaches_nothing() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("answer")),
            ..Default::default()
        };
        let out = step_out_from_end("deepseek-v4-pro", end, true, AdapterKind::DeepSeek);
        assert!(
            !out.assistant_message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ReasoningContent(_)))
        );
    }

    #[test]
    fn stalled_step_out_keeps_streamed_prefix() {
        let cause = ProviderError::from_genai(
            &web_stream_http(StatusCode::SERVICE_UNAVAILABLE),
            "test-model",
        );
        let brief = cause.brief();
        let step = stalled_step_out(
            "test-model",
            "partial answer so far",
            "",
            &cause,
            false,
            AdapterKind::OpenAI,
        );
        assert_eq!(step.cut_short, Some(CutShort::Stalled(brief)));
        assert!(step.tool_calls.is_empty());
        assert!(step.stop_reason.is_none());
        assert_eq!(
            step.assistant_message.content.first_text(),
            Some("partial answer so far")
        );
        assert!(step.reasoning.is_none());
    }

    #[test]
    fn stalled_step_out_keeps_streamed_reasoning_even_without_text() {
        let cause = ProviderError::from_genai(
            &web_stream_http(StatusCode::SERVICE_UNAVAILABLE),
            "test-model",
        );
        let step = stalled_step_out(
            "test-model",
            "",
            "let me work through the cases",
            &cause,
            false,
            AdapterKind::OpenAI,
        );
        assert_eq!(
            step.reasoning.as_deref(),
            Some("let me work through the cases")
        );
        assert!(step.assistant_message.content.iter().any(|part| matches!(
            part,
            ContentPart::ReasoningContent(reasoning)
                if reasoning == "let me work through the cases"
        )));
    }
}
