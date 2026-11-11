//! Streaming turns and partial-response projection.

use super::error::{CancelSite, CutShort};
use super::request::{Request, complete_options};
use super::retry::{Attempt, retry_with_backoff, wait_for_cancel};
use super::transport::{Pool, Transport};
use super::usage::{Usage, usage_from};
use super::wire::{manufacture, tool_defs};
use super::{ProviderError, Selected};
use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatFrameSink, ChatMessage, ChatStreamEvent, FrameCtx, RawFrameRef, StopReason, StreamEnd,
    ToolCall,
};
use ral_core::fact::RecordingFailure;
use ral_core::process::Scope;
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

impl Pool {
    pub(super) fn complete<F: FnMut(Delta<'_>)>(
        &self,
        transport: &Transport,
        selected: &Selected,
        request: Request<'_>,
        on_delta: &mut F,
        before_attempt: &mut dyn FnMut() -> Result<(), RecordingFailure>,
        cancel: &Scope,
    ) -> Result<StepOut, ProviderError> {
        let model = selected.model.as_str();
        let adapter = transport.adapter();
        let tools = tool_defs(adapter, transport.live_search(), request);
        let options = complete_options(self.cache_key(), selected);

        let mut heard = false;
        self.block_on(retry_with_backoff(
            CancelSite::BeforeRequest,
            cancel,
            async |retrying| {
                if let Err(error) = before_request(
                    retrying,
                    heard.then(|| Usage::unreported(transport.metered())),
                    on_delta,
                    before_attempt,
                ) {
                    return Attempt::Failed(error);
                }
                heard = false;
                if let Err(error) = transport.prepare(cancel).await {
                    return Attempt::Failed(error);
                }
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
                        ) => result.map_err(|error| ProviderError::from_genai(&error, &selected.service, model))?,
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
                                    return Err(ProviderError::from_genai(&error, &selected.service, model));
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
                            ChatStreamEvent::ToolCallChunk(_) => heard = true,
                            ChatStreamEvent::ThoughtSignatureChunk(chunk) => {
                                heard |= !chunk.content.is_empty();
                            }
                            ChatStreamEvent::Start | ChatStreamEvent::Heartbeat => {}
                        }
                    }
                    Err(ProviderError::stream_ended_without_end(&selected.service, model))
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

fn before_request(
    retrying: Option<&ProviderError>,
    lost: Option<Usage>,
    on_delta: &mut impl FnMut(Delta<'_>),
    before_attempt: &mut dyn FnMut() -> Result<(), RecordingFailure>,
) -> Result<(), ProviderError> {
    if let Some(cause) = retrying {
        on_delta(Delta::Retry { cause, lost });
    }
    before_attempt().map_err(ProviderError::Unavailable)
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
    let content = end.captured_content.clone().unwrap_or_default();
    let reasoning = end
        .captured_reasoning_content
        .clone()
        .filter(|_| !content.contains_reasoning_content());
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

    #[test]
    fn every_request_certifies_after_recording_the_previous_attempt() {
        let recorder = crate::record::Emitter::none();
        let first = recorder
            .emit(crate::record::Forensic::TurnStarted {
                turn: 1,
                tuning: super::super::Tuning::default(),
                via: None,
            })
            .unwrap()
            .locus()
            .seq()
            .commit();
        let through = std::cell::Cell::new(first);
        let mut delta = |delta: Delta<'_>| {
            if let Delta::Retry { cause, .. } = delta {
                through.set(
                    recorder
                        .emit(crate::record::Forensic::Retried {
                            error: cause.clone(),
                        })
                        .unwrap()
                        .locus()
                        .seq()
                        .commit(),
                );
            }
        };
        let mut certify = || {
            recorder.sync().unwrap();
            recorder.assert_synced_through(through.get());
            Ok(())
        };
        before_request(None, None, &mut delta, &mut certify).unwrap();
        let error = ProviderError::stream_ended_without_end("fixture", "m");
        before_request(Some(&error), None, &mut delta, &mut certify).unwrap();
        assert!(through.get() > first);
        recorder.assert_synced_through(through.get());
        let failure = before_request(None, None, &mut delta, &mut || {
            Err(RecordingFailure::CellPoisoned(
                ral_core::fact::RecordingFailureCellPoisoned { agent: 1u64.into() },
            ))
        })
        .unwrap_err();
        assert!(matches!(failure, ProviderError::Unavailable(_)));
        assert!(!failure.retryable());
    }

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
        let reasoning: Vec<_> = out
            .assistant_message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ReasoningContent(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, ["let me think"]);
    }

    #[test]
    fn step_out_trusts_structured_reasoning_even_when_the_aggregate_differs() {
        let parts = vec![
            ContentPart::ThoughtSignature("signature-1".into()),
            ContentPart::ReasoningContent("same text".into()),
            ContentPart::ThoughtSignature("signature-2".into()),
            ContentPart::ReasoningContent("same text".into()),
            ContentPart::Text("answer".into()),
        ];
        let expected = serde_json::to_value(&parts).unwrap();
        let end = StreamEnd {
            captured_content: Some(MessageContent::from(parts)),
            captured_reasoning_content: Some("a different aggregate".into()),
            ..Default::default()
        };
        let out = step_out_from_end("m", end, Served::default(), true, AdapterKind::Anthropic);
        assert_eq!(
            serde_json::to_value(out.assistant_message.content).unwrap(),
            expected
        );
    }

    #[test]
    fn step_out_preserves_explicitly_empty_aggregate_reasoning() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("answer")),
            captured_reasoning_content: Some(String::new()),
            ..Default::default()
        };
        let out = step_out_from_end("m", end, Served::default(), true, AdapterKind::DeepSeek);
        assert!(
            matches!(out.assistant_message.content.iter().last(), Some(ContentPart::ReasoningContent(text)) if text.is_empty())
        );
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
            "fixture",
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
            "fixture",
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
