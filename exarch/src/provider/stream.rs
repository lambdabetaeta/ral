//! Streaming turns and partial-response projection.

use super::error::{CancelSite, CutShort};
use super::request::{Request, complete_options};
use super::retry::{Attempt, retry_with_backoff, wait_for_cancel};
use super::transport::{Engine, Transport};
use super::usage::{Usage, usage_from};
use super::wire::{manufacture, tool_defs};
use super::{ProviderError, Selection};
use crate::cancel;
use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatFrameSink, ChatMessage, ChatStreamEvent, FrameCtx, RawFrameRef, StopReason, StreamEnd,
    ToolCall,
};
use ral_core::sync::LockExt;
use serde::Deserialize;
use std::sync::{Arc, Mutex};

/// One streamed assistant response.
pub struct StepOut {
    pub assistant_message: ChatMessage,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
    pub cut_short: Option<CutShort>,
    pub served: Served,
}

/// Who answered a call, as far as the backend says: the response's generation
/// id and, on a router such as `OpenRouter`, the upstream provider it served.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Served {
    #[serde(rename = "id")]
    pub generation: Option<String>,
    pub provider: Option<String>,
}

/// Reads [`Served`] off the wire frames, which genai's normalised events drop.
#[derive(Default)]
struct ServedSink(Mutex<Served>);

impl ChatFrameSink for ServedSink {
    fn on_frame(&self, _: &FrameCtx, frame: RawFrameRef<'_>) {
        let mut served = self.0.lock_ignore_poison();
        if served.generation.is_some() && served.provider.is_some() {
            return;
        }
        if let Ok(head) = serde_json::from_str::<Served>(frame.data) {
            served.generation = served.generation.take().or(head.generation);
            served.provider = served.provider.take().or(head.provider);
        }
    }
}

impl ServedSink {
    fn served(&self) -> Served {
        self.0.lock_ignore_poison().clone()
    }
}

/// One streamed event of an assistant turn.
///
/// Prose and reasoning arrive interleaved on a single stream and their order
/// carries meaning — a reasoning run ends exactly where the prose after it
/// begins — so they reach the caller through one callback, which cannot lose
/// that order, rather than through two independent ones, which can.
#[derive(Clone, Copy)]
pub enum Delta<'a> {
    Say(&'a str),
    Think(&'a str),
    /// The attempt whose deltas came before failed and is being re-issued;
    /// deltas after it belong to the new attempt.
    Retry {
        /// The failure being retried.
        cause: &'a ProviderError,
        /// The failed attempt's usage when it streamed output the provider
        /// never reported; `None` when it failed before any.
        lost: Option<Usage>,
    },
}

impl Engine {
    pub(super) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        transport: &Transport,
        selection: &Selection,
        request: Request<'_>,
        on_delta: &mut F,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        self.refresh_if_stale(transport);
        let model = selection.model.as_str();
        let adapter = transport.adapter();
        let tools = tool_defs(adapter, transport.live_search(), request);
        let options = complete_options(
            self.cache_key(),
            selection.max_tokens_override,
            &selection.tuning,
            selection.via.as_ref(),
        );

        let mut heard = false;
        self.block_on(retry_with_backoff(
            CancelSite::BeforeRequest,
            cancel,
            async |retrying| {
                if let Some(cause) = retrying {
                    on_delta(Delta::Retry {
                        cause,
                        lost: heard.then(|| Usage::unreported(transport.metered())),
                    });
                }
                heard = false;
                let wire =
                    manufacture(adapter, request.system, request.transcript, &tools).into_request();
                let sink = Arc::new(ServedSink::default());
                let options = options.clone().with_raw_frame_sink_arc(sink.clone());
                let mut spoke = false;
                let mut streamed = String::new();
                let mut streamed_reasoning = String::new();
                let attempt_result: Result<StreamEnd, ProviderError> = async {
                    let mut response = tokio::select! {
                        biased;
                        () = wait_for_cancel(cancel) => {
                            return Err(ProviderError::Cancelled(CancelSite::BeforeRequest));
                        }
                        result = transport.client().exec_chat_stream(
                            model,
                            wire,
                            Some(&options),
                        ) => result.map_err(|error| ProviderError::from_genai(&error, model))?,
                    };
                    loop {
                        let event = tokio::select! {
                            biased;
                            () = wait_for_cancel(cancel) => {
                                return Err(ProviderError::Cancelled(CancelSite::MidStream));
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
                                spoke |= !chunk.content.trim().is_empty();
                                heard |= !chunk.content.is_empty();
                                streamed.push_str(&chunk.content);
                                on_delta(Delta::Say(&chunk.content));
                            }
                            ChatStreamEvent::End(end) => return Ok(end),
                            ChatStreamEvent::ReasoningChunk(chunk) => {
                                heard |= !chunk.content.is_empty();
                                streamed_reasoning.push_str(&chunk.content);
                                on_delta(Delta::Think(&chunk.content));
                            }
                            // Nothing to add to the turn: signatures and tool calls
                            // arrive whole in `End`, and liveness is genai's
                            // byte-level bound, so a heartbeat adds nothing here.
                            // Named rather than wildcarded so a new event cannot
                            // slip past.
                            ChatStreamEvent::Start
                            | ChatStreamEvent::Heartbeat
                            | ChatStreamEvent::ThoughtSignatureChunk(_)
                            | ChatStreamEvent::ToolCallChunk(_) => {}
                        }
                    }
                    Err(ProviderError::local_transient(
                        "stream ended without End event",
                    ))
                }
                .await;

                // Prose already streamed live cannot be unsent, so a failure the
                // driver would retry commits as `Stalled` once it has spoken;
                // any other failure, or one before prose, goes to the driver.
                let served = sink.served();
                match attempt_result {
                    Ok(end) => Attempt::Done(step_out_from_end(
                        model,
                        end,
                        served,
                        transport.metered(),
                        adapter,
                    )),
                    Err(error) if spoke && error.retryable() => Attempt::Done(stalled_step_out(
                        &streamed,
                        &streamed_reasoning,
                        &error,
                        served,
                        transport.metered(),
                    )),
                    Err(error) => Attempt::Failed(error),
                }
            },
        ))
    }
}

