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
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
    pub cut_short: Option<CutShort>,
}

/// One streamed piece of an assistant turn.
///
/// Prose and reasoning arrive interleaved on a single stream and their order
/// carries meaning — a reasoning run ends exactly where the prose after it
/// begins — so they reach the caller through one callback, which cannot lose
/// that order, rather than through two independent ones, which can.
#[derive(Clone, Copy)]
pub enum Delta<'a> {
    Say(&'a str),
    Think(&'a str),
}

/// Why an assistant turn ended before the model chose to stop.
#[derive(Debug, Clone)]
pub enum CutShort {
    OutputCap,
    /// The stream broke after text or reasoning had already reached the caller.
    /// The failure rides along whole — a stall is a provider error that arrived
    /// too late to retry, and the user is owed the same detail either way.
    Stalled(ProviderError),
}

/// One compaction summary response.
pub struct SummaryOut {
    pub summary: String,
    pub usage: Usage,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        transport: &Transport,
        model: &str,
        max_tokens_override: Option<u32>,
        tuning: &Tuning,
        route: Option<&str>,
        system: &str,
        messages: Vec<ChatMessage>,
        tool_enabled: bool,
        search: bool,
        on_delta: &mut F,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        self.refresh_if_stale(transport);
        let adapter = transport.adapter();
        let request_template = build_cached_request(adapter, system, messages);
        let tools = tool_defs(adapter, tool_enabled, search);
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
                        // The opening's bound, re-armed by every event: a
                        // `Heartbeat` through a long think counts as life, so only
                        // true silence ends the stream.
                        let event = tokio::select! {
                            biased;
                            () = wait_for_cancel(cancel) => {
                                return Err(ProviderError::Cancelled("mid-stream"));
                            }
                            () = tokio::time::sleep(idle_timeout(attempt)) => {
                                return Err(ProviderError::Transient {
                                    cause: "stream idle: no event within timeout".into(),
                                    attempts: 1,
                                    body: None,
                                });
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
                                on_delta(Delta::Say(&chunk.content));
                            }
                            ChatStreamEvent::End(end) => return Ok(end),
                            ChatStreamEvent::ReasoningChunk(chunk) => {
                                seen_streamed_content |= !chunk.content.is_empty();
                                streamed_reasoning.push_str(&chunk.content);
                                on_delta(Delta::Think(&chunk.content));
                            }
                            // Nothing to add to the turn: signatures and tool calls
                            // arrive whole in `End`, and a heartbeat's only use is
                            // the re-arming above. Named rather than wildcarded so
                            // a new event cannot slip past.
                            ChatStreamEvent::Start
                            | ChatStreamEvent::Heartbeat
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

                // Text already streamed live cannot be unsent, so a failure past
                // the first chunk commits as `Stalled` instead of retrying.
                // Cancellation is exempt: `deliberate` discards the turn whole.
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
                // `exec_chat` returns only when the whole reply is in, so this
                // bounds the whole call; no per-read timeout backs it up.
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
        assistant_message: ChatMessage::assistant(content).with_reasoning_content(reasoning),
        tool_calls,
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
            .with_reasoning_content(reasoning),
        tool_calls: Vec::new(),
        usage: usage_from(model, &genai::chat::Usage::default(), metered, adapter),
        stop_reason: None,
        cut_short: Some(CutShort::Stalled(cause.clone())),
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
    fn output_capped_end_is_cut_short_and_keeps_its_stop_reason() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("as far as I got")),
            captured_stop_reason: Some(StopReason::MaxTokens("max_tokens".into())),
            ..Default::default()
        };
        let out = step_out_from_end("m", end, true, AdapterKind::Anthropic);
        assert!(
            matches!(out.cut_short, Some(CutShort::OutputCap)),
            "an output cap must read as one, got {:?}",
            out.cut_short
        );
        assert!(matches!(out.stop_reason, Some(StopReason::MaxTokens(_))));
    }

    #[test]
    fn completed_end_is_not_cut_short() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("all done")),
            captured_stop_reason: Some(StopReason::Completed("end_turn".into())),
            ..Default::default()
        };
        let out = step_out_from_end("m", end, true, AdapterKind::Anthropic);
        assert!(out.cut_short.is_none());
    }

    /// The case `scripted::Reply::truncated_with_tool_calls` imitates: the cap
    /// lands with tool calls already captured, and both must reach `deliberate`.
    #[test]
    fn output_capped_end_keeps_its_tool_calls() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from(vec![ToolCall {
                call_id: "call-1".into(),
                fn_name: "ral".into(),
                fn_arguments: serde_json::json!({ "cmd": "echo hi" }),
                thought_signatures: None,
            }])),
            captured_stop_reason: Some(StopReason::MaxTokens("length".into())),
            ..Default::default()
        };
        let out = step_out_from_end("m", end, true, AdapterKind::Anthropic);
        assert!(matches!(out.cut_short, Some(CutShort::OutputCap)));
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].fn_name, "ral");
    }

    #[test]
    fn stalled_step_out_keeps_streamed_prefix() {
        let cause = ProviderError::from_genai(
            &web_stream_http(StatusCode::SERVICE_UNAVAILABLE),
            "test-model",
        );
        let step = stalled_step_out(
            "test-model",
            "partial answer so far",
            "",
            &cause,
            false,
            AdapterKind::OpenAI,
        );
        assert!(
            matches!(
                step.cut_short,
                Some(CutShort::Stalled(ProviderError::Transient { .. }))
            ),
            "the classified cause must ride the stall whole, got {:?}",
            step.cut_short
        );
        assert!(step.tool_calls.is_empty());
        assert!(step.stop_reason.is_none());
        assert_eq!(
            step.assistant_message.content.first_text(),
            Some("partial answer so far")
        );
        assert!(
            !step
                .assistant_message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ReasoningContent(_))),
            "a stall with no reasoning attaches none"
        );
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
        assert!(step.assistant_message.content.iter().any(|part| matches!(
            part,
            ContentPart::ReasoningContent(reasoning)
                if reasoning == "let me work through the cases"
        )));
    }
}