fn step_out_from_end(
    model: &str,
    end: StreamEnd,
    served: Served,
    metered: bool,
    adapter: AdapterKind,
) -> StepOut {
    let usage = end.captured_usage.as_ref().map_or_else(
        || Usage::unreported(metered),
        |raw| usage_from(model, raw, metered, adapter),
    );
    let stop_reason = end.captured_stop_reason.clone();
    let reasoning = end.captured_reasoning_content.clone();
    let content = end.captured_content.clone().unwrap_or_default();
    let response_id = end.captured_response_id.clone();
    let tool_calls = end.captured_into_tool_calls().unwrap_or_default();
    let cut_short = match &stop_reason {
        Some(StopReason::MaxTokens(raw)) => Some(CutShort::OutputCap {
            stop_reason: raw.clone(),
        }),
        _ => None,
    };
    StepOut {
        assistant_message: ChatMessage::assistant(content).with_reasoning_content(reasoning),
        tool_calls,
        usage,
        stop_reason,
        cut_short,
        served: Served {
            generation: response_id.or(served.generation),
            ..served
        },
    }
}

fn stalled_step_out(
    streamed: &str,
    streamed_reasoning: &str,
    cause: &ProviderError,
    served: Served,
    metered: bool,
) -> StepOut {
    let reasoning = (!streamed_reasoning.is_empty()).then(|| streamed_reasoning.to_string());
    StepOut {
        assistant_message: ChatMessage::assistant(streamed.to_string())
            .with_reasoning_content(reasoning),
        tool_calls: Vec::new(),
        usage: Usage::unreported(metered),
        stop_reason: None,
        cut_short: Some(CutShort::Stalled(cause.clone())),
        served,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::ModelIden;
    use genai::chat::{ContentPart, MessageContent};
    use reqwest::StatusCode;
    use reqwest::header::HeaderMap;

    fn web_stream_http(status: StatusCode) -> genai::Error {
        genai::Error::WebStream {
            model_iden: ModelIden::new(AdapterKind::Anthropic, "m"),
            cause: "stream open failed".into(),
            error: Box::new(genai::Error::HttpError {
                status,
                canonical_reason: status.canonical_reason().unwrap_or("").into(),
                body: String::new(),
                headers: Box::new(HeaderMap::new()),
            }),
        }
    }

    fn frame(data: &str) -> RawFrameRef<'_> {
        RawFrameRef {
            index: 0,
            event: None,
            data,
            elapsed_us: 0,
        }
    }

    #[test]
    fn served_sink_keeps_the_first_id_and_provider_seen() {
        let ctx = FrameCtx {
            stream_id: 0,
            model_iden: ModelIden::new(AdapterKind::OpenAI, "m"),
        };
        let sink = ServedSink::default();
        for data in [
            r#"{"choices":[]}"#,
            r#"{"id":"gen-1","provider":"DeepInfra","choices":[]}"#,
            r#"{"id":"gen-2","provider":"Other"}"#,
            "[DONE]",
        ] {
            sink.on_frame(&ctx, frame(data));
        }
        let served = sink.served();
        assert_eq!(served.generation.as_deref(), Some("gen-1"));
        assert_eq!(served.provider.as_deref(), Some("DeepInfra"));
    }

    #[test]
    fn step_out_preserves_reasoning_content() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("answer")),
            captured_reasoning_content: Some("let me think".into()),
            ..Default::default()
        };
        let out = step_out_from_end(
            "deepseek-v4-pro",
            end,
            Served::default(),
            true,
            AdapterKind::DeepSeek,
        );
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
        let out = step_out_from_end(
            "deepseek-v4-pro",
            end,
            Served::default(),
            true,
            AdapterKind::DeepSeek,
        );
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
        let out = step_out_from_end("m", end, Served::default(), true, AdapterKind::Anthropic);
        assert!(
            matches!(
                &out.cut_short,
                Some(CutShort::OutputCap { stop_reason }) if stop_reason == "max_tokens"
            ),
            "an output cap must read as one, under the provider's own raw \
             stop reason, got {:?}",
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
        let out = step_out_from_end("m", end, Served::default(), true, AdapterKind::Anthropic);
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
                fn_arguments: serde_json::json!({ "command": "echo hi" }),
                thought_signatures: None,
            }])),
            captured_stop_reason: Some(StopReason::MaxTokens("length".into())),
            ..Default::default()
        };
        let out = step_out_from_end("m", end, Served::default(), true, AdapterKind::Anthropic);
        assert!(matches!(out.cut_short, Some(CutShort::OutputCap { .. })));
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
            "partial answer so far",
            "",
            &cause,
            Served::default(),
            false,
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
            "partial answer",
            "let me work through the cases",
            &cause,
            Served::default(),
            false,
        );
        assert!(step.assistant_message.content.iter().any(|part| matches!(
            part,
            ContentPart::ReasoningContent(reasoning)
                if reasoning == "let me work through the cases"
        )));
    }
}
